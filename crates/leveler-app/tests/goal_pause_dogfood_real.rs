//! Real-provider dogfood for the user's stage-pause authorization boundary.
//!
//! The question this pins: when the model has bound a full two-stage Goal and
//! the user said "finish stage 1, then wait for my confirmation", does the real
//! runtime keep stage 2 unauthorized, answer an ordinary status question
//! without resuming work, and then continue the SAME goal when the user
//! authorizes it?
//!
//! Ignored by default because it spends real tokens:
//!
//! ```text
//! set -a; . ~/.leveler/deepseek.env; set +a
//! cargo test -p leveler-app --test goal_pause_dogfood_real -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{EngineStores, GoalRecord, GoalState, SessionRepository, TurnRepository};

const TIMEOUT: Duration = Duration::from_secs(900);

fn write_config(home: &std::path::Path) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        r#"
default_model = "deepseek/deepseek-v4.1-flash-1017"

[providers.deepseek]
protocol = "openai_chat"
base_url = "https://taotoken.net/api/v1"
api_key_env = "DEEPSEEK_API_KEY"

[models."deepseek-v4.1-flash-1017"]
provider = "deepseek"
model_id = "deepseek-v4.1-flash-1017"
protocol = "openai_chat"
context_window = 1048576
reliable_context = 786432
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
    plans: Vec<Vec<(String, String)>>,
    text: String,
    terminal: bool,
    failure: Option<String>,
    outcome: Option<String>,
    /// Every goal projection the runtime published while this turn ran. A
    /// client's paused badge reads exactly these rows.
    tasks: Vec<leveler_client_protocol::UiExecutionTask>,
}

impl TurnFacts {
    fn used(&self, tool: &str) -> bool {
        self.tools.iter().any(|name| name == tool)
    }
    fn paused(&self) -> bool {
        self.outcome.as_deref() == Some("paused")
    }
}

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
            Ok(Ok(RuntimeEvent::PlanUpdated { plan, .. })) => facts.plans.push(
                plan.steps
                    .iter()
                    .map(|step| {
                        (
                            step.description.clone(),
                            format!("{:?}", step.status).to_lowercase(),
                        )
                    })
                    .collect(),
            ),
            Ok(Ok(RuntimeEvent::AssistantTextDelta { delta, .. })) => facts.text.push_str(&delta),
            Ok(Ok(RuntimeEvent::SessionUpdated { session })) => {
                if let Some(task) = session.execution_task {
                    facts.tasks.push(task);
                }
            }
            // A quiet round the harness re-drove after a closeout. It is the
            // visible shape of a continuation on a real provider run.
            Ok(Ok(RuntimeEvent::Notification { message, .. })) => {
                if message.contains("催办") || message.contains("goal") {
                    facts.text.push_str(&format!("\n[notice] {message}"));
                }
            }
            Ok(Ok(RuntimeEvent::TurnCompleted)) => {
                facts.terminal = true;
                facts.outcome = Some("completed".into());
            }
            Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { reason, .. })) => {
                facts.terminal = true;
                facts.outcome = Some(format!("completed_with_warnings: {reason}"));
            }
            Ok(Ok(RuntimeEvent::TurnAnswered)) => {
                facts.terminal = true;
                facts.outcome = Some("answered".into());
            }
            Ok(Ok(RuntimeEvent::TurnPaused { .. })) => {
                facts.terminal = true;
                facts.outcome = Some("paused".into());
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
    println!("  plans={:?}", facts.plans);
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

async fn turn_kinds(app: &Application, session: &leveler_core::SessionId) -> Vec<String> {
    let db = app.open_database().await.unwrap();
    TurnRepository::new(&db)
        .list(session)
        .await
        .unwrap()
        .into_iter()
        .map(|turn| format!("{}:{}", turn.kind, turn.status))
        .collect()
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir, Arc<Application>) {
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
    (home, repo, app)
}

fn staged_request() -> &'static str {
    "请先把这件事用 commit_task 登记成一个需要持续跟踪的任务，然后完成两个阶段：\
     阶段 1 在仓库根目录创建 stage-1.txt，内容为一行 `stage 1`，然后运行 \
     `sh -c 'cat stage-1.txt'` 确认；阶段 1 完成后停下，等待我确认再执行阶段 2；\
     阶段 2 创建 stage-2.txt，内容为一行 `stage 2`，然后运行 \
     `sh -c 'cat stage-2.txt'` 确认。开始前先用 update_plan 记录这两个阶段。"
}

