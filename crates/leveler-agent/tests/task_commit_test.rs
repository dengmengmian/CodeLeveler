//! Chat-delegated task execution.
//!
//! A Chat turn stays a Chat turn for the session, but an explicitly delegated
//! task can commit itself to the Goal lifecycle mid-turn. The commit is a
//! structured declaration from the model (`commit_task`), not a keyword guess:
//! once bound, the running window uses the same closeout, verification,
//! budget and resume machinery as `/goal`.
//!
//! These tests drive the real engine, real SQLite, real request rows and the
//! real tool surface. The only fake is the model provider.

use async_trait::async_trait;
use leveler_agent::coding::{CodingRuntime, ExecutorFactory, TaskSpec};
use leveler_agent::{AutoClarify, StepLimits};
use leveler_core::{RequestId, SessionId};
use leveler_engine::{EngineBoot, ExecutionKind, TaskEngine};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::{Database, ModelRequestRepository, SessionRecord, SessionRepository};
use leveler_tools::ToolContext;
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use tokio_util::sync::CancellationToken;

const USER_REQUEST: &str = "接入全部运营后台能力到 MCP，读写都要，严格认证，完成后通过测试";

#[derive(Debug, Clone)]
enum Scripted {
    Tool(&'static str, serde_json::Value),
    Text(&'static str),
}

impl Scripted {
    fn commit() -> Self {
        Self::Tool(
            "commit_task",
            serde_json::json!({
                "objective": USER_REQUEST,
                "acceptance": ["go test ./... 通过", "MCP 读写工具与页面能力一致"]
            }),
        )
    }

    fn run_tests() -> Self {
        Self::Tool("run_command", test_runner_call())
    }

    fn complete() -> Self {
        Self::Tool(
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "MCP 接入完成",
                "verification": {
                    "status": "passed",
                    "detail": test_runner_label()
                }
            }),
        )
    }
}

fn test_runner_label() -> &'static str {
    if cfg!(windows) {
        "cmd /c run-tests.cmd"
    } else {
        "sh run-tests.sh"
    }
}

fn test_runner_call() -> serde_json::Value {
    if cfg!(windows) {
        serde_json::json!({
            "program": "cmd", "args": ["/c", "run-tests.cmd"],
            "reason": "run the test suite the user required"
        })
    } else {
        serde_json::json!({
            "program": "sh", "args": ["run-tests.sh"],
            "reason": "run the test suite the user required"
        })
    }
}

fn write_test_runner(dir: &std::path::Path) {
    if cfg!(windows) {
        std::fs::write(dir.join("run-tests.cmd"), "@echo off\r\nexit /b 0\r\n").unwrap();
    } else {
        let runner = dir.join("run-tests.sh");
        std::fs::write(&runner, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

struct Runtime {
    script: Mutex<VecDeque<Scripted>>,
    calls: AtomicU32,
    requests: Mutex<Vec<ModelRequest>>,
    /// Cancel the turn's token after returning the response for this 1-based
    /// call. Zero never cancels.
    cancel_after: AtomicU32,
    cancel: CancellationToken,
}

#[async_trait]
impl ModelRuntime for Runtime {
    async fn profile(&self, _: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id":"m","provider":"mock","model_id":"m","protocol":"openai_chat",
            "capabilities":{"streaming":true,"tool_calling":true,"parallel_tool_calls":false,"structured_output":false,"reasoning":false,"vision":false},
            "limits":{"context_window":64000,"reliable_context":32000,"max_output_tokens":2048,"max_tool_schema_bytes":65536,"max_parallel_tool_calls":1},
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
        // Auxiliary calls (summaries) carry no tools; they are not the turn.
        if request.tools.is_empty() {
            return Ok(leveler_model::stream_from_response(ModelResponse {
                request_id: RequestId::generate(),
                message: Message::text(Role::Assistant, "summary"),
                finish_reason: FinishReason::Stop,
                usage: usage(),
            }));
        }
        self.requests.lock().unwrap().push(request);
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let next = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Scripted::Text("script exhausted"));
        let (message, finish) = match next {
            Scripted::Tool(name, arguments) => (
                Message::from_parts(
                    Role::Assistant,
                    vec![ContentPart::ToolCall {
                        call: ToolCall {
                            id: leveler_core::ToolCallId::new(format!("call-{call}")),
                            name: name.to_string(),
                            arguments,
                        },
                    }],
                    None,
                ),
                FinishReason::ToolCalls,
            ),
            Scripted::Text(text) => (Message::text(Role::Assistant, text), FinishReason::Stop),
        };
        if call == self.cancel_after.load(Ordering::SeqCst) {
            self.cancel.cancel();
        }
        Ok(leveler_model::stream_from_response(ModelResponse {
            request_id: RequestId::generate(),
            message,
            finish_reason: finish,
            usage: usage(),
        }))
    }
}

const CALL_TOKENS: u64 = 490_000;

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: CALL_TOKENS,
        output_tokens: 10,
        ..Default::default()
    }
}

