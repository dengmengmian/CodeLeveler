//! End-to-end tests for M5 tool-call-granularity crash recovery.
//!
//! A process that dies mid tool-execution leaves a `ToolCallStarted` with no
//! matching `ToolCallFinished` in the event log. On `resume`, the engine's
//! `recover_crash_window` reconciles every such dangling call BEFORE re-driving
//! the model: a read-only (idempotent) tool is auto-replayed; a mutating,
//! unknown, or approval-pending tool blocks resume for explicit reconciliation
//! without replaying the call or continuing the model.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::AutoClarify;
use leveler_core::{ApprovalId, RequestId, SessionId, ToolCallId, TurnId};
use leveler_engine::{
    EngineError, EngineEvent, EventLog, ExecutionKind, ExecutorFactory, TaskEngine, TaskSpec,
};
use leveler_execution::{
    ApprovalDecision, ApprovalRequest, Approver, AutoApprove, AutoDeny, PermissionProfile,
    Workspace,
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::{Database, EventRepository, MessageRepository, TurnRepository};
use leveler_tools::{ToolContext, default_registry};
use leveler_verifier::VerificationPlan;

// ── scripted model runtime (mirrors direct_test's MockRuntime) ───────────────

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

/// A plain text assistant reply (chat turns are goal-mode-off and end on it).
fn text(value: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, value),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

/// The one scripted response the resume turn needs to end cleanly: declare the
/// goal complete so the goal-mode turn finishes without further work.
fn resume_to_completion() -> Vec<ModelResponse> {
    vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "recovered"}),
    )]
}

/// An approver that must never be consulted: a call into it is a test failure.
/// Proves the pending-approval branch skips a dangling call without re-running
/// the approval flow.
struct PanickingApprover;

#[async_trait]
impl Approver for PanickingApprover {
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        panic!("recovery must not consult the approver for a pending-approval dangling call");
    }
}

// ── harness ──────────────────────────────────────────────────────────────────

