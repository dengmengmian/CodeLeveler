//! Phase 1 side-effect barrier tests (core-runtime-convergence-plan).
//!
//! The barrier contract: a tool with possible side effects is dispatched only
//! after every canonical event announced so far — `ToolCallStarted`, and any
//! `ApprovalRequested`/`ApprovalResolved` produced while authorizing it — is
//! durably persisted. A persistence failure aborts the turn BEFORE the tool
//! runs; it is never reported as a working tool call.
//!
//! Companion: `phase0_baseline_test.rs` holds the inverted crash-window test
//! (`tool_side_effect_cannot_precede_durable_tool_call_started`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AutoClarify, ContinuationPolicy, StepLimits};
use leveler_core::{RequestId, SessionId, Timestamp, ToolCallId, TurnId};
use leveler_engine::{EventLog, ExecutorFactory, TurnInput, TurnKind, TurnRunner};
use leveler_execution::{
    ApprovalDecision, ApprovalRequest, Approver, PermissionProfile, Workspace,
};
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
        "barrier",
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

async fn run_chat_turn(
    h: &Harness,
    log: &EventLog<'_>,
    approver: Arc<dyn Approver>,
    text: &str,
) -> Result<leveler_engine::TurnRecordedOutcome, leveler_engine::EngineError> {
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
        log,
        factory: &h.factory,
        approver,
        clarifier: Arc::new(AutoClarify),
        expanded_context_budget: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        repo: None,
    };
    runner
        .run_turn(
            TurnKind::Chat,
            chat_profile(),
            TurnInput::Content {
                prior: Vec::new(),
                content: vec![ContentPart::Text { text: text.into() }],
            },
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
}

// ---------------------------------------------------------------------------
// 1. A persistence failure must abort the turn BEFORE the tool runs.
// ---------------------------------------------------------------------------

/// Fails appends of one event type; everything else persists normally.
struct FailingStartedStore {
    inner: Database,
}

/// Fails every `approval_resolved` append — the barrier after authorization
/// must then refuse, and no durable permission rule may exist.
struct FailingApprovalStore {
    inner: Database,
}

#[async_trait]
impl EventStore for FailingApprovalStore {
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
        if event_type == "approval_resolved" {
            return Err(StorageError::InvalidData(
                "injected: cannot persist approval_resolved".into(),
            ));
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

/// Answers every approval with "always", so the durable permission rule is
/// the thing under test.
struct ApproveAlwaysHuman;

#[async_trait]
impl Approver for ApproveAlwaysHuman {
    fn has_human(&self) -> bool {
        true
    }
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::ApproveAlways
    }
}

#[async_trait]
impl EventStore for FailingStartedStore {
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
            return Err(StorageError::InvalidData(
                "injected: cannot persist tool_call_started".into(),
            ));
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

/// If `ToolCallStarted` cannot be made durable, the tool must NOT execute and
/// the turn must fail loudly — an unexecuted tool is recoverable, an
/// unrecorded side effect is not.
#[tokio::test]
async fn persistence_failure_prevents_tool_execution() {
    let h = harness(vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
            }),
        ),
        text("done"),
    ])
    .await;
    let store = FailingStartedStore {
        inner: h.db.clone(),
    };
    let log = EventLog::new(&store, h.session.clone());

    let result = run_chat_turn(
        &h,
        &log,
        Arc::new(leveler_execution::AutoApprove),
        "patch it",
    )
    .await;

    assert!(
        result.is_err(),
        "a turn whose canonical tool event cannot persist must fail, not \
         report success"
    );
    let content = std::fs::read_to_string(h.dir.path().join("src/lib.rs")).unwrap();
    assert!(
        !content.contains("pub fn added"),
        "the tool ran although its ToolCallStarted was never durable — the \
         side-effect barrier must prevent execution on persistence failure"
    );
}

// ---------------------------------------------------------------------------
// 2. The approval outcome must be durable before dispatch (baseline risk R2).
// ---------------------------------------------------------------------------

/// Holds the durable write of `approval_resolved` open while watching for the
/// approved command's side effect (a file deletion).
struct GatedApprovalStore {
    inner: Database,
    /// Path whose disappearance is the approved command's side effect.
    watched: std::path::PathBuf,
    side_effect_before_durable: AtomicBool,
    gate_saw_no_side_effect: AtomicBool,
}

