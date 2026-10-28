//! Dogfood for the Goal route, against a REAL model and the REAL runtime.
//!
//! The reported accident: a session that had already committed and pushed its
//! work entered Goal Mode again for the next ordinary question ("确定都推上去了吗？"),
//! which opened a brand-new Goal, re-applied the completion contract and made
//! the UI claim Goal mode.
//!
//! This drives the real provider (DeepSeek through the configured gateway) and
//! asserts on durable facts only: the session's Goal route, the goal rows, the
//! terminal outcome and the tools the model actually called. Ignored by default
//! because it spends real tokens:
//!
//! ```text
//! DEEPSEEK_API_KEY=... cargo test -p leveler-app \
//!   --test goal_route_dogfood_real -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{EngineStores, GoalRecord, SessionRepository};

const TIMEOUT: Duration = Duration::from_secs(600);

fn write_config(home: &std::path::Path) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        r#"
default_model = "deepseek/deepseek-flash"

[providers.deepseek]
protocol = "openai_chat"
base_url = "https://taotoken.net/api/v1"
api_key_env = "DEEPSEEK_API_KEY"

[models."deepseek-flash"]
provider = "deepseek"
model_id = "deepseek-flash"
protocol = "openai_chat"
context_window = 1048576
max_output_tokens = 65536
reasoning = true
reasoning_style = "thinking_flag"
supported_efforts = ["low", "high", "max"]
reasoning_effort = "low"
thinking_supports_forced_tool_choice = false
passback_reasoning_content = true
"#,
    )
    .unwrap();
}

#[derive(Default, Debug)]
struct TurnFacts {
    tools: Vec<String>,
    text: String,
    terminal: bool,
    failure: Option<String>,
    outcome: Option<String>,
}

impl TurnFacts {
    fn used(&self, tool: &str) -> bool {
        self.tools.iter().any(|name| name == tool)
    }
}

/// Drive one turn and record what the real model did. A turn that never
/// settles is a fact worth printing, not a silent pass.
async fn run_turn(
    client: &Arc<InProcessRuntimeClient>,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    command: ClientCommand,
) -> TurnFacts {
    let mut facts = TurnFacts::default();
    client
        .send(command)
        .await
        .expect("the runtime must accept the command");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline && !facts.terminal {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Ok(RuntimeEvent::ToolCallStarted { name, .. })) => facts.tools.push(name),
            Ok(Ok(RuntimeEvent::AssistantTextDelta { delta, .. })) => facts.text.push_str(&delta),
            Ok(Ok(RuntimeEvent::TurnCompleted)) => {
                facts.terminal = true;
                facts.outcome = Some("completed".into());
            }
            Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { reason })) => {
                facts.terminal = true;
                facts.outcome = Some(format!("completed_with_warnings: {reason}"));
            }
            Ok(Ok(RuntimeEvent::TurnAnswered)) => {
                facts.terminal = true;
                facts.outcome = Some("answered".into());
            }
            Ok(Ok(RuntimeEvent::TurnIncomplete { reason })) => {
                facts.terminal = true;
                facts.outcome = Some(format!("incomplete: {reason}"));
            }
            Ok(Ok(RuntimeEvent::TurnTruncated { error })) => {
                facts.terminal = true;
                facts.outcome = Some(format!("truncated: {error}"));
            }
            Ok(Ok(RuntimeEvent::TurnCancelled)) => {
                facts.terminal = true;
                facts.outcome = Some("interrupted".into());
            }
            Ok(Ok(RuntimeEvent::TaskCancelled)) => {
                facts.terminal = true;
                facts.outcome = Some("cancelled".into());
            }
            Ok(Ok(RuntimeEvent::TurnFailed { error, .. })) => {
                facts.terminal = true;
                facts.failure = Some(error);
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                facts.failure = Some(error.to_string());
                break;
            }
            Err(_) => break,
        }
    }
    println!(
        "  tools={:?} outcome={:?} failure={:?}\n  answer: {}",
        facts.tools,
        facts.outcome,
        facts.failure,
        facts.text.trim()
    );
    facts
}