async fn harness(
    approver: Arc<dyn Approver>,
    responses: Vec<ModelResponse>,
) -> (TaskEngine, Database, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    std::fs::write(dir.path().join("README.md"), "# Project\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let db = Database::connect_in_memory().await.unwrap();
    let engine = TaskEngine {
        stores: leveler_storage::EngineStores::from_database(&db),
        runtime_id: leveler_core::RuntimeId::new("rt-test"),
        factory: ExecutorFactory {
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
        },
        approver,
        clarifier: Arc::new(AutoClarify),
        supervisor: None,
    };
    (engine, db, dir)
}

fn direct_spec(dir: &Path) -> TaskSpec {
    TaskSpec {
        runtime: leveler_engine::RuntimeTaskSpec {
            goal: "add a function".to_string(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: leveler_agent::StepLimits::default(),
        },
        coding: leveler_engine::CodingTaskSpec {
            repository: dir.to_path_buf(),
            mode: PermissionProfile::Assisted,
            sandbox: false,
            // No gates: the resume turn can at best land CompletedUnverified, which
            // keeps these tests focused on the crash-window reconciliation.
            verification: VerificationPlan::default(),
            base_commit: None,
        },
    }
}

/// Seed a minimal, replayable transcript so `resume` does not early-return with
/// "no transcript to resume".
async fn seed_transcript(db: &Database, session: &SessionId) {
    let system =
        serde_json::to_string(&Message::text(Role::System, "you are a coding agent")).unwrap();
    let user = serde_json::to_string(&Message::text(Role::User, "add a function")).unwrap();
    MessageRepository::new(db)
        .append(session, &[system, user], leveler_core::now())
        .await
        .unwrap();
}

/// Seed a dangling tool call `c1`: a `ToolCallStarted` with no matching
/// `ToolCallFinished`, exactly what a crash mid-execution leaves behind.
async fn seed_dangling_call(
    db: &Database,
    engine: &TaskEngine,
    session: &SessionId,
    name: &str,
    arguments: String,
) {
    let turn = TurnRepository::new(db)
        .start(session, "user", None, leveler_core::now())
        .await
        .unwrap();
    let turn_id = TurnId::new(turn.id);
    let log = EventLog::new(db, session.clone());
    log.append(
        Some(&turn_id),
        EngineEvent::ToolCallStarted {
            call_id: "c1".into(),
            name: name.into(),
            arguments,
            parallel: false,
            risk: engine.factory.registry.get(name).map(|tool| tool.risk()),
            agent_id: None,
        },
        &mut |_| {},
    )
    .await
    .unwrap();
}

/// Seed a dangling call that crashed while still blocked in approval: a
/// `ToolCallStarted` followed by an `ApprovalRequested` with no resolution — its
/// dispatch never ran, so there is no side effect to recover.
async fn seed_pending_approval_call(
    db: &Database,
    engine: &TaskEngine,
    session: &SessionId,
    name: &str,
    arguments: String,
) {
    let turn = TurnRepository::new(db)
        .start(session, "user", None, leveler_core::now())
        .await
        .unwrap();
    let turn_id = TurnId::new(turn.id);
    let log = EventLog::new(db, session.clone());
    log.append(
        Some(&turn_id),
        EngineEvent::ToolCallStarted {
            call_id: "c1".into(),
            name: name.into(),
            arguments,
            parallel: false,
            risk: engine.factory.registry.get(name).map(|tool| tool.risk()),
            agent_id: None,
        },
        &mut |_| {},
    )
    .await
    .unwrap();
    log.append(
        Some(&turn_id),
        EngineEvent::ApprovalRequested {
            id: ApprovalId::generate(),
            call_id: Some("c1".into()),
            agent_id: None,
            tool: name.into(),
            summary: "apply the interrupted patch".into(),
            command: None,
            risk: "assisted".into(),
        },
        &mut |_| {},
    )
    .await
    .unwrap();
}

/// Replay the persisted event log as decoded engine events, in order.
async fn recorded_events(db: &Database, session: &SessionId) -> Vec<EngineEvent> {
    EventRepository::new(db)
        .load(session)
        .await
        .unwrap()
        .iter()
        .map(|row| EngineEvent::from_payload(&row.payload).unwrap())
        .collect()
}

// ── tests ────────────────────────────────────────────────────────────────────

/// A read-only tool that crashed mid-execution is idempotent, so resume just
/// re-runs it and records the fresh result — no approval prompt.
#[tokio::test]
async fn safe_dangling_read_tool_is_auto_replayed_on_resume() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(
        &db,
        &engine,
        &session,
        "read_file",
        serde_json::json!({"path": "README.md"}).to_string(),
    )
    .await;

    engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let events = recorded_events(&db, &session).await;
    let replayed = events.iter().find_map(|e| match e {
        EngineEvent::ToolCallFinished {
            call_id, is_error, ..
        } if call_id == "c1" => Some(*is_error),
        _ => None,
    });
    assert_eq!(
        replayed,
        Some(false),
        "the safe dangling read must be reconciled with a successful ToolCallFinished for c1"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, EngineEvent::ApprovalRequested { .. })),
        "a read-only replay must never ask for approval"
    );
}

/// A mutating tool cannot be proven un-done. Resume must stop before consulting
/// an approver, replaying the tool, or re-driving the model (which could issue
/// the same side effect again).
#[tokio::test]
async fn non_idempotent_dangling_tool_blocks_resume_without_replay() {
    let patch = serde_json::json!({
        "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
    })
    .to_string();
    let (engine, db, dir) = harness(Arc::new(PanickingApprover), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(&db, &engine, &session, "apply_patch", patch).await;

    let err = engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            leveler_engine::EngineError::RecoveryConfirmationRequired {
                ref call_id,
                ref tool
            } if call_id == "c1" && tool == "apply_patch"
        ),
        "resume must stop with the exact uncertain call, got {err}"
    );

    let events = recorded_events(&db, &session).await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            EngineEvent::ToolCallFinished { call_id, .. } if call_id == "c1"
        )),
        "the uncertain call must remain dangling, never replayed or falsely completed"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, EngineEvent::ApprovalRequested { .. })),
        "generic approval must not be offered as if it resolved an unknown prior side effect"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        "pub fn old() {}\n",
        "recovery must not execute the uncertain patch"
    );
}

