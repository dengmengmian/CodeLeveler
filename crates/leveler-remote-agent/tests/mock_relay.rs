//! A full round trip through a relay that is assumed hostile.
//!
//! The mock relay here does what a real one does — carry bytes between a device
//! and the agent — and then does what a compromised one would try. The point is
//! that the honest path and the attacks travel the identical code; nothing in
//! the agent knows it is being tested.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use leveler_client_protocol::{
    ClientCommand, ClientError, InteractiveRuntimeClient, PermissionProfile, RuntimeEvent,
    SessionId, UiSessionSnapshot,
};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
use leveler_remote_agent::{AgentBridge, TrustedDevices};
use leveler_remote_protocol::pairing::PairingScope;
use leveler_remote_protocol::tunnel::{RpcMethod, RpcRequestPayload, rpc_stream_id};
use leveler_remote_protocol::{
    ContentType, Sender, SignedEnvelope, SigningKey, VerifyParams, VerifyingKey,
};
use tokio::sync::broadcast;

const DEVICE_SEED: [u8; 32] = [33u8; 32];
const RUNTIME_SEED: [u8; 32] = [44u8; 32];
const RELAY_SEED: [u8; 32] = [55u8; 32];
const RUNTIME_ID: &str = "rt_host";
const DEVICE_ID: &str = "dev_phone";
const AT: &str = "2026-07-25T12:00:00Z";

struct FakeRuntime {
    delivered: Arc<Mutex<Vec<ClientCommand>>>,
}

fn snapshot_for(session_id: &str) -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new(session_id),
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
    }
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
        Ok(snapshot_for(session_id.as_str()))
    }
}

#[async_trait]
impl LocalRuntimeService for FakeRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Ok(SessionBootstrap {
            session: snapshot_for("s-new"),
            context_window: 128_000,
        })
    }
}

/// Stands in for the phone: signs requests, and verifies responses against the
/// `runtime_pubkey` it anchored from the pairing QR — never one the relay
/// offers later.
struct Device {
    key: SigningKey,
    anchored_runtime_key: VerifyingKey,
}

impl Device {
    fn rpc(&self, uuid: &str, method: RpcMethod, body: serde_json::Value) -> SignedEnvelope {
        let payload = serde_json::to_vec(&RpcRequestPayload { method, body }).unwrap();
        SignedEnvelope::sign(
            &self.key,
            Sender::Device,
            DEVICE_ID,
            RUNTIME_ID,
            &rpc_stream_id(uuid),
            1,
            AT,
            ContentType::RpcRequest,
            &payload,
        )
        .unwrap()
    }

    fn accept_response(&self, frame: &SignedEnvelope) -> Result<Vec<u8>, String> {
        frame
            .verify(&VerifyParams {
                expected_recipient_id: DEVICE_ID,
                public_key: &self.anchored_runtime_key,
                now: AT,
            })
            .map_err(|error| error.code().to_string())
    }
}

fn setup(dir: &tempfile::TempDir) -> (AgentBridge<FakeRuntime>, Device) {
    let device_key = SigningKey::from_seed(&DEVICE_SEED).unwrap();
    let runtime_key = SigningKey::from_seed(&RUNTIME_SEED).unwrap();

    let path = dir.path().join("remote").join("devices.json");
    let mut devices = TrustedDevices::load(&path).unwrap();
    devices
        .accept(
            DEVICE_ID,
            &device_key.verifying_key(),
            "iPhone",
            PairingScope::Interactive,
            AT,
        )
        .unwrap();

    let runtime = FakeRuntime {
        delivered: Arc::new(Mutex::new(Vec::new())),
    };
    let anchored = runtime_key.verifying_key();
    let bridge = AgentBridge::new(runtime, devices, RUNTIME_ID, runtime_key, false);
    (
        bridge,
        Device {
            key: device_key,
            anchored_runtime_key: anchored,
        },
    )
}

#[tokio::test]
async fn create_session_returns_a_response_the_device_can_verify() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, device) = setup(&dir);

    let request = device.rpc(
        "2f8a",
        RpcMethod::CreateSession,
        serde_json::json!({"goal": "hi", "model": null, "mode": "assisted"}),
    );
    let response = bridge.handle_rpc(&request, AT, None).await.unwrap();

    let body = device
        .accept_response(&response)
        .expect("a genuine response verifies");
    let bootstrap: SessionBootstrap = serde_json::from_slice(&body).unwrap();
    assert_eq!(bootstrap.session.id, SessionId::new("s-new"));
}