struct Fixture {
    runtime: Arc<Runtime>,
    coding: CodingRuntime,
    db: Database,
    session: SessionId,
    dir: tempfile::TempDir,
    cancel: CancellationToken,
    task_cancel: Arc<AtomicBool>,
}

async fn fixture(script: Vec<Scripted>, read_only: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    write_test_runner(dir.path());
    let db = Database::connect_in_memory().await.unwrap();
    let record = SessionRecord::new(
        dir.path().to_string_lossy().as_ref(),
        "chat",
        "mock/m",
        leveler_core::now(),
    )
    .with_axes("chat", "single");
    SessionRepository::new(&db).create(&record).await.unwrap();
    let session = SessionId::new(record.id);
    let cancel = CancellationToken::new();
    let task_cancel = Arc::new(AtomicBool::new(false));
    let runtime = Arc::new(Runtime {
        script: Mutex::new(script.into()),
        calls: AtomicU32::new(0),
        requests: Mutex::new(Vec::new()),
        cancel_after: AtomicU32::new(0),
        cancel: cancel.clone(),
    });
    let context = ToolContext::with_environment(
        Workspace::new(dir.path()).unwrap(),
        PermissionProfile::Assisted,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            dir.path().into(),
            std::env::temp_dir(),
        )),
    )
    .with_read_only(read_only);
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    let coding = CodingRuntime {
        engine: TaskEngine {
            stores: leveler_storage::EngineStores::from_database(&db),
            runtime_id: leveler_core::RuntimeId::new("task-commit"),
            boot: EngineBoot {
                id: leveler_core::BootId::generate(),
                liveness: Arc::new(leveler_test_support::TestBoots::new()),
            },
        },
        factory: ExecutorFactory {
            resource_grants: Arc::new(leveler_storage::MemoryResourceGrantStore::default()),
            runtime: runtime.clone(),
            registry: Arc::new(registry),
            tool_context: context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: None,
            memory_expose: false,
            memory_service: None,
            memory_lanes: leveler_memory::MemoryLanes::ALL,
            background_tasks: Arc::new(leveler_execution::BackgroundTaskRegistry::new()),
            permission_rules: Default::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(".".into()),
            steering: None,
            allow_delegation: false,
            allow_host_input: true,
            capabilities: None,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Off,
            develop_model: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        task_cancel: task_cancel.clone(),
    };
    Fixture {
        runtime,
        coding,
        db,
        session,
        dir,
        cancel,
        task_cancel,
    }
}

fn spec(fx: &Fixture) -> TaskSpec {
    TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: USER_REQUEST.into(),
            kind: ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::UntilTerminal,
            limits: StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(fx.dir.path().into()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    }
}

async fn chat(fx: &Fixture) -> leveler_agent::coding::TaskReport {
    chat_with(fx, CancellationToken::new()).await
}

