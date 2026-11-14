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
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
#[cfg(unix)]
use leveler_execution::BackgroundTaskStatus;
use leveler_execution::{AutoApprove, PermissionProfile};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{
    EngineStores, GoalRecord, GoalState, MessageRepository, SessionRepository, TurnRepository,
};
#[cfg(unix)]
use leveler_test_support::sleep_command;
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

/// One model round that states its structured reading of the newest user
/// message. The runtime validates the quote and the operations; it never reads
/// the message itself.
fn declare_intent_frame(id: &str, quote: &str, operations: serde_json::Value) -> MockResponse {
    sse(vec![
        tool_call_frame(
            id,
            "declare_intent",
            serde_json::json!({"source_quote": quote, "operations": operations}),
        ),
        finish_frame("tool_calls"),
    ])
}

/// The pause boundary from `PAUSED_OBJECTIVE`, expressed against the plan's
/// stable step id instead of the step's wording.
fn declare_pause_after_stage_one() -> MockResponse {
    declare_intent_frame(
        "i-pause",
        "阶段 1 完成后停下，等待我确认再执行阶段 2",
        serde_json::json!([{
            "kind": "set_pause_boundary",
            "boundary": {
                "kind": "after_step",
                "step": {"step_id": "stage-1", "label": "阶段 1：创建 stage-1.txt 并确认"}
            }
        }]),
    )
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

/// The structured third terminal: the model closed this work window while the
/// goal still owes work. The next window is the runtime's, not the user's.
fn yielded_turn(note: &str) -> MockResponse {
    sse(vec![
        tool_call_frame(
            "c-yield",
            "update_goal",
            serde_json::json!({"status": "yield", "summary": note}),
        ),
        finish_frame("tool_calls"),
    ])
}

/// Durable user submissions in this session's transcript. The runtime's
/// continuation runs with no message, so this must not grow between windows.
async fn user_input_count(f: &Fixture) -> usize {
    let db = f.app.open_database().await.unwrap();
    MessageRepository::new(&db)
        .load(&f.session)
        .await
        .unwrap()
        .iter()
        .filter(|payload| {
            serde_json::from_str::<serde_json::Value>(payload)
                .ok()
                .and_then(|value| value.pointer("/origin/kind").cloned())
                .as_ref()
                .and_then(|kind| kind.as_str())
                == Some("user_input")
        })
        .count()
}

/// Tool results the model was actually handed in this session, in order.
async fn tool_result_texts(f: &Fixture) -> Vec<String> {
    let db = f.app.open_database().await.unwrap();
    let payloads = MessageRepository::new(&db).load(&f.session).await.unwrap();
    let mut texts = Vec::new();
    for payload in payloads {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload) else {
            continue;
        };
        if let Some(parts) = value.get("content").and_then(|content| content.as_array()) {
            for part in parts {
                if let Some(text) = part.pointer("/result/content").and_then(|v| v.as_str()) {
                    texts.push(text.to_string());
                }
            }
        }
    }
    texts
}

fn user_decision_blocked_turn() -> MockResponse {
    sse(vec![
        tool_call_frame(
            "c-user-decision",
            "update_goal",
            serde_json::json!({
                "status": "blocked",
                "blocked_kind": "user_decision",
                "summary": "the accounting policy is undecided"
            }),
        ),
        finish_frame("tool_calls"),
    ])
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

/// Wait for the turn's terminal event.
///
/// A stopped turn ROW is committed inside the engine, before the runtime
/// releases the turn's admission. The terminal event is published inside
/// `ActiveTurns::finish_with`, the same lock that removes the admission, so a
/// command sent after this event is ordered after the session accepts a new
/// turn. Sequencing a `/goal` on the row alone can land in the gap and be
/// treated as a steer into the turn that is already exiting.
async fn wait_for_turn_terminal(rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
    use tokio::sync::broadcast::error::RecvError;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::TurnCompleted))
            | Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { .. }))
            | Ok(Ok(RuntimeEvent::TurnAnswered))
            | Ok(Ok(RuntimeEvent::TurnPaused { .. }))
            | Ok(Ok(RuntimeEvent::TurnTruncated { .. }))
            | Ok(Ok(RuntimeEvent::TurnIncomplete { .. }))
            | Ok(Ok(RuntimeEvent::TurnBlocked { .. }))
            | Ok(Ok(RuntimeEvent::TurnFailed { .. }))
            | Ok(Ok(RuntimeEvent::TurnCancelled))
            | Ok(Ok(RuntimeEvent::TaskCancelled)) => return,
            // A yielded WINDOW is not a turn end: the logical turn keeps its
            // admission for the next window.
            Ok(Ok(_)) | Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => {
                panic!("the event stream closed before the turn ended")
            }
            Err(_) => panic!("the turn never ended"),
        }
    }
}

