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
    /// Wait on the background task id the runtime just returned to the model.
    /// The harness learns the id from the real tool result, never predicts it.
    WaitLatestBackground,
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
        self.requests.lock().unwrap().push(request.clone());
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
            Scripted::WaitLatestBackground => {
                let task_id = request
                    .messages
                    .iter()
                    .rev()
                    .flat_map(|message| message.content.iter().rev())
                    .find_map(|part| match part {
                        ContentPart::ToolResult { result } => {
                            result.content.split("task_id: ").nth(1).map(|rest| {
                                rest.split_whitespace()
                                    .next()
                                    .unwrap_or_default()
                                    .to_string()
                            })
                        }
                        _ => None,
                    })
                    .expect("a background start result must name its task");
                (
                    Message::from_parts(
                        Role::Assistant,
                        vec![ContentPart::ToolCall {
                            call: ToolCall {
                                id: leveler_core::ToolCallId::new(format!("call-{call}")),
                                name: "wait_task".to_string(),
                                arguments: serde_json::json!({ "task_id": task_id }),
                            },
                        }],
                        None,
                    ),
                    FinishReason::ToolCalls,
                )
            }
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
    _home: tempfile::TempDir,
    mode: PermissionProfile,
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
    fixture_with_mode(script, read_only, PermissionProfile::Assisted).await
}

async fn fixture_with_mode(
    script: Vec<Scripted>,
    read_only: bool,
    mode: PermissionProfile,
) -> Fixture {
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
    // Keep the sandbox scratch in a private home OUTSIDE the workspace: the
    // real home is not writable under the agent sandbox, and a test must not
    // write there. A home inside the workspace is refused by the product.
    let home = tempfile::tempdir().unwrap();
    let leveler_home = home.path().join("leveler-home");
    let scratch = home.path().join("tmp");
    std::fs::create_dir_all(&leveler_home).unwrap();
    std::fs::create_dir_all(&scratch).unwrap();
    let mut environment: Vec<(std::ffi::OsString, std::ffi::OsString)> =
        std::env::vars_os().collect();
    environment.retain(|(key, _)| key != "LEVELER_HOME");
    environment.push((
        std::ffi::OsString::from("LEVELER_HOME"),
        leveler_home.into_os_string(),
    ));
    let context = ToolContext::with_environment(
        Workspace::new(dir.path()).unwrap(),
        mode,
        Arc::new(leveler_core::EnvSnapshot::new(
            environment,
            dir.path().into(),
            scratch,
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
        _home: home,
        mode,
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
            mode: fx.mode,
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

/// A repository names its own suite. DevOrder's is `bin/make regression`: the
/// runtime recorded it and it passed, but the old runner vocabulary could not
/// see the name and refused to close the goal (session `1c3bf9aa`).
///
/// The structured contract has no runner vocabulary: the model declares the
/// `tests` requirement together with the exact command whose run is its
/// evidence. Any repository name works, because the match is identity, not a
/// recognised word.
#[cfg(unix)]
#[tokio::test]
async fn a_suite_named_in_the_repositorys_own_words_resolves_the_goal() {
    use std::os::unix::fs::PermissionsExt;

    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": USER_REQUEST,
                    "acceptance": ["go test ./... 通过", "MCP 读写工具与页面能力一致"],
                    "task_relation": "continue",
                    "requirements": [{
                        "kind": "tests",
                        "text": "完整测试通过",
                        "target": "bin/make regression",
                        "source_quote": "完成后通过测试"
                    }]
                }),
            ),
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
                        {"id": "R1", "status": "done", "evidence": "bin/make regression"}
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
            // The quote does not occur in the user's message: the declaration
            // is invalid, deterministically.
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": STAGED_REQUEST,
                    "acceptance": ["文章发布可访问"],
                    "requirements": [{
                        "kind": "named",
                        "text": "文章发布",
                        "source_quote": "这是一句用户没有说过的话"
                    }]
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
            .find(|content| content.contains("source_quote"))
            .expect("the model sees the refusal");
        assert!(
            refusal.contains("does not occur in the user's message"),
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

/// Before the declaration is committed there is no goal row and no requirement
/// set: the runtime does not derive one from the user's prose. The pre-commit
/// reader must say that honestly and name the structured declaration path,
/// instead of presenting an extracted candidate as authority.
#[tokio::test]
async fn the_scope_tool_is_readable_before_commit() {
    let fx = fixture(
        vec![
            Scripted::Tool("get_goal_scope", serde_json::json!({})),
            Scripted::Text("范围已读取，继续处理。"),
        ],
        false,
    )
    .await;

    let _report = chat_with_request(&fx, STAGED_REQUEST, CancellationToken::new()).await;

    {
        let requests = fx.runtime.requests.lock().unwrap();
        let tools = tool_names(&requests[0]);
        assert!(
            tools.contains(&"get_goal_scope"),
            "the scope reader must be on the pre-commit surface: {tools:?}"
        );
        let result = requests[1]
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result),
                _ => None,
            })
            .find(|result| result.call_id.as_str() == "call-1")
            .expect("the scope result reaches the model");
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result
                .content
                .contains("No committed goal exists for this turn"),
            "{}",
            result.content
        );
        assert!(
            result
                .content
                .contains("does not extract requirements from prose")
                && result.content.contains("commit_task")
                && result.content.contains("source_quote"),
            "the reader names the declaration path instead of inventing scope: {}",
            result.content
        );
    }
    assert!(
        goal_records(&fx).await.is_empty(),
        "reading the scope before a declaration must not register anything"
    );
}

/// A task that still owes a running goal HAS a scope; it is only this turn
/// that has not adopted it yet. The reader must state that durable fact and
/// name the structured adoption path, not tell a model facing real requirements
/// that no goal exists and that it must commit the task again.
#[tokio::test]
async fn the_scope_reader_names_an_owed_goal_before_the_turn_adopts_it() {
    let mut script = pause_stage_one_script();
    script.extend([
        Scripted::Tool("get_goal_scope", serde_json::json!({})),
        Scripted::Text("这一项先记下，本回合不继续执行。"),
    ]);
    let fx = fixture(script, false).await;
    write_verifier(fx.dir.path());
    pause_a_goal(&fx).await;
    let paused = goal_records(&fx).await;
    let goal_id = paused[0].id.clone();
    assert!(paused[0].user_paused_at.is_some(), "{paused:?}");

    let _ = say(&fx, "这一项先记下，暂时不继续。").await;

    let requests = fx.runtime.requests.lock().unwrap();
    let result = requests
        .last()
        .expect("the turn reached the model")
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|part| match part {
            ContentPart::ToolResult { result } => Some(result),
            _ => None,
        })
        .find(|result| {
            result.content.contains("running goal") || result.content.contains("committed goal")
        })
        .expect("the scope result reaches the model");
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains(goal_id.as_str()),
        "the reader names the goal the task still owes: {}",
        result.content
    );
    assert!(
        result.content.contains("declare_intent"),
        "the reader names the structured adoption path: {}",
        result.content
    );
    assert!(
        !result
            .content
            .contains("No committed goal exists for this turn"),
        "a paused owed goal is not 'no goal for this task': {}",
        result.content
    );
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

/// Adoption is the act that gives this turn a scope to change. The writer must
/// be on the surface of the very next request — a model told to adopt but left
/// without the writer cannot record the user's own waiver.
#[tokio::test]
async fn adopting_an_owed_goal_adds_the_scope_writer_to_the_next_request() {
    let mut script = pause_stage_one_script();
    script.extend([
        Scripted::Tool(
            "declare_intent",
            serde_json::json!({
                "source_quote": "这一项先记下",
                "operations": [{"kind": "amend_current_task"}]
            }),
        ),
        Scripted::Text("已记下，本回合不继续执行。"),
    ]);
    let fx = fixture(script, false).await;
    write_verifier(fx.dir.path());
    pause_a_goal(&fx).await;

    let _ = say(&fx, "这一项先记下，暂时不继续。").await;

    {
        let requests = fx.runtime.requests.lock().unwrap();
        let after = requests.last().expect("the turn reached the model");
        let receipt = after
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result),
                _ => None,
            })
            .find(|result| result.content.starts_with("Recorded 1 operation(s)"))
            .expect("the declaration receipt reaches the model");
        assert!(
            receipt.content.contains("declare_scope"),
            "the receipt names the writer that adoption just made available: {}",
            receipt.content
        );
        let tools = tool_names(after);
        assert!(
            tools.contains(&"declare_scope"),
            "the adopting turn must be able to write the scope it adopted: {tools:?}"
        );
        assert!(
            tools.contains(&"get_goal_scope"),
            "the adopting turn must be able to read the scope it adopted: {tools:?}"
        );
    }
    let paused = goal_records(&fx).await;
    assert!(
        paused[0].user_paused_at.is_some(),
        "amending a paused goal does not release the pause: {paused:?}"
    );
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

/// The commit gate extracts one requirement per deliverable, so a request that
/// enumerates more than the old fixed cap of 20 must still be declarable. The
/// audited session's request extracted 132 requirements while the tool schema
/// advertised `maxItems: 20`; the model could never satisfy both. The bounded
/// payload stays, and related deliverables share one check, so the same request
/// is committable without letting an inflated extraction set grow the schema.
#[tokio::test]
async fn a_request_with_more_than_twenty_deliverables_can_be_committed() {
    let deliverables: Vec<String> = (1..=22)
        .map(|index| format!("模块{index:02} 能力"))
        .collect();
    let request: &'static str = Box::leak(
        format!(
            "完成以下功能，完成后通过测试：\n{}",
            deliverables
                .iter()
                .map(|item| format!("- {item}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
        .into_boxed_str(),
    );
    // 22 deliverables + the test demand in 19 grouped checks.
    let mut acceptance: Vec<String> = Vec::new();
    for pair in deliverables.chunks(2).take(4) {
        acceptance.push(pair.join(" 与 "));
    }
    acceptance.extend(deliverables.iter().skip(8).cloned());
    acceptance.push("完成后通过测试".to_string());
    assert!(
        acceptance.len() <= 20,
        "the declaration stays inside the bounded payload"
    );
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": request,
                    "acceptance": acceptance
                }),
            ),
            Scripted::Text("任务已声明，继续执行。"),
            Scripted::Text("先到这里。"),
            Scripted::Text("剩下的下次继续。"),
        ],
        false,
    )
    .await;

    let _report = chat_with_request(&fx, request, CancellationToken::new()).await;

    {
        let requests = fx.runtime.requests.lock().unwrap();
        let schema_limit = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == "commit_task")
            .map(|tool| tool.input_schema["properties"]["acceptance"]["maxItems"].clone())
            .expect("a pre-commit surface offers commit_task");
        assert_eq!(
            schema_limit,
            serde_json::json!(20),
            "the acceptance payload keeps its fixed bound"
        );
        let commit_result = requests[1]
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result),
                _ => None,
            })
            .find(|result| result.call_id.as_str() == "call-1")
            .expect("the commit result reaches the model");
        assert!(
            !commit_result.is_error && commit_result.content.contains("Execution task committed"),
            "grouped checks cover 23 requirements without exceeding 20: {}",
            commit_result.content
        );
    }
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "exactly one durable task");
    assert_eq!(goals[0].objective, request);
}