#[async_trait]
impl EventStore for GatedApprovalStore {
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
        if event_type == "approval_resolved" {
            let mut observed = false;
            for _ in 0..300 {
                if !self.watched.exists() {
                    observed = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            if observed {
                self.side_effect_before_durable
                    .store(true, Ordering::SeqCst);
            } else {
                self.gate_saw_no_side_effect.store(true, Ordering::SeqCst);
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

/// Approves everything once, as an attended user would.
struct ApproveOnceHuman;

#[async_trait]
impl Approver for ApproveOnceHuman {
    fn has_human(&self) -> bool {
        true
    }
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::ApproveOnce
    }
}

/// A crash between the user's approval and the tool's side effect must leave
/// the decision durable: otherwise resume sees `ApprovalRequested` with no
/// resolution for a call whose side effect may already exist, and can only
/// stop for manual reconciliation. With the barrier, the resolved event is
/// durable before the command runs.
#[tokio::test]
async fn approval_resolution_is_durable_before_dispatch() {
    // Deleting a directory tree, spelled the way the host spells it. The
    // barrier under test is platform-neutral; `rm -rf` is not.
    let remove_tree = if cfg!(windows) {
        serde_json::json!({"program": "cmd", "args": ["/C", "rmdir", "/s", "/q", "scratch"]})
    } else {
        serde_json::json!({"program": "rm", "args": ["-rf", "scratch"]})
    };
    let h = harness(vec![
        tool_call("c1", "run_command", remove_tree),
        text("removed"),
    ])
    .await;
    let scratch = h.dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(scratch.join("f.txt"), "x").unwrap();

    let store = GatedApprovalStore {
        inner: h.db.clone(),
        watched: scratch.clone(),
        side_effect_before_durable: AtomicBool::new(false),
        gate_saw_no_side_effect: AtomicBool::new(false),
    };
    let log = EventLog::new(&store, h.session.clone());

    run_chat_turn(&h, &log, Arc::new(ApproveOnceHuman), "remove scratch")
        .await
        .expect("approved turn must complete");

    assert!(
        !scratch.exists(),
        "the approved command must actually have run after the barrier"
    );
    assert!(
        !store.side_effect_before_durable.load(Ordering::SeqCst),
        "the command's side effect became observable while ApprovalResolved \
         was still un-durable (baseline risk R2)"
    );
    assert!(
        store.gate_saw_no_side_effect.load(Ordering::SeqCst),
        "the approval_resolved gate never engaged — the approval flow did \
         not produce the expected canonical events"
    );
}

/// A standing "always" permission must never outlive the approval that
/// granted it. If the resolution cannot be made durable, the rule file must
/// not exist — otherwise a crash leaves a permanent grant that the event log
/// cannot explain.
#[tokio::test]
async fn a_failed_approval_flush_writes_no_standing_permission() {
    let h = harness(vec![
        tool_call(
            "c1",
            "run_command",
            serde_json::json!({"program": "rm", "args": ["-rf", "scratch"]}),
        ),
        text("done"),
    ])
    .await;
    std::fs::create_dir_all(h.dir.path().join("scratch")).unwrap();
    let rules_path = leveler_execution::project_rules_path(h.dir.path());

    let store = FailingApprovalStore {
        inner: h.db.clone(),
    };
    let log = EventLog::new(&store, h.session.clone());
    let mut factory = h.factory;
    factory.permission_rules_path = Some(rules_path.clone());
    let h = Harness { factory, ..h };

    let result = run_chat_turn(&h, &log, Arc::new(ApproveAlwaysHuman), "remove scratch").await;

    assert!(
        result.is_err(),
        "a turn whose approval resolution cannot persist must fail"
    );
    assert!(
        !rules_path.exists(),
        "a standing permission was written even though the approval that \
         granted it never became durable"
    );
}

// ---------------------------------------------------------------------------
// 3. Recovery needs to know WHAT was gated, not just that something was.
// ---------------------------------------------------------------------------

/// Every `ToolCallStarted` reaches the log with its risk filled in, and every
/// approval names the call it gated.
///
/// Both facts are easy to misread from the emit sites: the executor→engine
/// conversion and the delegated-agent barrier BOTH construct the event with
/// `risk: None`, because the pump — which owns the registry — stamps it on the
/// way to the log. This test pins the stamped result rather than the
/// placeholder, so a reader who concludes from one emit site that risk is
/// never recorded has a failing test to check the claim against.
///
/// The attribution matters for the same reason: recovery decides whether a
/// dangling call is safe to replay from its risk, and whether it was blocked
/// in approval from the ids on the approval events.
#[tokio::test]
async fn the_log_records_each_call_s_risk_and_what_the_approval_gated() {
    let h = harness(vec![
        tool_call(
            "c1",
            "run_command",
            serde_json::json!({"program": "rm", "args": ["-rf", "scratch"]}),
        ),
        text("removed"),
    ])
    .await;
    std::fs::create_dir_all(h.dir.path().join("scratch")).unwrap();
    let log = EventLog::new(&h.db, h.session.clone());

    run_chat_turn(&h, &log, Arc::new(ApproveOnceHuman), "remove scratch")
        .await
        .expect("approved turn must complete");

    let events = log.replay().await.unwrap();

    let risk = events
        .iter()
        .find_map(|e| match e {
            leveler_engine::EngineEvent::ToolCallStarted { name, risk, .. }
                if name == "run_command" =>
            {
                Some(*risk)
            }
            _ => None,
        })
        .expect("the call must be recorded");
    assert!(
        risk.is_some(),
        "the pump must stamp the call's risk; recovery reads this to decide \
         whether replaying the call unattended is safe"
    );

    for event in &events {
        match event {
            leveler_engine::EngineEvent::ApprovalRequested { call_id, .. }
            | leveler_engine::EngineEvent::ApprovalResolved { call_id, .. } => {
                assert_eq!(
                    call_id.as_deref(),
                    Some("c1"),
                    "an approval must name the call it gated: {event:?}"
                );
            }
            _ => {}
        }
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, leveler_engine::EngineEvent::ApprovalResolved { .. })),
        "the approval flow never ran, so nothing above was actually checked"
    );
}
