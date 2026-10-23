//! Budget attribution across a Chat turn upgraded to a Goal mid-flight.
//!
//! `/goal <task>` can arrive while a Chat turn already runs. That turn spent
//! model tokens before the declaration and may spend more after it, and a
//! later resume continues the GOAL. The goal's cap must therefore own the
//! whole upgraded window: a resume that reconstructs spend only from rows
//! written after the bind would hand the goal a fresh budget.
//!
//! These tests drive the real engine, real SQLite and the real request rows.
//! The only fake is the model provider.

use async_trait::async_trait;
use leveler_agent::coding::{CodingRuntime, ExecutorFactory, TaskSpec};
use leveler_agent::{AutoClarify, SteeringSource, StepLimits};
use leveler_core::{RequestId, SessionId, ToolCallId};
use leveler_engine::{EngineBoot, ExecutionKind, TaskEngine};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::{
    Database, ModelRequestRepository, SessionRecord, SessionRepository, TurnRepository,
};
use leveler_tools::ToolContext;
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use tokio_util::sync::CancellationToken;

const CALL_TOKENS: u64 = 490_000;
const OBJECTIVE: &str = "修复升级后的预算归属";

#[derive(Debug, Clone)]
enum Scripted {
    Tool(&'static str, serde_json::Value),
    Text,
}

impl Scripted {
    fn plan() -> Self {
        Self::Tool(
            "update_plan",
            serde_json::json!({"plan": [
                {"step": "keep working", "status": "in_progress"}
            ]}),
        )
    }

    fn goal(status: &str) -> Self {
        Self::Tool(
            "update_goal",
            serde_json::json!({"status": status, "summary": "scripted end"}),
        )
    }
}

struct Runtime {
    script: Mutex<VecDeque<Scripted>>,
    calls: AtomicU32,
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
        unreachable!("tracked chat calls stream")
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
        let next = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Scripted::goal("blocked"));
        let (message, finish) = match next {
            Scripted::Tool(name, arguments) => (
                Message::from_parts(
                    Role::Assistant,
                    vec![ContentPart::ToolCall {
                        call: ToolCall {
                            id: ToolCallId::new(format!(
                                "call-{}",
                                self.calls.load(Ordering::SeqCst)
                            )),
                            name: name.to_string(),
                            arguments,
                        },
                    }],
                    None,
                ),
                FinishReason::ToolCalls,
            ),
            Scripted::Text => (
                Message::text(Role::Assistant, "still working"),
                FinishReason::Stop,
            ),
        };
        Ok(leveler_model::stream_from_response(ModelResponse {
            request_id: RequestId::generate(),
            message,
            finish_reason: finish,
            usage: usage(),
        }))
    }
}

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: CALL_TOKENS,
        output_tokens: 0,
        ..Default::default()
    }
}

/// A `/goal` declaration that only becomes visible at a chosen round.
///
/// `schedule[i]` is what `take_goal_declaration` returns on its i-th call.
/// `None` keeps the turn a Chat; `Some(text)` is the accepted declaration and
/// flips `goal_requested` from then on.
struct ScheduledGoal {
    schedule: Mutex<VecDeque<Option<String>>>,
    requested: AtomicBool,
    seen: AtomicU32,
}

impl ScheduledGoal {
    fn new(schedule: Vec<Option<&str>>) -> Self {
        Self {
            schedule: Mutex::new(
                schedule
                    .into_iter()
                    .map(|text| text.map(str::to_string))
                    .collect(),
            ),
            requested: AtomicBool::new(false),
            seen: AtomicU32::new(0),
        }
    }
}

impl SteeringSource for ScheduledGoal {
    fn take_pending(&self) -> Vec<String> {
        Vec::new()
    }

    fn goal_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    fn take_goal_declaration(&self) -> Option<String> {
        self.seen.fetch_add(1, Ordering::SeqCst);
        // The first round of a Chat has no declaration yet; later rounds do.
        // `schedule` is consumed positionally so repeated rounds stay stable.
        let mut schedule = self.schedule.lock().unwrap();
        let next = schedule.pop_front().flatten();
        if next.is_some() {
            self.requested.store(true, Ordering::SeqCst);
        }
        next
    }
}