#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn goal_pause_query_resume() {
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = app
        .create_session_with_collaboration(
            &model,
            "two stages with a user pause",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    let mut events = client.subscribe_session(&session);
    println!(
        "D-GOAL-PAUSE session={} repo={}",
        session.as_str(),
        repo.path().display()
    );

    // ── 1. PAUSE ────────────────────────────────────────────────────────────
    let paused = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: staged_request().to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(
        paused.failure.is_none(),
        "the pause turn failed: {paused:?}"
    );
    let after_pause = goals(&app, &session).await;
    println!("goals after the pause turn: {after_pause:?}");
    println!("axis: {}", axis(&app, &session).await);
    println!("turns: {:?}", turn_kinds(&app, &session).await);
    assert_eq!(after_pause.len(), 1, "one durable task");
    let goal_id = after_pause[0].id.clone();
    assert!(
        paused.used("commit_task"),
        "the model must bind the full task: {:?}",
        paused.tools
    );
    assert_eq!(
        after_pause[0].state,
        GoalState::Running,
        "the goal is not concluded by the pause: {after_pause:?}"
    );
    assert!(
        after_pause[0].user_paused_at.is_some(),
        "the user's pause is durable: {after_pause:?}"
    );
    let acceptance_after_pause = after_pause[0].acceptance.clone();
    assert!(
        acceptance_after_pause.len() >= 2,
        "both stages stay in the acceptance plan: {after_pause:?}"
    );
    assert!(
        repo.path().join("stage-1.txt").exists(),
        "stage 1 must really exist"
    );
    assert!(
        !repo.path().join("stage-2.txt").exists(),
        "stage 2 is NOT authorized yet"
    );
    assert!(
        paused.paused(),
        "the turn must end at the pause, not as a stall: {paused:?}"
    );

    // ── 2. QUERY ────────────────────────────────────────────────────────────
    println!("D-GOAL-QUERY");
    let query = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "刚才阶段 1 修改了哪些文件？测试通过了吗？".to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(query.failure.is_none(), "the query turn failed: {query:?}");
    let after_query = goals(&app, &session).await;
    println!("goals after the query: {after_query:?}");
    assert_eq!(after_query.len(), 1, "a question opens no second task");
    assert_eq!(after_query[0].id, goal_id, "same goal identity");
    assert_eq!(after_query[0].state, GoalState::Running);
    assert!(
        after_query[0].user_paused_at.is_some(),
        "the question must not release the pause: {after_query:?}"
    );
    assert!(
        !repo.path().join("stage-2.txt").exists(),
        "the question must not authorize stage 2"
    );
    assert!(
        !query.text.trim().is_empty() && !query.text.to_lowercase().contains("stage 2 已创建"),
        "the query is answered without pretending the withheld stage exists: {}",
        query.text
    );

    // ── 3. RESUME ───────────────────────────────────────────────────────────
    println!("D-GOAL-RESUME");
    let resumed = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "确认，继续阶段 2，把剩余任务完成并验收。".to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(
        resumed.failure.is_none(),
        "the resume turn failed: {resumed:?}"
    );
    let after_resume = goals(&app, &session).await;
    println!("goals after the resume: {after_resume:?}");
    println!("turns: {:?}", turn_kinds(&app, &session).await);
    assert_eq!(after_resume.len(), 1, "still one goal");
    assert_eq!(after_resume[0].id, goal_id, "resumed by identity");
    assert_eq!(
        after_resume[0].state,
        GoalState::Settled,
        "the full acceptance is satisfied: {after_resume:?}"
    );
    assert!(
        after_resume[0].user_paused_at.is_none(),
        "authorization cleared the pause"
    );
    assert_eq!(
        after_resume[0].acceptance, acceptance_after_pause,
        "the acceptance plan was not narrowed or replaced: {after_resume:?}"
    );
    assert!(
        resumed.used("write_file") || resumed.used("apply_patch"),
        "the resume must do real work: {:?}",
        resumed.tools
    );
    assert!(
        repo.path().join("stage-2.txt").exists(),
        "stage 2 is really delivered"
    );
    assert_eq!(
        resumed.outcome.as_deref(),
        Some("completed"),
        "the goal closes through verification: {resumed:?}"
    );
}

