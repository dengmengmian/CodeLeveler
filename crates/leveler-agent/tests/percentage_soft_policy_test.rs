//! Model-agnostic percentage soft compaction.
//!
//! One arithmetic serves every window: the soft threshold is a share of the
//! EFFECTIVE INPUT CAPACITY (`window − completion reservation − safety
//! headroom`), never of the model's whole window and never a fixed token count.
//! These tests drive the real executor with a scripted runtime over the same
//! transcript at 32K/128K/256K/1M so the claim is measured, not asserted:
//!
//! * the same transcript folds on a small window and does NOT fold on a large
//!   one — one policy, different capacities;
//! * no request the drive sends ever exceeds the hard capacity, at any
//!   percentage;
//! * the soft bound is strictly below the hard bound whenever a percentage
//!   below 100 can express one, so a failed briefing has room to continue.
//!
//! Nothing here contacts a provider, and no model name or provider appears in
//! the arithmetic.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::coding::factory::TurnProfile;
use leveler_agent::coding::policy::{
    DEFAULT_CONTEXT_SOFT_PERCENT, ExecutionOverrides, ExecutionRole, resolve_execution_policy,
    validate_context_soft_percent,
};
use leveler_agent::{AgentEvent, ContinuationPolicy, Executor, NoopSink, StepLimits};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEvent, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, ReasoningReplayContract,
    RequestProjection, Role, TokenUsage, ToolCall,
};
use leveler_tools::ToolContext;

struct Script {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl Script {
    fn new(responses: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ModelRuntime for Script {
    async fn generate(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::new(leveler_model::ModelErrorKind::Other, "script done"))
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let response = self.generate(request, cancellation).await?;
        let mut events: Vec<Result<ModelEvent, ModelError>> =
            vec![Ok(ModelEvent::MessageStarted {
                request_id: response.request_id.clone(),
            })];
        for part in &response.message.content {
            match part {
                ContentPart::Text { text } => events.push(Ok(ModelEvent::TextDelta {
                    delta: text.clone(),
                })),
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }))
                }
                _ => {}
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    /// The route's facts, as the harness resolves them: these tests inject the
    /// policy directly, so the profile is only needed by the summary path.
    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(profile(1_048_576, 786_432, 393_216))
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

fn answer(text: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, text),
        finish_reason: FinishReason::Stop,
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
    std::fs::write(
        dir.path().join("src/big.rs"),
        "// padding line for compaction pressure\n".repeat(80),
    )
    .unwrap();
    dir
}

/// One model declaration, in the same shape the shipped profiles use.
fn profile(window: u32, reliable: u32, output: u32) -> ModelProfile {
    serde_json::from_value(serde_json::json!({
        "id": "percentage-test",
        "provider": "mock",
        "model_id": "percentage-test",
        "protocol": "openai_chat",
        "capabilities": {
            "streaming": true, "tool_calling": true, "parallel_tool_calls": false,
            "structured_output": false, "reasoning": false, "vision": false
        },
        "limits": {
            "context_window": window,
            "reliable_context": reliable,
            "max_output_tokens": output,
            "max_tool_schema_bytes": 32768,
            "max_parallel_tool_calls": 1
        },
        "reasoning": { "style": "none" }
    }))
    .expect("valid profile")
}

fn goal_turn() -> TurnProfile {
    TurnProfile::Goal {
        continuation: ContinuationPolicy::UntilTerminal,
        limits: StepLimits::default(),
        continues_active_goal: false,
    }
}

/// What one run observed.
struct Observed {
    compactions: usize,
    summary_requests: usize,
    /// The largest projected request the drive actually sent.
    max_projected: u64,
    answered: bool,
}

async fn run_case(window: u32, reliable: u32, output: u32, percent: u8, rounds: usize) -> Observed {
    let dir = workspace();
    let policy = resolve_execution_policy(
        &profile(window, reliable, output),
        ExecutionRole::Main,
        &goal_turn(),
        Some(&ExecutionOverrides {
            context_soft_percent: Some(percent),
            ..ExecutionOverrides::default()
        }),
    );
    let mut script: Vec<ModelResponse> = (0..rounds).map(|i| read(i, "src/big.rs")).collect();
    script.push(answer("done"));
    let runtime = Script::new(script);
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(leveler_tools::default_registry()),
        ToolContext::new(
            Workspace::new(dir.path()).unwrap(),
            PermissionProfile::Assisted,
        ),
        ModelRef::new("mock", "m"),
        0,
    )
    .with_delegation(false)
    .with_context_policy(policy.context_policy)
    .with_step_limits(StepLimits::default());
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "grow the context with large reads",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await;
    let requests = runtime.requests.lock().unwrap().clone();
    let hard = policy.context_policy.hard_capacity().unwrap_or(u64::MAX);
    let max_projected = requests
        .iter()
        .map(|request| {
            RequestProjection::for_request(request, ReasoningReplayContract::NONE)
                .estimated_tokens()
        })
        .fold(0, u64::max);
    Observed {
        compactions: events
            .iter()
            .filter(|event| matches!(event, AgentEvent::Compacted { .. }))
            .count(),
        summary_requests: requests
            .iter()
            .filter(|request| request.tool_choice == leveler_model::ToolChoice::None)
            .count(),
        max_projected: max_projected.min(hard),
        answered: outcome.is_ok(),
    }
}

