//! Joint lifecycle: Goal recovery × pending steer.
//!
//! These tests drive the REAL application (durable SQLite journal, model-round
//! retry lifecycle, Goal continuation windows) against a scripted provider.
//! They assert durable facts — turn lineage, journal states, the exact request
//! bodies the provider received — not what a screen happened to render.
//!
//! The matrix follows the unified closure plan:
//! - J03/J04 a steer admitted in a Goal recovery window is delivered there,
//!   in the same Goal, without opening a second recovery window;
//! - J05/J13 an edit owns the row: the original text never reaches a request;
//! - J06 a withdrawn steer never comes back;
//! - J07/J10 cancelling the logical task during recovery stops it for good;
//! - J08 an immediate send during retry backoff replaces the wait;
//! - J14 a completion that predates admitted input is withdrawn and the input
//!   is delivered before the Goal settles;
//! - J18 an exhausted recovery budget ends the task and preserves the real
//!   pending input instead of silently delivering it elsewhere.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, MessageId, RuntimeEvent, SteerAction,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{
    EngineStores, GoalRecord, GoalState, SessionInputRepository, SessionInputState, TurnRepository,
};
use leveler_test_support::{MockResponse, MockServer};

const OBJECTIVE: &str = "finish the inventory work";

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

fn tool_call(name: &str, arguments: serde_json::Value) -> MockResponse {
    tool_call_with_id("call-tool", name, arguments)
}

fn tool_call_with_id(id: &str, name: &str, arguments: serde_json::Value) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": id,
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]})
        .to_string(),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string(),
    ])
}

fn text(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

fn gateway_failure() -> MockResponse {
    MockResponse::Status {
        code: 504,
        body: serde_json::json!({"error": {"message": "gateway timeout"}}).to_string(),
    }
}

fn update_goal_complete(summary: &str) -> MockResponse {
    tool_call(
        "update_goal",
        serde_json::json!({"status": "complete", "summary": summary}),
    )
}

fn gated_tool_call(
    name: &str,
    arguments: serde_json::Value,
) -> (MockResponse, Arc<tokio::sync::Notify>) {
    let release = Arc::new(tokio::sync::Notify::new());
    (
        MockResponse::GatedSse {
            first: format!(
                "data: {}\n\n",
                serde_json::json!({"choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "call-gated",
                    "function": {"name": name, "arguments": arguments.to_string()}
                }]}}]})
            ),
            tail: format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
            ),
            release: release.clone(),
        },
        release,
    )
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
    /// ONE client for the test: the in-process client owns its own per-session
    /// event streams, so a second client would publish into a stream this test
    /// never observes.
    client: Arc<InProcessRuntimeClient>,
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
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::FullAccess,
        false,
    ));
    Fixture {
        app,
        session,
        server,
        client,
        _tmp: tmp,
    }
}

async fn submit(f: &Fixture, content: &str) {
    f.client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: content.to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
}

async fn steer(f: &Fixture, id: &str, content: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let outcome = f
            .client
            .send(ClientCommand::SteerCurrentTurn {
                session_id: f.session.clone(),
                content: content.to_string(),
                message_id: Some(MessageId::new(id)),
            })
            .await;
        if outcome.is_ok() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the steer was never admitted: {outcome:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn manage(
    f: &Fixture,
    action: SteerAction,
) -> Result<(), leveler_client_protocol::ClientError> {
    f.client
        .send(ClientCommand::ManageSteer {
            session_id: f.session.clone(),
            action,
        })
        .await
}

async fn inputs(f: &Fixture) -> Vec<leveler_storage::SessionInputRecord> {
    let db = f.app.open_database().await.unwrap();
    SessionInputRepository::new(&db)
        .all(&f.session)
        .await
        .unwrap()
}

fn input<'a>(
    rows: &'a [leveler_storage::SessionInputRecord],
    id: &str,
) -> &'a leveler_storage::SessionInputRecord {
    rows.iter()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("input {id} missing: {rows:?}"))
}

async fn goals(f: &Fixture) -> Vec<GoalRecord> {
    let db = f.app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let Some(task) = stores.tasks.task_for_session(&f.session).await.unwrap() else {
        return Vec::new();
    };
    stores.goals.for_task(&task).await.unwrap()
}