/// The other honest recovery: the model fixes the refused declaration and
/// re-declares the same scope. The first call quotes words the user never
/// said, so the runtime refuses it deterministically; the corrected call
/// declares every deliverable with a verbatim quote and is accepted.
#[tokio::test]
async fn a_model_recovers_from_a_refusal_and_registers_the_full_scope() {
    let acceptance = serde_json::json!([
        "文章发布可访问",
        "评论回复可提交且可见",
        "内容权限校验覆盖作者与管理员",
        format!("{} 通过", test_runner_label())
    ]);
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": STAGED_REQUEST,
                    "acceptance": acceptance.clone(),
                    "requirements": [
                        {"kind": "named", "text": "文章发布", "source_quote": "文章发布"},
                        {"kind": "named", "text": "评论回复", "source_quote": "评论回复"},
                        {"kind": "named", "text": "内容权限校验",
                         "source_quote": "用户没有这样说过"}
                    ]
                }),
            ),
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": STAGED_REQUEST,
                    "acceptance": acceptance,
                    "requirements": [
                        {"kind": "named", "text": "文章发布", "source_quote": "文章发布"},
                        {"kind": "named", "text": "评论回复", "source_quote": "评论回复"},
                        {"kind": "named", "text": "内容权限校验",
                         "source_quote": "内容权限校验"},
                        {"kind": "tests", "text": "测试通过", "target": test_runner_label(), "source_quote": "完成后通过测试"}
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
                        {"id": "R1", "status": "done", "evidence": "发布链路可访问"},
                        {"id": "R2", "status": "done", "evidence": "回复可见"},
                        {"id": "R3", "status": "done", "evidence": "作者/管理员/只读"},
                        {"id": "R4", "status": "done", "evidence": test_runner_label()}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;

    let report = chat_with_request(&fx, STAGED_REQUEST, CancellationToken::new()).await;

    {
        let requests = fx.runtime.requests.lock().unwrap();
        let refusal = requests[1]
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result),
                _ => None,
            })
            .find(|result| result.call_id.as_str() == "call-1")
            .expect("the first declaration's result reaches the model");
        assert!(
            refusal.is_error && refusal.content.contains("source_quote"),
            "the first declaration is refused for its quote: {}",
            refusal.content
        );
    }

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
    let requirements = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&goals[0].id)
        .await
        .unwrap();
    assert_eq!(
        requirements.len(),
        4,
        "the accepted call's four requirements are the recorded set: {requirements:?}"
    );
    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
}

/// The reported first-submission failure: the user wrote a requirement with
/// full-width punctuation, the model copied it with ASCII punctuation, and the
/// literal comparison refused a real statement. A span names the line; the
/// runtime cuts the user's own bytes, so the first declaration succeeds and the
/// recorded quote carries the punctuation the user actually wrote.
#[tokio::test]
async fn a_first_declaration_accepts_a_line_span_and_keeps_the_users_punctuation() {
    const REQUEST: &str = "请完成两个交付！\n一、把结果写进 report.txt。\n二、页面文案保持中文。";
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": REQUEST,
                    "acceptance": ["report.txt 存在", "页面文案为中文"],
                    "requirements": [
                        {"kind": "artifact", "text": "report.txt 存在", "target": "report.txt",
                         "source_span": {"start_line": 2, "end_line": 2}},
                        {"kind": "named", "text": "页面文案为中文",
                         "source_span": {"start_line": 3, "end_line": 3}}
                    ]
                }),
            ),
            Scripted::Tool(
                "write_file",
                serde_json::json!({"path": "report.txt", "content": "done\n"}),
            ),
            Scripted::run_tests(),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "两个交付完成",
                    "verification": {"status": "passed", "detail": test_runner_label()},
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "report.txt 存在"},
                        {"id": "R2", "status": "done", "evidence": "页面文案保持中文"}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;

    let report = chat_with_request(&fx, REQUEST, CancellationToken::new()).await;

    assert_eq!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Completed,
        "{report:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "the first declaration registers one goal");
    let requirements = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&goals[0].id)
        .await
        .unwrap();
    assert_eq!(requirements.len(), 2);
    assert_eq!(
        requirements[0].source_quote, "一、把结果写进 report.txt。",
        "the stored quote is the user's exact line"
    );
    assert_eq!(requirements[1].source_quote, "二、页面文案保持中文。");
}

/// A span outside the message is refused as invalid arguments and creates no
/// goal. The corrected call is the one that registers the scope; there is no
/// half-created goal, duplicate identity or requirement from the refused call.
#[tokio::test]
async fn a_rejected_span_registers_nothing_and_the_corrected_call_registers_once() {
    const REQUEST: &str = "请创建 stage-1.txt！\n完成后确认。";
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": REQUEST,
                    "acceptance": ["stage-1.txt 存在"],
                    "requirements": [{
                        "kind": "artifact", "text": "stage-1.txt 存在", "target": "stage-1.txt",
                        "source_span": {"start_line": 3, "end_line": 3}
                    }]
                }),
            ),
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": REQUEST,
                    "acceptance": ["stage-1.txt 存在"],
                    "requirements": [{
                        "kind": "artifact", "text": "stage-1.txt 存在", "target": "stage-1.txt",
                        "source_span": {"start_line": 1, "end_line": 1}
                    }]
                }),
            ),
            Scripted::Tool(
                "write_file",
                serde_json::json!({"path": "stage-1.txt", "content": "stage 1\n"}),
            ),
            Scripted::run_tests(),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "stage-1.txt 已创建",
                    "verification": {"status": "passed", "detail": test_runner_label()},
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "stage-1.txt 存在"}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;

    let report = chat_with_request(&fx, REQUEST, CancellationToken::new()).await;

    {
        let requests = fx.runtime.requests.lock().unwrap();
        let refusal = requests[1]
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result),
                _ => None,
            })
            .find(|result| result.call_id.as_str() == "call-1")
            .expect("the first declaration's result reaches the model");
        assert!(
            refusal.is_error
                && refusal
                    .content
                    .contains("outside the user's message, which has 2 line(s)"),
            "the refusal names the span fact: {}",
            refusal.content
        );
    }

    assert_eq!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Completed,
        "{report:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(
        goals.len(),
        1,
        "the refused call created no goal; the corrected call created exactly one"
    );
    let requirements = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&goals[0].id)
        .await
        .unwrap();
    assert_eq!(requirements.len(), 1);
    assert_eq!(requirements[0].source_quote, "请创建 stage-1.txt！");
    assert_eq!(requirements[0].id, "R1", "one identity, minted once");
}

/// The span form also serves a supplementary requirement added after the
/// commit. `declare_scope` resolves the span against the newest user input
/// before serde parses the declaration, so the durable event carries the
/// user's exact bytes rather than the coordinate.
#[tokio::test]
async fn a_supplementary_requirement_can_name_a_span_of_the_same_message() {
    const REQUEST: &str = "创建 a.txt。\n再创建 b.txt！";
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": REQUEST,
                    "acceptance": ["a.txt 存在", "b.txt 存在"],
                    "requirements": [{
                        "kind": "artifact", "text": "a.txt 存在", "target": "a.txt",
                        "source_span": {"start_line": 1, "end_line": 1}
                    }]
                }),
            ),
            Scripted::Tool(
                "declare_scope",
                serde_json::json!({
                    "operations": [{
                        "op": "add", "kind": "artifact", "text": "b.txt 存在",
                        "target": "b.txt",
                        "source_span": {"start_line": 2, "end_line": 2}
                    }]
                }),
            ),
            Scripted::Tool(
                "write_file",
                serde_json::json!({"path": "a.txt", "content": "a\n"}),
            ),
            Scripted::Tool(
                "write_file",
                serde_json::json!({"path": "b.txt", "content": "b\n"}),
            ),
            Scripted::run_tests(),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "两个文件都已创建",
                    "verification": {"status": "passed", "detail": test_runner_label()},
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "created a.txt"},
                        {"id": "R2", "status": "done", "evidence": "created b.txt"}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;

    let report = chat_with_request(&fx, REQUEST, CancellationToken::new()).await;

    assert_eq!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Completed,
        "{report:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    let requirements = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&goals[0].id)
        .await
        .unwrap();
    assert_eq!(requirements.len(), 2);
    assert_eq!(requirements[0].source_quote, "创建 a.txt。");
    assert_eq!(
        requirements[1].source_quote, "再创建 b.txt！",
        "the supplementary requirement keeps the user's punctuation"
    );
}

/// A first declaration that omits the conditional `target` is refused as
/// invalid arguments, not as an execution failure. The refusal names the exact
/// element and what the field must hold, registers no goal, and the corrected
/// call registers exactly one goal identity. Real sessions failed here because
/// the model-visible schema said only `content`/`command`/`tests` needed
/// `target`; this locks the schema contract's observable consequence end to
/// end.
#[tokio::test]
async fn a_declaration_that_omits_the_required_target_is_refused_then_registers_once() {
    const REQUEST: &str = "创建 stage-1.txt，完成后确认。";
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": REQUEST,
                    "acceptance": ["stage-1.txt 存在"],
                    "requirements": [{
                        "kind": "artifact",
                        "text": "stage-1.txt 存在",
                        "source_quote": "创建 stage-1.txt"
                    }]
                }),
            ),
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": REQUEST,
                    "acceptance": ["stage-1.txt 存在"],
                    "requirements": [{
                        "kind": "artifact",
                        "text": "stage-1.txt 存在",
                        "target": "stage-1.txt",
                        "source_quote": "创建 stage-1.txt"
                    }]
                }),
            ),
            write_stage(),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "stage-1.txt 已创建",
                    "verification": {
                        "status": "not_applicable",
                        "detail": "the artifact is checked against the workspace"
                    },
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "created stage-1.txt"}
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
            &spec_for(&fx, REQUEST),
            vec![ContentPart::Text {
                text: REQUEST.into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    {
        let requests = fx.runtime.requests.lock().unwrap();
        let refusal = requests[1]
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result),
                _ => None,
            })
            .find(|result| result.call_id.as_str() == "call-1")
            .expect("the first declaration's refusal reaches the model");
        assert!(refusal.is_error, "{}", refusal.content);
        assert!(
            refusal.content.contains("`requirements[0].target`")
                && refusal.content.contains("workspace-relative path")
                && refusal.content.contains("Fix:"),
            "the refusal must name the element, what it holds and the fix: {}",
            refusal.content
        );
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == leveler_agent::COMMIT_INVALID_DECLARATION_KIND
                    && detail.contains("requirements[0].target")
        )),
        "the refusal is an invalid declaration, not an execution failure: {events:?}"
    );
    assert_eq!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Completed,
        "the corrected declaration must still execute the task: {report:?}"
    );
    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Completed,
        "{report:?}"
    );
    assert_eq!(
        fx.runtime.calls.load(Ordering::SeqCst),
        5,
        "refusal, corrected declaration, work, completion and delivery: no hidden auto-retry"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(
        goals.len(),
        1,
        "the corrected declaration creates exactly one goal identity"
    );
    let requirements = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&goals[0].id)
        .await
        .unwrap();
    assert_eq!(
        requirements.len(),
        1,
        "only the corrected call's requirement is recorded: {requirements:?}"
    );
    assert_eq!(
        requirements[0].target.as_deref(),
        Some("stage-1.txt"),
        "the accepted declaration's target is the durable contract"
    );
    let log = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirement_log(&goals[0].id)
        .await
        .unwrap();
    assert_eq!(
        log.len(),
        1,
        "the refused call recorded nothing; the corrected call recorded one add: {log:?}"
    );
    assert_eq!(
        log[0].event.operation,
        leveler_lifecycle::ScopeOperationKind::Add
    );
    let verification = report.verification.expect("a verification report");
    assert_eq!(
        verification.status,
        leveler_lifecycle::VerificationStatus::Verified,
        "the artifact requirement is checked against the workspace: {verification:?}"
    );
}

/// The refusal alone leaves no half-registered task behind: no goal row and no
/// requirement history. A later turn starts clean.
#[tokio::test]
async fn an_artifact_without_a_target_creates_no_half_registered_goal() {
    const REQUEST: &str = "创建 stage-1.txt，完成后确认。";
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": REQUEST,
                    "acceptance": ["stage-1.txt 存在"],
                    "requirements": [{
                        "kind": "artifact",
                        "text": "stage-1.txt 存在",
                        "source_quote": "创建 stage-1.txt"
                    }]
                }),
            ),
            Scripted::Text("声明未通过，我没有创建任务。"),
        ],
        false,
    )
    .await;
    let mut events = Vec::new();

    let report = fx
        .coding
        .chat(
            &fx.session,
            &spec_for(&fx, REQUEST),
            vec![ContentPart::Text {
                text: REQUEST.into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    {
        let requests = fx.runtime.requests.lock().unwrap();
        let refusal = requests[1]
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result),
                _ => None,
            })
            .find(|result| result.call_id.as_str() == "call-1")
            .expect("the refusal reaches the model");
        assert!(refusal.is_error, "{}", refusal.content);
        assert!(
            refusal.content.contains("`requirements[0].target`")
                && refusal.content.contains("workspace-relative path")
                && refusal.content.contains("Fix:"),
            "{}",
            refusal.content
        );
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == leveler_agent::COMMIT_INVALID_DECLARATION_KIND
                    && detail.contains("requirements[0].target")
        )),
        "{events:?}"
    );
    assert!(
        goal_records(&fx).await.is_empty(),
        "a refusal is not a partially created task"
    );
    assert_eq!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Completed,
        "the turn answered; the refusal is about the declaration, not the user's request: {report:?}"
    );
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Answered);
}

