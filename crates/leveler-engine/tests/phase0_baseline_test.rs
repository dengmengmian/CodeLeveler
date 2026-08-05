//! Phase 0 baseline characterization tests (core-runtime-convergence-plan).
//!
//! These tests LOCK today's observable semantics before any architecture
//! change. Two of them intentionally document a known gap:
//!
//! 1. `tool_side_effect_can_precede_durable_tool_call_started` proves the
//!    crash window that phase 1 will close: the executor announces a tool
//!    call through a fire-and-forget observer and dispatches the tool
//!    without waiting for the `ToolCallStarted` event to become durable.
//!    When phase 1 lands, the assertion in this test must be INVERTED —
//!    a failing run of this test is the expected signal that the barrier
//!    now exists.
//!
//! 2. `blocked_goal_collapses_to_failed_at_the_task_level` documents that
//!    `update_goal(blocked)` is only discriminable from failure via the
//!    turn event's Debug-formatted `stop_reason` string, not via the typed
//!    session outcome. Phase 4 owns making stop reasons fully typed; until
//!    then this is the contract observers rely on.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AutoClarify, ContinuationPolicy, StepLimits};
use leveler_core::{RequestId, SessionId, Timestamp, ToolCallId, TurnId};
use leveler_engine::{EngineEvent, EventLog, ExecutorFactory, TurnInput, TurnKind, TurnRunner};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::{
    Database, EventRecord, EventStore, SessionRecord, SessionRepository, StorageError,
};
use leveler_tools::{ToolContext, default_registry};

// ---------------------------------------------------------------------------
// Scripted model runtime (same shape as direct_test.rs).
// ---------------------------------------------------------------------------

struct MockRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
}

impl MockRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
        }
    }
}

#[async_trait]
impl ModelRuntime for MockRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        let response = self.generate(request, cancellation).await?;
        let mut events: Vec<Result<ModelEvent, ModelError>> = Vec::new();
        events.push(Ok(ModelEvent::MessageStarted {
            request_id: response.request_id.clone(),
        }));
        for part in &response.message.content {
            match part {
                ContentPart::Text { text } => events.push(Ok(ModelEvent::TextDelta {
                    delta: text.clone(),
                })),
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }));
                }
                _ => {}
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id": "m", "provider": "mock", "model_id": "m", "protocol": "openai_chat",
            "capabilities": {
                "streaming": true, "tool_calling": true, "parallel_tool_calls": true,
                "structured_output": false, "reasoning": false, "vision": false
            },
            "limits": {
                "context_window": 128000, "reliable_context": 64000,
                "max_output_tokens": 4096, "max_tool_schema_bytes": 65536,
                "max_parallel_tool_calls": 4
            }
        }))
        .unwrap())
    }
}

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: name.to_string(),
                    arguments: args,
                },
            }],
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
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

// ---------------------------------------------------------------------------
// A deterministic EventStore fake that gates `tool_call_started` durability.
// Tests may use fakes; this one wraps the real SQLite store and only delays
// WHEN the started-event row would commit, changing no payloads.
// ---------------------------------------------------------------------------

struct GatedStartedStore {
    inner: Database,
    /// Workspace file whose mutation is the tool's side effect.
    watched: std::path::PathBuf,
    /// Marker text the side effect writes into `watched`.
    marker: &'static str,
    /// Set when the side effect was already observable in the workspace at
    /// the moment the `tool_call_started` row was about to commit.
    side_effect_before_durable: AtomicBool,
    /// Set when the poll gave up (side effect never appeared) — distinguishes
    /// "barrier exists" from "tool never ran".
    poll_timed_out: AtomicBool,
}

#[async_trait]
impl EventStore for GatedStartedStore {
    async fn append(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        event_type: &str,
        payload: &str,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        if event_type == "tool_call_started" {
            // Hold the durable write open and watch the workspace. If the
            // side-effect barrier existed, the executor could not reach the
            // tool until this append returned, so the marker could never
            // appear while we wait here.
            let mut observed = false;
            for _ in 0..2000 {
                let content = std::fs::read_to_string(&self.watched).unwrap_or_default();
                if content.contains(self.marker) {
                    observed = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            if observed {
                self.side_effect_before_durable
                    .store(true, Ordering::SeqCst);
            } else {
                self.poll_timed_out.store(true, Ordering::SeqCst);
            }
        }
        self.inner
            .append(session_id, turn_id, event_type, payload, now)
            .await
    }

    async fn load(&self, session_id: &SessionId) -> Result<Vec<EventRecord>, StorageError> {
        self.inner.load(session_id).await
    }

    async fn load_after(
        &self,
        session_id: &SessionId,
        after: i64,
    ) -> Result<Vec<EventRecord>, StorageError> {
        self.inner.load_after(session_id, after).await
    }

    async fn load_last_by_type(
        &self,
        session_id: &SessionId,
        event_type: &str,
        turn_id: Option<&TurnId>,
    ) -> Result<Option<EventRecord>, StorageError> {
        self.inner
            .load_last_by_type(session_id, event_type, turn_id)
            .await
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    db: Database,
    factory: ExecutorFactory,
    session: SessionId,
    dir: tempfile::TempDir,
}

async fn harness(responses: Vec<ModelResponse>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context = ToolContext::with_environment(
        workspace,
        PermissionProfile::Assisted,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )),
    );
    let db = Database::connect_in_memory().await.unwrap();
    let record = SessionRecord::new(
        dir.path().display().to_string(),
        "baseline",
        "mock/m",
        leveler_core::now(),
    );
    SessionRepository::new(&db).create(&record).await.unwrap();
    let session = SessionId::new(record.id);
    let factory = ExecutorFactory {
        runtime: Arc::new(MockRuntime::new(responses)),
        registry: Arc::new(default_registry()),
        tool_context,
        model: ModelRef::new("mock", "m"),
        commit_co_author: true,
        overrides: None,
        work_profile: leveler_agent::WorkProfile::Balanced,
        memory_index: String::new(),
        permission_rules: leveler_execution::PermissionRuleSet::default(),
        permission_rules_path: None,
        hook_runner: leveler_execution::HookRunner::empty(std::path::PathBuf::from(".")),
        grants_state_dir: None,
        steering: None,
        allow_delegation: true,
    };
    Harness {
        db,
        factory,
        session,
        dir,
    }
}

fn chat_profile() -> leveler_engine::TurnProfile {
    leveler_engine::TurnProfile::Chat {
        continuation: ContinuationPolicy::UntilTerminal,
        limits: StepLimits::default(),
    }
}

/// The patch whose application is the observable side effect.
fn patch_response() -> ModelResponse {
    tool_call(
        "c1",
        "apply_patch",
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
        }),
    )
}

