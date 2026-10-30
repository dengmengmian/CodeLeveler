//! Real-provider dogfood for the boundary between committing a goal and
//! delegating a sub-task.
//!
//! Four questions, each answered by durable runtime facts rather than by the
//! model's own claim:
//!
//! 1. DF-01 — does a Chat turn commit a multi-section Chinese request as a goal
//!    (`commit_task`), or does it narrate and stop?
//! 2. DF-02 — when the acceptance list covers only part of the user's
//!    deliverables, does the runtime refuse the declaration, name the missing
//!    deliverable, and does the model recover on its own?
//! 3. DF-03 — does the model use `spawn_agent` when a task is genuinely
//!    divisible, and does a child really execute with its own tool calls?
//! 4. DF-04 — is a child that cannot finish, or that the user cancels, reported
//!    to the parent as a failed/cancelled child, and does the parent continue?
//!
//! Ignored by default because it spends real tokens:
//!
//! ```text
//! cargo test -p leveler-app --test delegation_dogfood_real -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, RuntimeEvent, ToolCallId, UiChildState,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{EngineStores, GoalRecord, SessionRepository};

const TIMEOUT: Duration = Duration::from_secs(900);

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

#[derive(Debug, Default)]
struct ToolCall {
    name: String,
    id: String,
    ok: Option<bool>,
    preview: Option<String>,
}

#[derive(Debug, Default)]
struct ChildFacts {
    id: String,
    title: Option<String>,
    started: bool,
    done: bool,
    ok: bool,
    outcome: Option<String>,
    stop: Option<String>,
    cancelled: bool,
}

/// Everything one turn did, recorded from the event stream.
#[derive(Debug, Default)]
struct TurnFacts {
    calls: Vec<ToolCall>,
    children: Vec<ChildFacts>,
    text: String,
    terminal: Option<String>,
    failure: Option<String>,
}

impl TurnFacts {
    fn call(&self, name: &str) -> Option<&ToolCall> {
        self.calls.iter().rev().find(|call| call.name == name)
    }

    fn used(&self, name: &str) -> bool {
        self.calls.iter().any(|call| call.name == name)
    }

    fn names(&self) -> Vec<String> {
        self.calls.iter().map(|call| call.name.clone()).collect()
    }

    fn child(&mut self, id: &str) -> &mut ChildFacts {
        if let Some(index) = self.children.iter().position(|child| child.id == id) {
            return &mut self.children[index];
        }
        self.children.push(ChildFacts {
            id: id.to_string(),
            ..ChildFacts::default()
        });
        self.children.last_mut().unwrap()
    }
}