/// Every requirement kind the schema advertises is committable in one mixed
/// declaration. The runtime's target rule is conditional, so the five
/// mechanically anchored kinds carry a target and the five semantic kinds do
/// not; none is narrowed or rejected for lacking a path.
#[tokio::test]
async fn every_requirement_kind_commits_in_one_mixed_declaration() {
    const REQUEST: &str = "完成全部交付：a.txt 必须存在；b.txt 必须不存在；\
        运行 sh check.sh 检查；发布功能为命名交付物；c.txt 包含文字 hello；\
        运行 sh run-tests.sh 测试；禁止修改 Cargo.toml；保持现有基线；\
        先阅读源码；保持中文注释。完成后确认。";
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": REQUEST,
                    "acceptance": ["全部交付完成"],
                    "requirements": [
                        {"kind": "artifact", "text": "a.txt 必须存在",
                         "target": "a.txt", "source_quote": "a.txt 必须存在"},
                        {"kind": "artifact_absent", "text": "b.txt 必须不存在",
                         "target": "b.txt", "source_quote": "b.txt 必须不存在"},
                        {"kind": "command", "text": "运行 sh check.sh 检查",
                         "target": "sh check.sh", "source_quote": "运行 sh check.sh 检查"},
                        {"kind": "named", "text": "发布功能",
                         "source_quote": "发布功能为命名交付物"},
                        {"kind": "content", "text": "hello",
                         "target": "c.txt", "source_quote": "c.txt 包含文字 hello"},
                        {"kind": "tests", "text": "运行 sh run-tests.sh 测试",
                         "target": "sh run-tests.sh", "source_quote": "运行 sh run-tests.sh 测试"},
                        {"kind": "forbidden", "text": "禁止修改 Cargo.toml",
                         "source_quote": "禁止修改 Cargo.toml"},
                        {"kind": "baseline", "text": "保持现有基线",
                         "source_quote": "保持现有基线"},
                        {"kind": "preparation", "text": "先阅读源码",
                         "source_quote": "先阅读源码"},
                        {"kind": "constraint", "text": "保持中文注释",
                         "source_quote": "保持中文注释"}
                    ]
                }),
            ),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "blocked",
                    "summary": "scope recorded; execution is outside this contract test"
                }),
            ),
        ],
        false,
    )
    .await;
    let mut events = Vec::new();

    let _report = fx
        .coding
        .chat(
            &fx.session,
            &spec_for(&fx, REQUEST),
            vec![ContentPart::Text {
                text: REQUEST.into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        !events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, .. }
                if leveler_agent::is_commit_refusal_kind(kind)
        )),
        "every advertised kind must commit: {events:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    let requirements = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&goals[0].id)
        .await
        .unwrap();
    let recorded: Vec<(&str, Option<&str>)> = requirements
        .iter()
        .map(|item| (item.kind.as_str(), item.target.as_deref()))
        .collect();
    assert_eq!(
        recorded,
        [
            ("artifact", Some("a.txt")),
            ("artifact_absent", Some("b.txt")),
            ("command", Some("sh check.sh")),
            ("named", None),
            ("content", Some("c.txt")),
            ("tests", Some("sh run-tests.sh")),
            ("forbidden", None),
            ("baseline", None),
            ("preparation", None),
            ("constraint", None),
        ],
        "the committed set preserves every kind and only the anchored kinds' targets"
    );
}

/// The committed requirement set is readable by stable id. What the model
/// declared at commit is what the completion gate reads; a read cannot invent
/// or drop a requirement because it never re-parses the objective.
#[tokio::test]
async fn the_goal_scope_reads_the_declared_requirements_by_id() {
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": STAGED_REQUEST,
                    "acceptance": [
                        "文章发布可访问",
                        "评论回复可提交且可见",
                        "内容权限校验覆盖作者与管理员",
                        "sh run-tests.sh 通过"
                    ],
                    "requirements": [
                        {"kind": "named", "text": "文章发布", "source_quote": "文章发布"},
                        {"kind": "named", "text": "评论回复", "source_quote": "评论回复"},
                        {"kind": "named", "text": "内容权限校验", "source_quote": "内容权限校验"}
                    ]
                }),
            ),
            Scripted::Tool(
                "get_goal_scope",
                serde_json::json!({"offset": 0, "limit": 20, "history": true}),
            ),
            Scripted::run_tests(),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "三项社区业务完成",
                    "verification": {"status": "passed", "detail": test_runner_label()},
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "发布链路可访问"},
                        {"id": "R2", "status": "done", "evidence": "回复可见"},
                        {"id": "R3", "status": "done", "evidence": "作者/管理员/只读"}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;

    let report = chat_with_request(&fx, STAGED_REQUEST, CancellationToken::new()).await;

    {
        let requests = fx.runtime.requests.lock().unwrap();
        assert!(
            tool_names(&requests[0]).contains(&"get_goal_scope"),
            "the scope reader is advertised on the Chat surface: {:?}",
            tool_names(&requests[0])
        );
        let scope = requests
            .iter()
            .flat_map(|request| request.messages.iter())
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result.content.clone()),
                _ => None,
            })
            .find(|content| content.contains("Goal scope:"))
            .expect("the model reads the committed scope");
        for (id, text) in [
            ("R1", "文章发布"),
            ("R2", "评论回复"),
            ("R3", "内容权限校验"),
        ] {
            assert!(
                scope.contains(&format!("- {id} [deliverable] active: {text}")),
                "the scope names {id} as declared: {scope}"
            );
        }
    }

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "the declaration registers one goal");
    assert_eq!(goals[0].objective, STAGED_REQUEST);
    assert!(
        goals[0].state == leveler_storage::GoalState::Settled,
        "the declared set can be accounted and completed: {:?}",
        goals[0].state
    );
    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
}

/// The accepted completion carries the runtime's structured verification, not
/// the model's prose: the record names the requirement it belongs to and the
/// exact execution that proved it. The model's `verification.status` is a
/// declaration; these fields are the evidence.
#[tokio::test]
async fn an_accepted_completion_carries_the_structured_verification_records() {
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": STAGED_REQUEST,
                    "acceptance": [format!("{} 通过", test_runner_label())],
                    "requirements": [
                        {"kind": "command", "text": "验证命令通过", "target": test_runner_label(), "source_quote": "完成后通过测试"}
                    ]
                }),
            ),
            Scripted::run_tests(),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "测试通过",
                    "verification": {"status": "passed", "detail": test_runner_label()},
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "call_id:call-2"}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;

    let report = chat_with_request(&fx, STAGED_REQUEST, CancellationToken::new()).await;
    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
    let verification = report
        .verification
        .expect("an accepted completion carries its verification report");
    assert_eq!(
        verification.status,
        leveler_lifecycle::VerificationStatus::Verified,
        "the declared command requirement has a current passing run: {verification:?}"
    );
    assert_eq!(verification.records.len(), 1);
    let record = &verification.records[0];
    assert_eq!(record.requirement_id, "R1");
    assert_eq!(record.kind, "command");
    assert_eq!(
        record.state,
        leveler_lifecycle::VerificationRecordState::Verified
    );
    assert_eq!(record.call_id.as_deref(), Some("call-2"));
    assert_eq!(record.command.as_deref(), Some(test_runner_label()));
    assert_eq!(record.exit_code, Some(0));
    assert!(record.model_step > 0);
}

/// `content` compares the requirement's `text` to a line of the target file.
/// A model that records the requirement as prose cannot satisfy it by
/// rewriting the file; the refusal must name the declaration fix, and an
/// `amend` to the literal must then complete on the same evidence.
#[tokio::test]
async fn a_content_requirement_refusal_names_the_declaration_fix() {
    const REQUEST: &str = "创建 stage-1.txt，内容为一行 stage 1，完成后确认。";
    fn complete() -> Scripted {
        Scripted::Tool(
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "stage-1.txt 已创建",
                "verification": {
                    "status": "not_applicable",
                    "detail": "the content requirement is checked against the workspace"
                },
                "requirements": [
                    {"id": "R1", "status": "done", "evidence": "created stage-1.txt"}
                ]
            }),
        )
    }
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": REQUEST,
                    "acceptance": ["stage-1.txt 内容为一行 stage 1"],
                    "requirements": [{
                        "kind": "content",
                        "text": "stage-1.txt 的内容为一行 stage 1",
                        "target": "stage-1.txt",
                        "source_quote": "内容为一行 stage 1"
                    }]
                }),
            ),
            Scripted::Tool(
                "write_file",
                serde_json::json!({"path": "stage-1.txt", "content": "stage 1\n"}),
            ),
            complete(),
            Scripted::Tool(
                "declare_scope",
                serde_json::json!({
                    "operations": [{
                        "op": "amend",
                        "requirement_id": "R1",
                        "text": "stage 1",
                        "source_quote": "内容为一行 stage 1"
                    }]
                }),
            ),
            complete(),
        ],
        false,
    )
    .await;

    let mut events: Vec<leveler_engine::EngineEvent> = Vec::new();
    let report = fx
        .coding
        .chat(
            &fx.session,
            &spec_for(&fx, REQUEST),
            vec![ContentPart::Text {
                text: REQUEST.into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == "requirements_refused"
                    && detail.contains("R1")
                    && detail.contains("declare_scope")
        )),
        "the refusal names the declaration fix, not only the missing fact: {events:?}"
    );
    assert_eq!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Completed,
        "the amended declaration completes on the evidence that was already there: {report:?}"
    );
    let verification = report.verification.expect("a verification report");
    assert_eq!(
        verification.status,
        leveler_lifecycle::VerificationStatus::Verified
    );
    let goals = goal_records(&fx).await;
    let items = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&goals[0].id)
        .await
        .unwrap();
    assert_eq!(
        items[0].text, "stage 1",
        "the declaration now holds the literal the gate compares"
    );
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

/// A completion the runtime cannot mechanically check still completes: it is
/// accepted as unverified and the terminal says exactly which requirement
/// rests on the model's account. An unverified boundary is not a gate that
/// waits for the user to confirm anything.
#[tokio::test]
async fn an_unverified_completion_is_accepted_and_records_the_boundary() {
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": STAGED_REQUEST,
                    "acceptance": ["文章发布可访问"],
                    "requirements": [
                        {"kind": "named", "text": "文章发布", "source_quote": "文章发布"}
                    ]
                }),
            ),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "文章发布完成",
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "发布链路已可用"}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;

    let report = chat_with_request(&fx, STAGED_REQUEST, CancellationToken::new()).await;
    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
    let verification = report
        .verification
        .expect("an accepted completion carries its verification report");
    assert_eq!(
        verification.status,
        leveler_lifecycle::VerificationStatus::Unverified
    );
    assert_eq!(verification.records.len(), 1);
    let record = &verification.records[0];
    assert_eq!(record.requirement_id, "R1");
    assert_eq!(
        record.state,
        leveler_lifecycle::VerificationRecordState::Unverified
    );
    assert!(record.call_id.is_none());
    assert!(record.command.is_none());
}