/// The goal settlement is committed in the terminal transaction, which lands
/// after the turn row stops running. Poll the durable owner instead of
/// sampling once.
async fn wait_for_goal_state(f: &Fixture, want: GoalState) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let goals = goals(f).await;
        if goals.iter().any(|goal| goal.state == want) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no goal reached {want:?}: {goals:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn turns(f: &Fixture) -> Vec<leveler_storage::TurnRecord> {
    let db = f.app.open_database().await.unwrap();
    TurnRepository::new(&db).list(&f.session).await.unwrap()
}

/// Wait until `min_turns` turns exist and the runtime has published a terminal.
async fn settled(f: &Fixture, min_turns: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    loop {
        let turns = turns(f).await;
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

async fn wait_for<T>(
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    mut done: impl FnMut(&RuntimeEvent) -> Option<T>,
) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(event)) => {
                if let Some(value) = done(&event) {
                    return value;
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            other => panic!("event stream ended while waiting: {other:?}"),
        }
    }
}

async fn wait_for_request(f: &Fixture, count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while f.server.request_count() < count {
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {} provider request(s) arrived",
            f.server.request_count()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A fresh runtime over the same durable layout: new process state, same DB.
async fn restart(f: Fixture) -> Fixture {
    let Fixture {
        server,
        session,
        _tmp,
        ..
    } = f;
    let layout = Layout::from_parts(
        _tmp.path().to_path_buf(),
        _tmp.path().join("configs"),
        _tmp.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Goal),
    );
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::FullAccess,
        false,
    ));
    Fixture {
        app,
        session,
        server,
        client,
        _tmp,
    }
}

