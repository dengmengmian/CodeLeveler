//! End-to-end DirectStrategy tests (plan B3): a scripted model runtime drives
//! the engine and every side of persistence is asserted — turns, turn-stamped
//! messages, the append-only event log, and the terminal outcome column.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::coding::{CodingRuntime, ExecutorFactory, TaskSpec};
use leveler_agent::{AutoClarify, StopReason};
use leveler_core::{RequestId, ToolCallId};
use leveler_engine::{EngineEvent, ExecutionKind, TaskEngine, TaskOutcome};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::{
    Database, EventRepository, GoalStore, MessageRepository, SessionRepository, TurnRepository,
};
use leveler_tools::ToolContext;

/// The surface a real coding turn gets: the tool crate's composition plus the
/// harness controls THIS crate registers (`update_plan`). Production composes
/// the same two halves in `leveler-app`.
fn default_registry() -> leveler_tools::ToolRegistry {
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    registry
}

struct MockRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl MockRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Arc::new(Mutex::new(Vec::new())),
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
            },
            // Priced, so a run exercises the cost half of spend accounting
            // rather than leaving every row unpriced and every cost zero.
            "pricing": {
                "input_usd_per_mtok": 1.0,
                "output_usd_per_mtok": 4.0,
                "cached_input_usd_per_mtok": 0.1
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

fn patch_then_resolve() -> Vec<ModelResponse> {
    vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "added the function"}),
        ),
    ]
}

/// Understand JSON with a required AC that greps the patch fixture (`pub fn added`).
fn understand_met_required_ac() -> ModelResponse {
    let hint = grep_hint("pub fn added", "src/lib.rs");
    text(&format!(
        r#"{{"goal":"add a function","task_type":"feature","constraints":[],
        "acceptance_criteria":[{{"id":"AC-1","description":"added() exists",
        "verification_hint":"{hint}","required":true}}],
        "out_of_scope":[],"risk":"low","uncertainties":[]}}"#
    ))
}

/// Goal turn + understand that proves required acceptance (impl-class Verified path).
fn patch_resolve_and_proven_ac() -> Vec<ModelResponse> {
    let mut v = patch_then_resolve();
    v.push(understand_met_required_ac());
    v
}

struct Harness {
    engine: CodingRuntime,
    db: Database,
    dir: tempfile::TempDir,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

async fn harness(responses: Vec<ModelResponse>) -> Harness {
    harness_with(responses, PermissionProfile::Assisted, |_| {}).await
}

/// [`harness`], with the workspace profile and an extra setup pass over the
/// fresh workspace directory. Repository-operation fixtures need both: a real
/// git repo to move HEAD in, and the unrestricted profile that a `.git` write
/// only ever runs under.
async fn harness_with(
    responses: Vec<ModelResponse>,
    profile: PermissionProfile,
    setup: impl FnOnce(&std::path::Path),
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    setup(dir.path());
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context = ToolContext::with_environment(
        workspace,
        profile,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )),
    );
    let runtime = Arc::new(MockRuntime::new(responses));
    let requests = runtime.requests.clone();
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
            resource_grants: std::sync::Arc::new(
                leveler_storage::MemoryResourceGrantStore::default(),
            ),
            runtime,
            registry: Arc::new(default_registry()),
            tool_context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
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
            allow_host_input: true,
            capabilities: None,
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
        requests,
    }
}

/// A running turn left by a process that died: opened under the ownership of a
/// boot of this runtime that has since ended.
async fn crashed_turn(
    db: &Database,
    session: &leveler_core::SessionId,
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

#[tokio::test]
async fn factory_reasoning_override_reaches_every_model_request() {
    let mut h = harness(vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "done"}),
    )])
    .await;
    h.engine.factory.overrides = Some(leveler_agent::coding::ExecutionOverrides {
        reasoning_effort: Some(leveler_model::ReasoningEffort::High),
        ..leveler_agent::coding::ExecutionOverrides::default()
    });
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let requests = h.requests.lock().unwrap();
    assert!(!requests.is_empty());
    assert!(
        requests.iter().all(|request| {
            request.reasoning_effort == Some(leveler_model::ReasoningEffort::High)
        })
    );
}

#[tokio::test]
async fn main_reasoning_override_reaches_parent_requests_but_not_child_requests() {
    let mut h = harness(vec![
        tool_call(
            "s1",
            "spawn_agent",
            serde_json::json!({"task": "read src/lib.rs and report", "run_in_background": false}),
        ),
        text("child report: one function"),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "done"}),
        ),
    ])
    .await;
    h.engine.factory.overrides = Some(leveler_agent::coding::ExecutionOverrides {
        main_reasoning_effort: Some(leveler_model::ReasoningEffort::High),
        ..leveler_agent::coding::ExecutionOverrides::default()
    });
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    // Children never advertise spawn_agent; that is how the lanes separate.
    let requests = h.requests.lock().unwrap();
    let (parent, child): (Vec<_>, Vec<_>) = requests
        .iter()
        .partition(|request| request.tools.iter().any(|t| t.name == "spawn_agent"));
    assert!(!parent.is_empty() && !child.is_empty(), "both lanes ran");
    assert!(
        parent
            .iter()
            .all(|r| r.reasoning_effort == Some(leveler_model::ReasoningEffort::High))
    );
    // The mock profile declares no effort: a child keeps that default.
    assert!(child.iter().all(|r| r.reasoning_effort.is_none()));
}

/// A TerminalStore that always refuses to commit — engine-level failure
/// injection for "the terminal fact is atomic or absent".
#[derive(Clone, Copy)]
enum TerminalFailure {
    Task,
    Turn,
}

struct FailingTerminal {
    inner: Database,
    failure: TerminalFailure,
}

#[async_trait]
impl leveler_storage::TerminalStore for FailingTerminal {
    async fn finish_task(
        &self,
        session_id: &leveler_core::SessionId,
        event_type: &str,
        payload: &str,
        outcome: leveler_engine::TaskOutcome,
        status: leveler_lifecycle::SessionStatus,
        state: leveler_lifecycle::AgentState,
        now: leveler_core::Timestamp,
    ) -> Result<leveler_storage::EventRecord, leveler_storage::StorageError> {
        if matches!(self.failure, TerminalFailure::Task) {
            return Err(leveler_storage::StorageError::InvalidData(
                "injected task terminal failure".into(),
            ));
        }
        leveler_storage::TerminalStore::finish_task(
            &self.inner,
            session_id,
            event_type,
            payload,
            outcome,
            status,
            state,
            now,
        )
        .await
    }

    async fn finish_turn(
        &self,
        session_id: &leveler_core::SessionId,
        turn_id: &leveler_core::TurnId,
        event_type: &str,
        payload: &str,
        outcome: leveler_engine::TurnOutcome,
        now: leveler_core::Timestamp,
    ) -> Result<leveler_storage::EventRecord, leveler_storage::StorageError> {
        if matches!(self.failure, TerminalFailure::Turn) {
            return Err(leveler_storage::StorageError::InvalidData(
                "injected turn terminal failure".into(),
            ));
        }
        leveler_storage::TerminalStore::finish_turn(
            &self.inner,
            session_id,
            turn_id,
            event_type,
            payload,
            outcome,
            now,
        )
        .await
    }

    async fn finish_task_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &leveler_core::SessionId,
        event_type: &str,
        payload: &str,
        outcome: leveler_engine::TaskOutcome,
        status: leveler_lifecycle::SessionStatus,
        state: leveler_lifecycle::AgentState,
        goal: Option<&leveler_storage::GoalTerminalUpdate>,
        now: leveler_core::Timestamp,
    ) -> Result<leveler_storage::TaskTerminalCommit, leveler_storage::OwnershipError> {
        if matches!(self.failure, TerminalFailure::Task) {
            return Err(leveler_storage::OwnershipError::Storage(
                leveler_storage::StorageError::InvalidData("injected task terminal failure".into()),
            ));
        }
        leveler_storage::TerminalStore::finish_task_owned(
            &self.inner,
            token,
            session_id,
            event_type,
            payload,
            outcome,
            status,
            state,
            goal,
            now,
        )
        .await
    }

    async fn finish_turn_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &leveler_core::SessionId,
        turn_id: &leveler_core::TurnId,
        event_type: &str,
        payload: &str,
        outcome: leveler_engine::TurnOutcome,
        now: leveler_core::Timestamp,
    ) -> Result<leveler_storage::EventRecord, leveler_storage::OwnershipError> {
        if matches!(self.failure, TerminalFailure::Turn) {
            return Err(leveler_storage::OwnershipError::Storage(
                leveler_storage::StorageError::InvalidData("injected turn terminal failure".into()),
            ));
        }
        leveler_storage::TerminalStore::finish_turn_owned(
            &self.inner,
            token,
            session_id,
            turn_id,
            event_type,
            payload,
            outcome,
            now,
        )
        .await
    }
}

/// A MessageStore whose appends fail — the transcript is not durable and the
/// runtime must never pretend it is.
struct FailingMessages;

#[async_trait]
impl leveler_storage::MessageStore for FailingMessages {
    async fn append_in_turn(
        &self,
        _: &leveler_core::SessionId,
        _: &leveler_core::TurnId,
        _: &[String],
        _: leveler_core::Timestamp,
    ) -> Result<(), leveler_storage::StorageError> {
        Err(leveler_storage::StorageError::InvalidData(
            "injected transcript failure".into(),
        ))
    }

    async fn load(
        &self,
        _: &leveler_core::SessionId,
    ) -> Result<Vec<String>, leveler_storage::StorageError> {
        Ok(Vec::new())
    }

    async fn append_in_turn_owned(
        &self,
        _: &leveler_core::OwnershipToken,
        _: &leveler_core::SessionId,
        _: &leveler_core::TurnId,
        _: &[String],
        _: leveler_core::Timestamp,
    ) -> Result<(), leveler_storage::OwnershipError> {
        Err(leveler_storage::OwnershipError::Storage(
            leveler_storage::StorageError::InvalidData("injected transcript failure".into()),
        ))
    }

    async fn ensure_initiating_message_owned(
        &self,
        _: &leveler_core::OwnershipToken,
        _: &leveler_core::SessionId,
        _: &leveler_core::TurnId,
        _: &str,
        _: leveler_core::Timestamp,
    ) -> Result<bool, leveler_storage::OwnershipError> {
        Err(leveler_storage::OwnershipError::Storage(
            leveler_storage::StorageError::InvalidData("injected transcript failure".into()),
        ))
    }
}

/// Failure injection A/B at the engine level: when the atomic terminal commit
/// fails after the turn has settled, the run errors, and NEITHER TaskFinished
/// NOR the task outcome projection is visible — no half-commit.
#[tokio::test]
async fn a_failed_terminal_commit_leaves_no_half_visible_task_fact() {
    let mut h = harness(vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "done"}),
    )])
    .await;
    h.engine.engine.stores.terminal = Arc::new(FailingTerminal {
        inner: h.db.clone(),
        failure: TerminalFailure::Task,
    });
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();

    let result = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await;
    assert!(
        matches!(
            result,
            Err(leveler_engine::EngineError::TerminalCommitFailed(_))
        ),
        "a failed terminal commit must propagate as a nonterminal recovery fault: {result:?}"
    );

    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, None, "no outcome projection without its event");
    let rows = EventRepository::new(&h.db).load(&session).await.unwrap();
    assert_eq!(
        rows.iter()
            .filter(|event| event.event_type == "task_finished")
            .count(),
        0,
        "no task terminal event without its projection"
    );
    assert_eq!(
        rows.iter()
            .filter(|event| event.event_type == "turn_finished")
            .count(),
        1,
        "the already committed turn boundary remains truthful"
    );
}

/// A turn that cannot commit its own durable terminal leaves an open recovery
/// boundary. The task authority must not paper over that hole with Failed.
#[tokio::test]
async fn an_uncommitted_turn_terminal_prevents_the_task_terminal() {
    let mut h = harness(patch_resolve_and_proven_ac()).await;
    h.engine.engine.stores.terminal = Arc::new(FailingTerminal {
        inner: h.db.clone(),
        failure: TerminalFailure::Turn,
    });
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();

    let result = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await;
    assert!(
        matches!(
            result,
            Err(leveler_engine::EngineError::UnclosedTerminalBoundary(_))
        ),
        "the open turn must remain a recovery fault: {result:?}"
    );

    let rows = EventRepository::new(&h.db).load(&session).await.unwrap();
    assert!(
        rows.iter().all(|event| event.event_type != "turn_finished"
            && event.event_type != "task_finished"),
        "no terminal may be synthesized past the open turn: {rows:?}"
    );
    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].status, "running");
}

