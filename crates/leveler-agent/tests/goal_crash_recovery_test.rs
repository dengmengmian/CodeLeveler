//! Crash recovery for the Goal-creation → TurnStart window.
//!
//! Opening a goal and starting the turn that drives it used to be two
//! independent writes. A process that died between them left a Running goal
//! that no turn named and no later message could recover. These tests inject
//! the failure at the TurnStore boundary and then recover through the real
//! engine, real SQLite and the real turn WAL — no in-memory state stands in
//! for durability.

use async_trait::async_trait;
use leveler_agent::coding::{CodingRuntime, ExecutorFactory, TaskSpec};
use leveler_agent::{AutoClarify, StepLimits};
use leveler_core::{RequestId, SessionId, Timestamp, ToolCallId, TurnId};
use leveler_engine::{EngineBoot, ExecutionKind, TaskEngine};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::{
    Database, EngineStores, GoalState, ModelRequestRepository, SessionRecord, SessionRepository,
    StorageError, TurnRecord, TurnRepository, TurnStore,
};
use leveler_tools::ToolContext;
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use tokio_util::sync::CancellationToken;

const OBJECTIVE_A: &str = "修复崩溃恢复窗口";
const OBJECTIVE_B: &str = "验证并发会话隔离";

/// A turn store that fails the NEXT start, leaving exactly the durable state
/// a crash between goal creation and turn start leaves: the goal row and the
/// pending declaration, and no turn.
struct FailNextStart {
    inner: Arc<dyn TurnStore>,
    fail: AtomicBool,
}

impl FailNextStart {
    fn new(inner: Arc<dyn TurnStore>) -> Self {
        Self {
            inner,
            fail: AtomicBool::new(false),
        }
    }

    fn inject(&self) {
        self.fail.store(true, Ordering::SeqCst);
    }

    fn should_fail(&self) -> bool {
        self.fail.swap(false, Ordering::SeqCst)
    }
}

fn injected() -> leveler_storage::OwnershipError {
    leveler_storage::OwnershipError::Storage(StorageError::InvalidData(
        "injected crash between goal creation and turn start".into(),
    ))
}

#[async_trait]
impl TurnStore for FailNextStart {
    async fn start(
        &self,
        session_id: &SessionId,
        kind: &str,
        payload: Option<&str>,
        now: Timestamp,
    ) -> Result<TurnRecord, StorageError> {
        self.inner.start(session_id, kind, payload, now).await
    }

    async fn list_running(
        &self,
        session_id: Option<&SessionId>,
    ) -> Result<Vec<TurnRecord>, StorageError> {
        self.inner.list_running(session_id).await
    }

    async fn list_recovery_candidates(
        &self,
        session_id: Option<&SessionId>,
    ) -> Result<Vec<TurnRecord>, StorageError> {
        self.inner.list_recovery_candidates(session_id).await
    }

    async fn list_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<TurnRecord>, StorageError> {
        self.inner.list_for_session(session_id).await
    }

    async fn start_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        kind: &str,
        payload: Option<&str>,
        now: Timestamp,
    ) -> Result<TurnRecord, leveler_storage::OwnershipError> {
        if self.should_fail() {
            return Err(injected());
        }
        self.inner
            .start_owned(token, session_id, kind, payload, now)
            .await
    }

    async fn start_owned_consuming_declaration(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &SessionId,
        kind: &str,
        payload: Option<&str>,
        now: Timestamp,
        declaration: &str,
    ) -> Result<TurnRecord, leveler_storage::OwnershipError> {
        if self.should_fail() {
            return Err(injected());
        }
        self.inner
            .start_owned_consuming_declaration(token, session_id, kind, payload, now, declaration)
            .await
    }

    async fn bind_goal(
        &self,
        token: &leveler_core::OwnershipToken,
        turn_id: &TurnId,
        goal_id: &leveler_core::GoalId,
    ) -> Result<(), leveler_storage::OwnershipError> {
        self.inner.bind_goal(token, turn_id, goal_id).await
    }
}