struct Fixture {
    runtime: Arc<Runtime>,
    coding: CodingRuntime,
    db: Database,
    session: SessionId,
    dir: tempfile::TempDir,
}

async fn fixture(schedule: Vec<Option<&str>>, script: Vec<Scripted>, declare: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect_in_memory().await.unwrap();
    let record = SessionRecord::new(
        dir.path().to_string_lossy().as_ref(),
        "chat",
        "mock/m",
        leveler_core::now(),
    );
    SessionRepository::new(&db).create(&record).await.unwrap();
    let session = SessionId::new(record.id);
    // The app writes the axis and the declaration in one transaction when the
    // steer arrives. Reproduce that durable pair when the case starts from a
    // declaration; a declaration arriving MID-turn is the binder's fallback.
    if declare {
        SessionRepository::new(&db)
            .set_goal_declaration(&session, "goal", OBJECTIVE, leveler_core::now())
            .await
            .unwrap();
    }
    let runtime = Arc::new(Runtime {
        script: Mutex::new(script.into()),
        calls: AtomicU32::new(0),
    });
    let context = ToolContext::with_environment(
        Workspace::new(dir.path()).unwrap(),
        PermissionProfile::Assisted,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            dir.path().into(),
            std::env::temp_dir(),
        )),
    );
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    let coding = CodingRuntime {
        engine: TaskEngine {
            stores: leveler_storage::EngineStores::from_database(&db),
            runtime_id: leveler_core::RuntimeId::new("chat-goal-budget"),
            boot: EngineBoot {
                id: leveler_core::BootId::generate(),
                liveness: Arc::new(leveler_test_support::TestBoots::new()),
            },
        },
        factory: ExecutorFactory {
            resource_grants: std::sync::Arc::new(
                leveler_storage::MemoryResourceGrantStore::default(),
            ),
            runtime: runtime.clone(),
            registry: Arc::new(registry),
            tool_context: context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: None,
            memory_catalog: String::new(),
            memory_expose: true,
            memory_root: None,
            memory_service: None,
            memory_lanes: leveler_memory::MemoryLanes::ALL,
            background_tasks: Arc::new(leveler_execution::BackgroundTaskRegistry::new()),
            permission_rules: Default::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(".".into()),
            steering: Some(Arc::new(ScheduledGoal::new(schedule))),
            allow_delegation: true,
            allow_host_input: true,
            capabilities: None,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Off,
            develop_model: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        task_cancel: Arc::new(AtomicBool::new(false)),
    };
    Fixture {
        runtime,
        coding,
        db,
        session,
        dir,
    }
}