/// Failure injection C: when the transcript append fails, the turn fails
/// loudly (AgentError::Persistence) and the task lands Failed — the runtime
/// never continues as if the transcript were durable.
#[tokio::test]
async fn a_failed_transcript_append_fails_the_turn_loudly() {
    let mut h = harness(vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "done"}),
    )])
    .await;
    h.engine.engine.stores.messages = Arc::new(FailingMessages);
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();

    let result = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await;
    let error = result.expect_err("an un-durable transcript must fail the run");
    assert!(
        error.to_string().contains("injected transcript failure"),
        "the persistence cause must be named: {error}"
    );
    // The terminal store still works, so the failure is committed honestly.
    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Failed));
}

/// A model runtime that hijacks task ownership (CAS to another runtime)
/// before answering with a mutating tool call — Scenario H's deterministic
/// "stale before dispatch" injection.
struct HijackingRuntime {
    inner: MockRuntime,
    db: Database,
    session: Arc<std::sync::OnceLock<leveler_core::SessionId>>,
    hijacked: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl ModelRuntime for HijackingRuntime {
    async fn generate(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        if !self
            .hijacked
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            // Steal ownership via legitimate CAS: read current, acquire as a
            // different runtime. The engine's token is now stale.
            let session = self.session.get().expect("session registered").clone();
            let task = leveler_storage::TaskStore::task_for_session(&self.db, &session)
                .await
                .unwrap()
                .expect("task exists");
            let current = leveler_storage::OwnershipStore::current(&self.db, &task)
                .await
                .unwrap()
                .unwrap();
            leveler_storage::OwnershipStore::acquire(
                &self.db,
                &task,
                &leveler_core::RuntimeId::new("rt-hijacker"),
                &leveler_core::BootId::new("test-boot"),
                current.epoch,
            )
            .await
            .unwrap();
        }
        self.inner.generate(request, cancellation).await
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let response = self.generate(request, cancellation).await?;
        Ok(leveler_model::stream_from_response(response))
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        self.inner.profile(model).await
    }
}

/// Scenario H: ownership is lost between acquisition and the first tool
/// dispatch. ToolHost's fence must refuse the dispatch — the mutating tool
/// never runs — and the stale runtime writes no terminal fact.
#[tokio::test]
async fn stale_ownership_prevents_tool_dispatch() {
    let mut h = harness(patch_then_resolve()).await;
    let session_cell = Arc::new(std::sync::OnceLock::new());
    h.engine.factory.runtime = Arc::new(HijackingRuntime {
        inner: MockRuntime::new(patch_then_resolve()),
        db: h.db.clone(),
        session: session_cell.clone(),
        hijacked: std::sync::atomic::AtomicBool::new(false),
    });
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    session_cell.set(session.clone()).unwrap();

    let result = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await;
    let error = result.expect_err("a stale runtime must abort");
    // Two independent gates can fire first, both typed stale: the fenced
    // canonical append (persist-before-side-effect already refuses to record
    // the call) or the ToolHost ownership fence. Either way the run aborts
    // with a named ownership failure.
    let text = error.to_string();
    assert!(
        text.contains("stale runtime ownership") || text.contains("stale ownership for task"),
        "the failure must be a named ownership fence, got: {error}"
    );
    // The mutating tool NEVER executed.
    let source = std::fs::read_to_string(h.dir.path().join("src/lib.rs")).unwrap();
    assert!(
        !source.contains("pub fn added"),
        "apply_patch must not have run: {source}"
    );
    // The stale runtime wrote no terminal fact.
    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, None, "a stale runtime stamps no outcome");
}

/// Scenario J: same runtime restarts — reacquire advances the epoch, the old
/// token is fenced, and recovery proceeds under the new token.
#[tokio::test]
async fn restart_reacquires_a_fresh_epoch_and_fences_the_old_token() {
    let h = harness(Vec::new()).await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    let task = h
        .engine
        .engine
        .task_for_session(&session)
        .await
        .unwrap()
        .unwrap();
    let rt = leveler_core::RuntimeId::new("rt-test");
    let old = leveler_storage::OwnershipStore::acquire(
        &h.db,
        &task,
        &rt,
        &leveler_core::BootId::new("test-boot"),
        leveler_core::OwnerEpoch::UNOWNED,
    )
    .await
    .unwrap();
    // A crash left a running turn started under the old incarnation.
    leveler_storage::TurnStore::start_owned(
        &h.db,
        &old,
        &session,
        "user",
        None,
        leveler_core::now(),
    )
    .await
    .unwrap();
    let reap = leveler_engine::reap_after_restart(
        &h.engine.engine,
        None,
        leveler_engine::ReapScope::EndedBoots,
    )
    .await
    .unwrap();
    assert_eq!(reap.events.len(), 1, "the orphan turn is reaped");
    assert!(reap.conflicts.is_empty());
    // The old token is now stale (epoch advanced by the reacquire).
    assert!(
        leveler_storage::EventStore::append_owned(
            &h.db,
            &old,
            &session,
            None,
            "task_started",
            "{}",
            leveler_core::now()
        )
        .await
        .is_err(),
        "the pre-restart token must be powerless"
    );
}

/// Scenario K: a task owned by another runtime is never reaped or run —
/// explicit conflict, no mutation.
#[tokio::test]
async fn a_foreign_owned_task_is_reported_not_touched() {
    let h = harness(Vec::new()).await;
    let mut spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    let task = h
        .engine
        .engine
        .task_for_session(&session)
        .await
        .unwrap()
        .unwrap();
    let other = leveler_core::RuntimeId::new("rt-other");
    let foreign = leveler_storage::OwnershipStore::acquire(
        &h.db,
        &task,
        &other,
        &leveler_core::BootId::new("test-boot"),
        leveler_core::OwnerEpoch::UNOWNED,
    )
    .await
    .unwrap();
    leveler_storage::TurnStore::start_owned(
        &h.db,
        &foreign,
        &session,
        "user",
        None,
        leveler_core::now(),
    )
    .await
    .unwrap();
    spec.coding.mode = PermissionProfile::FullAccess;
    spec.coding.sandbox = true;

    // Restart reap as rt-test: conflict reported, turn untouched.
    let reap = leveler_engine::reap_after_restart(
        &h.engine.engine,
        None,
        leveler_engine::ReapScope::EndedBoots,
    )
    .await
    .unwrap();
    assert!(reap.events.is_empty());
    assert_eq!(reap.conflicts.len(), 1);
    assert_eq!(
        leveler_storage::TurnStore::list_running(&h.db, Some(&session))
            .await
            .unwrap()
            .len(),
        1,
        "the foreign turn must still be running"
    );
    // Running the task from this engine is an explicit conflict, not a steal.
    let error = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("must refuse a foreign-owned task");
    assert!(
        error.to_string().contains("owned by runtime"),
        "conflict must be named: {error}"
    );
    assert!(
        leveler_storage::GoalStore::unfinished(&h.db)
            .await
            .unwrap()
            .is_empty(),
        "ownership rejection must happen before the Coding harness opens a goal"
    );
    assert_eq!(
        leveler_storage::SessionStore::execution(&h.db, &session)
            .await
            .unwrap(),
        Some(("assisted".into(), false, "direct".into(), None)),
        "ownership rejection must happen before execution config changes"
    );
}

#[tokio::test]
async fn create_task_records_the_durable_task_association() {
    let h = harness(Vec::new()).await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();

    let task = h
        .engine
        .engine
        .task_for_session(&session)
        .await
        .unwrap()
        .expect("create_task must record the task association");
    assert_eq!(
        leveler_storage::TaskStore::session_for_task(&h.db, &task)
            .await
            .unwrap()
            .as_ref(),
        Some(&session),
        "the association must read back in both directions"
    );
}

#[tokio::test]
async fn running_a_legacy_session_backfills_its_task_and_stamps_task_started() {
    let mut h = harness(vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "done"}),
    )])
    .await;
    h.engine.factory.allow_delegation = false;
    let spec = spec(&h);
    // A session written by an older binary: session row only, no task row.
    let record = leveler_storage::SessionRecord::new(
        h.dir.path().display().to_string(),
        "add a function",
        "mock/m",
        leveler_core::now(),
    );
    SessionRepository::new(&h.db).create(&record).await.unwrap();
    let session = leveler_core::SessionId::new(record.id);
    assert_eq!(
        h.engine.engine.task_for_session(&session).await.unwrap(),
        None
    );

    let mut events = Vec::new();
    h.engine
        .run(
            &session,
            &spec,
            &mut |e| events.push(e),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let task = h
        .engine
        .engine
        .task_for_session(&session)
        .await
        .unwrap()
        .expect("running must ensure the task association");
    let stamped = events.iter().find_map(|e| match e {
        EngineEvent::TaskStarted { task_id, .. } => Some(task_id.clone()),
        _ => None,
    });
    assert_eq!(
        stamped,
        Some(Some(task)),
        "TaskStarted must carry the durable task id"
    );
}

fn spec(h: &Harness) -> TaskSpec {
    TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "add a function".to_string(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: leveler_agent::StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(h.dir.path().to_path_buf()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    }
}

/// `grep`-style acceptance hint for the platform's shell (`sh -c` on Unix,
/// `cmd /c` on Windows), already JSON-escaped for the understand fixture.
fn grep_hint(needle: &str, file: &str) -> String {
    if cfg!(windows) {
        // findstr parses `/` in the file argument as option switches, and the
        // JSON fixture escapes both the quotes and the path backslashes.
        format!("findstr \\\"{needle}\\\" {}", file.replace('/', "\\\\"))
    } else {
        format!("grep -q '{needle}' {file}")
    }
}

#[tokio::test]
async fn direct_run_persists_turns_messages_events_and_outcome() {
    // Impl-class Verified requires proven Met required AC (not empty fallback).
    let h = harness(patch_resolve_and_proven_ac()).await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();

    let mut seen: Vec<EngineEvent> = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &spec,
            &mut |e| seen.push(e),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Completed);
    assert_eq!(report.modified_files, vec!["src/lib.rs".to_string()]);

    // Session row: execution config + terminal outcome.
    let (mode, sandbox, kind, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (mode.as_str(), sandbox, kind.as_str(), outcome),
        ("assisted", false, "direct", Some(TaskOutcome::Completed))
    );

    // One user turn, completed, owning the transcript messages.
    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        (turns[0].kind.as_str(), turns[0].status.as_str()),
        ("user", "completed")
    );
    assert!(turns[0].finished_at.is_some());
    let turn_id = leveler_core::TurnId::new(turns[0].id.clone());
    let turn_messages = MessageRepository::new(&h.db)
        .load_for_turn(&session, &turn_id)
        .await
        .unwrap();
    assert!(
        !turn_messages.is_empty(),
        "the transcript must be stamped with the turn id"
    );

    // The event log: ordered, persisted, and shaped as expected.
    let rows = EventRepository::new(&h.db).load(&session).await.unwrap();
    let types: Vec<&str> = rows.iter().map(|r| r.event_type.as_str()).collect();
    assert_eq!(types.first(), Some(&"task_started"));
    assert_eq!(types.last(), Some(&"task_finished"));
    for expected in [
        "turn_started",
        "tool_call_started",
        "tool_call_finished",
        "turn_finished",
    ] {
        assert!(types.contains(&expected), "missing {expected} in {types:?}");
    }
    let sequences: Vec<i64> = rows.iter().map(|r| r.sequence).collect();
    assert_eq!(
        sequences,
        (1..=rows.len() as i64).collect::<Vec<_>>(),
        "sequences must be gapless"
    );

    // The observer saw the same terminal event (persist-before-forward held).
    assert!(seen.iter().any(|e| matches!(
        e,
        EngineEvent::TaskFinished {
            outcome: TaskOutcome::Completed,
            ..
        }
    )));
}

#[cfg(any())]
#[tokio::test]
async fn no_gates_means_completed_with_verification_not_run() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::Completed);
    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Completed));
}

/// Pure Q&A (no mutations) with a green gate plan: the run completed and no
/// check ran — the repo being healthy is not a verdict about the answer.
#[cfg(any())]
#[tokio::test]
async fn pure_qa_with_green_gates_is_completed_with_verification_not_run() {
    let h = harness(vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "auth uses JWT sessions"}),
    )])
    .await;
    let mut s = spec(&h);
    s.runtime.goal = "explain how auth works".to_string();
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Completed);
    assert!(
        report.modified_files.is_empty(),
        "Q&A must not leave mutations: {:?}",
        report.modified_files
    );
    assert!(
        report.verification.is_none(),
        "no mutation: the checks are not run"
    );
    assert_eq!(
        report.verification_status,
        leveler_lifecycle::VerificationStatus::NotRun
    );
}

