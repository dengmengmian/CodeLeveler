//! End-to-end DirectStrategy tests (plan B3): a scripted model runtime drives
//! the engine and every side of persistence is asserted — turns, turn-stamped
//! messages, the append-only event log, and the terminal outcome column.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AutoClarify, StopReason};
use leveler_core::{RequestId, ToolCallId};
use leveler_engine::{
    EngineEvent, ExecutionKind, ExecutorFactory, TaskEngine, TaskOutcome, TaskSpec,
};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::{
    Database, EventRepository, MessageRepository, SessionRepository, TurnRepository,
};
use leveler_tools::{ToolContext, default_registry};
use leveler_verifier::{CheckKind, VerificationCommand, VerificationPlan};

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
        // Completion Reconciliation Gate calls are answered out of band so
        // scripted FIFOs and request-count assertions stay about the loop.
        if let Some(reply) = leveler_test_support::reconcile_autopilot(&request) {
            return Ok(reply);
        }
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
    engine: TaskEngine,
    db: Database,
    dir: tempfile::TempDir,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
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
    let runtime = Arc::new(MockRuntime::new(responses));
    let requests = runtime.requests.clone();
    let db = Database::connect_in_memory().await.unwrap();
    let engine = TaskEngine {
        stores: leveler_storage::EngineStores::from_database(&db),
        runtime_id: leveler_core::RuntimeId::new("rt-test"),
        factory: ExecutorFactory {
            runtime,
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
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        supervisor: None,
    };
    Harness {
        engine,
        db,
        dir,
        requests,
    }
}

#[tokio::test]
async fn factory_reasoning_override_reaches_every_model_request() {
    let mut h = harness(vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "done"}),
    )])
    .await;
    h.engine.factory.overrides = Some(leveler_engine::ExecutionOverrides {
        reasoning_effort: Some(leveler_model::ReasoningEffort::High),
        ..leveler_engine::ExecutionOverrides::default()
    });
    let spec = spec(&h, VerificationPlan::default());
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

/// A TerminalStore that always refuses to commit — engine-level failure
/// injection for "the terminal fact is atomic or absent".
struct FailingTerminal;

#[async_trait]
impl leveler_storage::TerminalStore for FailingTerminal {
    async fn finish_task(
        &self,
        _: &leveler_core::SessionId,
        _: &str,
        _: &str,
        _: leveler_engine::TaskOutcome,
        _: leveler_lifecycle::SessionStatus,
        _: leveler_lifecycle::AgentState,
        _: leveler_core::Timestamp,
    ) -> Result<leveler_storage::EventRecord, leveler_storage::StorageError> {
        Err(leveler_storage::StorageError::InvalidData(
            "injected terminal failure".into(),
        ))
    }

    async fn finish_turn(
        &self,
        _: &leveler_core::SessionId,
        _: &leveler_core::TurnId,
        _: &str,
        _: &str,
        _: leveler_engine::TurnOutcome,
        _: leveler_core::Timestamp,
    ) -> Result<leveler_storage::EventRecord, leveler_storage::StorageError> {
        Err(leveler_storage::StorageError::InvalidData(
            "injected terminal failure".into(),
        ))
    }

    async fn finish_task_owned(
        &self,
        _: &leveler_core::OwnershipToken,
        _: &leveler_core::SessionId,
        _: &str,
        _: &str,
        _: leveler_engine::TaskOutcome,
        _: leveler_lifecycle::SessionStatus,
        _: leveler_lifecycle::AgentState,
        _: leveler_core::Timestamp,
    ) -> Result<leveler_storage::EventRecord, leveler_storage::OwnershipError> {
        Err(leveler_storage::OwnershipError::Storage(
            leveler_storage::StorageError::InvalidData("injected terminal failure".into()),
        ))
    }

