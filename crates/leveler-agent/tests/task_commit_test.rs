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
                "acceptance": ["go test ./... 通过", "MCP 读写工具与页面能力一致"],
                // Existing suite: a re-commit of a task that already owes a
                // goal is an explicit continuation, not a silent adoption.
                "task_relation": "continue"
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

/// Append the answer a real model must write after it resolves its goal: the
/// runtime takes one delivery round when the resolving round carried no text.
fn with_delivery(mut script: Vec<Scripted>) -> Vec<Scripted> {
    let resolves_without_text = matches!(script.last(), Some(Scripted::Tool("update_goal", _)));
    if resolves_without_text {
        script.push(Scripted::Text("已完成，这是给用户的最终说明。"));
    }
    script
}

async fn fixture(script: Vec<Scripted>, read_only: bool) -> Fixture {
    let script = with_delivery(script);
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
            tool_discovery: None,
            resource_grants: Arc::new(leveler_storage::MemoryResourceGrantStore::default()),
            runtime: runtime.clone(),
            registry: Arc::new(registry),
            tool_context: context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: true,
            overrides: None,

            background_tasks: Arc::new(leveler_execution::BackgroundTaskRegistry::new()),
            permission_rules: Default::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(".".into()),
            steering: None,
            allow_delegation: false,
            allow_host_input: true,
            capabilities: None,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Off,
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

    // commit + stage summary + the tests the stage ran + the resolution + its
    // delivery round: the stage summary still buys its round inside this turn.
    assert_eq!(
        fx.runtime.calls.load(Ordering::SeqCst),
        5,
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

/// A repository names its own suite. DevOrder's is `make regression`: the
/// runtime recorded it, it passed, and the goal still refused to close because
/// the runner's name was not in the test vocabulary (session `1c3bf9aa`). The
/// suite the user demanded has to be the suite the runtime recognises.
#[cfg(unix)]
#[tokio::test]
async fn a_suite_named_in_the_repositorys_own_words_resolves_the_goal() {
    use std::os::unix::fs::PermissionsExt;

    let fx = fixture(
        vec![
            Scripted::commit(),
            Scripted::Tool(
                "run_command",
                serde_json::json!({
                    "program": "bin/make",
                    "args": ["regression"],
                    "reason": "run the suite the user required"
                }),
            ),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "本地全套已启动，13 组真实回归全过",
                    "verification": {"status": "passed", "detail": "bin/make regression"},
                    "requirements": [
                        {"requirement": "tests", "status": "done",
                         "evidence": "bin/make regression 退出码 0：13 组用例、191 条断言通过"}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;
    let bin = fx.dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let suite = bin.join("make");
    std::fs::write(&suite, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&suite, std::fs::Permissions::from_mode(0o755)).unwrap();

    let report = chat(&fx).await;

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Completed,
        "{report:?}"
    );
    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "exactly one durable task");
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
}

/// A verification command judges the workspace. Writing scratch evidence to
/// an absolute path outside it changes no file the command looked at, so it
/// must not make a fresh pass look stale.
#[tokio::test]
async fn a_write_outside_the_workspace_does_not_stale_the_verification_run() {
    let outside = tempfile::tempdir().unwrap();
    let evidence = outside.path().join("captured-evidence.txt");
    let fx = fixture(
        vec![
            Scripted::commit(),
            Scripted::run_tests(),
            Scripted::Tool(
                "write_file",
                serde_json::json!({
                    "path": evidence.to_string_lossy(),
                    "content": "observed evidence\n",
                }),
            ),
            Scripted::complete(),
            Scripted::Text("完成，外部证据已保存。"),
        ],
        false,
    )
    .await;

    let report = chat(&fx).await;

    assert!(
        evidence.is_file(),
        "the scratch write must really have happened outside the workspace"
    );
    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Completed,
        "a write outside the workspace must not invalidate the verification: {report:?}"
    );
    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
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

/// A request whose deliverables the runtime can actually enumerate.
const STAGED_REQUEST: &str = "完成文章发布、评论回复、内容权限校验三个功能，完成后通过测试";

async fn chat_with_request(
    fx: &Fixture,
    request: &'static str,
    cancellation: CancellationToken,
) -> leveler_agent::coding::TaskReport {
    let mut spec = spec(fx);
    spec.runtime.goal = request.into();
    fx.coding
        .chat(
            &fx.session,
            &spec,
            vec![ContentPart::Text {
                text: request.into(),
            }],
            &mut |_| {},
            cancellation,
        )
        .await
        .unwrap()
}

/// A refused declaration is about the CONTRACT, not the request. The harness
/// must not treat the refusal as the end of the task, and must not promote the
/// Chat turn into a goal on the model's behalf either: one bounded continuation
/// states the refusal and the user's work is still outstanding.
#[tokio::test]
async fn a_refused_declaration_buys_a_continuation_and_does_not_create_a_goal() {
    let fx = fixture(
        vec![
            // Covers one deliverable out of three: the refusal is deterministic.
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": STAGED_REQUEST,
                    "acceptance": ["文章发布可访问"]
                }),
            ),
            Scripted::Text("我先完成了文章发布这一部分。"),
            Scripted::run_tests(),
            Scripted::Text("另外两项也做完了，测试通过。"),
        ],
        false,
    )
    .await;

    let report = chat_with_request(&fx, STAGED_REQUEST, CancellationToken::new()).await;

    assert_eq!(
        fx.runtime.calls.load(Ordering::SeqCst),
        4,
        "the quiet round after a refusal buys exactly one continuation, and the          resumed work then closes"
    );
    {
        let requests = fx.runtime.requests.lock().unwrap();
        // The refusal itself is structured and actionable.
        let refusal = requests[1]
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result.content.clone()),
                _ => None,
            })
            .find(|content| content.contains("commit_task refused"))
            .expect("the model sees the refusal");
        assert!(refusal.contains("[deliverable]"), "{refusal}");
        assert!(
            refusal.contains("评论回复") && refusal.contains("内容权限校验"),
            "{refusal}"
        );
        assert!(refusal.contains("Fix:"), "{refusal}");
        // The continuation names the refusal as the reason, not the plan.
        let nudge = requests[2]
            .messages
            .iter()
            .map(|message| message.text_content())
            .find(|text| text.contains("commit_task declaration was refused"))
            .expect("the continuation states the refused declaration");
        assert!(nudge.contains("not about the user's request"), "{nudge}");
        assert!(nudge.contains("still outstanding"), "{nudge}");
        assert!(
            !nudge.contains("update_goal"),
            "a refusal does not silently turn a Chat turn into a goal: {nudge}"
        );
    }
    assert!(
        goal_records(&fx).await.is_empty(),
        "no goal was registered: the harness never declares a task on the model's behalf"
    );
    // The turn answered. That is the turn's fact, not a delivery verdict: the
    // terminal carries the refused declaration so the answer cannot be read as
    // acceptance of the user's request.
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Answered);
    assert!(
        report.completion_warnings.iter().any(|warning| warning
            .contains("no execution task was registered")
            && warning.contains("not with a verified delivery")),
        "the report says what the refusal means: {report:?}"
    );
}