/// The reported defect, end to end: a branch switch succeeds, the project has
/// a RED test gate, and the run must still finish with no verdict — because a
/// repository operation authors nothing, so the project's source checks were
/// never owed.
///
/// Before the fix the tree diff around `git switch` reported every file that
/// differs between the branches, the engine read that as a source change, ran
/// the plan, and the turn ended `⚠ 已完成 · 验证未通过 · test` on a task that
/// had written nothing.
#[cfg(unix)]
#[cfg(any())]
#[tokio::test]
async fn a_branch_switch_does_not_inherit_the_projects_test_gate() {
    let h = harness_with(
        vec![
            tool_call(
                "c1",
                "run_command",
                serde_json::json!({"program": "git", "args": ["switch", "feat"]}),
            ),
            tool_call(
                "g1",
                "update_goal",
                serde_json::json!({"status": "complete", "summary": "switched to feat"}),
            ),
        ],
        PermissionProfile::FullAccess,
        |dir| {
            use leveler_test_support::git::{init_repo, run};
            init_repo(dir);
            run(dir, &["add", "-A"]);
            run(dir, &["commit", "-qm", "init"]);
            run(dir, &["switch", "-qc", "feat"]);
            std::fs::write(dir.join("src/lib.rs"), "pub fn new() {}\n").unwrap();
            std::fs::write(dir.join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
            run(dir, &["add", "-A"]);
            run(dir, &["commit", "-qm", "feat"]);
            run(dir, &["switch", "-q", "main"]);
        },
    )
    .await;
    let mut s = spec(&h);
    s.coding.mode = PermissionProfile::FullAccess;
    s.runtime.goal = "切换到 feat 分支".to_string();

    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(h.dir.path().join("src/lib.rs")).unwrap(),
        "pub fn new() {}\n",
        "the switch must actually have happened"
    );
    assert_eq!(report.outcome, TaskOutcome::Completed);
    assert!(
        report.modified_files.is_empty(),
        "a branch switch authors nothing: {:?}",
        report.modified_files
    );
    assert_eq!(
        report.verification_status,
        leveler_lifecycle::VerificationStatus::NotRun,
        "a red source gate is not owed by a repository operation"
    );
}

/// Case 2: real edits, the project's checks pass over the final tree, and
/// the model declared completion → Completed with checks Passed. No hidden
/// judge, reviewer, or repair turn runs.
#[cfg(any())]
#[tokio::test]
async fn edits_with_green_gates_complete_with_checks_passed() {
    let h = harness(patch_resolve_and_proven_ac()).await;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Completed);
    assert_eq!(
        report.verification_status,
        leveler_lifecycle::VerificationStatus::Passed
    );
    assert!(!report.modified_files.is_empty());
    assert!(report.verification.is_some());
    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    assert_eq!(
        turns.iter().map(|t| t.kind.as_str()).collect::<Vec<_>>(),
        vec!["user"],
        "exactly one turn: no repair turn was opened on the model's behalf"
    );
}

/// Green gates + real mutation is Verified even when the model never produced
/// usable acceptance criteria.
///
/// The gate ran the project's own checks against the edited tree — that is the
/// evidence. Requiring a *proven* criterion on top of it meant a model that
/// merely failed to restate its goal turned a correct, fully green turn into
/// "有改动但缺少系统级验收背书".
#[cfg(any())]
#[tokio::test]
async fn impl_green_gates_are_verified_without_proven_acceptance() {
    // No understand response → fallback optional AC → no proven required Met.
    let h = harness(patch_then_resolve()).await;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::Completed);
    assert_eq!(
        report.verification_status,
        leveler_lifecycle::VerificationStatus::Passed
    );
}

/// Delete a workspace file; understand fails (no response) → mutation-derived
/// `test ! -e` proves absence → Verified despite optional fallback AC.
#[cfg(any())]
#[tokio::test]
async fn delete_file_with_green_gates_and_no_understand_is_verified() {
    let responses = vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Delete File: quicksort.py\n*** End Patch"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "deleted quicksort.py"}),
        ),
        // no understand response → fallback + mutation-derived AC
    ];
    let h = harness(responses).await;
    std::fs::write(h.dir.path().join("quicksort.py"), "def qs(): pass\n").unwrap();
    let mut s = spec(&h);
    s.runtime.goal = "delete quicksort.py".to_string();
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert!(
        !std::path::Path::new(&h.dir.path().join("quicksort.py")).exists(),
        "file must be gone on disk"
    );
    assert_eq!(report.outcome, TaskOutcome::Completed);
    assert_eq!(
        report.verification_status,
        leveler_lifecycle::VerificationStatus::Passed
    );
    assert!(
        report
            .modified_files
            .iter()
            .any(|p| p.contains("quicksort.py")),
        "modified_files should track delete: {:?}",
        report.modified_files
    );
    assert!(report.outcome.is_completed());
}

#[tokio::test]
async fn top_level_goal_runs_until_terminal_past_the_old_model_round_budget() {
    let h = harness(vec![
        tool_call("c1", "list_files", serde_json::json!({"path": "."})),
        tool_call("c2", "list_files", serde_json::json!({"path": "src"})),
        tool_call("c3", "read_file", serde_json::json!({"path": "src/lib.rs"})),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "inspection complete"}),
        ),
    ])
    .await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.model_steps, 4);
}

#[tokio::test]
async fn bounded_eval_goal_still_stops_at_the_case_round_limit() {
    let h = harness(vec![
        tool_call("c1", "list_files", serde_json::json!({"path": "."})),
        tool_call("c2", "list_files", serde_json::json!({"path": "src"})),
        tool_call("c3", "read_file", serde_json::json!({"path": "src/lib.rs"})),
    ])
    .await;
    let mut spec = spec(&h);
    spec.runtime.continuation = leveler_agent::ContinuationPolicy::bounded(2);
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::BudgetLimited);
    assert_eq!(report.stop_reason, StopReason::BudgetExhausted);
    assert_eq!(report.model_steps, 2);
}

#[tokio::test]
async fn direct_budget_stop_preserves_the_executor_detail() {
    let h = harness(vec![text("never reached")]).await;
    let mut spec = spec(&h);
    spec.runtime.limits.max_duration = Some(std::time::Duration::ZERO);
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::BudgetLimited);
    assert_eq!(report.stop_reason, StopReason::BudgetExhausted);
    assert!(
        report
            .stop_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("dimension=duration") && detail.contains("cap=0")),
        "executor budget detail must survive into TaskReport: {report:?}"
    );
}

#[tokio::test]
async fn agent_failure_persists_terminal_task_and_turn_events() {
    // No scripted response makes the first model request fail inside the turn.
    // The query projections already become failed; the canonical log must carry
    // the same terminal facts so replay cannot disagree with those projections.
    let h = harness(Vec::new()).await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();

    let error = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("an exhausted model runtime must fail the task");
    assert!(matches!(
        error,
        leveler_engine::EngineError::Execution { .. }
    ));

    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Failed));
    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].status, "failed");

    let events = EventRepository::new(&h.db)
        .load(&session)
        .await
        .unwrap()
        .into_iter()
        .map(|row| EngineEvent::from_payload(&row.payload).unwrap())
        .collect::<Vec<_>>();
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::TurnFinished {
                turn_id,
                outcome: leveler_engine::TurnOutcome::Failed,
                ..
            } if turn_id.as_str() == turns[0].id
        )),
        "a failed turn must have a canonical terminal event: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Failed,
                ..
            }
        )),
        "a failed task must have a canonical terminal event: {events:?}"
    );
}

#[tokio::test]
async fn cancellation_is_recorded_as_interrupted() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();

    let token = CancellationToken::new();
    token.cancel();
    let err = h
        .engine
        .run(&session, &spec, &mut |_| {}, token)
        .await
        .expect_err("a pre-cancelled run must not succeed");
    assert!(matches!(err, leveler_engine::EngineError::Cancelled));

    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Interrupted));
    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    assert_eq!(turns[0].status, "interrupted");
}

/// `leveler run`'s default 200-round window changes only where a run that
/// never resolves stops: completion and cancellation keep their own terminals.
#[tokio::test]
async fn a_wide_round_window_keeps_completion_and_cancellation_terminals() {
    let window = |h: &Harness| {
        let mut s = spec(h);
        s.runtime.continuation = leveler_agent::ContinuationPolicy::bounded(200);
        s
    };

    let h = harness(patch_then_resolve()).await;
    let spec = window(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Completed));

    let h = harness(patch_then_resolve()).await;
    let spec = window(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let err = h
        .engine
        .run(&session, &spec, &mut |_| {}, token)
        .await
        .expect_err("a cancelled run must not succeed");
    assert!(matches!(err, leveler_engine::EngineError::Cancelled));
    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Interrupted));
}

#[tokio::test]
async fn cancellation_at_the_terminal_publish_boundary_wins_over_completion() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();

    let cancellation = CancellationToken::new();
    let signal = cancellation.clone();
    let mut reached_publish = false;
    let err = h
        .engine
        .run(
            &session,
            &spec,
            &mut |event| {
                if matches!(
                    event,
                    EngineEvent::FinalizationPhaseStarted { ref phase, .. }
                        if phase == "publishing_terminal"
                ) {
                    reached_publish = true;
                    signal.cancel();
                }
            },
            cancellation,
        )
        .await
        .expect_err("a cancellation observed immediately before commit must win");
    assert!(matches!(err, leveler_engine::EngineError::Cancelled));
    assert!(reached_publish, "test must reach the final commit boundary");

    let events = EventRepository::new(&h.db)
        .load(&session)
        .await
        .unwrap()
        .into_iter()
        .map(|row| EngineEvent::from_payload(&row.payload).unwrap())
        .collect::<Vec<_>>();
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::TaskFinished {
            outcome: TaskOutcome::Interrupted,
            ..
        }
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        EngineEvent::TaskFinished {
            outcome: TaskOutcome::Completed,
            ..
        }
    )));
}

/// Kill -9 / unclean TUI exit can leave a permanent `running` turn. Starting a
/// new turn must reap that zombie before inserting the next row.
#[tokio::test]
async fn starting_a_turn_reaps_orphan_running_siblings() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();

    // Simulate a zombie left by process kill: status running, no finished_at.
    let zombie = crashed_turn(&h.db, &session, "chat").await;
    assert_eq!(zombie.status, "running");
    assert!(zombie.finished_at.is_none());

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::Completed);

    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    assert!(
        turns.len() >= 2,
        "zombie + at least one new turn, got {}",
        turns.len()
    );
    let zombie_row = turns.iter().find(|t| t.id == zombie.id).unwrap();
    assert_eq!(
        zombie_row.status, "interrupted",
        "orphan running turn must be reaped before the next turn starts"
    );
    assert!(zombie_row.finished_at.is_some());
    assert!(
        turns.iter().any(|t| t.status == "completed"),
        "new turn must complete: {:?}",
        turns
            .iter()
            .map(|t| (t.kind.as_str(), t.status.as_str()))
            .collect::<Vec<_>>()
    );
    assert!(
        turns
            .iter()
            .all(|t| t.status != "running" || t.finished_at.is_some()),
        "no permanent running zombies should remain"
    );
    let events = EventRepository::new(&h.db)
        .load(&session)
        .await
        .unwrap()
        .into_iter()
        .map(|row| EngineEvent::from_payload(&row.payload).unwrap())
        .collect::<Vec<_>>();
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::TurnFinished {
                turn_id,
                outcome: leveler_engine::TurnOutcome::Interrupted,
                ..
            } if turn_id.as_str() == zombie.id
        )),
        "reaping must leave a canonical interruption event: {events:?}"
    );
}

