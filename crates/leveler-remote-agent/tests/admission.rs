//! What a frame from a phone must survive before it becomes a command.
//!
//! Each test names the attacker it stands for. The positive case is one line;
//! the rest are the reasons this layer exists.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use leveler_client_protocol::{
    ApprovalDecision, ApprovalId, ClientCommand, ClientError, InteractiveRuntimeClient,
    PermissionProfile, RuntimeEvent, SessionId, UiSessionSnapshot,
};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
use leveler_remote_agent::{Admitted, AgentBridge, SingleProject, TrustedDevices};
use leveler_remote_protocol::pairing::PairingScope;
use leveler_remote_protocol::{ContentType, Sender, SignedEnvelope, SigningKey};
use tokio::sync::broadcast;

const DEVICE_SEED: [u8; 32] = [11u8; 32];
const RELAY_SEED: [u8; 32] = [22u8; 32];
const RUNTIME_SEED: [u8; 32] = [77u8; 32];
const RUNTIME_ID: &str = "rt_host";
const DEVICE_ID: &str = "dev_phone";
const AT: &str = "2026-07-25T12:00:00Z";
const PROJECT_ID: &str = "0123456789abcdef";

/// Records what actually reached the runtime, so a test can assert that a
/// refused frame delivered *nothing* rather than merely returning an error.
#[derive(Default)]
struct RecordingRuntime {
    delivered: Arc<Mutex<Vec<ClientCommand>>>,
}

#[async_trait]
impl InteractiveRuntimeClient for RecordingRuntime {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        self.delivered.lock().unwrap().push(command);
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        let (_sender, receiver) = broadcast::channel(1);
        receiver
    }

    async fn snapshot(&self, _session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        Err(ClientError::Runtime("not needed for these tests".into()))
    }
}

#[async_trait]
impl LocalRuntimeService for RecordingRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Err(ClientError::Runtime("not needed for these tests".into()))
    }
}

fn device_key() -> SigningKey {
    SigningKey::from_seed(&DEVICE_SEED).expect("seed is valid")
}

fn runtime_key() -> SigningKey {
    SigningKey::from_seed(&RUNTIME_SEED).expect("seed is valid")
}

/// A bridge with one accepted device, plus a handle on what got delivered.
fn bridge_with_paired_device(
    dir: &tempfile::TempDir,
    scope: PairingScope,
) -> (AgentBridge, Arc<Mutex<Vec<ClientCommand>>>) {
    let path = dir.path().join("remote").join("devices.json");
    let mut devices = TrustedDevices::load(&path).expect("empty store loads");
    devices
        .accept(
            DEVICE_ID,
            &device_key().verifying_key(),
            "iPhone",
            scope,
            AT,
        )
        .expect("accept persists");

    let runtime = RecordingRuntime::default();
    let delivered = runtime.delivered.clone();
    let routes = Arc::new(SingleProject::new(PROJECT_ID, "repo", Arc::new(runtime)));
    (
        AgentBridge::new(routes, devices, RUNTIME_ID, runtime_key(), false),
        delivered,
    )
}

fn upstream(key: &SigningKey, sender_id: &str, recipient: &str, body: &str) -> SignedEnvelope {
    SignedEnvelope::sign(
        key,
        Sender::Device,
        sender_id,
        recipient,
        "str_1",
        1,
        AT,
        ContentType::SessionUpstream,
        body.as_bytes(),
    )
    .expect("signing succeeds")
}

fn deliver_body(command_json: &str) -> String {
    format!(
        r#"{{"type":"deliver","command_id":"cmd-1","session_id":"s1","command":{command_json}}}"#
    )
}

#[tokio::test]
async fn a_frame_from_a_paired_device_reaches_the_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, delivered) = bridge_with_paired_device(&dir, PairingScope::Interactive);
    let frame = upstream(
        &device_key(),
        DEVICE_ID,
        RUNTIME_ID,
        &deliver_body(r#"{"type":"submit_message","session_id":"s1","content":"hi"}"#),
    );

    let admitted = bridge
        .admit_upstream(PROJECT_ID, &frame, AT, None)
        .await
        .unwrap();
    assert!(matches!(admitted, Admitted::Delivered { .. }));
    assert_eq!(delivered.lock().unwrap().len(), 1);
}