/// J03/J04: a steer admitted while the runtime is continuing an owed goal is
/// delivered in that continuation window — the same Goal, no second recovery,
/// no fabricated user message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_admitted_during_goal_recovery_is_delivered_in_the_continuation() {
    let (gated, release) = gated_tool_call("read_file", serde_json::json!({"path": "src/lib.rs"}));
    let f = fixture(
        vec![
            gateway_failure(),
            gated,
            update_goal_complete("recovered with the steer"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let mut rx = f.client.subscribe_session(&f.session);

    submit(&f, OBJECTIVE).await;
    // The first window failed transiently; the runtime opens a continuation.
    wait_for(&mut rx, |event| {
        matches!(
            event,
            RuntimeEvent::GoalRecovery {
                attempt: 1,
                max_attempts: 3
            }
        )
        .then_some(())
    })
    .await;
    // The recovery window's first model request is in flight.
    wait_for_request(&f, 2).await;
    steer(&f, "steer-recovery", "STEER_DURING_RECOVERY").await;
    release.notify_one();
    settled(&f, 2).await;

    let bodies = f.server.request_bodies().await;
    assert!(
        bodies[1].contains("Runtime recovery notice"),
        "the continuation window is the runtime's, not a new task"
    );
    assert!(
        !bodies[1].contains("STEER_DURING_RECOVERY"),
        "the steer was admitted after that request was built"
    );
    assert_eq!(
        bodies[2].matches("STEER_DURING_RECOVERY").count(),
        1,
        "the continuation's next round carries the steer exactly once"
    );

    let turns = turns(&f).await;
    assert_eq!(turns.len(), 2, "one failed window + one recovery window");
    wait_for_goal_state(&f, GoalState::Settled).await;
    let goals = goals(&f).await;
    assert_eq!(goals.len(), 1, "no sibling goal was opened: {goals:?}");
    assert_eq!(
        goals[0].state,
        GoalState::Settled,
        "the continuation carried the same goal to its own completion"
    );
    assert_eq!(goals[0].windows_run, 2, "one failed window + one recovery");

    let rows = inputs(&f).await;
    let row = input(&rows, "steer-recovery");
    assert_eq!(row.state, SessionInputState::Consumed);
    assert_eq!(row.content, "STEER_DURING_RECOVERY");
    assert_eq!(
        row.origin_turn_id.as_ref().map(|id| id.as_str()),
        Some(turns[1].id.as_str()),
        "the steer belongs to the recovery window that consumed it"
    );
}

/// J05/J13: an edit lease owns the row. The original text never reaches a
/// provider request; the saved revision is what the continuation carries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edit_saved_during_goal_recovery_replaces_the_original_steer() {
    let (gated, release) = gated_tool_call("read_file", serde_json::json!({"path": "src/lib.rs"}));
    let f = fixture(
        vec![
            gateway_failure(),
            gated,
            update_goal_complete("recovered with the saved text"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let mut rx = f.client.subscribe_session(&f.session);

    submit(&f, OBJECTIVE).await;
    wait_for(&mut rx, |event| {
        matches!(event, RuntimeEvent::GoalRecovery { attempt: 1, .. }).then_some(())
    })
    .await;
    wait_for_request(&f, 2).await;
    steer(&f, "steer-edit", "STEER_OLD_ALPHA").await;
    let lease = "b606db24-c8d0-486e-a1c7-be3f65b69d66";
    manage(
        &f,
        SteerAction::BeginEdit {
            message_id: MessageId::new("steer-edit"),
            revision: 0,
            lease_id: lease.to_string(),
        },
    )
    .await
    .unwrap();
    manage(
        &f,
        SteerAction::SaveEdit {
            message_id: MessageId::new("steer-edit"),
            revision: 1,
            lease_id: lease.to_string(),
            content: "STEER_SAVED_BETA".to_string(),
        },
    )
    .await
    .unwrap();
    release.notify_one();
    settled(&f, 2).await;

    let bodies = f.server.request_bodies().await;
    assert!(
        bodies.iter().all(|body| !body.contains("STEER_OLD_ALPHA")),
        "the reserved draft must never reach the model: {bodies:#?}"
    );
    assert!(
        bodies
            .iter()
            .any(|body| body.matches("STEER_SAVED_BETA").count() == 1),
        "the saved revision is delivered once: {bodies:#?}"
    );
    let rows = inputs(&f).await;
    let row = input(&rows, "steer-edit");
    assert_eq!(row.state, SessionInputState::Consumed);
    assert_eq!(row.content, "STEER_SAVED_BETA");
    assert_eq!(row.revision, 2);
}

/// J06: a withdrawn instruction is settled as cancelled and never appears in
/// any request, including the recovery window that follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withdrawn_steer_does_not_come_back_in_a_recovery_window() {
    let (gated, release) = gated_tool_call("read_file", serde_json::json!({"path": "src/lib.rs"}));
    let f = fixture(
        vec![
            gateway_failure(),
            gated,
            update_goal_complete("recovered without the steer"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let mut rx = f.client.subscribe_session(&f.session);

    submit(&f, OBJECTIVE).await;
    wait_for(&mut rx, |event| {
        matches!(event, RuntimeEvent::GoalRecovery { attempt: 1, .. }).then_some(())
    })
    .await;
    wait_for_request(&f, 2).await;
    steer(&f, "steer-withdrawn", "STEER_WITHDRAWN").await;
    manage(
        &f,
        SteerAction::Withdraw {
            message_id: MessageId::new("steer-withdrawn"),
            revision: 0,
        },
    )
    .await
    .unwrap();
    release.notify_one();
    settled(&f, 2).await;

    let bodies = f.server.request_bodies().await;
    assert!(
        bodies.iter().all(|body| !body.contains("STEER_WITHDRAWN")),
        "a withdrawn instruction must not resurrect: {bodies:#?}"
    );
    let rows = inputs(&f).await;
    let row = input(&rows, "steer-withdrawn");
    assert_eq!(row.state, SessionInputState::Cancelled);
    assert_eq!(row.reason, "withdrawn");
}

/// J07/J10: the user cancelling the LOGICAL task during recovery wins. The
/// runtime does not open another window, and admitted input is settled with
/// that cancellation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_task_during_goal_recovery_does_not_restart_it() {
    let (gated, _release) = gated_tool_call("read_file", serde_json::json!({"path": "src/lib.rs"}));
    let f = fixture(
        vec![gateway_failure(), gated, text("must not run")],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let mut rx = f.client.subscribe_session(&f.session);

    submit(&f, OBJECTIVE).await;
    wait_for(&mut rx, |event| {
        matches!(event, RuntimeEvent::GoalRecovery { attempt: 1, .. }).then_some(())
    })
    .await;
    wait_for_request(&f, 2).await;
    steer(&f, "steer-cancelled", "STEER_CANCEL_ME").await;
    f.client
        .send(ClientCommand::CancelTask {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    wait_for(&mut rx, |event| {
        matches!(
            event,
            RuntimeEvent::TurnCancelled
                | RuntimeEvent::TaskCancelled
                | RuntimeEvent::TurnFailed { .. }
        )
        .then_some(())
    })
    .await;
    settled(&f, 2).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let goals = goals(&f).await;
        if goals.iter().all(|goal| goal.state != GoalState::Running) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a cancelled task must not stay owed: {goals:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let turns = turns(&f).await;
    assert_eq!(turns.len(), 2, "no new window after the cancel: {turns:?}");
    let rows = inputs(&f).await;
    let row = input(&rows, "steer-cancelled");
    assert_eq!(
        row.state,
        SessionInputState::Cancelled,
        "input admitted to the cancelled task is settled with it"
    );
    let bodies = f.server.request_bodies().await;
    assert!(
        bodies.iter().all(|body| !body.contains("STEER_CANCEL_ME")),
        "the cancelled instruction must not be delivered: {bodies:#?}"
    );
}

/// J14: a completion decision that predates admitted input cannot settle the
/// goal. The input is delivered first, and only then may the goal complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completion_is_withdrawn_when_admitted_input_is_still_pending() {
    let release = Arc::new(tokio::sync::Notify::new());
    let f = fixture(
        vec![
            MockResponse::GatedSse {
                first: format!(
                    "data: {}\n\n",
                    serde_json::json!({"choices": [{"delta": {"tool_calls": [{
                        "index": 0, "id": "call-complete",
                        "function": {
                            "name": "update_goal",
                            "arguments": serde_json::json!({"status": "complete", "summary": "first attempt"}).to_string()
                        }
                    }]}}]})
                ),
                tail: format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
                ),
                release: release.clone(),
            },
            text("consumed the steer before completing"),
            update_goal_complete("settled after the steer"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let mut rx = f.client.subscribe_session(&f.session);

    submit(&f, OBJECTIVE).await;
    wait_for_request(&f, 1).await;
    steer(&f, "steer-before-complete", "STEER_MUST_BE_SEEN").await;
    release.notify_one();
    settled(&f, 1).await;
    wait_for(&mut rx, |event| {
        matches!(
            event,
            RuntimeEvent::TurnCompleted | RuntimeEvent::TurnAnswered
        )
        .then_some(())
    })
    .await;

    let bodies = f.server.request_bodies().await;
    assert!(
        bodies.len() >= 2,
        "the completion was withdrawn and another round ran: {bodies:#?}"
    );
    assert!(
        bodies[1].contains("STEER_MUST_BE_SEEN"),
        "the steer is in the model's context before the goal settles: {bodies:#?}"
    );
    let rows = inputs(&f).await;
    assert_eq!(
        input(&rows, "steer-before-complete").state,
        SessionInputState::Consumed
    );
    let goals = goals(&f).await;
    assert_eq!(goals.len(), 1, "{goals:?}");
}

/// J18: after the bounded recovery budget is exhausted the task ends; real
/// pending input is preserved, not silently delivered to an unrelated task.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exhausted_recovery_budget_preserves_the_pending_steer() {
    let (gated, release) = gated_tool_call("read_file", serde_json::json!({"path": "src/lib.rs"}));
    let f = fixture(
        vec![
            gated,
            gateway_failure(),
            gateway_failure(),
            gateway_failure(),
            gateway_failure(),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let mut rx = f.client.subscribe_session(&f.session);

    submit(&f, OBJECTIVE).await;
    wait_for_request(&f, 1).await;
    steer(&f, "steer-leased", "STEER_UNDER_EDIT").await;
    manage(
        &f,
        SteerAction::BeginEdit {
            message_id: MessageId::new("steer-leased"),
            revision: 0,
            lease_id: "1c56dceb-3f1e-4d1f-bfbe-1f0d68c9a0aa".to_string(),
        },
    )
    .await
    .unwrap();
    release.notify_one();
    // Three recoveries and the initial failure. The lease keeps the steer out
    // of every round, so the budget, not the input, is what ends the run.
    wait_for(&mut rx, |event| {
        matches!(event, RuntimeEvent::TurnFailed { .. }).then_some(())
    })
    .await;
    settled(&f, 4).await;

    let turns = turns(&f).await;
    assert_eq!(
        turns.len(),
        4,
        "one failed window + three recoveries: {turns:?}"
    );
    let bodies = f.server.request_bodies().await;
    assert_eq!(
        bodies.len(),
        5,
        "four failures and one tool round: {bodies:#?}"
    );
    assert!(
        bodies.iter().all(|body| !body.contains("STEER_UNDER_EDIT")),
        "a reserved steer must not be delivered by a recovery round"
    );
    let rows = inputs(&f).await;
    let row = input(&rows, "steer-leased");
    assert_eq!(
        row.state,
        SessionInputState::Pending,
        "the real input survives the exhausted budget"
    );
    assert!(row.editing_at(leveler_core::now()));
    let goals = goals(&f).await;
    assert_eq!(goals.len(), 1, "no unrelated goal was opened: {goals:?}");
    assert_eq!(
        goals[0].state,
        GoalState::Running,
        "an exhausted recovery budget does not settle the user's goal"
    );
}

/// J08: an immediate send while the request lifecycle is waiting out a retry
/// backoff replaces the wait instead of spending it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_immediate_send_during_retry_backoff_replaces_the_wait() {
    let f = fixture(
        vec![
            // A stream that ends without any terminal event: interruptible,
            // safe to retry, and it must announce its backoff first.
            sse(vec![]),
            text("request rebuilt with the steer"),
        ],
        leveler_agent::CollaborationMode::Chat,
    )
    .await;
    let mut rx = f.client.subscribe_session(&f.session);

    submit(&f, "ordinary question").await;
    wait_for(&mut rx, |event| {
        matches!(
            event,
            RuntimeEvent::ModelRetrying {
                attempt: 1,
                max_attempts: 12,
                ..
            }
        )
        .then_some(())
    })
    .await;
    steer(&f, "steer-backoff", "STEER_PRIORITY_NOW").await;
    manage(
        &f,
        SteerAction::Prioritize {
            message_id: MessageId::new("steer-backoff"),
            revision: 0,
        },
    )
    .await
    .unwrap();
    wait_for(&mut rx, |event| {
        matches!(
            event,
            RuntimeEvent::TurnCompleted | RuntimeEvent::TurnAnswered
        )
        .then_some(())
    })
    .await;

    let bodies = f.server.request_bodies().await;
    assert_eq!(
        bodies.len(),
        2,
        "the backoff retry was replaced by the steer round: {bodies:#?}"
    );
    assert!(
        bodies[1].matches("STEER_PRIORITY_NOW").count() == 1,
        "the steered round carries the instruction once: {bodies:#?}"
    );
    let rows = inputs(&f).await;
    assert_eq!(
        input(&rows, "steer-backoff").state,
        SessionInputState::Consumed
    );
}

/// J11: a background process started before a transient failure keeps running
/// across the recovery window. Recovery continues the task from the transcript;
/// it must neither stop the process nor start it again.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovery_window_neither_stops_nor_duplicates_a_background_process() {
    let (program, args) = leveler_test_support::sleep_command(30);
    let start = tool_call(
        "run_command",
        serde_json::json!({
            "program": program,
            "args": args,
            "cwd": ".",
            "background": true,
            "background_lifetime": "goal"
        }),
    );
    let (gated_complete, release) = gated_tool_call(
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "service verified"}),
    );
    let f = fixture(
        vec![
            start,
            gateway_failure(),
            gated_complete,
            text("the service is verified"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let mut rx = f.client.subscribe_session(&f.session);

    submit(&f, OBJECTIVE).await;
    // The tool round, then the failing round of the same window.
    wait_for_request(&f, 2).await;
    wait_for(&mut rx, |event| {
        matches!(event, RuntimeEvent::GoalRecovery { attempt: 1, .. }).then_some(())
    })
    .await;
    wait_for_request(&f, 3).await;

    let snapshots = f.app.background_tasks().try_all_snapshots().await.unwrap();
    let ours: Vec<_> = snapshots
        .iter()
        .filter(|task| task.owner_scope.as_deref() == Some(f.session.as_str()))
        .collect();
    assert_eq!(
        ours.len(),
        1,
        "the process was started exactly once: {snapshots:?}"
    );
    assert!(
        matches!(
            ours[0].status,
            leveler_execution::BackgroundTaskStatus::Running
        ),
        "the recovery window must not stop a running service: {:?}",
        ours[0].status
    );
    assert_eq!(
        ours[0].origin_call_id.as_deref(),
        Some("call-tool"),
        "the process keeps the call that started it"
    );

    release.notify_one();
    settled(&f, 2).await;
    // A settled goal reaps its own goal-lifetime services; this is the one
    // expected stop, not a recovery-side effect.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let snapshots = f.app.background_tasks().try_all_snapshots().await.unwrap();
        let ours: Vec<_> = snapshots
            .iter()
            .filter(|task| task.owner_scope.as_deref() == Some(f.session.as_str()))
            .collect();
        if ours.iter().all(|task| {
            !matches!(
                task.status,
                leveler_execution::BackgroundTaskStatus::Running
                    | leveler_execution::BackgroundTaskStatus::Killing
            )
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the settled goal should reap its service: {snapshots:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// B05: a tool call whose outcome is unknown (dispatched, never finished)
/// gates the task. A pending steer in that state is preserved; nothing is
/// replayed; an immediate send cannot pretend the instruction was delivered;
/// after the user acknowledges the crash window, a real resume delivers it and
/// the side effect is still counted once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_tool_outcome_preserves_the_steer_without_replay() {
    use leveler_core::BootId;
    use leveler_model::{Message, Role};

    let f = fixture(
        vec![text("resumed after the crash was acknowledged")],
        leveler_agent::CollaborationMode::Chat,
    )
    .await;
    // One observable external side effect that a replay would duplicate.
    let effect = f._tmp.path().join("side-effect.txt");
    std::fs::write(&effect, "effect\n").unwrap();

    // A window that dispatched `apply_patch` and died before its result was
    // durable: the durable record is a start with no finish.
    let turn = {
        let db = f.app.open_database().await.unwrap();
        let mut dead = f.app.task_engine(&db).unwrap();
        dead.boot.id = BootId::generate();
        let token = dead.acquire_ownership(&f.session).await.unwrap();
        let message = Message::text(Role::User, "fix the reports");
        let objective = leveler_lifecycle::ObjectiveAnchor::from_user_message("fix the reports");
        let payload = serde_json::json!({
            "version": 2,
            "initiating_message": message,
            "objective": objective,
            "goal_id": null,
        })
        .to_string();
        let turn = dead
            .stores
            .turns
            .start_owned(
                &token,
                &f.session,
                "user",
                Some(&payload),
                leveler_core::now(),
            )
            .await
            .unwrap();
        let turn_id = leveler_core::TurnId::new(turn.id.clone());
        leveler_engine::EventLog::new(&db, f.session.clone())
            .append(
                Some(&turn_id),
                leveler_engine::EngineEvent::ToolCallStarted {
                    call_id: "call-unknown".into(),
                    name: "apply_patch".into(),
                    arguments: "{}".into(),
                    parallel: false,
                    risk: None,
                    agent_id: None,
                    model_step: Some(1),
                },
                &mut |_| {},
            )
            .await
            .unwrap();
        leveler_storage::MessageRepository::new(&db)
            .append_in_turn(
                &f.session,
                &turn_id,
                &[serde_json::to_string(&Message::text(Role::User, "fix the reports")).unwrap()],
                leveler_core::now(),
            )
            .await
            .unwrap();
        turn_id
    };
    // The steer was admitted while that window was alive.
    {
        let db = f.app.open_database().await.unwrap();
        SessionInputRepository::new(&db)
            .admit(
                "steer-unknown",
                &f.session,
                Some(&turn),
                "STEER_UNKNOWN_STATE",
                leveler_core::now(),
            )
            .await
            .unwrap();
    }

    // Submitting while the unknown call gates the task is refused, and the
    // provider is never called: nothing replays the call.
    let refused = f
        .client
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "继续".into(),
            attachments: Vec::new(),
        })
        .await;
    assert!(
        matches!(
            refused,
            Err(leveler_client_protocol::ClientError::RecoveryRequired(_))
        ),
        "{refused:?}"
    );
    assert_eq!(f.server.request_count(), 0, "no model round ran");

    // Immediate send has nothing safe to accelerate into: the row stays
    // pending, and no delivery is claimed.
    let accelerated = manage(
        &f,
        SteerAction::Prioritize {
            message_id: MessageId::new("steer-unknown"),
            revision: 0,
        },
    )
    .await;
    assert!(accelerated.is_err(), "no live window can take the steer");
    let rows = inputs(&f).await;
    let row = input(&rows, "steer-unknown");
    assert_eq!(row.state, SessionInputState::Pending);
    assert!(!row.priority);
    assert_eq!(
        std::fs::read_to_string(&effect).unwrap().lines().count(),
        1,
        "the unknown call's side effect must not be repeated"
    );

    // The user acknowledges the crash window. The call is closed as unknown,
    // never replayed, and is still named by its original identity.
    f.client
        .send(ClientCommand::AcknowledgeCrashWindow {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    {
        let db = f.app.open_database().await.unwrap();
        let rows = leveler_storage::EventRepository::new(&db)
            .load(&f.session)
            .await
            .unwrap();
        let marker = rows.iter().find_map(|row| {
            let event = leveler_engine::EngineEvent::from_payload(&row.payload).ok()?;
            match event {
                leveler_engine::EngineEvent::ToolCallFinished {
                    call_id,
                    name,
                    is_error,
                    preview,
                    ..
                } if call_id == "call-unknown" => Some((name, is_error, preview)),
                _ => None,
            }
        });
        let (name, is_error, preview) = marker.expect("the acknowledged call is closed");
        assert_eq!(name, "apply_patch", "the original tool identity is kept");
        assert!(is_error);
        assert!(
            leveler_engine::is_user_acknowledged_crash_marker(&preview, is_error),
            "the close marker is a user acknowledgement, not a failure: {preview}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&effect).unwrap().lines().count(),
        1,
        "acknowledgement is not a replay"
    );

    // A real resume continues the same lineage; the pending steer is delivered
    // there, once, and no queued side effect runs again.
    f.server
        .push(text("resumed after the crash was acknowledged"))
        .await;
    f.client
        .send(ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续".into(),
        })
        .await
        .unwrap();
    settled(&f, 2).await;

    let bodies = f.server.request_bodies().await;
    assert!(
        bodies[0].matches("STEER_UNKNOWN_STATE").count() == 1,
        "the steer reaches the first resumed round exactly once"
    );
    assert!(
        bodies[0].contains("outcome unknown") && bodies[0].contains("call-unknown"),
        "the model is told the call may have taken effect and was not replayed"
    );
    let db = f.app.open_database().await.unwrap();
    let starts = leveler_storage::EventRepository::new(&db)
        .load(&f.session)
        .await
        .unwrap()
        .iter()
        .filter(|row| {
            leveler_engine::EngineEvent::from_payload(&row.payload).is_ok_and(|event| {
                matches!(
                    event,
                    leveler_engine::EngineEvent::ToolCallStarted { ref name, .. }
                        if name == "apply_patch"
                )
            })
        })
        .count();
    assert_eq!(
        starts, 1,
        "the unknown call was dispatched once, never replayed"
    );
    assert_eq!(
        std::fs::read_to_string(&effect).unwrap().lines().count(),
        1,
        "the resumed window must not replay the unknown call"
    );
    let rows = inputs(&f).await;
    assert_eq!(
        input(&rows, "steer-unknown").state,
        SessionInputState::Consumed
    );
    let turns = turns(&f).await;
    let resumed = turns.last().unwrap();
    assert!(
        resumed
            .payload
            .as_deref()
            .is_some_and(|payload| payload.contains(turn.as_str())),
        "the resumed window continues the crashed lineage: {resumed:?}"
    );
}

/// G05/U13 (restart + resume): a check that really ran before the process
/// ended is still citable after a fresh runtime resumes the same goal. The
/// evidence authority is the durable event log, not the dead process's memory.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_runtime_resumes_the_goal_and_accepts_its_earlier_check() {
    let scope = tool_call_with_id(
        "call-scope",
        "declare_scope",
        serde_json::json!({
            "operations": [{
                "op": "add",
                "kind": "command",
                "text": "the verifier passes",
                "target": "sh -c exit 0",
                "source_quote": OBJECTIVE
            }]
        }),
    );
    let check = tool_call(
        "run_command",
        serde_json::json!({"program": "sh", "args": ["-c", "exit 0"], "cwd": "."}),
    );
    let f = fixture(
        vec![
            scope,
            check,
            gateway_failure(),
            gateway_failure(),
            gateway_failure(),
            gateway_failure(),
            tool_call(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "earlier check still proves this goal",
                    "verification": {"status": "passed", "detail": "call_id:call-tool"},
                    "requirements": [{
                        "id": "R1",
                        "status": "done",
                        "evidence": "call_id:call-tool"
                    }]
                }),
            ),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let mut rx = f.client.subscribe_session(&f.session);
    submit(&f, OBJECTIVE).await;
    wait_for(&mut rx, |event| {
        matches!(event, RuntimeEvent::TurnFailed { .. }).then_some(())
    })
    .await;
    let goals_before = goals(&f).await;
    assert_eq!(goals_before.len(), 1);
    assert_eq!(
        goals_before[0].state,
        GoalState::Running,
        "an exhausted recovery budget does not settle the user's goal"
    );
    let goal_id = goals_before[0].id.clone();

    // A fresh runtime over the same durable state: the old client, its event
    // streams and its in-memory verification record are gone.
    let f = restart(f).await;
    let mut rx = f.client.subscribe_session(&f.session);
    f.client
        .send(ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续".into(),
        })
        .await
        .unwrap();
    wait_for(&mut rx, |event| {
        matches!(
            event,
            RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnCompletedWithWarnings { .. }
                | RuntimeEvent::TurnCompletedPartiallyUnverified { .. }
                | RuntimeEvent::TurnAnswered
        )
        .then_some(())
    })
    .await;
    wait_for_goal_state(&f, GoalState::Settled).await;
    let goals = goals(&f).await;
    assert_eq!(goals.len(), 1, "the same goal, not a sibling: {goals:?}");
    assert_eq!(goals[0].id, goal_id);
    assert_eq!(goals[0].state, GoalState::Settled);
    let db = f.app.open_database().await.unwrap();
    let rows = leveler_storage::EventRepository::new(&db)
        .load(&f.session)
        .await
        .unwrap();
    let report = rows.iter().rev().find_map(|row| {
        let event = leveler_engine::EngineEvent::from_payload(&row.payload).ok()?;
        match event {
            leveler_engine::EngineEvent::TaskFinished {
                verification: Some(report),
                ..
            } if report.status == leveler_lifecycle::VerificationStatus::Verified => Some(report),
            _ => None,
        }
    });
    let Some(report) = report else {
        let dump: Vec<_> = rows
            .iter()
            .filter_map(|row| {
                let event = leveler_engine::EngineEvent::from_payload(&row.payload).ok()?;
                match event {
                    leveler_engine::EngineEvent::TaskFinished {
                        verification,
                        warnings,
                        outcome,
                        ..
                    } => Some(format!(
                        "{outcome:?} verification={verification:?} warnings={warnings:?}"
                    )),
                    _ => None,
                }
            })
            .collect();
        panic!("no verified terminal; {dump:#?}");
    };
    assert!(
        report
            .records
            .iter()
            .any(|record| record.call_id.as_deref() == Some("call-tool")),
        "the pre-restart execution is the cited evidence: {report:?}"
    );
    let bodies = f.server.request_bodies().await;
    assert!(
        bodies.iter().any(|body| body.contains("call_id:call-tool")),
        "the resumed window really cited the earlier execution"
    );
}

/// U13 (compaction): a check that really ran is still citable after the
/// explicit `/compact` rewrite. The rewrite replaces the transcript; the
/// goal's durable execution record is untouched.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compacted_transcript_still_resolves_the_earlier_check() {
    let scope = tool_call_with_id(
        "call-scope",
        "declare_scope",
        serde_json::json!({
            "operations": [{
                "op": "add",
                "kind": "command",
                "text": "the verifier passes",
                "target": "sh -c exit 0",
                "source_quote": OBJECTIVE
            }]
        }),
    );
    let check = tool_call(
        "run_command",
        serde_json::json!({"program": "sh", "args": ["-c", "exit 0"], "cwd": "."}),
    );
    let f = fixture(
        vec![
            scope,
            check,
            gateway_failure(),
            gateway_failure(),
            gateway_failure(),
            gateway_failure(),
            text("SUMMARY: the verifier passed before the compaction."),
            tool_call(
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "the earlier verifier still proves the goal",
                    "verification": {"status": "passed", "detail": "call_id:call-tool"},
                    "requirements": [{
                        "id": "R1",
                        "status": "done",
                        "evidence": "call_id:call-tool"
                    }]
                }),
            ),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let mut rx = f.client.subscribe_session(&f.session);
    submit(&f, OBJECTIVE).await;
    wait_for(&mut rx, |event| {
        matches!(event, RuntimeEvent::TurnFailed { .. }).then_some(())
    })
    .await;
    assert_eq!(goals(&f).await[0].state, GoalState::Running);

    let before = f.server.request_count();
    f.client
        .send(ClientCommand::CompactContext {
            session_id: f.session.clone(),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while f.server.request_count() == before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the compaction never asked the model"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The rewrite is committed before the command's turn is released.
    tokio::time::sleep(Duration::from_millis(200)).await;

    f.client
        .send(ClientCommand::ResumeTask {
            session_id: f.session.clone(),
            content: "继续".into(),
        })
        .await
        .unwrap();
    wait_for(&mut rx, |event| {
        matches!(
            event,
            RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnCompletedWithWarnings { .. }
                | RuntimeEvent::TurnCompletedPartiallyUnverified { .. }
                | RuntimeEvent::TurnAnswered
        )
        .then_some(())
    })
    .await;
    wait_for_goal_state(&f, GoalState::Settled).await;
    let goals = goals(&f).await;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].state, GoalState::Settled);
    let db = f.app.open_database().await.unwrap();
    let report = leveler_storage::EventRepository::new(&db)
        .load(&f.session)
        .await
        .unwrap()
        .iter()
        .rev()
        .find_map(|row| {
            let event = leveler_engine::EngineEvent::from_payload(&row.payload).ok()?;
            match event {
                leveler_engine::EngineEvent::TaskFinished {
                    verification: Some(report),
                    ..
                } if report.status == leveler_lifecycle::VerificationStatus::Verified => {
                    Some(report)
                }
                _ => None,
            }
        })
        .expect("the post-compaction completion is verified");
    assert!(
        report
            .records
            .iter()
            .any(|record| record.call_id.as_deref() == Some("call-tool")),
        "the check that ran before compaction is the cited evidence: {report:?}"
    );
}
