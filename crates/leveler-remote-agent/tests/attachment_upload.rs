//! Uploading a file from a phone, through the agent, into the runtime.
//!
//! The chunk arithmetic has its own unit tests. What these are about is the
//! part only the whole path shows: that the bytes arrive as an ordinary
//! `AddAttachmentData` rather than by the agent writing the media store itself,
//! that the phone gets back the runtime's own `AttachmentRef` under the
//! runtime's signature, and that a device paired to watch cannot upload.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use leveler_client_protocol::{
    AttachmentId, AttachmentKind, AttachmentRef, ClientCommand, ClientError,
    InteractiveRuntimeClient, RuntimeEvent, SessionId, UiSessionSnapshot,
};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
use leveler_remote_agent::{AgentBridge, SingleProject, TrustedDevices};
use leveler_remote_protocol::pairing::PairingScope;
use leveler_remote_protocol::{ContentType, Sender, SignedEnvelope, SigningKey, VerifyParams};
use tokio::sync::broadcast;

const DEVICE_SEED: [u8; 32] = [11u8; 32];
const RUNTIME_SEED: [u8; 32] = [77u8; 32];
const RUNTIME_ID: &str = "rt_host";
const DEVICE_ID: &str = "dev_phone";
const AT: &str = "2026-07-25T12:00:00Z";
const PROJECT_ID: &str = "0123456789abcdef";

/// A runtime that processes an attachment the way the real one does: the
/// command goes in, and an event carrying an `AttachmentRef` comes out.
struct ImportingRuntime {
    delivered: Arc<Mutex<Vec<ClientCommand>>>,
    events: broadcast::Sender<RuntimeEvent>,
    /// When set, report a failure instead of storing — the case where a file
    /// arrives intact and the runtime still cannot use it.
    reject_with: Option<String>,
}

impl ImportingRuntime {
    fn new(reject_with: Option<String>) -> Self {
        let (events, _) = broadcast::channel(16);
        Self {
            delivered: Arc::new(Mutex::new(Vec::new())),
            events,
            reject_with,
        }
    }
}

#[async_trait]
impl InteractiveRuntimeClient for ImportingRuntime {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        if let ClientCommand::AddAttachmentData {
            name, data_base64, ..
        } = &command
        {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_base64.as_bytes())
                .expect("the agent sends base64");
            let event = match &self.reject_with {
                Some(error) => RuntimeEvent::AttachmentProcessingFailed {
                    error: error.clone(),
                },
                None => RuntimeEvent::AttachmentAdded {
                    attachment: AttachmentRef {
                        id: AttachmentId::new("att_1"),
                        kind: AttachmentKind::Image,
                        name: name.clone(),
                        mime_type: "image/png".to_string(),
                        size_bytes: bytes.len() as u64,
                        sha256: "deadbeef".to_string(),
                        width: Some(2),
                        height: Some(1),
                    },
                },
            };
            let _ = self.events.send(event);
        }
        self.delivered.lock().unwrap().push(command);
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self, _session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        Err(ClientError::Runtime("not needed here".into()))
    }
}

#[async_trait]
impl LocalRuntimeService for ImportingRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Err(ClientError::Runtime("not needed here".into()))
    }
}

fn device_key() -> SigningKey {
    SigningKey::from_seed(&DEVICE_SEED).expect("seed is valid")
}

fn runtime_key() -> SigningKey {
    SigningKey::from_seed(&RUNTIME_SEED).expect("seed is valid")
}

fn bridge(
    dir: &tempfile::TempDir,
    scope: PairingScope,
    reject_with: Option<String>,
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

    let runtime = ImportingRuntime::new(reject_with);
    let delivered = runtime.delivered.clone();
    let routes = Arc::new(SingleProject::new(PROJECT_ID, "repo", Arc::new(runtime)));
    (
        AgentBridge::new(routes, devices, RUNTIME_ID, runtime_key(), false),
        delivered,
    )
}

