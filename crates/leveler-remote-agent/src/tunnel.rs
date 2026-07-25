//! The agent's outbound connection to a relay.
//!
//! Outbound because the alternative — listening on the developer's machine — is
//! the remote-code-execution gateway this whole design exists to avoid. The
//! relay never dials in.
//!
//! Everything arriving here is untrusted until [`AgentBridge`] says otherwise.
//! This module is transport only: it reads frames, hands them to the bridge, and
//! writes back what the bridge produced. It makes no authorization decision of
//! its own, so there is no second place where a policy could be forgotten.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::{SinkExt as _, StreamExt as _};
use leveler_local_transport::LocalRuntimeService;
use leveler_remote_protocol::tunnel::{AgentToRelay, RelayToAgent, RoutingError};
use leveler_remote_protocol::{ContentType, SignedEnvelope};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::bridge::{AdmissionError, Admitted, AgentBridge};

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("could not reach the relay: {0}")]
    Connect(String),
    #[error("tunnel closed: {0}")]
    Closed(String),
}

/// One live APP stream, from this agent's point of view.
struct StreamState {
    device_id: String,
    /// Monotonic per stream, as the envelope spec requires. Starting from the
    /// agent's own counter rather than echoing the device's keeps the two
    /// directions independent.
    next_seq: u64,
}

/// Connect to `relay_ws_url` and serve frames until the connection ends.
///
/// `now` is injected so a test can pin the clock the signatures are stamped
/// with; production passes a closure reading the real one.
pub async fn run_tunnel<R, F>(
    relay_ws_url: &str,
    runtime_id: &str,
    display_name: &str,
    bridge: Arc<AgentBridge<R>>,
    now: F,
) -> Result<(), TunnelError>
where
    R: LocalRuntimeService + Send + Sync + 'static,
    F: Fn() -> String + Send + Sync + 'static,
{
    let url = format!(
        "{relay_ws_url}/v1/agent/tunnel?runtime_id={runtime_id}&display_name={display_name}"
    );
    let (socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|error| TunnelError::Connect(error.to_string()))?;
    let (mut sink, mut incoming) = socket.split();

    // Replies are queued rather than written inline so that handling a frame
    // never blocks on the socket.
    let (outbox_tx, mut outbox) = mpsc::unbounded_channel::<AgentToRelay>();
    let mut streams: HashMap<String, StreamState> = HashMap::new();

    loop {
        tokio::select! {
            queued = outbox.recv() => match queued {
                Some(frame) => {
                    let text = serde_json::to_string(&frame)
                        .map_err(|error| TunnelError::Closed(error.to_string()))?;
                    sink.send(Message::Text(text.into()))
                        .await
                        .map_err(|error| TunnelError::Closed(error.to_string()))?;
                }
                None => break,
            },
            received = incoming.next() => match received {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<RelayToAgent>(&text) {
                        Ok(frame) => {
                            handle(&bridge, &mut streams, &outbox_tx, runtime_id, &now, frame).await;
                        }
                        Err(error) => {
                            // A frame this build cannot parse is the relay's
                            // problem, not grounds to drop every live stream.
                            tracing::warn!(%error, "unparseable relay frame");
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(TunnelError::Closed(error.to_string())),
            },
        }
    }
    Ok(())
}

async fn handle<R, F>(
    bridge: &Arc<AgentBridge<R>>,
    streams: &mut HashMap<String, StreamState>,
    outbox: &mpsc::UnboundedSender<AgentToRelay>,
    runtime_id: &str,
    now: &F,
    frame: RelayToAgent,
) where
    R: LocalRuntimeService + Send + Sync + 'static,
    F: Fn() -> String,
{
    match frame {
        RelayToAgent::OpenStream {
            stream_id,
            device_id,
            ..
        } => {
            // Accepting only means "this stream exists". Whether the device is
            // trusted is decided per frame against the local store, so a stream
            // opened for a device this host never paired with simply produces
            // refusals.
            streams.insert(
                stream_id.clone(),
                StreamState {
                    device_id,
                    next_seq: 1,
                },
            );
            let _ = outbox.send(AgentToRelay::StreamAccepted { stream_id });
        }

        RelayToAgent::CloseStream { stream_id, .. } => {
            streams.remove(&stream_id);
        }

        RelayToAgent::ForwardUpstream { stream_id, frame } => {
            let timestamp = now();
            // `None`: the relay is never asked for a key, so it cannot supply
            // one to be compared against — let alone used.
            let outcome = bridge.admit_upstream(&frame, &timestamp, None).await;

            let downstream = match outcome {
                Ok(Admitted::Delivered { command_id, .. }) => {
                    session_json(&serde_json::json!({"type": "ack", "command_id": command_id}))
                }
                Ok(Admitted::SnapshotRequested { session_id }) => {
                    match bridge.snapshot(&session_id).await {
                        Ok(snapshot) => session_json(&serde_json::json!({
                            "type": "snapshot",
                            "session": snapshot
                        })),
                        Err(error) => error_json(&error, None),
                    }
                }
                Err(error) => {
                    // Report the refusal to the device rather than dropping the
                    // frame: a phone that never hears back cannot tell a denied
                    // command from a lost one.
                    error_json(&error, command_id_of(&frame))
                }
            };

            if let Some(state) = streams.get_mut(&stream_id) {
                let seq = state.next_seq;
                state.next_seq += 1;
                let device_id = state.device_id.clone();
                if let Ok(signed) = bridge.sign_downstream(
                    runtime_id,
                    &device_id,
                    &stream_id,
                    seq,
                    &timestamp,
                    ContentType::SessionDownstream,
                    downstream.as_bytes(),
                ) {
                    let _ = outbox.send(AgentToRelay::ForwardDownstream {
                        stream_id,
                        frame: signed,
                    });
                }
            }
        }

        RelayToAgent::RpcRequest { rpc_id, envelope } => {
            let timestamp = now();
            match bridge.handle_rpc(&envelope, &timestamp, None).await {
                Ok(signed) => {
                    let _ = outbox.send(AgentToRelay::RpcResponse {
                        rpc_id,
                        envelope: Some(signed),
                        error: None,
                    });
                }
                Err(error) => {
                    // Routing-level only: there is no runtime result to sign,
                    // so this carries no business body.
                    let _ = outbox.send(AgentToRelay::RpcResponse {
                        rpc_id,
                        envelope: None,
                        error: Some(RoutingError {
                            code: error.code().to_string(),
                            message: error.to_string(),
                        }),
                    });
                }
            }
        }

        RelayToAgent::RegisterAck { .. }
        | RelayToAgent::RegisterNack { .. }
        | RelayToAgent::PairingPending { .. }
        | RelayToAgent::HeartbeatAck { .. } => {}
    }
}

fn session_json(value: &serde_json::Value) -> String {
    value.to_string()
}

fn error_json(error: &AdmissionError, command_id: Option<String>) -> String {
    serde_json::json!({
        "type": "error",
        "code": error.code(),
        "message": error.to_string(),
        "command_id": command_id,
    })
    .to_string()
}

/// Best-effort correlation for a refusal. The payload failed verification or
/// policy, so this is read only to echo an id back — never trusted otherwise.
fn command_id_of(frame: &SignedEnvelope) -> Option<String> {
    let payload = frame.payload().ok()?;
    let value: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    value
        .get("command_id")
        .and_then(|id| id.as_str())
        .map(|id| id.to_string())
}
