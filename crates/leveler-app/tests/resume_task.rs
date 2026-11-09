//! `继续` is a logical-task continuation, not a fresh chat turn.
//!
//! When a task window ends early (an interruption, or a budget boundary), the
//! session still owns its task, goal and plan. The next continuation must
//! re-enter through the runtime's resume path so that identity is preserved:
//! same task, same goal, plan seeded before the model can re-declare one, and
//! an amendment added on top of the original objective rather than replacing
//! it.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient};
use leveler_execution::{AutoApprove, PermissionProfile};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{EngineStores, EventRepository, GoalRecord, GoalState, TurnRepository};
use leveler_test_support::{MockResponse, MockServer};
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

/// One model round stating its reading of the user's message through the
/// structured intent contract. Pauses, continuations and cancellations are
/// declared by the model, never inferred by the runtime from the user's words.
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
            // The axis is the fixture's: a Goal session runs the goal profile
            // these tests are about, while a Chat session keeps a `继续` with
            // nothing to resume an ordinary chat turn.
            .with_collaboration(axis),
    );
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

/// Round 1 declares a two-step plan and leaves step 1 in progress; round 2 does
/// real work. A bounded run then stops at its ceiling with the plan still open.
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
                "c-final-plan",
                "update_plan",
                serde_json::json!({"plan": [
                    {"step": "implement the inventory core", "status": "completed"},
                    {"step": "run the tests", "status": "pending"}
                ]}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                "c-goal",
                "update_goal",
                serde_json::json!({"status": "complete", "summary": "continued to the end"}),
            ),
            finish_frame("tool_calls"),
        ]),
    ]
}

