//! End-to-end tests for M5 tool-call-granularity crash recovery.
//!
//! A process that dies mid tool-execution leaves a `ToolCallStarted` with no
//! matching `ToolCallFinished` in the event log. On `resume`, the engine's
//! `recover_crash_window` reconciles every such dangling call BEFORE re-driving
//! the model. Nothing is replayed — the crash left the call's outcome unknown.
//! A pure runtime observation may be closed as explicitly interrupted; every
//! other call blocks the turn until the user acknowledges the crash window, and
//! the acknowledgement records it as unresolved rather than as a result.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::AutoClarify;
use leveler_agent::coding::{CodingRuntime, ExecutorFactory, TaskSpec};
use leveler_core::{ApprovalId, RequestId, SessionId, ToolCallId, TurnId};
use leveler_engine::{EngineError, EngineEvent, EventLog, ExecutionKind, TaskEngine};
use leveler_execution::{
    ApprovalDecision, ApprovalRequest, Approver, AutoApprove, AutoDeny, PermissionProfile,
    Workspace,
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::{Database, EventRepository, MessageRepository};
use leveler_tools::ToolContext;

/// The surface a real coding turn gets: the tool crate's composition plus the
/// harness controls THIS crate registers (`update_plan`). Production composes
/// the same two halves in `leveler-app`.
fn default_registry() -> leveler_tools::ToolRegistry {
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    registry
}

// ── scripted model runtime (mirrors direct_test's MockRuntime) ───────────────

struct MockRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    /// Every request the runtime actually sent, in order. A test that asserts
    /// what the MODEL can see must read this, not the event log.
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

/// Append the answer a real model writes after resolving its goal: the runtime
/// takes one delivery round when the resolving round carried no text.
fn with_delivery(mut responses: Vec<ModelResponse>) -> Vec<ModelResponse> {
    let resolves_without_text = responses.last().is_some_and(|response| {
        response.message.text_content().trim().is_empty()
            && response.message.content.iter().any(
                |part| matches!(part, ContentPart::ToolCall { call } if call.name == "update_goal"),
            )
    });
    if resolves_without_text {
        responses.push(text("Done: the goal is resolved and reported."));
    }
    responses
}

impl MockRuntime {
    fn capturing(responses: Vec<ModelResponse>, requests: Arc<Mutex<Vec<ModelRequest>>>) -> Self {
        let responses = with_delivery(responses);
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests,
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
        self.requests.lock().unwrap().push(request);
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
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: name.to_string(),
                    arguments: args,
                },
            }],
            route: None,
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
) -> (CodingRuntime, Database, tempfile::TempDir) {
    let (engine, db, dir, _) = harness_captured(approver, responses).await;
    (engine, db, dir)
}