    async fn finish_turn_owned(
        &self,
        _: &leveler_core::OwnershipToken,
        _: &leveler_core::SessionId,
        _: &leveler_core::TurnId,
        _: &str,
        _: &str,
        _: leveler_engine::TurnOutcome,
        _: leveler_core::Timestamp,
    ) -> Result<leveler_storage::EventRecord, leveler_storage::OwnershipError> {
        Err(leveler_storage::OwnershipError::Storage(
            leveler_storage::StorageError::InvalidData("injected terminal failure".into()),
        ))
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
}

/// Failure injection A/B at the engine level: when the atomic terminal commit
/// fails, the run errors, and NEITHER the terminal event NOR the outcome
/// projection is visible — no half-commit.
#[tokio::test]
async fn a_failed_terminal_commit_leaves_no_half_visible_task_fact() {
    let mut h = harness(vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "done"}),
    )])
    .await;
    h.engine.stores.terminal = Arc::new(FailingTerminal);
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    let result = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await;
    assert!(result.is_err(), "a failed terminal commit must propagate");

    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, None, "no outcome projection without its event");
    let terminal_events = EventRepository::new(&h.db)
        .load(&session)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == "task_finished" || e.event_type == "turn_finished")
        .count();
    assert_eq!(
        terminal_events, 0,
        "no terminal event without its projection"
    );
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
    h.engine.stores.messages = Arc::new(FailingMessages);
    let spec = spec(&h, VerificationPlan::default());
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
        // Completion Reconciliation Gate calls are answered out of band so
        // scripted FIFOs and request-count assertions stay about the loop.
        if let Some(reply) = leveler_test_support::reconcile_autopilot(&request) {
            return Ok(reply);
        }
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
    let spec = spec(&h, VerificationPlan::default());
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
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();
    let task = h.engine.task_for_session(&session).await.unwrap().unwrap();
    let rt = leveler_core::RuntimeId::new("rt-test");
    let old = leveler_storage::OwnershipStore::acquire(
        &h.db,
        &task,
        &rt,
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

    let stores = leveler_storage::EngineStores::from_database(&h.db);
    let reap = leveler_engine::reap_after_restart(&stores, &rt, None)
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
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();
    let task = h.engine.task_for_session(&session).await.unwrap().unwrap();
    let other = leveler_core::RuntimeId::new("rt-other");
    let foreign = leveler_storage::OwnershipStore::acquire(
        &h.db,
        &task,
        &other,
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

    // Restart reap as rt-test: conflict reported, turn untouched.
    let stores = leveler_storage::EngineStores::from_database(&h.db);
    let reap =
        leveler_engine::reap_after_restart(&stores, &leveler_core::RuntimeId::new("rt-test"), None)
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
}

#[tokio::test]
async fn create_task_records_the_durable_task_association() {
    let h = harness(Vec::new()).await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    let task = h
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
    let spec = spec(&h, VerificationPlan::default());
    // A session written by an older binary: session row only, no task row.
    let record = leveler_storage::SessionRecord::new(
        h.dir.path().display().to_string(),
        "add a function",
        "mock/m",
        leveler_core::now(),
    );
    SessionRepository::new(&h.db).create(&record).await.unwrap();
    let session = leveler_core::SessionId::new(record.id);
    assert_eq!(h.engine.task_for_session(&session).await.unwrap(), None);

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

fn spec(h: &Harness, plan: VerificationPlan) -> TaskSpec {
    TaskSpec {
        runtime: leveler_engine::RuntimeTaskSpec {
            goal: "add a function".to_string(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: leveler_agent::StepLimits::default(),
        },
        coding: leveler_engine::CodingTaskSpec {
            repository: h.dir.path().to_path_buf(),
            mode: PermissionProfile::Assisted,
            sandbox: false,
            verification: plan,
            base_commit: None,
        },
    }
}

fn gate(name: &str, program: &str) -> VerificationPlan {
    // Unix fixtures use `true`/`false`; neither exists on Windows runners,
    // so spell the same exit codes via cmd there.
    let (program, args) = match (cfg!(windows), program) {
        (true, "true") => ("cmd".to_string(), vec!["/c".into(), "exit 0".into()]),
        (true, "false") => ("cmd".to_string(), vec!["/c".into(), "exit 1".into()]),
        _ => (program.to_string(), Vec::new()),
    };
    VerificationPlan {
        commands: vec![VerificationCommand {
            name: name.into(),
            program,
            args,
            kind: CheckKind::Test,
            gating: true,
            timeout_seconds: 30,
            scope_policy: Default::default(),
        }],
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
    let spec = spec(&h, gate("ok", "true"));
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

    assert_eq!(report.outcome, TaskOutcome::Verified);
    assert_eq!(report.modified_files, vec!["src/lib.rs".to_string()]);

    // Session row: execution config + terminal outcome.
    let (mode, sandbox, kind, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (mode.as_str(), sandbox, kind.as_str(), outcome),
        ("assisted", false, "direct", Some(TaskOutcome::Verified))
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
        "verification_started",
        "verification_check",
        "verification_finished",
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
            outcome: TaskOutcome::Verified,
            ..
        }
    )));
}

#[tokio::test]
async fn no_gates_means_completed_unverified() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();
    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::CompletedUnverified);
    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::CompletedUnverified));
}