async fn settled(app: &Application, session: &leveler_core::SessionId, min_turns: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let db = app.open_database().await.unwrap();
        let turns = TurnRepository::new(&db).list(session).await.unwrap();
        // The wait is about TURNS. A goal that still owes work (blocked,
        // recoverably failed, budget-stopped) is a normal resting state, not
        // evidence that something is still running.
        if turns.len() >= min_turns && turns.iter().all(|turn| turn.status != "running") {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not settle in time: turns={turns:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for the turn row AND the runtime's admit lease.
///
/// `settled` stops at the durable row, but the runtime releases its per-session
/// lease *after* the terminal event is enqueued, so a console that submits the
/// follow-up in that window is refused with "already has an active turn". That
/// is a race in the probe, not a product fact: the product's own admission
/// contract is the lease.
async fn settled_with_client(
    app: &Application,
    client: &InProcessRuntimeClient,
    session: &leveler_core::SessionId,
    min_turns: usize,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let row_stopped = {
            let db = app.open_database().await.unwrap();
            let turns = TurnRepository::new(&db).list(session).await.unwrap();
            assert!(
                tokio::time::Instant::now() < deadline,
                "the turn did not settle in time: turns={turns:?}"
            );
            turns.len() >= min_turns && turns.iter().all(|turn| turn.status != "running")
        };
        if row_stopped && !client.has_live_turn(session) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until the session's goal records satisfy `done`.
///
/// A goal is settled in the task's terminal transaction, which lands after the
/// turn row stops running; reading the record before that commit observes a
/// state the window has already left.
async fn wait_for_goals(
    app: &Application,
    session: &leveler_core::SessionId,
    done: impl Fn(&[GoalRecord]) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let db = app.open_database().await.unwrap();
        let stores = EngineStores::from_database(&db);
        let goals = match stores.tasks.task_for_session(session).await.unwrap() {
            Some(task) => stores.goals.for_task(&task).await.unwrap(),
            None => Vec::new(),
        };
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

/// The core closure: an interrupted task + `继续` re-enters resume, keeps the
/// task's goal, seeds its plan, and carries the amendment as an addition.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuation_resumes_the_logical_task_and_seeds_its_plan() {
    assert_continuation_preserves_task(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_submit_continuation_preserves_original_goal_and_seeds_plan() {
    assert_continuation_preserves_task(true).await;
}

async fn assert_continuation_preserves_task(ordinary_submit: bool) {
    let f = fixture(interrupted_work(), leveler_agent::CollaborationMode::Goal).await;
    let model = ModelRef::new("mock", "m");

    // A window that ends early: two rounds, plan still open.
    let report = f
        .app
        .run_in_session_bounded(
            &f.session,
            &model,
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
    assert!(
        matches!(
            report.stop_reason,
            leveler_agent::StopReason::TurnLimitReached
                | leveler_agent::StopReason::BudgetExhausted
        ),
        "the fixture must end at the budget boundary: {report:?}"
    );

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let running: Vec<_> = stores
        .goals
        .for_task(&task)
        .await
        .unwrap()
        .into_iter()
        .filter(|goal| goal.state == GoalState::Running)
        .collect();
    assert_eq!(running.len(), 1, "the interrupted task still owes its goal");
    let goal_id = running[0].id.clone();
    drop(db);

    // The user continues, with an amendment.
    let client = Arc::new(InProcessRuntimeClient::new(
        f.app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    let client: Arc<dyn InteractiveRuntimeClient> = client;
    let command = if ordinary_submit {
        ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "继续，但是先不要跑测试".to_string(),
            attachments: Vec::new(),
        }
    } else {
        ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续，但是先不要跑测试".to_string(),
        }
    };
    client.send(command).await.unwrap();
    settled(&f.app, &f.session, 2).await;
    wait_for_goals(&f.app, &f.session, |goals| {
        goals
            .iter()
            .any(|goal| goal.id == goal_id && goal.state == GoalState::Settled)
    })
    .await;

    let db = f.app.open_database().await.unwrap();
    let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
    assert_eq!(turns.len(), 2, "the continuation is one more turn");
    let resumed = &turns[1];
    assert_eq!(
        resumed.kind, "user",
        "the continuation must enter the resume path, not a fresh chat turn"
    );
    let payload = resumed.payload.as_deref().unwrap_or_default();
    assert!(
        payload.contains("但是先不要跑测试"),
        "the amendment must be the resumed turn's own input: {payload}"
    );

    // The plan is seeded before the model can re-declare one: the first
    // `plan_updated` of the resumed turn precedes its first tool call.
    let stores = EngineStores::from_database(&db);
    let rows = EventRepository::new(&db).load(&f.session).await.unwrap();
    let resumed_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.turn_id.as_deref() == Some(resumed.id.as_str()))
        .collect();
    let first_plan = resumed_rows
        .iter()
        .find(|row| row.event_type == "plan_updated")
        .unwrap_or_else(|| panic!("the resumed turn must re-emit the seeded plan"));
    let step_count = first_plan.payload.matches("\"step\"").count();
    assert!(
        step_count >= 2,
        "the seeded plan must be the interrupted one (two steps), got: {}",
        first_plan.payload
    );
    let first_tool = resumed_rows
        .iter()
        .find(|row| row.event_type == "tool_call_started");
    if let Some(first_tool) = first_tool {
        assert!(
            first_plan.sequence < first_tool.sequence,
            "the plan must be restored before the model runs a tool: plan={} tool={}",
            first_plan.sequence,
            first_tool.sequence
        );
    }

    // The goal is the same one, and the successful continuation settles it.
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(
        goals.len(),
        1,
        "no second goal was opened by the continuation"
    );
    assert_eq!(
        goals[0].id, goal_id,
        "the original goal identity is preserved"
    );
    assert_eq!(
        goals[0].state,
        GoalState::Settled,
        "a completed continuation settles the original goal"
    );

    // The terminal binds that goal; a chat fallback would have written null.
    let terminal = stores
        .events
        .load_last_by_type(&f.session, "task_finished", None)
        .await
        .unwrap()
        .expect("a terminal was written");
    assert!(
        terminal.payload.contains(goal_id.as_str()),
        "the task terminal must name the settled goal: {}",
        terminal.payload
    );
}

/// No resumable task: `继续` is an ordinary message, not a resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuation_without_a_resumable_task_is_an_ordinary_message() {
    assert_fresh_continuation_is_ordinary(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_submit_continuation_without_task_is_an_ordinary_message() {
    assert_fresh_continuation_is_ordinary(true).await;
}

async fn assert_fresh_continuation_is_ordinary(ordinary_submit: bool) {
    let f = fixture(
        vec![sse(vec![
            serde_json::json!({"choices": [{"delta": {"content": "hello"}, "finish_reason": "stop"}]})
                .to_string(),
        ])],
        leveler_agent::CollaborationMode::Chat,
    )
    .await;
    let client: Arc<dyn InteractiveRuntimeClient> = Arc::new(InProcessRuntimeClient::new(
        f.app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ));
    let command = if ordinary_submit {
        ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "继续".to_string(),
            attachments: Vec::new(),
        }
    } else {
        ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续".to_string(),
        }
    };
    client.send(command).await.unwrap();
    settled(&f.app, &f.session, 1).await;

    let db = f.app.open_database().await.unwrap();
    let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        turns[0].kind, "chat",
        "with nothing to resume, `继续` stays an ordinary chat turn"
    );
}

async fn bounded_interrupted_fixture() -> Fixture {
    let f = fixture(interrupted_work(), leveler_agent::CollaborationMode::Goal).await;
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
        .unwrap();
    f
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_task_cannot_be_reopened_by_a_continuation_declaration() {
    // The bounded run consumes the first two scripted rounds. The next message
    // is an ordinary turn; the model reads `继续` as a continuation and states
    // that through the structured contract. The runtime refuses it: the goal
    // is settled, so there is nothing to continue. The refusal is the authority
    // here — the client does not keyword-reject the message.
    let mut responses = interrupted_work();
    responses.push(declare_intent_frame(
        "c-continue",
        "继续",
        serde_json::json!([{"kind": "continue_task"}]),
    ));
    responses.push(sse(vec![
        serde_json::json!({"choices": [{"delta": {
            "content": "该任务已取消，没有可继续的工作。"
        }}]})
        .to_string(),
        finish_frame("stop"),
    ]));
    let f = fixture(responses, leveler_agent::CollaborationMode::Goal).await;
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
        .unwrap();
    let client = InProcessRuntimeClient::new(
        f.app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );
    client
        .send(ClientCommand::CancelTask {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "继续".into(),
            attachments: Vec::new(),
        })
        .await
        .expect("a message after a cancellation is an ordinary message");
    settled(&f.app, &f.session, 2).await;

    // The model really did declare the continuation and really did read the
    // runtime's refusal, rather than never asking.
    let bodies = f.server.request_bodies().await;
    let refusal = bodies.iter().any(|body| {
        body.contains("No running goal exists for this task") && body.contains("declare_intent")
    });
    assert!(
        refusal,
        "the model must be told the runtime refused the continuation: {bodies:#?}"
    );

    let db = f.app.open_database().await.unwrap();
    let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
    assert_eq!(turns.len(), 2, "the cancelled task was not reopened");
    assert_eq!(
        turns[1].kind, "chat",
        "with a settled goal the message stays an ordinary turn"
    );
    let task = EngineStores::from_database(&db)
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = EngineStores::from_database(&db)
        .goals
        .for_task(&task)
        .await
        .unwrap();
    assert_eq!(goals.len(), 1, "no second goal was opened: {goals:?}");
    assert_eq!(
        goals[0].state,
        GoalState::Settled,
        "the cancelled goal stays settled"
    );
    // The cancellation terminal was written when the user cancelled; the later
    // ordinary turn adds its own terminal, so the cancel fact is looked up by
    // its reason rather than by "last".
    let terminals: Vec<_> = EventRepository::new(&db)
        .load(&f.session)
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.event_type == "task_finished")
        .collect();
    assert!(
        terminals
            .iter()
            .any(|row| row.payload.contains("user_cancelled_task")),
        "the cancellation is a durable terminal: {terminals:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuation_with_attachment_keeps_ordinary_content_validation() {
    let f = bounded_interrupted_fixture().await;
    let client = InProcessRuntimeClient::new(
        f.app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );
    // A text-only model must reject this image. Routing it through Resume
    // would silently drop the attachment and start the interrupted work.
    let attachment = leveler_client_protocol::AttachmentRef {
        id: leveler_client_protocol::AttachmentId::new("continuation-image"),
        kind: leveler_client_protocol::AttachmentKind::Image,
        name: "continuation.png".into(),
        mime_type: "image/png".into(),
        size_bytes: 1,
        sha256: "not-imported".into(),
        width: Some(1),
        height: Some(1),
    };
    let error = client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "继续".into(),
            attachments: vec![attachment],
        })
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("图片") || error.to_string().contains("图像"),
        "{error}"
    );
    let db = f.app.open_database().await.unwrap();
    assert_eq!(
        TurnRepository::new(&db)
            .list(&f.session)
            .await
            .unwrap()
            .len(),
        1
    );
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].objective, "finish the inventory work");
    assert_eq!(goals[0].state, GoalState::Running);
}

