//! The Phase 1 acceptance path, walked once, in order.
//!
//! Every step here has a focused test elsewhere. What this adds is the
//! *sequence*: a device that pairs, switches projects, gets an approval denied
//! for it, reconnects, is revoked, and finally finds its host gone. Bugs that
//! only appear in combination — a revocation that works alone but not after a
//! project switch, a reconnect that loses the stream binding — have nowhere to
//! hide in a walk like this, and nowhere to show up in tests that each start
//! from a clean fixture.
//!
//! The relay is the real router, the agent the real tunnel, the signatures real
//! Ed25519. Only the runtimes are fakes, because a fake runtime is the only way
//! to assert what did *not* reach one.
//!
//! Unix-only, matching the MVP's host support.
#![cfg(unix)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt as _, StreamExt as _};
use leveler_client_protocol::{
    ApprovalId, ClientCommand, ClientError, InteractiveRuntimeClient, PermissionProfile,
    RuntimeEvent, SessionId, UiApprovalRequest, UiSessionSnapshot,
};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
use leveler_relay::{RelayState, build_router};
use leveler_remote_agent::{
    AgentBridge, ProjectInfo, ProjectRoutes, RouteError, TrustedDevices, run_tunnel,
};
use leveler_remote_protocol::auth::{
    RUNTIME_AUTH_HEADER, RuntimeAssertion, SessionAuthRequest, runtime_action,
};
use leveler_remote_protocol::pairing::PairingScope;
use leveler_remote_protocol::tunnel::{RpcMethod, RpcRequestPayload, rpc_stream_id};
use leveler_remote_protocol::{
    ContentType, Sender, SignedEnvelope, SigningKey, VerifyParams, VerifyingKey,
};
use leveler_session_wire::ProjectStatus;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

const DEVICE_SEED: [u8; 32] = [101u8; 32];
const RUNTIME_SEED: [u8; 32] = [102u8; 32];
const DEVICE_ID: &str = "dev_phone";
const ENROLLMENT_SECRET: &str = "operator-secret-for-tests";
const ALPHA: &str = "alpha0000project";
const BETA: &str = "beta00000project";
/// Short enough for a test, long enough that the poll cycle is exercised.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(1);

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn device_key() -> SigningKey {
    SigningKey::from_seed(&DEVICE_SEED).unwrap()
}

fn host_key() -> SigningKey {
    SigningKey::from_seed(&RUNTIME_SEED).unwrap()
}

fn now_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn nonce() -> String {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("n{}", NEXT.fetch_add(1, Ordering::SeqCst))
}

fn runtime_auth(action: &str, runtime_id: &str) -> String {
    RuntimeAssertion::header_value(&host_key(), action, runtime_id, &now_stamp(), &nonce())
}

/// A runtime that records what it was told and can be made to speak.
struct FakeRuntime {
    label: String,
    delivered: Arc<Mutex<Vec<ClientCommand>>>,
    events: broadcast::Sender<RuntimeEvent>,
    local_waiters: Arc<AtomicUsize>,
}

impl FakeRuntime {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            delivered: Arc::new(Mutex::new(Vec::new())),
            events: broadcast::channel(64).0,
            local_waiters: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl InteractiveRuntimeClient for FakeRuntime {
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
            repository: self.label.clone(),
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
            recaps: Vec::new(),
            user_shells: Vec::new(),
            completion_report: None,
            reasoning: None,
            work_profile: None,
            collaboration: None,
        })
    }
}

#[async_trait]
impl LocalRuntimeService for FakeRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        let session = self.snapshot(&SessionId::new("s-new")).await?;
        Ok(SessionBootstrap {
            session,
            context_window: 128_000,
        })
    }

    async fn local_waiter_count(&self) -> Result<usize, ClientError> {
        Ok(self.local_waiters.load(Ordering::SeqCst))
    }
}

/// The two projects the user has open on this machine.
struct OpenProjects {
    alpha: Arc<FakeRuntime>,
    beta: Arc<FakeRuntime>,
}

#[async_trait]
impl ProjectRoutes for OpenProjects {
    async fn projects(&self) -> Vec<ProjectInfo> {
        vec![
            ProjectInfo {
                project_id: ALPHA.to_string(),
                path_display: "alpha".to_string(),
                status: ProjectStatus::Online,
            },
            ProjectInfo {
                project_id: BETA.to_string(),
                path_display: "beta".to_string(),
                status: ProjectStatus::Online,
            },
        ]
    }