/// The requirement contract is language-independent. The model reads the
/// user's message and declares the same structure; the runtime only checks the
/// quote, the identity and the execution record. Chinese, English, Japanese,
/// German, Spanish and mixed input take one path because no word list exists.
#[tokio::test]
async fn the_same_requirement_contract_holds_in_any_language() {
    struct Case {
        label: &'static str,
        objective: &'static str,
        quote: &'static str,
        requirement: &'static str,
    }
    let cases = [
        Case {
            label: "zh",
            objective: "完成文章发布，完成后运行 sh run-tests.sh 并确保通过",
            quote: "完成后运行 sh run-tests.sh 并确保通过",
            requirement: "sh run-tests.sh 通过",
        },
        Case {
            label: "en",
            objective: "Implement the publisher and run sh run-tests.sh before calling it done.",
            quote: "run sh run-tests.sh before calling it done",
            requirement: "sh run-tests.sh passes",
        },
        Case {
            label: "ja",
            objective: "公開機能を実装し、完了前に sh run-tests.sh を実行して成功させてください。",
            quote: "完了前に sh run-tests.sh を実行して成功させてください",
            requirement: "sh run-tests.sh が成功する",
        },
        Case {
            label: "de",
            objective: "Implementiere den Publisher und führe vor dem Abschluss sh run-tests.sh aus.",
            quote: "führe vor dem Abschluss sh run-tests.sh aus",
            requirement: "sh run-tests.sh ist erfolgreich",
        },
        Case {
            label: "es",
            objective: "Implementa el publicador y ejecuta sh run-tests.sh antes de terminar.",
            quote: "ejecuta sh run-tests.sh antes de terminar",
            requirement: "sh run-tests.sh pasa",
        },
        Case {
            label: "mixed",
            objective: "实现发布功能 then run sh run-tests.sh 确保通过",
            quote: "run sh run-tests.sh 确保通过",
            requirement: "sh run-tests.sh 通过",
        },
    ];

    for case in cases {
        let fx = fixture(
            vec![
                Scripted::Tool(
                    "commit_task",
                    serde_json::json!({
                        "objective": case.objective,
                        "acceptance": [case.requirement],
                        "requirements": [{
                            "kind": "tests",
                            "text": case.requirement,
                            "target": test_runner_label(),
                            "source_quote": case.quote
                        }]
                    }),
                ),
                Scripted::run_tests(),
                Scripted::Tool(
                    "update_goal",
                    serde_json::json!({
                        "status": "complete",
                        "summary": "done",
                        "verification": {"status": "passed", "detail": test_runner_label()},
                        "requirements": [
                            {"id": "R1", "status": "done", "evidence": "call_id:call-2"}
                        ]
                    }),
                ),
            ],
            false,
        )
        .await;

        let report = chat_with_request(&fx, case.objective, CancellationToken::new()).await;
        assert_eq!(
            report.outcome,
            leveler_lifecycle::TaskOutcome::Completed,
            "{}: {report:?}",
            case.label
        );
        let verification = report
            .verification
            .unwrap_or_else(|| panic!("{}: no verification report", case.label));
        // The declared command ran and passed in every language. Its coverage
        // stays unverified because no structured test result exists, not
        // because of the language it was declared in.
        assert_eq!(
            verification.status,
            leveler_lifecycle::VerificationStatus::Unverified,
            "{}: {verification:?}",
            case.label
        );
        assert_eq!(verification.records.len(), 1, "{}", case.label);
        assert_eq!(
            verification.records[0].state,
            leveler_lifecycle::VerificationRecordState::Unverified,
            "{}",
            case.label
        );
        assert_eq!(
            verification.records[0].command.as_deref(),
            Some(test_runner_label()),
            "{}",
            case.label
        );
    }
}

/// No language's word for "tests" is evidence. A summary that claims the
/// suite passed produces no run record, so the completion stays unverified.
#[tokio::test]
async fn a_success_claim_in_any_language_is_not_verification() {
    let summaries = [
        "测试全部通过",
        "All tests pass",
        "テストはすべて成功しました",
        "Alle Tests sind grün",
        "Todas las pruebas pasan",
        "tests 全部 pass",
    ];
    for summary in summaries {
        let fx = fixture(
            vec![
                Scripted::Tool(
                    "commit_task",
                    serde_json::json!({
                        "objective": STAGED_REQUEST,
                        "acceptance": ["文章发布可访问"]
                    }),
                ),
                write_stage(),
                Scripted::Tool(
                    "update_goal",
                    serde_json::json!({
                        "status": "complete",
                        "summary": summary
                    }),
                ),
            ],
            false,
        )
        .await;

        let report = chat_with_request(&fx, STAGED_REQUEST, CancellationToken::new()).await;
        assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
        let verification = report
            .verification
            .unwrap_or_else(|| panic!("{summary}: no verification report"));
        assert_eq!(
            verification.status,
            leveler_lifecycle::VerificationStatus::Unverified,
            "{summary}: a prose claim created evidence"
        );
        assert!(verification.records.is_empty(), "{summary}");
    }
}

/// A passing command proves an execution, not a delivered requirement. A
/// model that records no requirement — field absent or explicitly empty — and
/// then cites `echo ready` must not leave the terminal as
/// `Completed + Verified` with an empty record set: nothing was verified.
#[tokio::test]
async fn a_passing_command_cannot_verify_an_undeclared_requirement() {
    const REQUEST: &str = "创建一个文件，然后运行 echo ready 确认。";
    for requirements in [None, Some(serde_json::json!([]))] {
        let mut commit = serde_json::json!({
            "objective": REQUEST,
            "acceptance": ["echo ready 输出 ready"]
        });
        if let Some(requirements) = requirements.clone() {
            commit["requirements"] = requirements;
        }
        let fx = fixture(
            vec![
                Scripted::Tool("commit_task", commit),
                write_stage(),
                Scripted::Tool(
                    "run_command",
                    serde_json::json!({
                        "program": "echo",
                        "args": ["ready"],
                        "reason": "confirm the workspace state"
                    }),
                ),
                Scripted::Tool(
                    "update_goal",
                    serde_json::json!({
                        "status": "complete",
                        "summary": "文件已创建，echo ready 通过",
                        "verification": {"status": "passed", "detail": "echo ready"}
                    }),
                ),
            ],
            false,
        )
        .await;

        let report = chat_with_request(&fx, REQUEST, CancellationToken::new()).await;
        assert_eq!(
            report.outcome,
            leveler_lifecycle::TaskOutcome::Completed,
            "{requirements:?}: the work still finishes"
        );
        let verification = report
            .verification
            .unwrap_or_else(|| panic!("{requirements:?}: no verification report"));
        assert_eq!(
            verification.status,
            leveler_lifecycle::VerificationStatus::Unverified,
            "{requirements:?}: a passing command verified an undeclared requirement"
        );
        assert!(
            verification.records.is_empty(),
            "{requirements:?}: no requirement produced a record"
        );
    }
}

/// A requirement that the runtime can check mechanically still reaches
/// Verified, with the record naming the requirement and the evidence.
#[tokio::test]
async fn a_declared_artifact_requirement_is_verified_by_the_workspace() {
    const REQUEST: &str = "创建 stage-1.txt，完成后确认。";
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": REQUEST,
                    "acceptance": ["stage-1.txt 存在"],
                    "requirements": [{
                        "kind": "artifact",
                        "text": "stage-1.txt 存在",
                        "target": "stage-1.txt",
                        "source_quote": "创建 stage-1.txt"
                    }]
                }),
            ),
            write_stage(),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "stage-1.txt 已创建",
                    "verification": {
                        "status": "not_applicable",
                        "detail": "the artifact is checked against the workspace"
                    },
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "created stage-1.txt"}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;

    let report = chat_with_request(&fx, REQUEST, CancellationToken::new()).await;
    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
    let verification = report.verification.expect("report");
    assert_eq!(
        verification.status,
        leveler_lifecycle::VerificationStatus::Verified,
        "{verification:?}"
    );
    assert_eq!(verification.records.len(), 1);
    assert_eq!(verification.records[0].kind, "artifact");
    assert_eq!(
        verification.records[0].state,
        leveler_lifecycle::VerificationRecordState::Verified
    );
}

/// A semantic requirement the runtime cannot check is delivered, but its
/// report says so: `named` is unverified, never silently verified.
#[tokio::test]
async fn a_semantic_requirement_completes_unverified() {
    const REQUEST: &str = "实现文章发布，完成后确认。";
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": REQUEST,
                    "acceptance": ["文章发布可用"],
                    "requirements": [{
                        "kind": "named",
                        "text": "文章发布",
                        "source_quote": "实现文章发布"
                    }]
                }),
            ),
            Scripted::Text("文章发布已实现。"),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "文章发布已实现",
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "implemented the publish flow"}
                    ]
                }),
            ),
        ],
        false,
    )
    .await;

    let report = chat_with_request(&fx, REQUEST, CancellationToken::new()).await;
    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
    let verification = report.verification.expect("report");
    assert_eq!(
        verification.status,
        leveler_lifecycle::VerificationStatus::Unverified
    );
    assert_eq!(verification.records.len(), 1);
    assert_eq!(verification.records[0].kind, "deliverable");
    assert_eq!(
        verification.records[0].state,
        leveler_lifecycle::VerificationRecordState::Unverified
    );
}

#[tokio::test]
async fn a_long_p2_request_registers_its_declared_requirements() {
    const P2: &str = include_str!("../../../testdata/regression/p2_scope_classification.zh-CN.md");
    // More than 20 real deliverables fit the unchanged 20-item acceptance cap
    // by grouping; the cap is a payload bound, not a scope bound.
    let acceptance: Vec<String> = (1..=24)
        .map(|index| format!("本轮交付物 {index} 已交付并可用"))
        .collect::<Vec<_>>()
        .chunks(3)
        .map(|group| group.join("、"))
        .collect();
    let mut acceptance = acceptance;
    acceptance.push("迁移脚本 migrations/0007_billing.sql 已创建".to_string());
    acceptance.push("cargo test -p billing 通过".to_string());

    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": P2,
                    "acceptance": acceptance,
                    "requirements": [
                        {"kind": "baseline",
                         "text": "P0/P1 的租户、发票与基线能力已上线",
                         "source_quote": "以下能力已经上线，本阶段只在其上扩展，不重做"},
                        {"kind": "preparation",
                         "text": "阅读计费与发票代码",
                         "source_quote": "阅读 `crates/billing/src/tenant.rs` 与 `crates/finance/src/invoice.rs`"},
                        {"kind": "constraint",
                         "text": "所有新增代码必须有中文注释",
                         "source_quote": "所有新增代码必须有中文注释"},
                        {"kind": "named",
                         "text": "订阅计划管理",
                         "source_quote": "实现订阅计划管理"},
                        {"kind": "artifact",
                         "text": "迁移脚本 migrations/0007_billing.sql",
                         "target": "migrations/0007_billing.sql",
                         "source_quote": "交付必须包含迁移脚本 `migrations/0007_billing.sql`"},
                        {"kind": "tests",
                         "text": "cargo test -p billing 通过",
                         "target": "cargo test -p billing",
                         "source_quote": "完成后运行 `cargo test -p billing` 并确保通过"}
                    ]
                }),
            ),
            Scripted::Text("已按声明的要求注册任务。"),
        ],
        false,
    )
    .await;

    let _report = chat_with_request(&fx, P2, CancellationToken::new()).await;

    {
        let requests = fx.runtime.requests.lock().unwrap();
        let tool_results: Vec<String> = requests
            .iter()
            .flat_map(|request| request.messages.iter())
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result.content.clone()),
                _ => None,
            })
            .collect();
        let receipt = tool_results
            .iter()
            .find(|content| content.contains("Execution task committed"))
            .expect("the declaration is recorded");
        for expected in [
            "[baseline]",
            "[preparation]",
            "[constraint]",
            "[deliverable]",
            "[artifact]",
            "[tests]",
            "订阅计划管理",
            "migrations/0007_billing.sql",
            "所有新增代码必须有中文注释",
        ] {
            assert!(
                receipt.contains(expected),
                "the receipt lacks `{expected}`: {receipt}"
            );
        }
        assert!(
            !tool_results
                .iter()
                .any(|content| content.contains("could not record the task")),
            "a valid structured declaration must register: {tool_results:#?}"
        );
    }

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "the declaration registers one goal");
    assert_eq!(goals[0].objective, P2.trim());
}

fn plan_shrink_objective() -> &'static str {
    "## 本轮交付物\n\n\
     1. 实现订阅计费。\n\
     2. 实现发票管理。\n\
     3. 实现财务对账。"
}