/// A model round that declares the goal blocked: the runtime terminal, not a
/// completed answer.
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

/// `blocked` is a statement about one work window, not the end of the user's
/// intent. `继续` must retry the SAME goal — original objective intact — not
/// open a second goal whose objective is literally the word `继续`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continue_after_a_blocked_goal_retries_the_same_record() {
    let f = fixture(
        vec![
            blocked_turn(),
            completed_turn("c-goal", "unblocked and finished"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
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
            8,
            None,
        )
        .await
        .expect("a blocked goal is a normal return");

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(goals.len(), 1);
    let goal_id = goals[0].id.clone();
    assert_eq!(
        goals[0].state,
        GoalState::Running,
        "a blocked goal still owes its work; only the window ended"
    );
    drop(db);

    let client: Arc<dyn InteractiveRuntimeClient> = Arc::new(InProcessRuntimeClient::new(
        f.app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ));
    client
        .send(ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续".to_string(),
        })
        .await
        .unwrap();
    settled(&f.app, &f.session, 2).await;
    wait_for_goals(&f.app, &f.session, |goals| {
        goals
            .iter()
            .any(|goal| goal.id == goal_id && goal.state == GoalState::Settled)
    })
    .await;

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(
        goals.len(),
        1,
        "`继续` retries the blocked goal; it must not open a second record: {goals:?}"
    );
    assert_eq!(goals[0].id, goal_id, "the goal identity is preserved");
    assert_eq!(
        goals[0].objective, "finish the inventory work",
        "the original objective is never replaced by the continuation word"
    );
    assert_eq!(goals[0].state, GoalState::Settled);

    let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(
        turns[1].kind, "user",
        "the retry enters the goal resume path, not a fresh chat turn"
    );
}

