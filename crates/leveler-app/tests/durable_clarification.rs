//! A user question is durable, not a property of one process's memory.
//!
//! The realtime channel and the decision have separate lifecycles: a client
//! disconnecting (or the process that asked dying) is a gap in delivery, not
//! the user declining. These tests crash the runtime at the two cuts that
//! matter — after the question was asked, and after the answer was committed
//! but before its continuation was admitted — and prove from durable rows that
//! exactly one settlement and exactly one continuation happen.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiClarificationRequest,
    UiPendingInteraction, UiTaskStatus,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{EngineStores, EventRepository, GoalState, TurnRepository};
use leveler_test_support::{MockResponse, MockServer};
use tokio::sync::broadcast;

const OBJECTIVE: &str = "produce the inventory report";
const QUESTION: &str = "需要简洁还是详细的版本？";
const ANSWER: &str = "详细，并在完成后说明改动";

/// Run a crash test on a runtime whose teardown is bounded.
///
/// The asking turn runs as a blocking task on a waiter that only exists in the
/// process the test simulates killing. That thread can never observe the
/// shutdown signal, so a plain runtime drop would wait forever for a process
/// that would not exist. The assertions have already run when the bound is
/// reached; Tokio leaks blocking tasks past it by contract.
fn crash_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    // A failed assertion must still reach the harness: unwinding through a
    // plain drop would park on the same blocked thread before the panic could
    // be reported.
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.block_on(future)));
    runtime.shutdown_timeout(Duration::from_secs(1));
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn tool_turn(id: &str, name: &str, arguments: serde_json::Value) -> MockResponse {
    let call = serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": id,
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]
    })
    .to_string();
    let finish =
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string();
    MockResponse::sse(&[&call, &finish])
}

fn enable_host_interaction() -> MockResponse {
    tool_turn(
        "c-capability",
        "capability",
        serde_json::json!({"action": "enable", "id": "host_interaction"}),
    )
}

fn ask_question() -> MockResponse {
    tool_turn(
        "c-question",
        "request_user_input",
        serde_json::json!({"question": QUESTION, "options": ["简洁", "详细"]}),
    )
}

fn finish_goal(id: &str, summary: &str) -> MockResponse {
    tool_turn(
        id,
        "update_goal",
        serde_json::json!({"status": "complete", "summary": summary}),
    )
}

fn write_config(root: &Path, base_url: &str) {
    isolate_global_config();
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
    root: tempfile::TempDir,
    server: MockServer,
}

async fn fixture(responses: Vec<MockResponse>) -> Fixture {
    isolate_global_config();
    let server = MockServer::start(responses).await;
    let root = tempfile::tempdir().unwrap();
    write_config(root.path(), &server.base_url());
    Fixture { root, server }
}

/// A process incarnation on the fixture's state directory.
fn assemble(root: &Path) -> Arc<Application> {
    let layout = Layout::from_parts(root.to_path_buf(), root.join("configs"), root.join("state"));
    Arc::new(Application::assemble(layout).unwrap())
}

fn client(app: &Arc<Application>) -> Arc<InProcessRuntimeClient> {
    Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ))
}

fn message(session: &leveler_core::SessionId, content: &str) -> ClientCommand {
    ClientCommand::SubmitMessage {
        session_id: session.clone(),
        content: content.to_string(),
        attachments: vec![],
    }
}

async fn clarification_request(
    events: &mut broadcast::Receiver<RuntimeEvent>,
) -> UiClarificationRequest {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::ClarificationRequested { request }) => return request,
                Ok(RuntimeEvent::TurnFailed { error, .. }) => {
                    panic!("the turn failed before asking: {error}")
                }
                Ok(_) => {}
                Err(error) => panic!("the event stream closed: {error}"),
            }
        }
    })
    .await
    .expect("the runtime should ask within 30s")
}