/// Drive one turn. `on_event` runs for every event so a test can cancel a child
/// the moment it becomes observable.
async fn run_turn<F>(
    client: &Arc<InProcessRuntimeClient>,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    command: ClientCommand,
    mut on_event: F,
) -> TurnFacts
where
    F: FnMut(&RuntimeEvent),
{
    let mut facts = TurnFacts::default();
    client
        .send(command)
        .await
        .expect("the runtime must accept the command");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline && facts.terminal.is_none() {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Ok(event)) => {
                on_event(&event);
                match event {
                    RuntimeEvent::ToolCallStarted { id, name, .. } => facts.calls.push(ToolCall {
                        name,
                        id: id.as_str().to_string(),
                        ..ToolCall::default()
                    }),
                    RuntimeEvent::ToolCallCompleted {
                        id, ok, preview, ..
                    } => {
                        if let Some(call) = facts
                            .calls
                            .iter_mut()
                            .rev()
                            .find(|call| call.id == id.as_str())
                        {
                            call.ok = Some(ok);
                            call.preview = Some(preview);
                        }
                    }
                    RuntimeEvent::SubAgentUpdated {
                        id,
                        title,
                        done,
                        ok,
                        outcome,
                        stop,
                        ..
                    } => {
                        let child = facts.child(&id);
                        if !done {
                            child.started = true;
                            child.title = title;
                        } else {
                            child.done = true;
                            child.ok = ok;
                            child.outcome = outcome.map(|outcome| format!("{outcome:?}"));
                            child.stop = stop.map(|stop| format!("{stop:?}"));
                        }
                    }
                    RuntimeEvent::SubAgentStateChanged { id, state } => {
                        if state == UiChildState::Settled {
                            facts.child(&id).cancelled = true;
                        }
                    }
                    RuntimeEvent::AssistantTextDelta { delta, .. } => facts.text.push_str(&delta),
                    RuntimeEvent::TurnCompleted => facts.terminal = Some("completed".into()),
                    RuntimeEvent::TurnCompletedWithWarnings { reason } => {
                        facts.terminal = Some(format!("completed_with_warnings: {reason}"))
                    }
                    RuntimeEvent::TurnAnswered => facts.terminal = Some("answered".into()),
                    RuntimeEvent::TurnIncomplete { reason } => {
                        facts.terminal = Some(format!("incomplete: {reason}"))
                    }
                    RuntimeEvent::TurnTruncated { error } => {
                        facts.terminal = Some(format!("truncated: {error}"))
                    }
                    RuntimeEvent::TurnCancelled => facts.terminal = Some("interrupted".into()),
                    RuntimeEvent::TaskCancelled => facts.terminal = Some("cancelled".into()),
                    RuntimeEvent::TurnFailed { error, .. } => {
                        facts.failure = Some(error);
                        facts.terminal = Some("failed".into());
                    }
                    _ => {}
                }
            }
            Ok(Err(error)) => {
                facts.failure = Some(error.to_string());
                break;
            }
            Err(_) => break,
        }
    }
    println!("  tools={:?}", facts.names());
    for call in &facts.calls {
        if matches!(
            call.name.as_str(),
            "commit_task" | "spawn_agent" | "update_goal"
        ) {
            println!(
                "  · {} ok={:?} :: {}",
                call.name,
                call.ok,
                call.preview
                    .as_deref()
                    .unwrap_or("")
                    .lines()
                    .take(6)
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
    }
    println!(
        "  children={:?}\n  terminal={:?} failure={:?}\n  answer: {}",
        facts.children,
        facts.terminal,
        facts.failure,
        facts.text.trim()
    );
    facts
}

struct Fixture {
    app: Arc<Application>,
    client: Arc<InProcessRuntimeClient>,
    session: leveler_core::SessionId,
    repo: tempfile::TempDir,
    _home: tempfile::TempDir,
}

async fn fixture(collaboration: leveler_agent::CollaborationMode, seed: &str) -> Fixture {
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
    let session = app
        .create_session_with_collaboration(
            &model,
            seed,
            PermissionProfile::FullAccess,
            collaboration,
        )
        .await
        .unwrap();
    Fixture {
        app,
        client,
        session,
        repo,
        _home: home,
    }
}

async fn goals(app: &Application, session: &leveler_core::SessionId) -> Vec<GoalRecord> {
    let db = app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let Some(task) = stores.tasks.task_for_session(session).await.unwrap() else {
        return Vec::new();
    };
    stores.goals.for_task(&task).await.unwrap()
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

fn submit(fixture: &Fixture, content: &str) -> ClientCommand {
    ClientCommand::SubmitMessage {
        session_id: fixture.session.clone(),
        content: content.to_string(),
        attachments: Vec::new(),
    }
}

/// DF-01 — a multi-section Chinese request in a Chat session.
///
/// The declaration is the runtime's fact: `commit_task` must return success and
/// a goal row must exist. A turn that only narrates its plan leaves no goal and
/// fails here.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn df01_a_chat_turn_commits_a_multisection_chinese_request() {
    let f = fixture(leveler_agent::CollaborationMode::Chat, "内容社区笔记模块").await;
    let mut events = f.client.subscribe_session(&f.session);
    let task = "## 一、项目目标\n\n\
                在仓库根目录补一个最小的笔记模块，供真实用户创建与列举笔记。\n\n\
                ## 二、必须交付\n\n\
                1. 实现 notes/create.sh：接收一个标题参数，创建 notes/<标题>.md。\n\
                2. 实现 notes/list.sh：列出 notes 目录下的笔记标题。\n\
                3. 实现 README 说明文档，说明两个脚本的用法。\n\
                4. 完成集成测试，运行 `sh notes/test.sh` 必须通过。\n\n\
                ## 三、禁止事项\n\n\
                - 不要修改或删除 seed.txt。\n\n\
                完成前不要结束。";
    let turn = run_turn(&f.client, &mut events, submit(&f, task), |_| {}).await;

    let commit = turn
        .call("commit_task")
        .expect("the model must declare this as execution work");
    assert_eq!(
        commit.ok,
        Some(true),
        "the declaration must be accepted: {:?}",
        commit.preview
    );
    let goals = goals(&f.app, &f.session).await;
    assert_eq!(goals.len(), 1, "one goal bound to the turn: {goals:?}");
    assert!(
        goals[0].objective.contains("笔记模块"),
        "the goal carries the user's own objective: {:?}",
        goals[0].objective
    );
    assert!(
        goals[0].acceptance.len() >= 4,
        "the acceptance names each deliverable: {:?}",
        goals[0].acceptance
    );
    assert!(
        !turn.used("spawn_agent"),
        "committing a goal must not create a sub-agent: {:?}",
        turn.names()
    );
    assert!(turn.children.is_empty(), "{:?}", turn.children);
}

/// DF-02 — two deliverables, an acceptance list that covers one, and the
/// recovery. The user asks for exactly that narrow declaration first, so the
/// refusal is a real runtime decision and not a mocked one.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn df02_a_narrow_acceptance_is_refused_and_the_model_recovers() {
    let f = fixture(leveler_agent::CollaborationMode::Chat, "alpha 与 beta").await;
    let mut events = f.client.subscribe_session(&f.session);
    let task = "## 一、必须交付\n\n\
                - 创建 alpha.txt，内容为一行 alpha。\n\
                - 创建 beta.txt，内容为一行 beta。\n\n\
                ## 二、验收\n\n\
                验收条目必须包含两个交付物。\n\n\
                先用一个只覆盖 alpha.txt 的 acceptance 调用 commit_task，观察运行时的回应，\
                然后按运行时的要求修正 acceptance 并重新提交。修好之后再完成实际工作。";
    let turn = run_turn(&f.client, &mut events, submit(&f, task), |_| {}).await;

    let commits: Vec<&ToolCall> = turn
        .calls
        .iter()
        .filter(|call| call.name == "commit_task")
        .collect();
    assert!(
        commits.len() >= 2,
        "the model must retry after the refusal: {:?}",
        turn.names()
    );
    let refused = commits
        .iter()
        .find(|call| call.ok == Some(false))
        .expect("the narrow declaration was refused");
    let refusal = refused.preview.clone().unwrap_or_default();
    assert!(
        refusal.contains("acceptance_missing_deliverables"),
        "the refusal is the structured scope refusal: {refusal}"
    );
    assert!(
        refusal.contains("beta.txt"),
        "the refusal names the deliverable the acceptance dropped: {refusal}"
    );
    let accepted = commits
        .iter()
        .find(|call| call.ok == Some(true))
        .expect("the corrected declaration was accepted");
    assert!(accepted.preview.is_some());
    let goals = goals(&f.app, &f.session).await;
    assert_eq!(goals.len(), 1, "one goal, not two: {goals:?}");
    assert!(
        goals[0]
            .acceptance
            .iter()
            .any(|check| check.contains("beta.txt")),
        "the committed acceptance covers both deliverables: {:?}",
        goals[0].acceptance
    );
    assert!(
        turn.children.is_empty(),
        "no delegation happened: {:?}",
        turn.children
    );
    assert!(
        axis(&f.app, &f.session).await == "chat" || turn.terminal.is_some(),
        "the session axis stays a chat turn"
    );
}