/// A plan is the model's own intent record. It may shrink — the incident's
/// model replaced a ten-step billing/finance plan with six refund/close steps
/// — but that must not narrow the user's scope, retire a deliverable, or turn
/// an incomplete goal into a completed one.
#[tokio::test]
async fn shrinking_the_plan_does_not_narrow_the_goals_scope() {
    let objective = plan_shrink_objective();
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": objective,
                    "acceptance": [
                        "订阅计费可用",
                        "发票管理可用",
                        "财务对账可用"
                    ],
                    "requirements": [
                        {"kind": "named", "text": "订阅计费", "source_quote": "实现订阅计费"},
                        {"kind": "named", "text": "发票管理", "source_quote": "实现发票管理"},
                        {"kind": "named", "text": "财务对账", "source_quote": "实现财务对账"}
                    ]
                }),
            ),
            Scripted::Tool(
                "update_plan",
                serde_json::json!({"plan": [
                    {"step": "订阅计费", "status": "in_progress"},
                    {"step": "发票管理", "status": "pending"},
                    {"step": "财务对账", "status": "pending"}
                ]}),
            ),
            // The model replaces the whole plan with what it actually did.
            Scripted::Tool(
                "update_plan",
                serde_json::json!({"plan": [
                    {"step": "退款流程", "status": "completed"},
                    {"step": "日终关账", "status": "completed"}
                ]}),
            ),
            Scripted::run_tests(),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "退款与关账完成",
                    "verification": {"status": "passed", "detail": test_runner_label()},
                    "requirements": [
                        {"id": "退款流程", "status": "done", "evidence": "已完成"},
                        {"id": "日终关账", "status": "done", "evidence": "已完成"}
                    ]
                }),
            ),
            Scripted::Tool(
                "get_goal_scope",
                serde_json::json!({"offset": 0, "limit": 20}),
            ),
            Scripted::Text("退款与关账已完成。"),
            Scripted::Text("退款与关账已完成。"),
        ],
        false,
    )
    .await;

    let mut intercepted = Vec::new();
    let mut plans = Vec::new();
    let report = fx
        .coding
        .chat(
            &fx.session,
            &spec_for(&fx, objective),
            vec![ContentPart::Text {
                text: objective.into(),
            }],
            &mut |event| match event {
                leveler_engine::EngineEvent::GoalIntercepted { kind, detail } => {
                    intercepted.push((kind, detail))
                }
                leveler_engine::EngineEvent::PlanUpdated { steps, created, .. } => {
                    plans.push((steps, created))
                }
                _ => {}
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(plans.len(), 2, "the model replaced its plan once");
    assert_eq!(plans[0].0.len(), 3);
    assert_eq!(plans[1].0.len(), 2);
    assert!(!plans[1].1, "the second declaration is an update");
    assert_eq!(plans[1].0[0].step, "退款流程");

    let refusal = intercepted
        .iter()
        .find(|(kind, _)| kind == "requirements_refused")
        .map(|(_, detail)| detail)
        .expect("completion through the shrunken plan is refused");
    for deliverable in ["订阅计费", "发票管理", "财务对账"] {
        assert!(
            refusal.contains(deliverable),
            "a plan edit cannot retire `{deliverable}`: {refusal}"
        );
    }

    let scope = {
        let requests = fx.runtime.requests.lock().unwrap();
        requests
            .iter()
            .flat_map(|request| request.messages.iter())
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result.content.clone()),
                _ => None,
            })
            .find(|content| content.contains("Goal scope:"))
            .expect("the scope is readable after the plan change")
    };
    for (id, deliverable) in [("R1", "订阅计费"), ("R2", "发票管理"), ("R3", "财务对账")]
    {
        assert!(
            scope.contains(&format!("- {id} [deliverable] active: {deliverable}")),
            "the goal still owes `{deliverable}` by id: {scope}"
        );
    }

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Stalled,
        "the turn cannot close a goal it left unresolved: {report:?}"
    );
    assert!(
        report
            .stop_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("goal_unresolved")),
        "the closeout names the unresolved goal, not a delivery: {report:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Running);
    assert_eq!(goals[0].objective, objective);
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
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": staged_objective(),
                    "acceptance": [
                        "stage-1.txt 存在",
                        "stage-2.txt 存在",
                        "stage-3.txt 存在",
                        verifier_acceptance(),
                    ],
                    "task_relation": "continue",
                    "requirements": [
                        {"kind": "artifact", "text": "stage-1.txt 存在",
                         "target": "stage-1.txt", "source_quote": "创建 stage-1.txt"},
                        {"kind": "artifact", "text": "stage-2.txt 存在",
                         "target": "stage-2.txt", "source_quote": "stage-2.txt"},
                        {"kind": "artifact", "text": "stage-3.txt 存在",
                         "target": "stage-3.txt", "source_quote": "stage-3.txt"},
                        {"kind": "command", "text": "verify.sh 通过",
                         "target": verifier_label(),
                         "source_quote": format!("运行 {}", verifier_label())}
                    ]
                }),
            ),
            write_stage(),
            // The model tries to re-declare a narrower plan mid-task. The stored
            // goal keeps the requirements it was committed against; a second
            // commit cannot seed a new scope into the same goal.
            commit_with(vec!["stage-1.txt 存在"], staged_objective()),
            Scripted::Tool("run_command", verifier_call()),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "all three stages are complete",
                    "verification": {"status": "passed", "detail": verifier_label()},
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "created stage-1.txt"},
                        {"id": "R2", "status": "done", "evidence": "created stage-2.txt"},
                        {"id": "R3", "status": "done", "evidence": "created stage-3.txt"},
                        {"id": "R4", "status": "done", "evidence": "ran the verifier"}
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
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": staged_objective(),
                    "acceptance": ["stage-1.txt 存在"],
                    "task_relation": "continue",
                    "requirements": [
                        {"kind": "artifact", "text": "stage-1.txt 存在",
                         "target": "stage-1.txt", "source_quote": "创建 stage-1.txt"},
                        {"kind": "artifact", "text": "stage-2.txt 存在",
                         "target": "stage-2.txt", "source_quote": "stage-2.txt"},
                        {"kind": "artifact", "text": "stage-3.txt 存在",
                         "target": "stage-3.txt", "source_quote": "stage-3.txt"},
                        {"kind": "command", "text": "verify.sh 通过",
                         "target": verifier_label(),
                         "source_quote": format!("运行 {}", verifier_label())}
                    ]
                }),
            ),
            write_stage(),
            Scripted::Tool("run_command", verifier_call()),
            // A one-line acceptance plan says what the model intends to check;
            // it cannot retire the requirements the commit itself declared.
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "stage-1 is done",
                    "verification": {"status": "passed", "detail": verifier_label()},
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "created stage-1.txt"},
                        {"id": "R4", "status": "done", "evidence": "ran the verifier"}
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

    assert_eq!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Blocked,
        "a narrow plan cannot make an incomplete goal complete: {report:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == "requirements_refused"
                    && detail.contains("stage-2.txt")
                    && detail.contains("stage-3.txt")
        )),
        "the refusal must name the requirements the plan omitted: {events:?}"
    );
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "the commit binds against the full request");
    assert_eq!(
        goals[0].acceptance.len(),
        1,
        "the model's acceptance plan is recorded as written"
    );
    let requirements = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&goals[0].id)
        .await
        .unwrap();
    assert_eq!(
        requirements.len(),
        4,
        "the declared requirement set is what completion is measured against"
    );
    assert!(
        requirements
            .iter()
            .any(|item| item.text.contains("stage-2.txt")
                && item.state == leveler_lifecycle::RequirementState::Active),
        "the omitted deliverable is still owed: {requirements:?}"
    );
}

/// A deterministic gate refusal cannot be satisfied by sending the exact same
/// completion account again. The repeated verdict is counted, and the drive
/// stops once the facts it reads have not changed across
/// [`MAX_IDENTICAL_GATE_REFUSALS`] attempts, instead of looping forever.
#[tokio::test]
async fn an_identical_gate_refusal_stops_instead_of_looping() {
    let incomplete_completion = || {
        Scripted::Tool(
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "stage-1 is done",
                "verification": {"status": "passed", "detail": verifier_label()},
                "requirements": [
                    {"id": "R1", "status": "done", "evidence": "created stage-1.txt"},
                    {"id": "R2", "status": "done", "evidence": "created stage-2.txt"},
                    {"id": "R3", "status": "done", "evidence": "created stage-3.txt"},
                    {"id": "R4", "status": "done", "evidence": "ran the verifier"}
                ]
            }),
        )
    };
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": staged_objective(),
                    "acceptance": ["stage-1.txt 存在", "stage-2.txt 存在", "stage-3.txt 存在"],
                    "task_relation": "continue",
                    "requirements": [
                        {"kind": "artifact", "text": "stage-1.txt 存在",
                         "target": "stage-1.txt", "source_quote": "创建 stage-1.txt"},
                        {"kind": "artifact", "text": "stage-2.txt 存在",
                         "target": "stage-2.txt", "source_quote": "stage-2.txt"},
                        {"kind": "artifact", "text": "stage-3.txt 存在",
                         "target": "stage-3.txt", "source_quote": "stage-3.txt"},
                        {"kind": "command", "text": "verify.sh 通过",
                         "target": verifier_label(),
                         "source_quote": format!("运行 {}", verifier_label())}
                    ]
                }),
            ),
            write_stage(),
            Scripted::Tool("run_command", verifier_call()),
            incomplete_completion(),
            // Real work between identical verdicts keeps the no-progress
            // watchdog quiet; the stuck-verdict bound must own this case.
            Scripted::Tool("run_command", verifier_call()),
            incomplete_completion(),
            Scripted::Tool("run_command", verifier_call()),
            incomplete_completion(),
            Scripted::Tool("run_command", verifier_call()),
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

    let refusals: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == "requirements_refused" =>
            {
                Some(detail)
            }
            _ => None,
        })
        .collect();
    assert!(
        refusals.len() >= 3,
        "every identical refusal must be visible: {events:?}"
    );
    assert!(
        refusals.iter().any(|detail| detail.contains("stage-2.txt")),
        "the refusal names the unaccounted deliverable: {refusals:?}"
    );
    let detail = report.stop_detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("same refusal") && detail.contains("retrying cannot satisfy it"),
        "a repeated deterministic verdict must stop the loop: {report:?}"
    );
    assert_ne!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Completed,
        "a refused completion is never a delivered task: {report:?}"
    );
    // The window ends as a failed run, but the declaration it was measured
    // against is still in durable history: stopping cannot erase the two
    // deliverables the model never produced.
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    let requirements = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&goals[0].id)
        .await
        .unwrap();
    assert!(
        requirements
            .iter()
            .any(|item| item.text.contains("stage-2.txt")
                && item.state == leveler_lifecycle::RequirementState::Active),
        "the unterminated requirement survives the failed window: {requirements:?}"
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
                if leveler_agent::is_commit_refusal_kind(kind) && detail.contains("##")
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
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": staged_objective(),
                    "acceptance": [
                        "stage-1.txt 存在",
                        "stage-2.txt 存在",
                        "stage-3.txt 存在",
                        verifier_acceptance(),
                    ],
                    "task_relation": "continue",
                    "requirements": [
                        {"kind": "artifact", "text": "stage-1.txt 存在",
                         "target": "stage-1.txt", "source_quote": "创建 stage-1.txt"},
                        {"kind": "artifact", "text": "stage-2.txt 存在",
                         "target": "stage-2.txt", "source_quote": "stage-2.txt"},
                        {"kind": "artifact", "text": "stage-3.txt 存在",
                         "target": "stage-3.txt", "source_quote": "stage-3.txt"},
                        {"kind": "command", "text": "verify.sh 通过",
                         "target": verifier_label(), "source_quote": format!("运行 {}", verifier_label())}
                    ]
                }),
            ),
            Scripted::Text("stage-1 is done; continuing."),
            write_stage(),
            // The waiver is the model's structured declaration, anchored to the
            // user's own resume message. The runtime checks the quote exists;
            // it does not decide from the words whether a waiver happened.
            Scripted::Tool(
                "declare_scope",
                serde_json::json!({
                    "operations": [
                        {"op": "waive", "requirement_id": "R2",
                         "source_quote": "stage-2.txt、stage-3.txt 暂时不做",
                         "reason": "the user deferred stages 2 and 3"},
                        {"op": "waive", "requirement_id": "R3",
                         "source_quote": "stage-2.txt、stage-3.txt 暂时不做",
                         "reason": "the user deferred stages 2 and 3"}
                    ]
                }),
            ),
            Scripted::Tool("run_command", verifier_call()),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "stage 1 delivered; stages 2-3 were waived by the user",
                    "verification": {"status": "passed", "detail": verifier_label()},
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "created stage-1.txt"},
                        {"id": "R4", "status": "done", "evidence": "ran the verifier"}
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
        "the user's declared waiver is authority: {resumed:?}"
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
        "the user's own message stays recorded verbatim: {goals:?}"
    );
    let requirements = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&goal_id)
        .await
        .unwrap();
    let state_of = |id: &str| {
        requirements
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.state)
    };
    assert_eq!(
        state_of("R1"),
        Some(leveler_lifecycle::RequirementState::Active),
        "{requirements:?}"
    );
    assert_eq!(
        state_of("R2"),
        Some(leveler_lifecycle::RequirementState::Waived),
        "the deferred stage is no longer owed: {requirements:?}"
    );
    assert_eq!(
        state_of("R3"),
        Some(leveler_lifecycle::RequirementState::Waived),
        "{requirements:?}"
    );
    assert_eq!(
        state_of("R4"),
        Some(leveler_lifecycle::RequirementState::Active),
        "{requirements:?}"
    );
    let history = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirement_log(&goal_id)
        .await
        .unwrap();
    assert!(
        history.iter().any(|record| {
            record.event.operation == leveler_lifecycle::ScopeOperationKind::Waive
                && record.event.source_quote.contains("暂时不做")
        }),
        "the waiver event carries the user's authorizing words: {history:?}"
    );
}

