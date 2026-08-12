//! R8: one session handed between clients (convergence plan phase 7).
//!
//! The runtime contract says task facts come only from canonical events and
//! snapshots: closing one client must not change them, a second client must
//! see the same session state, and handing the session over must not
//! re-execute anything. These tests drive the real transport (a Unix socket
//! server standing in for a detached TUI/daemon) plus the in-process client
//! the Web layer uses, over ONE session.

#![cfg(unix)]

use std::sync::Arc;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, CommandEnvelope, InteractiveRuntimeClient,
    PermissionProfile as WirePermissionProfile, RuntimeEvent,
};
use leveler_core::{CommandId, SessionId};
use leveler_execution::PermissionProfile;
use leveler_local_transport::{CreateSessionRequest, LocalSocketRuntimeClient, LocalSocketServer};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::MessageRepository;
use tokio_util::sync::CancellationToken;

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn write_config(root: &std::path::Path, base_url: &str) {
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
  context_window: 8192
  reliable_context: 4096
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

struct Harness {
    _tmp: tempfile::TempDir,
    app: Arc<Application>,
    runtime: Arc<InProcessRuntimeClient>,
    socket: std::path::PathBuf,
}

async fn harness() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    // Unreachable model endpoint: these tests assert transport/relay facts
    // (messages, snapshots, event routing), never model output.
    write_config(tmp.path(), "http://127.0.0.1:9");
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let runtime = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ));
    let socket = tmp.path().join("relay.sock");
    Harness {
        _tmp: tmp,
        app,
        runtime,
        socket,
    }
}

/// Wait for the session's in-flight turn to reach a terminal marker. The
/// model endpoint is unreachable in these tests, so a turn ends in
/// `TurnFailed` — which is exactly the point: it ended on its own, without
/// the client that started it.
async fn wait_turn_settled(rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(60), rx.recv())
            .await
            .expect("the turn must settle")
            .expect("event stream open");
        if matches!(
            event,
            RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnAnswered
                | RuntimeEvent::TurnTruncated { .. }
                | RuntimeEvent::TurnIncomplete { .. }
                | RuntimeEvent::TurnCompletedUnverified { .. }
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnCancelled
        ) {
            return;
        }
    }
}

async fn wait_for_message(app: &Application, session: &SessionId, needle: &str, label: &str) {
    let db = app.open_database().await.unwrap();
    for _ in 0..100 {
        let payloads = MessageRepository::new(&db).load(session).await.unwrap();
        if payloads.iter().any(|p| p.contains(needle)) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{label}: {needle:?} never reached the transcript");
}

/// The core relay property: client A creates a session and speaks, A goes
/// away entirely (socket closed, server shut down), then client B attaches to
/// the SAME session and sees A's work from the snapshot — no replay, no loss,
/// and B can carry the session forward.
#[tokio::test]
async fn a_session_survives_its_first_client_and_carries_to_the_next() {
    let h = harness().await;

    // ── client A: a detached TUI over the socket transport ───────────────
    let server_a = LocalSocketServer::bind(&h.socket, h.runtime.clone())
        .await
        .unwrap();
    let shutdown_a = CancellationToken::new();
    let task_a = tokio::spawn(server_a.serve(shutdown_a.clone()));
    let client_a = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();

    let bootstrap = client_a
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "relay across clients".to_string(),
            model: None,
            mode: WirePermissionProfile::RequestApproval,
        })
        .await
        .unwrap();
    let session = bootstrap.session.id.clone();
    // Subscribe on the RUNTIME (not through A's connection) so the turn stays
    // observable after A is gone — that is the invariant under test.
    let mut runtime_events = h.runtime.subscribe_session(&session);

    client_a
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "FROM_CLIENT_A".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_message(&h.app, &session, "FROM_CLIENT_A", "client A").await;

    let snapshot_a = client_a.snapshot(&session).await.unwrap();
    let a_message_count = snapshot_a.messages.len();
    assert!(
        snapshot_a
            .messages
            .iter()
            .any(|m| m.text.contains("FROM_CLIENT_A")),
        "client A must see its own message: {:?}",
        snapshot_a.messages
    );

    // ── A disappears mid-turn: connection dropped AND server torn down ───
    drop(client_a);
    shutdown_a.cancel();
    let _ = task_a.await;

    // Accepted work does not die with the UI that submitted it: the turn A
    // started runs to its own terminal marker with no client attached.
    wait_turn_settled(&mut runtime_events).await;

    // The task fact outlives the client that created it.
    let db = h.app.open_database().await.unwrap();
    let after_disconnect = MessageRepository::new(&db).load(&session).await.unwrap();
    assert!(
        after_disconnect.iter().any(|p| p.contains("FROM_CLIENT_A")),
        "closing a client must not erase the session's messages"
    );

    // ── client B: a fresh socket server + connection, same runtime ───────
    let socket_b = h.socket.with_extension("b");
    let server_b = LocalSocketServer::bind(&socket_b, h.runtime.clone())
        .await
        .unwrap();
    let shutdown_b = CancellationToken::new();
    let task_b = tokio::spawn(server_b.serve(shutdown_b.clone()));
    let client_b = LocalSocketRuntimeClient::connect(&socket_b).await.unwrap();

    let snapshot_b = client_b.snapshot(&session).await.unwrap();
    assert_eq!(
        snapshot_b.messages.len(),
        a_message_count,
        "the handover snapshot must match what A had — no loss, no duplication"
    );
    assert!(
        snapshot_b
            .messages
            .iter()
            .any(|m| m.text.contains("FROM_CLIENT_A")),
        "client B must inherit A's transcript from the snapshot"
    );
    assert_eq!(
        snapshot_b.id, session,
        "the handover must land on the same session"
    );

    // B carries the session forward; A's message is not re-executed.
    client_b
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "FROM_CLIENT_B".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_message(&h.app, &session, "FROM_CLIENT_B", "client B").await;

    let final_messages = MessageRepository::new(&db).load(&session).await.unwrap();
    let a_copies = final_messages
        .iter()
        .filter(|p| p.contains("FROM_CLIENT_A"))
        .count();
    assert_eq!(
        a_copies, 1,
        "handing the session over must not replay client A's message"
    );
    assert!(
        final_messages.iter().any(|p| p.contains("FROM_CLIENT_B")),
        "client B's message must land in the same transcript"
    );

    shutdown_b.cancel();
    let _ = task_b.await;
}

