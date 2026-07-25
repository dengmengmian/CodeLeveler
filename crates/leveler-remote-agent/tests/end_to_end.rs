//! The whole chain, for real: a phone, a relay process, and an agent.
//!
//! Everything before this has tested one half against a stand-in for the other.
//! Here the relay is the actual `leveler-relay` router, the agent is the actual
//! tunnel client, and the phone signs with a real Ed25519 key. Only the runtime
//! is a fake, because a fake runtime is the only way to assert what did and did
//! not reach it.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::{SinkExt as _, StreamExt as _};
use leveler_client_protocol::{
    ApprovalDecision, ApprovalId, ClientCommand, ClientError, InteractiveRuntimeClient,
    PermissionProfile, RuntimeEvent, SessionId, UiSessionSnapshot,
};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
use leveler_relay::{RelayState, build_router};
use leveler_remote_agent::{AgentBridge, TrustedDevices, run_tunnel};
use leveler_remote_protocol::pairing::PairingScope;
use leveler_remote_protocol::{
    ContentType, Sender, SignedEnvelope, SigningKey, VerifyParams, VerifyingKey,
};
use serde_json::json;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

const DEVICE_SEED: [u8; 32] = [61u8; 32];
const RUNTIME_SEED: [u8; 32] = [62u8; 32];
const RUNTIME_ID: &str = "rt_host";
const DEVICE_ID: &str = "dev_phone";
/// Pinned so signatures are deterministic and inside the verifier's window.
const AT: &str = "2026-07-25T12:00:00Z";

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct FakeRuntime {
    delivered: Arc<Mutex<Vec<ClientCommand>>>,
}

#[async_trait]
impl InteractiveRuntimeClient for FakeRuntime {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        self.delivered.lock().unwrap().push(command);
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        let (_sender, receiver) = broadcast::channel(1);
        receiver
    }

    async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        Ok(UiSessionSnapshot {
            id: session_id.clone(),
            repository: "/repo".to_string(),
            goal: "interactive session".to_string(),
            model: None,
            mode: PermissionProfile::Assisted,
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
            completion_report: None,
        })
    }
}

#[async_trait]
impl LocalRuntimeService for FakeRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Err(ClientError::Runtime("not used here".into()))
    }
}

/// A relay, an agent connected to it, and a paired device's token.
struct Chain {
    base: String,
    token: String,
    delivered: Arc<Mutex<Vec<ClientCommand>>>,
    runtime_key: VerifyingKey,
    _relay_state: RelayState,
}

