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
) -> (TaskEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    std::fs::write(dir.path().join("README.md"), "# Project\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let engine = TaskEngine {
        db: Database::connect_in_memory().await.unwrap(),
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
        },
        approver,
        clarifier: Arc::new(AutoClarify),
    };
    (engine, dir)
}

fn direct_spec(dir: &Path) -> TaskSpec {
    TaskSpec {
        repository: dir.to_path_buf(),
        goal: "add a function".to_string(),
        mode: PermissionProfile::Assisted,
        sandbox: false,
        kind: ExecutionKind::Direct,
        continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
        limits: leveler_agent::StepLimits::default(),
        // No gates: the resume turn can at best land CompletedUnverified, which
        // keeps these tests focused on the crash-window reconciliation.
        verification: VerificationPlan::default(),
    }
}

/// Seed a minimal, replayable transcript so `resume` does not early-return with
/// "no transcript to resume".
async fn seed_transcript(engine: &TaskEngine, session: &SessionId) {
    let system =
        serde_json::to_string(&Message::text(Role::System, "you are a coding agent")).unwrap();
    let user = serde_json::to_string(&Message::text(Role::User, "add a function")).unwrap();
    MessageRepository::new(&engine.db)
        .append(session, &[system, user], leveler_core::now())
        .await
        .unwrap();
}

/// Seed a dangling tool call `c1`: a `ToolCallStarted` with no matching
/// `ToolCallFinished`, exactly what a crash mid-execution leaves behind.
async fn seed_dangling_call(
    engine: &TaskEngine,
    session: &SessionId,
    name: &str,
    arguments: String,
) {
    let turn = TurnRepository::new(&engine.db)
        .start(session, "user", None, leveler_core::now())
        .await
        .unwrap();
    let turn_id = TurnId::new(turn.id);
    let log = EventLog::new(&engine.db, session.clone());
    log.append(
        Some(&turn_id),
        EngineEvent::ToolCallStarted {
            call_id: "c1".into(),
            name: name.into(),
            arguments,
            parallel: false,
            risk: engine.factory.registry.get(name).map(|tool| tool.risk()),
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
    engine: &TaskEngine,
    session: &SessionId,
    name: &str,
    arguments: String,
) {
    let turn = TurnRepository::new(&engine.db)
        .start(session, "user", None, leveler_core::now())
        .await
        .unwrap();
    let turn_id = TurnId::new(turn.id);
    let log = EventLog::new(&engine.db, session.clone());
    log.append(
        Some(&turn_id),
        EngineEvent::ToolCallStarted {
            call_id: "c1".into(),
            name: name.into(),
            arguments,
            parallel: false,
            risk: engine.factory.registry.get(name).map(|tool| tool.risk()),
        },
        &mut |_| {},
    )
    .await
    .unwrap();
    log.append(
        Some(&turn_id),
        EngineEvent::ApprovalRequested {
            id: ApprovalId::generate(),
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
async fn recorded_events(engine: &TaskEngine, session: &SessionId) -> Vec<EngineEvent> {
    EventRepository::new(&engine.db)
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
    let (engine, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&engine, &session).await;
    seed_dangling_call(
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

    let events = recorded_events(&engine, &session).await;
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
    let (engine, dir) = harness(Arc::new(PanickingApprover), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&engine, &session).await;
    seed_dangling_call(&engine, &session, "apply_patch", patch).await;

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

    let events = recorded_events(&engine, &session).await;
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
    let (engine, dir) = harness(Arc::new(AutoDeny), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&engine, &session).await;
    seed_dangling_call(&engine, &session, "apply_patch", patch).await;

    let err = engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        leveler_engine::EngineError::RecoveryConfirmationRequired { .. }
    ));

    let events = recorded_events(&engine, &session).await;
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
    let (engine, dir) = harness(Arc::new(PanickingApprover), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&engine, &session).await;
    seed_pending_approval_call(&engine, &session, "apply_patch", patch).await;

    // The seeded ApprovalRequested is the ONLY one that may appear; recovery
    // must not add a second (that would mean it re-entered the approval flow).
    let seeded_approvals = recorded_events(&engine, &session)
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

    let events = recorded_events(&engine, &session).await;
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
    let (engine, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&engine, &session).await;
    seed_dangling_call(&engine, &session, "apply_patch", "not valid json".into()).await;

    let err = engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("mutating risk must be classified before corrupt arguments");
    assert!(matches!(
        err,
        EngineError::RecoveryConfirmationRequired { ref call_id, ref tool }
            if call_id == "c1" && tool == "apply_patch"
    ));

    let events = recorded_events(&engine, &session).await;
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
    let (engine, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&engine, &session).await;
    seed_dangling_call(&engine, &session, "read_file", "not valid json".into()).await;

    engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let events = recorded_events(&engine, &session).await;
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
    let (engine, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&engine, &session).await;
    let turn = TurnRepository::new(&engine.db)
        .start(&session, "user", None, leveler_core::now())
        .await
        .unwrap();
    EventLog::new(&engine.db, session.clone())
        .append(
            Some(&TurnId::new(turn.id)),
            EngineEvent::ToolCallStarted {
                call_id: "legacy".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "README.md"}).to_string(),
                parallel: false,
                risk: None,
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
    let (engine, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&engine, &session).await;
    seed_dangling_call(&engine, &session, "apply_patch", patch).await;

    // Without acknowledgement the resume is blocked (locked elsewhere).
    engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("unacknowledged mutating dangling call blocks resume");

    // The user inspected the workspace and acknowledged: dangling calls close
    // with an explicit marker...
    let closed = engine.acknowledge_crash_window(&session).await.unwrap();
    assert_eq!(closed, 1, "exactly the one dangling call is closed");

    let events = recorded_events(&engine, &session).await;
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
