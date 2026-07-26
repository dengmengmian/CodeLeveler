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
//!
//! Traffic runs both ways independently. Answers to a device's frames follow its
//! requests, but the runtime also speaks unprompted — assistant output, tool
//! activity, approval prompts — so each open stream carries a pump forwarding
//! its project's event stream downstream. Without it a phone could send and be
//! acknowledged while seeing nothing come back, which is the difference between
//! a remote control and a write-only pipe.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt as _, StreamExt as _};
use leveler_remote_protocol::auth::AgentRegisterAssertion;
use leveler_remote_protocol::pairing::PairingScope;
use leveler_remote_protocol::tunnel::{AgentToRelay, RelayToAgent, RoutingError};
use leveler_remote_protocol::{ContentType, SignedEnvelope};
use leveler_session_wire::DownstreamMessage;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::approvals::{RemotePresence, WAITER_POLL, watch_approvals};
use crate::audit::{AuditEvent, hashed};
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
    /// The project this stream talks to, fixed when the stream opened. Frames
    /// are routed by it rather than by anything inside the frame, so a stream
    /// cannot wander between repositories mid-conversation.
    project_id: String,
    /// Monotonic per stream, as the envelope spec requires. Starting from the
    /// agent's own counter rather than echoing the device's keeps the two
    /// directions independent. Shared with the event pump, because both write
    /// to the same stream and the sequence must not fork.
    next_seq: Arc<Mutex<u64>>,
    /// Forwards this project's runtime events. Aborted when the stream closes,
    /// so a departed phone stops costing a subscription.
    pump: tokio::task::JoinHandle<()>,
    /// This stream's contribution to "somebody remote is watching this project",
    /// which is what arms the approval timeout. Released when the stream closes.
    presence: Option<RemotePresence>,
}

impl StreamState {
    fn take_seq(&self) -> u64 {
        let mut next = self.next_seq.lock().unwrap();
        let seq = *next;
        *next += 1;
        seq
    }
}

/// Sign one downstream payload for a stream and queue it for the relay.
///
/// The seq comes from the stream's shared counter, so replies and events
/// interleave without either overwriting the other's place in the sequence.
fn send_downstream(
    bridge: &AgentBridge,
    outbox: &mpsc::UnboundedSender<AgentToRelay>,
    runtime_id: &str,
    device_id: &str,
    stream_id: &str,
    seq: u64,
    timestamp: &str,
    payload: &[u8],
) {
    match bridge.sign_downstream(
        runtime_id,
        device_id,
        stream_id,
        seq,
        timestamp,
        ContentType::SessionDownstream,
        payload,
    ) {
        Ok(signed) => {
            let _ = outbox.send(AgentToRelay::ForwardDownstream {
                stream_id: stream_id.to_string(),
                frame: signed,
            });
        }
        Err(error) => tracing::warn!(code = error.code(), "could not sign a downstream frame"),
    }
}