async fn build_chain(dir: &tempfile::TempDir, scope: PairingScope) -> Chain {
    // The relay, as a real process would run it.
    let state = RelayState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = build_router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let base = address.to_string();

    // Pair the phone through the relay's real endpoints.
    let device_key = SigningKey::from_seed(&DEVICE_SEED).unwrap();
    let client = reqwest::Client::new();
    let http = format!("http://{base}");
    let begin: serde_json::Value = client
        .post(format!("{http}/v1/pair/begin"))
        .json(&json!({"runtime_id": RUNTIME_ID}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let complete: serde_json::Value = client
        .post(format!("{http}/v1/pair/complete"))
        .json(&json!({
            "device_id": DEVICE_ID,
            "device_pubkey": device_key.verifying_key().to_base64url(),
            "device_name": "iPhone",
            "platform": "ios",
            "pairing_secret": begin["pairing_secret"].as_str().unwrap(),
            "scope": scope,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    client
        .post(format!("{http}/v1/pair/confirm"))
        .json(&json!({
            "runtime_id": RUNTIME_ID,
            "pairing_id": complete["pairing_id"].as_str().unwrap(),
            "decision": "accept"
        }))
        .send()
        .await
        .unwrap();
    let auth: serde_json::Value = client
        .post(format!("{http}/v1/auth/session"))
        .json(&json!({"device_id": DEVICE_ID, "runtime_id": RUNTIME_ID}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = auth["access_token"].as_str().unwrap().to_string();

    // The agent's own store — separate from the relay's copy, and the only key
    // source it will consult.
    let runtime_key = SigningKey::from_seed(&RUNTIME_SEED).unwrap();
    let public_runtime_key = runtime_key.verifying_key();
    let mut devices = TrustedDevices::load(dir.path().join("remote/devices.json")).unwrap();
    devices
        .accept(DEVICE_ID, &device_key.verifying_key(), "iPhone", scope, AT)
        .unwrap();

    let runtime = FakeRuntime {
        delivered: Arc::new(Mutex::new(Vec::new())),
    };
    let delivered = runtime.delivered.clone();
    let bridge = Arc::new(AgentBridge::new(
        runtime,
        devices,
        RUNTIME_ID,
        runtime_key,
        false,
    ));

    let ws_base = format!("ws://{base}");
    tokio::spawn(async move {
        let _ = run_tunnel(&ws_base, RUNTIME_ID, "dev-box", bridge, || AT.to_string()).await;
    });

    // Wait for the tunnel to register before a device tries to connect.
    for _ in 0..100 {
        if state.is_online(RUNTIME_ID) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        state.is_online(RUNTIME_ID),
        "the agent should have registered"
    );

    Chain {
        base,
        token,
        delivered,
        runtime_key: public_runtime_key,
        _relay_state: state,
    }
}

async fn connect_app(chain: &Chain) -> Socket {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let mut request = format!("ws://{}/v1/hosts/{RUNTIME_ID}/session", chain.base)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", chain.token).parse().unwrap(),
    );
    let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket
}

/// Sign as the phone does, then send over the relay.
async fn send_command(app: &mut Socket, command: &ClientCommand) {
    let body = json!({
        "type": "deliver",
        "command_id": "cmd-1",
        "session_id": "s1",
        "command": command
    })
    .to_string();
    let frame = SignedEnvelope::sign(
        &SigningKey::from_seed(&DEVICE_SEED).unwrap(),
        Sender::Device,
        DEVICE_ID,
        RUNTIME_ID,
        "str_app",
        1,
        AT,
        ContentType::SessionUpstream,
        body.as_bytes(),
    )
    .unwrap();
    app.send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
        .await
        .unwrap();
}

/// Receive one downstream frame and verify it the way a phone would: against the
/// `runtime_pubkey` anchored at pairing, addressed to this device.
async fn recv_verified(app: &mut Socket, chain: &Chain) -> serde_json::Value {
    let message = tokio::time::timeout(std::time::Duration::from_secs(5), app.next())
        .await
        .expect("a downstream frame should arrive")
        .expect("socket open")
        .expect("no socket error");
    let text = match message {
        Message::Text(text) => text,
        other => panic!("expected text, got {other:?}"),
    };
    let envelope: SignedEnvelope = serde_json::from_str(&text).unwrap();
    let payload = envelope
        .verify(&VerifyParams {
            expected_recipient_id: DEVICE_ID,
            public_key: &chain.runtime_key,
            now: AT,
        })
        .expect("the runtime's signature must verify on the device");
    serde_json::from_slice(&payload).unwrap()
}

/// The path the whole design exists to provide: a phone submits a message, it
/// crosses a relay it does not trust, and the runtime on the developer's machine
/// runs it.
#[tokio::test]
async fn a_phone_command_crosses_the_relay_and_reaches_the_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let chain = build_chain(&dir, PairingScope::Interactive).await;
    let mut app = connect_app(&chain).await;

    send_command(
        &mut app,
        &ClientCommand::SubmitMessage {
            session_id: SessionId::new("s1"),
            content: "从手机发来的".to_string(),
            attachments: Vec::new(),
        },
    )
    .await;

    let ack = recv_verified(&mut app, &chain).await;
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["command_id"], "cmd-1");

    let delivered = chain.delivered.lock().unwrap();
    assert_eq!(delivered.len(), 1);
    assert!(matches!(
        &delivered[0],
        ClientCommand::SubmitMessage { content, .. } if content == "从手机发来的"
    ));
}

/// The capability gate still applies at the far end of a real relay: a
/// standing-permission grant is refused and never reaches the runtime.
#[tokio::test]
async fn a_disallowed_command_is_refused_across_the_whole_chain() {
    let dir = tempfile::tempdir().unwrap();
    let chain = build_chain(&dir, PairingScope::Interactive).await;
    let mut app = connect_app(&chain).await;

    send_command(
        &mut app,
        &ClientCommand::ApprovalDecision {
            request_id: ApprovalId::new("a1"),
            decision: ApprovalDecision::ApproveAlways,
        },
    )
    .await;

    let error = recv_verified(&mut app, &chain).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["code"], "approval_decision_not_allowed_remote");
    assert_eq!(
        error["command_id"], "cmd-1",
        "a refusal must be correlatable, or the phone cannot tell denied from lost"
    );
    assert!(
        chain.delivered.lock().unwrap().is_empty(),
        "the executor must never see a remote ApproveAlways"
    );
}

/// A relay that fabricates a frame cannot make the agent act, even though it
/// controls the socket the frame arrives on.
#[tokio::test]
async fn a_relay_fabricated_frame_is_refused_by_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let chain = build_chain(&dir, PairingScope::Interactive).await;
    let mut app = connect_app(&chain).await;

    // Sign with a key the host never accepted — the best a relay can do.
    let impostor = SigningKey::from_seed(&[99u8; 32]).unwrap();
    let body = json!({
        "type": "deliver", "command_id": "cmd-evil", "session_id": "s1",
        "command": {"type": "submit_message", "session_id": "s1", "content": "rm -rf"}
    })
    .to_string();
    let frame = SignedEnvelope::sign(
        &impostor,
        Sender::Device,
        DEVICE_ID,
        RUNTIME_ID,
        "str_app",
        1,
        AT,
        ContentType::SessionUpstream,
        body.as_bytes(),
    )
    .unwrap();
    app.send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
        .await
        .unwrap();

    let error = recv_verified(&mut app, &chain).await;
    assert_eq!(error["code"], "signature_invalid");
    assert!(
        chain.delivered.lock().unwrap().is_empty(),
        "a frame signed by an unaccepted key must not reach the runtime"
    );
}

/// A snapshot request is answered with runtime-signed session state.
#[tokio::test]
async fn a_snapshot_request_returns_signed_session_state() {
    let dir = tempfile::tempdir().unwrap();
    let chain = build_chain(&dir, PairingScope::Interactive).await;
    let mut app = connect_app(&chain).await;

    let body = json!({"type": "snapshot", "session_id": "s1"}).to_string();
    let frame = SignedEnvelope::sign(
        &SigningKey::from_seed(&DEVICE_SEED).unwrap(),
        Sender::Device,
        DEVICE_ID,
        RUNTIME_ID,
        "str_app",
        1,
        AT,
        ContentType::SessionUpstream,
        body.as_bytes(),
    )
    .unwrap();
    app.send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
        .await
        .unwrap();

    let snapshot = recv_verified(&mut app, &chain).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["session"]["id"], "s1");
    assert_eq!(snapshot["session"]["repository"], "/repo");
}

/// An observe pairing reaches the runtime with nothing.
#[tokio::test]
async fn an_observe_pairing_is_refused_across_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let chain = build_chain(&dir, PairingScope::Observe).await;
    let mut app = connect_app(&chain).await;

    send_command(
        &mut app,
        &ClientCommand::SubmitMessage {
            session_id: SessionId::new("s1"),
            content: "hi".to_string(),
            attachments: Vec::new(),
        },
    )
    .await;

    let error = recv_verified(&mut app, &chain).await;
    assert_eq!(error["code"], "command_not_allowed_remote");
    assert!(chain.delivered.lock().unwrap().is_empty());
}