async fn axis(app: &Application, session: &leveler_core::SessionId) -> String {
    let db = app.open_database().await.unwrap();
    SessionRepository::new(&db)
        .get(session)
        .await
        .unwrap()
        .unwrap()
        .collaboration
}

async fn goals(app: &Application, session: &leveler_core::SessionId) -> Vec<GoalRecord> {
    let db = app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let Some(task) = stores.tasks.task_for_session(session).await.unwrap() else {
        return Vec::new();
    };
    stores.goals.for_task(&task).await.unwrap()
}

#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn a_completed_goal_does_not_reclaim_the_next_ordinary_question() {
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("LEVELER_HOME", home.path());
    }
    write_config(home.path());
    std::fs::create_dir_all(repo.path().join("configs")).unwrap();
    std::fs::create_dir_all(repo.path().join("state")).unwrap();
    leveler_test_support::git::init_repo(repo.path());
    std::fs::write(repo.path().join("seed.txt"), "seed\n").unwrap();
    leveler_test_support::git::run(repo.path(), &["add", "-A"]);
    leveler_test_support::git::run(repo.path(), &["commit", "-qm", "seed"]);

    let app = Arc::new(
        Application::assemble(Layout::from_parts(
            repo.path().to_path_buf(),
            repo.path().join("configs"),
            repo.path().join("state"),
        ))
        .unwrap(),
    );
    let model = ModelRef::new("deepseek", "deepseek-flash");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));

    // ── 1. An explicitly delegated Goal that really commits the work ─────────
    let session = app
        .create_session_with_collaboration(
            &model,
            "deliver hello.txt",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Goal,
        )
        .await
        .unwrap();
    let mut events = client.subscribe_session(&session);
    println!("goal turn:");
    let goal_turn = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "在仓库根目录创建 hello.txt，内容为一行 hello；运行 cat hello.txt 确认；\
                      然后 git add hello.txt 并 commit，提交信息用 add hello。"
                .to_string(),
            attachments: vec![],
        },
    )
    .await;
    assert!(
        goal_turn.failure.is_none(),
        "the goal turn failed: {:?}",
        goal_turn.failure
    );
    assert!(
        goal_turn.used("write_file")
            && (goal_turn.used("run_command") || goal_turn.used("shell_command")),
        "the goal turn must actually write and verify: {:?}",
        goal_turn.tools
    );
    let after_goal = goals(&app, &session).await;
    println!("goals after the goal turn: {after_goal:?}");
    println!("axis after the goal turn: {}", axis(&app, &session).await);
    assert_eq!(after_goal.len(), 1, "one goal ran");
    assert!(
        after_goal[0].state == leveler_storage::GoalState::Settled,
        "the delivered goal must be settled, not left owed: {:?}",
        after_goal[0].state
    );
    assert_eq!(
        axis(&app, &session).await,
        "chat",
        "a completed goal must end its own Goal route"
    );

    // ── 2. The incident sentence, right after the delivery ───────────────────
    println!("the incident question:");
    let question = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "确定都推上去了吗？".to_string(),
            attachments: vec![],
        },
    )
    .await;
    assert!(
        question.failure.is_none(),
        "the question turn failed: {:?}",
        question.failure
    );
    // Either command surface is real execution: the point is that the model
    // inspected the repository instead of asserting from memory, and that the
    // turn was an ordinary one (a Goal turn would have carried `update_goal`).
    assert!(
        question.used("run_command") || question.used("shell_command"),
        "the model must check the repository instead of asserting: {:?}",
        question.tools
    );
    assert!(
        !question.used("update_goal"),
        "a verification question must not run the goal contract: {:?}",
        question.tools
    );
    let lowered = question.text.to_lowercase();
    assert!(
        ["remote", "push", "push", "推"]
            .iter()
            .any(|needle| lowered.contains(needle)),
        "the answer must report what the repository says about pushing: {}",
        question.text
    );
    assert!(
        !question.text.trim().is_empty(),
        "the question must be answered"
    );
    assert_eq!(
        goals(&app, &session).await.len(),
        1,
        "an ordinary question must never open a new Goal"
    );
    assert_eq!(
        axis(&app, &session).await,
        "chat",
        "the question ran as an ordinary turn and left the route in Chat"
    );

    // ── 3. `继续` with nothing to resume must not reactivate the goal ────────
    println!("a stray continuation after completion:");
    let stray = run_turn(
        &client,
        &mut events,
        ClientCommand::ResumeTask {
            session_id: session.clone(),
            content: "继续".to_string(),
        },
    )
    .await;
    println!("  stray outcome={:?}", stray.outcome);
    assert_eq!(
        goals(&app, &session).await.len(),
        1,
        "a completed goal must not be reopened by `继续`"
    );
    assert_eq!(
        axis(&app, &session).await,
        "chat",
        "the route stays Chat after a refused continuation"
    );
}