/// K19: pure Q&A (no mutations) with a green gate plan must stay
/// CompletedUnverified — never claim Verified just because the repo is healthy.
#[tokio::test]
async fn pure_qa_with_green_gates_is_completed_unverified() {
    let h = harness(vec![tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "auth uses JWT sessions"}),
    )])
    .await;
    let mut s = spec(&h, gate("ok", "true"));
    s.runtime.goal = "explain how auth works".to_string();
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::CompletedUnverified);
    assert!(
        report.modified_files.is_empty(),
        "Q&A must not leave mutations: {:?}",
        report.modified_files
    );
    assert!(
        report.verification.is_none(),
        "K19 early-exit skips verify when there is no mutation"
    );
    assert!(!report.outcome.is_success());
}

/// Implementation-class Direct task with real edits and all-green gates → Verified
/// via shared `finalize_task_outcome` (needs_mutation + has_mutation).
#[tokio::test]
async fn impl_with_mutations_and_green_gates_is_verified() {
    let h = harness(patch_resolve_and_proven_ac()).await;
    // Goal contains "add" → task_looks_like_implementation; patch mutates src/lib.rs.
    let s = spec(&h, gate("ok", "true"));
    assert!(
        s.runtime.goal.to_lowercase().contains("add"),
        "fixture goal must look like implementation"
    );
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Verified);
    assert!(
        !report.modified_files.is_empty(),
        "impl path requires observed mutation"
    );
    assert!(report.verification.is_some());
    assert!(report.outcome.is_success());
}

/// Green gates + real mutation is Verified even when the model never produced
/// usable acceptance criteria.
///
/// The gate ran the project's own checks against the edited tree — that is the
/// evidence. Requiring a *proven* criterion on top of it meant a model that
/// merely failed to restate its goal turned a correct, fully green turn into
/// "有改动但缺少系统级验收背书".
#[tokio::test]
async fn impl_green_gates_are_verified_without_proven_acceptance() {
    // No understand response → fallback optional AC → no proven required Met.
    let h = harness(patch_then_resolve()).await;
    let s = spec(&h, gate("ok", "true"));
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        report.outcome,
        TaskOutcome::Verified,
        "a passing gate on real changes is the completion evidence"
    );
    assert!(report.outcome.is_success());
}

/// Delete a workspace file; understand fails (no response) → mutation-derived
/// `test ! -e` proves absence → Verified despite optional fallback AC.
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
    let mut s = spec(&h, gate("ok", "true"));
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
    assert_eq!(
        report.outcome,
        TaskOutcome::Verified,
        "delete + green gates + MUT-DEL Met must Verified; got {:?}",
        report.outcome
    );
    assert!(
        report
            .modified_files
            .iter()
            .any(|p| p.contains("quicksort.py")),
        "modified_files should track delete: {:?}",
        report.modified_files
    );
    assert!(report.outcome.is_success());
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
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.rounds, 4);
}

#[tokio::test]
async fn active_goal_automatically_continues_in_a_new_persisted_turn_after_stall() {
    let h = harness(vec![
        text("still working 1"),
        text("still working 2"),
        text("still working 3"),
        text("still working 4"),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "finished after continuation"}),
        ),
    ])
    .await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::CompletedUnverified);
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.rounds, 5);
    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    assert_eq!(
        turns
            .iter()
            .map(|turn| (turn.kind.as_str(), turn.status.as_str()))
            .collect::<Vec<_>>(),
        vec![("user", "completed"), ("user", "completed")]
    );
}