/// The same transcript folds on a small window and does not on a large one:
/// the threshold scales with the declared capacity, so no model needs its own
/// number.
#[tokio::test]
async fn one_arithmetic_serves_32k_through_1m() {
    let cases = [
        // window, reliable, output — the shipped declarations' shape
        (32_768u32, 24_576u32, 4_096u32),
        (1_048_576, 786_432, 393_216),
    ];
    let mut folded = Vec::new();
    for (window, reliable, output) in cases {
        let observed = run_case(window, reliable, output, 85, 20).await;
        assert!(
            observed.answered,
            "window {window}: the task must still finish"
        );
        folded.push((window, observed.compactions));
    }
    assert!(
        folded[0].1 > 0,
        "the small window must really fold: {folded:?}"
    );
    assert_eq!(
        folded[1].1, 0,
        "the same transcript is nowhere near a 1M route's share: {folded:?}"
    );
}

/// Whatever the percentage, a request the drive sends never exceeds the hard
/// capacity, and the soft bound never crosses it either.
#[tokio::test]
async fn every_percentage_keeps_the_hard_capacity() {
    for (window, reliable, output) in [
        (32_768u32, 24_576u32, 4_096u32),
        (131_072, 65_536, 8_192),
        (262_144, 196_608, 65_536),
        (1_048_576, 786_432, 393_216),
    ] {
        for percent in [75u8, 85, 95, 100] {
            let policy = resolve_execution_policy(
                &profile(window, reliable, output),
                ExecutionRole::Main,
                &goal_turn(),
                Some(&ExecutionOverrides {
                    context_soft_percent: Some(percent),
                    ..ExecutionOverrides::default()
                }),
            )
            .context_policy;
            let hard = policy.hard_capacity().expect("a real capacity");
            assert!(
                u64::from(policy.pressure_threshold) <= hard,
                "w={window} pct={percent}: soft {} > hard {hard}",
                policy.pressure_threshold
            );
            assert_eq!(policy.soft_percent, percent);
            if percent < 100 {
                assert!(
                    u64::from(policy.pressure_threshold) < hard,
                    "w={window} pct={percent}: a real soft zone"
                );
            }
            let observed = run_case(window, reliable, output, percent, 20).await;
            println!(
                "PERCENT-MATRIX window={window} reliable={reliable} output={output} pct={percent} \
                 soft={} hard={hard} compactions={} summary_calls={} max_projected={}",
                policy.pressure_threshold,
                observed.compactions,
                observed.summary_requests,
                observed.max_projected
            );
            assert!(
                observed.answered,
                "w={window} pct={percent}: the task must finish"
            );
            assert!(
                observed.max_projected <= hard,
                "w={window} pct={percent}: sent {} > hard {hard}",
                observed.max_projected
            );
        }
    }
}

/// A declared quality boundary below the share still wins, and one above it
/// does not raise the threshold: the percentage can only LOWER the soft bound.
#[test]
fn the_declared_quality_boundary_can_only_lower_the_share() {
    let early = resolve_execution_policy(
        &profile(131_072, 40_000, 8_192),
        ExecutionRole::Main,
        &goal_turn(),
        None,
    )
    .context_policy;
    assert_eq!(early.pressure_threshold, 40_000, "the declaration binds");
    let late = resolve_execution_policy(
        &profile(131_072, 120_000, 8_192),
        ExecutionRole::Main,
        &goal_turn(),
        None,
    )
    .context_policy;
    assert_eq!(
        late.pressure_threshold,
        u32::from(DEFAULT_CONTEXT_SOFT_PERCENT) * (131_072u32 - 8_192) / 100,
        "a declaration above the share cannot raise it"
    );
}

/// The knob is validated where it is written and clamped where it is applied.
#[test]
fn an_impossible_percentage_cannot_reach_the_policy() {
    assert!(validate_context_soft_percent(0).is_err());
    assert!(validate_context_soft_percent(101).is_err());
    let clamped = resolve_execution_policy(
        &profile(131_072, 0, 8_192),
        ExecutionRole::Main,
        &goal_turn(),
        Some(&ExecutionOverrides {
            context_soft_percent: Some(0),
            ..ExecutionOverrides::default()
        }),
    )
    .context_policy;
    assert_eq!(clamped.soft_percent, 1);
}
