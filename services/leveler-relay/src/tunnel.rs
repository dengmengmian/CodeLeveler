//! The two WebSockets, and the forwarding between them.
//!
//! The agent dials `/v1/agent/tunnel` outbound; a device connects to
//! `/v1/hosts/{host_id}/session` with a routing token. The relay carries signed
//! envelopes between them **verbatim**. It never unwraps one to re-wrap the
//! payload in JSON of its own — that would discard the signature covering the
//! header, which is the only reason either side can trust what it receives.
//!
//! What the relay does decide is routing: which agent a frame goes to, and which
//! device gets the answer. Getting that wrong is the isolation failure the tests
//! here exist to catch.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use tokio::sync::mpsc;

use leveler_remote_protocol::SignedEnvelope;
use leveler_remote_protocol::auth::{AgentRegisterAssertion, ProtocolVersionDto};
use leveler_remote_protocol::tunnel::{AgentToRelay, RelayToAgent};

use crate::state::RelayState;

#[derive(Debug, Deserialize)]
pub(crate) struct TunnelQuery {
    runtime_id: String,
    #[serde(default)]
    display_name: Option<String>,
    /// Signed over `{runtime_id}|{timestamp}` with the runtime key bound at
    /// `pair/begin`.
    timestamp: String,
    sig: String,
}

/// The agent's outbound control channel.
///
/// The agent proves it owns the runtime id it claims. Without this a
/// `runtime_id` would be a bare name, and anyone able to reach the relay could
/// register as someone else's machine and be handed their devices' streams.
pub(crate) async fn agent_tunnel(
    State(state): State<RelayState>,
    Query(query): Query<TunnelQuery>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, crate::routes::Failure> {
    let pubkey = state
        .runtime_key(&query.runtime_id)
        .ok_or(crate::state::RelayError::Unauthorized)?;
    let key = leveler_remote_protocol::VerifyingKey::from_base64url(&pubkey)
        .map_err(|_| crate::state::RelayError::Unauthorized)?;
    let assertion = AgentRegisterAssertion::signing_input(&query.runtime_id, &query.timestamp);
    crate::routes::verify_signature(&key, assertion.as_bytes(), &query.sig)?;
    crate::routes::within_window(&query.timestamp)?;

    Ok(upgrade.on_upgrade(move |socket| serve_agent(state, query, socket)))
}

async fn serve_agent(state: RelayState, query: TunnelQuery, socket: WebSocket) {
    let (mut sink, mut incoming) = socket.split();
    let (to_agent, mut outbox) = mpsc::unbounded_channel::<RelayToAgent>();

    let runtime_id = query.runtime_id.clone();
    let display_name = query.display_name.unwrap_or_else(|| runtime_id.clone());
    state.register_runtime(&runtime_id, &display_name, to_agent);

    let ack = RelayToAgent::RegisterAck {
        runtime_id: runtime_id.clone(),
        protocol: ProtocolVersionDto { major: 1, minor: 3 },
    };
    if send_json(&mut sink, &ack).await.is_err() {
        state.unregister_runtime(&runtime_id);
        return;
    }

    // Two directions on one socket: frames the relay has for the agent, and
    // frames the agent sends back for its devices.
    loop {
        tokio::select! {
            queued = outbox.recv() => match queued {
                Some(frame) => {
                    if send_json(&mut sink, &frame).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            received = incoming.next() => match received {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<AgentToRelay>(&text) {
                        Ok(frame) => handle_agent_frame(&state, frame),
                        Err(error) => {
                            tracing::debug!(%error, "unparseable agent frame");
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    tracing::debug!(%error, "agent tunnel error");
                    break;
                }
            },
        }
    }

    // The runtime is gone: its streams go too, and nothing is kept for a later
    // replay.
    state.unregister_runtime(&runtime_id);
}

fn handle_agent_frame(state: &RelayState, frame: AgentToRelay) {
    match frame {
        AgentToRelay::ForwardDownstream { stream_id, frame } => {
            if let Err(error) = state.forward_downstream(&stream_id, frame) {
                tracing::debug!(code = error.code(), %stream_id, "downstream not routable");
            }
        }
        AgentToRelay::RpcResponse {
            envelope: Some(envelope),
            ..
        } => {
            // RPC responses ride the stream named in the signed envelope, which
            // is what binds a response to its request.
            let stream_id = envelope.stream_id.clone();
            if let Err(error) = state.forward_downstream(&stream_id, envelope) {
                tracing::debug!(code = error.code(), %stream_id, "rpc response not routable");
            }
        }
        AgentToRelay::StreamRejected { stream_id, code } => {
            state.close_stream(&stream_id, &code);
        }
        // Heartbeats, registration and acceptance need no routing decision.
        _ => {}
    }
}

/// Which project a device's stream is for. Optional, so a single-project host
/// needs no extra ceremony; the agent refuses the stream if the host has more
/// than one open and the device named none.
#[derive(Debug, Deserialize)]
pub(crate) struct SessionQuery {
    #[serde(default)]
    project_id: Option<String>,
}

/// A device's session stream.
pub(crate) async fn app_session(
    State(state): State<RelayState>,
    Path(host_id): Path<String>,
    Query(query): Query<SessionQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, crate::routes::Failure> {
    // Authorize before the upgrade: a rejected caller should never reach a
    // WebSocket at all.
    let token = crate::routes::bearer(&headers)?;
    let claims = state.authorize(&token, Some(&host_id), now())?;
    state.require_online(&host_id)?;

    let device_id = claims.sub.clone();
    let scope = claims.pairing_scope;
    let jti = claims.jti.clone();
    Ok(upgrade.on_upgrade(move |socket| {
        serve_app(
            state,
            socket,
            device_id,
            host_id,
            query.project_id,
            scope,
            jti,
        )
    }))
}

#[allow(clippy::too_many_arguments)]
async fn serve_app(
    state: RelayState,
    socket: WebSocket,
    device_id: String,
    runtime_id: String,
    project_id: Option<String>,
    scope: leveler_remote_protocol::pairing::PairingScope,
    jti: String,
) {
    let (mut sink, mut incoming) = socket.split();
    let (to_app, mut outbox) = mpsc::unbounded_channel::<SignedEnvelope>();

    let stream_id = match state.open_stream(
        &device_id,
        &runtime_id,
        project_id.as_deref(),
        scope,
        &jti,
        to_app,
    ) {
        Ok(stream_id) => stream_id,
        Err(_) => return,
    };

    loop {
        tokio::select! {
            queued = outbox.recv() => match queued {
                Some(frame) => {
                    if send_json(&mut sink, &frame).await.is_err() {
                        break;
                    }
                }
                // The route was dropped — revoked, or the runtime went away.
                None => break,
            },
            received = incoming.next() => match received {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<SignedEnvelope>(&text) {
                        Ok(frame) => {
                            if let Err(error) = state.forward_upstream(&stream_id, frame) {
                                tracing::debug!(code = error.code(), "upstream not routable");
                                break;
                            }
                        }
                        Err(error) => {
                            tracing::debug!(%error, "unparseable device frame");
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    tracing::debug!(%error, "app session error");
                    break;
                }
            },
        }
    }

    state.close_stream(&stream_id, "app_gone");
}

async fn send_json<T: serde::Serialize>(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    value: &T,
) -> Result<(), ()> {
    let text = serde_json::to_string(value).map_err(|_| ())?;
    sink.send(Message::Text(text.into())).await.map_err(|_| ())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