async fn chat_with(
    fx: &Fixture,
    cancellation: CancellationToken,
) -> leveler_agent::coding::TaskReport {
    fx.coding
        .chat(
            &fx.session,
            &spec(fx),
            vec![ContentPart::Text {
                text: USER_REQUEST.into(),
            }],
            &mut |_| {},
            cancellation,
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

fn tool_names(request: &ModelRequest) -> Vec<&str> {
    request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect()
}

#[tokio::test]
async fn a_chat_delegation_commits_the_task_and_continues_past_a_stage_summary() {
    let fx = fixture(
        vec![
            Scripted::commit(),
            Scripted::Text("这一批完成了，剩下的下一轮继续。"),
            Scripted::run_tests(),
            Scripted::complete(),
        ],
        false,
    )
    .await;

    let report = chat(&fx).await;

    assert_eq!(
        fx.runtime.calls.load(Ordering::SeqCst),
        4,
        "the stage summary must buy another round inside the same turn"
    );
    {
        let requests = fx.runtime.requests.lock().unwrap();
        let first = tool_names(&requests[0]);
        assert!(
            first.contains(&"commit_task"),
            "a Chat turn must be able to declare a task: {first:?}"
        );
        assert!(
            !first.contains(&"update_goal"),
            "completion authority belongs to a task that exists: {first:?}"
        );
        let after = tool_names(&requests[1]);
        assert!(
            after.contains(&"update_goal"),
            "the committed turn must expose the goal contract: {after:?}"
        );
        assert!(
            !after.contains(&"commit_task"),
            "a committed turn must not re-declare the task: {after:?}"
        );
    }

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Completed,
        "{report:?}"
    );
    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "exactly one durable task");
    assert_eq!(goals[0].objective, USER_REQUEST);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);

    // The session stays a Chat session; only the turn acquired a task.
    let collaboration = SessionRepository::new(&fx.db)
        .get(&fx.session)
        .await
        .unwrap()
        .unwrap()
        .collaboration;
    assert_eq!(collaboration, "chat");

    // The whole window — including the pre-commit call — is billed to the task.
    let rows = ModelRequestRepository::new(&fx.db)
        .load_for_session(&fx.session)
        .await
        .unwrap();
    assert!(
        rows.iter()
            .all(|row| row.budget_scope.as_deref() == Some(goals[0].id.as_str())),
        "every model call in the committed window belongs to the task: {:?}",
        rows.iter()
            .map(|row| row.budget_scope.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_declared_objective_cannot_narrow_the_users_request() {
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": "只接入只读接口",
                    "acceptance": ["只读工具可调用", "sh run-tests.sh 通过"]
                }),
            ),
            Scripted::run_tests(),
            Scripted::complete(),
        ],
        false,
    )
    .await;

    let report = chat(&fx).await;

    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(
        goals[0].objective, USER_REQUEST,
        "the durable objective is the user's request; the model's restatement cannot shrink it"
    );
}

#[tokio::test]
async fn an_ordinary_chat_answer_does_not_create_a_task() {
    let fx = fixture(
        vec![Scripted::Text("MCP 认证通常使用 OAuth 或签名 token。")],
        false,
    )
    .await;

    let report = chat(&fx).await;

    assert_eq!(fx.runtime.calls.load(Ordering::SeqCst), 1);
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Answered);
    assert!(goal_records(&fx).await.is_empty(), "no task may be created");
}