// ---------------------------------------------------------------------------
// 1. The phase-1 crash window, reproduced deterministically.
// ---------------------------------------------------------------------------

/// CURRENT behavior (phase 0 baseline): the tool's side effect lands in the
/// workspace while the `ToolCallStarted` row is still un-durable. A process
/// crash in that window leaves a side effect with NO dangling-call record,
/// so resume reconciliation cannot see it. Phase 1 must invert this test.
#[tokio::test]
async fn tool_side_effect_can_precede_durable_tool_call_started() {
    let h = harness(vec![patch_response(), text("done")]).await;
    let store = GatedStartedStore {
        inner: h.db.clone(),
        watched: h.dir.path().join("src/lib.rs"),
        marker: "pub fn added",
        side_effect_before_durable: AtomicBool::new(false),
        poll_timed_out: AtomicBool::new(false),
    };
    let log = EventLog::new(&store, h.session.clone());
    let runner = TurnRunner {
        db: &h.db,
        session_id: h.session.clone(),
        log: &log,
        factory: &h.factory,
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
    };

    let recorded = runner
        .run_turn(
            TurnKind::Chat,
            chat_profile(),
            TurnInput::Content {
                prior: Vec::new(),
                content: vec![ContentPart::Text {
                    text: "add a function".into(),
                }],
            },
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("turn must complete");
    assert_eq!(
        recorded.outcome.modified_files,
        vec!["src/lib.rs".to_string()]
    );

    assert!(
        !store.poll_timed_out.load(Ordering::SeqCst),
        "the tool never ran while the started-event write was held open — \
         either the side-effect barrier now exists (good: invert this test \
         as phase 1 evidence) or the harness broke"
    );
    assert!(
        store.side_effect_before_durable.load(Ordering::SeqCst),
        "phase 0 baseline: the workspace mutation is observable before \
         ToolCallStarted is durable (the phase-1 crash window)"
    );

    // The event ordering itself is still correct on the happy path:
    // started precedes finished in the durable log.
    let rows = h.db.load(&h.session).await.unwrap();
    let started = rows
        .iter()
        .position(|r| r.event_type == "tool_call_started")
        .expect("started event persisted");
    let finished = rows
        .iter()
        .position(|r| r.event_type == "tool_call_finished")
        .expect("finished event persisted");
    assert!(
        started < finished,
        "Started must precede Finished in the log"
    );
}

// ---------------------------------------------------------------------------
// 2. Stop-reason discriminability at the task level (phase 0, task 7).
// ---------------------------------------------------------------------------

/// CURRENT behavior: `update_goal(blocked)` maps to `TaskOutcome::Failed` at
/// the session level; "blocked" survives only in the turn event's
/// Debug-formatted `stop_reason` string. This locks where the information
/// lives today so phase 4 (typed stop reasons) has a baseline to diff.
#[tokio::test]
async fn blocked_goal_collapses_to_failed_at_the_task_level() {
    use leveler_engine::{ExecutionKind, TaskEngine, TaskOutcome, TaskSpec};

    let h = harness(vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "blocked", "summary": "cannot proceed"}),
    )])
    .await;
    let engine = TaskEngine {
        db: h.db,
        factory: h.factory,
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
    };
    let spec = TaskSpec {
        repository: h.dir.path().to_path_buf(),
        goal: "do the impossible".to_string(),
        mode: PermissionProfile::Assisted,
        sandbox: false,
        kind: ExecutionKind::Direct,
        continuation: ContinuationPolicy::UntilTerminal,
        limits: StepLimits::default(),
        verification: leveler_verifier::VerificationPlan::default(),
        base_commit: None,
    };
    let session = engine.create_task(&spec).await.unwrap();
    let mut turn_stop_reasons: Vec<String> = Vec::new();
    let report = engine
        .run(
            &session,
            &spec,
            &mut |e| {
                if let EngineEvent::TurnFinished { stop_reason, .. } = &e {
                    turn_stop_reasons.push(stop_reason.clone());
                }
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // Typed at the agent layer…
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Blocked);
    // …collapsed at the session layer…
    assert_eq!(report.outcome, TaskOutcome::Failed);
    // …and stringly-typed in the durable turn event.
    assert_eq!(turn_stop_reasons, vec!["Blocked".to_string()]);
}
