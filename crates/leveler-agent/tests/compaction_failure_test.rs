//! PR5d — the failure semantics of a compaction summary.
//!
//! A summary is advisory context. These tests drive the real `Executor` with a
//! scripted runtime and prove the two bounds apart:
//!
//! * SOFT (over the quality boundary, below the hard capacity): a summary that
//!   errors or times out must NOT interrupt the task; the original active
//!   history stays and the loop continues.
//! * HARD (over the hard capacity): a summary that fails must still yield a
//!   legal request — folded mechanically — or the turn fails explicitly with
//!   `ContextManagementFailure` BEFORE any oversized request is sent.
//!
//! No provider is contacted. The estimator's own numbers are the only figures
//! used; no synthetic provider usage is injected.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::coding::policy::{ContextRetentionPolicy, ResolvedContextPolicy};
use leveler_agent::{AgentError, AgentEvent, Executor, NoopSink, StepLimits, StopReason};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    COMPACTION_BREADCRUMB_MARKER, ContentPart, FinishReason, Message, ModelError, ModelErrorKind,
    ModelEvent, ModelEventStream, ModelProfile, ModelRef, ModelRequest, ModelResponse,
    ModelRuntime, Role, TokenUsage, ToolCall, ToolChoice,
};
use leveler_tools::ToolContext;

/// How the summary request behaves. The MAIN rounds are unaffected.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Summary {
    /// A usable briefing: the fold proceeds as it always has.
    Produced,
    /// A provider fault: an immediate, non-retryable failure.
    Failed,
    /// A call that never makes progress; only the caller's deadline ends it.
    Hangs,
}

struct CompactionRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
    summary: Summary,
}

impl CompactionRuntime {
    fn new(responses: Vec<ModelResponse>, summary: Summary) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
            summary,
        })
    }

    fn main_requests(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.tool_choice != ToolChoice::None)
            .cloned()
            .collect()
    }

    fn summary_requests(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.tool_choice == ToolChoice::None)
            .count()
    }

    fn events(response: ModelResponse) -> ModelEventStream {
        let mut events = vec![Ok(ModelEvent::MessageStarted {
            request_id: response.request_id,
        })];
        for part in &response.message.content {
            match part {
                ContentPart::Reasoning { text } => events.push(Ok(ModelEvent::ReasoningDelta {
                    delta: text.clone(),
                })),
                ContentPart::Text { text } => events.push(Ok(ModelEvent::TextDelta {
                    delta: text.clone(),
                })),
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }))
                }
                other => panic!("unexpected scripted content: {other:?}"),
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Box::pin(futures::stream::iter(events))
    }
}

#[async_trait]
impl ModelRuntime for CompactionRuntime {
    async fn generate(
        &self,
        _: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unreachable!("the executor uses streaming")
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let is_summary = request.tool_choice == ToolChoice::None;
        self.requests.lock().unwrap().push(request);
        if is_summary {
            match self.summary {
                Summary::Produced => {
                    return Ok(Self::events(answer("briefing about earlier rounds")));
                }
                Summary::Failed => {
                    return Err(ModelError::new(
                        ModelErrorKind::ProviderUnavailable,
                        "summary endpoint unavailable",
                    ));
                }
                Summary::Hangs => {
                    // The round is bounded by the caller's remaining task time;
                    // the child token it owns is cancelled at that bound.
                    cancellation.cancelled().await;
                    return Err(ModelError::cancelled());
                }
            }
        }
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        Ok(Self::events(response))
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id": model.to_string(),
            "provider": model.provider,
            "model_id": model.model,
            "protocol": "openai_chat",
            "capabilities": {
                "streaming": true, "tool_calling": true, "parallel_tool_calls": false,
                "structured_output": false, "reasoning": false, "vision": false
            },
            "limits": {
                "context_window": 131072, "reliable_context": 65536, "max_output_tokens": 8192,
                "max_tool_schema_bytes": 32768, "max_parallel_tool_calls": 1
            },
            "reasoning": {"style": "none"}
        }))
        .unwrap())
    }
}