#[tokio::test]
async fn a_committed_chat_task_resumes_as_a_goal_from_a_chat_session() {
    let fx = fixture(
        vec![
            Scripted::commit(),
            Scripted::Text("这一批完成了，剩下的下一轮继续。"),
            Scripted::run_tests(),
            Scripted::complete(),
        ],
        false,
    )
    .await;
    fx.runtime.cancel_after.store(2, Ordering::SeqCst);

    let interrupted = fx
        .coding
        .chat(
            &fx.session,
            &spec(&fx),
            vec![ContentPart::Text {
                text: USER_REQUEST.into(),
            }],
            &mut |_| {},
            fx.cancel.clone(),
        )
        .await;
    assert!(interrupted.is_err(), "the cancelled window must not finish");

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    let goal_id = goals[0].id.clone();
    assert_eq!(goals[0].state, leveler_storage::GoalState::Running);
    assert_eq!(goals[0].objective, USER_REQUEST);

    let resumed = fx
        .coding
        .resume_with_instruction(
            &fx.session,
            &spec(&fx),
            Message::user_input("继续"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        resumed.stop_reason,
        leveler_agent::StopReason::Completed,
        "{resumed:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "resume must not open a second task");
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(goals[0].objective, USER_REQUEST);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);

    // The resumed request runs the Goal contract from its first round, even
    // though the session's durable axis is still `chat`.
    let requests = fx.runtime.requests.lock().unwrap();
    let resumed_tools = tool_names(&requests[2]);
    assert!(
        resumed_tools.contains(&"update_goal"),
        "a resumed committed task must expose the goal contract: {resumed_tools:?}"
    );
    assert!(
        !resumed_tools.contains(&"commit_task"),
        "the task already exists; it must not be declared again: {resumed_tools:?}"
    );
}

#[tokio::test]
async fn a_committed_task_stops_without_progress_instead_of_looping() {
    let fx = fixture(
        vec![
            Scripted::commit(),
            Scripted::Text("先到这里。"),
            Scripted::Text("剩下的下次继续。"),
            Scripted::Text("建议后续再处理。"),
            Scripted::Text("仍未完成。"),
        ],
        false,
    )
    .await;

    let report = chat(&fx).await;

    assert!(
        matches!(
            report.stop_reason,
            leveler_agent::StopReason::Stalled | leveler_agent::StopReason::Incomplete
        ),
        "quiet stage summaries must be bounded, got {report:?}"
    );
    assert_eq!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Interrupted,
        "a stalled task is not a success"
    );
    assert!(
        fx.runtime.calls.load(Ordering::SeqCst) <= 4,
        "no-progress continuation is bounded (commit + initial + two nudges), got {}",
        fx.runtime.calls.load(Ordering::SeqCst)
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(
        goals[0].state,
        leveler_storage::GoalState::Running,
        "the unfinished task stays owed and resumable"
    );
}

#[tokio::test]
async fn a_user_cancel_settles_the_committed_task() {
    let fx = fixture(
        vec![
            Scripted::commit(),
            Scripted::Text("这一批完成了，剩下的下一轮继续。"),
        ],
        false,
    )
    .await;
    fx.task_cancel.store(true, Ordering::SeqCst);
    fx.runtime.cancel_after.store(2, Ordering::SeqCst);

    let result = fx
        .coding
        .chat(
            &fx.session,
            &spec(&fx),
            vec![ContentPart::Text {
                text: USER_REQUEST.into(),
            }],
            &mut |_| {},
            fx.cancel.clone(),
        )
        .await;
    assert!(result.is_err(), "a cancelled task must not report success");

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(
        goals[0].state,
        leveler_storage::GoalState::Settled,
        "cancelling the logical task settles the goal, so `继续` cannot reopen it"
    );
}

#[tokio::test]
async fn a_committed_task_respects_its_budget_and_stays_owed() {
    let fx = fixture(
        vec![
            Scripted::commit(),
            Scripted::Text("still working"),
            Scripted::Text("still working"),
        ],
        false,
    )
    .await;
    let mut spec = spec(&fx);
    spec.runtime.limits = StepLimits {
        // One round costs more than the cap; the first (commit) round is
        // admitted, the next one is refused.
        max_model_tokens: Some(400_000),
        ..Default::default()
    };

    let report = fx
        .coding
        .chat(
            &fx.session,
            &spec,
            vec![ContentPart::Text {
                text: USER_REQUEST.into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::BudgetExhausted,
        "{report:?}"
    );
    assert_eq!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::BudgetLimited
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(
        goals[0].state,
        leveler_storage::GoalState::Running,
        "a budget pause is not a completion"
    );
    assert!(
        fx.runtime.calls.load(Ordering::SeqCst) <= 2,
        "the cap must stop further model calls, got {}",
        fx.runtime.calls.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn a_read_only_turn_cannot_declare_an_execution_task() {
    let fx = fixture(vec![Scripted::Text("只分析，不修改。")], true).await;

    let report = chat(&fx).await;

    {
        let requests = fx.runtime.requests.lock().unwrap();
        let tools = tool_names(&requests[0]);
        assert!(
            !tools.contains(&"commit_task"),
            "a planning/read-only surface must not offer execution: {tools:?}"
        );
    }
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Answered);
    assert!(goal_records(&fx).await.is_empty());
}

// ---------------------------------------------------------------------------
// DF-22: acceptance-scope integrity.
//
// The user's request names the deliverables; the model's acceptance is a
// proposed plan, never a replacement for that scope. These tests drive the
// real engine and tool surface; only the provider is scripted.
// ---------------------------------------------------------------------------

fn staged_objective() -> &'static str {
    if cfg!(windows) {
        "完成全部三个阶段：创建 stage-1.txt、stage-2.txt、stage-3.txt，\
         最后运行 cmd /c verify.cmd，全部做完再结束。"
    } else {
        "完成全部三个阶段：创建 stage-1.txt、stage-2.txt、stage-3.txt，\
         最后运行 sh ./verify.sh，全部做完再结束。"
    }
}

fn verifier_path() -> &'static str {
    if cfg!(windows) {
        "verify.cmd"
    } else {
        "./verify.sh"
    }
}

fn verifier_label() -> &'static str {
    if cfg!(windows) {
        "cmd /c verify.cmd"
    } else {
        "sh ./verify.sh"
    }
}

fn verifier_call() -> serde_json::Value {
    if cfg!(windows) {
        serde_json::json!({"program": "cmd", "args": ["/c", "verify.cmd"], "reason": "run the acceptance check"})
    } else {
        serde_json::json!({"program": "sh", "args": ["./verify.sh"], "reason": "run the acceptance check"})
    }
}

fn checker_label() -> &'static str {
    if cfg!(windows) {
        "cmd /c check.cmd"
    } else {
        "sh check.sh"
    }
}