/// Wait until the session's goal records satisfy `done`.
///
/// A goal is settled in the task's terminal transaction, which lands after the
/// turn row stops running; reading the record before that commit observes a
/// state the window has already left.
async fn wait_for_goals(f: &Fixture, done: impl Fn(&[GoalRecord]) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let goals = goals(f).await;
        if done(&goals) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the goal did not reach its terminal state: {goals:?}"
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
    wait_for_goals(&f, |goals| {
        goals
            .iter()
            .any(|goal| goal.id == goal_id && goal.state == GoalState::Settled)
    })
    .await;

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

/// A window the model ended through `update_goal(yield)` is continued by the
/// runtime itself: same goal, same objective, a fresh work window, and NO user
/// message. Two model requests and two turn rows prove the second window ran;
/// the transcript holding exactly the user's own message proves nobody typed
/// the continuation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_yielded_window_is_continued_by_the_runtime_without_a_user_message() {
    let f = fixture(
        vec![
            yielded_turn("阶段 1 已落地；阶段 2 未开始，下一窗口继续。"),
            completed_turn("c-continue", "两个阶段都已完成"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;

    client(&f)
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: OBJECTIVE.to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();

    settled(&f, 2).await;
    wait_for_goals(&f, |goals| {
        goals.iter().any(|goal| goal.state == GoalState::Settled)
    })
    .await;

    let goals = goals(&f).await;
    assert_eq!(
        goals.len(),
        1,
        "the continuation is the same goal, not a sibling: {goals:?}"
    );
    assert_eq!(goals[0].objective, OBJECTIVE);
    assert_eq!(
        goals[0].windows_run, 2,
        "each work window commits its own count: {goals:?}"
    );

    let db = f.app.open_database().await.unwrap();
    let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
    assert_eq!(
        turns.len(),
        2,
        "one durable turn row per work window: {turns:?}"
    );
    let bodies = f.server.request_bodies().await;
    assert!(
        bodies.len() >= 2,
        "the second window was really driven by the model: {} request(s)",
        bodies.len()
    );
    assert_eq!(
        user_input_count(&f).await,
        1,
        "the runtime, not the user, started the second window"
    );
}

#[cfg(unix)]
fn os_process_exists(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("query isolated test process liveness")
        .success()
}

/// A goal-level service the model started does not belong to one work window:
/// the yielded window must not reap it, the next window sees it running, and
/// the goal's own terminal is what cleans it up. The second window holds on a
/// foreground command, so the cross-window observation is a fact, not a race.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_service_survives_a_yielded_window_and_is_reaped_when_the_goal_ends() {
    let (program, args) = sleep_command(120);
    let f = fixture(
        vec![
            sse(vec![
                tool_call_frame(
                    "c-service",
                    "run_command",
                    serde_json::json!({"program": program, "args": args, "background": true}),
                ),
                finish_frame("tool_calls"),
            ]),
            yielded_turn("服务已启动，窗口到此为止；下一窗口继续。"),
            sse(vec![
                tool_call_frame(
                    "c-hold",
                    "run_command",
                    serde_json::json!({"program": "sleep", "args": ["1"]}),
                ),
                finish_frame("tool_calls"),
            ]),
            completed_turn("c-done", "两个窗口的工作都已完成"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;

    let runtime = Arc::new(InProcessRuntimeClient::new_with_options(
        f.app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
        true,
    ));
    runtime
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: OBJECTIVE.to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let (service_id, pid) = loop {
        let ids = f
            .app
            .background_tasks()
            .active_ids_for_scope(f.session.as_str())
            .await;
        if let Some(id) = ids.into_iter().next() {
            let snapshot = f
                .app
                .background_tasks()
                .get(&id)
                .await
                .expect("the background service record exists");
            if let Some(pid) = snapshot.pid {
                break (id, pid);
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the goal service never started"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(
        os_process_exists(pid),
        "the service exists before the yield"
    );

    // The second window is now the one running. The service must still be the
    // SAME live process: the yielded window's terminal did not settle the goal,
    // so it did not own the service's lifetime.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let db = f.app.open_database().await.unwrap();
        let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
        if turns.len() >= 2 && turns.iter().any(|turn| turn.status == "running") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second window was never admitted: {turns:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let during = f
        .app
        .background_tasks()
        .get(&service_id)
        .await
        .expect("the registry retains the service");
    assert_eq!(during.status, BackgroundTaskStatus::Running);
    assert_eq!(
        during.pid,
        Some(pid),
        "same process identity across windows"
    );
    assert!(
        os_process_exists(pid),
        "the service outlived the yielded window"
    );
    let live = goals(&f).await;
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(
        live[0].state,
        GoalState::Running,
        "the yield ended the window, not the goal: {live:?}"
    );
    assert_eq!(
        live[0].windows_run, 1,
        "only the yielded window has committed so far: {live:?}"
    );

    settled(&f, 2).await;
    wait_for_goals(&f, |goals| {
        goals.iter().any(|goal| goal.state == GoalState::Settled)
    })
    .await;

    // The goal's terminal is the service's lifetime boundary.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let snapshot = f
            .app
            .background_tasks()
            .get(&service_id)
            .await
            .expect("the registry retains the settled service");
        if snapshot.status == BackgroundTaskStatus::Killed && !os_process_exists(pid) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the goal terminal did not reap its service: {snapshot:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
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
    // One failed logical request = the first attempt plus the route's default
    // retry budget. The provider advertises `Retry-After: 0`, so the retries
    // are immediate and this stays a fast, deterministic test.
    let mut responses =
        vec![unavailable_with_immediate_retry(); 1 + leveler_model::DEFAULT_MAX_RETRIES as usize];
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
    wait_for_goals(&f, |goals| goals.iter().any(|goal| goal.windows_run >= 2)).await;

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

// ---------------------------------------------------------------------------
// A supplement stated to an existing Goal.
//
// The user's message, the scope it extends, the authorization to execute it
// and the completion of the goal are four separate facts. A Goal-route session
// whose window ended at the user's own pause keeps serving that goal, so a
// supplement typed next is scope for it — recorded, never executed, and never a
// release of the pause.
// ---------------------------------------------------------------------------

const PAUSED_OBJECTIVE: &str = "完成两个阶段：阶段 1 在仓库根目录创建 stage-1.txt，\
     然后运行 sh -c 'cat stage-1.txt' 确认；阶段 1 完成后停下，等待我确认再执行阶段 2；\
     阶段 2 创建 stage-2.txt，然后运行 sh -c 'cat stage-2.txt' 确认。";

/// Stage 1 delivered and the model's own plan reached the user's boundary.
fn paused_after_stage_one() -> Vec<MockResponse> {
    vec![
        sse(vec![
            tool_call_frame(
                "p1",
                "update_plan",
                serde_json::json!({"plan": [
                    {"id": "stage-1", "step": "阶段 1：创建 stage-1.txt 并运行 sh -c 'cat stage-1.txt' 确认", "status": "in_progress"},
                    {"step": "阶段 1 完成后停下，等待我确认", "status": "pending"},
                    {"id": "stage-2", "step": "阶段 2：创建 stage-2.txt 并运行 sh -c 'cat stage-2.txt' 确认", "status": "pending"}
                ]}),
            ),
            finish_frame("tool_calls"),
        ]),
        declare_pause_after_stage_one(),
        sse(vec![
            tool_call_frame(
                "w1",
                "write_file",
                serde_json::json!({"path": "stage-1.txt", "content": "stage 1\n"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "p2",
                "update_plan",
                serde_json::json!({"plan": [
                    {"id": "stage-1", "step": "阶段 1：创建 stage-1.txt 并运行 sh -c 'cat stage-1.txt' 确认", "status": "completed"},
                    {"step": "阶段 1 完成后停下，等待我确认", "status": "completed"},
                    {"id": "stage-2", "step": "阶段 2：创建 stage-2.txt 并运行 sh -c 'cat stage-2.txt' 确认", "status": "pending"}
                ]}),
            ),
            finish_frame("tool_calls"),
        ]),
        plain_answer("阶段 1 已完成。按你的要求，阶段 2 等你的确认。"),
    ]
}

/// A supplement that names a new deliverable and contains no `继续`.
const SUPPLEMENT: &str = "顺便把 stage-extra.txt 也补上，内容为一行 extra，\
     然后运行 sh -c 'cat stage-extra.txt' 确认。";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_supplement_to_a_paused_goal_is_scope_and_keeps_the_pause() {
    let mut responses = paused_after_stage_one();
    // The model reads the supplement as an amendment to the same goal; it is
    // scope, not an authorization to continue.
    responses.push(declare_intent_frame(
        "i-amend",
        SUPPLEMENT,
        serde_json::json!([{"kind": "amend_current_task"}]),
    ));
    responses.push(plain_answer("补充要求已记下；阶段 2 仍等你的确认。"));
    let f = fixture(responses, leveler_agent::CollaborationMode::Goal).await;
    run_goal_turn(&f, PAUSED_OBJECTIVE).await;

    let paused = goals(&f).await;
    assert_eq!(paused.len(), 1, "{paused:?}");
    let goal_id = paused[0].id.clone();
    assert!(
        paused[0].user_paused_at.is_some(),
        "the user's boundary is durable: {paused:?}"
    );
    assert!(paused[0].amendments.is_empty(), "{paused:?}");

    client(&f)
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: SUPPLEMENT.to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    settled(&f, 2).await;

    let goals = goals(&f).await;
    assert_eq!(
        goals.len(),
        1,
        "a supplement opens no second goal: {goals:?}"
    );
    assert_eq!(goals[0].id, goal_id, "identity is the goal, not the text");
    assert!(
        goals[0].amendments.iter().any(|a| a.trim() == SUPPLEMENT),
        "the user's own words are durable scope without a resume keyword: {goals:?}"
    );
    assert!(
        goals[0].user_paused_at.is_some(),
        "adding a requirement is not an authorization to continue: {goals:?}"
    );
    assert_eq!(goals[0].state, GoalState::Running);
    assert!(
        !f._tmp.path().join("stage-extra.txt").exists(),
        "the supplement is recorded, not executed"
    );
    assert!(
        !f._tmp.path().join("stage-2.txt").exists(),
        "the withheld stage stays withheld"
    );
    // The original contract is untouched by the supplement: the objective is
    // the user's own words, and the acceptance the model committed to stays as
    // it was declared.
    assert_eq!(goals[0].objective, PAUSED_OBJECTIVE);
    assert_eq!(
        goals[0].acceptance, paused[0].acceptance,
        "a supplement does not rewrite the stored acceptance: {goals:?}"
    );
}

/// A status question is answered and changes nothing: no amendment, no new
/// goal, no release of the pause.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_status_question_inside_a_paused_goal_adds_no_scope() {
    let mut responses = paused_after_stage_one();
    // The model reads the message as a question about the task, not as an
    // authorization to drive the withheld stage.
    responses.push(declare_intent_frame(
        "i-query",
        "刚才修改了哪些文件？测试通过了吗？",
        serde_json::json!([{"kind": "answer_only"}]),
    ));
    responses.push(plain_answer("阶段 1 创建了 stage-1.txt；没有跑测试。"));
    let f = fixture(responses, leveler_agent::CollaborationMode::Goal).await;
    run_goal_turn(&f, PAUSED_OBJECTIVE).await;

    client(&f)
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "刚才修改了哪些文件？测试通过了吗？现在进度怎么样？".to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    settled(&f, 2).await;

    let goals = goals(&f).await;
    assert_eq!(goals.len(), 1, "{goals:?}");
    assert_eq!(
        goals[0].amendments,
        Vec::<String>::new(),
        "a question about the work is not scope: {goals:?}"
    );
    assert!(
        goals[0].user_paused_at.is_some(),
        "the question does not release the pause: {goals:?}"
    );
}

// ---------------------------------------------------------------------------
// Two goals owed at once: an earlier one the user paused, and the one this
// session is executing.
//
// The projection must show the goal the LIVE turn serves. "The first running
// row" would present goal A's pause while goal B runs, which is exactly the
// state the user cannot act on.
// ---------------------------------------------------------------------------

/// Chat-sourced goal A: committed, stage 1 delivered, stopped at the user's
/// own boundary.
#[cfg(unix)]
fn committed_pause_turn() -> Vec<MockResponse> {
    vec![
        sse(vec![
            tool_call_frame(
                "c-commit",
                "commit_task",
                serde_json::json!({
                    "objective": PAUSED_OBJECTIVE,
                    "acceptance": ["stage-1.txt 存在", "stage-2.txt 存在"]
                }),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "p1",
                "update_plan",
                serde_json::json!({"plan": [
                    {"id": "stage-1", "step": "阶段 1：创建 stage-1.txt 并运行 sh -c 'cat stage-1.txt' 确认", "status": "in_progress"},
                    {"step": "阶段 1 完成后停下，等待我确认", "status": "pending"},
                    {"id": "stage-2", "step": "阶段 2：创建 stage-2.txt 并运行 sh -c 'cat stage-2.txt' 确认", "status": "pending"}
                ]}),
            ),
            finish_frame("tool_calls"),
        ]),
        declare_pause_after_stage_one(),
        sse(vec![
            tool_call_frame(
                "w1",
                "write_file",
                serde_json::json!({"path": "stage-1.txt", "content": "stage 1\n"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "p2",
                "update_plan",
                serde_json::json!({"plan": [
                    {"id": "stage-1", "step": "阶段 1：创建 stage-1.txt 并运行 sh -c 'cat stage-1.txt' 确认", "status": "completed"},
                    {"step": "阶段 1 完成后停下，等待我确认", "status": "completed"},
                    {"id": "stage-2", "step": "阶段 2：创建 stage-2.txt 并运行 sh -c 'cat stage-2.txt' 确认", "status": "pending"}
                ]}),
            ),
            finish_frame("tool_calls"),
        ]),
        plain_answer("阶段 1 已完成。按你的要求，阶段 2 等你的确认。"),
    ]
}

#[cfg(unix)]
const SECOND_OBJECTIVE: &str = "创建 export-b1.txt，然后运行 sh ./verify.sh 验收。\
     一次做完，中间不要停下来等我确认。";

/// A goal B turn whose FIRST model call is slow, so the test can observe the
/// session while goal B is genuinely executing. The slow response is a plain
/// answer: the goal contract then drives the next round, which does the work.
#[cfg(unix)]
fn running_second_goal() -> Vec<MockResponse> {
    vec![
        MockResponse::SilentThenJson {
            silent_ms: 600,
            body: serde_json::json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "开始执行。"},
                    "finish_reason": "stop"
                }]
            })
            .to_string(),
        },
        sse(vec![
            tool_call_frame(
                "b-w1",
                "write_file",
                serde_json::json!({"path": "export-b1.txt", "content": "b1\n"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "b-run",
                "run_command",
                serde_json::json!({"program": "sh", "args": ["./verify.sh"], "reason": "run the acceptance check"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "b-done",
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "export-b1.txt 已创建并验收",
                    "verification": {"status": "passed", "detail": "sh ./verify.sh"},
                    "requirements": [
                        {"requirement": "export-b1.txt", "status": "done",
                         "evidence": "created export-b1.txt"}
                    ]
                }),
            ),
            finish_frame("tool_calls"),
        ]),
    ]
}

/// The fixed turn speaks `sh` and `./verify.sh`; the pause boundary it checks is
/// the product's, but this fixture only runs on a POSIX host.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_second_goal_is_the_projected_goal_while_it_executes() {
    let mut responses = committed_pause_turn();
    responses.extend(running_second_goal());
    let f = fixture(responses, leveler_agent::CollaborationMode::Chat).await;
    let verifier = f._tmp.path().join("verify.sh");
    std::fs::write(&verifier, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&verifier, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // ONE client: `active` (which window is running) lives on the client, so a
    // snapshot must come from the same runtime that admitted the turn.
    let runtime = client(&f);
    // Subscribed before the first turn: the terminal event is the runtime's
    // own "this turn no longer holds the session" boundary.
    let mut events = runtime.subscribe();

    // ── goal A: committed, paused at the user's own boundary ───────────────
    runtime
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: PAUSED_OBJECTIVE.to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    settled(&f, 1).await;
    // The turn row stops running inside the engine; the admission is released
    // when the runtime publishes the terminal event. Wait for that boundary —
    // not the row — before submitting `/goal B`, or the command can be taken
    // as a steer into the turn that is already exiting.
    wait_for_turn_terminal(&mut events).await;
    // The conclusion that writes this window's goal facts — including the
    // pause — runs after the turn row turns terminal (the same ordering
    // `wait_for_goals` documents). Wait for the durable fact this assertion
    // is about, not only for the turn row.
    wait_for_goals(&f, |goals| {
        goals.len() == 1 && goals[0].user_paused_at.is_some()
    })
    .await;
    let after_pause = goals(&f).await;
    assert_eq!(after_pause.len(), 1, "{after_pause:?}");
    let goal_a = after_pause[0].id.clone();
    assert!(
        after_pause[0].user_paused_at.is_some(),
        "A is paused: {after_pause:?}"
    );
    let a_projection = runtime
        .snapshot(&f.session)
        .await
        .unwrap()
        .execution_task
        .expect("A is owed");
    assert_eq!(a_projection.goal_id, goal_a.as_str());
    assert!(a_projection.user_paused);
    assert!(!a_projection.running);

    // ── goal B starts while A is still owed and paused ─────────────────────
    runtime
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: format!("/goal {SECOND_OBJECTIVE}"),
            attachments: Vec::new(),
        })
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last = None;
    let mut live = None;
    while tokio::time::Instant::now() < deadline {
        let snapshot = runtime.snapshot(&f.session).await.unwrap();
        last = snapshot.execution_task.clone();
        if let Some(task) = snapshot.execution_task
            && task.running
            && task.goal_id != goal_a.as_str()
        {
            live = Some(task);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let live = live.unwrap_or_else(|| {
        panic!("goal B must be projected while it executes; last execution_task: {last:?}")
    });
    let goals_now = goals(&f).await;
    let goal_b = goals_now
        .iter()
        .find(|goal| goal.id != goal_a)
        .expect("goal B exists")
        .id
        .clone();
    assert_eq!(live.goal_id, goal_b.as_str(), "the live turn serves B");
    assert!(!live.user_paused, "B never inherits A's pause");
    assert_eq!(live.objective, SECOND_OBJECTIVE);
    assert!(
        goals_now
            .iter()
            .any(|goal| goal.id == goal_a && goal.user_paused_at.is_some()),
        "A's own pause is durable while B runs: {goals_now:?}"
    );

    // ── B finishes; nothing is left owed, and A is unchanged ───────────────
    wait_for_goals(&f, |goals| {
        goals
            .iter()
            .any(|goal| goal.id == goal_b && goal.state == GoalState::Settled)
    })
    .await;
    let final_state = goals(&f).await;
    assert_eq!(final_state.len(), 2, "{final_state:?}");
    let a_now = final_state
        .iter()
        .find(|goal| goal.id == goal_a)
        .expect("A still exists");
    assert_eq!(a_now.state, GoalState::Running);
    assert!(a_now.user_paused_at.is_some(), "A's history is intact");
    assert_eq!(a_now.objective, PAUSED_OBJECTIVE);
    let b_now = final_state
        .iter()
        .find(|goal| goal.id == goal_b)
        .expect("B exists");
    assert!(b_now.user_paused_at.is_none());
    let after_b = runtime.snapshot(&f.session).await.unwrap();
    assert_eq!(
        after_b.execution_task.map(|task| task.goal_id),
        Some(goal_a.to_string()),
        "with B settled, the still-owed goal is A again"
    );
    assert!(f._tmp.path().join("export-b1.txt").exists(), "B delivered");
    assert!(
        !f._tmp.path().join("stage-2.txt").exists(),
        "B never executed A's withheld stage"
    );
}

/// The reported P2-C failure, at the tool surface: an interactive Goal whose
/// model needed a user decision had no `request_user_input` in its first
/// request, so the only structured way to ask was invisible. A human-attached
/// turn must advertise it without a capability-enable round.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interactive_goal_advertises_request_user_input_on_the_first_request() {
    let f = fixture(vec![blocked_turn()], leveler_agent::CollaborationMode::Goal).await;
    client(&f)
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: OBJECTIVE.to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    settled(&f, 1).await;

    let bodies = f.server.request_bodies().await;
    assert!(!bodies.is_empty(), "the model was never asked");
    let names = tool_names(&bodies[0]);
    assert!(
        names.iter().any(|name| name == "request_user_input"),
        "a human-attached Goal must expose the structured question tool on its first request; \
         got {names:?}"
    );
}

/// The unattended side of the boundary: no human is attached, so no tool may
/// pretend one is. The tool is absent from the first request rather than
/// present and guaranteed to return "unattended".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unattended_goal_does_not_advertise_request_user_input() {
    let f = fixture(vec![blocked_turn()], leveler_agent::CollaborationMode::Goal).await;
    run_goal_turn(&f, OBJECTIVE).await;

    let bodies = f.server.request_bodies().await;
    assert!(!bodies.is_empty(), "the model was never asked");
    let names = tool_names(&bodies[0]);
    assert!(
        !names.iter().any(|name| name == "request_user_input"),
        "an unattended Goal must not advertise a user channel; got {names:?}"
    );
}

/// Without a channel there is nothing to repair to. A structured
/// `user_decision` block is accepted, and the receipt states the degradation:
/// the decision was never put to the user, so the runtime does not pretend an
/// interaction happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unattended_user_decision_block_is_an_explicit_degradation() {
    let f = fixture(
        vec![user_decision_blocked_turn()],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    run_goal_turn(&f, OBJECTIVE).await;

    let bodies = f.server.request_bodies().await;
    assert_eq!(
        bodies.len(),
        1,
        "no question channel exists, so there is no repair round: {bodies:?}"
    );
    let texts = tool_result_texts(&f).await;
    assert!(
        texts
            .iter()
            .any(|text| text.contains("never put to the user as a question")),
        "the accepted block must state that no interaction happened: {texts:?}"
    );
}