#[tokio::test]
async fn interrupted_direct_task_resumes_from_the_persisted_transcript() {
    // Phase 1: interrupt immediately — the seed transcript persists, the
    // session ends `interrupted`.
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let _ = h
        .engine
        .run(&session, &spec, &mut |_| {}, token)
        .await
        .expect_err("pre-cancelled");
    let before = MessageRepository::new(&h.db).load(&session).await.unwrap();
    assert!(!before.is_empty(), "the seed must have been persisted");
    let owed = GoalStore::unfinished(&h.db).await.unwrap();
    assert_eq!(owed.len(), 1, "the interrupted run leaves one owed goal");
    let goal_id = owed[0].id.clone();
    let task_id = owed[0].task_id.clone();
    assert_eq!(owed[0].windows_run, 1);

    // Phase 2: resume on the same database with a fresh scripted runtime.
    let dir2 = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir2.path().join("src")).unwrap();
    std::fs::write(dir2.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(dir2.path()).unwrap();
    let engine2 = CodingRuntime {
        engine: TaskEngine {
            stores: leveler_storage::EngineStores::from_database(&h.db),
            runtime_id: leveler_core::RuntimeId::new("rt-test"),
            boot: leveler_engine::EngineBoot {
                id: leveler_core::BootId::generate(),
                liveness: std::sync::Arc::new(leveler_test_support::TestBoots::new()),
            },
        },
        factory: ExecutorFactory {
            resource_grants: std::sync::Arc::new(
                leveler_storage::MemoryResourceGrantStore::default(),
            ),
            runtime: Arc::new(MockRuntime::new(patch_then_resolve())),
            registry: Arc::new(default_registry()),
            tool_context: ToolContext::with_environment(
                workspace,
                PermissionProfile::Assisted,
                Arc::new(leveler_core::EnvSnapshot::new(
                    std::env::vars_os(),
                    std::env::current_dir().unwrap_or_default(),
                    std::env::temp_dir(),
                )),
            ),
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
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
            allow_host_input: true,
            capabilities: None,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Off,
            develop_model: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        task_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let spec2 = TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "add a function".to_string(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: leveler_agent::StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(dir2.path().to_path_buf()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    };

    let report = engine2
        .resume(&session, &spec2, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::Completed);

    // Two turns: the interrupted original and the completed resume.
    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    let statuses: Vec<&str> = turns.iter().map(|t| t.status.as_str()).collect();
    assert_eq!(statuses, vec!["interrupted", "completed"]);
    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Completed));
    let goals = GoalStore::for_task(&h.db, &task_id).await.unwrap();
    assert_eq!(goals.len(), 1, "resume must reuse the existing goal");
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
    assert_eq!(
        goals[0].windows_run, 2,
        "the explicit resume consumes exactly one additional work window"
    );
}

#[tokio::test]
async fn resume_refuses_a_successfully_completed_session() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let err = h
        .engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("a finished session must not be re-driven");
    assert!(err.to_string().contains("already completed"), "{err}");
}

/// The bad-dogfood shape: `sessions.outcome` is `completed`, but the durable
/// terminal stopped with `answered` — the model emitted a phase summary and
/// never declared the goal complete. That is not a proven completion, so the
/// remaining work must stay reachable through resume/reopen.
///
/// The counterpart above (`resume_refuses_a_successfully_completed_session`)
/// protects the other half: a declared `update_goal(complete)` stop stays
/// terminal.
#[tokio::test]
async fn resume_allows_a_session_that_only_answered() {
    let h = harness(vec![
        text("phase 1 done; A3/A4 and D/F still remain"),
        text("continued; still working on the remainder"),
    ])
    .await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    // A conversational turn: it goes quiet with text and no goal terminal.
    h.engine
        .chat(
            &session,
            &spec,
            vec![ContentPart::Text {
                text: "finish the whole task".to_string(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let row = EventRepository::new(&h.db)
        .load_last_by_type(&session, "task_finished", None)
        .await
        .unwrap()
        .expect("a terminal was committed");
    let EngineEvent::TaskFinished { stop, .. } = EngineEvent::from_payload(&row.payload).unwrap()
    else {
        panic!("expected TaskFinished");
    };
    assert_eq!(stop, Some(StopReason::Answered));
    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome,
        Some(TaskOutcome::Completed),
        "the session column still reads completed — which is exactly why the stop decides"
    );

    let report = h
        .engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect("an answered terminal is not a proven completion");
    assert_eq!(report.stop_reason, StopReason::Answered);
}

/// A parallel multi-agent parent session is written by the launcher, not run by
/// the engine. Resume must refuse it by its kind, not incidentally because its
/// transcript is empty (§18.11).
#[tokio::test]
async fn resume_refuses_a_parallel_parent_session() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    SessionRepository::new(&h.db)
        .set_execution(
            &session,
            "assisted",
            false,
            ExecutionKind::Parallel.as_str(),
            leveler_core::now(),
        )
        .await
        .unwrap();

    let err = h
        .engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("a parallel parent session must not be resumable");
    assert!(err.to_string().contains("parallel parent"), "{err}");
}

/// Quiet text without `update_goal` must not read as a successful task finish.
///
/// Headless `engine.run` always uses the Goal turn profile (direct tool loop).
/// Going quiet exhausts closeout / continuation and must land on a non-success
/// outcome — never Verified / CompletedUnverified "as if done".
#[tokio::test]
async fn quiet_without_update_goal_is_not_task_success() {
    // Enough quiet rounds to burn goal nudge + engine continuation budget.
    let mut responses = Vec::new();
    for _ in 0..12 {
        responses.push(text("看起来做完了。"));
    }
    let h = harness(responses).await;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_ne!(
        report.outcome,
        TaskOutcome::Completed,
        "quiet must never read as a completed run: {:?}",
        report.outcome
    );
    assert!(
        matches!(
            report.outcome,
            TaskOutcome::Failed | TaskOutcome::BudgetLimited | TaskOutcome::Interrupted
        ),
        "expected non-success terminal for unresolved goal, got {:?}",
        report.outcome
    );
}

/// Direct must not spend an extra model call inventing acceptance criteria.
///
/// The scripted runtime here supplies exactly the turn's responses and nothing
/// more, so any additional `understand` round would exhaust the queue and fail
/// the run. This is the regression guard for the removed
/// `direct_extract_and_evaluate_acceptance` step.
#[tokio::test]
async fn direct_spends_no_extra_model_call_on_acceptance() {
    let h = harness(patch_then_resolve()).await;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Completed);
}

/// A goal the model lets go quiet past the continuation bound ends where the
/// model stopped. The runtime records the stall; it does not open a second
/// turn on the model's behalf.
#[tokio::test]
async fn a_stalled_goal_ends_after_one_turn() {
    let h = harness(vec![
        text("still working 1"),
        text("still working 2"),
        text("still working 3"),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "reached after an explicit resume"}),
        ),
    ])
    .await;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    // The goal was never resolved, and nothing re-drove it into a second turn.
    assert_eq!(report.stop_reason, StopReason::Stalled);
    assert_eq!(report.outcome, TaskOutcome::Interrupted);
    assert_eq!(GoalStore::unfinished(&h.db).await.unwrap().len(), 1);
    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    assert_eq!(
        turns.len(),
        1,
        "the runtime must not open a second turn on the model's behalf: {turns:?}"
    );
    let resumed = h
        .engine
        .resume(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect("the unresolved goal remains resumable");
    assert_eq!(resumed.outcome, TaskOutcome::Completed);
    assert!(GoalStore::unfinished(&h.db).await.unwrap().is_empty());
}

#[tokio::test]
async fn independent_review_required_launches_on_an_ordinary_change() {
    let mut responses = patch_then_resolve();
    responses.push(text("ordinary change: no blocking defect"));
    responses.push(text("ordinary change: no blocking defect"));
    let mut h = harness(responses).await;
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let mut seen: Vec<EngineEvent> = Vec::new();
    h.engine
        .run(
            &session,
            &s,
            &mut |event| seen.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let reviewers: Vec<_> = seen
        .iter()
        .filter_map(|event| match event {
            EngineEvent::SubAgentStarted { id, role, .. } if role == "reviewer" => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reviewers.len(),
        1,
        "required must launch a reviewer after any product mutation"
    );
}

struct RejectReviewTerminalEvents {
    inner: Database,
}

#[async_trait]
impl leveler_storage::EventStore for RejectReviewTerminalEvents {
    async fn append(
        &self,
        session_id: &leveler_core::SessionId,
        turn_id: Option<&leveler_core::TurnId>,
        event_type: &str,
        payload: &str,
        now: leveler_core::Timestamp,
    ) -> Result<leveler_storage::EventRecord, leveler_storage::StorageError> {
        if event_type == "sub_agent_finished" {
            return Err(leveler_storage::StorageError::InvalidData(
                "review terminal unavailable (injected)".to_string(),
            ));
        }
        leveler_storage::EventStore::append(
            &self.inner,
            session_id,
            turn_id,
            event_type,
            payload,
            now,
        )
        .await
    }

    async fn load(
        &self,
        session_id: &leveler_core::SessionId,
    ) -> Result<Vec<leveler_storage::EventRecord>, leveler_storage::StorageError> {
        leveler_storage::EventStore::load(&self.inner, session_id).await
    }

    async fn load_after(
        &self,
        session_id: &leveler_core::SessionId,
        after: i64,
    ) -> Result<Vec<leveler_storage::EventRecord>, leveler_storage::StorageError> {
        leveler_storage::EventStore::load_after(&self.inner, session_id, after).await
    }

    async fn load_last_by_type(
        &self,
        session_id: &leveler_core::SessionId,
        event_type: &str,
        turn_id: Option<&leveler_core::TurnId>,
    ) -> Result<Option<leveler_storage::EventRecord>, leveler_storage::StorageError> {
        leveler_storage::EventStore::load_last_by_type(&self.inner, session_id, event_type, turn_id)
            .await
    }

    async fn append_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &leveler_core::SessionId,
        turn_id: Option<&leveler_core::TurnId>,
        event_type: &str,
        payload: &str,
        now: leveler_core::Timestamp,
    ) -> Result<leveler_storage::EventRecord, leveler_storage::OwnershipError> {
        if event_type == "sub_agent_finished" {
            return Err(leveler_storage::OwnershipError::Storage(
                leveler_storage::StorageError::InvalidData(
                    "review terminal unavailable (injected)".to_string(),
                ),
            ));
        }
        leveler_storage::EventStore::append_owned(
            &self.inner,
            token,
            session_id,
            turn_id,
            event_type,
            payload,
            now,
        )
        .await
    }
}

#[tokio::test]
async fn an_unsettled_started_review_prevents_the_task_terminal() {
    let mut responses = patch_then_resolve();
    responses.push(text("review completed without findings"));
    responses.push(text("review completed without findings"));
    let mut h = harness(responses).await;
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let spec = spec(&h);
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine.engine.stores.events = Arc::new(RejectReviewTerminalEvents {
        inner: h.db.clone(),
    });

    let error = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("an open review activation must prevent task terminal publication");
    assert!(
        matches!(
            error,
            leveler_engine::EngineError::UnclosedTerminalBoundary(_)
        ),
        "unexpected error: {error}"
    );

    let events = EventRepository::new(&h.db)
        .load(&session)
        .await
        .unwrap()
        .into_iter()
        .map(|row| EngineEvent::from_payload(&row.payload).unwrap())
        .collect::<Vec<_>>();
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::SubAgentStarted { role, .. } if role == "reviewer"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        EngineEvent::SubAgentFinished { nickname, .. } if nickname == "reviewer"
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::TaskFinished { .. }))
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        EngineEvent::ReviewStage { action, .. } if action == "launch_failed"
    )));
}

/// A harness-launched reviewer judges the change; it must not become a second
/// author of it. The role is physically read-only (no write tools in its
/// registry), so an attempt to patch is refused rather than merely discouraged.
#[tokio::test]
async fn harness_reviewer_cannot_modify_the_code_it_reviews() {
    let mut responses = vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Add File: src/auth.rs\n+pub fn login() {}\n*** End Patch"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "added the login entry point"}),
        ),
    ];
    // The reviewer's first move is to "fix" what it is reviewing.
    responses.push(tool_call(
        "r1",
        "apply_patch",
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: src/auth.rs\n-pub fn login() {}\n+pub fn login() { todo!() }\n*** End Patch"
        }),
    ));
    responses.push(text("reviewed src/auth.rs: login() has no rate limiting"));
    responses.push(text("reviewed src/auth.rs: login() has no rate limiting"));

    let mut h = harness(responses).await;
    h.engine.approver = Arc::new(leveler_execution::AutoDeny);
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    h.engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let written = std::fs::read_to_string(h.dir.path().join("src/auth.rs")).unwrap();
    assert_eq!(
        written, "pub fn login() {}\n",
        "the reviewer must not be able to edit the change it is judging"
    );
}

// ── R011-F1: window progress must recognise refinement ──────────────────────
//
// R011 measured: turn 1 touched 9 new files; turns 2–3 touched 0 new files but
// performed 5 real write operations (compile-fix-test refinement). The window
// judge counted only modified-file-set growth, so two refinement windows read
// as no progress and the goal died with `outcome=failed` while work was landing.