#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn goal_continue_without_a_pause() {
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = app
        .create_session_with_collaboration(
            &model,
            "two stages in one go",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    let mut events = client.subscribe_session(&session);
    println!("D-GOAL-CONTINUE session={}", session.as_str());
    let run = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content:
                "请先用 commit_task 登记下面这件事，然后一次完成两个阶段，中间不要停下来等我确认：\
                      阶段 1 在仓库根目录创建 stage-1.txt（内容为一行 `stage 1`），\
                      并运行 sh -c 'cat stage-1.txt' 确认输出；\
                      阶段 2 在仓库根目录创建 stage-2.txt（内容为一行 `stage 2`），\
                      并运行 sh -c 'cat stage-2.txt' 确认输出。"
                    .to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(run.failure.is_none(), "the run failed: {run:?}");
    assert!(
        run.used("commit_task"),
        "the task must be committed: {:?}",
        run.tools
    );
    assert!(
        repo.path().join("stage-1.txt").exists() && repo.path().join("stage-2.txt").exists(),
        "both stages are really delivered: {:?}",
        run.tools
    );
    let goals = goals(&app, &session).await;
    println!("goals: {goals:?}");
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].state, GoalState::Settled);
    assert!(
        goals[0].user_paused_at.is_none(),
        "no pause was ever reached"
    );
    assert_eq!(run.outcome.as_deref(), Some("completed"));
}

/// Pressure scenario: the user asks for a progress note after stage 1 but does
/// NOT pause. A model that ends its round there must be re-driven. A real model
/// that simply finishes both stages in one window is a valid result and this
/// branch is then NOT VERIFIED for the real provider; the deterministic
/// Executor test covers the early-closeout decision.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn goal_early_closeout_pressure() {
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = app
        .create_session_with_collaboration(
            &model,
            "two stages, progress note after stage one",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Goal,
        )
        .await
        .unwrap();
    let mut events = client.subscribe_session(&session);
    println!("D-GOAL-EARLY-CLOSEOUT session={}", session.as_str());
    let run = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "完成两个阶段：阶段 1 在仓库根目录创建 stage-1.txt（内容为一行 `stage 1`）\
                      并运行 sh -c 'cat stage-1.txt' 确认；阶段 2 在仓库根目录创建 stage-2.txt\
                      （内容为一行 `stage 2`）并运行 sh -c 'cat stage-2.txt' 确认。\
                      阶段 1 完成后先写一段进度小结，然后继续完成阶段 2——\
                      我没有说要停下等你确认，中间不要停。"
                .to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(run.failure.is_none(), "the run failed: {run:?}");
    let goals = goals(&app, &session).await;
    println!("goals: {goals:?} outcome={:?}", run.outcome);
    let both = repo.path().join("stage-1.txt").exists() && repo.path().join("stage-2.txt").exists();
    if both
        && goals
            .first()
            .is_some_and(|goal| goal.state == GoalState::Settled)
    {
        println!(
            "NOT VERIFIED (real provider): the model did not end early; it completed both stages \
             in one window. The early-closeout branch is covered by the deterministic Executor test."
        );
        return;
    }
    // If it DID end early, the harness must have driven it on and the goal must
    // not be a quiet, resolved task.
    assert_eq!(
        goals.first().map(|goal| goal.state),
        Some(GoalState::Running),
        "a premature summary is not a completed goal: {goals:?}"
    );
    assert!(
        !run.paused(),
        "no user pause was authorized, so the round must not end as paused: {run:?}"
    );
    panic!(
        "the run ended early without completing the goal: outcome={:?} text={}",
        run.outcome,
        run.text.trim()
    );
}