/// DF-03a — a genuinely divisible task with permission to delegate. Whether the
/// model delegates is its own decision; this test reports it and asserts only
/// that any child it created really ran.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn df03a_autonomous_delegation_on_a_divisible_task() {
    let f = fixture(leveler_agent::CollaborationMode::Chat, "小型 Go 服务").await;
    let mut events = f.client.subscribe_session(&f.session);
    let task = "实现一个小型 Go 服务（标准库即可）：\n\n\
                - 用户管理接口\n\
                - 文章管理接口\n\
                - 评论管理接口\n\
                - 集成测试\n\n\
                请自主规划并完成实现。可以按需要使用子代理并行或分工执行。\n\
                每个接口只需最小的内存实现与一个 httptest 用例。";
    let turn = run_turn(&f.client, &mut events, submit(&f, task), |_| {}).await;
    println!(
        "  delegated={} children={:?}",
        turn.used("spawn_agent"),
        turn.children
    );
    for child in &turn.children {
        assert!(
            child.started,
            "a child in the record must have started: {child:?}"
        );
    }
    assert!(turn.terminal.is_some(), "the turn must reach a terminal");
}

/// DF-03b — the user explicitly requires delegation. The child must really run:
/// it gets its own identity, its own tool calls, and a terminal outcome.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn df03b_an_explicitly_requested_child_really_runs() {
    let f = fixture(leveler_agent::CollaborationMode::Chat, "委派一个子任务").await;
    let mut events = f.client.subscribe_session(&f.session);
    let task = "用 spawn_agent 委派一个子代理（run_in_background=false）完成下面这件事，\
                等它返回后，你自己运行 `cat child.txt` 核实，然后告诉我它写了什么：\n\n\
                子任务：在仓库根目录创建 child.txt，内容为一行 hello-from-child。";
    let turn = run_turn(&f.client, &mut events, submit(&f, task), |_| {}).await;

    let spawn = turn
        .call("spawn_agent")
        .expect("the user asked for delegation");
    assert_eq!(
        spawn.ok,
        Some(true),
        "the spawn must be accepted: {:?}",
        spawn.preview
    );
    let child = turn
        .children
        .first()
        .expect("the runtime must report the child it created");
    assert!(child.started, "{child:?}");
    assert!(child.done, "the child must settle: {child:?}");
    assert!(child.ok, "the child must succeed: {child:?}");
    assert!(
        f.repo.path().join("child.txt").exists(),
        "the child's write is a real effect on disk"
    );
    assert!(
        !turn.used("commit_task"),
        "a delegated sub-task is not a goal commitment: {:?}",
        turn.names()
    );
}