fn spec_windowed(h: &Harness, goal: &str, rounds_per_window: u32) -> TaskSpec {
    let mut s = spec(h);
    s.runtime.goal = goal.to_string();
    // UntilTerminal keeps the round budget UNPINNED, so hitting the per-turn
    // ceiling ends a WORK WINDOW (policy opens the next one), not the goal.
    s.runtime.continuation = leveler_agent::ContinuationPolicy::UntilTerminal;
    s.runtime.limits = leveler_agent::StepLimits {
        max_model_steps: Some(rounds_per_window),
        ..leveler_agent::StepLimits::default()
    };
    s
}

fn patch_add(id: &str, path: &str, line: &str) -> ModelResponse {
    tool_call(
        id,
        "apply_patch",
        serde_json::json!({
            "patch": format!("*** Begin Patch\n*** Add File: {path}\n+{line}\n*** End Patch")
        }),
    )
}

fn read_call_named(id: &str, path: &str) -> ModelResponse {
    tool_call(id, "read_file", serde_json::json!({"path": path}))
}

// ── R011-F2 / R013-F1: reviewer reach and observability ─────────────────────

/// Collect persisted review_stage events for one session.
async fn review_stage_rows(
    db: &Database,
    session: &leveler_core::SessionId,
) -> Vec<(bool, String, String)> {
    let store = leveler_storage::EngineStores::from_database(db);
    let mut out = Vec::new();
    for row in store.events.load(session).await.unwrap() {
        if row.event_type == "review_stage"
            && let Ok(leveler_engine::EngineEvent::ReviewStage {
                required,
                action,
                detail,
            }) = leveler_engine::EngineEvent::from_payload(&row.payload)
        {
            out.push((required, action, detail));
        }
    }
    out
}

/// R011's accident: a security-shaped, wide diff whose goal dies at the round
/// ceiling. The review that policy requires must still run before the terminal
/// fact is written — a failed high-risk change needs eyes more, not less.
#[tokio::test]
async fn required_review_runs_even_when_the_goal_fails_at_the_ceiling() {
    let mut h = harness(vec![
        // Window 1 (the only one this spec allows): touch a security path,
        // then keep "working" until the 2-round ceiling.
        patch_add("c1", "src/auth.rs", "pub fn login() {}"),
        read_call_named("c2", "src/auth.rs"),
        // The reviewer child answers once launched.
        text("reviewed the auth change: no blocking defect"),
        text("reviewed the auth change: no blocking defect"),
        text("unused"),
    ])
    .await;
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let mut s = spec_windowed(&h, "harden the login path", 2);
    // Pin the round budget so the ceiling is the GOAL terminal (no next window)
    // — exactly R011's ending, minus the wait.
    s.runtime.continuation = leveler_agent::ContinuationPolicy::bounded(2);
    let session = h.engine.create_task(&s).await.unwrap();
    let mut seen: Vec<EngineEvent> = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |e| seen.push(e),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        !report.outcome.is_completed(),
        "the goal still fails — review must not launder a ceiling stop: {:?}",
        report.outcome
    );
    let reviewers = seen
        .iter()
        .filter(|e| matches!(e, EngineEvent::SubAgentStarted { role, .. } if role == "reviewer"))
        .count();
    assert_eq!(
        reviewers, 1,
        "a required review must run before the failed terminal is sealed"
    );
}

/// R013's accident, made loud: when the review cannot even be launched, the
/// failure must persist as a review_stage event — never a silent downgrade.
struct FailingProfileRuntime {
    inner: MockRuntime,
    profile_calls: std::sync::atomic::AtomicUsize,
    fail_from: usize,
}

#[async_trait]
impl ModelRuntime for FailingProfileRuntime {
    async fn generate(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.inner.generate(request, cancellation).await
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.inner.stream(request, cancellation).await
    }
    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        let n = self
            .profile_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n >= self.fail_from {
            return Err(ModelError::new(
                leveler_model::ModelErrorKind::Other,
                "profile store unavailable (injected)",
            ));
        }
        self.inner.profile(model).await
    }
}

#[tokio::test]
async fn unlaunchable_review_leaves_a_persisted_trace() {
    // Same shape as the passing security review, but the reviewer's executor
    // cannot be built: the SECOND profile fetch (run_review's factory.build)
    // fails while the main turn's succeeds.
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
    let runtime = Arc::new(FailingProfileRuntime {
        inner: MockRuntime::new(vec![
            tool_call(
                "c1",
                "apply_patch",
                serde_json::json!({
                    "patch": "*** Begin Patch\n*** Add File: src/auth.rs\n+pub fn login() {}\n*** End Patch"
                }),
            ),
            tool_call(
                "g1",
                "update_goal",
                serde_json::json!({"status": "complete", "summary": "login added"}),
            ),
        ]),
        profile_calls: std::sync::atomic::AtomicUsize::new(0),
        fail_from: 1,
    });
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
            resource_grants: std::sync::Arc::new(
                leveler_storage::MemoryResourceGrantStore::default(),
            ),
            runtime,
            registry: Arc::new(default_registry()),
            tool_context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
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
            allow_host_input: true,
            capabilities: None,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Required,
            develop_model: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        task_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let s = TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "add a login entry point".to_string(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: leveler_agent::StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(dir.path().to_path_buf()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    };
    let session = engine.create_task(&s).await.unwrap();
    let report = engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        report.outcome,
        TaskOutcome::Completed,
        "an unlaunchable required review still refuses Verified"
    );
    let stages = review_stage_rows(&db, &session).await;
    assert!(
        stages.iter().any(|(req, action, detail)| *req
            && action == "launch_failed"
            && detail.contains("profile store unavailable")),
        "the launch failure must be persisted with its cause — silence was the \
         R013 defect; got {stages:?}"
    );
}

/// The cheap half of observability: even a change that needs no review leaves
/// an eligibility record, so \"no reviewer\" is always explainable.
#[tokio::test]
async fn not_required_review_is_still_recorded() {
    let h = harness(patch_then_resolve()).await;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let _ = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    let stages = review_stage_rows(&h.db, &session).await;
    assert!(
        stages
            .iter()
            .any(|(req, action, _)| !req && action == "not_required"),
        "eligibility must be evaluated and persisted even when review is not \
         required; got {stages:?}"
    );
}

/// R013r's production finding: an unbounded reviewer burned the FULL 100-round
/// turn ceiling, and when the ceiling stopped it the synthetic stop sentence
/// replaced the findings it had already voiced. A reviewer reading a diff must
/// be cheaply bounded, and a ceilinged review must keep what it established.
#[tokio::test]
async fn ceilinged_reviewer_is_bounded_and_keeps_partial_findings() {
    let mut responses = vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Add File: src/auth.rs\n+pub fn login() {}\n*** End Patch"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "login added"}),
        ),
    ];
    // The reviewer voices a finding in its first round, then wanders: reads
    // forever without concluding. Without a bound it would consume every
    // response below; with one it stops early and the finding survives.
    responses.push(ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![
                ContentPart::Text {
                    text: "FINDING: login() accepts empty credentials".to_string(),
                },
                ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new("r1"),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "src/auth.rs"}),
                    },
                },
            ],
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    });
    for i in 0..60 {
        responses.push(tool_call(
            &format!("r{}", i + 2),
            "read_file",
            serde_json::json!({"path": "src/auth.rs"}),
        ));
    }
    let mut h = harness(responses).await;
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let mut seen: Vec<EngineEvent> = Vec::new();
    let _ = h
        .engine
        .run(
            &session,
            &s,
            &mut |e| seen.push(e),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let total_requests = h.requests.lock().unwrap().len();
    assert!(
        total_requests <= 30,
        "a reviewer reading a diff must be bounded, not free to burn the full \
         turn ceiling: {total_requests} model calls"
    );
    let summary = seen
        .iter()
        .find_map(|e| match e {
            EngineEvent::SubAgentFinished { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .expect("the reviewer must reach a terminal event");
    assert!(
        summary.contains("FINDING: login() accepts empty credentials"),
        "a ceilinged review must keep the findings it voiced, not just the \
         stop sentence: {summary}"
    );
}

// ── Multi-agent product closure: reviewer findings + blocking closure truth ──

/// The last persisted EvidenceLedger snapshot for one session.
async fn persisted_ledger(
    db: &Database,
    session: &leveler_core::SessionId,
) -> Option<leveler_lifecycle::EvidenceLedger> {
    let store = leveler_storage::EngineStores::from_database(db);
    let mut out = None;
    for row in store.events.load(session).await.unwrap() {
        if row.event_type == "evidence_ledger_updated"
            && let Ok(leveler_engine::EngineEvent::EvidenceLedgerUpdated { ledger }) =
                leveler_engine::EngineEvent::from_payload(&row.payload)
        {
            out = Some(ledger);
        }
    }
    out
}

/// A reviewer's correctness finding is adopted durably and attributed — and
/// the model's declared completion stands beside it. The finding used to
/// carry a `blocking` flag that refused the closure until the parent walked it
/// through a resolution lifecycle; a reviewer's sentence of English was never
/// a mechanical fact the runtime could hold a task on.
#[tokio::test]
async fn a_reviewer_finding_is_adopted_without_gating_the_closure() {
    let responses = vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Add File: src/auth.rs\n+pub fn login() {}\n*** End Patch"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "added the login entry point"}),
        ),
        // Reviewer child rounds: one typed blocking finding, then prose.
        tool_call(
            "rf1",
            "report_finding",
            serde_json::json!({
                "kind": "correctness",
                "summary": "login() accepts any password",
                "file": "src/auth.rs",
                "blocking": true
            }),
        ),
        text("reviewed src/auth.rs: one blocking defect reported"),
        text("reviewed src/auth.rs: one blocking defect reported"),
    ];

    let mut h = harness(responses).await;
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        report.outcome,
        TaskOutcome::Completed,
        "the model's declared end stands; the finding is recorded beside it"
    );

    let stages = review_stage_rows(&h.db, &session).await;
    assert!(
        !stages
            .iter()
            .any(|(_, action, _)| action == "blocking_finding_open"),
        "no stage may refuse a closure over a finding: {stages:?}"
    );

    let ledger = persisted_ledger(&h.db, &session)
        .await
        .expect("adoption must persist a ledger snapshot");
    assert_eq!(ledger.findings.len(), 1);
    let f = &ledger.findings[0];
    assert_eq!(f.role, "reviewer");
    assert!(f.source_child.starts_with("reviewer-"));
    assert_eq!(f.summary, "login() accepts any password");
}

/// An observation from a reviewer is knowledge: adopted durably, and the
/// closure stands.
#[tokio::test]
async fn a_reviewer_observation_does_not_refuse_the_closure() {
    let responses = vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Add File: src/auth.rs\n+pub fn login() {}\n*** End Patch"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "added the login entry point"}),
        ),
        tool_call(
            "rf1",
            "report_finding",
            serde_json::json!({
                "kind": "observation",
                "summary": "consider rate limiting later",
                "file": "src/auth.rs"
            }),
        ),
        text("reviewed src/auth.rs: nothing blocking"),
        text("reviewed src/auth.rs: nothing blocking"),
    ];

    let mut h = harness(responses).await;
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Completed);
    let ledger = persisted_ledger(&h.db, &session)
        .await
        .expect("the finding must still be adopted durably");
    assert_eq!(ledger.findings.len(), 1);
    assert_eq!(ledger.findings[0].summary, "consider rate limiting later");
}

/// EventLog replay: reloading the last EvidenceLedgerUpdated after a
/// reviewer adoption returns the same single finding. Resume of a completed
/// session is refused by the engine (start a new task); the durable contract
/// is the snapshot, not a second drive.
#[tokio::test]
async fn persisted_findings_reload_without_duplication() {
    let responses = vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Add File: src/auth.rs\n+pub fn login() {}\n*** End Patch"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "added login"}),
        ),
        tool_call(
            "rf1",
            "report_finding",
            serde_json::json!({
                "kind": "correctness",
                "summary": "login() accepts any password",
                "file": "src/auth.rs",
                "blocking": true
            }),
        ),
        text("reviewed: one blocking defect"),
        text("reviewed: one blocking defect"),
    ];
    let mut h = harness(responses).await;
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    h.engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let first = persisted_ledger(&h.db, &session).await.unwrap();
    let second = persisted_ledger(&h.db, &session).await.unwrap();
    assert_eq!(first.findings.len(), 1);
    assert_eq!(first, second, "reload must be identical, not duplicated");
}

// ── Phase 1: contribution trace closure on the independent-review path ──

/// The last `SubAgentFinished` contribution projection for one session.
fn terminal_contribution(
    seen: &[EngineEvent],
) -> Option<Option<leveler_lifecycle::ChildResultProjection>> {
    seen.iter().rev().find_map(|e| match e {
        EngineEvent::SubAgentFinished { contribution, .. } => Some(contribution.clone()),
        _ => None,
    })
}