/// The same conservative stop applies regardless of an auto-deny policy: the
/// uncertain call remains visible instead of being marked as if it had not run.
#[tokio::test]
async fn non_idempotent_dangling_tool_is_not_falsely_marked_skipped() {
    let patch = serde_json::json!({
        "patch": "*** Begin Patch\n*** Update File: README.md\n # Project\n+added by patch\n*** End Patch"
    })
    .to_string();
    let (engine, db, dir) = harness(Arc::new(AutoDeny), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(&db, &engine, &session, "apply_patch", patch).await;

    let err = engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        leveler_engine::EngineError::RecoveryConfirmationRequired { .. }
    ));

    let events = recorded_events(&db, &session).await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            EngineEvent::ToolCallFinished { call_id, .. } if call_id == "c1"
        )),
        "unknown prior execution must not be mislabeled as skipped"
    );

    let readme = std::fs::read_to_string(dir.path().join("README.md")).unwrap();
    assert_eq!(
        readme, "# Project\n",
        "a denied patch must not have run its side effect"
    );
}

/// A persisted ApprovalRequested without ApprovalResolved cannot prove that the
/// tool never dispatched: the resolution may have been queued while the tool
/// started, then lost in a crash before the event-log pump flushed it. Recovery
/// must conservatively block before replay or model re-drive.
#[tokio::test]
async fn pending_approval_dangling_call_blocks_without_replay() {
    let patch = serde_json::json!({
        "patch": "*** Begin Patch\n*** Update File: README.md\n # Project\n+added by patch\n*** End Patch"
    })
    .to_string();
    let (engine, db, dir) = harness(Arc::new(PanickingApprover), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_pending_approval_call(&db, &engine, &session, "apply_patch", patch).await;

    // The seeded ApprovalRequested is the ONLY one that may appear; recovery
    // must not add a second (that would mean it re-entered the approval flow).
    let seeded_approvals = recorded_events(&db, &session)
        .await
        .iter()
        .filter(|e| matches!(e, EngineEvent::ApprovalRequested { .. }))
        .count();
    assert_eq!(seeded_approvals, 1, "sanity: exactly the seeded approval");

    let err = engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("an uncertain pending approval must block resume");

    assert!(
        matches!(
            err,
            EngineError::RecoveryConfirmationRequired { ref call_id, ref tool }
                if call_id == "c1" && tool == "apply_patch"
        ),
        "unexpected recovery error: {err:?}"
    );

    let events = recorded_events(&db, &session).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, EngineEvent::ToolCallFinished { call_id, .. } if call_id == "c1")),
        "an uncertain call must remain dangling rather than be marked finished"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, EngineEvent::ApprovalRequested { .. }))
            .count(),
        1,
        "recovery must not open a new approval for a pending-approval call"
    );
    // Recovery itself must not run the side effect.
    let readme = std::fs::read_to_string(dir.path().join("README.md")).unwrap();
    assert_eq!(readme, "# Project\n", "a blocked call must not touch files");
}