/// A compromised relay has no device key, so the best it can do is sign with
/// its own — and nothing may reach the runtime.
#[tokio::test]
async fn a_forged_signature_delivers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, delivered) = bridge_with_paired_device(&dir, PairingScope::Interactive);
    let relay_key = SigningKey::from_seed(&RELAY_SEED).expect("seed is valid");
    let frame = upstream(
        &relay_key,
        DEVICE_ID,
        RUNTIME_ID,
        &deliver_body(r#"{"type":"submit_message","session_id":"s1","content":"hi"}"#),
    );

    let error = bridge
        .admit_upstream(PROJECT_ID, &frame, AT, None)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "signature_invalid");
    assert!(
        delivered.lock().unwrap().is_empty(),
        "a forged frame must not reach the runtime"
    );
}

/// The relay-substitutes-a-key attack. The frame is validly signed — by the
/// relay's key — and the relay presents that key as the device's. The agent
/// must consult only what the user accepted locally.
#[tokio::test]
async fn a_relay_supplied_key_is_never_used_to_verify() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, delivered) = bridge_with_paired_device(&dir, PairingScope::Interactive);
    let relay_key = SigningKey::from_seed(&RELAY_SEED).expect("seed is valid");
    let relay_pubkey = relay_key.verifying_key().to_base64url();

    let frame = upstream(
        &relay_key,
        DEVICE_ID,
        RUNTIME_ID,
        &deliver_body(r#"{"type":"submit_message","session_id":"s1","content":"hi"}"#),
    );

    let error = bridge
        .admit_upstream(PROJECT_ID, &frame, AT, Some(&relay_pubkey))
        .await
        .unwrap_err();
    assert_eq!(
        error.code(),
        "pubkey_mismatch",
        "the substitution should be reported, not silently tolerated"
    );
    assert!(delivered.lock().unwrap().is_empty());
}

/// An unknown device is refused even with a perfectly valid signature — the
/// signature proves who signed, not that this host ever agreed to trust them.
#[tokio::test]
async fn an_unpaired_device_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, delivered) = bridge_with_paired_device(&dir, PairingScope::Interactive);
    let frame = upstream(
        &device_key(),
        "dev_stranger",
        RUNTIME_ID,
        &deliver_body(r#"{"type":"submit_message","session_id":"s1","content":"hi"}"#),
    );

    assert_eq!(
        bridge
            .admit_upstream(PROJECT_ID, &frame, AT, None)
            .await
            .unwrap_err()
            .code(),
        "unauthorized"
    );
    assert!(delivered.lock().unwrap().is_empty());
}

/// Revocation takes effect on the next frame, not at the next reconnection.
#[tokio::test]
async fn a_revoked_device_stops_being_admitted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote").join("devices.json");
    let mut devices = TrustedDevices::load(&path).unwrap();
    devices
        .accept(
            DEVICE_ID,
            &device_key().verifying_key(),
            "iPhone",
            PairingScope::Interactive,
            AT,
        )
        .unwrap();
    assert!(devices.revoke(DEVICE_ID, AT).unwrap());

    let runtime = RecordingRuntime::default();
    let delivered = runtime.delivered.clone();
    let bridge = AgentBridge::new(
        Arc::new(SingleProject::new(PROJECT_ID, "repo", Arc::new(runtime))),
        devices,
        RUNTIME_ID,
        runtime_key(),
        false,
    );

    let frame = upstream(
        &device_key(),
        DEVICE_ID,
        RUNTIME_ID,
        &deliver_body(r#"{"type":"submit_message","session_id":"s1","content":"hi"}"#),
    );
    assert_eq!(
        bridge
            .admit_upstream(PROJECT_ID, &frame, AT, None)
            .await
            .unwrap_err()
            .code(),
        "device_revoked"
    );
    assert!(delivered.lock().unwrap().is_empty());
}

/// A genuine frame this device signed for another host, replayed here.
#[tokio::test]
async fn a_frame_addressed_to_another_runtime_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, delivered) = bridge_with_paired_device(&dir, PairingScope::Interactive);
    let frame = upstream(
        &device_key(),
        DEVICE_ID,
        "rt_other_machine",
        &deliver_body(r#"{"type":"submit_message","session_id":"s1","content":"hi"}"#),
    );

    assert_eq!(
        bridge
            .admit_upstream(PROJECT_ID, &frame, AT, None)
            .await
            .unwrap_err()
            .code(),
        "recipient_mismatch"
    );
    assert!(delivered.lock().unwrap().is_empty());
}