async fn harness_captured(
    approver: Arc<dyn Approver>,
    responses: Vec<ModelResponse>,
) -> (
    CodingRuntime,
    Database,
    tempfile::TempDir,
    Arc<Mutex<Vec<ModelRequest>>>,
) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    std::fs::write(dir.path().join("README.md"), "# Project\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let db = Database::connect_in_memory().await.unwrap();
    let engine = CodingRuntime {
        engine: TaskEngine {
            stores: leveler_storage::EngineStores::from_database(&db),
            runtime_id: leveler_core::RuntimeId::new("rt-test"),
            boot: leveler_engine::EngineBoot {
                id: leveler_core::BootId::generate(),
                liveness: std::sync::Arc::new(leveler_test_support::TestBoots::new()),
            },
        },
        factory: ExecutorFactory {
            tool_discovery: None,
            resource_grants: std::sync::Arc::new(
                leveler_storage::MemoryResourceGrantStore::default(),
            ),
            runtime: Arc::new(MockRuntime::capturing(responses, Arc::clone(&requests))),
            registry: Arc::new(default_registry()),
            tool_context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: None,

            background_tasks: std::sync::Arc::new(leveler_execution::BackgroundTaskRegistry::new()),
            permission_rules: leveler_execution::PermissionRuleSet::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(std::path::PathBuf::from(".")),
            steering: None,
            allow_delegation: true,
            allow_host_input: true,
            capabilities: None,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Off,
        },
        approver,
        clarifier: Arc::new(AutoClarify),
        task_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    (engine, db, dir, requests)
}

fn direct_spec(dir: &Path) -> TaskSpec {
    TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "add a function".to_string(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: leveler_agent::StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(dir.to_path_buf()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
            // No gates: the resume turn can at best land CompletedUnverified, which
            // keeps these tests focused on the crash-window reconciliation.
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
    engine: &CodingRuntime,
    session: &SessionId,
    name: &str,
    arguments: String,
) {
    let turn = crashed_turn(db, session, "user").await;
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
            model_step: None,
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
    engine: &CodingRuntime,
    session: &SessionId,
    name: &str,
    arguments: String,
) {
    let turn = crashed_turn(db, session, "user").await;
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
            model_step: None,
        },
        &mut |_| {},
    )
    .await
    .unwrap();
    log.append(
        Some(&turn_id),
        EngineEvent::ApprovalRequested {
            grant: None,
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

/// A running turn left by a process that died: opened under the ownership of a
/// boot of this runtime that has since ended.
async fn crashed_turn(
    db: &Database,
    session: &SessionId,
    kind: &str,
) -> leveler_storage::TurnRecord {
    let stores = leveler_storage::EngineStores::from_database(db);
    let dead = TaskEngine {
        stores: stores.clone(),
        runtime_id: leveler_core::RuntimeId::new("rt-test"),
        boot: leveler_engine::EngineBoot {
            id: leveler_core::BootId::generate(),
            liveness: std::sync::Arc::new(leveler_test_support::TestBoots::new()),
        },
    };
    let token = dead.acquire_ownership(session).await.unwrap();
    stores
        .turns
        .start_owned(&token, session, kind, None, leveler_core::now())
        .await
        .unwrap()
}

/// No dangling call is replayed — not even a read-only one. Re-running it
/// would record a fresh answer for a call whose real outcome the crash left
/// unknown. Resume stops until the user acknowledges the crash window; the
/// acknowledgement records the call as explicitly unresolved.
#[tokio::test]
async fn a_dangling_read_tool_stops_resume_until_acknowledged() {
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

    let err = engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("an interrupted call's outcome is unknown, so resume must stop");
    assert!(
        matches!(
            &err,
            EngineError::RecoveryConfirmationRequired { call_id, tool }
                if call_id == "c1" && tool == "read_file"
        ),
        "unexpected error: {err:?}"
    );
    let events = recorded_events(&db, &session).await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            EngineEvent::ToolCallFinished { call_id, .. } if call_id == "c1"
        )),
        "recovery must not replay the call: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, EngineEvent::ApprovalRequested { .. })),
        "recovery must not route a replay through approval"
    );

    assert_eq!(engine.acknowledge_crash_window(&session).await.unwrap(), 1);
    engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect("resume proceeds after acknowledgement");

    let events = recorded_events(&db, &session).await;
    let finishes: Vec<(&str, bool)> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ToolCallFinished {
                call_id,
                is_error,
                preview,
                ..
            } if call_id == "c1" => Some((preview.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finishes.len(),
        1,
        "the acknowledged call gets exactly one honest marker: {events:?}"
    );
    assert!(
        finishes[0].1,
        "acknowledgement is not success: {finishes:?}"
    );
    assert!(
        finishes[0].0.contains("user-acknowledged crash recovery"),
        "the marker must say the outcome is unknown: {finishes:?}"
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

/// Unreadable arguments cannot excuse a replay. Since no call is replayed, they
/// also cannot make recovery decide differently: the interrupted call still
/// has an unknown outcome and waits for the user's acknowledgement.
#[tokio::test]
async fn corrupt_arguments_on_a_read_call_still_stop_for_acknowledgement() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(&db, &engine, &session, "read_file", "not valid json".into()).await;

    // Arguments are not parsed any more: recovery never replays a call, so a
    // corrupt payload changes nothing about the unknown outcome.
    let err = engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("the interrupted call still needs the user's acknowledgement");
    assert!(
        matches!(
            &err,
            EngineError::RecoveryConfirmationRequired { call_id, tool }
                if call_id == "c1" && tool == "read_file"
        ),
        "unexpected error: {err:?}"
    );

    let events = recorded_events(&db, &session).await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            EngineEvent::ToolCallFinished { call_id, .. } if call_id == "c1"
        )),
        "recovery must not record a result for the unreadable call: {events:?}"
    );
}

