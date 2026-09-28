//! PR5e — the entry level of the unified compaction failure semantics.
//!
//! PR5d gave the coding drive two bounds (quality threshold vs hard capacity).
//! Chat and resume assembled their prior context through `assemble_measured`,
//! which had no hard-capacity signal and therefore stayed fail-closed: any
//! summary error aborted the turn even when the uncompacted request was legal
//! to send. These tests drive the REAL `chat` and `resume` entries and prove
//! they now share the drive's contract:
//!
//! * a soft summary failure keeps the original history and the turn continues;
//! * a cancelled task is not a summary failure and still propagates.
//!
//! The mock profile declares a tiny quality boundary and a wide window, so the
//! seeded history is ALWAYS over the threshold and under the hard capacity —
//! the soft zone. No provider is contacted.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::coding::{CodingRuntime, ExecutorFactory, TaskSpec};
use leveler_agent::{AutoClarify, ContinuationPolicy, StepLimits};
use leveler_core::{RequestId, ToolCallId};
use leveler_engine::{ContextSummarizer, EngineEvent, ExecutionKind, TaskEngine};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelErrorKind, ModelEvent, ModelEventStream,
    ModelProfile, ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
    ToolChoice,
};
use leveler_storage::{Database, MessageRepository, SessionRepository};
use leveler_tools::ToolContext;

fn default_registry() -> leveler_tools::ToolRegistry {
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    registry
}

/// How the runtime answers a summary request (`tool_choice == None`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Summary {
    /// A usable briefing.
    Produced,
    /// A non-retryable provider fault: no briefing will ever arrive.
    Failed,
    /// The summary call hangs until its token is cancelled.
    Hangs,
}

struct EntryRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
    summary: Summary,
}

impl EntryRuntime {
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
impl ModelRuntime for EntryRuntime {
    async fn generate(
        &self,
        _: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unreachable!("this runtime is driven through its stream")
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
                    return Ok(Self::events(text("briefing about earlier rounds")));
                }
                Summary::Failed => {
                    return Err(ModelError::new(
                        ModelErrorKind::Auth,
                        "summary endpoint rejected the call",
                    ));
                }
                Summary::Hangs => {
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
        // Tiny quality boundary, wide window: any non-trivial history is over
        // the threshold and far under the hard capacity — the soft zone.
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
                "context_window": 1000000, "reliable_context": 4096, "max_output_tokens": 1024,
                "max_tool_schema_bytes": 65536, "max_parallel_tool_calls": 1
            },
            "reasoning": {"style": "none"}
        }))
        .unwrap())
    }
}

fn text(value: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, value),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

fn partial_plan(id: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::from_parts(
            Role::Assistant,
            vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: "update_plan".into(),
                    arguments: serde_json::json!({
                        "plan": [
                            {"step": "implemented", "status": "completed"},
                            {"step": "publish later", "status": "pending"}
                        ]
                    }),
                },
            }],
            None,
        ),
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

struct Harness {
    engine: CodingRuntime,
    db: Database,
    dir: tempfile::TempDir,
    runtime: Arc<EntryRuntime>,
}

async fn harness(responses: Vec<ModelResponse>, summary: Summary) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn value() -> i32 { 1 }\n",
    )
    .unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = EntryRuntime::new(responses, summary);
    let db = Database::connect_in_memory().await.unwrap();
    let engine = CodingRuntime {
        engine: TaskEngine {
            stores: leveler_storage::EngineStores::from_database(&db),
            runtime_id: leveler_core::RuntimeId::new("rt-pr5e"),
            boot: leveler_engine::EngineBoot {
                id: leveler_core::BootId::generate(),
                liveness: std::sync::Arc::new(leveler_test_support::TestBoots::new()),
            },
        },
        factory: ExecutorFactory {
            runtime: runtime.clone(),
            registry: Arc::new(default_registry()),
            tool_context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: false,
            overrides: None,
            memory_catalog: String::new(),
            memory_expose: true,
            memory_root: None,
            background_tasks: std::sync::Arc::new(leveler_execution::BackgroundTaskRegistry::new()),
            permission_rules: leveler_execution::PermissionRuleSet::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(std::path::PathBuf::from(".")),
            steering: None,
            allow_delegation: true,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Off,
            develop_model: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        task_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    Harness {
        engine,
        db,
        dir,
        runtime,
    }
}

fn spec(h: &Harness, goal: &str) -> TaskSpec {
    TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: goal.into(),
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::bounded(6),
            limits: StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: h.dir.path().to_path_buf(),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    }
}

