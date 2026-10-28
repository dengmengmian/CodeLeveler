//! Message routing versus the Goal lifecycle.
//!
//! A Goal is a durable commitment, not a mode that captures every later
//! message. These tests lock the boundary the two share:
//!
//! - a Goal that ended a work window (blocked, failed, completed) is over for
//!   THIS message stream: the next ordinary message is a conversation, and no
//!   new Goal may be opened from its text;
//! - the ended Goal keeps its identity, objective and recovery path, and an
//!   explicit continuation re-enters exactly that Goal;
//! - a Goal that still owes work inside an open window keeps receiving the
//!   session's messages, unchanged in identity.
//!
//! They assert durable facts only: goal rows, the session's route, turn kinds
//! and the tool surface the model was actually given.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient};
use leveler_execution::{AutoApprove, PermissionProfile};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{EngineStores, GoalRecord, GoalState, SessionRepository, TurnRepository};
use leveler_test_support::{MockResponse, MockServer};
use tokio_util::sync::CancellationToken;

const OBJECTIVE: &str = "finish the inventory work";
const QUESTION: &str = "为什么没有权限？这个仓库的权限设置是什么？";

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn sse(frames: Vec<String>) -> MockResponse {
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(&frame);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    MockResponse::Sse { body }
}

fn tool_call_frame(id: &str, name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": id,
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]
    })
    .to_string()
}

fn finish_frame(reason: &str) -> String {
    serde_json::json!({"choices": [{"delta": {}, "finish_reason": reason}]}).to_string()
}

/// One assistant message with text and no tool calls: the shape of an answer.
fn answer_frame(text: &str) -> String {
    serde_json::json!({"choices": [{"delta": {"content": text}}]}).to_string()
}

fn write_config(root: &std::path::Path, base_url: &str) {
    std::fs::create_dir_all(root.join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.join("configs/models")).unwrap();
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("configs/models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities:
  streaming: true
  tool_calling: true
  parallel_tool_calls: false
  structured_output: true
  reasoning: false
  vision: false
limits:
  context_window: 131072
  reliable_context: 65536
  max_output_tokens: 1024
  max_tool_schema_bytes: 8192
  max_parallel_tool_calls: 1
compatibility:
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#,
    )
    .unwrap();
}

struct Fixture {
    app: Arc<Application>,
    session: leveler_core::SessionId,
    server: MockServer,
    _tmp: tempfile::TempDir,
}

async fn fixture(responses: Vec<MockResponse>, axis: leveler_agent::CollaborationMode) -> Fixture {
    isolate_global_config();
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    write_config(tmp.path(), &server.base_url());
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(axis),
    );
    let session = app
        .create_session(&ModelRef::new("mock", "m"), OBJECTIVE)
        .await
        .unwrap();
    Fixture {
        app,
        session,
        server,
        _tmp: tmp,
    }
}

fn blocked_turn() -> MockResponse {
    sse(vec![
        tool_call_frame(
            "c-block",
            "update_goal",
            serde_json::json!({"status": "blocked", "summary": "missing credentials"}),
        ),
        finish_frame("tool_calls"),
    ])
}

fn completed_turn(id: &str, summary: &str) -> MockResponse {
    sse(vec![
        tool_call_frame(
            id,
            "update_goal",
            serde_json::json!({"status": "complete", "summary": summary}),
        ),
        finish_frame("tool_calls"),
    ])
}

fn plain_answer(text: &str) -> MockResponse {
    sse(vec![answer_frame(text), finish_frame("stop")])
}

async fn goals(f: &Fixture) -> Vec<GoalRecord> {
    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let Some(task) = stores.tasks.task_for_session(&f.session).await.unwrap() else {
        return Vec::new();
    };
    stores.goals.for_task(&task).await.unwrap()
}

async fn axis(f: &Fixture) -> String {
    let db = f.app.open_database().await.unwrap();
    SessionRepository::new(&db)
        .get(&f.session)
        .await
        .unwrap()
        .unwrap()
        .collaboration
}