#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn explicit_goal_still_works_and_ordinary_questions_stay_ordinary() {
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("LEVELER_HOME", home.path());
    }
    write_config(home.path());
    std::fs::create_dir_all(repo.path().join("configs")).unwrap();
    std::fs::create_dir_all(repo.path().join("state")).unwrap();
    leveler_test_support::git::init_repo(repo.path());
    std::fs::write(repo.path().join("README.md"), "# demo\n").unwrap();
    leveler_test_support::git::run(repo.path(), &["add", "-A"]);
    leveler_test_support::git::run(repo.path(), &["commit", "-qm", "seed"]);

    let app = Arc::new(
        Application::assemble(Layout::from_parts(
            repo.path().to_path_buf(),
            repo.path().join("configs"),
            repo.path().join("state"),
        ))
        .unwrap(),
    );
    let model = ModelRef::new("deepseek", "deepseek-flash");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));

    // ── A. An ordinary question in a fresh Chat session stays ordinary ───────
    let chat = app
        .create_session_with_collaboration(
            &model,
            "conversation",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    let mut events = client.subscribe_session(&chat);
    println!("an ordinary explanation request:");
    let explain = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: chat.clone(),
            content: "解释一下这个仓库里 README.md 写了什么，不要修改任何文件。".to_string(),
            attachments: vec![],
        },
    )
    .await;
    assert!(explain.failure.is_none(), "{:?}", explain.failure);
    assert!(
        goals(&app, &chat).await.is_empty(),
        "a question must not commit a Goal: {:?}",
        goals(&app, &chat).await
    );
    assert_eq!(axis(&app, &chat).await, "chat");

    // ── B. `/goal` is still the explicit, forced entry ──────────────────────
    println!("an explicit /goal:");
    let forced = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: chat.clone(),
            content: "/goal 把 README.md 的标题改成 demo-app，运行 cat README.md 验证后 commit"
                .to_string(),
            attachments: vec![],
        },
    )
    .await;
    assert!(forced.failure.is_none(), "{:?}", forced.failure);
    let forced_goals = goals(&app, &chat).await;
    println!("  goals={forced_goals:?} axis={}", axis(&app, &chat).await);
    assert_eq!(forced_goals.len(), 1, "`/goal` must still create the goal");
    assert!(
        forced_goals[0].objective.contains("demo-app"),
        "the objective is the user's task: {}",
        forced_goals[0].objective
    );
    assert_eq!(
        axis(&app, &chat).await,
        "chat",
        "the goal was delivered, so the route ended with it"
    );

    // ── C. Real work with several deliverables may upgrade itself ───────────
    // The model decides; the runtime must not. Either answer is acceptable,
    // and the objective must never be narrowed or enlarged by the upgrade.
    println!("an execution request with several deliverables:");
    let work = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: chat.clone(),
            content: "新增 LICENSE 文件（MIT）并在 README.md 末尾加一个 License 小节，\
                      两处都完成后运行 git status 确认，再报告结果。"
                .to_string(),
            attachments: vec![],
        },
    )
    .await;
    assert!(work.failure.is_none(), "{:?}", work.failure);
    let upgraded = goals(&app, &chat).await;
    println!(
        "  auto-upgraded={} axis={} tools={:?}",
        upgraded.len() > 1,
        axis(&app, &chat).await,
        work.tools
    );
    // `for_task` is newest first, so an upgrade is the newest goal (if any).
    if let Some(goal) = upgraded
        .first()
        .filter(|goal| goal.objective.contains("LICENSE"))
    {
        assert!(
            goal.objective.contains("README"),
            "an upgrade must keep the whole stated request, not narrow it: {}",
            goal.objective
        );
    }
    assert!(
        work.used("write_file") || work.used("apply_patch"),
        "the execution request must really edit files: {:?}",
        work.tools
    );
}