#[tokio::test]
async fn bounded_eval_goal_still_stops_at_the_case_round_limit() {
    let h = harness(vec![
        tool_call("c1", "list_files", serde_json::json!({"path": "."})),
        tool_call("c2", "list_files", serde_json::json!({"path": "src"})),
        tool_call("c3", "read_file", serde_json::json!({"path": "src/lib.rs"})),
    ])
    .await;
    let mut spec = spec(&h, VerificationPlan::default());
    spec.runtime.continuation = leveler_agent::ContinuationPolicy::bounded(2);
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::BudgetLimited);
    assert_eq!(report.stop_reason, StopReason::BudgetExhausted);
    assert_eq!(report.rounds, 2);
}

#[tokio::test]
async fn direct_budget_stop_preserves_the_executor_detail() {
    let h = harness(vec![text("never reached")]).await;
    let mut spec = spec(&h, VerificationPlan::default());
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
async fn a_successful_repair_converges_on_fresh_verification() {
    // The goal turn leaves the tree failing the gate; the repair turn creates
    // the marker the gate checks for. Verified must come from the fresh
    // post-repair verification — the gate genuinely fails before the repair
    // and can only pass against the repaired tree.
    let mut responses = patch_then_resolve();
    responses.push(tool_call(
        "r1",
        "apply_patch",
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: repaired.marker\n+ok\n*** End Patch"
        }),
    ));
    responses.push(tool_call(
        "g2",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "repaired"}),
    ));
    let h = harness(responses).await;
    let plan = VerificationPlan {
        commands: vec![VerificationCommand {
            name: "marker".into(),
            program: if cfg!(windows) { "cmd" } else { "sh" }.into(),
            args: if cfg!(windows) {
                vec![
                    "/c".into(),
                    "if exist repaired.marker (exit 0) else (exit 1)".into(),
                ]
            } else {
                vec!["-c".into(), "test -f repaired.marker".into()]
            },
            kind: CheckKind::Test,
            gating: true,
            timeout_seconds: 30,
            scope_policy: Default::default(),
        }],
    };
    let spec = spec(&h, plan);
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        report.outcome,
        TaskOutcome::Verified,
        "a repaired tree with a green fresh verification must verify"
    );

    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    let kinds: Vec<&str> = turns.iter().map(|t| t.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["user", "repair"],
        "exactly one repair turn, then convergence"
    );
}

#[tokio::test]
async fn failed_verification_repairs_once_then_fails() {
    // Goal turn (patch + resolve), one repair turn (resolve again), gate
    // always fails → Failed after the bounded repair.
    let mut responses = patch_then_resolve();
    responses.push(tool_call(
        "g2",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "repaired"}),
    ));
    let h = harness(responses).await;
    let spec = spec(&h, gate("bad", "false"));
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::Failed);
    assert!(
        !report.outcome.is_success(),
        "failed verification must never count as automation success"
    );

    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    let kinds: Vec<&str> = turns.iter().map(|t| t.kind.as_str()).collect();
    assert_eq!(kinds, vec!["user", "repair"]);
    assert_eq!(
        turns[1].payload.as_deref(),
        Some(r#"{"attempt":1}"#),
        "the repair turn records its attempt"
    );

    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Failed));
}

#[tokio::test]
async fn agent_failure_persists_terminal_task_and_turn_events() {
    // No scripted response makes the first model request fail inside the turn.
    // The query projections already become failed; the canonical log must carry
    // the same terminal facts so replay cannot disagree with those projections.
    let h = harness(Vec::new()).await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    let error = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect_err("an exhausted model runtime must fail the task");
    assert!(matches!(error, leveler_engine::EngineError::Agent(_)));

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
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    let token = CancellationToken::new();
    token.cancel();
    let err = h
        .engine
        .run(&session, &spec, &mut |_| {}, token)
        .await
        .expect_err("a pre-cancelled run must not succeed");
    assert!(matches!(
        err,
        leveler_engine::EngineError::Agent(leveler_agent::AgentError::Cancelled)
    ));

    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::Interrupted));
    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    assert_eq!(turns[0].status, "interrupted");
}