#[tokio::test]
async fn a_tests_requirement_needs_its_declared_command_to_run() {
    let objective = "实现全部接口，并通过完整测试。";
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": objective,
                    "acceptance": ["完整测试通过"],
                    "task_relation": "continue",
                    "requirements": [
                        {"kind": "tests", "text": "完整测试通过",
                         "target": test_runner_label(),
                         "source_quote": "通过完整测试"}
                    ]
                }),
            ),
            Scripted::Tool("run_command", checker_call()),
            // A passing build check is not the declared test command. The
            // runtime matches the declaration, not a command it guesses at.
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "the interfaces compile",
                    "verification": {"status": "passed", "detail": checker_label()},
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": "the interfaces compile"}
                    ]
                }),
            ),
            Scripted::run_tests(),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "the interfaces are implemented and the suite passes",
                    "verification": {"status": "passed", "detail": test_runner_label()},
                    "requirements": [
                        {"id": "R1", "status": "done", "evidence": test_runner_label()}
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
            &spec_for(&fx, objective),
            vec![ContentPart::Text {
                text: objective.into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == "requirements_refused"
                    && detail.contains("R1")
                    && detail.contains(test_runner_label())
                    && detail.contains("no passing run")
        )),
        "a build check is not the declared test command: {events:?}"
    );
    assert_eq!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Completed,
        "the declared test command's passing run resolves the goal: {report:?}"
    );
    let verification = report.verification.expect("report");
    assert_eq!(
        verification.status,
        leveler_lifecycle::VerificationStatus::Unverified,
        "the run proves command success, not structured test results: {verification:?}"
    );
    assert_eq!(verification.records.len(), 1);
    assert_eq!(
        verification.records[0].state,
        leveler_lifecycle::VerificationRecordState::Unverified
    );
    assert_eq!(
        verification.records[0].command.as_deref(),
        Some(test_runner_label())
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

/// A fresh long-task objective whose authorization is stated as a negation
/// ("不需要逐步确认"). It names two artifacts plus their verification, so the
/// runtime can hold the model to an explicit scope without inventing one.
fn authorized_long_objective() -> &'static str {
    if cfg!(windows) {
        "分两阶段交付：第一阶段创建 long-1.txt 并运行 cmd /c verify.cmd 验收；\
         第二阶段创建 long-2.txt 并运行 cmd /c verify.cmd 验收。\
         全过程不需要逐步确认，也不要中途停下来问我，直到两项都完成并验收。"
    } else {
        "分两阶段交付：第一阶段创建 long-1.txt 并运行 sh ./verify.sh 验收；\
         第二阶段创建 long-2.txt 并运行 sh ./verify.sh 验收。\
         全过程不需要逐步确认，也不要中途停下来问我，直到两项都完成并验收。"
    }
}

fn stage_plan(status1: &'static str, status2: &'static str) -> Scripted {
    Scripted::Tool(
        "update_plan",
        serde_json::json!({
            "plan": [
                {"id": "stage-1", "step": "阶段 1：创建 stage-1.txt 并验收", "status": status1},
                {"id": "stage-2", "step": "阶段 2：创建 stage-2.txt 并验收", "status": status2}
            ]
        }),
    )
}

/// The model's structured reading of the user's boundary: stop once the stage
/// the user named is completed. The quote is verbatim from the objective; the
/// boundary references the plan's stable step id, not its wording.
fn declare_pause_after_stage_one() -> Scripted {
    Scripted::Tool(
        "declare_intent",
        serde_json::json!({
            "source_quote": "阶段 1 完成后停下，等待我确认再执行阶段 2",
            "operations": [{
                "kind": "set_pause_boundary",
                "boundary": {
                    "kind": "after_step",
                    "step": {"step_id": "stage-1", "label": "阶段 1：创建 stage-1.txt 并验收"}
                }
            }]
        }),
    )
}

/// The model's reading of a user message that authorizes the withheld work.
/// `quote` must occur verbatim in that message.
fn declare_continue(quote: &'static str) -> Scripted {
    Scripted::Tool(
        "declare_intent",
        serde_json::json!({
            "source_quote": quote,
            "operations": [{"kind": "continue_task"}]
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
        declare_pause_after_stage_one(),
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
        7,
        "the reached boundary buys NO extra model round: the seven calls are the script's own"
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

/// A required review can report a finding on a window that stopped at the
/// user's pause. The pause is still the terminal truth: the report stays
/// `Completed` + `Answered` with the pause detail and the warning beside it,
/// the durable Goal stays open and paused, and no consumer may read the
/// warning as an accepted delivery of the withheld stage.
#[tokio::test]
async fn a_paused_goal_with_a_review_warning_keeps_the_pause_terminal() {
    let mut script = pause_stage_one_script();
    script.push(Scripted::Tool(
        "report_finding",
        serde_json::json!({
            "kind": "correctness",
            "summary": "stage-1.txt is empty",
            "file": "stage-1.txt",
            "blocking": true
        }),
    ));
    script.push(Scripted::Text("reviewed stage-1.txt: one finding reported"));
    script.push(Scripted::Text("reviewed stage-1.txt: one finding reported"));

    let mut fx = fixture(script, false).await;
    fx.coding.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
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

    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Answered);
    assert!(
        report
            .stop_detail
            .as_deref()
            .is_some_and(leveler_agent::closeout::is_user_pause_detail),
        "the pause detail must survive the review: {report:?}"
    );
    assert!(
        !report.completion_warnings.is_empty(),
        "the reviewer finding must stay on the report: {report:?}"
    );

    // The live terminal and its durable row carry the same facts the client
    // projection reads on replay.
    let live = events
        .iter()
        .find_map(|event| match event {
            leveler_engine::EngineEvent::TaskFinished {
                outcome,
                reason,
                stop,
                warnings,
                ..
            } => Some((*outcome, reason.clone(), *stop, warnings.clone())),
            _ => None,
        })
        .expect("the task terminal must be published");
    assert_eq!(live.0, leveler_lifecycle::TaskOutcome::Completed);
    assert_eq!(live.2, Some(leveler_agent::StopReason::Answered));
    assert!(
        live.1
            .as_deref()
            .is_some_and(leveler_agent::closeout::is_user_pause_detail),
        "the live terminal must say the user paused: {live:?}"
    );
    assert!(!live.3.is_empty(), "the live terminal keeps the warning");

    let stores = leveler_storage::EngineStores::from_database(&fx.db);
    let durable = stores
        .events
        .load(&fx.session)
        .await
        .unwrap()
        .into_iter()
        .find_map(|row| {
            (row.event_type == "task_finished")
                .then(|| leveler_engine::EngineEvent::from_payload(&row.payload).unwrap())
                .and_then(|event| match event {
                    leveler_engine::EngineEvent::TaskFinished {
                        outcome,
                        reason,
                        stop,
                        warnings,
                        ..
                    } => Some((outcome, reason, stop, warnings)),
                    _ => None,
                })
        })
        .expect("the task terminal must be durable");
    assert_eq!(durable.0, leveler_lifecycle::TaskOutcome::Completed);
    assert_eq!(durable.2, Some(leveler_agent::StopReason::Answered));
    assert!(
        durable
            .1
            .as_deref()
            .is_some_and(leveler_agent::closeout::is_user_pause_detail),
        "the durable terminal must say the user paused: {durable:?}"
    );
    assert!(
        !durable.3.is_empty(),
        "the durable terminal keeps the warning"
    );

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Running);
    assert!(
        goals[0].user_paused_at.is_some(),
        "the warning must not release the pause: {goals:?}"
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
}

/// A plain Chat answer can also receive a required-review warning. That is a
/// warning about the work, not a registration failure: nothing was declared,
/// so the executor must not emit the refusal fact the clients read.
#[tokio::test]
async fn a_plain_chat_review_warning_is_not_a_registration_failure() {
    let mut fx = fixture(
        vec![
            write_named("answer.txt"),
            Scripted::Text("已创建 answer.txt。"),
            Scripted::Tool(
                "report_finding",
                serde_json::json!({
                    "kind": "correctness",
                    "summary": "answer.txt is empty",
                    "file": "answer.txt",
                    "blocking": true
                }),
            ),
            Scripted::Text("reviewed answer.txt: one finding reported"),
            Scripted::Text("reviewed answer.txt: one finding reported"),
        ],
        false,
    )
    .await;
    fx.coding.factory.independent_review = leveler_agent::coding::IndependentReviewPolicy::Required;
    let mut events = Vec::new();
    let objective = "创建 answer.txt 并交付";

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

    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Answered);
    assert!(
        !report.completion_warnings.is_empty(),
        "the review finding must stay on the report: {report:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::GoalIntercepted { kind, .. }
                if leveler_agent::is_commit_refusal_kind(kind)
        )),
        "a plain Chat answer attempted no declaration: {events:?}"
    );
    assert!(
        goal_records(&fx).await.is_empty(),
        "a plain Chat registers no goal"
    );
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

/// Invalid arguments are refused as an invalid declaration. They are not an
/// acceptance that narrowed the user's scope, and the gate stays strict: the
/// call registers nothing and tells the model which field to fix.
#[tokio::test]
async fn a_schema_invalid_declaration_is_named_as_invalid_arguments() {
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": USER_REQUEST,
                    // A string, not an array: the arguments do not have the
                    // shape the tool declares.
                    "acceptance": "go test ./... 通过",
                    "task_relation": "continue",
                }),
            ),
            Scripted::Text("I could not commit the task."),
        ],
        false,
    )
    .await;
    let mut events = Vec::new();

    let report = fx
        .coding
        .chat(
            &fx.session,
            &spec_for(&fx, USER_REQUEST),
            vec![ContentPart::Text {
                text: USER_REQUEST.into(),
            }],
            &mut |event| events.push(event),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let refusal = events
        .iter()
        .find_map(|event| match event {
            leveler_engine::EngineEvent::GoalIntercepted { kind, detail }
                if kind == "invalid_declaration" || kind == "acceptance_narrowed" =>
            {
                Some((kind.clone(), detail.clone()))
            }
            _ => None,
        })
        .expect("the malformed declaration is refused");
    assert_eq!(
        refusal.0, "invalid_declaration",
        "a schema error is not a narrowed acceptance: {refusal:?}"
    );
    assert!(
        refusal.1.contains("acceptance"),
        "the refusal names the field to fix: {refusal:?}"
    );
    assert!(
        goal_records(&fx).await.is_empty(),
        "invalid arguments register no goal"
    );
    assert_eq!(
        report.outcome,
        leveler_lifecycle::TaskOutcome::Completed,
        "the turn ends on the model's own answer: {report:?}"
    );
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
async fn a_declaration_that_quotes_an_older_message_is_refused() {
    // The user's continuation is "确认，继续阶段 2". The model tries to
    // authorize it by quoting the ORIGINAL objective — a real span of an older
    // message. Provenance is per newest input: an older authorization cannot
    // stand in for the message being read now.
    let mut script = pause_stage_one_script();
    script.extend([
        Scripted::Tool(
            "declare_intent",
            serde_json::json!({
                "source_quote": "阶段 1 完成后停下，等待我确认再执行阶段 2",
                "operations": [{"kind": "continue_task"}]
            }),
        ),
        Scripted::Text("没有得到新的继续授权，阶段 2 仍未执行。"),
    ]);
    let fx = fixture(script, false).await;
    write_verifier(fx.dir.path());
    pause_a_goal(&fx).await;
    let paused = goal_records(&fx).await;
    let goal_id = paused[0].id.clone();
    let requests_before = fx.runtime.requests.lock().unwrap().len();

    let report = say(&fx, "确认，继续阶段 2，把剩余任务完成并验收。").await;

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Answered,
        "the turn answers; it does not drive the goal: {report:?}"
    );
    {
        let requests = fx.runtime.requests.lock().unwrap();
        let refusal = requests[requests_before..]
            .iter()
            .flat_map(|request| request.messages.iter())
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result.content.clone()),
                _ => None,
            })
            .find(|content| content.contains("does not occur in the user's newest message"))
            .expect("the model sees the provenance refusal");
        assert!(
            refusal.contains("do not invent an instruction"),
            "{refusal}"
        );
    }
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Running);
    assert!(
        goals[0].user_paused_at.is_some(),
        "the older quote did not release the pause: {goals:?}"
    );
    assert!(
        !fx.dir.path().join("stage-2.txt").exists(),
        "no authority was created by the quote"
    );
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