/// Two clients attached at once (a TUI and the Web layer's in-process client)
/// observe the same session facts, and a command from either reaches both —
/// one runtime, one event stream, no per-client state machine.
#[tokio::test]
async fn concurrent_clients_share_one_event_stream_for_the_same_session() {
    let h = harness().await;

    let server = LocalSocketServer::bind(&h.socket, h.runtime.clone())
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(server.serve(shutdown.clone()));
    let socket_client = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();

    let bootstrap = socket_client
        .create_session(CreateSessionRequest {
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "two clients".to_string(),
            model: None,
            mode: WirePermissionProfile::RequestApproval,
        })
        .await
        .unwrap();
    let session = bootstrap.session.id.clone();

    // The Web-side client subscribes to the very same session.
    let mut in_process_events = h.runtime.subscribe_session(&session);
    let mut socket_events = socket_client.subscribe_session(&session);

    // A command issued over the socket must be observable by BOTH clients.
    socket_client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "SHARED_FACT".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();

    async fn wait_for_user_message(
        rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
        needle: &str,
        who: &str,
    ) {
        let deadline = std::time::Duration::from_secs(5);
        let found = tokio::time::timeout(deadline, async {
            loop {
                match rx.recv().await {
                    Ok(RuntimeEvent::UserMessageAdded { message })
                        if message.text.contains(needle) =>
                    {
                        return true;
                    }
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        })
        .await;
        assert!(
            matches!(found, Ok(true)),
            "{who} never observed {needle:?} on the shared session stream"
        );
    }

    wait_for_user_message(&mut socket_events, "SHARED_FACT", "the socket client").await;
    wait_for_user_message(
        &mut in_process_events,
        "SHARED_FACT",
        "the in-process client",
    )
    .await;

    // Compare the two views only once the turn has settled. `status` is a
    // live value: two snapshot calls a moment apart legitimately straddle the
    // created→running transition, and asserting across that samples a moving
    // number rather than proving anything about forked state.
    wait_turn_settled(&mut in_process_events).await;

    // Both clients' snapshots agree on the same facts.
    let via_socket = socket_client.snapshot(&session).await.unwrap();
    let via_in_process = h.runtime.snapshot(&session).await.unwrap();
    assert_eq!(
        via_socket.messages.len(),
        via_in_process.messages.len(),
        "the two clients disagree on the transcript length — state has forked"
    );
    assert_eq!(via_socket.id, via_in_process.id);
    assert_eq!(
        via_socket.status, via_in_process.status,
        "the two clients disagree on session status — state has forked"
    );

    // The turn already settled above; nothing is in flight at teardown.
    shutdown.cancel();
    let _ = task.await;
}

/// An at-least-once retry that crosses the handover (client B re-sends the
/// command id client A already delivered) must not execute twice: idempotency
/// is a runtime fact, not a per-connection one.
#[tokio::test]
async fn a_command_retried_by_the_next_client_is_not_executed_twice() {
    let h = harness().await;
    let session = h
        .app
        .create_session(&ModelRef::new("mock", "m"), "idempotent handover")
        .await
        .unwrap();

    let mut settle_rx = h.runtime.subscribe_session(&session);
    let envelope = CommandEnvelope {
        command_id: CommandId::new("cmd-relay"),
        session_id: session.clone(),
        expected_version: None,
        issued_at: "2026-08-06T00:00:00Z".to_string(),
        command: ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "RETRIED_ACROSS_CLIENTS".to_string(),
            attachments: vec![],
        },
    };

    // Client A delivers it…
    h.runtime.deliver(envelope.clone()).await.unwrap();
    wait_for_message(&h.app, &session, "RETRIED_ACROSS_CLIENTS", "first delivery").await;

    // …the connection drops before A sees the ack, and client B retries the
    // same envelope after taking over.
    let server = LocalSocketServer::bind(&h.socket, h.runtime.clone())
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(server.serve(shutdown.clone()));
    let client_b = LocalSocketRuntimeClient::connect(&h.socket).await.unwrap();
    client_b.deliver(envelope).await.unwrap();

    // Give a duplicate dispatch a chance to land before asserting it did not.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let db = h.app.open_database().await.unwrap();
    let copies = MessageRepository::new(&db)
        .load(&session)
        .await
        .unwrap()
        .iter()
        .filter(|p| p.contains("RETRIED_ACROSS_CLIENTS"))
        .count();
    assert_eq!(
        copies, 1,
        "a command retried after handover must dispatch exactly once"
    );

    // Let the turn this dispatched reach its terminal marker before tearing
    // the runtime down: a background task still driving a turn against a
    // dying Tokio runtime panics on the way out, which reads as a product
    // crash in the test output.
    wait_turn_settled(&mut settle_rx).await;
    shutdown.cancel();
    let _ = task.await;
}