/// Forward one project's runtime events to one device stream until the stream
/// closes.
///
/// Subscribes to the project's whole event stream, which is what the TUI and the
/// single-project web server also do against a per-repository daemon. Filtering
/// by session here would invent a third policy for a question the product has
/// already answered in one way everywhere else.
async fn pump_events<F>(
    bridge: Arc<AgentBridge>,
    outbox: mpsc::UnboundedSender<AgentToRelay>,
    runtime_id: String,
    device_id: String,
    stream_id: String,
    project_id: String,
    next_seq: Arc<Mutex<u64>>,
    now: Arc<F>,
) where
    F: Fn() -> String + Send + Sync + 'static,
{
    let mut events = match bridge.subscribe(&project_id).await {
        Ok(events) => events,
        Err(error) => {
            // The stream stays open: commands will report the same failure with
            // a code the device can act on, rather than the phone seeing a
            // socket vanish for no stated reason.
            tracing::warn!(code = error.code(), %project_id, "no event stream for this project");
            return;
        }
    };

    let take_seq = || {
        let mut next = next_seq.lock().unwrap();
        let seq = *next;
        *next += 1;
        seq
    };

    loop {
        match events.recv().await {
            Ok(event) => {
                let Ok(payload) = serde_json::to_vec(&DownstreamMessage::Event { event }) else {
                    continue;
                };
                send_downstream(
                    &bridge,
                    &outbox,
                    &runtime_id,
                    &device_id,
                    &stream_id,
                    take_seq(),
                    &now(),
                    &payload,
                );
            }
            // The device missed events, so its view is now a guess. Say so and
            // end the stream: continuing would render a transcript with holes
            // in it, which is worse than a reconnect.
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, %stream_id, "remote event subscriber lagged");
                if let Ok(payload) = serde_json::to_vec(&DownstreamMessage::Error {
                    code: "resync_required".to_string(),
                    message: "the event stream lagged; reconnect and resynchronize".to_string(),
                    command_id: None,
                }) {
                    send_downstream(
                        &bridge,
                        &outbox,
                        &runtime_id,
                        &device_id,
                        &stream_id,
                        take_seq(),
                        &now(),
                        &payload,
                    );
                }
                let _ = outbox.send(AgentToRelay::CloseStream {
                    stream_id,
                    reason: "resync_required".to_string(),
                });
                return;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Connect to `relay_ws_url` and serve frames until the connection ends.
///
/// `approval_timeout` is the host's `remote.approval_timeout_secs`: how long a
/// remote-only approval waits before the agent denies it.
///
/// `now` is injected so a test can pin the clock the signatures are stamped
/// with; production passes a closure reading the real one.
pub async fn run_tunnel<F>(
    relay_ws_url: &str,
    runtime_id: &str,
    display_name: &str,
    bridge: Arc<AgentBridge>,
    approval_timeout: std::time::Duration,
    now: F,
) -> Result<(), TunnelError>
where
    F: Fn() -> String + Send + Sync + 'static,
{
    // Prove ownership of the runtime id. A relay that accepted the name alone
    // would hand this machine's streams to whoever asked for them first.
    let timestamp = now();
    let assertion = AgentRegisterAssertion::signing_input(runtime_id, &timestamp);
    let sig = bridge.sign_assertion(assertion.as_bytes());
    let url = format!(
        "{}/v1/agent/tunnel?runtime_id={}&display_name={}&timestamp={}&sig={}",
        relay_ws_url,
        urlencode(runtime_id),
        urlencode(display_name),
        urlencode(&timestamp),
        urlencode(&sig)
    );
    let (socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|error| TunnelError::Connect(error.to_string()))?;
    let (mut sink, mut incoming) = socket.split();

    // Replies are queued rather than written inline so that handling a frame
    // never blocks on the socket.
    let (outbox_tx, mut outbox) = mpsc::unbounded_channel::<AgentToRelay>();
    let mut streams: HashMap<String, StreamState> = HashMap::new();
    // One approval watch per project, shared by every stream on it: two phones
    // on the same repository must not each fire their own auto-deny.
    let mut watches: HashMap<String, ProjectWatch> = HashMap::new();
    // Shared with each stream's event pump, which outlives a single frame.
    let now = Arc::new(now);

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
                            handle(
                                &bridge,
                                &mut streams,
                                &mut watches,
                                &outbox_tx,
                                runtime_id,
                                approval_timeout,
                                &now,
                                frame,
                            )
                            .await;

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
                Some(Err(error)) => {
                    abort_pumps(&mut streams);
                    abort_watches(&mut watches);
                    return Err(TunnelError::Closed(error.to_string()));
                }
            },
        }
    }
    // The connection is gone; nothing may keep forwarding into it, and with no
    // remote stream left there is no remote-only approval to expire.
    abort_pumps(&mut streams);
    abort_watches(&mut watches);
    Ok(())
}

/// One project's approval watch, plus how many streams still need it.
struct ProjectWatch {
    presence: RemotePresence,
    task: tokio::task::JoinHandle<()>,
}

fn abort_watches(watches: &mut HashMap<String, ProjectWatch>) {
    for (_, watch) in watches.drain() {
        watch.task.abort();
    }
}

/// Release one stream's hold on its project's approval watch, stopping the watch
/// when the last remote stream for that project goes.
fn release_stream(watches: &mut HashMap<String, ProjectWatch>, state: &StreamState) {
    state.pump.abort();
    let Some(presence) = &state.presence else {
        return;
    };
    presence.detach();
    if presence.count() == 0
        && let Some(watch) = watches.remove(&state.project_id)
    {
        watch.task.abort();
    }
}