#[tokio::test]
async fn a_declaration_with_two_running_goals_must_name_one() {
    // Two durable running goals. The model's declaration names neither, so the
    // runtime refuses instead of guessing — the ids are the fix, not a phrase.
    let first_pause = Scripted::Tool(
        "declare_intent",
        serde_json::json!({
            "source_quote": "接入全部运营后台能力到 MCP",
            "operations": [{"kind": "pause_task", "boundary": {"kind": "now"}}]
        }),
    );
    let second_commit = Scripted::Tool(
        "commit_task",
        serde_json::json!({
            "objective": "把清单剩下的部分做完",
            "acceptance": ["stage-2.txt 存在"],
            "task_relation": "new"
        }),
    );
    let second_pause = Scripted::Tool(
        "declare_intent",
        serde_json::json!({
            "source_quote": "把清单剩下的部分做完",
            "operations": [{"kind": "pause_task", "boundary": {"kind": "now"}}]
        }),
    );
    let fx = fixture(
        vec![
            Scripted::commit(),
            first_pause,
            Scripted::Text("已按你的要求暂停。"),
            second_commit,
            second_pause,
            Scripted::Text("第二个任务也先停在这里。"),
            Scripted::Tool(
                "declare_intent",
                serde_json::json!({
                    "source_quote": "继续",
                    "operations": [{"kind": "continue_task"}]
                }),
            ),
            Scripted::Text("需要你说明要继续哪一个任务。"),
        ],
        false,
    )
    .await;
    // Turn 1: the first goal, paused by declaration.
    fx.coding
        .chat(
            &fx.session,
            &spec(&fx),
            vec![ContentPart::Text {
                text: USER_REQUEST.into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();
    // Turn 2: an explicit independent task, paused too.
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
    let owed = goal_records(&fx).await;
    assert_eq!(owed.len(), 2, "two independent running goals: {owed:?}");
    assert!(
        owed.iter()
            .all(|goal| goal.state == leveler_storage::GoalState::Running)
    );
    let ids: Vec<String> = owed.iter().map(|goal| goal.id.to_string()).collect();

    let _ = say(&fx, "继续").await;

    let refusal = {
        let requests = fx.runtime.requests.lock().unwrap();
        requests
            .iter()
            .flat_map(|request| request.messages.iter())
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some(result.content.clone()),
                _ => None,
            })
            .find(|content| content.contains("running goals") && content.contains("will not guess"))
            .expect("the ambiguity refusal names the choice")
    };
    for id in &ids {
        assert!(
            refusal.contains(id),
            "the refusal lists goal {id}: {refusal}"
        );
    }
    let goals = goal_records(&fx).await;
    assert_eq!(
        goals.len(),
        2,
        "no goal was created or settled by the refusal"
    );
    assert_eq!(
        goals
            .iter()
            .map(|goal| goal.id.to_string())
            .collect::<Vec<_>>(),
        ids,
        "the same two goals"
    );
    std::fs::remove_dir_all(fx.dir.path()).ok();
}

#[tokio::test]
async fn an_explicit_continuation_resumes_the_paused_goal_and_verifies_it() {
    let mut script = pause_stage_one_script();
    script.extend([
        declare_continue("继续阶段 2"),
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

/// An intent may name the span instead of copying the continuation. The span is
/// resolved against the newest user input only — not the older objective — and
/// the durable intent log stores the exact line the runtime cut.
#[tokio::test]
async fn a_span_releases_the_pause_and_the_intent_log_keeps_the_cut_line() {
    const CONTINUATION: &str = "确认。\n继续阶段 2，把剩余任务完成并验收。";
    let mut script = pause_stage_one_script();
    script.extend([
        Scripted::Tool(
            "declare_intent",
            serde_json::json!({
                "source_span": {"start_line": 2, "end_line": 2},
                "operations": [{"kind": "continue_task"}]
            }),
        ),
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
    let goal_id = goal_records(&fx).await[0].id.clone();

    let resumed = fx
        .coding
        .resume_with_instruction(
            &fx.session,
            &spec_for(&fx, staged),
            Message::user_input(CONTINUATION),
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
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].id, goal_id);
    assert!(
        goals[0].user_paused_at.is_none(),
        "the span declaration released the pause"
    );
    let log = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .intent_log(&goal_id)
        .await
        .unwrap();
    assert_eq!(log.len(), 2, "the boundary and the continuation: {log:?}");
    let continuation = log.last().expect("the continuation entry");
    assert_eq!(
        continuation.source_quote, "继续阶段 2，把剩余任务完成并验收。",
        "the log carries the user's exact line, not the coordinate"
    );
    assert!(
        matches!(
            continuation.operations.as_slice(),
            [leveler_lifecycle::UserIntentOperation::ContinueTask]
        ),
        "{continuation:?}"
    );
}

/// The same authorization in another language, with no shared keyword: the
/// model reads it and quotes the user's own span. Nothing in the runtime has a
/// vocabulary to match it against.
#[tokio::test]
async fn a_continuation_in_another_language_releases_the_same_pause() {
    let mut script = pause_stage_one_script();
    script.extend([
        declare_continue("continue with stage 2"),
        write_named("stage-2.txt"),
        Scripted::Tool("run_command", verifier_call()),
        stage_plan("completed", "completed"),
        Scripted::Tool(
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "both stages delivered and verified",
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
    assert!(
        goal_records(&fx).await[0].user_paused_at.is_some(),
        "the pause is the precondition"
    );

    let report = say(
        &fx,
        "Okay — continue with stage 2 and finish the remaining work.",
    )
    .await;

    assert_eq!(report.stop_reason, leveler_agent::StopReason::Completed);
    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
    assert!(
        goals[0].user_paused_at.is_none(),
        "the English authorization released the pause: {goals:?}"
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

/// A fresh, isolated long task: the user explicitly refuses step-by-step
/// confirmation, names two artifacts plus verification, and the model tries to
/// end after the first stage. The runtime must not read that answer as a pause
/// or as completion: it continues the SAME scope, and only the window that
/// produces the second artifact and re-runs the verifier may close the goal.
#[tokio::test]
async fn a_long_task_without_step_confirmation_continues_instead_of_pausing() {
    let objective = authorized_long_objective();
    let fx = fixture(
        vec![
            commit_with(
                vec!["long-1.txt 存在", "long-2.txt 存在", verifier_acceptance()],
                objective,
            ),
            stage_plan("in_progress", "pending"),
            write_named("long-1.txt"),
            stage_plan("completed", "pending"),
            // The model stops with prose after stage 1. The user authorized no
            // step boundary, so this is an unfinished task, not a pause.
            Scripted::Text("第一阶段已交付。第二阶段我接下来继续处理。"),
            write_named("long-2.txt"),
            Scripted::Tool("run_command", verifier_call()),
            stage_plan("completed", "completed"),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "两个阶段都已完成并验收",
                    "verification": {"status": "passed", "detail": verifier_label()},
                    "requirements": [
                        {"requirement": "long-1.txt", "status": "done",
                         "evidence": "created long-1.txt"},
                        {"requirement": "long-2.txt", "status": "done",
                         "evidence": "created long-2.txt"},
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
        report.stop_reason,
        leveler_agent::StopReason::Completed,
        "the task reached its declared completion: {report:?}"
    );
    assert!(
        !report
            .stop_detail
            .as_deref()
            .is_some_and(leveler_agent::closeout::is_user_pause_detail),
        "a negated confirmation request is not a pause: {report:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            leveler_engine::EngineEvent::RuntimeInjection { kind, .. }
                if kind.contains("goal_unresolved")
        )),
        "the early answer must trigger a continuation, not a terminal: {events:?}"
    );

    let goals = goal_records(&fx).await;
    assert_eq!(
        goals.len(),
        1,
        "the continuation stays in one goal: {goals:?}"
    );
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
    assert!(
        goals[0].user_paused_at.is_none(),
        "the user never asked to wait: {goals:?}"
    );
    assert_eq!(
        goals[0].acceptance.len(),
        3,
        "both artifacts and their verification stay in scope: {goals:?}"
    );
    assert!(fx.dir.path().join("long-1.txt").exists());
    assert!(
        fx.dir.path().join("long-2.txt").exists(),
        "the runtime continued the withheld stage instead of accepting the answer"
    );
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
            declare_pause_after_stage_one(),
            write_named("stage-1.txt"),
            Scripted::Tool("run_command", verifier_call()),
            stage_plan("completed", "pending"),
            Scripted::Text("阶段 1 已完成并验收。按你的要求，阶段 2 等待你的确认。"),
            // The query turn: the model reads it as a question, not
            // authorization, and the runtime drives no withheld stage.
            Scripted::Tool(
                "declare_intent",
                serde_json::json!({
                    "source_quote": "刚才阶段 1 修改了哪些文件？测试通过了吗？",
                    "operations": [{"kind": "answer_only"}]
                }),
            ),
            Scripted::Text("阶段 1 修改了 stage-1.txt；验收命令 sh ./verify.sh 退出码 0。"),
            // The authorization turn: the model continues for real.
            declare_continue("继续阶段 2"),
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
        calls_after_pause + 2,
        "a query buys its declared reading and its own answer, no goal continuation"
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
                    {"id": "stage-1", "step": "阶段 1：创建 stage-1.txt", "status": "in_progress"},
                    {"id": "stage-1-verify", "step": "阶段 1：运行 sh ./verify.sh 确认", "status": "pending"},
                    {"step": "阶段 1 完成后停下，等待用户确认后再继续", "status": "pending"},
                    {"id": "stage-2", "step": "阶段 2：创建 stage-2.txt", "status": "pending"}
                ]}),
            ),
            Scripted::Tool(
                "declare_intent",
                serde_json::json!({
                    "source_quote": "阶段 1 完成后停下，等待我确认再执行阶段 2",
                    "operations": [{
                        "kind": "set_pause_boundary",
                        "boundary": {
                            "kind": "after_step",
                            "step": {"step_id": "stage-1-verify", "label": "阶段 1：运行 sh ./verify.sh 确认"}
                        }
                    }]
                }),
            ),
            write_named("stage-1.txt"),
            Scripted::Tool("run_command", verifier_call()),
            Scripted::Tool(
                "update_plan",
                serde_json::json!({"plan": [
                    {"id": "stage-1", "step": "阶段 1：创建 stage-1.txt", "status": "completed"},
                    {"id": "stage-1-verify", "step": "阶段 1：运行 sh ./verify.sh 确认", "status": "completed"},
                    {"step": "阶段 1 完成后停下，等待用户确认后再继续", "status": "in_progress"},
                    {"id": "stage-2", "step": "阶段 2：创建 stage-2.txt", "status": "pending"}
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
    // The model reads the supplement as an amendment to the same goal; the
    // runtime records the user's words from that declaration, not from the
    // text itself.
    script.push(Scripted::Tool(
        "declare_intent",
        serde_json::json!({
            "source_quote": supplement_request(),
            "operations": [{"kind": "amend_current_task"}]
        }),
    ));
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

    let question = "刚才阶段 1 修改了哪些文件？测试通过了吗？现在进度怎么样？";
    say(&fx, question).await;

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "{goals:?}");
    assert_eq!(
        goals[0].amendments,
        Vec::<String>::new(),
        "a question the model did not declare as an amendment stays out of scope: {goals:?}"
    );
    let requirements = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&goals[0].id)
        .await
        .unwrap();
    assert!(
        requirements.is_empty(),
        "a status question can never become a declared requirement: {requirements:?}"
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

    let request = "另外帮我检查一下 README 有没有过时的说明。";
    say(&fx, request).await;

    let goals = goal_records(&fx).await;
    assert_eq!(goals.len(), 1, "{goals:?}");
    assert_eq!(
        goals[0].amendments,
        Vec::<String>::new(),
        "without a declaration the message is not an amendment to this goal: {goals:?}"
    );
    let requirements = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&goals[0].id)
        .await
        .unwrap();
    assert!(
        requirements.is_empty(),
        "without a structured declaration the message enumerates no requirement: {requirements:?}"
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
    script.extend([
        // The paused window adopts the goal without releasing the pause, then
        // records the supplement as a structured requirement anchored to the
        // user's own words. Scope is a declaration, never an extraction.
        Scripted::Tool(
            "declare_intent",
            serde_json::json!({
                "source_quote": "顺便把 stage-extra.txt 也补上",
                "operations": [{"kind": "amend_current_task"}]
            }),
        ),
        Scripted::Tool(
            "declare_scope",
            serde_json::json!({
                "operations": [{
                    "op": "add",
                    "kind": "artifact",
                    "text": "stage-extra.txt 存在",
                    "target": "stage-extra.txt",
                    "source_quote": "顺便把 stage-extra.txt 也补上"
                }]
            }),
        ),
        Scripted::Text("补充要求已记下；阶段 2 仍等你确认。"),
        // The user's continuation releases the pause and continues the same
        // goal; only a validated declaration can do that.
        declare_continue("继续阶段 2"),
        // Stage 2 only: the supplement is still owed, and the model claims it
        // anyway. The runtime must refuse, not warn.
        write_named("stage-2.txt"),
        Scripted::Tool("run_command", verifier_call()),
        stage_plan("completed", "completed"),
        complete(serde_json::json!([
            {"id": "R1", "status": "done", "evidence": "created stage-extra.txt"}
        ])),
        // The refusal is answered with the real work.
        write_named("stage-extra.txt"),
        Scripted::Tool("run_command", verifier_call()),
        complete(serde_json::json!([
            {"id": "R1", "status": "done", "evidence": "created stage-extra.txt"}
        ])),
    ]);
    let fx = fixture(script, false).await;
    write_verifier(fx.dir.path());
    pause_a_goal(&fx).await;
    say(&fx, supplement_request()).await;
    let recorded = goal_records(&fx).await;
    let requirements = leveler_storage::EngineStores::from_database(&fx.db)
        .goals
        .requirements(&recorded[0].id)
        .await
        .unwrap();
    assert!(
        requirements
            .iter()
            .any(|item| item.target.as_deref() == Some("stage-extra.txt")),
        "the supplement is recorded as a structured requirement: {requirements:?}"
    );
    assert!(
        recorded[0].user_paused_at.is_some(),
        "declaring a requirement does not release the pause: {recorded:?}"
    );

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
        declare_continue("继续阶段 2"),
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
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": objective,
                    "acceptance": ["全部功能实测完成", "make up 成功"],
                    "task_relation": "continue",
                    "requirements": [
                        {"kind": "named", "text": "全部功能实测",
                         "source_quote": "把全部功能真实测试一遍"}
                    ]
                }),
            ),
            Scripted::Tool(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "全部功能实测完成",
                    "verification": {"status": "not_applicable", "detail": "只做了验证，没有改文件"},
                    "requirements": [
                        {"id": "R1", "status": "done",
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

/// B12: a background process that modifies the workspace after a verification
/// pass must expire that pass. The modification is learned from the durable
/// settlement the production `wait_task` returns, so the model cannot carry a
/// stale pass into completion.
#[tokio::test]
async fn b12_a_background_write_expires_the_verification_it_followed() {
    let fx = fixture_with_mode(
        vec![
            Scripted::commit(),
            Scripted::run_tests(),
            Scripted::Tool(
                "run_command",
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "sleep 0.1; printf 'background' > background-write.txt"],
                    "background": true
                }),
            ),
            Scripted::WaitLatestBackground,
            Scripted::complete(),
            Scripted::run_tests(),
            Scripted::complete(),
            Scripted::Text("后台写入了工作区，已重新验证后完成。"),
        ],
        false,
        PermissionProfile::FullAccess,
    )
    .await;

    leveler_test_support::git::init_repo(fx.dir.path());
    std::fs::write(fx.dir.path().join("seed.txt"), "seed\n").unwrap();
    leveler_test_support::git::run(fx.dir.path(), &["add", "-A"]);
    leveler_test_support::git::run(fx.dir.path(), &["commit", "-qm", "seed"]);

    let mut refusals = Vec::new();
    let report = fx
        .coding
        .chat(
            &fx.session,
            &spec(&fx),
            vec![ContentPart::Text {
                text: USER_REQUEST.into(),
            }],
            &mut |event| {
                if let leveler_engine::EngineEvent::GoalIntercepted { kind, detail } = event
                    && kind == "verification_refused"
                {
                    refusals.push(detail.to_string());
                }
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        fx.dir.path().join("background-write.txt").is_file(),
        "the real background process must have written the workspace"
    );
    assert_eq!(
        refusals.len(),
        1,
        "the stale pass must be refused exactly once: {refusals:?}; report={report:?}"
    );
    assert!(
        refusals[0].contains("verification expired")
            || refusals[0].contains("before the most recent file change"),
        "the refusal must name freshness: {}",
        refusals[0]
    );
    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Completed,
        "the re-run must permit completion: {report:?}"
    );
    assert_eq!(report.outcome, leveler_lifecycle::TaskOutcome::Completed);
}

/// Every tool result the scripted model was shown, in call order.
fn tool_results(fx: &Fixture) -> Vec<String> {
    fx.runtime
        .requests
        .lock()
        .unwrap()
        .iter()
        .flat_map(|request| request.messages.iter())
        .flat_map(|message| message.content.iter())
        .filter_map(|part| match part {
            ContentPart::ToolResult { result } => Some(result.content.clone()),
            _ => None,
        })
        .collect()
}

fn tool_result_containing(fx: &Fixture, needle: &str) -> String {
    tool_results(fx)
        .into_iter()
        .find(|content| content.contains(needle))
        .unwrap_or_else(|| panic!("no tool result contained `{needle}`"))
}

/// A malformed acceptance element names the exact array index, the actual JSON
/// type and the allowed shape, so the model can repair the call instead of
/// guessing what the generic contract sentence meant.
#[tokio::test]
async fn a_malformed_acceptance_entry_names_the_index_and_the_actual_type() {
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": USER_REQUEST,
                    "acceptance": ["go test ./... 通过", 42]
                }),
            ),
            Scripted::Text("这条声明没有登记成功。"),
        ],
        false,
    )
    .await;

    chat(&fx).await;

    let result = tool_result_containing(&fx, "acceptance[1]");
    assert!(result.contains("must be a string"), "{result}");
    assert!(result.contains("number"), "{result}");
    assert!(result.contains("Fix:"), "{result}");
    assert!(
        goal_records(&fx).await.is_empty(),
        "no malformed declaration may register a goal"
    );
}

/// More checks than the schema allows is a grouping decision, and the error
/// says so instead of repeating the generic contract.
#[tokio::test]
async fn too_many_acceptance_checks_ask_the_model_to_group() {
    let acceptance: Vec<String> = (0..21).map(|index| format!("检查 {index}")).collect();
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": USER_REQUEST,
                    "acceptance": acceptance
                }),
            ),
            Scripted::Text("无法以这么多条验收登记。"),
        ],
        false,
    )
    .await;

    chat(&fx).await;

    let result = tool_result_containing(&fx, "acceptance");
    assert!(result.contains("21"), "{result}");
    assert!(result.contains("20"), "{result}");
    assert!(result.contains("group"), "{result}");
}