struct Runtime {
    calls: AtomicU32,
    status: Mutex<&'static str>,
    /// Per-main-round script: `text` answers in plain text, anything else is
    /// an `update_goal` status. Empty falls back to `status`.
    script: Mutex<VecDeque<&'static str>>,
}

#[async_trait]
impl ModelRuntime for Runtime {
    async fn profile(&self, _: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id":"m","provider":"mock","model_id":"m","protocol":"openai_chat",
            "capabilities":{"streaming":true,"tool_calling":true,"parallel_tool_calls":true,"structured_output":false,"reasoning":false,"vision":false},
            "limits":{"context_window":64000,"reliable_context":32000,"max_output_tokens":2048,"max_tool_schema_bytes":65536,"max_parallel_tool_calls":4},
            "pricing":{"input_usd_per_mtok":1.0,"output_usd_per_mtok":4.0}
        }))
        .unwrap())
    }

    async fn generate(
        &self,
        _: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unreachable!("tracked turns stream")
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        if request.tools.is_empty() {
            return Ok(leveler_model::stream_from_response(ModelResponse {
                request_id: RequestId::generate(),
                message: Message::text(Role::Assistant, "summary"),
                finish_reason: FinishReason::Stop,
                usage: usage(),
            }));
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        let scripted = self.script.lock().unwrap().pop_front();
        if scripted == Some("text") {
            return Ok(leveler_model::stream_from_response(ModelResponse {
                request_id: RequestId::generate(),
                message: Message::text(Role::Assistant, "an ordinary chat answer"),
                finish_reason: FinishReason::Stop,
                usage: usage(),
            }));
        }
        let status = scripted.unwrap_or_else(|| *self.status.lock().unwrap());
        let message = Message::from_parts(
            Role::Assistant,
            vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(format!("goal-{}", self.calls.load(Ordering::SeqCst))),
                    name: "update_goal".into(),
                    arguments: serde_json::json!({
                        "status": status,
                        "summary": "scripted terminal"
                    }),
                },
            }],
            None,
        );
        Ok(leveler_model::stream_from_response(ModelResponse {
            request_id: RequestId::generate(),
            message,
            finish_reason: FinishReason::ToolCalls,
            usage: usage(),
        }))
    }
}

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 40,
        output_tokens: 5,
        ..Default::default()
    }
}

struct Fixture {
    runtime: Arc<Runtime>,
    coding: CodingRuntime,
    db: Database,
    turns: Arc<FailNextStart>,
    session: SessionId,
    dir: tempfile::TempDir,
}

async fn fixture(status: &'static str) -> Fixture {
    fixture_scripted(status, Vec::new()).await
}

async fn fixture_scripted(status: &'static str, script: Vec<&'static str>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect_in_memory().await.unwrap();
    let record = SessionRecord::new(
        dir.path().to_string_lossy().as_ref(),
        "goal",
        "mock/m",
        leveler_core::now(),
    );
    SessionRepository::new(&db).create(&record).await.unwrap();
    let session = SessionId::new(record.id);
    // The product only opens goals from a Goal-axis session; recovery must
    // work with that durable pair present.
    SessionRepository::new(&db)
        .set_turn_collaboration(&session, "goal", leveler_core::now())
        .await
        .unwrap();
    let runtime = Arc::new(Runtime {
        calls: AtomicU32::new(0),
        status: Mutex::new(status),
        script: Mutex::new(script.into()),
    });
    let mut stores = EngineStores::from_database(&db);
    let turns = Arc::new(FailNextStart::new(stores.turns.clone()));
    stores.turns = turns.clone();
    let coding = coding_runtime(stores, runtime.clone(), dir.path(), "goal-crash");
    Fixture {
        runtime,
        coding,
        db,
        turns,
        session,
        dir,
    }
}

