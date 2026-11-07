//! Accepted input has one identity from admission to transcript.
//!
//! A steer is journaled before its command answers, survives cancellation as a
//! settled row instead of resurfacing in a later turn, and reaches the
//! transcript under the SAME id the client was answered with.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, MessageId, RuntimeEvent, UiInputState, UiMessageKind,
    UiSessionSnapshot,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

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
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": "call-tool",
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

struct Harness {
    _tmp: tempfile::TempDir,
    _server: MockServer,
    app: Arc<Application>,
    model: ModelRef,
    client: Arc<InProcessRuntimeClient>,
    session: leveler_core::SessionId,
}

async fn client_with(
    responses: Vec<MockResponse>,
) -> (
    tempfile::TempDir,
    MockServer,
    Arc<InProcessRuntimeClient>,
    leveler_core::SessionId,
) {
    let harness = harness(responses).await;
    (
        harness._tmp,
        harness._server,
        harness.client,
        harness.session,
    )
}

async fn harness(responses: Vec<MockResponse>) -> Harness {
    harness_in(responses, leveler_agent::CollaborationMode::Chat).await
}

async fn harness_in(
    responses: Vec<MockResponse>,
    collaboration: leveler_agent::CollaborationMode,
) -> Harness {
    isolate_global_config();
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/models")).unwrap();
    std::fs::write(
        tmp.path().join("configs/providers/mock.yaml"),
        format!(
            "id: mock\nprotocol: openai_chat\nbase_url: {}\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("configs/models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(collaboration),
    );
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "goal").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    Harness {
        _tmp: tmp,
        _server: server,
        app,
        model,
        client,
        session,
    }
}

/// Collect the events whose message id matches until the requested
/// settlement, or fail the test with what was observed.
async fn settle_event(
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    seek: &MessageId,
    want: UiInputState,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut seen = Vec::new();
    let mut admitted = false;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::InputSettled {
                message_id, state, ..
            })) if &message_id == seek => {
                assert!(admitted, "settlement must follow admission: {seen:?}");
                assert_eq!(state, want);
                return;
            }
            Ok(Ok(RuntimeEvent::InputAdmitted { message })) => {
                admitted |= &message.id == seek;
                seen.push(("admitted", message.id.as_str().to_string()));
            }
            Ok(Ok(RuntimeEvent::InputSettled {
                message_id, state, ..
            })) => {
                seen.push(("settled", format!("{}={state:?}", message_id.as_str())));
            }
            Ok(Ok(other)) => {
                let kind = serde_json::to_value(&other).unwrap()["type"]
                    .as_str()
                    .unwrap_or("?")
                    .to_string();
                seen.push(("event", kind));
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            other => panic!("no {want:?} settlement for {seek:?}; saw {seen:?}: {other:?}"),
        }
    }
}

async fn snapshot(
    client: &InProcessRuntimeClient,
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    session: &leveler_core::SessionId,
) -> UiSessionSnapshot {
    client
        .send(ClientCommand::OpenSession {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    // The reply shares the stream with turn events, and this loop discards
    // them. A test that must see a settlement keeps a second receiver so a
    // snapshot cannot swallow it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::SessionOpened { session })) => return session,
            Ok(Ok(_)) => continue,
            other => panic!("no snapshot: {other:?}"),
        }
    }
}

async fn wait_for_terminal(rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(left, rx.recv())
            .await
            .expect("the turn ends")
            .expect("event stream open");
        let kind = serde_json::to_value(&event).unwrap()["type"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if kind.starts_with("turn_") && kind != "turn_progress" && kind != "turn_started" {
            return;
        }
    }
}