fn answer(text: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, text),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

fn read(index: usize, path: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::from_parts(
            Role::Assistant,
            vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(format!("read-{index}")),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": path}),
                },
            }],
            None,
        ),
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn value() -> i32 { 1 }\n",
    )
    .unwrap();
    // Large enough that a few rounds measurably grow the projection.
    std::fs::write(
        dir.path().join("src/big.rs"),
        "// padding line for compaction pressure\n".repeat(80),
    )
    .unwrap();
    dir
}

/// A model with a wide window and a low quality boundary: the gap between the
/// two is the soft zone a failed briefing may continue in.
fn soft_policy() -> ResolvedContextPolicy {
    ResolvedContextPolicy {
        context_window: 1_000_000,
        quality_boundary: 500,
        output_reservation: 1_000,
        headroom: 0,
        pressure_threshold: 500,
        // The percentage is bypassed: these fixtures drive exact thresholds.
        soft_percent: 100,
        retention: ContextRetentionPolicy {
            keep_recent_messages: 2,
            keep_recent_tokens: 0,
        },
    }
}

/// A model whose usable request capacity is one token: every fold is required,
/// and no mechanical fold can ever fit.
fn walled_policy() -> ResolvedContextPolicy {
    ResolvedContextPolicy {
        context_window: 101,
        quality_boundary: 0,
        output_reservation: 100,
        headroom: 0,
        pressure_threshold: 1,
        soft_percent: 100,
        retention: ContextRetentionPolicy {
            // No compactible middle: the mechanical fold has nothing to elide.
            keep_recent_messages: 1_000,
            keep_recent_tokens: 0,
        },
    }
}

fn executor(
    root: &std::path::Path,
    runtime: Arc<dyn ModelRuntime>,
    policy: ResolvedContextPolicy,
    limits: StepLimits,
) -> Executor {
    Executor::new(
        runtime,
        Arc::new(leveler_tools::default_registry()),
        ToolContext::new(Workspace::new(root).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        0,
    )
    .with_delegation(false)
    .with_context_policy(policy)
    .with_step_limits(limits)
}

fn rounds(count: usize) -> Vec<ModelResponse> {
    (0..count).map(|i| read(i, "src/lib.rs")).collect()
}

/// Test A — soft timeout. The request is over the quality boundary but fits the
/// hard capacity, so a summary that never completes must not cost the task.
#[tokio::test]
async fn a_soft_summary_timeout_keeps_the_task_alive() {
    let dir = workspace();
    let mut script = rounds(9);
    script.push(answer("done"));
    let runtime = CompactionRuntime::new(script, Summary::Hangs);

    let mut events = Vec::new();
    let outcome = executor(
        dir.path(),
        runtime.clone(),
        soft_policy(),
        // A small task deadline: the hanging summary is cancelled there, not at
        // any fixed 30s harness ceiling.
        StepLimits {
            max_duration: Some(std::time::Duration::from_millis(300)),
            ..StepLimits::default()
        },
    )
    .run(
        "investigate the source",
        &mut |event| events.push(event),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("a soft compaction failure must never abort the turn");

    // The task continued and the model's own scripted rounds were consumed.
    assert!(runtime.main_requests().len() >= 2, "the loop kept running");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Compacted { .. })),
        "no fold may be committed when the summary failed"
    );
    assert!(runtime.summary_requests() > 0, "a summary was attempted");
    // Whether it ends Answered or on its own small duration budget, it must be
    // a normal stop: the failure was absorbed, not turned into an abort.
    assert!(
        matches!(
            outcome.stop_reason,
            StopReason::Answered | StopReason::BudgetExhausted
        ),
        "unexpected stop: {:?}",
        outcome.stop_reason
    );
}