    async fn runtime(&self, project_id: &str) -> Result<Arc<dyn LocalRuntimeService>, RouteError> {
        match project_id {
            ALPHA => Ok(self.alpha.clone()),
            BETA => Ok(self.beta.clone()),
            _ => Err(RouteError::UnknownProject),
        }
    }

    async fn implied_project(&self) -> Result<String, RouteError> {
        Err(RouteError::ProjectRequired)
    }
}

/// Sign one upstream frame as the phone.
fn upstream(stream_seq: u64, runtime_id: &str, body: serde_json::Value) -> SignedEnvelope {
    SignedEnvelope::sign(
        &device_key(),
        Sender::Device,
        DEVICE_ID,
        runtime_id,
        "str_app",
        stream_seq,
        &now_stamp(),
        ContentType::SessionUpstream,
        body.to_string().as_bytes(),
    )
    .unwrap()
}

fn deliver(session: &str, content: &str) -> serde_json::Value {
    json!({
        "type": "deliver",
        "command_id": format!("cmd-{content}"),
        "session_id": session,
        "command": {
            "type": "submit_message",
            "session_id": session,
            "content": content,
            "attachments": []
        }
    })
}

/// Receive one frame and verify it the way the phone does.
async fn recv_verified(app: &mut Socket, anchored: &VerifyingKey) -> serde_json::Value {
    let message = tokio::time::timeout(Duration::from_secs(10), app.next())
        .await
        .expect("a downstream frame should arrive")
        .expect("the socket is open")
        .expect("no socket error");
    let Message::Text(text) = message else {
        panic!("expected text, got {message:?}");
    };
    let envelope: SignedEnvelope = serde_json::from_str(&text).unwrap();
    let payload = envelope
        .verify(&VerifyParams {
            expected_recipient_id: DEVICE_ID,
            public_key: anchored,
            now: &now_stamp(),
        })
        .expect("the runtime's signature must verify on the device");
    serde_json::from_slice(&payload).unwrap()
}

/// Read frames until one satisfies `wanted`, so an unrelated event in flight
/// does not make the walk order-dependent.
async fn recv_until(
    app: &mut Socket,
    anchored: &VerifyingKey,
    wanted: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    for _ in 0..50 {
        let frame = recv_verified(app, anchored).await;
        if wanted(&frame) {
            return frame;
        }
    }
    panic!("no matching frame arrived");
}