/// A stale busy client may submit a steer after the runtime became idle.
/// Admission still has to precede every execution event for that identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_subscriber_sees_admission_before_an_idle_steer_starts() {
    let harness = harness(vec![text("done"), text("done")]).await;
    // This observer has no optimistic input row. The admission must announce
    // its identity before task startup can expose it to an executor.
    let mut observer = harness.client.subscribe();
    let id = MessageId::new("idle-steer-order");
    harness
        .client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: harness.session.clone(),
            content: "answer briefly".into(),
            message_id: Some(id.clone()),
        })
        .await
        .unwrap();

    let mut admitted = false;
    let mut started = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(left, observer.recv())
            .await
            .expect("the input must settle")
            .expect("the observer must receive every event");
        match event {
            RuntimeEvent::InputAdmitted { message } if message.id == id => {
                assert!(!admitted, "one admission before execution");
                admitted = true;
            }
            RuntimeEvent::UserMessageAdded { message } if message.id == id => {
                assert!(admitted, "task startup exposed the input before admission");
                started = true;
            }
            RuntimeEvent::InputSettled {
                message_id, state, ..
            } if message_id == id => {
                assert!(admitted, "settlement arrived before the observer had a row");
                assert!(started, "the seed input belongs to the started turn");
                assert_eq!(state, UiInputState::Delivered);
                break;
            }
            _ => {}
        }
    }
    wait_for_terminal(&mut observer).await;
}

/// The append is durable before the command answers, and its admission,
/// settlement and transcript row all carry the client's own id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admitted_append_keeps_its_id_from_ack_to_transcript() {
    let (_tmp, _server, client, session) = client_with(vec![
        tool_call("shell_command", serde_json::json!({"cmd": "sleep 10"})),
        text("done"),
        text("done"),
    ])
    .await;
    let mut events = client.subscribe();
    let mut replies = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "run the slow command".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let id = MessageId::new("append-identity");
    client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: session.clone(),
            content: "also cover the admin page".into(),
            message_id: Some(id.clone()),
        })
        .await
        .unwrap();

    // The command has answered; the row it answered for must already be part
    // of the session's durable state (the snapshot reads the journal).
    let at_ack = snapshot(&client, &mut replies, &session).await;
    let admitted: Vec<_> = at_ack
        .messages
        .iter()
        .filter(|message| message.id == id)
        .collect();
    assert_eq!(admitted.len(), 1, "{at_ack:?}");
    assert!(
        matches!(
            admitted[0].kind,
            Some(UiMessageKind::PendingInput) | Some(UiMessageKind::DeliveredInput)
        ),
        "{:?}",
        admitted[0]
    );

    settle_event(&mut events, &id, UiInputState::Delivered).await;
    wait_for_terminal(&mut events).await;
    let after = snapshot(&client, &mut replies, &session).await;
    let rows: Vec<_> = after
        .messages
        .iter()
        .filter(|message| message.text == "also cover the admin page")
        .collect();
    assert_eq!(rows.len(), 1, "one row, one identity: {after:?}");
    assert_eq!(rows[0].id, id, "the transcript reused the client's id");
    assert_eq!(rows[0].kind, Some(UiMessageKind::DeliveredInput));
    assert!(rows[0].ordinal.is_some());
}

/// The Goal path shares the runtime's one input lifecycle: the turn profile
/// changes how the opening message is executed, not how an append is
/// admitted, identified, or consumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_turn_delivers_an_append_under_the_same_identity() {
    let harness = harness_in(
        vec![
            tool_call("shell_command", serde_json::json!({"cmd": "sleep 10"})),
            text("done"),
            text("done"),
        ],
        leveler_agent::CollaborationMode::Goal,
    )
    .await;
    let mut events = harness.client.subscribe();
    let mut replies = harness.client.subscribe();
    harness
        .client
        .send(ClientCommand::SubmitMessage {
            session_id: harness.session.clone(),
            content: "run the slow command".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let id = MessageId::new("goal-append");
    harness
        .client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: harness.session.clone(),
            content: "also cover the admin page".into(),
            message_id: Some(id.clone()),
        })
        .await
        .unwrap();
    settle_event(&mut events, &id, UiInputState::Delivered).await;
    wait_for_terminal(&mut events).await;

    let after = snapshot(&harness.client, &mut replies, &harness.session).await;
    let rows: Vec<_> = after
        .messages
        .iter()
        .filter(|message| message.id == id)
        .collect();
    assert_eq!(rows.len(), 1, "{after:?}");
    assert_eq!(rows[0].kind, Some(UiMessageKind::DeliveredInput));
    assert_eq!(rows[0].text, "also cover the admin page");
}