/// A transcript that is over the quality boundary (4096) and far under the hard
/// capacity (998 976), with a foldable middle.
async fn seed_soft_pressure_history(db: &Database, session: &leveler_core::SessionId) {
    let mut payloads =
        vec![serde_json::to_string(&Message::text(Role::User, "fix the login timeout")).unwrap()];
    let pad = "login-timeout-retry-and-session-detail ".repeat(20);
    for index in 0..60 {
        payloads.push(
            serde_json::to_string(&Message::text(
                Role::Assistant,
                format!("turn {index} {pad}"),
            ))
            .unwrap(),
        );
    }
    MessageRepository::new(db)
        .append(session, &payloads, leveler_core::now())
        .await
        .unwrap();
}

/// Test A — chat. Over the quality boundary, under the hard capacity, and the
/// briefing fails: the chat turn must NOT abort, and the original history must
/// reach the model uncompacted.
#[tokio::test]
async fn chat_continues_after_a_soft_summary_failure() {
    let h = harness(
        vec![text("the login timeout is a retry-policy bug")],
        Summary::Failed,
    )
    .await;
    let s = spec(&h, "chat session");
    let session = h.engine.create_task(&s).await.unwrap();
    seed_soft_pressure_history(&h.db, &session).await;

    let mut events = Vec::new();
    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "explain the login timeout".into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .expect("a soft summary failure must not abort a chat turn");

    assert!(
        h.runtime.summary_requests() > 0,
        "the fold was attempted, so the failure was real"
    );
    let mains = h.runtime.main_requests();
    assert!(!mains.is_empty(), "the chat request was sent");
    assert!(
        mains[0]
            .messages
            .iter()
            .any(|message| message.text_content().contains("login-timeout-retry")),
        "the original active history stands; it was not folded away"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::ContextSnapshot { .. })),
        "a failed briefing commits no snapshot"
    );
}