/// A Goal that was interrupted is still owed: `继续` must re-enter THAT goal
/// (identity preserved, no sibling goal), and only its completion ends the
/// route. This is the other half of the contract the incident broke.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn an_interrupted_goal_resumes_as_the_same_goal() {
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("LEVELER_HOME", home.path());
    }
    write_config(home.path());
    std::fs::create_dir_all(repo.path().join("configs")).unwrap();
    std::fs::create_dir_all(repo.path().join("state")).unwrap();
    leveler_test_support::git::init_repo(repo.path());
    std::fs::write(repo.path().join("NOTES.md"), "# notes\n").unwrap();
    leveler_test_support::git::run(repo.path(), &["add", "-A"]);
    leveler_test_support::git::run(repo.path(), &["commit", "-qm", "seed"]);

    let app = Arc::new(
        Application::assemble(Layout::from_parts(
            repo.path().to_path_buf(),
            repo.path().join("configs"),
            repo.path().join("state"),
        ))
        .unwrap(),
    );
    let model = ModelRef::new("deepseek", "deepseek-flash");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = app
        .create_session_with_collaboration(
            &model,
            "resumable goal",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Goal,
        )
        .await
        .unwrap();
    let mut events = client.subscribe_session(&session);

    println!("a goal window that will be interrupted:");
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "第一步先运行 `sleep 20`（这是任务的一部分，必须真的跑完），然后在 NOTES.md \
                      末尾追加一行 done，并运行 cat NOTES.md 确认。"
                .to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    // Interrupt once the model has actually started executing something.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut started = false;
    while tokio::time::Instant::now() < deadline && !started {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Ok(Ok(RuntimeEvent::ToolCallStarted { name, .. })) =
            tokio::time::timeout(left, events.recv()).await
        {
            println!("  started tool: {name}");
            started = true;
        }
    }
    assert!(started, "the model never started executing");
    client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    let mut interrupted = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while tokio::time::Instant::now() < deadline && !interrupted {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Ok(event)) = tokio::time::timeout(left, events.recv()).await else {
            break;
        };
        interrupted = matches!(
            event,
            RuntimeEvent::TurnCancelled | RuntimeEvent::TurnFailed { .. }
        );
    }
    let owed = goals(&app, &session).await;
    println!(
        "  interrupted={interrupted} goals={:?} axis={}",
        owed.iter()
            .map(|g| (&g.objective, g.state))
            .collect::<Vec<_>>(),
        axis(&app, &session).await
    );
    assert!(interrupted, "the window must be interrupted, not completed");
    assert_eq!(owed.len(), 1);
    assert_eq!(
        owed[0].state,
        leveler_storage::GoalState::Running,
        "an interrupted goal is still owed"
    );
    assert_eq!(
        axis(&app, &session).await,
        "goal",
        "owed work keeps the session on the Goal route"
    );

    println!("continue it:");
    let resumed = run_turn(
        &client,
        &mut events,
        ClientCommand::ResumeTask {
            session_id: session.clone(),
            content: "继续".to_string(),
        },
    )
    .await;
    assert!(resumed.failure.is_none(), "{:?}", resumed.failure);
    let after = goals(&app, &session).await;
    println!(
        "  goals after resume: {:?} axis={}",
        after
            .iter()
            .map(|g| (&g.objective, g.state))
            .collect::<Vec<_>>(),
        axis(&app, &session).await
    );
    assert_eq!(
        after.len(),
        1,
        "a continuation resumes the same goal instead of opening a sibling"
    );
    assert_eq!(
        after[0].objective, owed[0].objective,
        "identity is preserved across the window"
    );
    assert_eq!(
        after[0].state,
        leveler_storage::GoalState::Settled,
        "the resumed window delivered the work"
    );
    assert_eq!(
        axis(&app, &session).await,
        "chat",
        "the delivered goal ended the route"
    );
}