/// The capability gate runs after the signature: a properly signed command
/// from a trusted device is still refused if it is not allowed remotely.
#[tokio::test]
async fn a_signed_but_disallowed_command_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, delivered) = bridge_with_paired_device(&dir, PairingScope::Interactive);

    let approve_always = serde_json::to_string(&ClientCommand::ApprovalDecision {
        request_id: ApprovalId::new("a1"),
        decision: ApprovalDecision::ApproveAlways,
    })
    .unwrap();
    let frame = upstream(
        &device_key(),
        DEVICE_ID,
        RUNTIME_ID,
        &deliver_body(&approve_always),
    );

    assert_eq!(
        bridge
            .admit_upstream(PROJECT_ID, &frame, AT, None)
            .await
            .unwrap_err()
            .code(),
        "approval_decision_not_allowed_remote"
    );
    assert!(
        delivered.lock().unwrap().is_empty(),
        "a standing permission grant must never reach the executor"
    );
}

/// `FullAccess` needs the host's own opt-in, which a device cannot supply.
#[tokio::test]
async fn full_access_is_refused_without_the_host_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, delivered) = bridge_with_paired_device(&dir, PairingScope::Interactive);

    let command = serde_json::to_string(&ClientCommand::SetPermissionProfile {
        session_id: SessionId::new("s1"),
        mode: PermissionProfile::FullAccess,
    })
    .unwrap();
    let frame = upstream(
        &device_key(),
        DEVICE_ID,
        RUNTIME_ID,
        &deliver_body(&command),
    );

    assert_eq!(
        bridge
            .admit_upstream(PROJECT_ID, &frame, AT, None)
            .await
            .unwrap_err()
            .code(),
        "permission_profile_not_allowed_remote"
    );
    assert!(delivered.lock().unwrap().is_empty());
}

/// An observe pairing may watch and nothing more.
#[tokio::test]
async fn an_observe_pairing_cannot_deliver() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, delivered) = bridge_with_paired_device(&dir, PairingScope::Observe);
    let frame = upstream(
        &device_key(),
        DEVICE_ID,
        RUNTIME_ID,
        &deliver_body(r#"{"type":"submit_message","session_id":"s1","content":"hi"}"#),
    );

    assert_eq!(
        bridge
            .admit_upstream(PROJECT_ID, &frame, AT, None)
            .await
            .unwrap_err()
            .code(),
        "command_not_allowed_remote"
    );
    assert!(delivered.lock().unwrap().is_empty());
}

/// The store is the durable record of what the user accepted, so it has to
/// survive a restart and stay unreadable by other accounts.
#[test]
fn the_device_store_round_trips_and_is_not_world_readable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote").join("devices.json");

    let mut devices = TrustedDevices::load(&path).unwrap();
    assert!(
        devices.devices().is_empty(),
        "a missing file is not an error"
    );
    devices
        .accept(
            DEVICE_ID,
            &device_key().verifying_key(),
            "iPhone",
            PairingScope::Interactive,
            AT,
        )
        .unwrap();

    let reloaded = TrustedDevices::load(&path).unwrap();
    assert_eq!(reloaded.devices().len(), 1);
    let (key, scope) = reloaded.key_for(DEVICE_ID, None).unwrap();
    assert_eq!(key, device_key().verifying_key());
    assert_eq!(scope, PairingScope::Interactive);
    assert_eq!(
        reloaded.devices()[0].fingerprint,
        device_key().verifying_key().fingerprint(),
        "the stored fingerprint is what the user confirmed"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "devices.json must not be group/world readable"
        );
    }
}

/// Accepting the same device again replaces its row rather than leaving two,
/// so a re-pair cannot leave a stale key behind that still verifies.
#[test]
fn re_accepting_a_device_replaces_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote").join("devices.json");
    let mut devices = TrustedDevices::load(&path).unwrap();

    devices
        .accept(
            DEVICE_ID,
            &device_key().verifying_key(),
            "iPhone",
            PairingScope::Interactive,
            AT,
        )
        .unwrap();
    let replacement = SigningKey::from_seed(&RELAY_SEED).unwrap().verifying_key();
    devices
        .accept(
            DEVICE_ID,
            &replacement,
            "iPhone (re-paired)",
            PairingScope::Observe,
            AT,
        )
        .unwrap();

    let reloaded = TrustedDevices::load(&path).unwrap();
    assert_eq!(reloaded.devices().len(), 1, "no stale row survives");
    let (key, scope) = reloaded.key_for(DEVICE_ID, None).unwrap();
    assert_eq!(key, replacement);
    assert_eq!(scope, PairingScope::Observe);
}