/// An explicit `/goal <new task>` after a blocked goal is a NEW intent and
/// must stay independent: it opens its own record and leaves the old one
/// exactly as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_explicit_new_goal_after_blocked_stays_independent() {
    let f = fixture(
        vec![
            blocked_turn(),
            completed_turn("c-new", "the new goal is done"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
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
            8,
            None,
        )
        .await
        .expect("a blocked goal is a normal return");

    let client: Arc<dyn InteractiveRuntimeClient> = Arc::new(InProcessRuntimeClient::new(
        f.app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ));
    client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "/goal 迁移到新的构建系统".to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    settled(&f.app, &f.session, 2).await;
    wait_for_goals(&f.app, &f.session, |goals| {
        goals
            .iter()
            .any(|goal| goal.objective == "迁移到新的构建系统" && goal.state == GoalState::Settled)
    })
    .await;

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(
        goals.len(),
        2,
        "an explicit new goal is its own record: {goals:?}"
    );
    let old = goals
        .iter()
        .find(|goal| goal.objective == "finish the inventory work")
        .expect("the blocked goal is kept");
    assert_eq!(
        old.state,
        GoalState::Running,
        "an explicit new task does not settle work the user never delivered"
    );
    let new = goals
        .iter()
        .find(|goal| goal.objective == "迁移到新的构建系统")
        .expect("the new objective is recorded verbatim");
    assert_eq!(new.state, GoalState::Settled);
}

/// A goal's verification facts belong to the GOAL, not to one work window.
///
/// Window 1 writes a file and runs the acceptance command, then stops at its
/// round budget. Window 2 continues the same goal and cites that command. The
/// gate must accept it: the command really ran in this task, after the last
/// change. A window-local record would answer "never ran in this task" and
/// force a pointless re-run — or, worse, let a model that re-runs nothing end
/// `blocked` on work that already passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn verification_facts_survive_the_next_work_window() {
    let (program, args, script_name, script) = if cfg!(windows) {
        (
            "cmd",
            vec!["/c", "verify.cmd"],
            "verify.cmd",
            "exit /b 0\r\n",
        )
    } else {
        ("sh", vec!["verify.sh"], "verify.sh", "exit 0\n")
    };
    let cited = format!("{program} {}", args.join(" "));
    let f = fixture(
        vec![
            sse(vec![
                tool_call_frame(
                    "p1",
                    "apply_patch",
                    serde_json::json!({
                        "patch": "*** Begin Patch\n*** Add File: src/feature.rs\n+pub fn feature() {}\n*** End Patch"
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                tool_call_frame(
                    "v1",
                    "run_command",
                    serde_json::json!({
                        "program": program,
                        "args": args,
                        "reason": "run the acceptance check"
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                tool_call_frame(
                    "g1",
                    "update_goal",
                    serde_json::json!({
                        "status": "complete",
                        "summary": "feature added and verified",
                        "verification": {"status": "passed", "detail": cited}
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    std::fs::write(f._tmp.path().join(script_name), script).unwrap();

    let model = ModelRef::new("mock", "m");
    let run = |rounds: u32| {
        let app = f.app.clone();
        let session = f.session.clone();
        let model = model.clone();
        async move {
            app.run_in_session_bounded(
                &session,
                &model,
                PermissionProfile::Assisted,
                "finish the inventory work",
                Arc::new(AutoApprove),
                false,
                &mut |_| {},
                CancellationToken::new(),
                rounds,
                None,
            )
            .await
        }
    };

    let first = run(2).await.expect("the first window stops at its budget");
    assert!(
        matches!(
            first.stop_reason,
            leveler_agent::StopReason::TurnLimitReached
                | leveler_agent::StopReason::BudgetExhausted
        ),
        "the first window must really be a budget stop: {first:?}"
    );
    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let owed = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].state, GoalState::Running);
    let goal_id = owed[0].id.clone();
    drop(db);

    let second = run(8).await.expect("the second window completes the goal");
    assert_eq!(
        second.stop_reason,
        leveler_agent::StopReason::Completed,
        "the cited verification from the previous window must be accepted: {second:?}"
    );
    assert!(
        !second
            .completion_warnings
            .iter()
            .any(|warning| warning.contains("without executable verification")),
        "a real, fresh-enough pass must not produce an unverified warning: {:?}",
        second.completion_warnings
    );

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let goal = stores.goals.get(&goal_id).await.unwrap().unwrap();
    assert_eq!(goal.state, GoalState::Settled);
    let rows = EventRepository::new(&db).load(&f.session).await.unwrap();
    assert!(
        rows.iter().all(|row| {
            row.event_type != "goal_intercepted" || !row.payload.contains("verification_refused")
        }),
        "the gate must not refuse a command the task actually ran"
    );
}

/// Session `1c3bf9aa`'s shape, at resume level: the user demanded the tests,
/// window 1 ran the repository's own suite (`make regression`) and stopped at
/// its round budget, and window 2 cites that run. The rebuilt record must read
/// the suite by the repository's own name, so the goal settles on the evidence
/// the task actually produced instead of demanding a pointless re-run.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repository_named_suite_survives_the_next_work_window() {
    use std::os::unix::fs::PermissionsExt;

    let f = fixture(
        vec![
            sse(vec![
                tool_call_frame(
                    "s1",
                    "run_command",
                    serde_json::json!({
                        "program": "bin/make",
                        "args": ["regression"],
                        "reason": "run the suite the user required"
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                tool_call_frame(
                    "g1",
                    "update_goal",
                    serde_json::json!({
                        "status": "complete",
                        "summary": "本地全套已启动，13 组真实回归全过",
                        "verification": {"status": "passed", "detail": "bin/make regression"},
                        "requirements": [{
                            "requirement": "tests",
                            "status": "done",
                            "evidence": "bin/make regression 退出码 0：13 组用例、191 条断言通过"
                        }]
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let bin = f._tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let suite = bin.join("make");
    std::fs::write(&suite, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&suite, std::fs::Permissions::from_mode(0o755)).unwrap();

    let model = ModelRef::new("mock", "m");
    let run = |rounds: u32| {
        let app = f.app.clone();
        let session = f.session.clone();
        let model = model.clone();
        async move {
            app.run_in_session_bounded(
                &session,
                &model,
                PermissionProfile::Assisted,
                "跑通全部测试并确认通过",
                Arc::new(AutoApprove),
                false,
                &mut |_| {},
                CancellationToken::new(),
                rounds,
                None,
            )
            .await
        }
    };

    let first = run(1).await.expect("the first window stops at its budget");
    assert!(
        matches!(
            first.stop_reason,
            leveler_agent::StopReason::TurnLimitReached
                | leveler_agent::StopReason::BudgetExhausted
        ),
        "the first window must really be a budget stop: {first:?}"
    );
    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let owed = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].state, GoalState::Running);
    let goal_id = owed[0].id.clone();
    drop(db);

    let second = run(8).await.expect("the second window completes the goal");
    assert_eq!(
        second.stop_reason,
        leveler_agent::StopReason::Completed,
        "the suite the previous window ran is evidence for this goal: {second:?}"
    );

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let goal = stores.goals.get(&goal_id).await.unwrap().unwrap();
    assert_eq!(goal.state, GoalState::Settled);
    let rows = EventRepository::new(&db).load(&f.session).await.unwrap();
    assert!(
        rows.iter().all(|row| row.event_type != "goal_intercepted"),
        "no gate may refuse a suite the task really ran: {rows:#?}"
    );
}

/// The companion of `a_started_command_is_not_rebuilt_as_a_pass`: a command
/// that was moved to the background, then really EXITED 0, is evidence for the
/// next window. Before the runtime recorded background results, this case was
/// indistinguishable from a start and the goal was refused for a suite that
/// had actually passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_background_command_is_evidence_after_resume() {
    let (script_name, script) = if cfg!(windows) {
        ("run-tests.cmd", "exit /b 0\r\n")
    } else {
        ("run-tests.sh", "exit 0\n")
    };
    let (program, args) = if cfg!(windows) {
        ("cmd", vec!["/c", script_name])
    } else {
        ("sh", vec![script_name])
    };
    let cited = format!("{program} {}", args.join(" "));
    let f = fixture(
        vec![
            sse(vec![
                tool_call_frame(
                    "b1",
                    "run_command",
                    serde_json::json!({
                        "program": program,
                        "args": args,
                        "background": true,
                        "reason": "run the suite the user required"
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                tool_call_frame(
                    "g1",
                    "update_goal",
                    serde_json::json!({
                        "status": "complete",
                        "summary": "the suite passed",
                        "verification": {"status": "passed", "detail": cited},
                        "requirements": [{
                            "requirement": "tests",
                            "status": "done",
                            "evidence": cited
                        }]
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    std::fs::write(f._tmp.path().join(script_name), script).unwrap();

    let model = ModelRef::new("mock", "m");
    let run = |rounds: u32| {
        let app = f.app.clone();
        let session = f.session.clone();
        let model = model.clone();
        async move {
            app.run_in_session_bounded(
                &session,
                &model,
                PermissionProfile::Assisted,
                "跑通全部测试并确认通过",
                Arc::new(AutoApprove),
                false,
                &mut |_| {},
                CancellationToken::new(),
                rounds,
                None,
            )
            .await
        }
    };

    let first = run(1).await.expect("the first window stops at its budget");
    assert!(
        matches!(
            first.stop_reason,
            leveler_agent::StopReason::TurnLimitReached
                | leveler_agent::StopReason::BudgetExhausted
        ),
        "the first window must really be a budget stop: {first:?}"
    );
    // The task really finished during (or just after) the first window, so its
    // result is durable before the next window reads the record.
    let db = f.app.open_database().await.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut rows = Vec::new();
    while std::time::Instant::now() < deadline {
        rows = EventRepository::new(&db).load(&f.session).await.unwrap();
        if rows
            .iter()
            .any(|row| row.event_type == "background_task_finished")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    drop(db);
    assert!(
        rows.iter()
            .any(|row| row.event_type == "background_task_finished"),
        "the exited background command must leave a durable result: {rows:#?}"
    );

    let second = run(8).await.expect("the second window settles");
    assert_eq!(
        second.stop_reason,
        leveler_agent::StopReason::Completed,
        "a backgrounded command that exited 0 is evidence: {second:?}"
    );
}

/// A recorded START is not a recorded RESULT. Window 1 starts a suite in the
/// background and stops at its round budget while the suite is STILL RUNNING;
/// the durable log then holds that start's successful tool result (no exit
/// code) and no result. Window 2 cites it. The rebuilt record must not turn
/// the start into a pass: a resumed goal may not close on a suite whose
/// outcome never reached the record.
///
/// A suite that has really EXITED is a different fact — see
/// `a_finished_background_command_is_evidence_after_resume`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_started_command_is_not_rebuilt_as_a_pass() {
    let (script_name, script) = if cfg!(windows) {
        ("run-tests.cmd", "ping -n 60 127.0.0.1 >NUL\r\n")
    } else {
        ("run-tests.sh", "sleep 60\nexit 0\n")
    };
    let (program, args) = if cfg!(windows) {
        ("cmd", vec!["/c", script_name])
    } else {
        ("sh", vec![script_name])
    };
    let cited = format!("{program} {}", args.join(" "));
    let f = fixture(
        vec![
            sse(vec![
                tool_call_frame(
                    "b1",
                    "run_command",
                    serde_json::json!({
                        "program": program,
                        "args": args,
                        "background": true,
                        "reason": "run the suite the user required"
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                tool_call_frame(
                    "g1",
                    "update_goal",
                    serde_json::json!({
                        "status": "complete",
                        "summary": "the suite passed",
                        "verification": {"status": "passed", "detail": cited},
                        "requirements": [{
                            "requirement": "tests",
                            "status": "done",
                            "evidence": cited
                        }]
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    std::fs::write(f._tmp.path().join(script_name), script).unwrap();

    let model = ModelRef::new("mock", "m");
    let run = |rounds: u32| {
        let app = f.app.clone();
        let session = f.session.clone();
        let model = model.clone();
        async move {
            app.run_in_session_bounded(
                &session,
                &model,
                PermissionProfile::Assisted,
                "跑通全部测试并确认通过",
                Arc::new(AutoApprove),
                false,
                &mut |_| {},
                CancellationToken::new(),
                rounds,
                None,
            )
            .await
        }
    };

    let first = run(1).await.expect("the first window stops at its budget");
    assert!(
        matches!(
            first.stop_reason,
            leveler_agent::StopReason::TurnLimitReached
                | leveler_agent::StopReason::BudgetExhausted
        ),
        "the first window must really be a budget stop: {first:?}"
    );
    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let owed = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].state, GoalState::Running);
    drop(db);

    let second = run(8).await.expect("the second window settles");
    assert_ne!(
        second.stop_reason,
        leveler_agent::StopReason::Completed,
        "a background start has no result to close the goal on: {second:?}"
    );

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let terminal = stores
        .events
        .load_last_by_type(&f.session, "task_finished", None)
        .await
        .unwrap()
        .expect("the second window wrote a terminal");
    assert!(
        !terminal.payload.contains("\"outcome\":\"completed\""),
        "the start must not become a completed goal: {}",
        terminal.payload
    );
    let rows = EventRepository::new(&db).load(&f.session).await.unwrap();
    assert!(
        rows.iter().any(|row| {
            row.event_type == "goal_intercepted" && row.payload.contains("verification_refused")
        }),
        "the citation must be refused on the record: {rows:#?}"
    );
}

/// A failed acceptance check is a fact about the GOAL, not about one window.
/// The retry may not forget it: a premature `complete` is refused until the
/// check actually passes, and the refusal is the same mechanical answer a live
/// window would give.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_verification_survives_into_the_retry() {
    let (program, args, script_name) = if cfg!(windows) {
        ("cmd", vec!["/c", "verify.cmd"], "verify.cmd")
    } else {
        ("sh", vec!["verify.sh"], "verify.sh")
    };
    let cited = format!("{program} {}", args.join(" "));
    let f = fixture(
        vec![
            sse(vec![
                tool_call_frame(
                    "p1",
                    "apply_patch",
                    serde_json::json!({
                        "patch": "*** Begin Patch\n*** Add File: src/feature.rs\n+pub fn feature() {}\n*** End Patch"
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                tool_call_frame(
                    "v1",
                    "run_command",
                    serde_json::json!({
                        "program": program,
                        "args": args,
                        "reason": "run the acceptance check"
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                tool_call_frame(
                    "g-block",
                    "update_goal",
                    serde_json::json!({"status": "blocked", "summary": "acceptance fails"}),
                ),
                finish_frame("tool_calls"),
            ]),
            // Retry: first tries to close without evidence — the remembered
            // failure must refuse it.
            sse(vec![
                tool_call_frame(
                    "g-premature",
                    "update_goal",
                    serde_json::json!({
                        "status": "complete",
                        "summary": "treat it as done",
                        "verification": {"status": "not_applicable", "detail": "looks fine"}
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
            // Then re-runs the now-passing check and closes with it cited.
            sse(vec![
                tool_call_frame(
                    "v2",
                    "run_command",
                    serde_json::json!({
                        "program": program,
                        "args": args,
                        "reason": "re-run the acceptance check"
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                tool_call_frame(
                    "g-done",
                    "update_goal",
                    serde_json::json!({
                        "status": "complete",
                        "summary": "acceptance passes",
                        "verification": {"status": "passed", "detail": cited}
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let fail_script = if cfg!(windows) {
        "exit /b 1\r\n"
    } else {
        "exit 1\n"
    };
    std::fs::write(f._tmp.path().join(script_name), fail_script).unwrap();

    let model = ModelRef::new("mock", "m");
    let first = f
        .app
        .run_in_session_bounded(
            &f.session,
            &model,
            PermissionProfile::Assisted,
            "finish the inventory work",
            Arc::new(AutoApprove),
            false,
            &mut |_| {},
            CancellationToken::new(),
            8,
            None,
        )
        .await
        .expect("a blocked goal is a normal return");
    assert_eq!(first.stop_reason, leveler_agent::StopReason::Blocked);

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(goals.len(), 1);
    let goal_id = goals[0].id.clone();
    assert_eq!(
        goals[0].state,
        GoalState::Running,
        "a blocked goal still owes its work; only the window ended"
    );
    drop(db);

    // The fix lands between windows; the retry must still remember the fail.
    let pass_script = if cfg!(windows) {
        "exit /b 0\r\n"
    } else {
        "exit 0\n"
    };
    std::fs::write(f._tmp.path().join(script_name), pass_script).unwrap();

    let client: Arc<dyn InteractiveRuntimeClient> = Arc::new(InProcessRuntimeClient::new(
        f.app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    client
        .send(ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续".to_string(),
        })
        .await
        .unwrap();
    settled(&f.app, &f.session, 2).await;
    wait_for_goals(&f.app, &f.session, |goals| {
        goals
            .iter()
            .any(|goal| goal.id == goal_id && goal.state == GoalState::Settled)
    })
    .await;

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(goals.len(), 1, "the retry is the same goal: {goals:?}");
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(goals[0].state, GoalState::Settled);
    let terminal = stores
        .events
        .load_last_by_type(&f.session, "task_finished", None)
        .await
        .unwrap()
        .expect("the retry wrote a terminal");
    assert!(
        terminal.payload.contains("\"stop\":\"completed\"")
            || terminal.payload.contains("completed"),
        "the retry ends completed after the check passes: {}",
        terminal.payload
    );
    let rows = EventRepository::new(&db).load(&f.session).await.unwrap();
    assert!(
        rows.iter().any(|row| {
            row.event_type == "goal_intercepted" && row.payload.contains("verification_refused")
        }),
        "the remembered failure must refuse the evidence-free completion"
    );
}

/// A paused goal whose session is still Chat (the model committed the task from
/// a Chat turn) must be re-entered by the user's own continuation instruction —
/// not only by the literal word `继续`. The runtime routes it through the same
/// resume path, so the goal identity, acceptance and evidence are reused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_continuation_authorization_reenters_the_paused_goal_from_chat() {
    let staged = "完成两个阶段：创建 stage-1.txt 和 stage-2.txt；\
                  阶段 1 完成后停下，等待我确认再执行阶段 2。";
    let f = fixture(
        vec![
            // Turn 1: commit the full two-stage task, deliver stage 1, stop.
            sse(vec![
                tool_call_frame(
                    "c-commit",
                    "commit_task",
                    serde_json::json!({
                        "objective": staged,
                        "acceptance": ["stage-1.txt 存在", "stage-2.txt 存在"]
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                tool_call_frame(
                    "c-plan-1",
                    "update_plan",
                    serde_json::json!({"plan": [
                        {"id": "stage-1", "step": "阶段 1：创建 stage-1.txt", "status": "in_progress"},
                        {"id": "stage-2", "step": "阶段 2：创建 stage-2.txt", "status": "pending"}
                    ]}),
                ),
                finish_frame("tool_calls"),
            ]),
            declare_intent_frame(
                "c-bound",
                "阶段 1 完成后停下，等待我确认再执行阶段 2",
                serde_json::json!([{
                    "kind": "set_pause_boundary",
                    "boundary": {"kind": "after_step", "step": {"step_id": "stage-1"}}
                }]),
            ),
            sse(vec![
                tool_call_frame(
                    "c-write-1",
                    "write_file",
                    serde_json::json!({"path": "stage-1.txt", "content": "stage 1\n"}),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                tool_call_frame(
                    "c-plan-2",
                    "update_plan",
                    serde_json::json!({"plan": [
                        {"id": "stage-1", "step": "阶段 1：创建 stage-1.txt", "status": "completed"},
                        {"id": "stage-2", "step": "阶段 2：创建 stage-2.txt", "status": "pending"}
                    ]}),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                serde_json::json!({"choices": [{"delta": {
                    "content": "阶段 1 已完成并验证；阶段 2 等待你的确认。"
                }}]})
                .to_string(),
                finish_frame("stop"),
            ]),
            // Turn 2: the user authorizes the rest. The model reads it as a
            // continuation of the paused goal and says so before acting.
            declare_intent_frame(
                "c-continue",
                "确认，继续阶段 2",
                serde_json::json!([{"kind": "continue_task"}]),
            ),
            sse(vec![
                tool_call_frame(
                    "c-write-2",
                    "write_file",
                    serde_json::json!({"path": "stage-2.txt", "content": "stage 2\n"}),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                tool_call_frame(
                    "c-plan-3",
                    "update_plan",
                    serde_json::json!({"plan": [
                        {"id": "stage-1", "step": "阶段 1：创建 stage-1.txt", "status": "completed"},
                        {"id": "stage-2", "step": "阶段 2：创建 stage-2.txt", "status": "completed"}
                    ]}),
                ),
                finish_frame("tool_calls"),
            ]),
            sse(vec![
                tool_call_frame(
                    "c-goal",
                    "update_goal",
                    serde_json::json!({
                        "status": "complete",
                        "summary": "两个阶段都已交付",
                        "verification": {"status": "not_applicable", "detail": "file deliverables"},
                        "requirements": [
                            {"requirement": "stage-1.txt", "status": "done",
                             "evidence": "created stage-1.txt"},
                            {"requirement": "stage-2.txt", "status": "done",
                             "evidence": "created stage-2.txt"}
                        ]
                    }),
                ),
                finish_frame("tool_calls"),
            ]),
        ],
        leveler_agent::CollaborationMode::Chat,
    )
    .await;
    let runtime = Arc::new(InProcessRuntimeClient::new(
        f.app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ));
    let client: Arc<dyn InteractiveRuntimeClient> = runtime.clone();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: staged.to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    settled_with_client(&f.app, runtime.as_ref(), &f.session, 1).await;
    wait_for_goals(&f.app, &f.session, |goals| {
        goals.len() == 1 && goals[0].user_paused_at.is_some()
    })
    .await;
    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    let goal_id = goals[0].id.clone();
    assert_eq!(goals[0].state, GoalState::Running);
    assert!(!f._tmp.path().join("stage-2.txt").exists());
    drop(db);

    client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "确认，继续阶段 2，把剩余任务完成并验收。".to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    settled_with_client(&f.app, runtime.as_ref(), &f.session, 2).await;
    wait_for_goals(&f.app, &f.session, |goals| {
        goals
            .iter()
            .any(|goal| goal.id == goal_id && goal.state == GoalState::Settled)
    })
    .await;

    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(&f.session)
        .await
        .unwrap()
        .unwrap();
    let goals = stores.goals.for_task(&task).await.unwrap();
    assert_eq!(
        goals.len(),
        1,
        "one goal, re-entered by identity: {goals:?}"
    );
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(
        goals[0].objective, staged,
        "the objective is never replaced"
    );
    assert_eq!(
        goals[0].acceptance,
        vec![
            "stage-1.txt 存在".to_string(),
            "stage-2.txt 存在".to_string()
        ],
        "the acceptance plan is reused, not re-declared"
    );
    assert!(goals[0].user_paused_at.is_none());
    assert!(
        goals[0]
            .amendments
            .iter()
            .any(|amendment| amendment.contains("继续阶段 2")),
        "the authorization is durable with its goal: {goals:?}"
    );
    let turns = TurnRepository::new(&db).list(&f.session).await.unwrap();
    assert_eq!(turns.len(), 2);
    // The client started an ordinary message; that is what the row records.
    // The goal is re-entered by the model's structured declaration inside the
    // turn (`declare_intent continue_task`), not by a client-side route that
    // would have to read the user's wording.
    assert_eq!(
        turns[1].kind, "chat",
        "an ordinary message starts an ordinary turn; the goal is adopted by declaration"
    );
    assert!(f._tmp.path().join("stage-2.txt").exists());
}