/// Kill -9 / unclean TUI exit can leave a permanent `running` turn. Starting a
/// new turn must reap that zombie before inserting the next row.
#[tokio::test]
async fn starting_a_turn_reaps_orphan_running_siblings() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    // Simulate a zombie left by process kill: status running, no finished_at.
    let zombie = TurnRepository::new(&h.db)
        .start(&session, "chat", None, leveler_core::now())
        .await
        .unwrap();
    assert_eq!(zombie.status, "running");
    assert!(zombie.finished_at.is_none());

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::CompletedUnverified);

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
    let spec = spec(&h, VerificationPlan::default());
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

    // Phase 2: resume on the same database with a fresh scripted runtime.
    let dir2 = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir2.path().join("src")).unwrap();
    std::fs::write(dir2.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(dir2.path()).unwrap();
    let engine2 = TaskEngine {
        stores: leveler_storage::EngineStores::from_database(&h.db),
        runtime_id: leveler_core::RuntimeId::new("rt-test"),
        factory: ExecutorFactory {
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
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        supervisor: None,
    };
    let spec2 = TaskSpec {
        runtime: leveler_engine::RuntimeTaskSpec {
            goal: "add a function".to_string(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: leveler_agent::StepLimits::default(),
        },
        coding: leveler_engine::CodingTaskSpec {
            repository: dir2.path().to_path_buf(),
            mode: PermissionProfile::Assisted,
            sandbox: false,
            verification: VerificationPlan::default(),
            base_commit: None,
        },
    };

    let report = engine2
        .resume(&session, &spec2, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.outcome, TaskOutcome::CompletedUnverified);

    // Two turns: the interrupted original and the completed resume.
    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    let statuses: Vec<&str> = turns.iter().map(|t| t.status.as_str()).collect();
    assert_eq!(statuses, vec!["interrupted", "completed"]);
    let (_, _, _, outcome) = SessionRepository::new(&h.db)
        .execution(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(TaskOutcome::CompletedUnverified));
}

#[tokio::test]
async fn resume_refuses_a_successfully_completed_session() {
    let h = harness(patch_then_resolve()).await;
    let spec = spec(&h, VerificationPlan::default());
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

/// The engine's goal continuation (`continue_active_goal`) opens a whole new
/// turn AFTER the user already saw a final answer. Without an advisory event a
/// UI can only show a bare "waiting for model" for the entire continuation —
/// which reads as a hang. Every continuation round must name itself.
#[tokio::test]
async fn goal_continuation_announces_itself_before_re_prompting() {
    // Code change + a real answer, but `update_goal` never called: the closeout
    // spends its nudge budget on GoalUnresolved and stalls, which is exactly
    // what drives the engine into a continuation turn.
    let mut responses = vec![tool_call(
        "c1",
        "apply_patch",
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
        }),
    )];
    for _ in 0..8 {
        responses.push(text("已经改完了。"));
    }
    let h = harness(responses).await;
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    let mut seen: Vec<EngineEvent> = Vec::new();
    // Response exhaustion may end the run in an error; the advisory must have
    // been emitted before the continuation turn asked the model anything.
    let _ = h
        .engine
        .run(
            &session,
            &spec,
            &mut |e| seen.push(e),
            CancellationToken::new(),
        )
        .await;

    assert!(
        seen.iter().any(|event| matches!(
            event,
            EngineEvent::AdvisoryStarted { kind } if kind == "goal_continuation"
        )),
        "a goal continuation turn must announce itself: {seen:?}"
    );
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
    let s = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_ne!(
        report.outcome,
        TaskOutcome::Verified,
        "quiet must never claim verified: {:?}",
        report.outcome
    );
    assert_ne!(
        report.outcome,
        TaskOutcome::CompletedUnverified,
        "quiet must never claim completed-unverified success: {:?}",
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
    let s = spec(&h, gate("ok", "true"));
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.outcome, TaskOutcome::Verified);
}

/// The supervision decision is injectable: the same stalled script that the
/// default policy nudges into a second turn produces exactly ONE turn under a
/// supervisor that never continues. Mechanism stays in the engine; the
/// judgement is replaceable (convergence plan phase 4/5).
#[tokio::test]
async fn a_supervisor_policy_that_never_continues_leaves_one_turn() {
    let mut h = harness(vec![
        text("still working 1"),
        text("still working 2"),
        text("still working 3"),
        text("still working 4"),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "would finish after a continue"}),
        ),
    ])
    .await;
    h.engine = h
        .engine
        .with_supervisor(std::sync::Arc::new(leveler_engine::NoContinuation));
    let spec = spec(&h, VerificationPlan::default());
    let session = h.engine.create_task(&spec).await.unwrap();

    let report = h
        .engine
        .run(&session, &spec, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    // The goal was never resolved, because the supervisor did not re-drive it.
    assert_eq!(report.stop_reason, StopReason::Stalled);
    let turns = TurnRepository::new(&h.db).list(&session).await.unwrap();
    assert_eq!(
        turns.len(),
        1,
        "a no-continuation supervisor must not open a second turn: {turns:?}"
    );
}