/// One `upload_attachment` RPC, signed by the device as the real app signs it.
fn upload(stream_id: &str, index: u32, total: u32, bytes: &[u8]) -> SignedEnvelope {
    let body = serde_json::json!({
        "method": "upload_attachment",
        "project_id": PROJECT_ID,
        "body": {
            "session_id": "s1",
            "name": "shot.png",
            "chunk_index": index,
            "chunk_total": total,
            "data_base64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    });
    SignedEnvelope::sign(
        &device_key(),
        Sender::Device,
        DEVICE_ID,
        RUNTIME_ID,
        stream_id,
        1,
        AT,
        ContentType::RpcRequest,
        &serde_json::to_vec(&body).unwrap(),
    )
    .expect("signing succeeds")
}

fn answer(response: &SignedEnvelope) -> serde_json::Value {
    let verified = response
        .verify(&VerifyParams {
            expected_recipient_id: DEVICE_ID,
            public_key: &runtime_key().verifying_key(),
            now: AT,
        })
        .expect("the phone can verify the runtime's answer");
    serde_json::from_slice(&verified).expect("the body is json")
}

#[tokio::test]
async fn a_chunked_upload_reaches_the_runtime_whole_and_comes_back_signed() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, delivered) = bridge(&dir, PairingScope::Interactive, None);

    // The first chunk must not be mistaken for a whole file: answering with an
    // AttachmentRef here would tell the phone a half-file was stored.
    let first = bridge
        .handle_rpc(&upload("rpc:a", 0, 2, b"\x89PNG-first"), AT, None)
        .await
        .expect("first chunk is accepted");
    assert_eq!(answer(&first)["status"], "chunk_received");
    assert!(
        delivered.lock().unwrap().is_empty(),
        "nothing may reach the runtime until the file is whole"
    );

    let second = bridge
        .handle_rpc(&upload("rpc:b", 1, 2, b"-second"), AT, None)
        .await
        .expect("the file completes");

    // The answer is the runtime's own record, verified under the runtime key
    // the phone anchored from the pairing QR.
    let attachment = answer(&second);
    assert_eq!(attachment["name"], "shot.png");
    assert_eq!(attachment["id"], "att_1");
    assert_eq!(attachment["size_bytes"], 17);

    // And the bytes went in through the ordinary command path, joined in order.
    let commands = delivered.lock().unwrap().clone();
    assert_eq!(commands.len(), 1);
    match &commands[0] {
        ClientCommand::AddAttachmentData {
            session_id,
            name,
            data_base64,
        } => {
            assert_eq!(session_id.as_str(), "s1");
            assert_eq!(name, "shot.png");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_base64.as_bytes())
                .unwrap();
            assert_eq!(bytes, b"\x89PNG-first-second");
        }
        other => panic!("the agent must use the runtime's own import command, got {other:?}"),
    }
}

#[tokio::test]
async fn a_runtime_that_refuses_the_file_is_reported_rather_than_papered_over() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, _) = bridge(
        &dir,
        PairingScope::Interactive,
        Some("unsupported image".to_string()),
    );

    let error = bridge
        .handle_rpc(&upload("rpc:a", 0, 1, b"junk"), AT, None)
        .await
        .expect_err("a failed import is a failed RPC");
    assert!(
        format!("{error}").contains("unsupported image"),
        "the phone should be told why, got {error}"
    );
}

#[tokio::test]
async fn an_observe_pairing_cannot_upload() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, delivered) = bridge(&dir, PairingScope::Observe, None);

    let error = bridge
        .handle_rpc(&upload("rpc:a", 0, 1, b"anything"), AT, None)
        .await
        .expect_err("observe may not add to a session");
    assert_eq!(error.code(), "command_not_allowed_remote");
    assert!(
        delivered.lock().unwrap().is_empty(),
        "a refused upload must not reach the runtime"
    );
}

#[tokio::test]
async fn an_oversized_upload_is_refused_with_a_code_the_app_can_explain() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, delivered) = bridge(&dir, PairingScope::Interactive, None);

    let too_big = vec![0u8; leveler_remote_agent::MAX_ATTACHMENT_BYTES + 1];
    let error = bridge
        .handle_rpc(&upload("rpc:a", 0, 1, &too_big), AT, None)
        .await
        .expect_err("over the cap");
    assert_eq!(error.code(), "payload_too_large");
    assert!(delivered.lock().unwrap().is_empty());
}