/// Wait until `min_turns` turns exist and none is running.
async fn settled(f: &Fixture, min_turns: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let db = f.app.open_database().await.unwrap();
        let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
        if turns.len() >= min_turns && turns.iter().all(|turn| turn.status != "running") {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not settle in time: {turns:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn run_goal_turn(f: &Fixture, content: &str) {
    f.app
        .run_in_session_bounded(
            &f.session,
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            content,
            Arc::new(AutoApprove),
            false,
            &mut |_| {},
            CancellationToken::new(),
            8,
            None,
        )
        .await
        .expect("a blocked goal is a normal return");
}

/// The tool names a request actually exposed. Read from the request's own
/// `tools` array, never from a substring of the whole body.
fn tool_names(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    value
        .get("tools")
        .and_then(|tools| tools.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.get("function")?.get("name")?.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn client(f: &Fixture) -> Arc<dyn InteractiveRuntimeClient> {
    Arc::new(InProcessRuntimeClient::new(
        f.app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ))
}

/// The reported accident, in the smallest form: a Goal that ended its window
/// **blocked** kept the session's Goal route, so the next ordinary question was
/// turned into a Goal whose objective was that question.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_goal_answers_an_ordinary_question_without_opening_a_goal() {
    let f = fixture(
        vec![
            blocked_turn(),
            // A question is answered, not committed.
            plain_answer("权限来自 config/limits 的 read_only 范围。"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    run_goal_turn(&f, OBJECTIVE).await;

    let after_block = goals(&f).await;
    assert_eq!(after_block.len(), 1, "{after_block:?}");
    assert_eq!(after_block[0].objective, OBJECTIVE);
    assert_eq!(
        after_block[0].state,
        GoalState::Running,
        "a blocked goal still owes its work; only the window ended"
    );
    assert_eq!(
        axis(&f).await,
        "chat",
        "the closed window handed the conversation back to the user"
    );

    // The ordinary question: this is the message the incident routed wrongly.
    client(&f)
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: QUESTION.to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    settled(&f, 2).await;

    let goals = goals(&f).await;
    assert_eq!(
        goals.len(),
        1,
        "an ordinary question must never open a Goal: {goals:?}"
    );
    assert_eq!(
        goals[0].objective, OBJECTIVE,
        "the user's objective must not be rewritten by a question"
    );
    assert!(
        !goals[0].amendments.iter().any(|a| a.contains("权限")),
        "a question is not a scope amendment: {:?}",
        goals[0].amendments
    );

    let db = f.app.open_database().await.unwrap();
    let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
    let question_turn = turns.last().expect("the question ran");
    assert_eq!(
        question_turn.kind, "chat",
        "the question must run as an ordinary Chat turn: {turns:?}"
    );
    let body = f.server.request_bodies().await;
    let question_request = body.last().expect("the question reached the model");
    assert!(
        !tool_names(question_request)
            .iter()
            .any(|name| name == "update_goal"),
        "a Chat turn carries no Goal contract: {:?}",
        tool_names(question_request)
    );
    assert!(
        tool_names(question_request)
            .iter()
            .any(|name| name == "commit_task"),
        "a Chat turn keeps the delegation entry: {:?}",
        tool_names(question_request)
    );
    assert_eq!(
        axis(&f).await,
        "chat",
        "a settled goal does not leave the Goal route behind"
    );
}

/// The other half of the same contract: after the ordinary question, an
/// explicit continuation re-enters the SAME goal — same id, same objective, and
/// the completion gate still applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_goal_resumes_the_same_identity_after_an_ordinary_question() {
    let f = fixture(
        vec![
            blocked_turn(),
            plain_answer("权限来自 config/limits 的 read_only 范围。"),
            completed_turn("c-resume", "the blocked work is finished"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    run_goal_turn(&f, OBJECTIVE).await;
    let blocked = goals(&f).await;
    assert_eq!(blocked.len(), 1);
    let goal_id = blocked[0].id.clone();

    client(&f)
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: QUESTION.to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    settled(&f, 2).await;

    client(&f)
        .send(ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续".to_string(),
        })
        .await
        .unwrap();
    settled(&f, 3).await;

    let goals = goals(&f).await;
    assert_eq!(
        goals.len(),
        1,
        "continuing a blocked goal must not open a sibling: {goals:?}"
    );
    assert_eq!(
        goals[0].id, goal_id,
        "the continuation re-entered the blocked goal"
    );
    assert_eq!(goals[0].objective, OBJECTIVE);
    assert_eq!(
        goals[0].state,
        GoalState::Settled,
        "the resumed window really ran and closed"
    );
    assert!(
        goals[0].windows_run >= 2,
        "the resumed window is counted on the goal: {:?} turns={:?}",
        goals[0].windows_run,
        TurnRepository::new(&f.app.open_database().await.unwrap())
            .list(&f.session)
            .await
            .unwrap()
            .iter()
            .map(|turn| (turn.kind.clone(), turn.status.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        axis(&f).await,
        "chat",
        "the goal delivered its work, so the route ended with it"
    );
}

/// A Goal that still owes work inside an open window (its round budget ran out)
/// keeps receiving the session's messages: identity and objective survive, and
/// no sibling Goal is minted from whatever the user typed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owed_goal_keeps_its_identity_when_the_next_message_arrives() {
    // A plain answer in a Goal turn is not a completion: the executor keeps the
    // goal owed, and the bounded run ends at its round ceiling.
    let f = fixture(
        vec![plain_answer("下一步需要把清单的读取接口补上。")],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    run_goal_turn(&f, OBJECTIVE).await;

    let owed = goals(&f).await;
    assert_eq!(owed.len(), 1, "{owed:?}");
    let goal_id = owed[0].id.clone();
    assert_eq!(
        owed[0].state,
        GoalState::Running,
        "a window that ran out of rounds still owes its work"
    );

    client(&f)
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "顺便看一眼 README 有没有过时的说明".to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    settled(&f, 2).await;

    let goals = goals(&f).await;
    assert_eq!(
        goals.len(),
        1,
        "an ordinary message inside an owed goal must not mint a new one: {goals:?}"
    );
    assert_eq!(goals[0].id, goal_id, "identity is the goal, not the text");
    assert!(
        goals[0].objective.starts_with(OBJECTIVE),
        "the original objective stays the anchor: {:?}",
        goals[0].objective
    );
}

/// A cancelled task is terminal: the next message is an ordinary conversation
/// and no Goal is reopened, reclaimed or reopened by its text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_goal_leaves_the_next_message_ordinary() {
    let f = fixture(
        vec![
            blocked_turn(),
            plain_answer("任务已取消，没有待恢复的目标。"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    run_goal_turn(&f, OBJECTIVE).await;
    let before = goals(&f).await;
    assert_eq!(before.len(), 1);
    let goal_id = before[0].id.clone();

    client(&f)
        .send(ClientCommand::CancelTask {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    // Idempotent on a second delivery: the terminal is already cancelled.
    client(&f)
        .send(ClientCommand::CancelTask {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();

    client(&f)
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "现在 Git 是什么状态？".to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    settled(&f, 2).await;

    let goals = goals(&f).await;
    assert_eq!(goals.len(), 1, "no Goal may be minted after a cancel");
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(
        goals[0].state,
        GoalState::Settled,
        "the cancelled goal stays settled"
    );
    let db = f.app.open_database().await.unwrap();
    let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
    assert_eq!(
        turns.last().map(|turn| turn.kind.as_str()),
        Some("chat"),
        "a cancelled task leaves the next message ordinary: {turns:?}"
    );
}

/// A provider outage ends the window as a recoverable `failed`: the goal still
/// owes its work, so it stays owed and resumable — but the conversation is the
/// user's again. The question they ask is answered in Chat, and `继续`
/// afterwards re-enters the SAME goal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recoverably_failed_goal_answers_a_question_then_resumes_the_same_goal() {
    // One failed logical request = the first attempt plus the retry budget. The
    // provider advertises `Retry-After: 0`, so the retries are immediate and
    // this stays a fast, deterministic test.
    let mut responses = vec![unavailable_with_immediate_retry(); 11];
    responses.push(plain_answer("上一步失败于 provider 5xx；权限无关。"));
    responses.push(completed_turn("c-resume", "the failed work is finished"));
    let f = fixture(responses, leveler_agent::CollaborationMode::Goal).await;
    let failed_run = f
        .app
        .run_in_session_bounded(
            &f.session,
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            OBJECTIVE,
            Arc::new(AutoApprove),
            false,
            &mut |_| {},
            CancellationToken::new(),
            8,
            None,
        )
        .await;
    assert!(
        failed_run.is_err(),
        "an exhausted provider fails the window"
    );

    let failed = goals(&f).await;
    assert_eq!(failed.len(), 1, "{failed:?}");
    let goal_id = failed[0].id.clone();
    assert_eq!(
        failed[0].state,
        GoalState::Running,
        "a recoverable failure leaves the work owed"
    );
    assert_eq!(failed[0].objective, OBJECTIVE);
    assert_eq!(
        axis(&f).await,
        "chat",
        "the failed window ended the Goal route"
    );

    client(&f)
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: QUESTION.to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    settled(&f, 2).await;

    let after_question = goals(&f).await;
    assert_eq!(
        after_question.len(),
        1,
        "the question must not open a goal: {after_question:?}"
    );
    assert_eq!(after_question[0].objective, OBJECTIVE);
    assert!(after_question[0].acceptance.is_empty());
    let db = f.app.open_database().await.unwrap();
    let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
    assert_eq!(
        turns.last().map(|turn| turn.kind.as_str()),
        Some("chat"),
        "the question ran as an ordinary Chat turn: {turns:?}"
    );
    let body = f.server.request_bodies().await;
    let question_request = body.last().expect("the question reached the model");
    assert!(
        !tool_names(question_request)
            .iter()
            .any(|name| name == "update_goal"),
        "a Chat turn carries no Goal contract: {:?}",
        tool_names(question_request)
    );

    client(&f)
        .send(ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续".to_string(),
        })
        .await
        .unwrap();
    settled(&f, 3).await;

    let goals = goals(&f).await;
    assert_eq!(goals.len(), 1, "the retry is the same goal: {goals:?}");
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(goals[0].objective, OBJECTIVE);
    assert!(goals[0].windows_run >= 2, "{goals:?}");
}

/// A 5xx with `Retry-After: 0`: a provider outage that is retried immediately.
fn unavailable_with_immediate_retry() -> MockResponse {
    MockResponse::StatusWithHeaders {
        code: 500,
        body: r#"{"error":{"message":"upstream unavailable"}}"#.to_string(),
        headers: vec![("Retry-After".to_string(), "0".to_string())],
    }
}