// ── Shared harness for the routing lifecycle scenarios ───────────────────────

/// One real provider + real runtime + real SQLite, with an isolated home and
/// repository. Tests run one at a time (`--test-threads=1`): `LEVELER_HOME` is
/// process-wide.
struct RealFixture {
    app: Arc<Application>,
    client: Arc<InProcessRuntimeClient>,
    model: ModelRef,
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
}

impl RealFixture {
    async fn new() -> Self {
        assert!(
            std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
            "DEEPSEEK_API_KEY must carry the gateway key"
        );
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("LEVELER_HOME", home.path());
        }
        write_config(home.path());
        std::fs::create_dir_all(repo.path().join("configs")).unwrap();
        std::fs::create_dir_all(repo.path().join("state")).unwrap();
        leveler_test_support::git::init_repo(repo.path());
        std::fs::write(repo.path().join("README.md"), "# demo\n").unwrap();
        std::fs::write(repo.path().join("config.yaml"), "name: demo\n").unwrap();
        leveler_test_support::git::run(repo.path(), &["add", "-A"]);
        leveler_test_support::git::run(repo.path(), &["commit", "-qm", "seed"]);
        let app = Arc::new(
            Application::assemble(Layout::from_parts(
                repo.path().to_path_buf(),
                repo.path().join("configs"),
                repo.path().join("state"),
            ))
            .unwrap(),
        );
        let model = ModelRef::new("deepseek", "deepseek-flash");
        let client = Arc::new(InProcessRuntimeClient::new(
            app.clone(),
            model.clone(),
            PermissionProfile::FullAccess,
            false,
        ));
        Self {
            app,
            client,
            model,
            home,
            repo,
        }
    }

    async fn session(&self, objective: &str, goal_axis: bool) -> leveler_core::SessionId {
        let axis = if goal_axis {
            leveler_agent::CollaborationMode::Goal
        } else {
            leveler_agent::CollaborationMode::Chat
        };
        self.app
            .create_session_with_collaboration(
                &self.model,
                objective,
                PermissionProfile::FullAccess,
                axis,
            )
            .await
            .unwrap()
    }

    async fn axis(&self, session: &leveler_core::SessionId) -> String {
        axis(&self.app, session).await
    }

    async fn goals(&self, session: &leveler_core::SessionId) -> Vec<GoalRecord> {
        goals(&self.app, session).await
    }

    async fn turn_kinds(&self, session: &leveler_core::SessionId) -> Vec<String> {
        let db = self.app.open_database().await.unwrap();
        leveler_storage::TurnRepository::new(&db)
            .list(session)
            .await
            .unwrap()
            .into_iter()
            .map(|turn| turn.kind)
            .collect()
    }
}

