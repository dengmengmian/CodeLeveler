//! The WebSocket endpoint: one connection per browser tab.
//!
//! Lifecycle: authenticate → upgrade → (optional, when `?session=` is given)
//! push that session's snapshot → forward the runtime's global event stream
//! downstream while parsing upstream frames. A lagging event subscription
//! forces a resync: the client reloads from a fresh snapshot after reconnect,
//! so canonical state is never silently skipped.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use leveler_client_protocol::{
    ClientError, CommandEnvelope, CommandId, ProtocolEnvelope, RuntimeEvent, SessionId,
};

use crate::auth;
use crate::protocol::{DownstreamMessage, UpstreamMessage};
use crate::server::AppState;

/// Query parameters accepted on the upgrade request: `/ws?session=<id>`.
/// (`token` is read from the raw query by the auth check.)
#[derive(Debug, Deserialize)]
pub(crate) struct WsQuery {
    /// Session to snapshot-push immediately after connect.
    session: Option<String>,
}

/// Upgrade to a WebSocket when the presented token matches; otherwise 401
/// before any protocol bytes are exchanged.
pub(crate) async fn ws_handler(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !auth::is_authorized(raw_query.as_deref(), &headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    upgrade.on_upgrade(move |socket| handle_socket(socket, state, query.session))
}

/// Drive one WebSocket connection until the client hangs up or the event
/// stream forces a resync.
async fn handle_socket(socket: WebSocket, state: AppState, session: Option<String>) {
    let (sink, mut stream) = socket.split();
    // All outbound frames funnel through one channel so the write task is the
    // sole owner of the sink.
    let (outgoing, outgoing_rx) = mpsc::channel::<DownstreamMessage>(64);
    let events = state.service.subscribe();
    let mut write_task = tokio::spawn(write_loop(sink, outgoing_rx, events, session.clone()));

    // Greeting frame: the requested session's snapshot, or an error frame if
    // the session is unknown — the connection stays up either way.
    if let Some(session_id) = &session {
        let frame = match state.service.snapshot(&SessionId::new(session_id)).await {
            Ok(session) => DownstreamMessage::Snapshot { session },
            Err(error) => error_frame(&error, None),
        };
        if outgoing.send(frame).await.is_err() {
            write_task.abort();
            return;
        }
    }

    read_loop(&state, &mut stream, &outgoing, &mut write_task).await;
    write_task.abort();
}

/// Parse upstream frames until the socket closes or the write task ends.
async fn read_loop(
    state: &AppState,
    stream: &mut SplitStream<WebSocket>,
    outgoing: &mpsc::Sender<DownstreamMessage>,
    write_task: &mut JoinHandle<()>,
) {
    loop {
        tokio::select! {
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    handle_upstream(state, &text, outgoing).await;
                }
                // Binary frames carry no meaning on this protocol; ping/pong is
                // answered by the transport itself.
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            // The write task only ends early after a lagged subscription sent
            // `resync_required`; nothing more can usefully happen here.
            _ = &mut *write_task => break,
        }
    }
}

/// Forward runtime events and queued outbound frames to the socket.
async fn write_loop(
    mut sink: SplitSink<WebSocket, Message>,
    mut outgoing: mpsc::Receiver<DownstreamMessage>,
    mut events: broadcast::Receiver<RuntimeEvent>,
    session: Option<String>,
) {
    loop {
        tokio::select! {
            frame = outgoing.recv() => {
                // All senders dropped → the read side is gone; stop writing.
                let Some(frame) = frame else { return };
                if send_frame(&mut sink, &frame).await.is_err() {
                    return;
                }
            }
            event = events.recv() => match event {
                Ok(event) => {
                    if send_frame(&mut sink, &DownstreamMessage::Event { event })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "web event subscriber lagged; forcing resync");
                    let frame = DownstreamMessage::ResyncRequired {
                        session_id: session.unwrap_or_default(),
                    };
                    let _ = send_frame(&mut sink, &frame).await;
                    let _ = sink.send(Message::Close(None)).await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// Serialize and send one downstream frame.
async fn send_frame(
    sink: &mut SplitSink<WebSocket, Message>,
    frame: &DownstreamMessage,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(frame).expect("downstream frames always serialize");
    sink.send(Message::Text(text.into())).await
}

/// Handle one upstream text frame: deliver a command, or answer a snapshot
/// request. Failures produce an `error` frame; the connection stays open.
async fn handle_upstream(state: &AppState, text: &str, outgoing: &mpsc::Sender<DownstreamMessage>) {
    let message = match serde_json::from_str::<UpstreamMessage>(text) {
        Ok(message) => message,
        Err(error) => {
            send_or_ignore(
                outgoing,
                DownstreamMessage::Error {
                    code: "invalid_frame".to_string(),
                    message: format!("unrecognized upstream frame: {error}"),
                    command_id: None,
                },
            )
            .await;
            return;
        }
    };
    match message {
        UpstreamMessage::Deliver {
            command_id,
            session_id,
            command,
        } => {
            let envelope = CommandEnvelope {
                command_id: CommandId::new(command_id.clone()),
                session_id: SessionId::new(session_id),
                expected_version: None,
                issued_at: leveler_core::now().to_rfc3339(),
                command,
            };
            let frame = match state
                .service
                .deliver_protocol(ProtocolEnvelope::wrap(envelope))
                .await
            {
                Ok(()) => DownstreamMessage::Ack { command_id },
                Err(error) => error_frame(&error, Some(command_id)),
            };
            send_or_ignore(outgoing, frame).await;
        }
        UpstreamMessage::Snapshot { session_id } => {
            let frame = match state.service.snapshot(&SessionId::new(session_id)).await {
                Ok(session) => DownstreamMessage::Snapshot { session },
                Err(error) => error_frame(&error, None),
            };
            send_or_ignore(outgoing, frame).await;
        }
    }
}

/// Render a client error as an `error` frame, correlating it when known.
fn error_frame(error: &ClientError, command_id: Option<String>) -> DownstreamMessage {
    let code = match error {
        ClientError::SessionNotFound(_) => "session_not_found",
        ClientError::Runtime(_) => "runtime_error",
    };
    DownstreamMessage::Error {
        code: code.to_string(),
        message: error.to_string(),
        command_id,
    }
}

/// Best-effort send: a full/closed channel means the socket is going away.
async fn send_or_ignore(outgoing: &mpsc::Sender<DownstreamMessage>, frame: DownstreamMessage) {
    let _ = outgoing.send(frame).await;
}