/// A journal row admitted by an earlier process is part of the snapshot a new
/// client opens. Nothing in this process saw the admission event; the durable
/// journal is the only place the accepted text survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_input_from_a_previous_process_is_in_the_snapshot() {
    let harness = harness(vec![text("done")]).await;
    let db = harness.app.open_database().await.unwrap();
    leveler_storage::SessionInputRepository::new(&db)
        .admit(
            "recovered-append",
            &harness.session,
            None,
            "also cover the admin page",
            leveler_core::now(),
        )
        .await
        .unwrap();

    let mut replies = harness.client.subscribe();
    let session = snapshot(&harness.client, &mut replies, &harness.session).await;
    let rows: Vec<_> = session
        .messages
        .iter()
        .filter(|message| message.id == MessageId::new("recovered-append"))
        .collect();
    assert_eq!(rows.len(), 1, "{session:?}");
    assert_eq!(rows[0].kind, Some(UiMessageKind::PendingInput));
    assert_eq!(rows[0].text, "also cover the admin page");
}

/// Resume rebuilds the conversation from history, not from the snapshot's
/// message list. That replay must keep the identity and delivery state of an
/// accepted input, or the reopened client paints it as a fresh plain message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivered_append_reopens_under_its_own_identity() {
    let harness = harness(vec![
        tool_call("shell_command", serde_json::json!({"cmd": "sleep 10"})),
        text("done"),
        text("done"),
        text("done"),
    ])
    .await;
    let mut events = harness.client.subscribe();
    harness
        .client
        .send(ClientCommand::SubmitMessage {
            session_id: harness.session.clone(),
            content: "run the slow command".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let id = MessageId::new("reopen-identity");
    harness
        .client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: harness.session.clone(),
            content: "also cover the admin page".into(),
            message_id: Some(id.clone()),
        })
        .await
        .unwrap();
    settle_event(&mut events, &id, UiInputState::Delivered).await;
    wait_for_terminal(&mut events).await;

    let db = harness.app.open_database().await.unwrap();
    let (entries, _) = leveler_app::session_history::load_session_history(&db, &harness.session)
        .await
        .unwrap();
    let row = entries
        .iter()
        .find_map(|entry| match &entry.event {
            RuntimeEvent::UserMessageAdded { message }
                if message.text == "also cover the admin page" =>
            {
                Some(message)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the append is in history: {entries:?}"));
    assert_eq!(row.id, id, "history reuses the client's identity");
    assert_eq!(row.kind, Some(UiMessageKind::DeliveredInput));
}

/// An ordinary submission has no append journal row: its write-ahead turn
/// record is the durable acceptance. The command answers only after that row
/// exists, so a crash at the acknowledgement cannot lose the text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ordinary_submit_is_durable_when_the_command_answers() {
    let harness = harness(vec![
        tool_call("shell_command", serde_json::json!({"cmd": "sleep 10"})),
        text("done"),
    ])
    .await;
    harness
        .client
        .send(ClientCommand::SubmitMessage {
            session_id: harness.session.clone(),
            content: "run the slow command".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let db = harness.app.open_database().await.unwrap();
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&harness.session)
        .await
        .unwrap();
    let row = turns
        .iter()
        .find(|turn| {
            turn.payload
                .as_deref()
                .is_some_and(|payload| payload.contains("run the slow command"))
        })
        .unwrap_or_else(|| panic!("the initiating message is in a turn row: {turns:?}"));
    assert_eq!(
        row.status, "running",
        "the acknowledgement waits for this row"
    );
    assert!(
        leveler_storage::SessionInputRepository::new(&db)
            .all(&harness.session)
            .await
            .unwrap()
            .is_empty(),
        "a turn input is not an accepted append"
    );
}

/// A second client that never saw the first client's optimistic paint reads
/// the same input state from the runtime's durable projection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_client_reads_the_same_input_state_from_the_runtime() {
    let harness = harness(vec![
        tool_call("shell_command", serde_json::json!({"cmd": "sleep 10"})),
        text("done"),
        text("done"),
    ])
    .await;
    let mut events = harness.client.subscribe();
    harness
        .client
        .send(ClientCommand::SubmitMessage {
            session_id: harness.session.clone(),
            content: "run the slow command".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let id = MessageId::new("shared-append");
    harness
        .client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: harness.session.clone(),
            content: "also cover the admin page".into(),
            message_id: Some(id.clone()),
        })
        .await
        .unwrap();

    let second = InProcessRuntimeClient::new(
        harness.app.clone(),
        harness.model.clone(),
        PermissionProfile::FullAccess,
        false,
    );
    let mut second_replies = second.subscribe();
    let from_second = snapshot(&second, &mut second_replies, &harness.session).await;
    let rows: Vec<_> = from_second
        .messages
        .iter()
        .filter(|message| message.id == id)
        .collect();
    assert_eq!(rows.len(), 1, "{from_second:?}");
    assert!(
        matches!(
            rows[0].kind,
            Some(UiMessageKind::PendingInput) | Some(UiMessageKind::DeliveredInput)
        ),
        "{:?}",
        rows[0]
    );

    settle_event(&mut events, &id, UiInputState::Delivered).await;
    wait_for_terminal(&mut events).await;
    let after = snapshot(&second, &mut second_replies, &harness.session).await;
    let rows: Vec<_> = after
        .messages
        .iter()
        .filter(|message| message.id == id)
        .collect();
    assert_eq!(rows.len(), 1, "{after:?}");
    assert_eq!(rows[0].kind, Some(UiMessageKind::DeliveredInput));
    assert!(rows[0].ordinal.is_some());
    assert_eq!(
        after
            .messages
            .iter()
            .filter(|message| message.text == "also cover the admin page")
            .count(),
        1,
        "one message, one row: {after:?}"
    );
}

/// Cancel settles the pending append under its own id. A later, unrelated turn
/// must not run it or resurrect it as a fresh message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_append_is_settled_and_never_resurrects() {
    let (_tmp, _server, client, session) = client_with(vec![
        tool_call("shell_command", serde_json::json!({"cmd": "sleep 10"})),
        text("done"),
        text("later"),
    ])
    .await;
    let mut events = client.subscribe();
    let mut replies = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "run the slow command".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let id = MessageId::new("append-cancelled");
    client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: session.clone(),
            content: "also cover the admin page".into(),
            message_id: Some(id.clone()),
        })
        .await
        .unwrap();
    client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    settle_event(&mut events, &id, UiInputState::Cancelled).await;
    wait_for_terminal(&mut events).await;

    let after_cancel = snapshot(&client, &mut replies, &session).await;
    let rows: Vec<_> = after_cancel
        .messages
        .iter()
        .filter(|message| message.id == id)
        .collect();
    assert_eq!(rows.len(), 1, "{after_cancel:?}");
    assert_eq!(rows[0].kind, Some(UiMessageKind::CancelledInput));

    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "unrelated later work".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_terminal(&mut events).await;
    let after_later = snapshot(&client, &mut replies, &session).await;
    let cancelled: Vec<_> = after_later
        .messages
        .iter()
        .filter(|message| message.text == "also cover the admin page")
        .collect();
    assert_eq!(cancelled.len(), 1, "{after_later:?}");
    assert_eq!(cancelled[0].id, id);
    assert_eq!(cancelled[0].kind, Some(UiMessageKind::CancelledInput));
}