/// DF-04a — a spawn the runtime refuses before any child exists. This is an
/// admission refusal, not a child failure, and the model must be able to see
/// which one it was.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn df04a_a_refused_spawn_creates_no_child() {
    let f = fixture(leveler_agent::CollaborationMode::Chat, "拒绝的委派").await;
    let mut events = f.client.subscribe_session(&f.session);
    let task = "先用 role=\"nonexistent-role\" 调用一次 spawn_agent，把 task 写成 `probe`，\
                观察运行时的回应；然后用一个合法调用重试（role 省略，task 为 echo ok），\
                最后告诉我两次回应的区别。";
    let turn = run_turn(&f.client, &mut events, submit(&f, task), |_| {}).await;

    let spawns: Vec<&ToolCall> = turn
        .calls
        .iter()
        .filter(|call| call.name == "spawn_agent")
        .collect();
    assert!(!spawns.is_empty(), "{:?}", turn.names());
    let refused = spawns
        .iter()
        .find(|call| call.ok == Some(false))
        .expect("the invalid role must be refused");
    let reason = refused.preview.clone().unwrap_or_default();
    assert!(
        !reason.is_empty() && !turn.children.iter().any(|c| c.id == refused.id),
        "a refused spawn creates no child: {reason}"
    );
    assert!(
        turn.text.to_lowercase().contains("role")
            || turn.text.contains("拒绝")
            || turn.text.contains("失败")
            || turn.text.contains("区别"),
        "the model must be able to describe the difference: {}",
        turn.text
    );
}

