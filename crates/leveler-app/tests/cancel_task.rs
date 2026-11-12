//! Explicit task cancellation is terminal, not a resumable pause.
//!
//! `CancelCurrentTurn` (Esc) interrupts a window and leaves the logical task
//! resumable. `CancelTask` (`/goal cancel`) ends the task: the runtime records
//! a `cancelled` outcome, settles the goal, and refuses a later `继续` rather
//! than silently reopening work the user ended.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_execution::{AutoApprove, PermissionProfile};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{EngineStores, GoalState, TurnRepository};
use leveler_test_support::{MockResponse, MockServer, sleep_command};
use tokio_util::sync::CancellationToken;

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
    _tmp: tempfile::TempDir,
    server: MockServer,
}

async fn fixture(responses: Vec<MockResponse>) -> Fixture {
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
    let app = Arc::new(Application::assemble(layout).unwrap());
    let session = app
        .create_session(&ModelRef::new("mock", "m"), "finish the inventory work")
        .await
        .unwrap();
    Fixture {
        app,
        session,
        _tmp: tmp,
        server,
    }
}

/// A window that ends early with the plan still open: resumable.
fn interrupted_work() -> Vec<MockResponse> {
    vec![
        sse(vec![
            tool_call_frame(
                "c-plan",
                "update_plan",
                serde_json::json!({"plan": [
                    {"step": "implement the inventory core", "status": "in_progress"},
                    {"step": "run the tests", "status": "pending"}
                ]}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "c-read",
                "read_file",
                serde_json::json!({"path": "src/lib.rs"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "c-goal",
                "update_goal",
                serde_json::json!({"status": "complete", "summary": "done"}),
            ),
            finish_frame("tool_calls"),
        ]),
    ]
}

/// A turn that stays running: one tool call blocks for a while.
fn blocking_work() -> Vec<MockResponse> {
    let (program, args) = sleep_command(30);
    vec![sse(vec![
        tool_call_frame(
            "c-sleep",
            "run_command",
            serde_json::json!({ "program": program, "args": args }),
        ),
        finish_frame("tool_calls"),
    ])]
}

async fn last_outcome(db: &leveler_storage::Database, session: &leveler_core::SessionId) -> String {
    use leveler_engine::EngineEvent;
    let stores = EngineStores::from_database(db);
    // The task terminal is committed after the turn terminal, and markedly so
    // on Windows, where stopping the child process is not instantaneous. Wait
    // for the terminal that belongs to the newest turn: an earlier turn's
    // task_finished is already durable, so its mere presence is not evidence
    // that this turn has settled.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let row = loop {
        let turn = stores
            .events
            .load_last_by_type(session, "turn_finished", None)
            .await
            .unwrap();
        if let Some(row) = stores
            .events
            .load_last_by_type(session, "task_finished", None)
            .await
            .unwrap()
            && turn
                .as_ref()
                .is_none_or(|turn| row.sequence > turn.sequence)
        {
            break row;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no task_finished event for the newest turn was recorded"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    match EngineEvent::from_payload(&row.payload).unwrap() {
        EngineEvent::TaskFinished { outcome, .. } => outcome.as_str().to_string(),
        other => panic!("unexpected event: {other:?}"),
    }
}

async fn settled(app: &Application, session: &leveler_core::SessionId, min_turns: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let db = app.open_database().await.unwrap();
        let turns = TurnRepository::new(&db).list(session).await.unwrap();
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

async fn wait_for_running_turn(app: &Application, session: &leveler_core::SessionId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let db = app.open_database().await.unwrap();
        let turns = TurnRepository::new(&db).list(session).await.unwrap();
        if turns.iter().any(|turn| turn.status == "running") {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no turn started in time: {turns:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Make an interrupted, resumable task through the bounded eval entry point.
async fn interrupted_session(f: &Fixture) {
    f.app
        .run_in_session_bounded(
            &f.session,
            &ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            "finish the inventory work",
            Arc::new(AutoApprove),
            false,
            &mut |_| {},
            CancellationToken::new(),
            2,
            None,
        )
        .await
        .expect("a bounded stop is a normal return");
}

fn client(f: &Fixture) -> Arc<dyn InteractiveRuntimeClient> {
    Arc::new(InProcessRuntimeClient::new(
        f.app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ))
}

/// B: an explicit cancel of an interrupted task records `cancelled` and settles
/// the goal, so it is no longer resumable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_cancel_of_an_interrupted_task_is_terminal() {
    let f = fixture(interrupted_work()).await;
    interrupted_session(&f).await;

    let db = f.app.open_database().await.unwrap();
    assert_eq!(last_outcome(&db, &f.session).await, "budget_limited");
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stores
            .goals
            .for_task(&task)
            .await
            .unwrap()
            .iter()
            .filter(|goal| goal.state == GoalState::Running)
            .count(),
        1,
        "the fixture must leave a running goal"
    );
    drop(db);

    client(&f)
        .send(ClientCommand::CancelTask {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();

    let db = f.app.open_database().await.unwrap();
    assert_eq!(
        last_outcome(&db, &f.session).await,
        "cancelled",
        "an explicit cancel is its own terminal outcome"
    );
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    assert!(
        stores
            .goals
            .for_task(&task)
            .await
            .unwrap()
            .iter()
            .all(|goal| goal.state == GoalState::Settled),
        "cancelling the task settles its goal"
    );
}

/// C: `继续` after an explicit cancel must NOT resume the cancelled task.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continue_after_explicit_cancel_is_refused() {
    let f = fixture(interrupted_work()).await;
    interrupted_session(&f).await;

    let db = f.app.open_database().await.unwrap();
    let turns_before = TurnRepository::new(&db)
        .list(&f.session)
        .await
        .unwrap()
        .len();
    drop(db);

    let client = client(&f);
    client
        .send(ClientCommand::CancelTask {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    let refused = client
        .send(ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续".to_string(),
        })
        .await;
    let message = refused.expect_err("a continuation on a cancelled task must be refused");
    assert!(
        message.to_string().contains("已被取消"),
        "the refusal names the cancellation: {message}"
    );

    let db = f.app.open_database().await.unwrap();
    let turns_after = TurnRepository::new(&db).list(&f.session).await.unwrap();
    assert_eq!(
        turns_after.len(),
        turns_before,
        "a refused continuation must not create a turn: {turns_after:?}"
    );
}

/// A window that declares its goal blocked: the goal keeps owing the work,
/// while the session's route returns to Chat for the next message.
fn blocked_work() -> MockResponse {
    sse(vec![
        tool_call_frame(
            "c-block",
            "update_goal",
            serde_json::json!({"status": "blocked", "summary": "missing credentials"}),
        ),
        finish_frame("tool_calls"),
    ])
}

/// E: `/goal cancel` ends the LOGICAL task even when the running window has
/// not bound the goal it owes. After a blocked window the user's continuation
/// runs as an ordinary Chat turn, and the model may not have declared against
/// the goal before the cancel arrives. Cancelling then must settle that owed
/// goal in the same terminal transaction; leaving it `running` would expose a
/// row no continuation can ever re-enter (the task is cancelled).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_cancel_of_a_running_unbound_window_settles_the_owed_goal() {
    let mut responses = blocking_work();
    responses.insert(0, blocked_work());
    let f = fixture(responses).await;
    // The concrete client exposes the live-turn lease. A terminal row can be
    // committed while the admission is still held, and the next message must
    // wait for the lease rather than only for the turn row.
    let client = InProcessRuntimeClient::new(
        f.app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );
    client
        .send(ClientCommand::RunGoal {
            session_id: f.session.clone(),
            content: "finish the inventory work".to_string(),
            require_new_turn: false,
        })
        .await
        .unwrap();
    settled(&f.app, &f.session, 1).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while client.has_live_turn(&f.session) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the blocked window never released its admission"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals_before = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(
        goals_before
            .iter()
            .filter(|goal| goal.state == GoalState::Running)
            .count(),
        1,
        "the blocked window keeps the goal owed: {goals_before:?}"
    );
    let session_row = leveler_storage::SessionRepository::new(&db)
        .get(&f.session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        session_row.collaboration, "chat",
        "a blocked window hands control back: the continuation is an ordinary Chat message"
    );
    drop(db);

    // The continuation is an ordinary Chat message; no declaration has bound
    // it to the goal when the user cancels.
    client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "继续".to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    wait_for_running_turn(&f.app, &f.session).await;
    client
        .send(ClientCommand::CancelTask {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    settled(&f.app, &f.session, 2).await;

    let db = f.app.open_database().await.unwrap();
    assert_eq!(
        last_outcome(&db, &f.session).await,
        "cancelled",
        "a running task cancel must record a cancelled terminal"
    );
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert!(
        goals.iter().all(|goal| goal.state == GoalState::Settled),
        "cancelling the logical task settles the goal it still owed: {goals:?}"
    );
}

/// E2 (real client route): the UI's `继续` after a blocked window is a RESUME,
/// not a plain chat message. Cancelling that resumed window must settle the
/// owed goal too — and the later resume must then be refused. A resume turn
/// carries its continuation lineage in its WAL payload; settling must not
/// depend on the cancelled turn's own declaration.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_cancel_of_a_resumed_window_settles_the_owed_goal() {
    let mut responses = blocking_work();
    responses.insert(0, blocked_work());
    responses.push(sse(vec![finish_frame("stop")]));
    let f = fixture(responses).await;
    let client = InProcessRuntimeClient::new(
        f.app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );
    client
        .send(ClientCommand::RunGoal {
            session_id: f.session.clone(),
            content: "finish the inventory work".to_string(),
            require_new_turn: false,
        })
        .await
        .unwrap();
    settled(&f.app, &f.session, 1).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while client.has_live_turn(&f.session) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the blocked window never released its admission"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stores
            .goals
            .for_task(&task)
            .await
            .unwrap()
            .iter()
            .filter(|goal| goal.state == GoalState::Running)
            .count(),
        1,
        "the blocked window keeps its goal owed"
    );
    drop(db);

    // Exactly what the TUI sends when the user accepts the blocked goal's
    // `继续` affordance.
    client
        .send(ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续".to_string(),
        })
        .await
        .unwrap();
    wait_for_running_turn(&f.app, &f.session).await;
    client
        .send(ClientCommand::CancelTask {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    settled(&f.app, &f.session, 2).await;

    let db = f.app.open_database().await.unwrap();
    assert_eq!(last_outcome(&db, &f.session).await, "cancelled");
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert!(
        goals.iter().all(|goal| goal.state == GoalState::Settled),
        "a cancelled resumed window must not leave its goal running: {goals:?}"
    );
    let turns_before = TurnRepository::new(&db)
        .list(&f.session)
        .await
        .unwrap()
        .len();
    drop(db);

    let refused = client
        .send(ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续".to_string(),
        })
        .await;
    refused.expect_err("a cancelled task must refuse a later resume");
    let db = f.app.open_database().await.unwrap();
    assert_eq!(
        TurnRepository::new(&db)
            .list(&f.session)
            .await
            .unwrap()
            .len(),
        turns_before,
        "a refused resume must not create a turn"
    );
}

/// A window that ends with the goal still owed, so the runtime owns the next
/// window instead of the user.
fn yielded_work() -> MockResponse {
    sse(vec![
        tool_call_frame(
            "c-yield",
            "update_goal",
            serde_json::json!({"status": "yield", "summary": "本窗口到此为止，下一窗口继续"}),
        ),
        finish_frame("tool_calls"),
    ])
}

/// B (running): cancelling the logical task while a turn runs records
/// `cancelled`, not resumable `interrupted`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_cancel_of_a_running_task_records_cancelled() {
    let f = fixture(blocking_work()).await;
    let client = client(&f);
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunGoal {
            session_id: f.session.clone(),
            content: "finish the inventory work".to_string(),
            require_new_turn: false,
        })
        .await
        .unwrap();
    wait_for_running_turn(&f.app, &f.session).await;

    client
        .send(ClientCommand::CancelTask {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    settled(&f.app, &f.session, 1).await;

    let db = f.app.open_database().await.unwrap();
    assert_eq!(
        last_outcome(&db, &f.session).await,
        "cancelled",
        "a running task cancel must record a cancelled terminal"
    );
    drop(db);

    // The user sees the terminal task-cancelled event.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Ok(RuntimeEvent::TaskCancelled)) => {
                saw = true;
                break;
            }
            Ok(Ok(_)) | Ok(Err(_)) => continue,
            Err(_) => break,
        }
    }
    assert!(saw, "the runtime must publish a TaskCancelled terminal");
}

/// A window the runtime continued is still the user's to end. The cancel
/// reaches the continued window (the runtime, not a message, started it),
/// settles the logical goal as `cancelled`, and refuses a later automatic
/// continuation without writing anything: the user's cancellation stays the
/// last durable terminal instead of being overwritten by a failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_continued_window_ends_the_logical_goal() {
    let mut responses = blocking_work();
    responses.insert(0, yielded_work());
    let f = fixture(responses).await;
    let client = client(&f);
    client
        .send(ClientCommand::RunGoal {
            session_id: f.session.clone(),
            content: "finish the inventory work".to_string(),
            require_new_turn: false,
        })
        .await
        .unwrap();

    // Window 1 yields; the runtime starts window 2 with no user input. The
    // second turn row and second model request are the runtime's continuation.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let db = f.app.open_database().await.unwrap();
        let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
        if turns.len() >= 2
            && turns.iter().any(|turn| turn.status == "running")
            && f.server.request_bodies().await.len() >= 2
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the yielded window was never continued: {turns:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    client
        .send(ClientCommand::CancelTask {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    settled(&f.app, &f.session, 2).await;

    let db = f.app.open_database().await.unwrap();
    assert_eq!(
        last_outcome(&db, &f.session).await,
        "cancelled",
        "cancelling a continued window ends the LOGICAL task"
    );
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert!(
        goals.iter().all(|goal| goal.state == GoalState::Settled),
        "the cancelled logical task leaves no owed goal: {goals:?}"
    );
    drop(db);
    assert_eq!(
        f.server.request_bodies().await.len(),
        2,
        "a cancelled logical task starts no further window"
    );

    // The continuation that would have followed finds nothing owed and writes
    // nothing: no request, no terminal.
    let refused = f
        .app
        .resume_in_session_continuation(
            &f.session,
            Arc::new(AutoApprove),
            Arc::new(leveler_agent::AutoClarify),
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            &mut |_| {},
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(refused, Err(leveler_app::AppError::GoalSettled(_))),
        "a settled goal is not owed: {refused:?}"
    );
    assert_eq!(
        f.server.request_bodies().await.len(),
        2,
        "a refused continuation sends no model request"
    );
    let db = f.app.open_database().await.unwrap();
    assert_eq!(
        last_outcome(&db, &f.session).await,
        "cancelled",
        "a refused continuation must not overwrite the user's cancellation"
    );
}