/// Test A (provider fault) — same soft zone, an immediate error instead of a
/// timeout. The active history is unchanged and the loop reaches the answer.
#[tokio::test]
async fn a_soft_summary_error_keeps_the_original_history() {
    let dir = workspace();
    let mut script = rounds(9);
    script.push(answer("done"));
    let runtime = CompactionRuntime::new(script, Summary::Failed);

    let mut events = Vec::new();
    let outcome = executor(
        dir.path(),
        runtime.clone(),
        soft_policy(),
        StepLimits::default(),
    )
    .run(
        "investigate the source",
        &mut |event| events.push(event),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("a soft compaction failure must never abort the turn");

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Compacted { .. })),
        "a failed summary commits no fold"
    );
    // Test E: nothing partial reaches the next-request surface either.
    assert!(
        !events.iter().any(|event| match event {
            AgentEvent::ContextSnapshot { messages } => messages.iter().any(|message| message
                .text_content()
                .contains(COMPACTION_BREADCRUMB_MARKER)),
            _ => false,
        }),
        "no snapshot may carry a fold that was never produced"
    );
}

/// Test D — after a soft failure the next round is a normal round: a real tool
/// exchange, a real follow-up request, and no breadcrumb smuggled in.
#[tokio::test]
async fn the_round_after_a_soft_failure_is_a_normal_round() {
    let dir = workspace();
    let mut script = rounds(9);
    script.push(answer("done"));
    let runtime = CompactionRuntime::new(script, Summary::Failed);

    executor(
        dir.path(),
        runtime.clone(),
        soft_policy(),
        StepLimits::default(),
    )
    .run(
        "investigate the source",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("turn");

    let mains = runtime.main_requests();
    assert!(
        mains.len() >= 2,
        "a later round really ran after the failure"
    );
    for request in &mains {
        assert!(
            !request.messages.iter().any(|message| message
                .text_content()
                .contains(COMPACTION_BREADCRUMB_MARKER)),
            "the uncompacted history keeps no fold breadcrumb"
        );
    }
}

/// Test B — hard-required, and no fold can make the request legal. The turn
/// must fail EXPLICITLY and the oversized follow-up request must never be sent.
#[tokio::test]
async fn a_hard_summary_failure_refuses_to_send_an_oversized_request() {
    let dir = workspace();
    let mut script = rounds(3);
    script.push(answer("done"));
    let runtime = CompactionRuntime::new(script, Summary::Failed);

    let error = executor(
        dir.path(),
        runtime.clone(),
        walled_policy(),
        StepLimits::default(),
    )
    .run(
        "investigate the source",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect_err("an impossible context is an explicit failure, not a silent abort");

    assert!(
        matches!(error, AgentError::ContextManagementFailure(_)),
        "the failure must be named as context management: {error:?}"
    );
    // Only the first (unavoidably unknown) request went out; the drive refused
    // to send the next one it had already measured as illegal.
    assert_eq!(
        runtime.main_requests().len(),
        1,
        "no request after the detected over-capacity point may be sent"
    );
}

/// Test B (fallback) — hard-required, and a mechanical fold CAN make the
/// request legal. The failed briefing must not cost the task: the runtime folds
/// without one and the loop continues.
#[tokio::test]
async fn a_hard_summary_failure_folds_mechanically_when_that_fits() {
    let dir = workspace();

    // Calibrate against the runtime's own tool+system+user baseline instead of
    // guessing it: capacity sits just above round 1, so round 1 is legal.
    let probe = CompactionRuntime::new(vec![answer("done")], Summary::Produced);
    executor(
        dir.path(),
        probe.clone(),
        soft_policy(),
        StepLimits::default(),
    )
    .run(
        "investigate the source",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("probe");
    let baseline = probe.main_requests()[0]
        .projection
        .as_ref()
        .unwrap()
        .estimated_tokens();

    let policy = ResolvedContextPolicy {
        context_window: (baseline + 600) as u32,
        quality_boundary: 0,
        output_reservation: 0,
        headroom: 0,
        pressure_threshold: 1,
        // The percentage is bypassed: these fixtures drive exact thresholds.
        soft_percent: 100,
        retention: ContextRetentionPolicy {
            keep_recent_messages: 2,
            keep_recent_tokens: 0,
        },
    };

    // Many SMALL rounds: the projection grows past the capacity while the
    // retained tail stays small, so the mechanical fold fits.
    let mut script = rounds(40);
    script.push(answer("done"));
    let runtime = CompactionRuntime::new(script, Summary::Failed);
    let mut events = Vec::new();
    let outcome = executor(dir.path(), runtime.clone(), policy, StepLimits::default())
        .run(
            "investigate the source",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("a mechanical fold can still make the request legal");

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(runtime.summary_requests() > 0, "a briefing was attempted");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Compacted { .. })),
        "the hard path folded without a briefing"
    );
    // The fallback still carries the breadcrumb that names the fold.
    assert!(events.iter().any(|event| match event {
        AgentEvent::ContextSnapshot { messages } => messages.iter().any(|message| {
            message
                .text_content()
                .contains(COMPACTION_BREADCRUMB_MARKER)
        }),
        _ => false,
    }));
}

/// Test C — the happy path is unchanged: a produced briefing folds and the run
/// finishes.
#[tokio::test]
async fn a_successful_compaction_still_folds() {
    let dir = workspace();
    let mut script = rounds(6);
    script.push(answer("done"));
    let runtime = CompactionRuntime::new(script, Summary::Produced);

    let mut events = Vec::new();
    let outcome = executor(
        dir.path(),
        runtime.clone(),
        soft_policy(),
        StepLimits::default(),
    )
    .run(
        "investigate the source",
        &mut |event| events.push(event),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("turn");

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Compacted { .. })),
        "a produced briefing must still commit a fold"
    );
}