/// The other honest recovery: the model fixes the contract and re-declares.
#[tokio::test]
async fn a_model_recovers_from_a_refusal_and_registers_the_full_scope() {
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": STAGED_REQUEST,
                    "acceptance": ["文章发布可访问"]
                }),
            ),
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": STAGED_REQUEST,
                    "acceptance": [
                        "文章发布可访问",
                        "评论回复可提交且可见",
                        "内容权限校验覆盖作者与管理员",
                        "sh run-tests.sh 通过"
                    ]
                }),
            ),
            Scripted::run_tests(),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "三项社区业务完成",
                    "verification": {"status": "passed", "detail": test_runner_label()},
                    "requirements": [
                        {"requirement": "文章发布", "status": "done", "evidence": "发布链路可访问"},
                        {"requirement": "评论回复", "status": "done", "evidence": "回复可见"},
                        {"requirement": "内容权限校验", "status": "done", "evidence": "作者/管理员/只读"},
                        {"requirement": "tests", "status": "done", "evidence": test_runner_label()}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;

    let report = chat_with_request(&fx, STAGED_REQUEST, CancellationToken::new()).await;

    let goals = goal_records(&fx).await;
    assert_eq!(
        goals.len(),
        1,
        "the corrected declaration registers one goal"
    );
    assert_eq!(goals[0].objective, STAGED_REQUEST);
    assert!(
        goals[0].state == leveler_storage::GoalState::Settled,
        "the corrected contract completes: {:?}",
        goals[0].state
    );
    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
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

/// The acceptance item that names the verifier the Objective itself names.
fn verifier_acceptance() -> &'static str {
    if cfg!(windows) {
        "cmd /c verify.cmd 通过"
    } else {
        "sh ./verify.sh 通过"
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
        serde_json::json!({
            "objective": objective,
            "acceptance": acceptance,
            "task_relation": "continue"
        }),
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
                    verifier_acceptance(),
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
    assert_eq!(
        goals[0].state,
        leveler_storage::GoalState::Running,
        "a blocked goal still owes the deliverables it could not produce"
    );
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

/// A deterministic scope refusal cannot be satisfied by sending the exact
/// same objective and acceptance again. The second answer must say that
/// nothing changed, so the model knows to alter its plan instead of looping.
#[tokio::test]
async fn an_identical_commit_refusal_is_named_as_a_repeat() {
    let fx = fixture(
        vec![
            commit_with(vec!["stage-1.txt 存在"], staged_objective()),
            commit_with(vec!["stage-1.txt 存在"], staged_objective()),
            Scripted::Text("I will change the acceptance checks."),
        ],
        false,
    )
    .await;
    let mut events = Vec::new();
    fx.coding
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

    let refusals: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == "acceptance_narrowed" =>
            {
                Some(detail)
            }
            _ => None,
        })
        .collect();
    assert!(
        refusals.len() >= 2,
        "both refusals must be visible: {events:?}"
    );
    assert!(
        refusals
            .iter()
            .any(|detail| detail.contains("refusal #2") && detail.contains("have not changed")),
        "a repeated deterministic refusal must say it is unchanged: {refusals:?}"
    );
    assert!(
        goal_records(&fx).await.is_empty(),
        "a refused commit must not create a task"
    );
}

/// Markdown section numbering is document structure, not an enumerated
/// deliverable. A long spec whose headings use `## 一、...` used to extract
/// `## 一` as a requirement, so `commit_task` refused every honest acceptance
/// list. The real binder must accept this request.
#[tokio::test]
async fn markdown_headings_do_not_become_commit_requirements() {
    const OBJECTIVE: &str = "# Phase 3C 开发任务\n\n\
        ## 一、项目目标\n\n在现有项目中实现内容社区功能。\n\n\
        ## 二、后端实现\n\n使用 Go + PostgreSQL 完成业务接口。\n\n\
        ## 三、前端实现\n\n实现文章、讨论和评论界面。\n\n\
        ## 四、验收\n\n运行测试，验证功能完整性。\n\n\
        请自主规划并执行。\n允许根据任务需要拆分并委托子任务。";
    let fx = fixture(
        vec![
            commit_with(
                vec!["实现文章、讨论和评论界面", "运行测试并验证功能完整性"],
                OBJECTIVE,
            ),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "blocked",
                    "summary": "scope confirmed; implementation not started in this contract test"
                }),
            ),
        ],
        false,
    )
    .await;
    let mut events = Vec::new();
    let report = fx
        .coding
        .chat(
            &fx.session,
            &spec_for(&fx, OBJECTIVE),
            vec![ContentPart::Text {
                text: OBJECTIVE.into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        !events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == "acceptance_narrowed" && detail.contains("##")
        )),
        "a Markdown heading must never be the reason a commit is refused: {events:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(
        goals.len(),
        1,
        "the real binder must accept a spec with Markdown headings: {report:?}"
    );
    assert_eq!(goals[0].acceptance.len(), 2);
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
                    verifier_acceptance(),
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