/// MA-VALUE-REVIEWER-PILOT found the treatment arm unscorable: the reviewer
/// adopted findings into the parent ledger, then the terminal event reported
/// `contribution: null`. Nothing could join "a reviewer ran" to "and this is
/// what the parent did with what it found".
#[tokio::test]
async fn a_reviewer_finding_reaches_the_terminal_contribution_trace() {
    let responses = vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Add File: src/auth.rs\n+pub fn login() {}\n*** End Patch"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "added the login entry point"}),
        ),
        tool_call(
            "rf1",
            "report_finding",
            serde_json::json!({
                "kind": "correctness",
                "summary": "login() accepts any password",
                "file": "src/auth.rs",
                "blocking": true
            }),
        ),
        text("reviewed src/auth.rs: one blocking defect reported"),
        text("reviewed src/auth.rs: one blocking defect reported"),
    ];

    let mut h = harness(responses).await;
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let mut seen: Vec<EngineEvent> = Vec::new();
    let _ = h
        .engine
        .run(
            &session,
            &s,
            &mut |e| seen.push(e),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let contribution = terminal_contribution(&seen)
        .expect("the reviewer must reach a terminal event")
        .expect("a reviewer that adopted findings must not report `not measured`");
    assert_eq!(
        contribution.findings_total, 1,
        "the adopted finding must be counted: {contribution:?}"
    );
    assert_eq!(contribution.role, "reviewer");
    assert_eq!(
        contribution.profile_id.as_deref(),
        Some("reviewer"),
        "the trace must name the capability contract that produced it"
    );
    let typed = seen.iter().rev().find_map(|e| match e {
        EngineEvent::SubAgentFinished { outcome, stop, .. } => Some((*outcome, *stop)),
        _ => None,
    });
    assert_eq!(
        typed,
        Some((
            Some(leveler_lifecycle::ChildStatus::CompletedWithFindings),
            Some(leveler_lifecycle::ChildStop::Completed)
        )),
        "the reviewer terminal is typed like every other child's"
    );
}

/// A reviewer that reports no structured finding contributed a measured zero.
/// That is not the same fact as "no projection exists", and the difference is
/// exactly what made the pilot report claim five zero-finding reviewers that
/// had in fact all reported.
#[tokio::test]
async fn a_reviewer_without_findings_reports_a_measured_zero_not_null() {
    let mut responses = vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Add File: src/auth.rs\n+pub fn login() {}\n*** End Patch"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "added the login entry point"}),
        ),
    ];
    responses.push(text("reviewed src/auth.rs: no blocking defect found"));
    responses.push(text("reviewed src/auth.rs: no blocking defect found"));

    let mut h = harness(responses).await;
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let mut seen: Vec<EngineEvent> = Vec::new();
    let _ = h
        .engine
        .run(
            &session,
            &s,
            &mut |e| seen.push(e),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let contribution = terminal_contribution(&seen)
        .expect("the reviewer must reach a terminal event")
        .expect("a reviewer that ran must report a projection, even an empty one");
    assert_eq!(contribution.findings_total, 0);
    assert_eq!(contribution.role, "reviewer");
    assert!(
        contribution.profile_id.is_some(),
        "the capability contract must travel with the trace"
    );
}

#[tokio::test]
async fn completion_does_not_run_a_host_owned_verification_gate() {
    let h = harness(patch_resolve_and_proven_ac()).await;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    h.engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let rows = EventRepository::new(&h.db).load(&session).await.unwrap();
    let verification_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.event_type.starts_with("verification_"))
        .collect();
    assert!(
        verification_rows.is_empty(),
        "completion must not run or persist a host-owned verification gate: {verification_rows:?}"
    );
}

/// F7-C TEST B. A failing gate is equally an observation, and its record has
/// to say it failed rather than being absent — an absent record reads as "no
/// check ran", which is a different and softer fact.
#[cfg(any())]
#[tokio::test]
async fn a_failed_engine_gate_is_recorded_as_a_failed_observation() {
    // A red gate buys one repair turn (DIRECT_REPAIR_ATTEMPTS), so the script
    // has to carry a second pass through the loop.
    let mut responses = patch_resolve_and_proven_ac();
    responses.extend(patch_resolve_and_proven_ac());
    let h = harness(responses).await;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    // Case 3: the model declared completion, the project's checks failed.
    // Both facts are reported; no repair turn is opened on the model's behalf.
    assert_eq!(report.outcome, TaskOutcome::Completed);
    assert_eq!(
        report.verification_status,
        leveler_lifecycle::VerificationStatus::Failed
    );
    assert_eq!(
        TurnRepository::new(&h.db)
            .list(&session)
            .await
            .unwrap()
            .len(),
        1,
        "a failed check must not open an automatic repair turn"
    );
    let rows = EventRepository::new(&h.db).load(&session).await.unwrap();
    let check = rows
        .iter()
        .find(|row| row.event_type == "verification_check")
        .expect("the failed check must be durable");
    let payload: serde_json::Value = serde_json::from_str(&check.payload).unwrap();
    assert_eq!(payload["payload"]["observation"]["kind"], "failed");
    assert_ne!(payload["payload"]["execution"]["exit_code"], 0);
}

/// F7-C TEST I. A completion attempt may be made more than once. The ledger
/// must not grow a fresh copy of the same observation each time.
#[cfg(any())]
#[tokio::test]
async fn one_verification_attempt_has_one_canonical_check_fact() {
    let h = harness(patch_resolve_and_proven_ac()).await;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    h.engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let rows = EventRepository::new(&h.db).load(&session).await.unwrap();
    let engine_records: Vec<_> = rows
        .iter()
        .filter(|row| row.event_type == "verification_check")
        .collect();
    assert_eq!(
        engine_records.len(),
        1,
        "one observation per check per attempt: {engine_records:?}"
    );
}

/// The harness-launched closure reviewer is a model consumer like any child:
/// every call it makes is a `model_requests` row under its own agent id, and
/// its rounds land in the session's persisted progress. Before this, six
/// reviews across the C2 batches ran for minutes each and left no row and no
/// round behind — invisible to the bill and to the task budget.
#[tokio::test]
async fn a_harness_launched_review_is_accounted_and_folded_into_the_session() {
    let mut responses = vec![
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Add File: src/auth.rs\n+pub fn login() {}\n*** End Patch"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "added the login entry point"}),
        ),
    ];
    responses.push(text("reviewed src/auth.rs: no blocking defect found"));
    responses.push(text("reviewed src/auth.rs: no blocking defect found"));
    let mut h = harness(responses).await;
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let mut seen: Vec<EngineEvent> = Vec::new();
    h.engine
        .run(
            &session,
            &s,
            &mut |event| seen.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let reviewer = seen
        .iter()
        .find_map(|event| match event {
            EngineEvent::SubAgentStarted { id, role, .. } if role == "reviewer" => Some(id.clone()),
            _ => None,
        })
        .expect("a security-shaped change launches a reviewer");

    // Accounting: the reviewer's calls are rows under its id.
    let rows = leveler_storage::ModelRequestRepository::new(&h.db)
        .load_for_session(&session)
        .await
        .unwrap();
    let reviewer_rows = rows
        .iter()
        .filter(|r| r.agent_id.as_deref() == Some(reviewer.as_str()))
        .count();
    let root_rows = rows.iter().filter(|r| r.agent_id.is_none()).count();
    assert!(
        reviewer_rows >= 1,
        "the reviewer made model calls and none is on the books: {rows:?}"
    );
    assert!(root_rows >= 1, "the root's own rows are still its own");

    // Budget: the session's persisted progress absorbed the reviewer's rounds.
    let started_at = seen
        .iter()
        .position(|e| matches!(e, EngineEvent::SubAgentStarted { .. }))
        .unwrap();
    let before = seen[..started_at]
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ProgressUpdated { ledger } => Some(ledger.cumulative_model_steps),
            _ => None,
        })
        .next_back()
        .unwrap_or(0);
    let after = seen[started_at..]
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ProgressUpdated { ledger } => Some(ledger.cumulative_model_steps),
            _ => None,
        })
        .next_back()
        .unwrap_or(before);
    assert!(
        after >= before + reviewer_rows as u32,
        "the reviewer's {reviewer_rows} round(s) must be in the session's cumulative rounds: before={before} after={after}"
    );
}

/// Stamp provider-reported usage onto a scripted response.
fn with_usage(mut response: ModelResponse, input: u64, cached: u64, output: u64) -> ModelResponse {
    response.usage = TokenUsage {
        input_tokens: input,
        cache_creation_input_tokens: 0,
        cached_input_tokens: cached,
        reasoning_tokens: None,
        output_tokens: output,
    };
    response
}

/// RCP-A. The number a budget guard admits on and the number the bill
/// reconciles to must be the same number.
///
/// `model_requests` is the durable factual authority for what a session spent.
/// The runtime used to compute its own spend beside it — from the same
/// provider usage, but only for the main loop's own rounds — so a session's
/// advisory calls, its folds and its reviewer were on the books and invisible
/// to admission, while a child's tokens arrived twice by two paths that did
/// not agree. Two authorities that disagree is not an accounting nicety: it is
/// a cost cap that binds at the wrong time, in whichever direction the drift
/// happens to run.
///
/// This pins the projection to the ledger field by field over a run that
/// spends in every lane it has: root rounds with reported usage, a
/// contract-derivation advisory the mock answers with none, and a
/// harness-launched reviewer.
#[tokio::test]
async fn runtime_spend_admission_reconciles_with_the_durable_ledger() {
    let mut responses = vec![
        // Deliberately unstamped: a gateway that reports no usage at all.
        tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Add File: src/auth.rs\n+pub fn login() {}\n*** End Patch"
            }),
        ),
        with_usage(
            tool_call(
                "g1",
                "update_goal",
                serde_json::json!({"status": "complete", "summary": "added the login entry point"}),
            ),
            5_000,
            4_500,
            80,
        ),
    ];
    responses.push(with_usage(
        text("reviewed src/auth.rs: no blocking defect found"),
        900,
        0,
        40,
    ));
    responses.push(with_usage(
        text("reviewed src/auth.rs: no blocking defect found"),
        950,
        0,
        30,
    ));
    let mut h = harness(responses).await;
    // The reviewer is the child lane this reconciliation is about, and
    // `IndependentReviewPolicy` defaults to `Off`: a run that spends in one
    // lane only cannot prove the two authorities agree, so ask for it.
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let mut seen: Vec<EngineEvent> = Vec::new();
    h.engine
        .run(
            &session,
            &s,
            &mut |event| seen.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let durable = leveler_storage::ModelRequestRepository::new(&h.db)
        .reconcile_session(&session)
        .await
        .unwrap();
    let ledger = seen
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ProgressUpdated { ledger } => Some(ledger.clone()),
            _ => None,
        })
        .next_back()
        .expect("a run that spent anything publishes its ledger");

    assert!(
        !durable.by_agent.is_empty(),
        "the run must spend in a child lane too, or this proves nothing: {durable:?}"
    );
    assert!(
        durable.total.requests > durable.root.requests,
        "root and child rows must both be present: {durable:?}"
    );
    assert_eq!(
        ledger.cumulative_model_tokens - ledger.cumulative_estimated_model_tokens,
        durable.total.input_tokens + durable.total.output_tokens,
        "the audited share of runtime token admission must BE the durable \
         ledger's own total (durable={:?}, runtime={}, estimated={})",
        durable.total,
        ledger.cumulative_model_tokens,
        ledger.cumulative_estimated_model_tokens,
    );
    assert_eq!(
        ledger.cumulative_cost_usd_micros, durable.total.cost_usd_micros,
        "runtime cost admission must be the durable ledger's own total \
         (durable={:?}, runtime={})",
        durable.total, ledger.cumulative_cost_usd_micros,
    );
    assert_eq!(
        durable.total.rows_without_cost, 1,
        "the missing usage remains unpriced even when the model has pricing: {:?}",
        durable.total
    );
    assert!(ledger.has_unpriced_model_attempt);
    assert!(
        ledger.cumulative_estimated_model_tokens > 0,
        "the first round reports no usage at all; the estimate standing in for \
         it is what keeps a token budget binding, and it must stay visible as \
         an estimate rather than blend into the audited total"
    );
}

// ── Model-step semantics: budget correctness, not a task budget ─────────────