/// Test B — resume. Same soft regime and a failing briefing: resume must
/// continue from the persisted history instead of aborting.
#[tokio::test]
async fn resume_continues_after_a_soft_summary_failure() {
    let h = harness(
        vec![
            partial_plan("p1"),
            text("resumed and finished the login fix"),
        ],
        Summary::Failed,
    )
    .await;
    let mut s = spec(&h, "prepare but do not publish");
    s.runtime.continuation = ContinuationPolicy::bounded(1);
    let session = h.engine.create_task(&s).await.unwrap();

    let first = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .expect("bounded first window");
    assert_ne!(first.outcome, leveler_lifecycle::TaskOutcome::Completed);

    seed_soft_pressure_history(&h.db, &session).await;
    SessionRepository::new(&h.db)
        .set_outcome(
            &session,
            leveler_lifecycle::TaskOutcome::Interrupted,
            leveler_core::now(),
        )
        .await
        .unwrap();

    h.engine
        .resume(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .expect("a soft summary failure must not abort a resume");
}

/// Test C — the happy path is unchanged: a briefing that IS produced still
/// folds at the chat entry and is persisted as the new active context.
#[tokio::test]
async fn chat_folds_when_the_briefing_is_produced() {
    let h = harness(
        vec![text("the login timeout is a retry-policy bug")],
        Summary::Produced,
    )
    .await;
    let s = spec(&h, "chat session");
    let session = h.engine.create_task(&s).await.unwrap();
    seed_soft_pressure_history(&h.db, &session).await;

    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "explain the login timeout".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("a produced briefing folds and the chat continues");

    assert!(
        h.runtime.summary_requests() > 0,
        "the entry attempted the fold"
    );
    let mains = h.runtime.main_requests();
    assert!(
        mains[0].messages.iter().any(|message| message
            .text_content()
            .contains("briefing about earlier rounds")),
        "the produced briefing becomes part of the active context"
    );
}

/// Test F — a task cancelled WHILE the compaction summary is in flight must
/// propagate: chat records an interrupted turn instead of continuing to a
/// model request as if the summary had merely failed.
#[tokio::test]
async fn chat_propagates_a_cancellation_that_arrives_during_compaction() {
    let h = harness(vec![text("must never be sent")], Summary::Hangs).await;
    let s = spec(&h, "chat session");
    let session = h.engine.create_task(&s).await.unwrap();
    seed_soft_pressure_history(&h.db, &session).await;

    let cancellation = CancellationToken::new();
    let killer = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        killer.cancel();
    });

    let result = h
        .engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "explain the login timeout".into(),
            }],
            &mut |_| {},
            cancellation,
        )
        .await;

    assert!(
        matches!(result, Err(leveler_engine::EngineError::Cancelled)),
        "user cancellation must propagate, not become a soft continue: {result:?}"
    );
    assert!(
        h.runtime.main_requests().is_empty(),
        "no model request may follow a cancelled compaction"
    );
}

/// Test F — `ModelSummarizer` reports a recoverable summary fault as "no
/// briefing" (which the lifecycle turns into a soft continue or a mechanical
/// fold), never as an aborted turn.
#[tokio::test]
async fn a_provider_fault_is_reported_as_no_briefing() {
    let h = harness(Vec::new(), Summary::Failed).await;
    let session = h
        .engine
        .create_task(&spec(&h, "chat session"))
        .await
        .unwrap();
    let cancellation = CancellationToken::new();
    let summarizer =
        h.engine
            .context_summarizer(&session, "scope", StepLimits::default(), &cancellation);

    let history = soft_history();
    let result = summarizer
        .summarize(&history)
        .await
        .expect("a provider fault is not a fatal assembly error");
    assert_eq!(result, None, "no briefing was produced");
    assert_eq!(
        h.runtime.summary_requests(),
        1,
        "the summary call really failed"
    );
}

/// Test F — a cancelled task is NOT a soft summary failure. When the summary is
/// cancelled mid-flight the summarizer must surface cancellation, so the caller
/// records an interrupted turn instead of sending the model request anyway.
#[tokio::test]
async fn a_cancelled_task_is_reported_as_cancelled() {
    let h = harness(Vec::new(), Summary::Hangs).await;
    let session = h
        .engine
        .create_task(&spec(&h, "chat session"))
        .await
        .unwrap();
    let cancellation = CancellationToken::new();
    let summarizer =
        h.engine
            .context_summarizer(&session, "scope", StepLimits::default(), &cancellation);

    // Cancel the TASK token while the summary call is in flight.
    let killer = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        killer.cancel();
    });

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        summarizer.summarize(&soft_history()),
    )
    .await
    .expect("the cancelled summary must return promptly");

    assert!(
        matches!(result, Err(leveler_engine::EngineError::Cancelled)),
        "cancellation must propagate, not be swallowed as a soft summary failure: {result:?}"
    );
}

/// The transcript both summarizer tests fold: a head, a foldable middle and a
/// tail, so `summary_request` actually builds a model call.
fn soft_history() -> Vec<Message> {
    let mut messages = vec![Message::text(Role::User, "fix the login timeout")];
    let pad = "login-timeout-retry-and-session-detail ".repeat(20);
    for index in 0..60 {
        messages.push(Message::text(
            Role::Assistant,
            format!("turn {index} {pad}"),
        ));
    }
    messages
}
