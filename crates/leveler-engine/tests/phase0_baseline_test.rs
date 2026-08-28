//! Phase 0/1 baseline characterization tests (core-runtime-convergence-plan).
//!
//! 1. `tool_side_effect_cannot_precede_durable_tool_call_started` is the
//!    phase 1 side-effect barrier contract: the executor must not dispatch
//!    a tool until the announcing `ToolCallStarted` event is durable. In
//!    phase 0 this test was the inverse — it deterministically PROVED the
//!    crash window (side effect observable while the started row was held
//!    un-durable). Phase 1 closed the window, so the assertions flipped,
//!    exactly as the phase 0 version said they must.
//!
//! 2. `blocked_goal_is_typed_in_terminal_events_and_session_status` is the
//!    phase 4 contract: blocked (and every other stop) is machine-
//!    discriminable via the typed `stop` field and the session status
//!    column, both written by the engine as the single lifecycle writer.
//!    (The phase 0 version proved blocked survived only as a Debug string.)

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
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        // Completion Reconciliation Gate calls are answered out of band so
        // scripted FIFOs and request-count assertions stay about the loop.
        if let Some(reply) = leveler_test_support::derive_autopilot(&request) {
            return Ok(reply);
        }
        if let Some(reply) = leveler_test_support::reconcile_autopilot(&request) {
            return Ok(reply);
        }
        self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        // One shared definition of response→stream semantics (phase 6).
        let response = self.generate(request, cancellation).await?;
        Ok(leveler_model::stream_from_response(response))
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
    // Fenced append: these doubles test event persistence/gating, not
    // ownership - delegate to the unfenced path.
    async fn append_owned(
        &self,
        _token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        turn_id: Option<&leveler_core::TurnId>,
        event_type: &str,
        payload: &str,
        now: leveler_core::Timestamp,
    ) -> Result<EventRecord, leveler_storage::OwnershipError> {
        self.append(session_id, turn_id, event_type, payload, now)
            .await
            .map_err(leveler_storage::OwnershipError::Storage)
    }

    async fn append(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        event_type: &str,
        payload: &str,
        now: Timestamp,
    ) -> Result<EventRecord, StorageError> {
        if event_type == "tool_call_started" {
            // Hold the durable write open and watch the workspace. The
            // side-effect barrier means the executor cannot reach the tool
            // until this append returns, so the marker must never appear
            // while we wait here.
            let mut observed = false;
            for _ in 0..300 {
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
        independent_review: leveler_engine::IndependentReviewPolicy::Auto,
        completion_judge_model: None,
        completion_judge_timeout: None,
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
// 1. The phase-1 side-effect barrier, verified deterministically.
// ---------------------------------------------------------------------------

/// Phase 1 contract: the tool's side effect must NOT be observable while the
/// `ToolCallStarted` row is still un-durable. A process crash before the
/// started row commits therefore leaves no unrecorded side effect, and every
/// side effect that did happen has a dangling-call record for reconciliation.
/// (The phase 0 version of this test proved the opposite — the crash window —
/// with the same gated store.)
#[tokio::test]
async fn tool_side_effect_cannot_precede_durable_tool_call_started() {
    let h = harness(vec![patch_response(), text("done")]).await;
    let store = GatedStartedStore {
        inner: h.db.clone(),
        watched: h.dir.path().join("src/lib.rs"),
        marker: "pub fn added",
        side_effect_before_durable: AtomicBool::new(false),
        poll_timed_out: AtomicBool::new(false),
    };
    let log = EventLog::new(&store, h.session.clone());
    let stores = leveler_storage::EngineStores::from_database(&h.db);
    let task =
        leveler_storage::TaskStore::ensure_for_session(&h.db, &h.session, leveler_core::now())
            .await
            .unwrap();
    let owner = leveler_storage::OwnershipStore::current(&h.db, &task)
        .await
        .unwrap()
        .unwrap();
    let token = leveler_storage::OwnershipStore::acquire(
        &h.db,
        &task,
        &leveler_core::RuntimeId::new("rt-test"),
        owner.epoch,
    )
    .await
    .unwrap();
    let runner = TurnRunner {
        stores: &stores,
        token,
        session_id: h.session.clone(),
        log: &log,
        factory: &h.factory,
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        expanded_context_budget: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        repo: None,
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
        !store.side_effect_before_durable.load(Ordering::SeqCst),
        "side-effect barrier violated: the workspace mutation became \
         observable while ToolCallStarted was still un-durable"
    );
    assert!(
        store.poll_timed_out.load(Ordering::SeqCst),
        "the gated append never saw the executor waiting — the tool either \
         never ran or the started event was not appended at all"
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
// 2. Stop-reason discriminability at the task level (phase 4 contract).
// ---------------------------------------------------------------------------

/// Phase 4 contract: `update_goal(blocked)` is machine-discriminable at every
/// durable level — the typed `stop` field on both terminal events and the
/// session's `Blocked` status column, written by the engine as the single
/// lifecycle writer. (`TaskOutcome::Failed` remains the coarse outcome for
/// non-success, as before; the phase 0 version of this test proved "blocked"
/// only survived as a Debug string.)
#[tokio::test]
async fn blocked_goal_is_typed_in_terminal_events_and_session_status() {
    use leveler_engine::{ExecutionKind, TaskEngine, TaskOutcome, TaskSpec};

    let h = harness(vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "blocked", "summary": "cannot proceed"}),
    )])
    .await;
    let stores = leveler_storage::EngineStores::from_database(&h.db);
    let engine = TaskEngine {
        stores,
        runtime_id: leveler_core::RuntimeId::new("rt-test"),
        factory: h.factory,
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        supervisor: None,
    };
    let spec = TaskSpec {
        runtime: leveler_engine::RuntimeTaskSpec {
            goal: "do the impossible".to_string(),
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::UntilTerminal,
            limits: StepLimits::default(),
        },
        coding: leveler_engine::CodingTaskSpec {
            repository: h.dir.path().to_path_buf(),
            mode: PermissionProfile::Assisted,
            sandbox: false,
            verification: leveler_verifier::VerificationPlan::default(),
            base_commit: None,
        },
    };
    let session = engine.create_task(&spec).await.unwrap();
    let mut turn_stops: Vec<Option<leveler_agent::StopReason>> = Vec::new();
    let mut task_stop: Option<Option<leveler_agent::StopReason>> = None;
    let report = engine
        .run(
            &session,
            &spec,
            &mut |e| match &e {
                EngineEvent::TurnFinished { stop, .. } => turn_stops.push(*stop),
                EngineEvent::TaskFinished { stop, .. } => task_stop = Some(*stop),
                _ => {}
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // Typed in the report, in both terminal events, and in the session row.
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Blocked);
    assert_eq!(report.outcome, TaskOutcome::Failed);
    assert_eq!(turn_stops, vec![Some(leveler_agent::StopReason::Blocked)]);
    assert_eq!(
        task_stop,
        Some(Some(leveler_agent::StopReason::Blocked)),
        "TaskFinished must carry the typed stop reason"
    );
    let record = SessionRepository::new(&h.db)
        .get(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        record.status,
        leveler_lifecycle::SessionStatus::Blocked,
        "the engine must be the writer of the terminal session status"
    );
}

/// The engine — not the app layer — owns the session lifecycle columns:
/// Running while a task executes, terminal status/state stamped atomically
/// with the outcome.
#[tokio::test]
async fn engine_stamps_running_and_terminal_session_status_itself() {
    use leveler_engine::{ExecutionKind, TaskEngine, TaskSpec};

    let h = harness(vec![
        patch_response(),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "done"}),
        ),
    ])
    .await;
    let stores = leveler_storage::EngineStores::from_database(&h.db);
    let engine = TaskEngine {
        stores,
        runtime_id: leveler_core::RuntimeId::new("rt-test"),
        factory: h.factory,
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        supervisor: None,
    };
    let spec = TaskSpec {
        runtime: leveler_engine::RuntimeTaskSpec {
            goal: "add a function".to_string(),
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::UntilTerminal,
            limits: StepLimits::default(),
        },
        coding: leveler_engine::CodingTaskSpec {
            repository: h.dir.path().to_path_buf(),
            mode: PermissionProfile::Assisted,
            sandbox: false,
            verification: leveler_verifier::VerificationPlan::default(),
            base_commit: None,
        },
    };
    let session = engine.create_task(&spec).await.unwrap();
    engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let record = SessionRepository::new(&h.db)
        .get(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        record.status,
        leveler_lifecycle::SessionStatus::Completed,
        "terminal status must come from the engine, with no app-layer writer"
    );
    assert_eq!(record.state, leveler_lifecycle::AgentState::Complete);
}