/// D-QUESTION: a status query changes no scope, and a request asked as a
/// question still reaches the goal's acceptance.
///
/// The two facts are asserted against the runtime, not the model's prose: the
/// pause stays recorded across the queries, no file appears that only an
/// authorization would have produced, and the question-form request really
/// lands in the durable amendment list and in the delivered workspace.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn goal_question_form_amendment() {
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = app
        .create_session_with_collaboration(
            &model,
            "a pause, two queries, then a request asked as a question",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    let mut events = client.subscribe_session(&session);
    println!("D-QUESTION session={}", session.as_str());

    // ── 1. PAUSE ────────────────────────────────────────────────────────────
    let paused = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: staged_request().to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(
        paused.failure.is_none(),
        "the pause turn failed: {paused:?}"
    );
    assert!(paused.used("commit_task"), "tools: {:?}", paused.tools);
    let goal_id = goals(&app, &session).await[0].id.clone();

    // ── 2. QUERIES ─────────────────────────────────────────────────────────
    for question in ["测试通过了吗？", "现在进度怎么样？"] {
        println!("D-QUESTION-QUERY {question}");
        let query = run_turn(
            &client,
            &mut events,
            ClientCommand::SubmitMessage {
                session_id: session.clone(),
                content: question.to_string(),
                attachments: Vec::new(),
            },
        )
        .await;
        assert!(query.failure.is_none(), "the query turn failed: {query:?}");
        let after = goals(&app, &session).await;
        assert_eq!(after.len(), 1, "a question opens no second task: {after:?}");
        assert_eq!(after[0].id, goal_id, "same goal identity");
        assert_eq!(after[0].state, GoalState::Running);
        assert!(
            after[0].user_paused_at.is_some(),
            "a question never releases the user's pause: {after:?}"
        );
        assert!(
            after[0].amendments.is_empty(),
            "a status query is not scope authority: {after:?}"
        );
        assert!(
            !repo.path().join("stage-2.txt").exists(),
            "a status query authorizes nothing: {:?}",
            query.tools
        );
    }

    // ── 3. A REQUEST ASKED AS A QUESTION ───────────────────────────────────
    println!("D-QUESTION-REQUEST");
    let continuation = "确认，继续阶段 2，可以吗？另外能不能顺便把 stage-extra.txt 也补上\
                        （内容为一行 `extra`），然后运行 sh -c 'cat stage-extra.txt' 确认？";
    let run = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: continuation.to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(run.failure.is_none(), "the request turn failed: {run:?}");
    let after = goals(&app, &session).await;
    println!("goals after the question-form request: {after:?}");
    assert_eq!(after.len(), 1, "still the same goal: {after:?}");
    assert_eq!(after[0].id, goal_id, "no second goal is created");
    assert!(
        after[0]
            .amendments
            .iter()
            .any(|amendment| amendment.contains("stage-extra.txt")),
        "the question-form request is durable scope authority: {after:?}"
    );
    assert_eq!(
        after[0].state,
        GoalState::Settled,
        "the goal closes through verification, including the new deliverable: {after:?}"
    );
    assert!(
        after[0].user_paused_at.is_none(),
        "the continuation authorization released the pause"
    );
    assert!(
        repo.path().join("stage-2.txt").exists(),
        "the authorized stage really ran: {:?}",
        run.tools
    );
    assert!(
        repo.path().join("stage-extra.txt").exists(),
        "the question-form request was really delivered, not silently dropped: {:?}",
        run.tools
    );
    assert!(
        run.used("write_file") || run.used("apply_patch"),
        "real tool execution: {:?}",
        run.tools
    );
    // The new deliverable entered the acceptance gate: the run closes either
    // cleanly or with the honest warning that its evidence for the added item
    // is the model's account. Both are accepted completions; a stall is not.
    let outcome = run.outcome.clone().unwrap_or_default();
    assert!(
        outcome == "completed"
            || (outcome.starts_with("completed_with_warnings") && outcome.contains("extra")),
        "the goal closes through verification, and the added item is named in any \
         remaining warning: {run:?}"
    );
}