/// ONE task owes ONE goal.
///
/// A later Chat window that commits the same work must JOIN the goal the task
/// already owes. Opening a sibling would split one intent across two identities
/// and restart the acceptance the user's request was committed against, and the
/// model's paraphrase of the objective is not evidence of a new task. The user's
/// new instruction becomes an amendment on that goal, never a replacement of its
/// objective.
#[tokio::test]
async fn a_later_commit_joins_the_owed_goal_instead_of_opening_a_second_one() {
    let fx = fixture(
        vec![
            Scripted::commit(),
            Scripted::Text("第一阶段完成，后面的还没做。"),
            // A new Chat window: the user asks for the rest in their own words,
            // and the model commits again with a restated objective.
            commit_with(vec!["stage-2.txt 存在"], "把清单剩下的部分做完"),
            Scripted::Text("还在继续。"),
        ],
        false,
    )
    .await;
    // Window 1 commits and is then interrupted, leaving the task owed.
    fx.runtime.cancel_after.store(2, Ordering::SeqCst);
    let first = fx
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
    assert!(first.is_err(), "the interrupted window reports no success");
    let owed = goal_records(&fx).await;
    assert_eq!(owed.len(), 1);
    let goal_id = owed[0].id.clone();
    assert_eq!(
        owed[0].state,
        leveler_storage::GoalState::Running,
        "an interrupted window leaves the task owed"
    );
    let windows_after_first = owed[0].windows_run;

    fx.coding
        .chat(
            &fx.session,
            &spec(&fx),
            vec![ContentPart::Text {
                text: "把清单剩下的部分做完".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let goals = goal_records(&fx).await;
    assert_eq!(
        goals.len(),
        1,
        "one task owes one goal; a restated objective must not open a second: {goals:?}"
    );
    assert_eq!(
        goals[0].id, goal_id,
        "the identity of the owed task is kept"
    );
    assert_eq!(
        goals[0].objective, USER_REQUEST,
        "the objective is the user's, never the model's paraphrase"
    );
    assert_eq!(
        goals[0].acceptance.len(),
        2,
        "the acceptance the task was committed against is not replaced: {goals:?}"
    );
    assert!(
        goals[0]
            .amendments
            .iter()
            .any(|amendment| amendment.contains("剩下的部分")),
        "the user's new instruction is recorded as scope, not as a new goal: {goals:?}"
    );
    assert!(
        goals[0].windows_run > windows_after_first,
        "the joined window is counted on the same goal: {} then {}",
        windows_after_first,
        goals[0].windows_run
    );
}

/// A Chat conversation with several tool rounds is not a task. Only the model's
/// explicit declaration may commit one — tool count, rounds and file writes are
/// not a reason to upgrade.
#[tokio::test]
async fn a_multi_round_chat_with_tools_does_not_upgrade_itself() {
    let fx = fixture(
        vec![
            Scripted::Tool(
                "write_file",
                serde_json::json!({"path": "notes.md", "content": "# notes\n"}),
            ),
            Scripted::Tool("read_file", serde_json::json!({"path": "notes.md"})),
            Scripted::Tool(
                "run_command",
                serde_json::json!({"program": "sh", "args": ["-c", "true"], "reason": "check"}),
            ),
            Scripted::Text("记下了，没有需要持续跟踪的任务。"),
        ],
        false,
    )
    .await;

    let report = chat(&fx).await;

    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
    assert!(
        goal_records(&fx).await.is_empty(),
        "an ordinary conversation must not commit a goal: {:?}",
        goal_records(&fx).await
    );
    {
        let requests = fx.runtime.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| tool_names(request).contains(&"commit_task")),
            "the delegation entry stays available to the model"
        );
    }
    assert_eq!(
        SessionRepository::new(&fx.db)
            .get(&fx.session)
            .await
            .unwrap()
            .unwrap()
            .collaboration,
        "chat",
        "the session's axis is untouched"
    );
}

// ---------------------------------------------------------------------------
// A Goal bound from a Chat turn, stopped at the user's own stage boundary.
//
// The user authorized stage 1 only. The goal still owes stage 2, and the pause
// is an AUTHORIZATION fact — not a completion, not a failure, and not something
// a status question can release. Only the user's own continuation does.
// ---------------------------------------------------------------------------

fn staged_pause_objective() -> &'static str {
    if cfg!(windows) {
        "完成两个阶段：阶段 1 创建 stage-1.txt 并运行 cmd /c verify.cmd 验收；\
         阶段 1 完成后停下，等待我确认再执行阶段 2；\
         阶段 2 创建 stage-2.txt 并运行 cmd /c verify.cmd 验收。"
    } else {
        "完成两个阶段：阶段 1 创建 stage-1.txt 并运行 sh ./verify.sh 验收；\
         阶段 1 完成后停下，等待我确认再执行阶段 2；\
         阶段 2 创建 stage-2.txt 并运行 sh ./verify.sh 验收。"
    }
}

fn staged_full_objective() -> &'static str {
    if cfg!(windows) {
        "一次完成两个阶段：阶段 1 创建 stage-1.txt 并运行 cmd /c verify.cmd 验收；\
         阶段 2 创建 stage-2.txt 并运行 cmd /c verify.cmd 验收。\
         中间不要停下来等我确认。"
    } else {
        "一次完成两个阶段：阶段 1 创建 stage-1.txt 并运行 sh ./verify.sh 验收；\
         阶段 2 创建 stage-2.txt 并运行 sh ./verify.sh 验收。\
         中间不要停下来等我确认。"
    }
}