/// The last `ProgressUpdated` ledger the session persisted.
async fn last_ledger(
    db: &Database,
    session: &leveler_core::SessionId,
) -> leveler_lifecycle::ProgressLedger {
    let store = leveler_storage::EngineStores::from_database(db);
    let mut latest = None;
    for row in store.events.load(session).await.unwrap() {
        if let Ok(leveler_engine::EngineEvent::ProgressUpdated { ledger }) =
            leveler_engine::EngineEvent::from_payload(&row.payload)
        {
            latest = Some(ledger);
        }
    }
    latest.expect("a run persists its progress ledger")
}

/// §F: a goal task that needs MORE model steps than the small task-level counts
/// the old wiring used (10/15) must still complete normally. The product's
/// shape is `UntilTerminal` + a generous mechanical safety ceiling, so a model
/// step count can never stand in for "the task is done".
#[tokio::test]
async fn a_goal_task_longer_than_a_small_step_count_still_completes() {
    const READS: usize = 11;
    let mut responses: Vec<ModelResponse> = (0..READS)
        .map(|i| {
            tool_call(
                &format!("r{i}"),
                "read_file",
                serde_json::json!({"path": format!("src/f{i}.rs")}),
            )
        })
        .collect();
    responses.push(patch_add("p1", "src/added.rs", "pub fn added() {}"));
    responses.push(tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "inspected and added a function"}),
    ));

    // Distinct files so every read is novel progress: the run is long because
    // the WORK is long, which is exactly the case a step count must not cut.
    let h = harness_with(responses, PermissionProfile::Assisted, |dir| {
        for i in 0..READS {
            std::fs::write(
                dir.join("src").join(format!("f{i}.rs")),
                format!("pub fn f{i}() {{}}\n"),
            )
            .unwrap();
        }
    })
    .await;

    let mut s = spec(&h);
    s.runtime.continuation = leveler_agent::ContinuationPolicy::UntilTerminal;
    s.runtime.limits = leveler_agent::StepLimits {
        max_model_steps: Some(leveler_agent::DEFAULT_MODEL_STEP_CEILING),
        ..leveler_agent::StepLimits::default()
    };
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        report.outcome,
        TaskOutcome::Completed,
        "a >10 step task must complete, not be truncated: {report:?}"
    );
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert!(
        report.model_steps as usize > 10,
        "this task deliberately needs more than 10 model steps, got {}",
        report.model_steps
    );
    assert!(
        report.model_steps < leveler_agent::DEFAULT_MODEL_STEP_CEILING,
        "the safety ceiling must not be what ended this run: {}",
        report.model_steps
    );
}

/// §G + §E end-to-end: the safety ceiling ends a drive honestly (resumable,
/// `TurnLimitReached`, not a resource budget), and a resume continues the
/// task's spend while getting a fresh per-drive step counter.
#[tokio::test]
async fn the_step_ceiling_is_per_drive_and_resume_continues_task_spend() {
    // Drive 1: the ceiling stops it after two model steps.
    let h = harness(vec![
        read_call_named("c1", "src/lib.rs"),
        read_call_named("c2", "src/lib.rs"),
        read_call_named("c3", "src/lib.rs"),
    ])
    .await;
    let mut s = spec(&h);
    s.runtime.continuation = leveler_agent::ContinuationPolicy::UntilTerminal;
    s.runtime.limits = leveler_agent::StepLimits {
        max_model_steps: Some(2),
        ..leveler_agent::StepLimits::default()
    };
    let session = h.engine.create_task(&s).await.unwrap();
    let first = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(first.stop_reason, StopReason::TurnLimitReached);
    assert_eq!(
        first.model_steps, 2,
        "the pinned ceiling is exactly what fired"
    );
    let detail = first.stop_detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("model_step_ceiling=2"),
        "the detail must name the ceiling that fired: {detail:?}"
    );
    let after_first = last_ledger(&h.db, &session).await;

    // Drive 2: resume on the same database with a generous ceiling.
    let dir2 = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir2.path().join("src")).unwrap();
    std::fs::write(dir2.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(dir2.path()).unwrap();
    let engine2 = CodingRuntime {
        engine: TaskEngine {
            stores: leveler_storage::EngineStores::from_database(&h.db),
            runtime_id: leveler_core::RuntimeId::new("rt-test"),
            boot: leveler_engine::EngineBoot {
                id: leveler_core::BootId::generate(),
                liveness: std::sync::Arc::new(leveler_test_support::TestBoots::new()),
            },
        },
        factory: ExecutorFactory {
            resource_grants: std::sync::Arc::new(
                leveler_storage::MemoryResourceGrantStore::default(),
            ),
            runtime: Arc::new(MockRuntime::new(vec![
                patch_add("p1", "src/added.rs", "pub fn added() {}"),
                tool_call(
                    "g1",
                    "update_goal",
                    serde_json::json!({"status": "complete", "summary": "done"}),
                ),
            ])),
            registry: Arc::new(default_registry()),
            tool_context: ToolContext::with_environment(
                workspace,
                PermissionProfile::Assisted,
                Arc::new(leveler_core::EnvSnapshot::new(
                    std::env::vars_os(),
                    std::env::current_dir().unwrap_or_default(),
                    std::env::temp_dir(),
                )),
            ),
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
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
            allow_host_input: true,
            capabilities: None,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Off,
            develop_model: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        task_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let mut s2 = spec(&h);
    s2.coding.repository = Some(dir2.path().to_path_buf());
    s2.runtime.continuation = leveler_agent::ContinuationPolicy::UntilTerminal;
    s2.runtime.limits = leveler_agent::StepLimits {
        max_model_steps: Some(leveler_agent::DEFAULT_MODEL_STEP_CEILING),
        ..leveler_agent::StepLimits::default()
    };

    let second = engine2
        .resume(&session, &s2, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(second.outcome, TaskOutcome::Completed);

    // §G: the second drive's count is its OWN (a fresh per-drive ceiling), and
    // the task epoch carries the sum. Neither number is a "task budget left".
    let after_second = last_ledger(&h.db, &session).await;
    assert_eq!(after_second.cumulative_model_steps, 2 + second.model_steps);
    assert!(
        after_second.cumulative_model_tokens > after_first.cumulative_model_tokens,
        "task-level token spend must keep accumulating across a resume: {} -> {}",
        after_first.cumulative_model_tokens,
        after_second.cumulative_model_tokens
    );
}

#[tokio::test]
async fn harness_reviewer_uses_parent_residual_token_budget() {
    let responses = vec![
        with_usage(
            tool_call(
                "edit",
                "apply_patch",
                serde_json::json!({
                    "patch": "*** Begin Patch\n*** Add File: src/auth.rs\n+pub fn login() {}\n*** End Patch"
                }),
            ),
            49_980,
            0,
            20,
        ),
        with_usage(
            tool_call(
                "done",
                "update_goal",
                serde_json::json!({
                    "status": "complete", "summary": "added login"
                }),
            ),
            99_980,
            0,
            20,
        ),
        with_usage(
            tool_call(
                "inspect",
                "read_file",
                serde_json::json!({"path": "src/auth.rs"}),
            ),
            59_980,
            0,
            20,
        ),
        with_usage(
            text("This response must not be requested after the shared budget is spent."),
            30,
            0,
            20,
        ),
    ];
    let mut h = harness(responses).await;
    h.engine.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let mut s = spec(&h);
    s.runtime.limits.max_model_tokens = Some(200_000);
    let session = h.engine.create_task(&s).await.unwrap();
    let mut seen = Vec::new();
    h.engine
        .run(
            &session,
            &s,
            &mut |event| seen.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        h.requests.lock().unwrap().len(),
        3,
        "reviewer must stop after consuming the remaining 50,000 tokens, rather than receive a fresh 200,000-token cap"
    );
    let rows = leveler_storage::ModelRequestRepository::new(&h.db)
        .load_for_session(&session)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        3,
        "every actual request must be durable exactly once"
    );
    assert_eq!(rows.iter().filter(|r| r.agent_id.is_some()).count(), 1);
    let ledger = seen
        .iter()
        .rev()
        .find_map(|event| match event {
            EngineEvent::ProgressUpdated { ledger } => Some(ledger),
            _ => None,
        })
        .unwrap();
    assert_eq!(ledger.cumulative_model_tokens, 210_000);
}

#[tokio::test]
async fn a_multimodal_goal_requires_resolution_and_keeps_its_original_wal_content() {
    let h = harness(vec![
        text("I looked at the image."),
        text("The image has been inspected."),
        tool_call(
            "finish",
            "update_goal",
            serde_json::json!({"status":"complete","summary":"implemented the pictured change"}),
        ),
    ])
    .await;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let content = vec![
        ContentPart::Text {
            text: s.runtime.goal.clone(),
        },
        ContentPart::Image {
            source: leveler_model::ImageSource::Url {
                url: "https://example.invalid/task.png".into(),
            },
        },
    ];
    let report = h
        .engine
        .run_with_content(
            &session,
            &s,
            content.clone(),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        report.stop_reason,
        StopReason::Completed,
        "an image cannot turn a Goal into an ordinary answered chat: the quiet rounds \
         continue until the model actually resolves the goal"
    );
    assert_eq!(report.outcome, TaskOutcome::Completed);
    assert!(
        GoalStore::unfinished(&h.db).await.unwrap().is_empty(),
        "the goal is settled by the model's own terminal, not by an answer"
    );
    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    assert_eq!(
        turns.len(),
        1,
        "still one turn: continuation buys rounds, not turns"
    );
    assert_eq!(turns[0].kind, "user");
    let continuation =
        leveler_engine::decode_turn_continuation(turns[0].payload.as_deref().unwrap()).unwrap();
    assert!(
        continuation.goal_id.is_some(),
        "the Goal identity is durable from the first turn"
    );
    assert_eq!(continuation.initiating_message.unwrap().content, content);
    {
        let requests = h.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            3,
            "two continued quiet rounds, then the resolving call"
        );
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|tool| tool.name == "update_goal")
        );
        assert!(
            requests[0]
                .messages
                .iter()
                .any(|message| message.role == Role::User && message.content == content)
        );
    }
}

struct FailingCheckpointWrites {
    db: Database,
    fail: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl leveler_storage::GoalCheckpointStore for FailingCheckpointWrites {
    async fn create(
        &self,
        new: leveler_storage::NewGoalCheckpoint,
        now: leveler_core::Timestamp,
    ) -> Result<leveler_storage::GoalCheckpointRecord, leveler_storage::StorageError> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(leveler_storage::StorageError::InvalidData(
                "checkpoint disk unavailable".into(),
            ));
        }
        leveler_storage::GoalCheckpointStore::create(&self.db, new, now).await
    }
    async fn get(
        &self,
        id: &leveler_core::GoalCheckpointId,
    ) -> Result<Option<leveler_storage::GoalCheckpointRecord>, leveler_storage::StorageError> {
        leveler_storage::GoalCheckpointStore::get(&self.db, id).await
    }
    async fn latest_for_goal(
        &self,
        id: &leveler_core::GoalId,
    ) -> Result<Option<leveler_storage::GoalCheckpointRecord>, leveler_storage::StorageError> {
        leveler_storage::GoalCheckpointStore::latest_for_goal(&self.db, id).await
    }
    async fn for_goal(
        &self,
        id: &leveler_core::GoalId,
    ) -> Result<Vec<leveler_storage::GoalCheckpointRecord>, leveler_storage::StorageError> {
        leveler_storage::GoalCheckpointStore::for_goal(&self.db, id).await
    }
}