async fn connect_app(base: &str, runtime_id: &str, token: &str, project_id: &str) -> Socket {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let mut request = format!("ws://{base}/v1/hosts/{runtime_id}/session?project_id={project_id}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket
}

/// The whole Phase 1 path, in the order a user would live it.
#[tokio::test]
async fn a_phone_pairs_switches_projects_gets_a_timeout_reconnects_and_is_revoked() {
    let dir = tempfile::tempdir().unwrap();
    let http = reqwest::Client::new();

    // ---------------------------------------------------------------- relay
    let state = RelayState::with_enrollment_secret(ENROLLMENT_SECRET);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = listener.local_addr().unwrap().to_string();
    let router = build_router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let url = format!("http://{base}");

    // ------------------------------------------------------------ the host
    let key = host_key();
    let runtime_id = leveler_remote_agent::runtime_id_for(&key.verifying_key());
    let anchored = key.verifying_key();

    let enrolled = http
        .post(format!("{url}/v1/runtimes/enroll"))
        .bearer_auth(ENROLLMENT_SECRET)
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(runtime_action::ENROLL, &runtime_id),
        )
        .json(&json!({
            "runtime_id": runtime_id,
            "runtime_pubkey": key.verifying_key().to_base64url()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(enrolled.status(), 204, "the host enrolls with the relay");

    // ----------------------------------------------------------- pairing
    let begin: serde_json::Value = http
        .post(format!("{url}/v1/pair/begin"))
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(runtime_action::PAIR_BEGIN, &runtime_id),
        )
        .json(&json!({"runtime_id": runtime_id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let complete: serde_json::Value = http
        .post(format!("{url}/v1/pair/complete"))
        .json(&json!({
            "device_id": DEVICE_ID,
            "device_pubkey": device_key().verifying_key().to_base64url(),
            "device_name": "iPhone",
            "platform": "ios",
            "pairing_secret": begin["pairing_secret"].as_str().unwrap(),
            "scope": "interactive",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // The host writes its own trust record first, then tells the relay — the
    // order the CLI uses, so a crash cannot leave the relay ahead of the host.
    let devices_path = dir.path().join("remote/devices.json");
    let mut devices = TrustedDevices::load(&devices_path).unwrap();
    devices
        .accept(
            DEVICE_ID,
            &device_key().verifying_key(),
            "iPhone",
            PairingScope::Interactive,
            &now_stamp(),
        )
        .unwrap();
    let confirmed = http
        .post(format!("{url}/v1/pair/confirm"))
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(runtime_action::PAIR_CONFIRM, &runtime_id),
        )
        .json(&json!({
            "runtime_id": runtime_id,
            "pairing_id": complete["pairing_id"].as_str().unwrap(),
            "decision": "accept"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(confirmed.status(), 204);

    // ------------------------------------------------------------- agent up
    let alpha = Arc::new(FakeRuntime::new("alpha"));
    let beta = Arc::new(FakeRuntime::new("beta"));
    let bridge = Arc::new(AgentBridge::new(
        Arc::new(OpenProjects {
            alpha: alpha.clone(),
            beta: beta.clone(),
        }),
        TrustedDevices::load(&devices_path).unwrap(),
        runtime_id.clone(),
        host_key(),
        false,
    ));
    let ws_base = format!("ws://{base}");
    {
        let runtime_id = runtime_id.clone();
        tokio::spawn(async move {
            let _ = run_tunnel(
                &ws_base,
                &runtime_id,
                "dev-box",
                bridge,
                APPROVAL_TIMEOUT,
                now_stamp,
            )
            .await;
        });
    }
    for _ in 0..200 {
        if state.is_online(&runtime_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(state.is_online(&runtime_id), "the agent should register");

    // ------------------------------------------------------------ the phone
    let timestamp = now_stamp();
    let auth_nonce = nonce();
    let assertion =
        SessionAuthRequest::signing_input(DEVICE_ID, &runtime_id, &timestamp, &auth_nonce);
    let auth: serde_json::Value = http
        .post(format!("{url}/v1/auth/session"))
        .json(&json!({
            "device_id": DEVICE_ID, "runtime_id": runtime_id,
            "timestamp": timestamp, "nonce": auth_nonce,
            "sig": b64(&device_key().sign_detached(assertion.as_bytes())),
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = auth["access_token"].as_str().unwrap().to_string();

    // 1. The project list arrives signed, with both repositories.
    let listed = rpc(
        &http,
        &url,
        &runtime_id,
        &token,
        &anchored,
        RpcMethod::ListProjects,
        None,
        json!({}),
        "rpc-projects",
    )
    .await;
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 2, "both open projects are visible");

    // 2. A session in the first project, created through the signed RPC.
    let bootstrap = rpc(
        &http,
        &url,
        &runtime_id,
        &token,
        &anchored,
        RpcMethod::CreateSession,
        Some(ALPHA),
        json!({"goal": "修 bug", "model": null, "mode": "request_approval"}),
        "rpc-create",
    )
    .await;
    assert_eq!(bootstrap["session"]["repository"], "alpha");

    // 3. Talk to alpha, and see the runtime's output come back.
    let mut app = connect_app(&base, &runtime_id, &token, ALPHA).await;
    send(
        &mut app,
        upstream(1, &runtime_id, deliver("s1", "给 alpha")),
    )
    .await;
    let ack = recv_until(&mut app, &anchored, |frame| frame["type"] == "ack").await;
    assert_eq!(ack["command_id"], "cmd-给 alpha");
    assert_eq!(alpha.delivered.lock().unwrap().len(), 1);
    assert!(beta.delivered.lock().unwrap().is_empty());

    wait_for(|| alpha.events.receiver_count() > 0).await;
    alpha
        .events
        .send(RuntimeEvent::AgentActivity {
            label: "跑测试".to_string(),
        })
        .unwrap();
    let event = recv_until(&mut app, &anchored, |frame| frame["type"] == "event").await;
    assert_eq!(event["event"]["label"], "跑测试");

    // 4. Switch to the other project. Its stream is its own.
    let mut on_beta = connect_app(&base, &runtime_id, &token, BETA).await;
    send(
        &mut on_beta,
        upstream(1, &runtime_id, deliver("s2", "给 beta")),
    )
    .await;
    let ack = recv_until(&mut on_beta, &anchored, |frame| frame["type"] == "ack").await;
    assert_eq!(ack["command_id"], "cmd-给 beta");
    assert_eq!(beta.delivered.lock().unwrap().len(), 1);
    assert_eq!(
        alpha.delivered.lock().unwrap().len(),
        1,
        "the first project must not see the second's traffic"
    );

    // 5. An approval nobody local can answer is denied by the host itself.
    alpha
        .events
        .send(RuntimeEvent::ApprovalRequested {
            request: UiApprovalRequest {
                id: ApprovalId::new("a1"),
                tool: "run_command".to_string(),
                summary: "rm -rf build".to_string(),
                command: Some("rm -rf build".to_string()),
                risks: Vec::new(),
            },
        })
        .unwrap();
    let denied = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let denials = alpha
                .delivered
                .lock()
                .unwrap()
                .iter()
                .filter(|command| matches!(command, ClientCommand::ApprovalDecision { .. }))
                .count();
            if denials > 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        denied.is_ok(),
        "a remote-only approval must not wait forever"
    );

    // 6. The phone is killed and comes back: a fresh stream, a fresh snapshot.
    drop(app);
    let mut app = connect_app(&base, &runtime_id, &token, ALPHA).await;
    send(
        &mut app,
        upstream(
            1,
            &runtime_id,
            json!({"type": "snapshot", "session_id": "s1"}),
        ),
    )
    .await;
    let snapshot = recv_until(&mut app, &anchored, |frame| frame["type"] == "snapshot").await;
    assert_eq!(snapshot["session"]["repository"], "alpha");

    // 7. The user revokes the phone. The host stops trusting it, and the relay
    //    closes what it had open.
    let revoked = http
        .delete(format!("{url}/v1/devices/{DEVICE_ID}"))
        .query(&[("runtime_id", runtime_id.as_str())])
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(runtime_action::DEVICE_REVOKE, &runtime_id),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), 204);
    let mut store = TrustedDevices::load(&devices_path).unwrap();
    assert!(store.revoke(DEVICE_ID, &now_stamp()).unwrap());

    // Its token is inert on the next request.
    let refused = http
        .post(format!("{url}/v1/hosts/{runtime_id}/rpc"))
        .bearer_auth(&token)
        .json(&signed_rpc(
            &runtime_id,
            RpcMethod::ListProjects,
            None,
            json!({}),
            "rpc-after-revoke",
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401, "a revoked device is done");

    // 8. The host goes away entirely: nothing is queued for it.
    state.unregister_runtime(&runtime_id);
    let offline = http
        .post(format!("{url}/v1/hosts/{runtime_id}/rpc"))
        .bearer_auth(&token)
        .json(&signed_rpc(
            &runtime_id,
            RpcMethod::ListProjects,
            None,
            json!({}),
            "rpc-offline",
        ))
        .send()
        .await
        .unwrap();
    assert!(
        offline.status() == 401 || offline.status() == 503,
        "an offline host answers, it does not queue: got {}",
        offline.status()
    );
}

async fn send(app: &mut Socket, frame: SignedEnvelope) {
    app.send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
        .await
        .unwrap();
}

/// Build the device-signed envelope an RPC travels in.
fn signed_rpc(
    runtime_id: &str,
    method: RpcMethod,
    project_id: Option<&str>,
    body: serde_json::Value,
    uuid: &str,
) -> SignedEnvelope {
    let payload = serde_json::to_vec(&RpcRequestPayload {
        method,
        project_id: project_id.map(|id| id.to_string()),
        body,
    })
    .unwrap();
    SignedEnvelope::sign(
        &device_key(),
        Sender::Device,
        DEVICE_ID,
        runtime_id,
        &rpc_stream_id(uuid),
        1,
        &now_stamp(),
        ContentType::RpcRequest,
        &payload,
    )
    .unwrap()
}

/// Post one RPC and verify the answer the way the phone does.
#[allow(clippy::too_many_arguments)]
async fn rpc(
    http: &reqwest::Client,
    url: &str,
    runtime_id: &str,
    token: &str,
    anchored: &VerifyingKey,
    method: RpcMethod,
    project_id: Option<&str>,
    body: serde_json::Value,
    uuid: &str,
) -> serde_json::Value {
    let response = http
        .post(format!("{url}/v1/hosts/{runtime_id}/rpc"))
        .bearer_auth(token)
        .json(&signed_rpc(runtime_id, method, project_id, body, uuid))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "rpc {uuid} should succeed");
    let envelope: SignedEnvelope = response.json().await.unwrap();
    let payload = envelope
        .verify(&VerifyParams {
            expected_recipient_id: DEVICE_ID,
            public_key: anchored,
            now: &now_stamp(),
        })
        .expect("an RPC result must be verifiable with the anchored runtime key");
    serde_json::from_slice(&payload).unwrap()
}

async fn wait_for(mut condition: impl FnMut() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition never became true");
}