fn checker_call() -> serde_json::Value {
    if cfg!(windows) {
        serde_json::json!({"program": "cmd", "args": ["/c", "check.cmd"], "reason": "run the build check"})
    } else {
        serde_json::json!({"program": "sh", "args": ["check.sh"], "reason": "run the build check"})
    }
}

fn write_verifier(dir: &std::path::Path) {
    if cfg!(windows) {
        std::fs::write(dir.join("verify.cmd"), "@echo off\r\nexit /b 0\r\n").unwrap();
        std::fs::write(dir.join("check.cmd"), "@echo off\r\nexit /b 0\r\n").unwrap();
    } else {
        let verifier = dir.join("verify.sh");
        std::fs::write(&verifier, "#!/bin/sh\nexit 0\n").unwrap();
        let checker = dir.join("check.sh");
        std::fs::write(&checker, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&verifier, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::set_permissions(&checker, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

fn spec_for(fx: &Fixture, goal: &str) -> TaskSpec {
    let mut spec = spec(fx);
    spec.runtime.goal = goal.to_string();
    spec
}

fn commit_with(acceptance: Vec<&str>, objective: &str) -> Scripted {
    Scripted::Tool(
        "commit_task",
        serde_json::json!({"objective": objective, "acceptance": acceptance}),
    )
}

fn write_stage() -> Scripted {
    Scripted::Tool(
        "write_file",
        serde_json::json!({
            "path": "stage-1.txt",
            "content": "stage 1\n"
        }),
    )
}

#[tokio::test]
async fn a_completion_cannot_omit_a_user_named_deliverable() {
    let fx = fixture(
        vec![
            commit_with(
                vec![
                    "stage-1.txt 存在",
                    "stage-2.txt 存在",
                    "stage-3.txt 存在",
                    "sh ./verify.sh 通过",
                ],
                staged_objective(),
            ),
            write_stage(),
            // The model tries to re-declare a narrower plan mid-task. A bound
            // goal has no acceptance update path; the stored plan stays the
            // full one.
            commit_with(vec!["stage-1.txt 存在"], staged_objective()),
            Scripted::Tool("run_command", verifier_call()),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "all three stages are complete",
                    "verification": {"status": "passed", "detail": verifier_label()},
                    "requirements": [
                        {"requirement": "stage-1.txt", "status": "done",
                         "evidence": "created stage-1.txt"},
                        {"requirement": "stage-2.txt", "status": "done",
                         "evidence": "created stage-2.txt"},
                        {"requirement": "stage-3.txt", "status": "done",
                         "evidence": "created stage-3.txt"},
                        {"requirement": verifier_path(), "status": "done",
                         "evidence": "ran the verifier"}
                    ]
                }),
            ),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "blocked",
                    "summary": "stage-2 and stage-3 were never created"
                }),
            ),
        ],
        false,
    )
    .await;
    write_verifier(fx.dir.path());
    let mut events = Vec::new();

    let report = fx
        .coding
        .chat(
            &fx.session,
            &spec_for(&fx, staged_objective()),
            vec![ContentPart::Text {
                text: staged_objective().into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_ne!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Completed,
        "a goal must not complete while user-named deliverables are missing: {report:?}"
    );
    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Blocked);
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == "requirements_refused" && detail.contains("stage-2.txt")
        )),
        "the refusal must name the missing deliverable: {events:?}"
    );
    assert!(!fx.dir.path().join("stage-2.txt").exists());
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
    assert_eq!(
        goals[0].acceptance.len(),
        4,
        "the model's later narrow declaration cannot replace the stored plan: {goals:?}"
    );
}