/// Corrupt arguments cannot downgrade a mutating call into a safe one. Risk is
/// classified first, so the uncertain side effect blocks resume and remains
/// dangling for manual reconciliation.
#[tokio::test]
async fn corrupt_arguments_on_mutating_call_still_require_confirmation() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(
        &db,
        &engine,
        &session,
        "apply_patch",
        "not valid json".into(),
    )
    .await;

    let err = engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("mutating risk must be classified before corrupt arguments");
    assert!(matches!(
        err,
        EngineError::RecoveryConfirmationRequired { ref call_id, ref tool }
            if call_id == "c1" && tool == "apply_patch"
    ));

    let events = recorded_events(&db, &session).await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            EngineEvent::ToolCallFinished { call_id, .. } if call_id == "c1"
        )),
        "the uncertain mutating call must remain dangling"
    );
}

/// Safe tools may be inspected further after risk classification. Invalid JSON
/// cannot be replayed, so it is recorded as an errored completion and resume may
/// continue without executing a side effect.
#[tokio::test]
async fn corrupt_arguments_on_safe_call_are_recorded_without_replay() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(&db, &engine, &session, "read_file", "not valid json".into()).await;

    engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let events = recorded_events(&db, &session).await;
    assert!(events.iter().any(|e| matches!(
        e,
        EngineEvent::ToolCallFinished { call_id, is_error: true, preview, .. }
            if call_id == "c1" && preview.contains("corrupt arguments for safe tool")
    )));
}

/// Old ToolCallStarted payloads have no persisted risk. Even if today's
/// registry classifies the tool as Safe, recovery must not reinterpret history.
#[tokio::test]
async fn legacy_call_without_persisted_risk_blocks_conservatively() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    let turn = TurnRepository::new(&db)
        .start(&session, "user", None, leveler_core::now())
        .await
        .unwrap();
    EventLog::new(&db, session.clone())
        .append(
            Some(&TurnId::new(turn.id)),
            EngineEvent::ToolCallStarted {
                call_id: "legacy".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "README.md"}).to_string(),
                parallel: false,
                risk: None,
                agent_id: None,
            },
            &mut |_| {},
        )
        .await
        .unwrap();

    let err = engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        EngineError::RecoveryConfirmationRequired { ref call_id, .. } if call_id == "legacy"
    ));
}

/// The explicit reconciliation flow the conservative stop promises: after the
/// user verifies the workspace, `acknowledge_crash_window` closes every
/// dangling call with an explicit user-acknowledged marker, and the next
/// resume proceeds instead of failing forever on the same call.
#[tokio::test]
async fn acknowledged_crash_window_unblocks_resume() {
    let patch = serde_json::json!({
        "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
    })
    .to_string();
    let (engine, db, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(&db, &engine, &session, "apply_patch", patch).await;

    // Without acknowledgement the resume is blocked (locked elsewhere).
    engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("unacknowledged mutating dangling call blocks resume");

    // The user inspected the workspace and acknowledged: dangling calls close
    // with an explicit marker...
    let closed = engine.acknowledge_crash_window(&session).await.unwrap();
    assert_eq!(closed, 1, "exactly the one dangling call is closed");

    let events = recorded_events(&db, &session).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            EngineEvent::ToolCallFinished { call_id, is_error: true, preview, .. }
                if call_id == "c1" && preview.contains("acknowledged")
        )),
        "the closure must be an explicit user-acknowledged marker, not a fake success: {events:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        "pub fn old() {}\n",
        "acknowledgement must not execute the uncertain patch"
    );

    // ...and the next resume proceeds to completion.
    engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect("resume must proceed after explicit acknowledgement");
}

// ── phase 3: the interactive chat path reconciles the crash window too ───────