/// Scenario A — a blocked goal hands the conversation back, keeps its identity,
/// and `继续` re-enters THAT goal.
///
/// The block is real: the task requires a deployment token the repository does
/// not contain, and the objective says not to invent one.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn a_blocked_goal_answers_a_question_and_resumes_itself() {
    let f = RealFixture::new().await;
    let objective = "在仓库根目录创建 deploy.sh 并读取 secret/prod.token，把它写入 .env 的 \
                     DEPLOY_TOKEN。如果 secret/prod.token 不存在，就用 \
                     update_goal(status=\"blocked\") 说明缺少什么，禁止编造令牌。";
    let session = f.session(objective, true).await;
    let mut events = f.client.subscribe_session(&session);

    println!("goal window (expected to block):");
    let blocked = run_turn(
        &f.client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: objective.to_string(),
            attachments: vec![],
        },
    )
    .await;
    assert!(blocked.failure.is_none(), "{:?}", blocked.failure);
    let after_block = f.goals(&session).await;
    println!("  goals={after_block:?} axis={}", f.axis(&session).await);
    assert_eq!(after_block.len(), 1, "one goal for one delegated task");
    let goal_id = after_block[0].id.clone();
    assert_eq!(after_block[0].objective, objective);
    assert_eq!(
        after_block[0].state,
        leveler_storage::GoalState::Running,
        "a blocked goal still owes its work: {after_block:?}"
    );
    assert_eq!(
        f.axis(&session).await,
        "chat",
        "the closed window no longer claims the next message"
    );

    println!("the user asks why:");
    let question = run_turn(
        &f.client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "为什么会被阻塞？具体缺的是什么？请只回答，不要修改任何文件。".to_string(),
            attachments: vec![],
        },
    )
    .await;
    assert!(question.failure.is_none(), "{:?}", question.failure);
    assert!(!question.text.trim().is_empty(), "the question is answered");
    let after_question = f.goals(&session).await;
    println!("  goals={after_question:?} axis={}", f.axis(&session).await);
    assert_eq!(
        after_question.len(),
        1,
        "a question must never open a Goal: {after_question:?}"
    );
    assert_eq!(
        after_question[0].id, goal_id,
        "the goal's identity survives the question"
    );
    assert_eq!(after_question[0].objective, objective);
    assert!(
        after_question[0].amendments.is_empty(),
        "a question is not scope on the goal: {:?}",
        after_question[0].amendments
    );
    assert_eq!(
        f.turn_kinds(&session).await.last().map(String::as_str),
        Some("chat"),
        "the question ran as an ordinary Chat turn"
    );

    println!("the user asks to continue:");
    let resumed = run_turn(
        &f.client,
        &mut events,
        ClientCommand::ResumeTask {
            session_id: session.clone(),
            content: "继续".to_string(),
        },
    )
    .await;
    assert!(resumed.failure.is_none(), "{:?}", resumed.failure);
    let after_resume = f.goals(&session).await;
    println!("  goals={after_resume:?} axis={}", f.axis(&session).await);
    assert_eq!(
        after_resume.len(),
        1,
        "`继续` must re-enter the blocked goal, not open a sibling: {after_resume:?}"
    );
    assert_eq!(after_resume[0].id, goal_id, "same goal identity");
    assert_eq!(after_resume[0].objective, objective);
    assert!(
        after_resume[0].windows_run >= 2,
        "the resumed window is counted on the same goal: {}",
        after_resume[0].windows_run
    );

    drop((f.home, f.repo));
}