fn stage_plan(status1: &'static str, status2: &'static str) -> Scripted {
    Scripted::Tool(
        "update_plan",
        serde_json::json!({
            "plan": [
                {"step": "阶段 1：创建 stage-1.txt 并验收", "status": status1},
                {"step": "阶段 2：创建 stage-2.txt 并验收", "status": status2}
            ]
        }),
    )
}

fn write_named(name: &'static str) -> Scripted {
    Scripted::Tool(
        "write_file",
        serde_json::json!({"path": name, "content": format!("{name}\n")}),
    )
}

fn pause_stage_one_script() -> Vec<Scripted> {
    vec![
        commit_with(
            vec![
                "stage-1.txt 存在",
                "stage-2.txt 存在",
                verifier_acceptance(),
            ],
            staged_pause_objective(),
        ),
        stage_plan("in_progress", "pending"),
        write_named("stage-1.txt"),
        Scripted::Tool("run_command", verifier_call()),
        stage_plan("completed", "pending"),
        Scripted::Text("阶段 1 已完成并验收。按你的要求，阶段 2 等待你的确认，本轮先停在这里。"),
    ]
}

#[tokio::test]
async fn a_goal_bound_pause_keeps_the_goal_open_and_unpressured() {
    let fx = fixture(pause_stage_one_script(), false).await;
    write_verifier(fx.dir.path());
    let mut events = Vec::new();
    let report = fx
        .coding
        .chat(
            &fx.session,
            &spec_for(&fx, staged_pause_objective()),
            vec![ContentPart::Text {
                text: staged_pause_objective().into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Answered,
        "a user pause ends the round normally, not as a stall: {report:?}"
    );
    assert!(
        report
            .stop_detail
            .as_deref()
            .is_some_and(leveler_agent::closeout::is_user_pause_detail),
        "the terminal must say WHY the round ended: {report:?}"
    );
    assert_eq!(
        fx.runtime.calls.load(Ordering::SeqCst),
        6,
        "the reached boundary buys NO extra model round"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::RuntimeInjection { kind, .. }
                if kind.contains("goal_unresolved")
        )),
        "a user pause is not an unresolved goal to pressure: {events:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(
        goals[0].state,
        leveler_storage::GoalState::Running,
        "the goal is not cancelled and not completed"
    );
    assert!(
        goals[0].user_paused_at.is_some(),
        "the pause is durable with the goal it bounds: {goals:?}"
    );
    assert_eq!(
        goals[0].acceptance.len(),
        3,
        "the full acceptance plan is preserved: {goals:?}"
    );
    assert!(
        !fx.dir.path().join("stage-2.txt").exists(),
        "the withheld stage must not be executed"
    );
    assert!(fx.dir.path().join("stage-1.txt").exists());
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

#[tokio::test]
async fn a_status_question_after_a_pause_does_not_release_it() {
    let mut script = pause_stage_one_script();
    script.push(Scripted::Text(
        "阶段 1 修改了 stage-1.txt；验收命令 sh ./verify.sh 退出码 0。",
    ));
    let fx = fixture(script, false).await;
    write_verifier(fx.dir.path());
    let staged = staged_pause_objective();
    fx.coding
        .chat(
            &fx.session,
            &spec_for(&fx, staged),
            vec![ContentPart::Text {
                text: staged.into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let goal_id = goal_records(&fx).await[0].id.clone();

    let query = "刚才阶段 1 修改了哪些文件？测试通过了吗？";
    let answer = fx
        .coding
        .chat(
            &fx.session,
            &spec(&fx),
            vec![ContentPart::Text { text: query.into() }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(answer.stop_reason, leveler_agent::StopReason::Answered);
    assert!(
        !answer
            .stop_detail
            .as_deref()
            .is_some_and(leveler_agent::closeout::is_user_pause_detail),
        "the question is an ordinary answer, not a second pause boundary"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "a question must not open a second goal");
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Running);
    assert!(
        goals[0].user_paused_at.is_some(),
        "the pause outlives the question: {goals:?}"
    );
    assert!(
        !fx.dir.path().join("stage-2.txt").exists(),
        "the question does not authorize the withheld stage"
    );
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

#[tokio::test]
async fn an_explicit_continuation_resumes_the_paused_goal_and_verifies_it() {
    let mut script = pause_stage_one_script();
    script.extend([
        write_named("stage-2.txt"),
        Scripted::Tool("run_command", verifier_call()),
        stage_plan("completed", "completed"),
        Scripted::Tool(
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "阶段 1 与阶段 2 都已交付并验收",
                "verification": {"status": "passed", "detail": verifier_label()},
                "requirements": [
                    {"requirement": "stage-1.txt", "status": "done",
                     "evidence": "created stage-1.txt"},
                    {"requirement": "stage-2.txt", "status": "done",
                     "evidence": "created stage-2.txt"},
                    {"requirement": verifier_path(), "status": "done",
                     "evidence": "ran the verifier"}
                ]
            }),
        ),
    ]);
    let fx = fixture(script, false).await;
    write_verifier(fx.dir.path());
    let staged = staged_pause_objective();
    fx.coding
        .chat(
            &fx.session,
            &spec_for(&fx, staged),
            vec![ContentPart::Text {
                text: staged.into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let paused = goal_records(&fx).await;
    let goal_id = paused[0].id.clone();
    assert!(paused[0].user_paused_at.is_some());

    let resumed = fx
        .coding
        .resume_with_instruction(
            &fx.session,
            &spec_for(&fx, staged),
            Message::user_input("确认，继续阶段 2，把剩余任务完成并验收。"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        resumed.stop_reason,
        leveler_agent::StopReason::Completed,
        "completion requires the real verification, not the pause being lifted: {resumed:?}"
    );
    assert_eq!(resumed.outcome, leveler_lifecycle::TaskOutcome::Completed);
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "the same goal, not a smaller replacement");
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
    assert_eq!(
        goals[0].objective, staged,
        "the original objective is never narrowed by the continuation"
    );
    assert_eq!(
        goals[0].acceptance.len(),
        3,
        "the stored acceptance plan is unchanged: {goals:?}"
    );
    assert!(
        goals[0].user_paused_at.is_none(),
        "authorization to continue clears the pause: {goals:?}"
    );
    assert!(
        goals[0]
            .amendments
            .iter()
            .any(|amendment| amendment.contains("继续阶段 2")),
        "the continuation is durable with its goal: {goals:?}"
    );
    assert!(fx.dir.path().join("stage-2.txt").exists());
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

/// The reverse: without a user pause, a model claiming it needs confirmation
/// is still held to the goal contract, and the harness drives it to finish.
#[tokio::test]
async fn a_model_claimed_confirmation_does_not_pause_a_goal() {
    let fx = fixture(
        vec![
            commit_with(
                vec![
                    "stage-1.txt 存在",
                    "stage-2.txt 存在",
                    verifier_acceptance(),
                ],
                staged_full_objective(),
            ),
            stage_plan("in_progress", "pending"),
            write_named("stage-1.txt"),
            stage_plan("completed", "pending"),
            Scripted::Text("阶段 1 完成了。阶段 2 需要你确认后才能继续。"),
            // The harness states the still-active goal; the model continues.
            write_named("stage-2.txt"),
            Scripted::Tool("run_command", verifier_call()),
            stage_plan("completed", "completed"),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "两个阶段都已完成并验收",
                    "verification": {"status": "passed", "detail": verifier_label()},
                    "requirements": [
                        {"requirement": "stage-1.txt", "status": "done",
                         "evidence": "created stage-1.txt"},
                        {"requirement": "stage-2.txt", "status": "done",
                         "evidence": "created stage-2.txt"},
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
    let mut events = Vec::new();
    let report = fx
        .coding
        .chat(
            &fx.session,
            &spec_for(&fx, staged_full_objective()),
            vec![ContentPart::Text {
                text: staged_full_objective().into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Completed,
        "the model's own claim is not a user pause: {report:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::RuntimeInjection { kind, .. }
                if kind.contains("goal_unresolved")
        )),
        "the Goal continuation must still fire without a user pause: {events:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
    assert!(goals[0].user_paused_at.is_none());
    assert!(fx.dir.path().join("stage-2.txt").exists());
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

/// A Goal session (not a Chat turn that committed) keeps its route armed at a
/// user pause, so the next message is still about this goal. A status question
/// must be answered inside that route WITHOUT the harness driving the withheld
/// stage; an explicit authorization then continues the same goal.
#[tokio::test]
async fn a_paused_goal_route_answers_a_query_then_continues_on_authorization() {
    let query = "刚才阶段 1 修改了哪些文件？测试通过了吗？";
    let release = "确认，继续阶段 2，把剩余任务完成并验收。";
    let fx = fixture(
        vec![
            stage_plan("in_progress", "pending"),
            write_named("stage-1.txt"),
            Scripted::Tool("run_command", verifier_call()),
            stage_plan("completed", "pending"),
            Scripted::Text("阶段 1 已完成并验收。按你的要求，阶段 2 等待你的确认。"),
            // The query turn: an ordinary answer, not authorization.
            Scripted::Text("阶段 1 修改了 stage-1.txt；验收命令 sh ./verify.sh 退出码 0。"),
            // The authorization turn: the model continues for real.
            write_named("stage-2.txt"),
            Scripted::Tool("run_command", verifier_call()),
            stage_plan("completed", "completed"),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "两个阶段都已交付并验收",
                    "verification": {"status": "passed", "detail": verifier_label()},
                    "requirements": [
                        {"requirement": "stage-1.txt", "status": "done",
                         "evidence": "created stage-1.txt"},
                        {"requirement": "stage-2.txt", "status": "done",
                         "evidence": "created stage-2.txt"},
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
    let staged = staged_pause_objective();
    let mut first_events = Vec::new();
    let paused = fx
        .coding
        .run(
            &fx.session,
            &spec_for(&fx, staged),
            &mut |event| first_events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(paused.stop_reason, leveler_agent::StopReason::Answered);
    assert!(
        paused
            .stop_detail
            .as_deref()
            .is_some_and(leveler_agent::closeout::is_user_pause_detail)
    );
    let goal_id = goal_records(&fx).await[0].id.clone();
    assert!(goal_records(&fx).await[0].user_paused_at.is_some());
    let calls_after_pause = fx.runtime.calls.load(Ordering::SeqCst);

    let mut query_events = Vec::new();
    let answered = fx
        .coding
        .run(
            &fx.session,
            &spec_for(&fx, query),
            &mut |event| query_events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        answered.stop_reason,
        leveler_agent::StopReason::Answered,
        "the query turn ends normally: {answered:?}"
    );
    assert!(
        answered
            .stop_detail
            .as_deref()
            .is_some_and(leveler_agent::closeout::is_user_pause_detail),
        "the durable pause still governs the query turn: {answered:?}"
    );
    assert_eq!(
        fx.runtime.calls.load(Ordering::SeqCst),
        calls_after_pause + 1,
        "a query buys exactly its own answer, no goal continuation"
    );
    assert!(
        !query_events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::RuntimeInjection { kind, .. }
                if kind.contains("goal_unresolved")
        )),
        "a paused goal must not be pressured by a status question: {query_events:?}"
    );
    let after_query = goal_records(&fx).await;
    assert_eq!(after_query.len(), 1);
    assert_eq!(after_query[0].id, goal_id);
    assert_eq!(after_query[0].state, leveler_storage::GoalState::Running);
    assert!(after_query[0].user_paused_at.is_some());
    assert!(!fx.dir.path().join("stage-2.txt").exists());

    let mut release_events = Vec::new();
    let completed = fx
        .coding
        .run(
            &fx.session,
            &spec_for(&fx, release),
            &mut |event| release_events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        completed.stop_reason,
        leveler_agent::StopReason::Completed,
        "the user's authorization continues the goal to real completion: {completed:?} events={release_events:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
    assert!(goals[0].user_paused_at.is_none());
    assert_eq!(
        goals[0].objective, staged,
        "the query and the authorization never replace the goal's objective"
    );
    assert_eq!(
        goals[0].acceptance.len(),
        0,
        "a /goal-shaped task has no model plan"
    );
    assert!(fx.dir.path().join("stage-2.txt").exists());
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

/// The real-provider shape: a model that reached the user's boundary reports
/// "waiting for the user" and reaches for `update_goal(blocked)`. That would
/// hide the pause, end the goal route and leave resumption to a literal `继续`.
/// The harness must state that the pause is not an obstruction and let the
/// round end normally, keeping the goal paused and recoverable.
#[tokio::test]
async fn a_blocked_claim_at_the_user_boundary_is_a_pause_not_a_block() {
    let fx = fixture(
        vec![
            commit_with(
                vec!["stage-1.txt 存在", "stage-2.txt 存在", verifier_acceptance()],
                staged_pause_objective(),
            ),
            Scripted::Tool(
                "update_plan",
                serde_json::json!({"plan": [
                    {"step": "阶段 1：创建 stage-1.txt", "status": "in_progress"},
                    {"step": "阶段 1：运行 sh ./verify.sh 确认", "status": "pending"},
                    {"step": "阶段 1 完成后停下，等待用户确认后再继续", "status": "pending"},
                    {"step": "阶段 2：创建 stage-2.txt", "status": "pending"}
                ]}),
            ),
            write_named("stage-1.txt"),
            Scripted::Tool("run_command", verifier_call()),
            Scripted::Tool(
                "update_plan",
                serde_json::json!({"plan": [
                    {"step": "阶段 1：创建 stage-1.txt", "status": "completed"},
                    {"step": "阶段 1：运行 sh ./verify.sh 确认", "status": "completed"},
                    {"step": "阶段 1 完成后停下，等待用户确认后再继续", "status": "in_progress"},
                    {"step": "阶段 2：创建 stage-2.txt", "status": "pending"}
                ]}),
            ),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({"status": "blocked", "summary": "waiting for the user's confirmation"}),
            ),
            Scripted::Text("阶段 1 已完成并验收；阶段 2 等待你的确认。"),
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
            &spec_for(&fx, staged_pause_objective()),
            vec![ContentPart::Text {
                text: staged_pause_objective().into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Answered,
        "the user's pause is not a blocked goal: {report:?}"
    );
    assert!(
        report
            .stop_detail
            .as_deref()
            .is_some_and(leveler_agent::closeout::is_user_pause_detail),
        "the terminal records the pause: {report:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, .. }
                if kind == "user_pause_boundary"
        )),
        "the blocked claim is answered with the pause fact: {events:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals[0].state, leveler_storage::GoalState::Running);
    assert!(goals[0].user_paused_at.is_some());
    assert!(!fx.dir.path().join("stage-2.txt").exists());
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

// ---------------------------------------------------------------------------
// A user supplement stated while the goal waits for them.
//
// A new execution requirement, a status question, an authorization to continue
// and the completion of the work are four different facts. The user's own
// supplement is SCOPE: it must reach the durable amendment list so a later
// window cannot forget it, and it must not release the pause, start the
// withheld stage, or open a second goal.
// ---------------------------------------------------------------------------

/// A supplement that names a new deliverable and needs no `继续` to be scope.
fn supplement_request() -> &'static str {
    if cfg!(windows) {
        "顺便把 stage-extra.txt 也补上，内容为一行 extra，然后运行 cmd /c verify.cmd 确认。"
    } else {
        "顺便把 stage-extra.txt 也补上，内容为一行 extra，然后运行 sh ./verify.sh 确认。"
    }
}

async fn pause_a_goal(fx: &Fixture) {
    let staged = staged_pause_objective();
    fx.coding
        .chat(
            &fx.session,
            &spec_for(fx, staged),
            vec![ContentPart::Text {
                text: staged.into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();
}

async fn say(fx: &Fixture, text: &str) -> leveler_agent::coding::TaskReport {
    fx.coding
        .chat(
            &fx.session,
            &spec(fx),
            vec![ContentPart::Text { text: text.into() }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_supplement_to_a_paused_goal_is_recorded_without_releasing_the_pause() {
    let mut script = pause_stage_one_script();
    script.push(Scripted::Text("补充要求已记下；阶段 2 仍等你确认。"));
    let fx = fixture(script, false).await;
    write_verifier(fx.dir.path());
    pause_a_goal(&fx).await;
    let paused = goal_records(&fx).await;
    assert_eq!(paused.len(), 1, "{paused:?}");
    let goal_id = paused[0].id.clone();
    assert!(paused[0].user_paused_at.is_some(), "{paused:?}");

    let supplement = supplement_request();
    say(&fx, supplement).await;

    let goals = goal_records(&fx).await;
    assert_eq!(
        goals.len(),
        1,
        "a supplement must not open a second goal: {goals:?}"
    );
    assert_eq!(
        goals[0].id, goal_id,
        "the same goal the user was talking about"
    );
    assert!(
        goals[0]
            .amendments
            .iter()
            .any(|amendment| amendment.trim() == supplement),
        "the user's own words are durable scope: {goals:?}"
    );
    assert!(
        goals[0].user_paused_at.is_some(),
        "adding a requirement is not an authorization to continue: {goals:?}"
    );
    assert_eq!(
        goals[0].state,
        leveler_storage::GoalState::Running,
        "the goal still owes its work"
    );
    assert!(
        !fx.dir.path().join("stage-extra.txt").exists(),
        "the supplement is recorded, not executed: {:?}",
        fx.runtime.requests.lock().unwrap().len()
    );
    assert!(
        !fx.dir.path().join("stage-2.txt").exists(),
        "the withheld stage stays withheld"
    );
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

#[tokio::test]
async fn a_status_question_in_a_paused_goal_is_not_scope() {
    let mut script = pause_stage_one_script();
    script.push(Scripted::Text(
        "阶段 1 修改了 stage-1.txt；验收命令退出码 0。",
    ));
    let fx = fixture(script, false).await;
    write_verifier(fx.dir.path());
    pause_a_goal(&fx).await;
    let paused = goal_records(&fx).await;
    assert_eq!(paused[0].amendments.len(), 0, "{paused:?}");

    say(
        &fx,
        "刚才阶段 1 修改了哪些文件？测试通过了吗？现在进度怎么样？",
    )
    .await;

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "{goals:?}");
    assert_eq!(
        goals[0].amendments,
        Vec::<String>::new(),
        "a question about the work changes no scope: {goals:?}"
    );
    assert!(goals[0].user_paused_at.is_some(), "{goals:?}");
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

#[tokio::test]
async fn an_independent_request_with_no_deliverable_is_not_scope() {
    let mut script = pause_stage_one_script();
    script.push(Scripted::Text("README 没有过时的说明。"));
    let fx = fixture(script, false).await;
    write_verifier(fx.dir.path());
    pause_a_goal(&fx).await;

    say(&fx, "另外帮我检查一下 README 有没有过时的说明。").await;

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "{goals:?}");
    assert_eq!(
        goals[0].amendments,
        Vec::<String>::new(),
        "an open-ended request enumerates no deliverable, so it adds no scope: {goals:?}"
    );
    assert!(
        goals[0].user_paused_at.is_some(),
        "an unrelated request does not release the pause: {goals:?}"
    );
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

/// A requirement the user added mid-flight is gated with its OWN mechanical
/// evidence.
///
/// The reported shape: the supplement entered the goal scope and the goal still
/// closed on the model's account alone. Here the first completion is refused
/// because the newly required artifact does not exist, and only the window that
/// really creates it (and re-runs the verifier) may complete.
#[tokio::test]
async fn a_dynamic_requirement_needs_its_own_mechanical_evidence() {
    fn complete(accounts: serde_json::Value) -> Scripted {
        Scripted::Tool(
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "阶段 2 与补充要求都已交付",
                "verification": {"status": "passed", "detail": verifier_label()},
                "requirements": accounts
            }),
        )
    }
    let mut script = pause_stage_one_script();
    script.push(Scripted::Text("补充要求已记下；阶段 2 仍等你确认。"));
    script.extend([
        // Stage 2 only: the supplement is still owed, and the model claims it
        // anyway. The runtime must refuse, not warn.
        write_named("stage-2.txt"),
        Scripted::Tool("run_command", verifier_call()),
        stage_plan("completed", "completed"),
        complete(serde_json::json!([
            {"requirement": "stage-1.txt", "status": "done", "evidence": "created stage-1.txt"},
            {"requirement": "stage-2.txt", "status": "done", "evidence": "created stage-2.txt"},
            {"requirement": "stage-extra.txt", "status": "done", "evidence": "created stage-extra.txt"},
            {"requirement": verifier_path(), "status": "done", "evidence": "ran the verifier"}
        ])),
        // The refusal is answered with the real work.
        write_named("stage-extra.txt"),
        Scripted::Tool("run_command", verifier_call()),
        complete(serde_json::json!([
            {"requirement": "stage-1.txt", "status": "done", "evidence": "created stage-1.txt"},
            {"requirement": "stage-2.txt", "status": "done", "evidence": "created stage-2.txt"},
            {"requirement": "stage-extra.txt", "status": "done", "evidence": "created stage-extra.txt"},
            {"requirement": verifier_path(), "status": "done", "evidence": "ran the verifier"}
        ])),
    ]);
    let fx = fixture(script, false).await;
    write_verifier(fx.dir.path());
    pause_a_goal(&fx).await;
    say(&fx, supplement_request()).await;

    let staged = staged_pause_objective();
    let mut events = Vec::new();
    let resumed = fx
        .coding
        .resume_with_instruction(
            &fx.session,
            &spec_for(&fx, staged),
            Message::user_input("确认，继续阶段 2，把剩余任务和补充要求一起完成并验收。"),
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == "requirements_refused" && detail.contains("stage-extra.txt")
        )),
        "the missing dynamic deliverable must be refused by name: {events:?}"
    );
    assert_eq!(
        resumed.stop_reason,
        leveler_agent::StopReason::Completed,
        "the goal closes on the real evidence of the second attempt: {resumed:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
    assert!(
        goals[0]
            .amendments
            .iter()
            .any(|amendment| amendment.trim() == supplement_request()),
        "the supplement survives the resume: {goals:?}"
    );
    assert!(
        fx.dir.path().join("stage-extra.txt").exists(),
        "the dynamic requirement was really delivered"
    );
    assert!(
        !resumed
            .completion_warnings
            .iter()
            .any(|warning| warning.contains("stage-extra.txt")),
        "a delivered, mechanically checked requirement is not an unverified boundary: {resumed:?}"
    );
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

/// The user's authorization is the newest word about the pause.
///
/// The durable pause is cleared at the END of a window, so during the window
/// the user just authorized it is still stored. Reading the stored column as
/// "still paused" would cut the authorized window short at its first quiet
/// round and re-pause a goal the user has already released.
#[tokio::test]
async fn an_authorized_window_is_not_cut_short_as_paused() {
    let mut script = pause_stage_one_script();
    script.extend([
        write_named("stage-2.txt"),
        Scripted::Tool("run_command", verifier_call()),
        stage_plan("completed", "completed"),
        // The model delivers and closes with prose, without declaring the goal
        // complete. The window must not be read as a user pause again.
        Scripted::Text("阶段 2 也已交付并验收，两个阶段都完成了。"),
    ]);
    let fx = fixture(script, false).await;
    write_verifier(fx.dir.path());
    let staged = staged_pause_objective();
    pause_a_goal(&fx).await;
    let paused = goal_records(&fx).await;
    assert!(paused[0].user_paused_at.is_some(), "{paused:?}");

    let resumed = fx
        .coding
        .resume_with_instruction(
            &fx.session,
            &spec_for(&fx, staged),
            Message::user_input("确认，继续阶段 2，把剩余任务完成并验收。"),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        !resumed
            .stop_detail
            .as_deref()
            .is_some_and(leveler_agent::closeout::is_user_pause_detail),
        "the user released the pause, so the window is not a pause: {resumed:?}"
    );
    let goals = goal_records(&fx).await;
    assert!(
        goals[0].user_paused_at.is_none(),
        "the authorized window must not re-pause the goal: {goals:?}"
    );
    assert!(fx.dir.path().join("stage-2.txt").exists());
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

/// An accepted completion can still have a boundary the runtime could not
/// verify. That boundary was stated only on the terminal, so the model wrote
/// the delivery without it and could claim more than the record held. The
/// acceptance the model reads must carry the runtime's own facts.
#[tokio::test]
async fn an_accepted_completion_tells_the_model_what_it_could_not_verify() {
    // "把全部功能真实测试一遍" names no file, no command and no suite: the
    // runtime can hold the model to naming the subject, but it can never prove
    // the coverage. That is a boundary, and it must reach the model.
    let objective = "本地启动项目，把全部功能真实测试一遍。";
    let fx = fixture(
        vec![
            commit_with(vec!["全部功能实测完成", "make up 成功"], objective),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "全部功能实测完成",
                    "verification": {"status": "not_applicable", "detail": "只做了验证，没有改文件"},
                    "requirements": [
                        {"requirement": "全部功能", "status": "done",
                         "evidence": "门户页面、运营接口与 MCP 工具都实测过"}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;
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

    let receipt = events
        .iter()
        .find_map(|event| match event {
            leveler_engine::EngineEvent::ToolCallFinished {
                name,
                is_error,
                preview,
                ..
            } if name == "update_goal" && !is_error => Some(preview.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the accepted completion has no tool result: {events:?}"));
    assert!(
        receipt.contains("全部功能") && receipt.contains("no mechanical check"),
        "the receipt must carry the boundary the runtime computed: {receipt}"
    );
    assert_ne!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Failed,
        "an accepted completion with a boundary is still a completion: {report:?}"
    );
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

/// F-11: a second commit with a different objective and no explicit relation
/// must be refused, not silently adopted into the running goal.
#[tokio::test]
async fn a_commit_without_a_relation_is_refused_instead_of_silently_adopting() {
    let fx = fixture(
        vec![
            Scripted::commit(),
            Scripted::Text("第一阶段完成，后面的还没做。"),
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": "把清单剩下的部分做完",
                    "acceptance": ["stage-2.txt 存在"]
                }),
            ),
            Scripted::Text("还在继续。"),
        ],
        false,
    )
    .await;
    fx.runtime.cancel_after.store(2, Ordering::SeqCst);
    let first = fx
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
    assert!(first.is_err(), "the interrupted window reports no success");
    let owed = goal_records(&fx).await;
    assert_eq!(owed.len(), 1);
    let goal_id = owed[0].id.clone();
    let acceptance = owed[0].acceptance.clone();
    let objective = owed[0].objective.clone();

    let mut events = Vec::new();
    let _ = fx
        .coding
        .chat(
            &fx.session,
            &spec(&fx),
            vec![ContentPart::Text {
                text: "把清单剩下的部分做完".into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await;

    let goals = goal_records(&fx).await;
    assert_eq!(
        goals.len(),
        1,
        "no second goal may be opened without an explicit relation: {goals:?}"
    );
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(goals[0].objective, objective);
    assert_eq!(
        goals[0].acceptance, acceptance,
        "the old acceptance must not be replaced or widened"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::ToolCallFinished {
                name,
                is_error: true,
                preview,
                ..
            } if name == "commit_task" && preview.contains("task_relation_required")
        )),
        "the refusal must name the missing relation: {events:?}"
    );
}

/// F-11: an explicit `new` relation opens an independent goal with its own
/// objective/acceptance; the old running goal is untouched.
#[tokio::test]
async fn an_explicit_new_relation_opens_an_independent_goal() {
    let fx = fixture(
        vec![
            Scripted::commit(),
            Scripted::Text("第一阶段完成，后面的还没做。"),
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": "把清单剩下的部分做完",
                    "acceptance": ["stage-2.txt 存在"],
                    "task_relation": "new"
                }),
            ),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({"status": "blocked", "summary": "independent task paused"}),
            ),
            Scripted::Text("独立任务暂时阻塞。"),
        ],
        false,
    )
    .await;
    fx.runtime.cancel_after.store(2, Ordering::SeqCst);
    let first = fx
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
    assert!(first.is_err(), "the interrupted window reports no success");
    let old = goal_records(&fx).await;
    assert_eq!(old.len(), 1);
    let old_id = old[0].id.clone();
    let old_acceptance = old[0].acceptance.clone();

    let _ = fx
        .coding
        .chat(
            &fx.session,
            &spec(&fx),
            vec![ContentPart::Text {
                text: "把清单剩下的部分做完".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await;

    let goals = goal_records(&fx).await;
    assert_eq!(
        goals.len(),
        2,
        "an explicit new task gets its own goal: {goals:?}"
    );
    let old = goals.iter().find(|goal| goal.id == old_id).unwrap();
    assert_eq!(old.acceptance, old_acceptance, "the old task is untouched");
    assert_eq!(
        old.state,
        leveler_storage::GoalState::Running,
        "the old goal still owes its work"
    );
    let new = goals.iter().find(|goal| goal.id != old_id).unwrap();
    assert_eq!(new.objective, "把清单剩下的部分做完");
    assert_eq!(new.acceptance, vec!["stage-2.txt 存在".to_string()]);
    assert_ne!(
        new.acceptance, old.acceptance,
        "the new task must not inherit the old acceptance"
    );
}