/// Stop every stream's event pump. Dropping the map alone would leave the tasks
/// running with a sender nobody reads.
fn abort_pumps(streams: &mut HashMap<String, StreamState>) {
    for (_, state) in streams.drain() {
        state.pump.abort();
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle<F>(
    bridge: &Arc<AgentBridge>,
    streams: &mut HashMap<String, StreamState>,
    watches: &mut HashMap<String, ProjectWatch>,
    outbox: &mpsc::UnboundedSender<AgentToRelay>,
    runtime_id: &str,
    approval_timeout: std::time::Duration,
    now: &Arc<F>,
    frame: RelayToAgent,
) where
    F: Fn() -> String + Send + Sync + 'static,
{
    match frame {
        RelayToAgent::OpenStream {
            stream_id,
            device_id,
            project_id,
            pairing_scope,
            ..
        } => {
            // Bind the stream to a project up front. Refusing here — rather than
            // guessing — is what keeps a two-project host from quietly sending
            // one repository's work to the other.
            let project_id = match bridge.resolve_project(project_id.as_deref()).await {
                Ok(project_id) => project_id,
                Err(error) => {
                    let _ = outbox.send(AgentToRelay::StreamRejected {
                        stream_id,
                        code: error.code().to_string(),
                    });
                    return;
                }
            };

            // Accepting only means "this stream exists". Whether the device is
            // trusted is decided per frame against the local store, so a stream
            // opened for a device this host never paired with simply produces
            // refusals.
            // Only an interactive stream counts as somebody who could answer an
            // approval. An observe pairing cannot decide anything, so treating
            // it as a waiter would arm a countdown nobody could stop.
            let presence = if pairing_scope == PairingScope::Interactive {
                if !watches.contains_key(&project_id) {
                    match bridge.runtime_for_project(&project_id).await {
                        Ok(runtime) => {
                            let presence = RemotePresence::default();
                            let task = tokio::spawn(watch_approvals(
                                runtime,
                                project_id.clone(),
                                device_id.clone(),
                                presence.clone(),
                                approval_timeout,
                                WAITER_POLL,
                                bridge.audit_log(),
                            ));
                            watches.insert(project_id.clone(), ProjectWatch { presence, task });
                        }
                        // No runtime to watch: the stream still opens, and every
                        // frame on it reports the same failure with a code.
                        Err(error) => tracing::warn!(
                            code = error.code(),
                            %project_id,
                            "no approval watch for this project"
                        ),
                    }
                }
                watches.get(&project_id).map(|watch| {
                    watch.presence.attach();
                    watch.presence.clone()
                })
            } else {
                None
            };

            let next_seq = Arc::new(Mutex::new(1));
            let pump = tokio::spawn(pump_events(
                bridge.clone(),
                outbox.clone(),
                runtime_id.to_string(),
                device_id.clone(),
                stream_id.clone(),
                project_id.clone(),
                next_seq.clone(),
                now.clone(),
            ));
            if let Some(previous) = streams.insert(
                stream_id.clone(),
                StreamState {
                    device_id,
                    project_id,
                    next_seq,
                    pump,
                    presence,
                },
            ) {
                release_stream(watches, &previous);
            }
            if let Some(state) = streams.get(&stream_id) {
                bridge.audit(AuditEvent::StreamOpened {
                    device: hashed(&state.device_id),
                    project: state.project_id.clone(),
                });
            }
            let _ = outbox.send(AgentToRelay::StreamAccepted { stream_id });
        }

        RelayToAgent::CloseStream { stream_id, reason } => {
            if let Some(state) = streams.remove(&stream_id) {
                bridge.audit(AuditEvent::StreamClosed {
                    device: hashed(&state.device_id),
                    project: state.project_id.clone(),
                    reason,
                });
                release_stream(watches, &state);
            }
        }

        RelayToAgent::ForwardUpstream { stream_id, frame } => {
            let timestamp = now();
            // A frame for a stream this agent never opened has no project and no
            // signing seq; there is nothing to answer on.
            let Some(project_id) = streams.get(&stream_id).map(|s| s.project_id.clone()) else {
                tracing::warn!(%stream_id, "upstream frame for an unknown stream");
                return;
            };
            // Which session the phone is on, for the approval timeout to answer
            // in. Read before admission on purpose: the id is only a routing
            // hint, and a frame that fails policy still tells us what the user
            // is looking at.
            if let Some(state) = streams.get(&stream_id)
                && let Some(presence) = &state.presence
                && let Some(session_id) = session_of(&frame)
            {
                presence.note_session(&session_id);
            }
            // `None`: the relay is never asked for a key, so it cannot supply
            // one to be compared against — let alone used.
            let outcome = bridge
                .admit_upstream(&project_id, &frame, &timestamp, None)
                .await;

            let downstream = match outcome {
                Ok(Admitted::Delivered { command_id, .. }) => {
                    session_json(&serde_json::json!({"type": "ack", "command_id": command_id}))
                }
                Ok(Admitted::SnapshotRequested { session_id }) => {
                    match bridge.snapshot(&project_id, &session_id).await {
                        Ok(snapshot) => session_json(&serde_json::json!({
                            "type": "snapshot",
                            "session": snapshot
                        })),
                        Err(error) => error_json(&error, None),
                    }
                }
                Err(error) => {
                    // Frames refused before policy — bad signature, unknown
                    // device, unroutable project — have no command kind to
                    // record, only the check that stopped them.
                    if !matches!(error, AdmissionError::Refused { .. }) {
                        bridge.audit(AuditEvent::Refused {
                            device: hashed(&frame.sender_id),
                            project: project_id.clone(),
                            command: None,
                            code: error.code().to_string(),
                        });
                    }
                    // Report the refusal to the device rather than dropping the
                    // frame: a phone that never hears back cannot tell a denied
                    // command from a lost one.
                    error_json(&error, command_id_of(&frame))
                }
            };

            if let Some(state) = streams.get(&stream_id) {
                send_downstream(
                    bridge,
                    outbox,
                    runtime_id,
                    &state.device_id,
                    &stream_id,
                    state.take_seq(),
                    &timestamp,
                    downstream.as_bytes(),
                );
            }
        }

        RelayToAgent::RpcRequest { rpc_id, envelope } => {
            let timestamp = now();
            let method = rpc_method_of(&envelope);
            let device = hashed(&envelope.sender_id);
            match bridge.handle_rpc(&envelope, &timestamp, None).await {
                Ok(signed) => {
                    bridge.audit(AuditEvent::Rpc {
                        device,
                        project: rpc_project_of(&envelope),
                        method,
                        result: "ok".to_string(),
                    });
                    let _ = outbox.send(AgentToRelay::RpcResponse {
                        rpc_id,
                        envelope: Some(signed),
                        error: None,
                    });
                }
                Err(error) => {
                    bridge.audit(AuditEvent::Rpc {
                        device,
                        project: rpc_project_of(&envelope),
                        method,
                        result: error.code().to_string(),
                    });
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

/// Percent-encode the few characters standard base64 contributes to a query.
/// Percent-encode a query value.
///
/// Everything outside the unreserved set is escaped, not just the few
/// characters base64 contributes. A display name is whatever the user typed —
/// "我的 Mac" is an ordinary answer to "what is this machine called" — and a
/// space alone makes the URI invalid, so the agent fails to connect at all with
/// a message about the *URI* that says nothing about the name.
fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
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

/// The RPC method named in a frame, for the audit line. Read before
/// verification, so it is a label rather than a claim: an unverifiable frame is
/// recorded as `unknown` and refused anyway.
fn rpc_method_of(frame: &SignedEnvelope) -> String {
    rpc_field(frame, "method").unwrap_or_else(|| "unknown".to_string())
}

/// The project an RPC named, for the audit line.
fn rpc_project_of(frame: &SignedEnvelope) -> String {
    rpc_field(frame, "project_id").unwrap_or_else(|| "-".to_string())
}

fn rpc_field(frame: &SignedEnvelope, field: &str) -> Option<String> {
    let payload = frame.payload().ok()?;
    let value: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    value
        .get(field)
        .and_then(|found| found.as_str())
        .map(|found| found.to_string())
}

/// Which session an upstream frame targets, for the approval timeout. Read from
/// an unverified payload, so it is treated as a hint and never as authority:
/// the runtime binds each approval to its own session and refuses a decision
/// aimed at another.
fn session_of(frame: &SignedEnvelope) -> Option<String> {
    let payload = frame.payload().ok()?;
    let value: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    value
        .get("session_id")
        .and_then(|id| id.as_str())
        .map(|id| id.to_string())
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