#[tokio::test]
async fn a_narrow_acceptance_cannot_commit_the_users_full_request() {
    let fx = fixture(
        vec![
            commit_with(vec!["stage-1.txt 存在"], staged_objective()),
            Scripted::Text("I cannot narrow the user's scope."),
        ],
        false,
    )
    .await;
    write_verifier(fx.dir.path());
    let mut events = Vec::new();

    let report = fx
        .coding
        .chat(
            &fx.session,
            &spec_for(&fx, staged_objective()),
            vec![ContentPart::Text {
                text: staged_objective().into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Answered,
        "a refused commit leaves the turn a Chat answer: {report:?}"
    );
    assert!(
        goal_records(&fx).await.is_empty(),
        "a task whose acceptance drops user deliverables must not be bound"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == "acceptance_narrowed"
                    && detail.contains("stage-2.txt")
                    && detail.contains("stage-3.txt")
        )),
        "the refusal must name every dropped deliverable: {events:?}"
    );
}

#[tokio::test]
async fn a_user_can_explicitly_narrow_the_scope_on_continuation() {
    let fx = fixture(
        vec![
            commit_with(
                vec![
                    "stage-1.txt 存在",
                    "stage-2.txt 存在",
                    "stage-3.txt 存在",
                    "sh ./verify.sh 通过",
                ],
                staged_objective(),
            ),
            Scripted::Text("stage-1 is done; continuing."),
            write_stage(),
            Scripted::Tool("run_command", verifier_call()),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "stage 1 delivered; stages 2-3 were waived by the user",
                    "verification": {"status": "passed", "detail": verifier_label()},
                    "requirements": [
                        {"requirement": "stage-1.txt", "status": "done",
                         "evidence": "created stage-1.txt"},
                        {"requirement": verifier_path(), "status": "done",
                         "evidence": "ran the verifier"}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;
    write_verifier(fx.dir.path());
    fx.runtime.cancel_after.store(2, Ordering::SeqCst);

    let interrupted = fx
        .coding
        .chat(
            &fx.session,
            &spec_for(&fx, staged_objective()),
            vec![ContentPart::Text {
                text: staged_objective().into(),
            }],
            &mut |_| {},
            fx.cancel.clone(),
        )
        .await;
    assert!(interrupted.is_err(), "the cancelled window must not finish");
    let goal_id = goal_records(&fx).await[0].id.clone();

    let resumed = fx
        .coding
        .resume_with_instruction(
            &fx.session,
            &spec_for(&fx, staged_objective()),
            Message::user_input("stage-2.txt、stage-3.txt 暂时不做，这次只交付 stage-1.txt。"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        resumed.outcome,
        leveler_lifecycle::TaskOutcome::Completed,
        "the user's own waiver is authority: {resumed:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
    assert!(
        goals[0]
            .amendments
            .iter()
            .any(|amendment| amendment.contains("暂时不做")),
        "the waiver must be durable with its source: {goals:?}"
    );
}

#[tokio::test]
async fn a_test_requirement_cannot_be_satisfied_by_a_build_check() {
    let objective = "实现全部接口，并通过完整测试。";
    let fx = fixture(
        vec![
            commit_with(vec!["sh run-tests.sh 通过"], objective),
            Scripted::Tool("run_command", checker_call()),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "the interfaces compile",
                    "verification": {"status": "passed", "detail": checker_label()}
                }),
            ),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "blocked",
                    "summary": "the required test suite has not run"
                }),
            ),
        ],
        false,
    )
    .await;
    write_verifier(fx.dir.path());
    let mut events = Vec::new();

    let report = fx
        .coding
        .chat(
            &fx.session,
            &spec_for(&fx, objective),
            vec![ContentPart::Text {
                text: objective.into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Blocked,
        "a build check is not the tests the user asked for: {report:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == "requirements_refused" && detail.contains("test")
        )),
        "the refusal must name the test requirement: {events:?}"
    );
}
