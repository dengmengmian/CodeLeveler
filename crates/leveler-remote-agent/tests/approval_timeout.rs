//! The deny-on-timeout rule, driven end to end against a runtime.
//!
//! `leveler-remote-protocol` already tests the state machine's transitions. What
//! is tested here is the part that can silently do nothing: a watcher that never
//! polls, never arms, or denies in the wrong circumstances. Every case asserts
//! what the runtime *received*, because "the timer fired" is not the claim — "an
//! approval was denied" is.
//!
//! Time is paused, so a 120-second window costs no wall clock and the results do
//! not depend on scheduling luck.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use leveler_client_protocol::{
    ApprovalId, ClientCommand, ClientError, InteractiveRuntimeClient, PermissionProfile,
    RuntimeEvent, SessionId, UiApprovalRequest, UiSessionSnapshot,
};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
use leveler_remote_agent::{AgentBridge, SingleProject, TrustedDevices, run_tunnel};
use leveler_remote_protocol::pairing::PairingScope;
use leveler_remote_protocol::{ContentType, Sender, SignedEnvelope, SigningKey};
use tokio::sync::broadcast;

const DEVICE_SEED: [u8; 32] = [91u8; 32];
const RUNTIME_SEED: [u8; 32] = [92u8; 32];
const RUNTIME_ID: &str = "rt_host";
const DEVICE_ID: &str = "dev_phone";
const PROJECT_ID: &str = "0123456789abcdef";
const SESSION_ID: &str = "s1";
const APPROVAL_ID: &str = "a1";
const TIMEOUT: Duration = Duration::from_secs(120);

/// A runtime whose local-waiter count a test can move, standing in for someone
/// opening or closing a terminal on the developer's machine.
struct WatchedRuntime {
    delivered: Arc<Mutex<Vec<ClientCommand>>>,
    events: broadcast::Sender<RuntimeEvent>,
    local_waiters: Arc<AtomicUsize>,
}

impl WatchedRuntime {
    fn new(local_waiters: usize) -> Self {
        Self {
            delivered: Arc::new(Mutex::new(Vec::new())),
            events: broadcast::channel(64).0,
            local_waiters: Arc::new(AtomicUsize::new(local_waiters)),
        }
    }
}

#[async_trait]
impl InteractiveRuntimeClient for WatchedRuntime {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        self.delivered.lock().unwrap().push(command);
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        Ok(UiSessionSnapshot {
            id: session_id.clone(),
            repository: "/repo".to_string(),
            goal: String::new(),
            model: None,
            mode: PermissionProfile::RequestApproval,
            branch: None,
            status: "idle".to_string(),
            messages: Vec::new(),
            pending_interactions: Vec::new(),
            available_models: Vec::new(),
            vision: false,
            last_sequence: None,
            active_tools: Vec::new(),
            plan: None,
            verification: None,
            diff: None,
            checkpoints: Vec::new(),
            user_shells: Vec::new(),
            completion_report: None,
        })
    }
}

#[async_trait]
impl LocalRuntimeService for WatchedRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Err(ClientError::Runtime("not used here".into()))
    }

    async fn local_waiter_count(&self) -> Result<usize, ClientError> {
        Ok(self.local_waiters.load(Ordering::SeqCst))
    }
}

/// A running agent tunnel against a mock relay, with one device stream open.
struct Host {
    delivered: Arc<Mutex<Vec<ClientCommand>>>,
    events: broadcast::Sender<RuntimeEvent>,
    local_waiters: Arc<AtomicUsize>,
    /// Frames the mock relay will send the agent — used to close the device's
    /// stream the way a phone disconnecting does.
    to_agent: tokio::sync::mpsc::UnboundedSender<leveler_remote_protocol::tunnel::RelayToAgent>,
    _dir: tempfile::TempDir,
}

/// Stand up a relay, an agent, and one paired device holding an open stream.
///
/// The relay is the real `leveler-relay` router; only the runtime is a stand-in,
/// because the assertions are about what did and did not reach it.
async fn host_with_stream(local_waiters: usize, scope: PairingScope) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let runtime = WatchedRuntime::new(local_waiters);
    let delivered = runtime.delivered.clone();
    let events = runtime.events.clone();
    let waiters = runtime.local_waiters.clone();

    let device_key = SigningKey::from_seed(&DEVICE_SEED).unwrap();
    let runtime_key = SigningKey::from_seed(&RUNTIME_SEED).unwrap();
    let mut devices = TrustedDevices::load(dir.path().join("remote/devices.json")).unwrap();
    devices
        .accept(
            DEVICE_ID,
            &device_key.verifying_key(),
            "iPhone",
            scope,
            &now_stamp(),
        )
        .unwrap();

    let bridge = Arc::new(AgentBridge::new(
        Arc::new(SingleProject::new(PROJECT_ID, "repo", Arc::new(runtime))),
        devices,
        RUNTIME_ID,
        runtime_key,
        false,
    ));

    let to_agent = mock_relay::spawn(bridge, scope).await;

    Host {
        delivered,
        events,
        local_waiters: waiters,
        to_agent,
        _dir: dir,
    }
}

