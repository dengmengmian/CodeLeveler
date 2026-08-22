//! Fetching a registered attachment through the signed RPC.
//!
//! The phone already has an `AttachmentRef` (sha256). This is the other half:
//! bytes come back on the same pairing channel. The agent must not open a
//! workspace path, and `../` in the hash must never become a filesystem lookup.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use leveler_client_protocol::{
    ClientCommand, ClientError, InteractiveRuntimeClient, RuntimeEvent, SessionId,
    UiSessionSnapshot,
};
use leveler_local_transport::{
    AttachmentBytes, CreateSessionRequest, LocalRuntimeService, SessionBootstrap,
};
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
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct ServingRuntime {
    blobs: HashMap<String, AttachmentBytes>,
    events: broadcast::Sender<RuntimeEvent>,
}

impl ServingRuntime {
    fn with_markdown() -> Self {
        let mut blobs = HashMap::new();
        blobs.insert(
            SHA.to_string(),
            AttachmentBytes {
                mime_type: "text/markdown".to_string(),
                bytes: b"# review\n\npass\n".to_vec(),
            },
        );
        Self {
            blobs,
            events: broadcast::channel(4).0,
        }
    }
}

#[async_trait]
impl InteractiveRuntimeClient for ServingRuntime {
    async fn send(&self, _command: ClientCommand) -> Result<(), ClientError> {
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self, _session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        Err(ClientError::Runtime("not needed".into()))
    }
}

#[async_trait]
impl LocalRuntimeService for ServingRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Err(ClientError::Runtime("not needed".into()))
    }

    async fn fetch_attachment(&self, sha256: &str) -> Result<AttachmentBytes, ClientError> {
        self.blobs
            .get(sha256)
            .cloned()
            .ok_or_else(|| ClientError::Runtime("attachment not found".into()))
    }
}

fn device_key() -> SigningKey {
    SigningKey::from_seed(&DEVICE_SEED).expect("seed is valid")
}

fn runtime_key() -> SigningKey {
    SigningKey::from_seed(&RUNTIME_SEED).expect("seed is valid")
}

fn bridge(dir: &tempfile::TempDir, scope: PairingScope) -> AgentBridge {
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
    let routes = Arc::new(SingleProject::new(
        PROJECT_ID,
        "repo",
        Arc::new(ServingRuntime::with_markdown()),
    ));
    AgentBridge::new(routes, devices, RUNTIME_ID, runtime_key(), false)
}

fn fetch(sha256: &str, chunk_index: u32) -> SignedEnvelope {
    let body = serde_json::json!({
        "method": "fetch_attachment",
        "project_id": PROJECT_ID,
        "body": { "sha256": sha256, "chunk_index": chunk_index },
    });
    SignedEnvelope::sign(
        &device_key(),
        Sender::Device,
        DEVICE_ID,
        RUNTIME_ID,
        "rpc:fetch",
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
async fn fetch_returns_registered_bytes_under_the_runtime_signature() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(&dir, PairingScope::Interactive);
    let response = bridge
        .handle_rpc(&fetch(SHA, 0), AT, None)
        .await
        .expect("fetch is admitted");
    let body = answer(&response);
    assert_eq!(body["sha256"], SHA);
    assert_eq!(body["mime_type"], "text/markdown");
    assert_eq!(body["chunk_index"], 0);
    assert_eq!(body["chunk_total"], 1);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body["data_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(bytes, b"# review\n\npass\n");
}

#[tokio::test]
async fn an_observe_pairing_may_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(&dir, PairingScope::Observe);
    let response = bridge
        .handle_rpc(&fetch(SHA, 0), AT, None)
        .await
        .expect("observe may read");
    assert_eq!(answer(&response)["mime_type"], "text/markdown");
}

#[tokio::test]
async fn a_path_shaped_id_is_refused_before_the_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(&dir, PairingScope::Interactive);
    let error = bridge
        .handle_rpc(&fetch("../passwd", 0), AT, None)
        .await
        .expect_err("traversal is malformed");
    assert_eq!(error.code(), "invalid_frame");
}

#[tokio::test]
async fn a_missing_hash_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(&dir, PairingScope::Interactive);
    let missing = "bb".repeat(32);
    let error = bridge
        .handle_rpc(&fetch(&missing, 0), AT, None)
        .await
        .expect_err("unknown hash");
    assert_eq!(error.code(), "not_found");
}