/// Old ToolCallStarted payloads have no persisted risk. Even if today's
/// registry classifies the tool as Safe, recovery must not reinterpret history.
#[tokio::test]
async fn legacy_call_without_persisted_risk_blocks_conservatively() {
    let (engine, db, dir) = harness(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    let turn = crashed_turn(&db, &session, "user").await;
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
                model_step: None,
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

/// Repeating the confirmation changes nothing: the first call closes the
/// dangling call, the second finds nothing to close and appends no row. A
/// duplicate terminal would make one interrupted call look like several and
/// would give the model two recovery facts for one unknown outcome.
#[tokio::test]
async fn acknowledging_the_crash_window_twice_closes_nothing_the_second_time() {
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

    assert_eq!(engine.acknowledge_crash_window(&session).await.unwrap(), 1);
    assert_eq!(
        engine.acknowledge_crash_window(&session).await.unwrap(),
        0,
        "a repeated acknowledgement must not append another terminal"
    );

    let events = recorded_events(&db, &session).await;
    let markers = events
        .iter()
        .filter(|event| event.is_user_acknowledged_crash_marker())
        .count();
    assert_eq!(markers, 1, "one interrupted call, one recovery fact");

    let log = EventLog::new(&db, session.clone());
    assert!(
        log.dangling_tool_calls().await.unwrap().is_empty(),
        "the acknowledged call is closed for snapshot reconstruction"
    );
    let acknowledged = log.acknowledged_crash_calls().await.unwrap();
    assert_eq!(acknowledged.len(), 1);
    assert_eq!(acknowledged[0].call_id, "c1");
    assert_eq!(acknowledged[0].name, "read_file");
}

/// The model must see the recovery fact, not just the user. The interrupted
/// call's tool result was never persisted, so without this notice the next
/// turn re-drives the same request blind — and may repeat a side effect the
/// crash already executed. The notice states the outcome is unknown and that
/// nothing was replayed; it claims neither success nor failure.
#[tokio::test]
async fn the_turn_after_acknowledgement_tells_the_model_the_call_was_not_replayed() {
    let (engine, db, dir, requests) =
        harness_captured(Arc::new(AutoApprove), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(
        &db,
        &engine,
        &session,
        "run_command",
        serde_json::json!({
            "program": "sh",
            "args": ["-c", "echo start >> ran.txt; sleep 8; echo end >> ran.txt"]
        })
        .to_string(),
    )
    .await;

    engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("the unacknowledged call blocks resume");
    assert!(
        requests.lock().unwrap().is_empty(),
        "a gated turn must not reach the model"
    );
    assert_eq!(engine.acknowledge_crash_window(&session).await.unwrap(), 1);

    engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect("resume must proceed after explicit acknowledgement");

    let requests = requests.lock().unwrap();
    assert!(
        !requests.is_empty(),
        "the resumed turn must reach the model"
    );
    let seen = requests[0]
        .messages
        .iter()
        .map(|message| message.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        "Recovered after an interruption",
        "run_command",
        "c1",
        "outcome unknown",
        "not replayed",
    ] {
        assert!(seen.contains(needle), "missing {needle:?} in:\n{seen}");
    }
    assert!(
        !seen.contains("Recovered after an interruption") || !seen.contains("was replayed"),
        "the notice must never present the call as replayed:\n{seen}"
    );
}

/// Acknowledging is finite recovery, not an execution: once the markers are
/// written the session is unowned at the acknowledgement's generation, and a
/// live sibling boot runs next without waiting for this boot to exit.
#[tokio::test]
async fn acknowledging_the_crash_window_leaves_the_session_unowned() {
    let (engine, db, _dir) = harness(Arc::new(AutoApprove), vec![]).await;
    let spec = direct_spec(_dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(&db, &engine, &session, "apply_patch", "{}".into()).await;

    assert_eq!(engine.acknowledge_crash_window(&session).await.unwrap(), 1);
    let stores = &engine.engine.stores;
    let task = stores
        .tasks
        .task_for_session(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stores.ownership.current(&task).await.unwrap().unwrap(),
        leveler_storage::TaskOwner {
            runtime: None,
            boot: None,
            epoch: leveler_core::OwnerEpoch::new(2),
        }
    );

    let boots = Arc::new(leveler_test_support::TestBoots::new());
    boots.set(&engine.engine.boot.id, leveler_core::BootLiveness::Alive);
    let sibling = TaskEngine {
        stores: stores.clone(),
        runtime_id: leveler_core::RuntimeId::new("rt-test"),
        boot: leveler_engine::EngineBoot {
            id: leveler_core::BootId::generate(),
            liveness: boots,
        },
    };
    let next = sibling.acquire_ownership(&session).await.unwrap();
    assert_eq!(next.owner_epoch.get(), 3);
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

/// Interactive chat applies the same rule as resume: a dangling call stops the
/// turn before the model runs, and the user's message is not consumed. After
/// acknowledgement the chat proceeds and the call is never replayed.
#[tokio::test]
async fn chat_stops_a_dangling_read_tool_until_acknowledged() {
    let (engine, db, dir, requests) =
        harness_captured(Arc::new(AutoApprove), vec![text("继续。")]).await;
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

    let ask = || {
        vec![leveler_model::ContentPart::Text {
            text: "继续".into(),
        }]
    };
    let err = engine
        .chat(
            &session,
            &spec,
            ask(),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect_err("a dangling read must stop chat until the user acknowledges it");
    assert!(
        matches!(
            &err,
            EngineError::RecoveryConfirmationRequired { call_id, tool }
                if call_id == "c1" && tool == "read_file"
        ),
        "unexpected error: {err:?}"
    );

    let events = recorded_events(&db, &session).await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            EngineEvent::ToolCallFinished { call_id, .. } if call_id == "c1"
        )),
        "chat must not replay the dangling read: {events:?}"
    );

    assert!(
        requests.lock().unwrap().is_empty(),
        "the gated chat must not reach the model"
    );
    engine.acknowledge_crash_window(&session).await.unwrap();
    engine
        .chat(
            &session,
            &spec,
            ask(),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("chat proceeds after acknowledgement");
    let seen = {
        let requests = requests.lock().unwrap();
        requests[0]
            .messages
            .iter()
            .map(|message| message.text_content())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        seen.contains("Recovered after an interruption") && seen.contains("(call c1)"),
        "interactive chat must carry the recovery context to the model:\n{seen}"
    );

    let events = recorded_events(&db, &session).await;
    let finishes: Vec<(&str, bool)> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ToolCallFinished {
                call_id,
                is_error,
                preview,
                ..
            } if call_id == "c1" => Some((preview.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finishes.len(),
        1,
        "the acknowledged call gets exactly one honest marker: {events:?}"
    );
    assert!(
        finishes[0].1,
        "acknowledgement is not success: {finishes:?}"
    );
    assert!(
        finishes[0].0.contains("user-acknowledged crash recovery"),
        "the marker must say the outcome is unknown: {finishes:?}"
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

/// Declaring itself replay-safe does not authorize recovery to re-run a tool:
/// `read_file` is the strongest case, and it still stops for confirmation.
#[tokio::test]
async fn a_declared_replay_safe_tool_is_not_replayed() {
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

    let err = engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("no tool may be replayed without the user's acknowledgement");
    assert!(
        matches!(
            &err,
            EngineError::RecoveryConfirmationRequired { call_id, tool }
                if call_id == "c1" && tool == "read_file"
        ),
        "unexpected error: {err:?}"
    );

    let events = recorded_events(&db, &session).await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            EngineEvent::ToolCallFinished { call_id, .. } if call_id == "c1"
        )),
        "the declared replay-safe read must not be re-run: {events:?}"
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

    let turn = crashed_turn(&db, &session, "user").await;
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
                model_step: None,
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
            exit_code: None,
            stop: None,
            call_id: "c1".into(),
            name: "apply_patch".into(),
            is_error: false,
            preview: "done".into(),
            agent_id: Some("agent-a".to_string()),
            applied_diff: None,
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

    let task = engine
        .engine
        .task_for_session(&session)
        .await
        .unwrap()
        .unwrap();
    // The crashed turn's boot already took the task once.
    let seeded = leveler_storage::OwnershipStore::current(&db, &task)
        .await
        .unwrap()
        .unwrap()
        .epoch;
    let rt = leveler_core::RuntimeId::new("rt-test");
    let stale = leveler_storage::OwnershipStore::acquire(
        &db,
        &task,
        &rt,
        &leveler_core::BootId::new("test-boot"),
        seeded,
    )
    .await
    .unwrap();
    // Reacquire: the first token is now stale.
    leveler_storage::OwnershipStore::acquire(
        &db,
        &task,
        &rt,
        &leveler_core::BootId::new("test-boot"),
        stale.owner_epoch,
    )
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

    let task = engine
        .engine
        .task_for_session(&session)
        .await
        .unwrap()
        .unwrap();
    // The crashed turn's boot already took the task once.
    let seeded = leveler_storage::OwnershipStore::current(&db, &task)
        .await
        .unwrap()
        .unwrap()
        .epoch;
    leveler_storage::OwnershipStore::acquire(
        &db,
        &task,
        &leveler_core::RuntimeId::new("rt-other"),
        &leveler_core::BootId::new("test-boot"),
        seeded,
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

/// A refused escalation leaves NO crash window behind.
///
/// This is the other half of the same rule the tests above enforce: a call
/// that started and never finished is treated as "may have run, may have left
/// a side effect", and that reading is correct — for a call that actually
/// crashed mid-flight. A command whose elevation the user denied never ran,
/// and the runtime knows it: the denial is right there in the log. Answering
/// only the model and leaving the announced call open turned that certainty
/// into an unknown, and every later window blocked on
/// `RecoveryConfirmationRequired` for a side effect that does not exist.
///
/// Found by the Unified Dogfood V1 gate (`LOST_TOOL_RESULTS`), on a run where
/// the model asked to write outside the workspace and was refused.
#[tokio::test]
async fn a_denied_escalation_leaves_no_dangling_call_behind() {
    let mut script = vec![tool_call(
        "c1",
        "run_command",
        serde_json::json!({
            "cmd": "echo ran > /tmp/outside.txt",
            "escalate": { "reason": "needs to write outside", "unrestricted_fs": true }
        }),
    )];
    // The drive keeps its round loop going after the refusal; honest stop
    // replies fill it out without adding a second call to reconcile.
    for _ in 0..10 {
        script.push(text("the elevation was refused; stopping"));
    }
    let (engine, db, dir) = harness(Arc::new(AutoDeny), script).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let log = EventLog::new(&db, session.clone());
    assert!(
        log.dangling_tool_calls().await.unwrap().is_empty(),
        "a refusal is a terminal fact: it must close the call it refused, not \
         leave a crash window the next resume has to reconcile"
    );
    let events = recorded_events(&db, &session).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            EngineEvent::ToolCallFinished {
                call_id,
                is_error: true,
                ..
            } if call_id == "c1"
        )),
        "the refusal must be durable as the call's terminal, not only in the \
         model's transcript"
    );
}

/// An interrupted OBSERVATION is not an unresolved side effect.
///
/// `wait_task` / `get_task` are `RiskLevel::Safe` but deliberately not
/// replayable: a replay would consume the background task's settlement exactly
/// once and could apply its rollback a second time. Halting the resume for them
/// protects nothing, because the crash destroyed only the tool's ANSWER — the
/// task registry still holds the task, and the model can simply ask again.
///
/// Found in a real TUI dogfood: a session killed while polling a long test
/// suite could not be resumed at all. The user's next message was refused with
/// "resume with --confirm-recovery", a flag the TUI resume path has no way to
/// pass — so that session was permanently unusable.
#[tokio::test]
async fn an_interrupted_observation_is_skipped_and_does_not_block_resume() {
    let (engine, db, dir) = harness(Arc::new(AutoDeny), resume_to_completion()).await;
    let spec = direct_spec(dir.path());
    let session = engine.create_task(&spec).await.unwrap();
    seed_transcript(&db, &session).await;
    seed_dangling_call(
        &db,
        &engine,
        &session,
        "wait_task",
        serde_json::json!({"task_id": "bg-1"}).to_string(),
    )
    .await;

    engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect("an interrupted observation must not block resume");

    let log = EventLog::new(&db, session.clone());
    assert!(
        log.dangling_tool_calls().await.unwrap().is_empty(),
        "the interrupted observation must be closed, not left dangling"
    );
    let events = recorded_events(&db, &session).await;
    let finished = events.iter().find_map(|e| match e {
        EngineEvent::ToolCallFinished {
            call_id, is_error, ..
        } if call_id == "c1" => Some(*is_error),
        _ => None,
    });
    assert_eq!(
        finished,
        Some(true),
        "a skipped observation is recorded as an errored result, never as a success"
    );
}