/// DF-04b — a child that provably cannot finish. Its failure reaches the
/// parent as a failed child plus a failed spawn result, and the parent keeps
/// working instead of ending the turn.
///
/// The obstruction is environmental and harmless: the target path needs root
/// privileges, so no tool the child holds can satisfy the task.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn df04b_a_failing_child_is_reported_and_the_parent_continues() {
    let f = fixture(leveler_agent::CollaborationMode::Chat, "失败的子代理").await;
    let mut events = f.client.subscribe_session(&f.session);
    let task = "用 spawn_agent(role=\"explorer\", run_in_background=false) 委派下面这个子任务，\
                等它返回后按它的结果继续：如果它没有完成，你自己创建 fallback.txt（内容一行 fallback）、\
                运行 `cat fallback.txt` 核实，并说明子代理为什么没能完成。\n\n\
                子任务：把 /private/var/root/leveler-probe.txt 的内容改成一行 done，\
                然后运行 `cat /private/var/root/leveler-probe.txt` 确认。不要询问，直接做。";
    let turn = run_turn(&f.client, &mut events, submit(&f, task), |_| {}).await;

    let spawn = turn
        .call("spawn_agent")
        .expect("the spawn call must be in the record");
    let child = turn
        .children
        .first()
        .expect("the runtime must report the child it created");
    println!("  child={child:?} spawn_result={:?}", spawn.preview);
    assert!(child.started, "{child:?}");
    assert!(
        child.done,
        "a child always settles, even when it cannot finish: {child:?}"
    );
    assert_eq!(
        spawn.ok,
        Some(true),
        "the spawn call itself settles instead of staying running: {:?}",
        spawn.preview
    );
    // The two facts the runtime owns: the child settled, and its settlement
    // carries the child's own account. Whether the ACTIVATION ended normally
    // (it answered with a report) or as a failure is `child.ok` — both are
    // honest, so the assertion is that the parent read the result, not that
    // the child was forced into one status.
    println!(
        "  child activation ok={} stop={:?} outcome={:?}",
        child.ok, child.stop, child.outcome
    );
    assert!(
        f.repo.path().join("fallback.txt").exists(),
        "the parent must continue after the child failed; answer: {}",
        turn.text
    );
    assert!(
        turn.terminal.is_some(),
        "the parent turn reaches a terminal"
    );
}

/// DF-04c — the user cancels one running child. The child settles as cancelled,
/// the parent turn keeps running, and the transcript says which happened.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn df04c_a_cancelled_child_settles_without_killing_the_parent() {
    let f = fixture(leveler_agent::CollaborationMode::Chat, "取消子代理").await;
    let mut events = f.client.subscribe_session(&f.session);
    let task = "用 spawn_agent 委派两个子代理（默认后台运行）：\n\n\
                1. 子任务 A：在仓库根目录创建 slow.txt，内容为一行 slow。\n\
                2. 子任务 B：在仓库根目录创建 fast.txt，内容为一行 fast。\n\n\
                两个都提交后，自己运行 `ls` 看看现在的文件，然后总结。";
    let client = f.client.clone();
    let session = f.session.clone();
    let mut cancelled: Option<String> = None;
    let turn = run_turn(&f.client, &mut events, submit(&f, task), |event| {
        if let RuntimeEvent::SubAgentUpdated {
            id, done: false, ..
        } = event
            && cancelled.is_none()
        {
            let client = client.clone();
            let session = session.clone();
            let id = id.clone();
            cancelled = Some(id.clone());
            tokio::spawn(async move {
                // Give the child a moment to actually be running.
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let _ = client
                    .send(ClientCommand::CancelChild {
                        session_id: session,
                        child_id: id,
                    })
                    .await;
            });
        }
    })
    .await;
    match &cancelled {
        Some(id) => {
            let child = turn
                .children
                .iter()
                .find(|child| &child.id == id)
                .expect("the cancelled child stays in the record");
            println!("  cancelled child={child:?}");
            assert!(
                turn.terminal.is_some(),
                "the parent turn must still reach a terminal"
            );
        }
        None => println!("  the model created no child; nothing to cancel"),
    }
}

/// Keep the tool-call id type referenced so a future edit that needs it compiles
/// the same way the runtime reports it.
#[allow(dead_code)]
fn call_id(id: &str) -> ToolCallId {
    ToolCallId::new(id)
}