fn now_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// A relay that does the two things this test needs: open one stream for the
/// device, and carry its frames. Smaller than the real one, and enough — the
/// property under test is on the agent's side.
mod mock_relay {
    use super::*;
    use futures_util::{SinkExt as _, StreamExt as _};
    use leveler_remote_protocol::tunnel::{AgentToRelay, RelayToAgent};

    pub(super) async fn spawn(
        bridge: Arc<AgentBridge>,
        scope: PairingScope,
    ) -> tokio::sync::mpsc::UnboundedSender<RelayToAgent> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (to_agent, inbox) = tokio::sync::mpsc::unbounded_channel::<RelayToAgent>();
        // One connection only, so the receiver is handed to whichever upgrade
        // arrives first.
        let inbox = Arc::new(Mutex::new(Some(inbox)));

        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/v1/agent/tunnel",
                axum::routing::get(move |upgrade: axum::extract::WebSocketUpgrade| async move {
                    let inbox = inbox.lock().unwrap().take();
                    upgrade.on_upgrade(move |socket| serve(socket, scope, inbox))
                }),
            );
            let _ = axum::serve(listener, app).await;
        });

        let ws_base = format!("ws://{address}");
        tokio::spawn(async move {
            let _ = run_tunnel(&ws_base, RUNTIME_ID, "dev-box", bridge, TIMEOUT, now_stamp).await;
        });
        to_agent
    }

    /// Open one stream, then keep the socket alive so the agent's pumps run.
    async fn serve(
        socket: axum::extract::ws::WebSocket,
        scope: PairingScope,
        inbox: Option<tokio::sync::mpsc::UnboundedReceiver<RelayToAgent>>,
    ) {
        use axum::extract::ws::Message;
        let (mut sink, mut incoming) = socket.split();
        let open = RelayToAgent::OpenStream {
            stream_id: "str_1".to_string(),
            device_id: DEVICE_ID.to_string(),
            pairing_scope: scope,
            access_jti: "jti_1".to_string(),
            project_id: Some(PROJECT_ID.to_string()),
        };
        if sink
            .send(Message::Text(serde_json::to_string(&open).unwrap().into()))
            .await
            .is_err()
        {
            return;
        }

        // Tell the agent which session the phone is on, the way a real frame
        // does: a snapshot request for it.
        let frame = SignedEnvelope::sign(
            &SigningKey::from_seed(&DEVICE_SEED).unwrap(),
            Sender::Device,
            DEVICE_ID,
            RUNTIME_ID,
            "str_1",
            1,
            &now_stamp(),
            ContentType::SessionUpstream,
            serde_json::json!({"type": "snapshot", "session_id": SESSION_ID})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        let forward = RelayToAgent::ForwardUpstream {
            stream_id: "str_1".to_string(),
            frame,
        };
        if sink
            .send(Message::Text(
                serde_json::to_string(&forward).unwrap().into(),
            ))
            .await
            .is_err()
        {
            return;
        }

        let mut inbox = inbox.expect("one connection per test");
        loop {
            tokio::select! {
                queued = inbox.recv() => match queued {
                    Some(frame) => {
                        let text = serde_json::to_string(&frame).unwrap();
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            return;
                        }
                    }
                    None => return,
                },
                received = incoming.next() => match received {
                    // Drain, so the agent's outbox never blocks.
                    Some(Ok(Message::Text(text))) => {
                        let _ = serde_json::from_str::<AgentToRelay>(&text);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return,
                },
            }
        }
    }
}

impl Host {
    /// Raise an approval on the runtime and let the watcher notice it.
    async fn raise_approval(&self) {
        // The watch subscribes when the stream opens; wait for it, since a
        // broadcast only reaches receivers that already exist.
        wait_for(|| self.events.receiver_count() >= 2).await;
        self.events
            .send(RuntimeEvent::ApprovalRequested {
                request: UiApprovalRequest {
                    id: ApprovalId::new(APPROVAL_ID),
                    tool: "run_command".to_string(),
                    summary: "rm -rf /tmp/x".to_string(),
                    command: Some("rm -rf /tmp/x".to_string()),
                    risks: Vec::new(),
                },
            })
            .unwrap();
        settle().await;
    }

    fn denials(&self) -> usize {
        self.delivered
            .lock()
            .unwrap()
            .iter()
            .filter(|command| {
                matches!(
                    command,
                    ClientCommand::ApprovalDecision {
                        decision: leveler_client_protocol::ApprovalDecision::Deny,
                        ..
                    }
                )
            })
            .count()
    }
}

/// Let paused time move past the window, then let the watcher act on it.
///
/// Two steps, because that is how the watcher works: one poll notices who is
/// attached and arms the countdown, and a later one finds it due. Under real
/// time those are a second apart and invisible.
async fn advance_past_timeout() {
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    tokio::time::advance(TIMEOUT + Duration::from_secs(5)).await;
    settle().await;
}