/// R007b N7 (mechanism half): a change policy classifies as review-worthy gets
/// an independent review that the **harness** launches. The model never calls
/// `spawn_agent` here — before this, the reviewer designation could only be
/// honoured by a model that chose to delegate, which R008 and R009 both did not.
#[tokio::test]
async fn security_shaped_change_gets_a_harness_launched_review() {
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
    // The reviewer child drives its own rounds against the same mock runtime.
    responses.push(text("reviewed src/auth.rs: no blocking defect found"));
    responses.push(text("reviewed src/auth.rs: no blocking defect found"));

    let h = harness(responses).await;
    let s = spec(&h, gate("ok", "true"));
    let session = h.engine.create_task(&s).await.unwrap();
    let mut seen: Vec<EngineEvent> = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| seen.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let reviewers: Vec<String> = seen
        .iter()
        .filter_map(|event| match event {
            EngineEvent::SubAgentStarted { id, role, .. } if role == "reviewer" => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reviewers.len(),
        1,
        "the harness must launch exactly one reviewer for a security-shaped change"
    );
    assert!(
        seen.iter().any(|event| matches!(
            event,
            EngineEvent::SubAgentFinished { id, ok: true, .. } if *id == reviewers[0]
        )),
        "the reviewer must reach a terminal result, not merely be announced"
    );
    assert_eq!(
        report.outcome,
        TaskOutcome::Verified,
        "a review that actually happened must not leave the task downgraded"
    );
}

/// The reviewer is not a tax on every task: an ordinary edit that policy does
/// not classify as review-worthy runs no reviewer at all.
#[tokio::test]
async fn ordinary_change_launches_no_reviewer() {
    let h = harness(patch_then_resolve()).await;
    let s = spec(&h, gate("ok", "true"));
    let session = h.engine.create_task(&s).await.unwrap();
    let mut seen: Vec<EngineEvent> = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| seen.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, EngineEvent::SubAgentStarted { .. })),
        "a narrow, non-security edit must not pay for an independent review"
    );
    assert_eq!(report.outcome, TaskOutcome::Verified);
}

#[tokio::test]
async fn independent_review_off_skips_even_security_shaped_changes() {
    let mut h = harness(vec![
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
    ])
    .await;
    h.engine.factory.independent_review = leveler_engine::IndependentReviewPolicy::Off;
    let s = spec(&h, gate("ok", "true"));
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
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            EngineEvent::SubAgentStarted { role, .. } if role == "reviewer"
        )),
        "independent_review=off must not launch a reviewer, even on auth.rs"
    );
    assert!(seen.iter().any(|event| matches!(
        event,
        EngineEvent::ReviewStage { action, .. } if action == "not_required"
    )));
}