/// The durable request is the authority; wait until its row is committed,
/// because the model-facing event and the event-log write are separate steps.
async fn durable_request(
    app: &Application,
    session: &leveler_core::SessionId,
    id: &leveler_core::ClarificationId,
) -> leveler_engine::PendingClarification {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let db = app.open_database().await.unwrap();
        let stores = EngineStores::from_database(&db);
        let log = leveler_engine::EventLog::new(stores.events.as_ref(), session.clone());
        if let Some(request) = log
            .pending_clarifications()
            .await
            .unwrap()
            .into_iter()
            .find(|request| &request.id == id)
        {
            return request;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the question never became durable"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn goal_id(app: &Application, session: &leveler_core::SessionId) -> leveler_core::GoalId {
    let db = app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let task = stores
        .tasks
        .task_for_session(session)
        .await
        .unwrap()
        .expect("the goal session has a task");
    stores.goals.for_task(&task).await.unwrap()[0].id.clone()
}

async fn goals_all_settled(app: &Application, session: &leveler_core::SessionId) -> bool {
    let Ok(db) = app.open_database().await else {
        return false;
    };
    let stores = EngineStores::from_database(&db);
    let Ok(Some(task)) = stores.tasks.task_for_session(session).await else {
        return false;
    };
    let goals = stores.goals.for_task(&task).await.unwrap();
    !goals.is_empty() && goals.iter().all(|goal| goal.state == GoalState::Settled)
}

async fn wait_goal_settled(app: &Application, session: &leveler_core::SessionId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !goals_all_settled(app, session).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the continued goal did not settle"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_turns_stopped(app: &Application, session: &leveler_core::SessionId, min: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let db = app.open_database().await.unwrap();
        let turns = TurnRepository::new(&db).list(session).await.unwrap();
        if turns.len() >= min && turns.iter().all(|turn| turn.status != "running") {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turns did not stop: {turns:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Run the first window until it asks its question, and return everything a
/// crash would leave behind.
struct Asked {
    app: Arc<Application>,
    session: leveler_core::SessionId,
    client: Arc<InProcessRuntimeClient>,
    request: UiClarificationRequest,
    durable: leveler_engine::PendingClarification,
}

async fn ask_once(f: &Fixture) -> Asked {
    let app = assemble(f.root.path());
    let session = app
        .create_session_with_collaboration(
            &ModelRef::new("mock", "m"),
            OBJECTIVE,
            PermissionProfile::Assisted,
            leveler_agent::CollaborationMode::Goal,
        )
        .await
        .unwrap();
    let client = client(&app);
    let mut events = client.subscribe_session(&session);
    client
        .send(ClientCommand::RunGoal {
            session_id: session.clone(),
            content: OBJECTIVE.to_string(),
            require_new_turn: false,
        })
        .await
        .unwrap();
    let request = clarification_request(&mut events).await;
    let durable = durable_request(&app, &session, &request.id).await;
    Asked {
        app,
        session,
        client,
        request,
        durable,
    }
}

/// Kill this boot the way a crash does — its OS lock is gone and its in-memory
/// waiter table dies with the client — then restart on the same state dir and
/// reap the turn it left running.
async fn crash_and_restart(
    f: &Fixture,
    asked: Asked,
) -> (Arc<Application>, leveler_core::SessionId) {
    let boot = asked.app.boot_id().unwrap();
    drop(asked.client);
    drop(asked.app);
    // The OS releases a dead process's lock; unlinking the file is the same
    // durable evidence, and the boot id is never reused.
    std::fs::remove_file(
        f.root
            .path()
            .join("state/boots")
            .join(format!("{}.lock", boot.as_str())),
    )
    .unwrap();
    let app = assemble(f.root.path());
    let db = app.open_database().await.unwrap();
    let reaped = app
        .reap_zombie_turns(&db, Some(&asked.session))
        .await
        .unwrap();
    assert!(reaped > 0, "the crashed turn must be reaped, not believed");
    (app, asked.session)
}

fn tool_messages(body: &str) -> Vec<(String, String)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    value
        .get("messages")
        .and_then(|messages| messages.as_array())
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message.get("role").and_then(|r| r.as_str()) == Some("tool"))
                .filter_map(|message| {
                    Some((
                        message.get("tool_call_id")?.as_str()?.to_string(),
                        message.get("content")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The whole loop: the process disappears while the question is pending, the
/// answer arrives into a fresh runtime, and the goal continues by itself.
#[test]
fn an_answer_after_a_restart_settles_the_question_and_continues_the_goal() {
    crash_test(async {
        let f = fixture(vec![
            enable_host_interaction(),
            ask_question(),
            finish_goal("c-done", "inventory report delivered"),
        ])
        .await;
        let asked = ask_once(&f).await;
        assert_eq!(asked.request.question, QUESTION);
        assert_eq!(asked.request.options, ["简洁", "详细"]);
        assert!(asked.durable.is_actionable(), "{:?}", asked.durable);
        let request_id = asked.request.id.clone();
        let asked_turn = asked.durable.turn_id.clone().expect("the asking turn");
        let call_id = asked.durable.call_id.clone();
        let original_goal = goal_id(&asked.app, &asked.session).await;

        let before = asked.client.snapshot(&asked.session).await.unwrap();
        assert_eq!(before.task_status, Some(UiTaskStatus::WaitingUser));
        assert!(before.pending_interactions.iter().any(|item| matches!(
            item,
            UiPendingInteraction::Clarification(request) if request.id == request_id
        )));

        let (app, session) = crash_and_restart(&f, asked).await;
        let restarted = client(&app);
        let restored = restarted.snapshot(&session).await.unwrap();
        assert_eq!(
            restored.task_status,
            Some(UiTaskStatus::WaitingUser),
            "a question survives the boot that asked it"
        );
        assert!(
            restored.pending_interactions.iter().any(|item| matches!(
                item,
                UiPendingInteraction::Clarification(request)
                    if request.id == request_id
                        && request.question == QUESTION
                        && request.options == ["简洁", "详细"]
            )),
            "the restarted runtime restores the same card: {:?}",
            restored.pending_interactions
        );
        assert_eq!(
            goal_id(&app, &session).await,
            original_goal,
            "the answer must continue the original goal, not mint one"
        );

        // A new turn cannot slip in while the question is answerable.
        let refused = restarted
            .send(message(&session, "顺便改一下标题"))
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("仍在等待用户回答一个问题"), "{refused}");
        assert_eq!(f.server.request_count(), 2, "no turn may reach the model");

        restarted
            .send(ClientCommand::AnswerClarification {
                request_id: request_id.clone(),
                answer: ANSWER.to_string(),
                session_id: Some(session.clone()),
            })
            .await
            .unwrap();

        // The settlement is one atomic burst on the asking turn: the decision
        // and the tool result that carries it, with no window between them.
        let db = app.open_database().await.unwrap();
        let rows = EventRepository::new(&db).load(&session).await.unwrap();
        let answered: Vec<_> = rows
            .iter()
            .filter(|row| row.event_type == "clarification_answered")
            .collect();
        assert_eq!(answered.len(), 1, "exactly one terminal");
        let leveler_engine::EngineEvent::ClarificationAnswered {
            answer, outcome, ..
        } = leveler_engine::EngineEvent::from_payload(&answered[0].payload).unwrap()
        else {
            panic!("expected an answered event");
        };
        assert_eq!(outcome, "answered");
        assert_eq!(answer, ANSWER);
        assert_eq!(answered[0].turn_id.as_deref(), Some(asked_turn.as_str()));
        let closed = rows
            .iter()
            .find(|row| {
                row.event_type == "tool_call_finished"
                    && leveler_engine::EngineEvent::from_payload(&row.payload).is_ok_and(|event| {
                        matches!(event, leveler_engine::EngineEvent::ToolCallFinished {
                            call_id: closed, preview, ..
                        } if closed == call_id && preview == ANSWER)
                    })
            })
            .expect("the question's own tool call is closed with the answer");
        assert_eq!(closed.sequence, answered[0].sequence + 1);
        assert_eq!(closed.turn_id, answered[0].turn_id);

        wait_goal_settled(&app, &session).await;
        let turns = TurnRepository::new(&db).list(&session).await.unwrap();
        assert_eq!(
            turns.len(),
            2,
            "one crashed turn, one continuation: {turns:?}"
        );
        assert_eq!(turns[0].id, asked_turn);
        assert_eq!(turns[0].status, "interrupted");
        assert_eq!(turns.last().unwrap().status, "completed");

        let bodies = f.server.request_bodies().await;
        assert!(
            bodies.len() >= 3,
            "the continuation asked the model: {} requests",
            bodies.len()
        );
        let tool_results = tool_messages(&bodies[2]);
        assert!(
            tool_results
                .iter()
                .any(|(closed_id, content)| closed_id == &call_id && content == ANSWER),
            "the model must receive the user's answer as that call's result: {tool_results:?}"
        );
    });
}

/// A retry of the same answer is the same decision; a different answer to an
/// already-settled request did not happen and must not read as success.
#[test]
fn a_stale_answer_does_not_resume_the_goal_a_second_time() {
    crash_test(async {
        let f = fixture(vec![
            enable_host_interaction(),
            ask_question(),
            finish_goal("c-done", "inventory report delivered"),
        ])
        .await;
        let asked = ask_once(&f).await;
        let request_id = asked.request.id.clone();
        let (app, session) = crash_and_restart(&f, asked).await;
        let first = client(&app);
        first
            .send(ClientCommand::AnswerClarification {
                request_id: request_id.clone(),
                answer: ANSWER.to_string(),
                session_id: Some(session.clone()),
            })
            .await
            .unwrap();
        wait_goal_settled(&app, &session).await;
        wait_turns_stopped(&app, &session, 2).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let settled_requests = f.server.request_count();
        let db = app.open_database().await.unwrap();
        let settled_turns = TurnRepository::new(&db).list(&session).await.unwrap().len();
        assert_eq!(settled_turns, 2);

        // Another client, holding the card from before the answer, retries the
        // same decision: the transport lost an acknowledgement, not the
        // decision.
        first
            .send(ClientCommand::AnswerClarification {
                request_id: request_id.clone(),
                answer: ANSWER.to_string(),
                session_id: Some(session.clone()),
            })
            .await
            .unwrap();
        assert_eq!(
            f.server.request_count(),
            settled_requests,
            "no second continuation"
        );
        assert_eq!(
            TurnRepository::new(&db).list(&session).await.unwrap().len(),
            settled_turns
        );

        // A different decision for a request that is over must be reported as
        // not applied, and must not reopen or re-run anything.
        let stale = client(&app)
            .send(ClientCommand::AnswerClarification {
                request_id,
                answer: "简洁".to_string(),
                session_id: Some(session.clone()),
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(stale.contains("本次回答未生效"), "{stale}");
        assert_eq!(f.server.request_count(), settled_requests);
        assert_eq!(
            TurnRepository::new(&db).list(&session).await.unwrap().len(),
            settled_turns
        );
        let answered: Vec<_> = EventRepository::new(&db)
            .load(&session)
            .await
            .unwrap()
            .into_iter()
            .filter(|row| row.event_type == "clarification_answered")
            .collect();
        assert_eq!(answered.len(), 1, "the first decision stays authoritative");
    });
}

/// Cancelling the logical task invalidates its question: a late answer is
/// refused, not recorded, and starts no work.
#[test]
fn a_late_answer_after_cancel_starts_no_work() {
    crash_test(async {
        let f = fixture(vec![enable_host_interaction(), ask_question()]).await;
        let asked = ask_once(&f).await;
        let request_id = asked.request.id.clone();
        let (app, session) = crash_and_restart(&f, asked).await;
        let restarted = client(&app);
        restarted
            .send(ClientCommand::CancelTask {
                session_id: session.clone(),
            })
            .await
            .unwrap();

        let db = app.open_database().await.unwrap();
        let stores = EngineStores::from_database(&db);
        let log = leveler_engine::EventLog::new(stores.events.as_ref(), session.clone());
        assert!(
            log.pending_clarifications().await.unwrap().is_empty(),
            "cancel closes the question"
        );
        let terminal = log
            .clarification_settlement(&request_id)
            .await
            .unwrap()
            .expect("the cancellation is durable");
        assert_eq!(terminal.outcome, "cancelled");

        let late = restarted
            .send(ClientCommand::AnswerClarification {
                request_id,
                answer: ANSWER.to_string(),
                session_id: Some(session.clone()),
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(late.contains("已随任务取消而关闭"), "{late}");
        assert!(late.contains("本次回答未生效"), "{late}");
        assert_eq!(f.server.request_count(), 2, "no continuation may run");
        assert_eq!(
            TurnRepository::new(&db).list(&session).await.unwrap().len(),
            1,
            "no new turn after a cancel"
        );
    });
}

/// The other cut: the answer committed, then the process died before the
/// continuation was admitted. The snapshot a client opens is where the runtime
/// finishes that work — once.
#[test]
fn a_committed_answer_without_a_continuation_is_resumed_once() {
    crash_test(async {
        let f = fixture(vec![
            enable_host_interaction(),
            ask_question(),
            finish_goal("c-done", "inventory report delivered"),
        ])
        .await;
        let asked = ask_once(&f).await;
        let request_id = asked.request.id.clone();
        let session = asked.session.clone();
        // The answer lands durably...
        {
            let db = asked.app.open_database().await.unwrap();
            let stores = EngineStores::from_database(&db);
            let log = leveler_engine::EventLog::new(stores.events.as_ref(), session.clone());
            let settlement = log.settle_clarification(&request_id, ANSWER).await.unwrap();
            assert!(matches!(
                settlement,
                leveler_engine::ClarificationSettlement::Settled(_)
            ));
        }
        // ...and the process dies before it admits the next window.
        let (app, session) = crash_and_restart(&f, Asked { session, ..asked }).await;

        // Opening the session is the embedded runtime's recovery point: the
        // snapshot must report the owed window as running and start it.
        let restarted = client(&app);
        let snapshot = restarted.snapshot(&session).await.unwrap();
        assert_eq!(
            snapshot.task_status,
            Some(UiTaskStatus::Running),
            "the owed continuation owns the session now"
        );
        wait_goal_settled(&app, &session).await;
        wait_turns_stopped(&app, &session, 2).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let db = app.open_database().await.unwrap();
        let turns = TurnRepository::new(&db).list(&session).await.unwrap();
        assert_eq!(turns.len(), 2, "exactly one continuation: {turns:?}");
        let requests = f.server.request_count();
        assert!(
            requests >= 3,
            "the continuation asked the model: {requests} requests"
        );

        // Opening it again must not start a third window.
        let again = client(&app);
        let _ = again.snapshot(&session).await.unwrap();
        assert_eq!(
            again
                .reconcile_owed_clarification_continuations()
                .await
                .unwrap(),
            0,
            "a finished continuation is not owed"
        );
        assert_eq!(
            TurnRepository::new(&db).list(&session).await.unwrap().len(),
            2
        );
        assert_eq!(f.server.request_count(), requests);
    });
}