fn coding_runtime(
    stores: EngineStores,
    runtime: Arc<Runtime>,
    root: &std::path::Path,
    runtime_id: &str,
) -> CodingRuntime {
    let context = ToolContext::with_environment(
        Workspace::new(root).unwrap(),
        PermissionProfile::Assisted,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            root.into(),
            std::env::temp_dir(),
        )),
    );
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    CodingRuntime {
        engine: TaskEngine {
            stores,
            runtime_id: leveler_core::RuntimeId::new(runtime_id),
            boot: EngineBoot {
                id: leveler_core::BootId::generate(),
                liveness: Arc::new(leveler_test_support::TestBoots::new()),
            },
        },
        factory: ExecutorFactory {
            resource_grants: Arc::new(leveler_storage::MemoryResourceGrantStore::default()),
            runtime,
            registry: Arc::new(registry),
            tool_context: context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: None,
            memory_expose: true,
            memory_service: None,
            memory_lanes: leveler_memory::MemoryLanes::ALL,
            background_tasks: Arc::new(leveler_execution::BackgroundTaskRegistry::new()),
            permission_rules: Default::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(".".into()),
            steering: None,
            allow_delegation: true,
            allow_host_input: true,
            capabilities: None,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Off,
            develop_model: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        task_cancel: Arc::new(AtomicBool::new(false)),
    }
}