/// The user's text is the durable objective. When it is present, the model's
/// `objective` is replaced by it and must not be able to veto the declaration
/// by being absent or longer than the model-facing limit.
#[tokio::test]
async fn the_users_text_replaces_the_model_objective_without_duplicate_validation() {
    for arguments in [
        serde_json::json!({
            "acceptance": ["go test ./... 通过", "MCP 读写工具与页面能力一致"]
        }),
        serde_json::json!({
            "objective": "x".repeat(4001),
            "acceptance": ["go test ./... 通过", "MCP 读写工具与页面能力一致"]
        }),
    ] {
        let fx = fixture(
            vec![
                Scripted::Tool("commit_task", arguments),
                Scripted::run_tests(),
                Scripted::complete(),
            ],
            false,
        )
        .await;

        let report = chat(&fx).await;

        assert_eq!(
            report.outcome,
            leveler_lifecycle::TaskOutcome::Completed,
            "{report:?}"
        );
        let goals = goal_records(&fx).await;
        assert_eq!(goals.len(), 1, "the declaration must bind one task");
        assert_eq!(
            goals[0].objective, USER_REQUEST,
            "the durable objective is the user's own text"
        );
    }
}

/// A relation with the wrong JSON type is not the same fact as an omitted
/// relation. It must be named, not silently ignored.
#[tokio::test]
async fn a_task_relation_with_the_wrong_type_is_named() {
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": USER_REQUEST,
                    "acceptance": ["go test ./... 通过", "MCP 读写工具与页面能力一致"],
                    "task_relation": 7
                }),
            ),
            Scripted::Text("登记失败。"),
        ],
        false,
    )
    .await;

    chat(&fx).await;

    let result = tool_result_containing(&fx, "task_relation");
    assert!(result.contains("string"), "{result}");
    assert!(result.contains("number"), "{result}");
    assert!(
        goal_records(&fx).await.is_empty(),
        "no malformed declaration may register a goal"
    );
}

/// A blank check is not a check. The error names the exact element so the
/// model can repair the array instead of guessing which entry it lost.
#[tokio::test]
async fn an_empty_acceptance_entry_names_its_index() {
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": USER_REQUEST,
                    "acceptance": ["go test ./... 通过", "   "]
                }),
            ),
            Scripted::Text("登记失败。"),
        ],
        false,
    )
    .await;

    chat(&fx).await;

    let result = tool_result_containing(&fx, "acceptance[1]");
    assert!(result.contains("empty"), "{result}");
    assert!(result.contains("Fix:"), "{result}");
}

/// An over-long check reports both the measured length and the limit, so the
/// model knows how much to remove rather than retrying blindly.
#[tokio::test]
async fn an_over_long_acceptance_entry_names_its_length_and_limit() {
    let fx = fixture(
        vec![
            Scripted::Tool(
                "commit_task",
                serde_json::json!({
                    "objective": USER_REQUEST,
                    "acceptance": ["x".repeat(501)]
                }),
            ),
            Scripted::Text("登记失败。"),
        ],
        false,
    )
    .await;

    chat(&fx).await;

    let result = tool_result_containing(&fx, "acceptance[0]");
    assert!(result.contains("501"), "{result}");
    assert!(result.contains("500"), "{result}");
    assert!(result.contains("Fix:"), "{result}");
}