/// Reopening a crashed session in the TUI/Web continues via `chat`, not
/// `resume`. A dangling MUTATING call must stop that path exactly like resume:
/// its side effect may already exist, and silently chatting on top of it hides
/// the workspace uncertainty from the user.
#[tokio::test]
async fn chat_blocks_on_a_mutating_dangling_call_until_acknowledged() {
    let (engine, db, dir) = harness(
        Arc::new(AutoApprove),
        vec![text("好的，已确认工作区状态。")],
    )
    .await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(
        &db,
        &engine,
        &session,
        "apply_patch",
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
        })
        .to_string(),
    )
    .await;

    let err = engine
        .chat(
            &session,
            &spec,
            vec![leveler_model::ContentPart::Text {
                text: "继续刚才的工作".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect_err("a mutating dangling call must block interactive chat");
    assert!(
        matches!(
            &err,
            leveler_engine::EngineError::RecoveryConfirmationRequired { call_id, tool }
                if call_id == "c1" && tool == "apply_patch"
        ),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        "pub fn old() {}\n",
        "the uncertain patch must not be replayed"
    );

    // After explicit acknowledgement the same chat proceeds.
    engine.acknowledge_crash_window(&session).await.unwrap();
    engine
        .chat(
            &session,
            &spec,
            vec![leveler_model::ContentPart::Text {
                text: "继续刚才的工作".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("chat proceeds after acknowledgement");
}

/// A dangling READ-ONLY call is idempotent: interactive chat reconciles it in
/// place (recorded finish, no prompt, no block) — same policy as resume.
#[tokio::test]
async fn chat_auto_replays_a_safe_dangling_read_tool() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), vec![text("继续。")]).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(
        &db,
        &engine,
        &session,
        "read_file",
        serde_json::json!({"path": "README.md"}).to_string(),
    )
    .await;

    engine
        .chat(
            &session,
            &spec,
            vec![leveler_model::ContentPart::Text {
                text: "继续".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("a safe dangling read must not block chat");

    let events = recorded_events(&db, &session).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            EngineEvent::ToolCallFinished { call_id, is_error: false, .. } if call_id == "c1"
        )),
        "the dangling read must be reconciled with a recorded finish: {events:?}"
    );
}

/// `RiskLevel::Safe` answers "does this need approval", NOT "is replaying it
/// harmless". `create_checkpoint` is Safe and resets the rollback baseline;
/// replaying it after a crash would quietly move the point the user can
/// return to. Recovery must ask the TOOL, and a tool that never declared
/// itself replay-safe stops for human reconciliation.
#[tokio::test]
async fn a_safe_tool_with_side_effects_is_not_auto_replayed() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(
        &db,
        &engine,
        &session,
        "create_checkpoint",
        serde_json::json!({"label": "before the crash"}).to_string(),
    )
    .await;

    let err = engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("a Safe-labelled tool with side effects must not auto-replay");
    assert!(
        matches!(
            &err,
            leveler_engine::EngineError::RecoveryConfirmationRequired { tool, .. }
                if tool == "create_checkpoint"
        ),
        "expected recovery to stop for confirmation, got: {err:?}"
    );
}

/// The other half of the same rule: a tool that DOES declare itself
/// replay-safe still replays, so tightening the gate did not turn recovery
/// into "always ask".
#[tokio::test]
async fn a_declared_replay_safe_tool_still_replays() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(
        &db,
        &engine,
        &session,
        "read_file",
        serde_json::json!({"path": "README.md"}).to_string(),
    )
    .await;

    engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect("a declared replay-safe read must still be reconciled automatically");

    let events = recorded_events(&db, &session).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            EngineEvent::ToolCallFinished { call_id, is_error: false, .. } if call_id == "c1"
        )),
        "the safe read must be replayed and recorded: {events:?}"
    );
}