/// Scenario C — a Chat turn commits its own work through `commit_task`, the
/// contract is real (identity, acceptance, gate), and an interruption does not
/// lose or duplicate the task: `继续` resumes the SAME goal.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn a_committed_chat_task_survives_an_interruption_and_resumes() {
    let f = RealFixture::new().await;
    // The user says the work must be carried to completion before stopping:
    // that is the evidence `commit_task` exists for.
    let request = "这是一个多阶段、必须全部做到完成才能停的任务：阶段一在 docs/ 下创建 A.md \
                   写一行说明；阶段二创建 docs/B.md 写一行说明；阶段三创建 docs/INDEX.md 列出 \
                   前两个文件；最后运行 ls docs 确认三个文件都在。中途不要用阶段总结结束，\
                   全部做完才算完成。";
    let session = f.session("docs delivery", false).await;
    let mut events = f.client.subscribe_session(&session);

    // Commit, then interrupt the window before it can finish: the task must
    // survive as the same goal, not only inside the turn that declared it.
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: request.to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut tools = Vec::new();
    // `ToolCallCompleted` carries no name; the started event is the mapping.
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut committed = false;
    let mut terminal = false;
    while tokio::time::Instant::now() < deadline && !terminal {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Ok(RuntimeEvent::ToolCallStarted { id, name, .. })) => {
                tools.push(name.clone());
                names.insert(id.as_str().to_string(), name);
            }
            Ok(Ok(RuntimeEvent::ToolCallCompleted { id, ok, .. })) => {
                let name = names.get(id.as_str()).cloned().unwrap_or_default();
                // A successful commit is durable before its result is
                // published; only then is the window interrupted.
                if name == "commit_task" && ok && !committed {
                    committed = true;
                    f.client
                        .send(ClientCommand::CancelCurrentTurn {
                            session_id: session.clone(),
                        })
                        .await
                        .unwrap();
                }
            }
            Ok(Ok(RuntimeEvent::TurnCompleted))
            | Ok(Ok(RuntimeEvent::TurnAnswered))
            | Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { .. }))
            | Ok(Ok(RuntimeEvent::TurnIncomplete { .. }))
            | Ok(Ok(RuntimeEvent::TurnTruncated { .. }))
            | Ok(Ok(RuntimeEvent::TurnFailed { .. }))
            | Ok(Ok(RuntimeEvent::TurnCancelled))
            | Ok(Ok(RuntimeEvent::TaskCancelled)) => {
                terminal = true;
                if committed {
                    // If the model finished before the cancel landed, the goal
                    // was really completed; that is a valid outcome, not a
                    // routing failure.
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => panic!("event stream failed: {error}"),
            Err(_) => break,
        }
    }
    println!("  tools={tools:?} committed={committed}");
    assert!(
        committed,
        "the model must declare execution work with commit_task: {tools:?}"
    );
    let after_cancel = f.goals(&session).await;
    println!("  goals={after_cancel:?} axis={}", f.axis(&session).await);
    assert_eq!(after_cancel.len(), 1, "{after_cancel:?}");
    let goal_id = after_cancel[0].id.clone();
    assert!(
        after_cancel[0].objective.contains("A.md") && after_cancel[0].objective.contains("B.md"),
        "the upgrade keeps the whole stated request: {}",
        after_cancel[0].objective
    );
    assert!(
        !after_cancel[0].acceptance.is_empty(),
        "the commit records the checks it will be judged against"
    );
    assert_eq!(
        f.axis(&session).await,
        "chat",
        "a Chat-delegated task does not turn the session into Goal mode"
    );

    println!("continue after the interruption:");
    let resumed = run_turn(
        &f.client,
        &mut events,
        ClientCommand::ResumeTask {
            session_id: session.clone(),
            content: "继续".to_string(),
        },
    )
    .await;
    assert!(resumed.failure.is_none(), "{:?}", resumed.failure);
    let after_resume = f.goals(&session).await;
    println!("  goals={after_resume:?} axis={}", f.axis(&session).await);
    assert_eq!(
        after_resume.len(),
        1,
        "resume must not open a second task identity: {after_resume:?}"
    );
    assert_eq!(after_resume[0].id, goal_id, "same goal identity");
    assert_eq!(
        after_resume[0].objective, after_cancel[0].objective,
        "the objective is never rewritten by a continuation"
    );
    assert!(
        after_resume[0].windows_run >= 2,
        "the resumed window belongs to the same goal: {}",
        after_resume[0].windows_run
    );

    drop((f.home, f.repo));
}