#[tokio::test]
async fn a_failed_continuation_checkpoint_preserves_owed_goal_and_live_services_for_resume() {
    let mut h = harness(vec![
        text("Still inspecting."),
        text("Work remains."),
        text("More work remains."),
        tool_call(
            "finish",
            "update_goal",
            serde_json::json!({"status":"complete","summary":"finished after storage recovered"}),
        ),
    ])
    .await;
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    let checkpoints = Arc::new(FailingCheckpointWrites {
        db: h.db.clone(),
        fail: std::sync::atomic::AtomicBool::new(true),
    });
    h.engine.engine.stores.goal_checkpoints = checkpoints.clone();
    h.engine.factory.tool_context = h
        .engine
        .factory
        .tool_context
        .clone()
        .with_session_scope(session.as_str());
    let tasks = h.engine.factory.background_tasks.clone();
    let bg = tasks
        .spawn_owned(
            leveler_execution::ProcessRequest::new(
                "sleep",
                vec!["30".into()],
                h.dir.path().to_path_buf(),
            ),
            None,
            Some(session.as_str()),
        )
        .await
        .unwrap();
    let mut seen = Vec::new();
    let result = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| seen.push(event),
            CancellationToken::new(),
        )
        .await;
    let retained = tasks
        .active_snapshots_for_scope(session.as_str())
        .await
        .iter()
        .any(|task| task.id == bg);
    // Always reap the test process before assertions, including the red run.
    tasks.kill(&bg).await.unwrap();
    tasks
        .wait(
            &bg,
            Some(std::time::Duration::from_secs(5)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        result.is_err(),
        "checkpoint failure remains visible to the caller"
    );
    let row = SessionRepository::new(&h.db)
        .get(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        SessionRepository::new(&h.db)
            .execution(&session)
            .await
            .unwrap()
            .unwrap()
            .3,
        Some(TaskOutcome::Interrupted)
    );
    assert_eq!(row.status, leveler_lifecycle::SessionStatus::Interrupted);
    assert_eq!(row.state, leveler_lifecycle::AgentState::Execute);
    assert!(
        retained,
        "a failed derived checkpoint must not reap the owed goal's service"
    );
    assert_eq!(GoalStore::unfinished(&h.db).await.unwrap().len(), 1);
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, EngineEvent::GoalCheckpointCreated { .. }))
    );
    assert!(seen.iter().any(|event| matches!(event, EngineEvent::TaskFinished { outcome: TaskOutcome::Interrupted, reason: Some(reason), .. } if reason.contains("checkpoint disk unavailable"))));
    checkpoints
        .fail
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let resumed = h
        .engine
        .resume(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resumed.stop_reason, StopReason::Completed);
    assert!(GoalStore::unfinished(&h.db).await.unwrap().is_empty());
}

#[tokio::test]
async fn progressive_surface_loads_next_request_and_persists_exact_goal() {
    use leveler_agent::capability::{CapabilityDisclosure, CapabilityId};
    let mut h = harness(vec![
        tool_call("hidden", "find_symbol", serde_json::json!({"symbol":"old"})),
        tool_call(
            "enable",
            "capability",
            serde_json::json!({"action":"enable","id":"code_intelligence"}),
        ),
        tool_call("symbol", "find_symbol", serde_json::json!({"symbol":"old"})),
        tool_call(
            "memory",
            "capability",
            serde_json::json!({"action":"enable","id":"memory"}),
        ),
        tool_call(
            "done",
            "update_goal",
            serde_json::json!({"status":"complete","summary":"inspected old"}),
        ),
    ])
    .await;
    h.engine.factory.capabilities = Some(Arc::new(
        CapabilityDisclosure::new(
            CapabilityId::ALL.to_vec(),
            CapabilityId::ALL.to_vec(),
            vec![],
        )
        .unwrap(),
    ));
    h.engine.factory.memory_catalog = "test memory title".into();
    let mut s = spec(&h);
    s.runtime.goal = "inspect old".into();
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::Completed);
    // Clone out of the lock so no `MutexGuard` is held across the awaits below.
    let requests = h.requests.lock().unwrap().clone();
    let names = |index: usize| {
        requests[index]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
    };
    assert!(names(0).contains(&"capability"));
    assert!(!names(0).contains(&"find_symbol"));
    assert!(!names(1).contains(&"find_symbol"));
    assert!(names(2).contains(&"find_symbol"));
    assert!(names(3).contains(&"find_symbol"));
    assert!(requests[3].messages.iter().flat_map(|message| &message.content).any(|part| matches!(part, ContentPart::ToolResult { result } if result.call_id.as_str() == "symbol" && !result.is_error && result.content.contains("src/lib.rs"))));
    assert!(!names(0).contains(&"spawn_agent"));
    assert!(!names(0).contains(&"save_agent"));
    assert!(requests[1].messages.iter().flat_map(|message| &message.content).any(|part| matches!(part, ContentPart::ToolResult { result } if result.call_id.as_str() == "hidden" && result.is_error && result.content.contains("not exposed"))));
    let segments = |index: usize| {
        requests[index]
            .control_context
            .blocks
            .iter()
            .map(|block| block.name.as_str())
            .collect::<Vec<_>>()
    };
    assert!(!segments(0).contains(&"memory_guidance"));
    assert!(!segments(0).contains(&"skills_guidance"));
    assert!(!segments(0).contains(&"agent_catalog"));
    assert!(!segments(0).contains(&"multi_agent_guidance"));
    assert!(segments(4).contains(&"memory_guidance"));
    assert!(segments(4).contains(&"memory_catalog"));
    let task = h
        .engine
        .engine
        .task_for_session(&session)
        .await
        .unwrap()
        .unwrap();
    let goal = h.db.for_task(&task).await.unwrap().remove(0);
    assert_eq!(
        h.db.active_capabilities(&goal.id).await.unwrap(),
        vec!["code_intelligence", "memory"]
    );
}

#[tokio::test]
async fn progressive_same_round_enable_does_not_authorize_unadvertised_calls() {
    use leveler_agent::capability::{CapabilityDisclosure, CapabilityId};
    let mut enable = tool_call(
        "enable",
        "capability",
        serde_json::json!({"action":"enable","id":"memory"}),
    );
    enable.message.content.push(ContentPart::ToolCall {
        call: ToolCall {
            id: ToolCallId::new("hidden"),
            name: "remember".into(),
            arguments: serde_json::json!({"content":"must not write"}),
        },
    });
    let mut h = harness(vec![
        enable,
        tool_call(
            "done",
            "update_goal",
            serde_json::json!({"status":"complete","summary":"done"}),
        ),
    ])
    .await;
    h.engine.factory.capabilities = Some(Arc::new(
        CapabilityDisclosure::new(
            CapabilityId::ALL.to_vec(),
            CapabilityId::ALL.to_vec(),
            vec![],
        )
        .unwrap(),
    ));
    let s = spec(&h);
    let session = h.engine.create_task(&s).await.unwrap();
    h.engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    let requests = h.requests.lock().unwrap();
    assert!(!requests[0].tools.iter().any(|tool| tool.name == "remember"));
    assert!(requests[1].tools.iter().any(|tool| tool.name == "remember"));
    assert!(requests[1].messages.iter().flat_map(|message| &message.content).any(|part| matches!(part, ContentPart::ToolResult { result } if result.call_id.as_str() == "hidden" && result.is_error && result.content.contains("not exposed"))));
}

#[tokio::test]
async fn progressive_resume_restores_pack_and_new_goal_starts_small() {
    use leveler_agent::capability::{CapabilityDisclosure, CapabilityId};
    let mut h = harness(vec![
        tool_call(
            "enable",
            "capability",
            serde_json::json!({"action":"enable","id":"memory"}),
        ),
        tool_call(
            "resume_done",
            "update_goal",
            serde_json::json!({"status":"complete","summary":"done"}),
        ),
        tool_call(
            "new_done",
            "update_goal",
            serde_json::json!({"status":"complete","summary":"done"}),
        ),
    ])
    .await;
    let fresh = || {
        Arc::new(
            CapabilityDisclosure::new(
                CapabilityId::ALL.to_vec(),
                CapabilityId::ALL.to_vec(),
                vec![],
            )
            .unwrap(),
        )
    };
    h.engine.factory.capabilities = Some(fresh());
    let mut s = spec(&h);
    s.runtime.continuation = leveler_agent::ContinuationPolicy::bounded(1);
    let session = h.engine.create_task(&s).await.unwrap();
    let stopped = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(stopped.outcome, TaskOutcome::BudgetLimited);
    h.engine.factory.capabilities = Some(fresh());
    s.runtime.continuation = leveler_agent::ContinuationPolicy::UntilTerminal;
    h.engine
        .resume(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    s.runtime.goal = "new unrelated goal".into();
    let new_session = h.engine.create_task(&s).await.unwrap();
    h.engine
        .run(&new_session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    let requests = h.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].tools.iter().any(|tool| tool.name == "memory"));
    assert!(requests[1].tools.iter().any(|tool| tool.name == "memory"));
    assert!(!requests[2].tools.iter().any(|tool| tool.name == "memory"));
}

#[tokio::test]
async fn progressive_capability_control_rejects_malformed_arguments() {
    use leveler_agent::capability::{CapabilityDisclosure, CapabilityId};
    for args in [
        serde_json::json!({"action":"list","id":123}),
        serde_json::json!({"action":"enable","id":""}),
        serde_json::json!({"action":"status","unexpected":true}),
    ] {
        let mut h = harness(vec![
            tool_call("malformed", "capability", args.clone()),
            tool_call(
                "done",
                "update_goal",
                serde_json::json!({"status":"complete","summary":"done"}),
            ),
        ])
        .await;
        h.engine.factory.capabilities = Some(Arc::new(
            CapabilityDisclosure::new(
                CapabilityId::ALL.to_vec(),
                CapabilityId::ALL.to_vec(),
                vec![],
            )
            .unwrap(),
        ));
        let s = spec(&h);
        let session = h.engine.create_task(&s).await.unwrap();
        h.engine
            .run(&session, &s, &mut |_| {}, CancellationToken::new())
            .await
            .unwrap();
        let requests = h.requests.lock().unwrap();
        assert!(requests[1].messages.iter().flat_map(|message| &message.content).any(|part| matches!(part, ContentPart::ToolResult {result} if result.call_id.as_str() == "malformed" && result.is_error)), "malformed control was accepted: {args}");
        assert_eq!(requests[0].tools, requests[1].tools);
        assert!(
            h.engine
                .factory
                .capabilities
                .as_ref()
                .unwrap()
                .active()
                .is_empty()
        );
    }
}

/// Each optional capability, once the model enables it, appears on the very
/// next request with its own tools and its own guidance — and the membership
/// is persisted for the goal. This is the demand protocol Memory, Skills and
/// Multi-Agent run through; Browser and Code Intelligence have separate
/// real-model receipts.
#[tokio::test]
async fn progressive_demand_exposes_memory_skills_and_delegation() {
    use leveler_agent::capability::{CapabilityDisclosure, CapabilityId};
    let mut h = harness(vec![
        tool_call(
            "m",
            "capability",
            serde_json::json!({"action":"enable","id":"memory"}),
        ),
        tool_call(
            "s",
            "capability",
            serde_json::json!({"action":"enable","id":"skills"}),
        ),
        tool_call(
            "a",
            "capability",
            serde_json::json!({"action":"enable","id":"multi_agent"}),
        ),
        tool_call(
            "done",
            "update_goal",
            serde_json::json!({"status":"complete","summary":"loaded what was needed"}),
        ),
    ])
    .await;
    h.engine.factory.capabilities = Some(Arc::new(
        CapabilityDisclosure::new(
            CapabilityId::ALL.to_vec(),
            CapabilityId::ALL.to_vec(),
            vec![],
        )
        .unwrap(),
    ));
    h.engine.factory.memory_catalog = "test memory title".into();
    let mut s = spec(&h);
    s.runtime.goal = "load optional capabilities on demand".into();
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::Completed);

    // Clone out of the lock so no `MutexGuard` is held across the awaits below.
    let requests = h.requests.lock().unwrap().clone();
    let has = |index: usize, name: &str| requests[index].tools.iter().any(|t| t.name == name);
    let segments = |index: usize| {
        requests[index]
            .control_context
            .blocks
            .iter()
            .map(|block| block.name.as_str())
            .collect::<Vec<_>>()
    };

    // The initial request offers the control, not any of the optional tools.
    for absent in ["memory", "remember", "forget", "load_skill", "spawn_agent"] {
        assert!(!has(0, absent), "{absent} exposed before the model asked");
    }
    assert!(!segments(0).contains(&"memory_guidance"));
    assert!(!segments(0).contains(&"skills_guidance"));
    assert!(!segments(0).contains(&"multi_agent_guidance"));

    // Enable memory → next request carries the memory tools and guidance.
    assert!(has(1, "remember") && has(1, "forget") && has(1, "memory"));
    assert!(segments(1).contains(&"memory_guidance"));
    assert!(!has(1, "load_skill"));
    assert!(!has(1, "spawn_agent"));

    // Enable skills → the loader appears, nothing else leaked in.
    assert!(has(2, "load_skill"));
    assert!(segments(2).contains(&"skills_guidance"));
    assert!(!has(2, "spawn_agent"));

    // Enable multi_agent → delegation appears with its guidance.
    assert!(has(3, "spawn_agent"));
    assert!(segments(3).contains(&"multi_agent_guidance"));
    assert!(has(3, "load_skill") && has(3, "remember"));

    let task = h
        .engine
        .engine
        .task_for_session(&session)
        .await
        .unwrap()
        .unwrap();
    let goal = h.db.for_task(&task).await.unwrap().remove(0);
    assert_eq!(
        h.db.active_capabilities(&goal.id).await.unwrap(),
        vec!["memory", "multi_agent", "skills"]
    );
}