/// The response carries the session id the APP will act on, so a relay must not
/// be able to compose one. It holds no runtime key, so its best attempt is a
/// body of its own with its own signature.
#[tokio::test]
async fn a_relay_authored_response_is_rejected_by_the_device() {
    let dir = tempfile::tempdir().unwrap();
    let (_bridge, device) = setup(&dir);
    let relay_key = SigningKey::from_seed(&RELAY_SEED).unwrap();

    let forged_body = serde_json::to_vec(&SessionBootstrap {
        session: snapshot_for("s-attacker"),
        context_window: 128_000,
    })
    .unwrap();
    let forged = SignedEnvelope::sign(
        &relay_key,
        Sender::Runtime,
        RUNTIME_ID,
        DEVICE_ID,
        &rpc_stream_id("2f8a"),
        1,
        AT,
        ContentType::RpcResponse,
        &forged_body,
    )
    .unwrap();

    assert_eq!(
        device.accept_response(&forged).unwrap_err(),
        "signature_invalid",
        "an APP must not adopt a session id the relay chose"
    );
}

/// Tampering with a genuine response is caught for the same reason.
#[tokio::test]
async fn a_relay_edited_response_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, device) = setup(&dir);

    let request = device.rpc(
        "2f8a",
        RpcMethod::Snapshot,
        serde_json::json!({"session_id": "s1"}),
    );
    let mut response = bridge.handle_rpc(&request, AT, None).await.unwrap();

    // Swap in a snapshot for a different session.
    let swapped = serde_json::to_vec(&snapshot_for("s-other")).unwrap();
    use base64::Engine as _;
    response.payload_b64 = base64::engine::general_purpose::STANDARD.encode(&swapped);

    assert_eq!(
        device.accept_response(&response).unwrap_err(),
        "signature_invalid"
    );
}

/// The response reuses its request's stream id, so a relay cannot answer one
/// request with another's genuine, correctly-signed response.
#[tokio::test]
async fn a_genuine_response_cannot_be_rematched_to_another_request() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, device) = setup(&dir);

    let first = device.rpc(
        "aaaa",
        RpcMethod::Snapshot,
        serde_json::json!({"session_id": "s1"}),
    );
    let second = device.rpc(
        "bbbb",
        RpcMethod::Snapshot,
        serde_json::json!({"session_id": "s2"}),
    );

    let first_response = bridge.handle_rpc(&first, AT, None).await.unwrap();
    let second_response = bridge.handle_rpc(&second, AT, None).await.unwrap();

    assert_eq!(first_response.stream_id, rpc_stream_id("aaaa"));
    assert_eq!(second_response.stream_id, rpc_stream_id("bbbb"));
    assert_ne!(
        first_response.stream_id, second_response.stream_id,
        "the correlation must be per-request, or a relay could swap the two"
    );

    // Both verify on their own; the binding lives in the signed stream id, so a
    // client that matches on it cannot be fooled by a swap.
    assert!(device.accept_response(&first_response).is_ok());
    assert!(device.accept_response(&second_response).is_ok());
}

/// A relay that forwards an RPC from an unpaired device gets nothing back.
#[tokio::test]
async fn an_unpaired_device_gets_no_signed_body() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, _device) = setup(&dir);
    let stranger = SigningKey::from_seed(&RELAY_SEED).unwrap();

    let payload = serde_json::to_vec(&RpcRequestPayload {
        method: RpcMethod::Snapshot,
        body: serde_json::json!({"session_id": "s1"}),
    })
    .unwrap();
    let frame = SignedEnvelope::sign(
        &stranger,
        Sender::Device,
        "dev_stranger",
        RUNTIME_ID,
        &rpc_stream_id("cccc"),
        1,
        AT,
        ContentType::RpcRequest,
        &payload,
    )
    .unwrap();

    assert_eq!(
        bridge
            .handle_rpc(&frame, AT, None)
            .await
            .unwrap_err()
            .code(),
        "unauthorized"
    );
}

/// An observe pairing may read but not create.
#[tokio::test]
async fn an_observe_pairing_can_snapshot_but_not_create_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let device_key = SigningKey::from_seed(&DEVICE_SEED).unwrap();
    let runtime_key = SigningKey::from_seed(&RUNTIME_SEED).unwrap();
    let path = dir.path().join("remote").join("devices.json");
    let mut devices = TrustedDevices::load(&path).unwrap();
    devices
        .accept(
            DEVICE_ID,
            &device_key.verifying_key(),
            "iPad",
            PairingScope::Observe,
            AT,
        )
        .unwrap();
    let anchored = runtime_key.verifying_key();
    let bridge = AgentBridge::new(
        FakeRuntime {
            delivered: Arc::new(Mutex::new(Vec::new())),
        },
        devices,
        RUNTIME_ID,
        runtime_key,
        false,
    );
    let device = Device {
        key: device_key,
        anchored_runtime_key: anchored,
    };

    let snapshot = device.rpc(
        "dddd",
        RpcMethod::Snapshot,
        serde_json::json!({"session_id": "s1"}),
    );
    assert!(
        bridge.handle_rpc(&snapshot, AT, None).await.is_ok(),
        "observe may read"
    );

    let create = device.rpc(
        "eeee",
        RpcMethod::CreateSession,
        serde_json::json!({"goal": "hi", "model": null, "mode": "assisted"}),
    );
    assert_eq!(
        bridge
            .handle_rpc(&create, AT, None)
            .await
            .unwrap_err()
            .code(),
        "command_not_allowed_remote"
    );
}