/// A fold that elides real history but leaves the message COUNT unchanged is
/// durable in its `ContextSnapshot`, not in the `Compacted` notice.
///
/// The retained tail keeps the newest exchange whole (the provider refuses an
/// orphaned tool result) and the objective pin plus breadcrumb can cost about
/// what the elided rounds did, so the count is not a reliable "a fold happened"
/// signal. The notice spells counts and stays count-based; anything that must
/// not miss a real fold — resume, the RC evidence — reads the persisted folded
/// surface, whose breadcrumb row is the product's own marker.
#[tokio::test]
async fn a_count_equal_fold_is_recorded_in_its_snapshot_not_the_notice() {
    let dir = workspace();
    // The first round reads the LARGE file, the newest one the small file: the
    // fold keeps the newest exchange whole and elides the large one, releasing
    // real context, while the message count stays the same.
    let mut script = vec![read(0, "src/big.rs"), read(1, "src/lib.rs")];
    script.push(answer("done"));
    let runtime = CompactionRuntime::new(script, Summary::Produced);
    let mut events = Vec::new();
    let outcome = executor(
        dir.path(),
        runtime.clone(),
        soft_policy(),
        StepLimits::default(),
    )
    .run(
        "investigate the source",
        &mut |event| events.push(event),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("turn");

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Compacted { .. })),
        "the count did not shrink, so the count-spelled notice must not claim it did"
    );
    let folded = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ContextSnapshot { messages } => Some(messages.clone()),
            _ => None,
        })
        .expect("the folded surface is persisted for resume");
    assert!(
        folded.iter().any(|message| message
            .text_content()
            .contains(COMPACTION_BREADCRUMB_MARKER)),
        "the snapshot carries the product's own fold marker: {folded:?}"
    );
    assert!(
        !folded
            .iter()
            .any(|message| message.content.iter().any(|part| matches!(part,
                ContentPart::ToolResult { result }
                    if result.content.contains("padding line for compaction pressure")))),
        "the elided round's result really left the active surface: {folded:?}"
    );
}