fn spec(fx: &Fixture, goal: &str) -> TaskSpec {
    TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: goal.to_string(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::bounded(6),
            limits: StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(fx.dir.path().into()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    }
}

async fn goals(fx: &Fixture) -> Vec<leveler_storage::GoalRecord> {
    let stores = EngineStores::from_database(&fx.db);
    let task = stores
        .tasks
        .task_for_session(&fx.session)
        .await
        .unwrap()
        .unwrap();
    stores.goals.for_task(&task).await.unwrap()
}

async fn turns(fx: &Fixture) -> Vec<TurnRecord> {
    TurnRepository::new(&fx.db).list(&fx.session).await.unwrap()
}

async fn pending(fx: &Fixture) -> Option<String> {
    SessionRepository::new(&fx.db)
        .pending_goal(&fx.session)
        .await
        .unwrap()
}

/// Open a goal, then die before its turn exists. The durable state must be
/// recoverable: the objective is in the declaration, and the next Goal turn
/// adopts the SAME goal instead of opening one named `继续`.
#[tokio::test]
async fn a_goal_without_its_turn_recovers_the_original_objective() {
    let fx = fixture("complete").await;
    fx.turns.inject();
    let first = fx
        .coding
        .run(
            &fx.session,
            &spec(&fx, OBJECTIVE_A),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await;
    assert!(first.is_err(), "the injected turn-start failure surfaces");
    assert_eq!(
        turns(&fx).await.len(),
        0,
        "the crash happened before any turn row existed"
    );
    let opened = goals(&fx).await;
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].objective, OBJECTIVE_A);
    assert_eq!(
        opened[0].state,
        GoalState::Running,
        "the goal still owes the work the user declared"
    );
    assert_eq!(
        pending(&fx).await.as_deref(),
        Some(OBJECTIVE_A),
        "the declaration survives the dead turn start; it is consumed only by a real start"
    );

    // Recovery runs on a fresh runtime (the crashed boot is gone) and with a
    // working store. The user's next message is a continuation, not a new
    // objective.
    let recovered = coding_runtime(
        EngineStores::from_database(&fx.db),
        fx.runtime.clone(),
        fx.dir.path(),
        "goal-crash-recovered",
    );
    let report = recovered
        .run(
            &fx.session,
            &spec(&fx, "继续"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("the recovered goal runs to a terminal");
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Completed);

    let goals = goals(&fx).await;
    assert_eq!(goals.len(), 1, "one goal, not one per attempt: {goals:?}");
    assert_eq!(goals[0].id, opened[0].id, "the identity is preserved");
    assert_eq!(
        goals[0].objective, OBJECTIVE_A,
        "the continuation word never replaces the original objective"
    );
    assert_eq!(goals[0].state, GoalState::Settled);
    assert_eq!(
        SessionRepository::new(&fx.db)
            .get(&fx.session)
            .await
            .unwrap()
            .unwrap()
            .collaboration,
        "chat",
        "the faulted window's terminal ended the session's Goal route; the goal \
         is recovered by its identity, not by a lingering axis"
    );
    assert_eq!(pending(&fx).await, None, "the start consumed it");

    let turns = turns(&fx).await;
    assert_eq!(turns.len(), 1, "only the successful attempt has a turn");
    let lineage = leveler_engine::decode_turn_continuation(turns[0].payload.as_deref().unwrap())
        .expect("a v2 WAL payload");
    assert_eq!(
        lineage.goal_id.as_ref(),
        Some(&goals[0].id),
        "the turn names the goal it serves"
    );
    let bodies = ModelRequestRepository::new(&fx.db)
        .load_for_session(&fx.session)
        .await
        .unwrap();
    assert_eq!(bodies.len(), 1, "the failed start bought no model call");
}

/// A crash after the turn WAL and goal bind, but before its terminal: the
/// zombie turn is reaped and the resume continues the SAME goal.
#[tokio::test]
async fn a_bound_turn_without_a_terminal_resumes_the_same_goal() {
    let fx = fixture("complete").await;
    // Give the session a task and a durable goal exactly as `run_task` does,
    // then write the turn + lineage the engine writes before executing.
    let token = fx
        .coding
        .engine
        .start_task(
            &fx.session,
            leveler_lifecycle::AgentState::Execute,
            &leveler_engine::TaskExecution {
                mode: "assisted".into(),
                sandbox: false,
                kind: ExecutionKind::Direct,
            },
        )
        .await
        .unwrap();
    let goal = fx
        .coding
        .engine
        .open_goal(&token, OBJECTIVE_A)
        .await
        .unwrap();
    SessionRepository::new(&fx.db)
        .set_pending_goal_if_absent(&fx.session, OBJECTIVE_A, leveler_core::now())
        .await
        .unwrap();
    let payload = serde_json::json!({
        "version": 2,
        "initiating_message": Message::text(Role::User, "开始工作"),
        "objective": leveler_lifecycle::ObjectiveAnchor::from_session_goal(OBJECTIVE_A),
        "goal_id": goal.as_str(),
    })
    .to_string();
    let crashed = TurnStore::start_owned(
        &*fx.coding.engine.stores.turns,
        &token,
        &fx.session,
        "user",
        Some(&payload),
        leveler_core::now(),
    )
    .await
    .unwrap();
    fx.coding.engine.release_ownership(&token).await.unwrap();

    let resumed = fx
        .coding
        .resume_with_instruction(
            &fx.session,
            &spec(&fx, OBJECTIVE_A),
            Message::user_input("继续"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("the interrupted window resumes");
    assert_eq!(resumed.stop_reason, leveler_agent::StopReason::Completed);

    let turns = turns(&fx).await;
    assert_eq!(turns.len(), 2, "the zombie plus the resumed turn");
    assert_eq!(turns[0].id, crashed.id, "the crashed row is the one reaped");
    assert_ne!(
        turns[0].status, "running",
        "no turn may stay Running after recovery"
    );
    let goals = goals(&fx).await;
    assert_eq!(goals.len(), 1, "no duplicate goal: {goals:?}");
    assert_eq!(goals[0].id, goal);
    assert_eq!(goals[0].objective, OBJECTIVE_A);
    assert_eq!(goals[0].state, GoalState::Settled);
    assert_eq!(pending(&fx).await, None);
}

/// Recovery that itself dies at the next turn start must be repeatable: the
/// second recovery still finds the one goal and the original objective.
#[tokio::test]
async fn recovery_survives_a_second_crash_and_repeated_resumes() {
    let fx = fixture("blocked").await;
    fx.turns.inject();
    let first = fx
        .coding
        .run(
            &fx.session,
            &spec(&fx, OBJECTIVE_A),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await;
    assert!(first.is_err());
    let goal_id = goals(&fx).await[0].id.clone();

    // First recovery attempt dies at its own TurnStart. The goal was reopened
    // and the pending declaration is still there, so nothing is lost.
    fx.turns.inject();
    let second = fx
        .coding
        .run(
            &fx.session,
            &spec(&fx, "继续"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await;
    assert!(second.is_err(), "the second injected crash surfaces");
    assert_eq!(turns(&fx).await.len(), 0, "still no turn row");
    let goals_now = goals(&fx).await;
    assert_eq!(goals_now.len(), 1, "{goals_now:?}");
    assert_eq!(goals_now[0].id, goal_id, "the goal was not reopened twice");
    assert_eq!(goals_now[0].objective, OBJECTIVE_A);
    assert_eq!(
        goals_now[0].state,
        GoalState::Running,
        "a failed recovery does not settle the goal"
    );

    // Third attempt: blocked. The goal stays owed, one record.
    let third = fx
        .coding
        .run(
            &fx.session,
            &spec(&fx, "继续"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("a blocked goal is a normal report");
    assert_eq!(third.stop_reason, leveler_agent::StopReason::Blocked);
    let goals_now = goals(&fx).await;
    assert_eq!(goals_now.len(), 1, "{goals_now:?}");
    assert_eq!(goals_now[0].id, goal_id);
    assert_eq!(goals_now[0].objective, OBJECTIVE_A);

    // A repeat resume retries the SAME settled goal, and only a later
    // completion settles it.
    *fx.runtime.status.lock().unwrap() = "complete";
    let fourth = fx
        .coding
        .resume_with_instruction(
            &fx.session,
            &spec(&fx, OBJECTIVE_A),
            Message::user_input("继续"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("the blocked goal resumes");
    assert_eq!(fourth.stop_reason, leveler_agent::StopReason::Completed);
    let goals_now = goals(&fx).await;
    assert_eq!(goals_now.len(), 1);
    assert_eq!(goals_now[0].id, goal_id);
    assert_eq!(goals_now[0].objective, OBJECTIVE_A);
    assert_eq!(goals_now[0].state, GoalState::Settled);
    assert_eq!(pending(&fx).await, None);
}

/// Two sessions recovering at once keep their own goal, objective and turn
/// lineage: no cross-adoption through the shared task/goal tables.
#[tokio::test]
async fn concurrent_sessions_recover_their_own_goals() {
    let fx = fixture("complete").await;
    let record_b = SessionRecord::new(
        fx.dir.path().to_string_lossy().as_ref(),
        "goal",
        "mock/m",
        leveler_core::now(),
    );
    SessionRepository::new(&fx.db)
        .create(&record_b)
        .await
        .unwrap();
    let session_b = SessionId::new(record_b.id);

    for (session, objective) in [(&fx.session, OBJECTIVE_A), (&session_b, OBJECTIVE_B)] {
        let token = fx
            .coding
            .engine
            .start_task(
                session,
                leveler_lifecycle::AgentState::Execute,
                &leveler_engine::TaskExecution {
                    mode: "assisted".into(),
                    sandbox: false,
                    kind: ExecutionKind::Direct,
                },
            )
            .await
            .unwrap();
        fx.coding.engine.open_goal(&token, objective).await.unwrap();
        SessionRepository::new(&fx.db)
            .set_pending_goal_if_absent(session, objective, leveler_core::now())
            .await
            .unwrap();
        fx.coding.engine.release_ownership(&token).await.unwrap();
    }

    let spec_a = spec(&fx, "继续");
    let spec_b = spec(&fx, "继续");
    let mut ignore_a = |_| {};
    let mut ignore_b = |_| {};
    let (a, b) = tokio::join!(
        fx.coding.run(
            &fx.session,
            &spec_a,
            &mut ignore_a,
            CancellationToken::new()
        ),
        fx.coding
            .run(&session_b, &spec_b, &mut ignore_b, CancellationToken::new())
    );
    assert_eq!(a.unwrap().stop_reason, leveler_agent::StopReason::Completed);
    assert_eq!(b.unwrap().stop_reason, leveler_agent::StopReason::Completed);

    let stores = EngineStores::from_database(&fx.db);
    for (session, objective) in [(&fx.session, OBJECTIVE_A), (&session_b, OBJECTIVE_B)] {
        let task = stores
            .tasks
            .task_for_session(session)
            .await
            .unwrap()
            .unwrap();
        let goals = stores.goals.for_task(&task).await.unwrap();
        assert_eq!(goals.len(), 1, "one goal per task: {goals:?}");
        assert_eq!(goals[0].objective, objective);
        assert_eq!(goals[0].state, GoalState::Settled);
        let turns = TurnRepository::new(&fx.db).list(session).await.unwrap();
        assert_eq!(turns.len(), 1);
        let lineage =
            leveler_engine::decode_turn_continuation(turns[0].payload.as_deref().unwrap()).unwrap();
        assert_eq!(lineage.goal_id.as_ref(), Some(&goals[0].id));
    }
}

/// A Goal adopted from a pending declaration by a RESUME roots its checkpoint
/// at the adopted turn, not at the pre-goal Chat window the resume continued.
/// Before that rule, the milestone checkpoint failed with "turn X is not the
/// root of goal G" and the blocked goal was reported Interrupted instead of
/// reaching its own terminal.
#[tokio::test]
async fn a_pending_goal_adopted_by_resume_roots_its_checkpoint_at_its_own_turn() {
    let fx = fixture_scripted("blocked", vec!["text", "blocked"]).await;
    let chat = fx
        .coding
        .chat(
            &fx.session,
            &spec(&fx, "普通聊天"),
            vec![ContentPart::Text {
                text: "普通聊天".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("the chat turn answers");
    assert_eq!(chat.outcome, leveler_engine::TaskOutcome::Completed);

    // The crash window: a Running goal + its pending declaration, no turn.
    SessionRepository::new(&fx.db)
        .set_turn_collaboration(&fx.session, "goal", leveler_core::now())
        .await
        .unwrap();
    let token = fx
        .coding
        .engine
        .start_task(
            &fx.session,
            leveler_lifecycle::AgentState::Execute,
            &leveler_engine::TaskExecution {
                mode: "assisted".into(),
                sandbox: false,
                kind: ExecutionKind::Direct,
            },
        )
        .await
        .unwrap();
    let goal = fx
        .coding
        .engine
        .open_goal(&token, OBJECTIVE_A)
        .await
        .unwrap();
    SessionRepository::new(&fx.db)
        .set_pending_goal_if_absent(&fx.session, OBJECTIVE_A, leveler_core::now())
        .await
        .unwrap();
    fx.coding.engine.release_ownership(&token).await.unwrap();

    let report = fx
        .coding
        .resume_with_instruction(
            &fx.session,
            &spec(&fx, OBJECTIVE_A),
            Message::user_input("继续"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("the adopted goal resumes");
    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Blocked,
        "a checkpoint-root failure would turn this into Interrupted: {report:?}"
    );
    let goals = goals(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].id, goal);
    assert_eq!(goals[0].objective, OBJECTIVE_A);
    assert_eq!(
        goals[0].state,
        GoalState::Running,
        "a blocked goal still owes its work; only the window ended"
    );
    let turns = turns(&fx).await;
    assert_eq!(turns.len(), 2);
    let lineage = leveler_engine::decode_turn_continuation(turns[1].payload.as_deref().unwrap())
        .expect("v2 payload");
    assert_eq!(lineage.goal_id.as_ref(), Some(&goal));
    assert_eq!(
        lineage.root_turn_id.as_ref().map(|id| id.as_str()),
        Some(turns[0].id.as_str()),
        "the adopted turn still names the chat window it continued"
    );
}