/// D-MULTIGOAL: two goals in one session never share an authorization.
///
/// Goal A is paused and then legally finished; goal B is opened afterwards with
/// "finish it in one go". B must carry no pause of its own, and no published
/// projection may keep claiming A is still owed.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn goal_multi_goal_isolation() {
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = app
        .create_session_with_collaboration(
            &model,
            "two sequential goals",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    let mut events = client.subscribe_session(&session);
    println!("D-MULTIGOAL session={}", session.as_str());

    // ── 1. GOAL A pauses ────────────────────────────────────────────────────
    let paused = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: staged_request().to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(paused.failure.is_none(), "goal A failed: {paused:?}");
    let goal_a = goals(&app, &session).await;
    assert_eq!(goal_a.len(), 1, "one goal so far: {goal_a:?}");
    let goal_a_id = goal_a[0].id.clone();
    assert!(goal_a[0].user_paused_at.is_some(), "{goal_a:?}");
    let projection = client.snapshot(&session).await.unwrap().execution_task;
    let projection = projection.expect("the paused goal is projected");
    assert_eq!(projection.goal_id, goal_a_id.as_str());
    assert!(projection.user_paused, "the projection carries the pause");
    assert!(
        !projection.running,
        "a paused goal has no executing window: {projection:?}"
    );
    let mut published = paused.tasks.clone();

    // ── 2. GOAL A legally finishes ──────────────────────────────────────────
    let resumed = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "确认，继续阶段 2，把剩余任务完成并验收。".to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(
        resumed.failure.is_none(),
        "goal A resume failed: {resumed:?}"
    );
    let goal_a = goals(&app, &session).await;
    assert_eq!(goal_a[0].state, GoalState::Settled, "{goal_a:?}");
    assert!(
        client
            .snapshot(&session)
            .await
            .unwrap()
            .execution_task
            .is_none(),
        "a settled goal is no longer owed"
    );
    published.extend(resumed.tasks.iter().cloned());

    // ── 3. GOAL B, one go, no pause ─────────────────────────────────────────
    let second = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "请先用 commit_task 登记这件事，然后一次完成，中间不要停下来等我确认：\
                      创建 export-b1.txt（内容一行 `b1`）并运行 sh -c 'cat export-b1.txt' 确认；\
                      创建 export-b2.txt（内容一行 `b2`）并运行 sh -c 'cat export-b2.txt' 确认。"
                .to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(second.failure.is_none(), "goal B failed: {second:?}");
    assert!(second.used("commit_task"), "tools: {:?}", second.tools);
    let goals_now = goals(&app, &session).await;
    println!("goals after goal B: {goals_now:?}");
    assert_eq!(goals_now.len(), 2, "two distinct goals: {goals_now:?}");
    let goal_b = goals_now
        .iter()
        .find(|goal| goal.id != goal_a_id)
        .expect("goal B exists");
    assert_ne!(goal_b.id, goal_a_id);
    assert!(
        goal_b.user_paused_at.is_none(),
        "goal B carries no pause of its own: {goal_b:?}"
    );
    assert_eq!(goal_b.state, GoalState::Settled, "{goal_b:?}");
    published.extend(second.tasks.iter().cloned());
    println!("published goal projections: {published:?}");
    assert!(
        published
            .iter()
            .any(|task| task.goal_id == goal_a_id.as_str() && task.user_paused),
        "the pause is a published projection, not a client guess: {published:?}"
    );
    for task in &published {
        if task.goal_id == goal_a_id.as_str() {
            assert!(
                task.user_paused && !task.running,
                "goal A is only ever projected as the user's pause: {task:?}"
            );
        } else {
            assert_eq!(task.goal_id, goal_b.id.as_str(), "{task:?}");
            assert!(
                !task.user_paused,
                "goal B never inherits goal A's pause: {task:?}"
            );
        }
    }
    assert_eq!(
        client.snapshot(&session).await.unwrap().execution_task,
        None,
        "goal B settled, so nothing is owed"
    );
    assert!(
        repo.path().join("export-b1.txt").exists() && repo.path().join("export-b2.txt").exists(),
        "goal B really delivered: {:?}",
        second.tools
    );
    let outcome = second.outcome.clone().unwrap_or_default();
    assert!(
        outcome == "completed" || outcome.starts_with("completed_with_warnings"),
        "goal B closes through verification: {second:?}"
    );
}