/// Yield enough for the watcher's poll and delivery to run.
async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

/// Poll a condition under paused time.
async fn wait_for(mut condition: impl FnMut() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
    }
    panic!("condition never became true");
}

/// Nobody local can answer: the window expires and the host denies.
#[tokio::test(start_paused = true)]
async fn a_remote_only_approval_is_denied_when_it_times_out() {
    let host = host_with_stream(0, PairingScope::Interactive).await;
    host.raise_approval().await;

    assert_eq!(host.denials(), 0, "not before the window elapses");
    advance_past_timeout().await;
    assert_eq!(host.denials(), 1);

    let delivered = host.delivered.lock().unwrap();
    let ClientCommand::ApprovalDecision { request_id, .. } = &delivered[0] else {
        panic!("expected an approval decision, got {:?}", delivered[0]);
    };
    assert_eq!(request_id.as_str(), APPROVAL_ID);
}

/// Someone is at the keyboard: this is not a remote decision, and a phone's
/// policy must not answer it.
#[tokio::test(start_paused = true)]
async fn a_local_ui_keeps_the_approval_alive() {
    let host = host_with_stream(1, PairingScope::Interactive).await;
    host.raise_approval().await;

    advance_past_timeout().await;
    assert_eq!(
        host.denials(),
        0,
        "a prompt someone can still read must not expire"
    );
}

/// Both attached is still "not a remote decision": the person at the desk is
/// not on a clock set by a phone.
#[tokio::test(start_paused = true)]
async fn both_attached_means_no_countdown() {
    let host = host_with_stream(2, PairingScope::Interactive).await;
    host.raise_approval().await;

    advance_past_timeout().await;
    assert_eq!(host.denials(), 0);
}

/// The person closes their terminal while the prompt is up. Only now does the
/// countdown start — and it starts fresh, so a prompt that waited an hour still
/// gets its full window.
#[tokio::test(start_paused = true)]
async fn the_countdown_starts_when_the_last_local_ui_leaves() {
    let host = host_with_stream(1, PairingScope::Interactive).await;
    host.raise_approval().await;

    // An hour with someone watching: nothing expires.
    tokio::time::advance(Duration::from_secs(3600)).await;
    settle().await;
    assert_eq!(host.denials(), 0);

    host.local_waiters.store(0, Ordering::SeqCst);
    settle().await;

    // Most of a window, but not all of it.
    tokio::time::advance(TIMEOUT - Duration::from_secs(10)).await;
    settle().await;
    assert_eq!(
        host.denials(),
        0,
        "the window must run from the moment they left, not from the prompt"
    );

    advance_past_timeout().await;
    assert_eq!(host.denials(), 1);
}

/// A person answered. The countdown must not fire a second decision on top of
/// one somebody already made.
#[tokio::test(start_paused = true)]
async fn a_resolved_approval_cancels_its_countdown() {
    let host = host_with_stream(0, PairingScope::Interactive).await;
    host.raise_approval().await;

    host.events
        .send(RuntimeEvent::ApprovalResolved {
            id: ApprovalId::new(APPROVAL_ID),
        })
        .unwrap();
    settle().await;

    advance_past_timeout().await;
    assert_eq!(host.denials(), 0);
}

/// An observe pairing cannot answer an approval, so it is not a waiter. Treating
/// it as one would arm a countdown nobody could stop; counting it as somebody
/// who can decide would be worse still.
#[tokio::test(start_paused = true)]
async fn an_observe_stream_does_not_arm_the_countdown() {
    let host = host_with_stream(0, PairingScope::Observe).await;
    // No interactive stream exists, so nothing subscribes for approvals; the
    // event pump is the only subscriber.
    wait_for(|| host.events.receiver_count() >= 1).await;
    host.events
        .send(RuntimeEvent::ApprovalRequested {
            request: UiApprovalRequest {
                id: ApprovalId::new(APPROVAL_ID),
                tool: "run_command".to_string(),
                summary: "rm -rf /tmp/x".to_string(),
                command: None,
                risks: Vec::new(),
            },
        })
        .unwrap();
    settle().await;

    advance_past_timeout().await;
    assert_eq!(host.denials(), 0);
}

/// The phone disconnects while a prompt is up. With no remote stream left there
/// is no remote-only approval to expire, and the desktop keeps the prompt.
#[tokio::test(start_paused = true)]
async fn a_departed_phone_leaves_nothing_to_expire() {
    let host = host_with_stream(0, PairingScope::Interactive).await;
    host.raise_approval().await;

    // The phone hangs up: the relay closes its stream.
    host.to_agent
        .send(leveler_remote_protocol::tunnel::RelayToAgent::CloseStream {
            stream_id: "str_1".to_string(),
            reason: "app_gone".to_string(),
        })
        .unwrap();
    settle().await;

    advance_past_timeout().await;
    assert_eq!(
        host.denials(),
        0,
        "with nobody remote waiting, nothing should be auto-denied"
    );
}