#[tokio::test]
async fn independent_review_always_launches_on_an_ordinary_change() {
    let mut responses = patch_then_resolve();
    responses.push(text("ordinary change: no blocking defect"));
    responses.push(text("ordinary change: no blocking defect"));
    let mut h = harness(responses).await;
    h.engine.factory.independent_review = leveler_engine::IndependentReviewPolicy::Always;
    let s = spec(&h, gate("ok", "true"));
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
        "always must launch a reviewer after any product mutation"
    );
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

    let h = harness(responses).await;
    let s = spec(&h, gate("ok", "true"));
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
    let mut s = spec(h, VerificationPlan::default());
    s.runtime.goal = goal.to_string();
    // UntilTerminal keeps the round budget UNPINNED, so hitting the per-turn
    // ceiling ends a WORK WINDOW (policy opens the next one), not the goal.
    s.runtime.continuation = leveler_agent::ContinuationPolicy::UntilTerminal;
    s.runtime.limits = leveler_agent::StepLimits {
        max_rounds: Some(rounds_per_window),
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

fn patch_update(id: &str, path: &str, old: &str, new: &str) -> ModelResponse {
    tool_call(
        id,
        "apply_patch",
        serde_json::json!({
            "patch": format!(
                "*** Begin Patch\n*** Update File: {path}\n-{old}\n+{new}\n*** End Patch"
            )
        }),
    )
}

fn read_call_named(id: &str, path: &str) -> ModelResponse {
    tool_call(id, "read_file", serde_json::json!({"path": path}))
}

/// Refinement windows — real writes to files the goal already touched — are
/// progress. The goal must survive them and reach its natural finish.
#[tokio::test]
async fn refinement_windows_count_as_progress() {
    let h = harness(vec![
        // Window 1 (2 rounds): create the file, then read it → round ceiling.
        patch_add("w1a", "src/feature.rs", "pub fn feature() { /* v1 */ }"),
        read_call_named("w1b", "src/feature.rs"),
        // Window 2: REWRITE the same file (no new paths), then read → ceiling.
        patch_update(
            "w2a",
            "src/feature.rs",
            "pub fn feature() { /* v1 */ }",
            "pub fn feature() { /* v2 */ }",
        ),
        read_call_named("w2b", "src/feature.rs"),
        // Window 3: rewrite again — still no new paths.
        patch_update(
            "w3a",
            "src/feature.rs",
            "pub fn feature() { /* v2 */ }",
            "pub fn feature() { /* v3 */ }",
        ),
        read_call_named("w3b", "src/feature.rs"),
        // Window 4: the model closes the goal out explicitly. (Continued
        // windows run in goal mode, where bare prose never ends the turn.)
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "refinement done"}),
        ),
        // Slack for advisory calls; unused responses are harmless.
        text("unused"),
        text("unused"),
    ])
    .await;
    let s = spec_windowed(&h, "add the feature and polish it", 2);
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert!(
        matches!(
            report.stop_reason,
            leveler_agent::StopReason::Completed
                | leveler_agent::StopReason::Answered
                | leveler_agent::StopReason::CompletedUnverified
        ),
        "three windows of real work must reach the natural finish, not a \
         no-progress kill; got {:?} ({:?})",
        report.stop_reason,
        report.outcome,
    );
    let file = std::fs::read_to_string(h.dir.path().join("src/feature.rs")).unwrap();
    assert!(
        file.contains("v3"),
        "every refinement window's write must have landed: {file}"
    );
}