/// D-CHAT-AMEND: a supplement the user states to a paused goal, with no `继续`,
/// is durable scope — and it neither releases the pause nor starts the withheld
/// stage. An unrelated request never joins the goal. The user's own
/// authorization afterwards continues the SAME goal and covers the supplement.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn goal_chat_amend_without_the_word_continue() {
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = app
        .create_session_with_collaboration(
            &model,
            "chat amendment without a resume keyword",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    let mut events = client.subscribe_session(&session);
    println!("D-CHAT-AMEND session={}", session.as_str());

    // ── 1. the goal reaches the user's own pause ────────────────────────────
    let paused = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: staged_request().to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(
        paused.failure.is_none(),
        "the pause turn failed: {paused:?}"
    );
    let after_pause = goals(&app, &session).await;
    assert_eq!(after_pause.len(), 1, "{after_pause:?}");
    let goal_id = after_pause[0].id.clone();
    assert!(
        after_pause[0].user_paused_at.is_some(),
        "the pause is durable: {after_pause:?}"
    );
    println!("goal after pause: {after_pause:?}");

    // ── 2. a supplement, with no `继续` anywhere in it ──────────────────────
    let supplement = "顺便把 stage-extra.txt 也补上，内容为一行 `extra`，\
                      然后运行 sh -c 'cat stage-extra.txt' 确认。阶段 2 仍然等我确认。";
    let added = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: supplement.to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(
        added.failure.is_none(),
        "the supplement turn failed: {added:?}"
    );
    let after_supplement = goals(&app, &session).await;
    println!("goal after the supplement: {after_supplement:?}");
    assert_eq!(after_supplement.len(), 1, "one goal, not a second identity");
    assert_eq!(after_supplement[0].id, goal_id);
    assert!(
        after_supplement[0]
            .amendments
            .iter()
            .any(|amendment| amendment.contains("stage-extra.txt")),
        "the supplement is durable scope without a resume keyword: {after_supplement:?}"
    );
    assert!(
        after_supplement[0].user_paused_at.is_some(),
        "adding a requirement is not an authorization: {after_supplement:?}"
    );
    assert!(
        !repo.path().join("stage-2.txt").exists(),
        "the withheld stage stays withheld"
    );

    // ── 3. an unrelated request does not join the goal ──────────────────────
    let unrelated = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "另外帮我看一下 README 有没有过时的说明，只回答，不要改任何文件。".to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(unrelated.failure.is_none(), "{unrelated:?}");
    let after_unrelated = goals(&app, &session).await;
    println!("goal after the unrelated request: {after_unrelated:?}");
    assert!(
        !after_unrelated[0]
            .amendments
            .iter()
            .any(|amendment| amendment.contains("README")),
        "an open-ended unrelated request is not this goal's scope: {after_unrelated:?}"
    );
    assert!(
        after_unrelated[0].user_paused_at.is_some(),
        "an unrelated request does not release the pause: {after_unrelated:?}"
    );

    // ── 4. the user's own authorization continues the same goal ─────────────
    let resumed = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "确认，继续阶段 2，并把补充的 stage-extra.txt 一起完成和验收。".to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(resumed.failure.is_none(), "the resume failed: {resumed:?}");
    let finished = goals(&app, &session).await;
    println!("goal after the authorized continuation: {finished:?}");
    println!(
        "outcome: {:?}\nanswer: {}",
        resumed.outcome,
        resumed.text.trim()
    );
    assert_eq!(finished[0].id, goal_id, "the same goal continued");
    assert!(
        finished[0]
            .amendments
            .iter()
            .any(|amendment| amendment.contains("stage-extra.txt")),
        "the supplement survives the continuation: {finished:?}"
    );
    assert!(
        repo.path().join("stage-2.txt").exists(),
        "the authorized stage really ran: {:?}",
        resumed.tools
    );
    let completed = finished[0].state == GoalState::Settled;
    if completed {
        assert!(
            repo.path().join("stage-extra.txt").exists(),
            "a completed goal delivered the supplement too: {:?}",
            finished
        );
    } else {
        // The supplement may already be delivered while the rest of the goal
        // is still owed; what must never happen is the reverse — a completed
        // goal whose dynamic requirement was never delivered.
        println!(
            "note: the goal did not settle, so the remaining work is still owed (no false completion)"
        );
    }
}