/// Call ids are local to the agent that issued them, so two concurrent
/// sub-agents routinely produce the same one. If dangling calls were paired by
/// call id alone, one child's finish would close the other child's record and
/// a real side effect would vanish from recovery's view.
#[tokio::test]
async fn two_agents_sharing_a_call_id_do_not_close_each_others_records() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;

    let turn = TurnRepository::new(&db)
        .start(&session, "user", None, leveler_core::now())
        .await
        .unwrap();
    let turn_id = TurnId::new(turn.id);
    let log = EventLog::new(&db, session.clone());

    // Both children open a call with the SAME local id.
    for agent in ["agent-a", "agent-b"] {
        log.append(
            Some(&turn_id),
            EngineEvent::ToolCallStarted {
                call_id: "c1".into(),
                name: "apply_patch".into(),
                arguments: "{}".into(),
                parallel: false,
                risk: Some(leveler_execution::RiskLevel::WorkspaceWrite),
                agent_id: Some(agent.to_string()),
            },
            &mut |_| {},
        )
        .await
        .unwrap();
    }
    // Only agent-a finishes.
    log.append(
        Some(&turn_id),
        EngineEvent::ToolCallFinished {
            call_id: "c1".into(),
            name: "apply_patch".into(),
            is_error: false,
            preview: "done".into(),
            agent_id: Some("agent-a".to_string()),
        },
        &mut |_| {},
    )
    .await
    .unwrap();

    let dangling = log.dangling_tool_calls().await.unwrap();
    assert_eq!(
        dangling.len(),
        1,
        "agent-b's call must still be dangling: {dangling:?}"
    );
    assert_eq!(dangling[0].agent_id.as_deref(), Some("agent-b"));
}

/// A stale token cannot acknowledge the crash window: the dangling call
/// survives and no recovery ToolCallFinished is appended.
#[tokio::test]
async fn stale_runtime_cannot_acknowledge_crash_window() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), Vec::new()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(&db, &engine, &session, "apply_patch", "{}".into()).await;

    let task = engine.task_for_session(&session).await.unwrap().unwrap();
    let rt = leveler_core::RuntimeId::new("rt-test");
    let stale = leveler_storage::OwnershipStore::acquire(
        &db,
        &task,
        &rt,
        leveler_core::OwnerEpoch::UNOWNED,
    )
    .await
    .unwrap();
    // Reacquire: the first token is now stale.
    leveler_storage::OwnershipStore::acquire(&db, &task, &rt, stale.owner_epoch)
        .await
        .unwrap();

    let result = leveler_engine::acknowledge_crash_window(&db, &stale, &session).await;
    assert!(result.is_err(), "a stale token must not acknowledge");
    let log = EventLog::new(&db, session.clone());
    assert_eq!(
        log.dangling_tool_calls().await.unwrap().len(),
        1,
        "the dangling call must survive a stale acknowledgement"
    );
}

/// A different runtime cannot acknowledge a foreign-owned task's crash
/// window: explicit OwnershipConflict, canonical history untouched.
#[tokio::test]
async fn foreign_runtime_cannot_acknowledge_crash_window() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), Vec::new()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(&db, &engine, &session, "apply_patch", "{}".into()).await;

    let task = engine.task_for_session(&session).await.unwrap().unwrap();
    leveler_storage::OwnershipStore::acquire(
        &db,
        &task,
        &leveler_core::RuntimeId::new("rt-other"),
        leveler_core::OwnerEpoch::UNOWNED,
    )
    .await
    .unwrap();

    // The engine (rt-test) refuses: conflict, no steal, nothing written.
    let error = engine
        .acknowledge_crash_window(&session)
        .await
        .expect_err("foreign-owned task must not be acknowledged");
    assert!(
        error.to_string().contains("owned by runtime"),
        "must be a named conflict: {error}"
    );
    let log = EventLog::new(&db, session.clone());
    assert_eq!(log.dangling_tool_calls().await.unwrap().len(), 1);
}

/// The engine's own acknowledge path (current/unowned task) reacquires a
/// fresh epoch and closes the window — the positive control.
#[tokio::test]
async fn current_owner_can_acknowledge_crash_window() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), Vec::new()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(&db, &engine, &session, "apply_patch", "{}".into()).await;

    let closed = engine.acknowledge_crash_window(&session).await.unwrap();
    assert_eq!(closed, 1);
    let log = EventLog::new(&db, session.clone());
    assert!(log.dangling_tool_calls().await.unwrap().is_empty());
}