fn spec(fx: &Fixture, max_model_tokens: Option<u64>) -> TaskSpec {
    TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "chat".into(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: StepLimits {
                max_model_tokens,
                max_cost_usd_micros: None,
                max_duration: None,
                ..Default::default()
            },
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(fx.dir.path().into()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    }
}

async fn chat(fx: &Fixture, limits: StepLimits) -> leveler_agent::coding::TaskReport {
    let mut spec = spec(fx, limits.max_model_tokens);
    spec.runtime.limits = limits;
    fx.coding
        .chat(
            &fx.session,
            &spec,
            vec![ContentPart::Text {
                text: "start chat work".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

async fn goal_records(fx: &Fixture) -> Vec<leveler_storage::GoalRecord> {
    let stores = leveler_storage::EngineStores::from_database(&fx.db);
    let task = stores
        .tasks
        .task_for_session(&fx.session)
        .await
        .unwrap()
        .unwrap();
    stores.goals.for_task(&task).await.unwrap()
}

#[tokio::test]
async fn pre_upgrade_spend_counts_against_the_resumed_goal_budget() {
    // Round 1 is Chat work (35 tokens). The declaration lands before round 2,
    // which is the first Goal-contract round; it ends `blocked`, which leaves
    // the goal owed so a continuation re-enters it.
    let fx = fixture(
        vec![None, Some(OBJECTIVE)],
        vec![Scripted::plan(), Scripted::goal("blocked")],
        true,
    )
    .await;
    let first = chat(&fx, StepLimits::default()).await;
    assert_eq!(
        first.stop_reason,
        leveler_agent::StopReason::Blocked,
        "the upgraded window ends on the model's declaration: {first:?}"
    );
    assert_eq!(fx.runtime.calls.load(Ordering::SeqCst), 2);

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "the upgrade opened exactly one goal");
    let goal = goals[0].clone();
    assert_eq!(goal.objective, OBJECTIVE);

    // The upgraded window's whole spend belongs to the goal it now serves.
    let rows = ModelRequestRepository::new(&fx.db)
        .load_for_session(&fx.session)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row.budget_scope.as_deref() == Some(goal.id.as_str())),
        "both pre- and post-bind calls must be attributed to the goal: {:?}",
        rows.iter()
            .map(|row| row.budget_scope.clone())
            .collect::<Vec<_>>()
    );
    let spent: u64 = rows
        .iter()
        .map(|row| row.input_tokens + row.output_tokens)
        .sum();
    assert_eq!(spent, CALL_TOKENS * 2);

    // The goal's cap (50) is already spent by the upgraded window. Continuing
    // it must admit no further call: the 70 tokens it cost are its own.
    let resumed = fx
        .coding
        .resume_with_instruction(
            &fx.session,
            &spec(&fx, Some(100_000)),
            Message::user_input("继续"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("a budget stop is a normal report");
    assert_eq!(
        resumed.stop_reason,
        leveler_agent::StopReason::BudgetExhausted,
        "the goal budget must count the upgraded window: {resumed:?}"
    );
    assert_eq!(
        fx.runtime.calls.load(Ordering::SeqCst),
        2,
        "no model call may be admitted past the goal's exhausted cap"
    );

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "the continuation is the same goal");
    assert_eq!(goals[0].id, goal.id);
    assert_eq!(goals[0].objective, OBJECTIVE);
    assert_eq!(
        goals[0].state,
        leveler_storage::GoalState::Running,
        "budget exhaustion pauses the goal instead of completing it"
    );

    // The budget-stopped resume wrote a Goal turn whose lineage root is the
    // Chat turn that was upgraded. A further continuation must accept that
    // root as the same goal's lineage — the upgrade happened in that window.
    let again = fx
        .coding
        .resume_with_instruction(
            &fx.session,
            &spec(&fx, None),
            Message::user_input("继续"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("a repeated resume reads the upgraded lineage");
    assert_eq!(again.stop_reason, leveler_agent::StopReason::Blocked);
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].id, goal.id);
    assert_eq!(goals[0].objective, OBJECTIVE);
}

#[tokio::test]
async fn a_second_declaration_never_double_counts_or_replaces_the_objective() {
    // Two declarations while the Chat turn runs: the first binds, the second
    // is an instruction inside the same goal. The window's spend is counted
    // once, under that one goal.
    let fx = fixture(
        vec![None, Some(OBJECTIVE), Some("另一个目标")],
        vec![
            Scripted::plan(),
            Scripted::plan(),
            Scripted::goal("blocked"),
        ],
        true,
    )
    .await;
    let report = chat(&fx, StepLimits::default()).await;
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Blocked);
    assert_eq!(fx.runtime.calls.load(Ordering::SeqCst), 3);

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "a second declaration must not open a goal");
    assert_eq!(
        goals[0].objective, OBJECTIVE,
        "the original objective is never replaced by a later declaration"
    );
    let rows = ModelRequestRepository::new(&fx.db)
        .load_for_session(&fx.session)
        .await
        .unwrap();
    let spent: u64 = rows
        .iter()
        .map(|row| row.input_tokens + row.output_tokens)
        .sum();
    assert_eq!(
        spent,
        CALL_TOKENS * 3,
        "three calls, counted once each: no duplication, no omission"
    );
    assert!(
        rows.iter()
            .all(|row| row.budget_scope.as_deref() == Some(goals[0].id.as_str()))
    );
}

#[tokio::test]
async fn crossing_the_cap_after_upgrade_never_reports_completion() {
    // Round 1 (Chat) + round 2 (Goal) spend 70; the cap is 50. The next
    // admission must stop the window, and the goal must stay owed.
    let fx = fixture(
        vec![None, Some(OBJECTIVE)],
        vec![Scripted::plan(), Scripted::Text],
        true,
    )
    .await;
    let report = chat(
        &fx,
        StepLimits {
            max_model_tokens: Some(100_000),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::BudgetExhausted,
        "a cap crossed by the upgraded window is a budget stop: {report:?}"
    );
    assert_eq!(
        report.outcome,
        leveler_engine::TaskOutcome::BudgetLimited,
        "budget exhaustion must not masquerade as completion"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(
        goals[0].state,
        leveler_storage::GoalState::Running,
        "the goal still owes work after a budget stop"
    );
    let turns = TurnRepository::new(&fx.db).list(&fx.session).await.unwrap();
    let terminal = leveler_storage::EventRepository::new(&fx.db)
        .load_last_by_type(&fx.session, "task_finished", None)
        .await
        .unwrap()
        .expect("the window wrote a terminal");
    assert!(
        terminal.payload.contains("budget_limited"),
        "the durable terminal is budget-limited: {}",
        terminal.payload
    );
    assert_eq!(turns.last().unwrap().status, "completed");
}

/// A Goal declared in a RESUMED Chat window starts its lineage at the binding
/// window, not at whatever older chat window that resume continued. Repeated
/// Goal resumes must stay on that lineage and keep the one objective.
#[tokio::test]
async fn a_goal_declared_in_a_resumed_chat_window_keeps_its_own_lineage() {
    let fx = fixture(
        vec![None, None, Some(OBJECTIVE)],
        vec![Scripted::Text, Scripted::plan(), Scripted::goal("blocked")],
        false,
    )
    .await;
    let first = chat(&fx, StepLimits::default()).await;
    assert_eq!(first.outcome, leveler_engine::TaskOutcome::Completed);

    // The axis is Goal when the declaration arrives inside the NEXT (Chat)
    // window; only then is the mid-turn upgrade the user's current choice.
    SessionRepository::new(&fx.db)
        .set_turn_collaboration(&fx.session, "goal", leveler_core::now())
        .await
        .unwrap();
    let second = fx
        .coding
        .resume_with_instruction(
            &fx.session,
            &spec(&fx, None),
            Message::user_input("继续"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("the resumed Chat window runs");
    assert_eq!(second.stop_reason, leveler_agent::StopReason::Blocked);

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "the declaration opened one goal");
    assert_eq!(goals[0].objective, OBJECTIVE);
    let turns = TurnRepository::new(&fx.db).list(&fx.session).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(
        turns[1].kind, "chat",
        "the upgrade happened inside the resumed Chat window"
    );

    // Two more continuations: the first reads the upgraded window as the
    // goal's root; the second reads the FIRST resumed goal turn's root.
    for _ in 0..2 {
        let report = fx
            .coding
            .resume_with_instruction(
                &fx.session,
                &spec(&fx, None),
                Message::user_input("继续"),
                &mut |_| {},
                CancellationToken::new(),
            )
            .await
            .expect("repeated Goal resumes must accept the upgraded lineage");
        assert_eq!(report.stop_reason, leveler_agent::StopReason::Blocked);
    }
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].objective, OBJECTIVE);
    let turns = TurnRepository::new(&fx.db).list(&fx.session).await.unwrap();
    assert_eq!(turns.len(), 4, "two chat windows plus two Goal resumes");
}