/// D-MULTIGOAL-LIVE: while goal B executes, the runtime's own projection names
/// B, not the paused goal A that is still owed. The evidence is sampled DURING
/// the run, not reconstructed from the final snapshot.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn goal_multi_goal_live_projection() {
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = app
        .create_session_with_collaboration(
            &model,
            "multi goal live projection",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    let mut events = client.subscribe_session(&session);
    println!("D-MULTIGOAL-LIVE session={}", session.as_str());

    // ── goal A: paused, never settled, and still owed ───────────────────────
    let paused = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: staged_request().to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(paused.failure.is_none(), "{paused:?}");
    let goal_a = goals(&app, &session).await;
    assert_eq!(goal_a.len(), 1, "{goal_a:?}");
    let goal_a_id = goal_a[0].id.clone();
    assert!(goal_a[0].user_paused_at.is_some(), "{goal_a:?}");
    let projection = client.snapshot(&session).await.unwrap().execution_task;
    let projection = projection.expect("A is owed");
    assert_eq!(projection.goal_id, goal_a_id.as_str());
    assert!(
        projection.user_paused && !projection.running,
        "{projection:?}"
    );

    // ── goal B starts while A is still owed and paused ──────────────────────
    let live = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sampling = Arc::clone(&live);
    let poll_client = client.clone();
    let poll_session = session.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let sampler = tokio::spawn(async move {
        while !stop_flag.load(std::sync::atomic::Ordering::SeqCst) {
            if let Ok(snapshot) = poll_client.snapshot(&poll_session).await
                && let Some(task) = snapshot.execution_task
            {
                sampling.lock().unwrap().push(task);
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
    });

    let second = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "/goal 请先用 commit_task 登记，然后一次完成，中间不要停下来等我确认：\
                      创建 export-b1.txt（内容一行 `b1`）并运行 sh -c 'cat export-b1.txt' 确认。"
                .to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = sampler.await;
    let sampled = live.lock().unwrap().clone();
    println!("sampled projections while goal B ran: {}", sampled.len());
    for task in &sampled {
        println!("  {task:?}");
    }
    assert!(second.failure.is_none(), "goal B failed: {second:?}");

    let goals_now = goals(&app, &session).await;
    let goal_b = goals_now
        .iter()
        .find(|goal| goal.id != goal_a_id)
        .expect("goal B exists")
        .id
        .clone();
    let b_live: Vec<_> = sampled
        .iter()
        .filter(|task| task.goal_id == goal_b.as_str())
        .collect();
    assert!(
        b_live.iter().any(|task| task.running && !task.user_paused),
        "goal B must be projected as the executing goal while it runs: {sampled:?}"
    );
    assert!(
        !b_live.iter().any(|task| task.user_paused),
        "goal B never inherits goal A's pause: {b_live:?}"
    );
    // A's own pause is a durable fact about A, not about this window.
    let a_now = goals_now
        .iter()
        .find(|goal| goal.id == goal_a_id)
        .expect("A still exists");
    println!("goal A while B ran: {a_now:?}");
    assert_eq!(a_now.state, GoalState::Running);
    assert!(
        a_now.user_paused_at.is_some(),
        "A's pause is intact: {a_now:?}"
    );
    // A is still resumable afterwards: the same goal, by identity.
    let after_b = client.snapshot(&session).await.unwrap().execution_task;
    if let Some(task) = after_b {
        println!("owed after B: {task:?}");
        assert!(
            task.goal_id == goal_a_id.as_str() || task.goal_id == goal_b.as_str(),
            "the projection names an owned goal: {task:?}"
        );
    }
    assert!(
        repo.path().join("export-b1.txt").exists()
            || goal_b_is_unsettled(&app, &session, &goal_b).await,
        "B either delivered or is still owed — never silently dropped: {:?}",
        second.tools
    );
}

async fn goal_b_is_unsettled(
    app: &Application,
    session: &leveler_core::SessionId,
    goal: &leveler_core::GoalId,
) -> bool {
    goals(app, session)
        .await
        .iter()
        .any(|record| &record.id == goal && record.state == GoalState::Running)
}

/// D-ACCEPTANCE: a requirement the user added mid-flight, verified by a real
/// command whose result is the evidence — the goal closes through the gate for
/// that requirement instead of on the model's account.
///
/// The supplement names a file and the command that proves it, and no
/// inline-code literal, so every part of it has a mechanical check: the
/// artifact must exist and the command must have run successfully after the
/// last change.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn goal_dynamic_requirement_is_mechanically_verified() {
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let supplement = "再补一项要求：在仓库根目录创建 stage-extra.txt，\
                      然后运行 sh -c 'cat stage-extra.txt' 确认。阶段 2 仍然等我确认。";
    let session = app
        .create_session_with_collaboration(
            &model,
            "dynamic acceptance",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    let mut events = client.subscribe_session(&session);
    println!("D-ACCEPTANCE session={}", session.as_str());

    // A goal that still owes work: stage 1 delivered, the user's own pause.
    let paused = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: staged_request().to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(paused.failure.is_none(), "{paused:?}");
    let started = goals(&app, &session).await;
    assert_eq!(started.len(), 1, "{started:?}");
    let goal_id = started[0].id.clone();
    assert!(started[0].user_paused_at.is_some(), "{started:?}");
    println!("goal before the supplement: {started:?}");

    let added = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: supplement.to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(added.failure.is_none(), "{added:?}");
    let with_supplement = goals(&app, &session).await;
    assert!(
        with_supplement[0]
            .amendments
            .iter()
            .any(|amendment| amendment.contains("stage-extra.txt")),
        "the supplement is durable scope: {with_supplement:?}"
    );
    assert!(
        with_supplement[0].user_paused_at.is_some(),
        "the supplement does not authorize the withheld stage: {with_supplement:?}"
    );

    let resumed = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "确认，继续阶段 2，并把 stage-extra.txt 这一项一起做完和验收。".to_string(),
            attachments: Vec::new(),
        },
    )
    .await;
    assert!(resumed.failure.is_none(), "{resumed:?}");
    let finished = goals(&app, &session).await;
    println!("goal after the continuation: {finished:?}");
    println!(
        "outcome: {:?}\nanswer: {}",
        resumed.outcome,
        resumed.text.trim()
    );
    if finished[0].state == GoalState::Settled {
        assert_eq!(finished[0].id, goal_id);
        assert!(
            repo.path().join("stage-extra.txt").exists(),
            "a completed goal delivered the dynamic requirement: {:?}",
            resumed.tools
        );
        let outcome = resumed.outcome.clone().unwrap_or_default();
        assert!(
            !outcome.contains("stage-extra.txt"),
            "the dynamic requirement's own mechanical check decided it: {outcome}"
        );
        println!("outcome for the dynamic requirement: {outcome}");
    } else {
        println!(
            "note: goal state {:?} — no false completion was reported",
            finished[0].state
        );
        assert!(
            !repo.path().join("stage-extra.txt").exists(),
            "an undelivered dynamic requirement cannot be reported delivered"
        );
    }
}