/// The counter-guard: windows that only re-observe — no mutation, no
/// verification advancement — must still terminate the goal.
#[tokio::test]
async fn pure_observation_windows_still_terminate() {
    let h = harness(vec![
        // Window 1: real work.
        patch_add("s1", "src/feature.rs", "pub fn feature() {}"),
        read_call_named("s2", "src/feature.rs"),
        // Windows 2..: nothing but re-reads. The engine must stop granting
        // windows once the no-progress cap is hit; extra responses stay unused.
        read_call_named("s3", "src/feature.rs"),
        read_call_named("s4", "src/feature.rs"),
        read_call_named("s5", "src/feature.rs"),
        read_call_named("s6", "src/feature.rs"),
        read_call_named("s7", "src/feature.rs"),
        read_call_named("s8", "src/feature.rs"),
        text("should never be reached"),
    ])
    .await;
    let s = spec_windowed(&h, "add the feature", 2);
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::TurnLimitReached,
        "pure re-observation must exhaust the no-progress cap, not run forever: {:?}",
        report.stop_reason
    );
    assert!(
        !report.outcome.is_success(),
        "a goal that stopped making progress must not read as success"
    );
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
    let h = harness(vec![
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
        !report.outcome.is_success(),
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
        // Completion Reconciliation Gate calls are answered out of band so
        // scripted FIFOs and request-count assertions stay about the loop.
        if let Some(reply) = leveler_test_support::reconcile_autopilot(&request) {
            return Ok(reply);
        }
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
    let engine = TaskEngine {
        stores: leveler_storage::EngineStores::from_database(&db),
        runtime_id: leveler_core::RuntimeId::new("rt-test"),
        factory: ExecutorFactory {
            runtime,
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
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        supervisor: None,
    };
    let s = TaskSpec {
        runtime: leveler_engine::RuntimeTaskSpec {
            goal: "add a login entry point".to_string(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: leveler_agent::StepLimits::default(),
        },
        coding: leveler_engine::CodingTaskSpec {
            repository: dir.path().to_path_buf(),
            mode: PermissionProfile::Assisted,
            sandbox: false,
            verification: gate("ok", "true"),
            base_commit: None,
        },
    };
    let session = engine.create_task(&s).await.unwrap();
    let report = engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        report.outcome,
        TaskOutcome::CompletedUnverified,
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
    let s = spec(&h, gate("ok", "true"));
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
    let h = harness(responses).await;
    let s = spec(&h, gate("ok", "true"));
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

/// A reviewer's blocking correctness finding refuses a Verified closure and
/// survives durably: adopted into the persisted ledger at Acknowledged, with
/// the refusal staged as a review_stage row — never a silent downgrade.
#[tokio::test]
async fn a_blocking_reviewer_finding_refuses_verified_closure() {
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

    let h = harness(responses).await;
    let s = spec(&h, gate("ok", "true"));
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        report.outcome,
        TaskOutcome::CompletedUnverified,
        "an open blocking finding must refuse Verified"
    );

    let stages = review_stage_rows(&h.db, &session).await;
    assert!(
        stages
            .iter()
            .any(|(required, action, _)| *required && action == "blocking_finding_open"),
        "the refusal must be staged durably: {stages:?}"
    );

    let ledger = persisted_ledger(&h.db, &session)
        .await
        .expect("adoption must persist a ledger snapshot");
    assert_eq!(ledger.findings.len(), 1);
    let f = &ledger.findings[0];
    assert_eq!(f.state, leveler_lifecycle::FindingState::Acknowledged);
    assert_eq!(f.role, "reviewer");
    assert!(f.source_child.starts_with("reviewer-"));
    assert!(f.blocking);
    assert_eq!(f.summary, "login() accepts any password");
}

/// A reviewer finding that is NOT blocking is knowledge, not a gate: it is
/// adopted durably but the verified closure stands.
#[tokio::test]
async fn a_non_blocking_reviewer_finding_does_not_refuse_verified() {
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

    let h = harness(responses).await;
    let s = spec(&h, gate("ok", "true"));
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        report.outcome,
        TaskOutcome::Verified,
        "a non-blocking finding must not downgrade a reviewed task"
    );
    let ledger = persisted_ledger(&h.db, &session)
        .await
        .expect("the finding must still be adopted durably");
    assert_eq!(ledger.findings.len(), 1);
    assert!(!ledger.findings[0].blocking);
}

/// EventLog replay: reloading the last EvidenceLedgerUpdated after a
/// reviewer adoption returns the same single Acknowledged finding. Resume of
/// a CompletedUnverified session is refused by the engine (start a new
/// task); the durable contract is the snapshot, not a second drive.
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
    let h = harness(responses).await;
    let s = spec(&h, gate("ok", "true"));
    let session = h.engine.create_task(&s).await.unwrap();
    h.engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .unwrap();

    let first = persisted_ledger(&h.db, &session).await.unwrap();
    let second = persisted_ledger(&h.db, &session).await.unwrap();
    assert_eq!(first.findings.len(), 1);
    assert_eq!(first, second, "reload must be identical, not duplicated");
    assert_eq!(
        first.findings[0].state,
        leveler_lifecycle::FindingState::Acknowledged
    );
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

    let h = harness(responses).await;
    let s = spec(&h, gate("ok", "true"));
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
    assert_eq!(
        contribution.findings_acknowledged, 1,
        "adoption lands at Acknowledged, so the parent received it"
    );
    assert_eq!(contribution.findings_accepted, 0, "nobody judged it yet");
    assert_eq!(contribution.findings_open_blocking, 1);
    assert_eq!(contribution.role, "reviewer");
    assert_eq!(
        contribution.source,
        Some(leveler_lifecycle::ContributionSource::IndependentReviewer {
            review_id: contribution.child_id.clone(),
        }),
        "the trace must name which mechanism produced the finding"
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

    let h = harness(responses).await;
    let s = spec(&h, gate("ok", "true"));
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
