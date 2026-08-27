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
use leveler_remote_agent::{AgentBridge, SingleProject, TrustedDevices, run_tunnel};
use leveler_remote_protocol::auth::{RUNTIME_AUTH_HEADER, runtime_action};
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
const PROJECT_ID: &str = "0123456789abcdef";
/// The secret the relay's operator configured; the host presents it once, to
/// enroll.
const ENROLLMENT_SECRET: &str = "operator-secret";

/// The header the host signs its own control-plane requests with.
fn runtime_auth(key: &SigningKey, action: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = format!("n{}", NEXT.fetch_add(1, Ordering::SeqCst));
    leveler_remote_protocol::auth::RuntimeAssertion::header_value(
        key,
        action,
        RUNTIME_ID,
        &now_stamp(),
        &nonce,
    )
}
/// The agent stamps its own frames with the real clock, because the relay
/// checks registration freshness against it.
fn now_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct FakeRuntime {
    delivered: Arc<Mutex<Vec<ClientCommand>>>,
    /// What the runtime emits, so a test can make the machine speak first.
    events: broadcast::Sender<RuntimeEvent>,
    /// When set, `subscribe` floods the channel *before* handing back the
    /// receiver, so the pump's first `recv` is guaranteed to report a lag. The
    /// alternative — racing a real subscriber — would be a flaky test of the
    /// one path that must not be guesswork.
    lag_on_subscribe: bool,
}

impl FakeRuntime {
    fn new() -> Self {
        Self {
            delivered: Arc::new(Mutex::new(Vec::new())),
            events: broadcast::channel(16).0,
            lag_on_subscribe: false,
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
        let receiver = self.events.subscribe();
        if self.lag_on_subscribe {
            for index in 0..64 {
                let _ = self.events.send(RuntimeEvent::AgentActivity {
                    label: format!("flood {index}"),
                });
            }
        }
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
        Err(ClientError::Runtime("not used here".into()))
    }
}

/// A relay, an agent connected to it, and a paired device's token.
struct Chain {
    base: String,
    token: String,
    delivered: Arc<Mutex<Vec<ClientCommand>>>,
    /// Lets a test make the runtime emit, the way real work does.
    events: broadcast::Sender<RuntimeEvent>,
    runtime_key: VerifyingKey,
    _relay_state: RelayState,
}

async fn build_chain(dir: &tempfile::TempDir, scope: PairingScope) -> Chain {
    build_chain_with(dir, scope, false).await
}

async fn build_chain_with(
    dir: &tempfile::TempDir,
    scope: PairingScope,
    lag_on_subscribe: bool,
) -> Chain {
    let runtime = FakeRuntime {
        lag_on_subscribe,
        ..FakeRuntime::new()
    };
    let delivered = runtime.delivered.clone();
    let events = runtime.events.clone();
    let routes = Arc::new(SingleProject::new(PROJECT_ID, "repo", Arc::new(runtime)));
    serve_chain(dir, scope, routes, delivered, events).await
}

/// Everything between "a phone with no pairing" and "a phone holding a token
/// against a live agent": the relay's real endpoints, the real tunnel, and the
/// caller's projects behind it.
async fn serve_chain(
    dir: &tempfile::TempDir,
    scope: PairingScope,
    routes: Arc<dyn leveler_remote_agent::ProjectRoutes>,
    delivered: Arc<Mutex<Vec<ClientCommand>>>,
    events: broadcast::Sender<RuntimeEvent>,
) -> Chain {
    // The relay, as a real process would run it.
    let state = RelayState::with_enrollment_secret(ENROLLMENT_SECRET);
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
    let host_key = SigningKey::from_seed(&RUNTIME_SEED).unwrap();
    let enrolled = client
        .post(format!("{http}/v1/runtimes/enroll"))
        .bearer_auth(ENROLLMENT_SECRET)
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(&host_key, runtime_action::ENROLL),
        )
        .json(&json!({
            "runtime_id": RUNTIME_ID,
            "runtime_pubkey": host_key.verifying_key().to_base64url()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(enrolled.status(), 204, "the host enrolls with the relay");
    let begin: serde_json::Value = client
        .post(format!("{http}/v1/pair/begin"))
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(&host_key, runtime_action::PAIR_BEGIN),
        )
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
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(&host_key, runtime_action::PAIR_CONFIRM),
        )
        .json(&json!({
            "runtime_id": RUNTIME_ID,
            "pairing_id": complete["pairing_id"].as_str().unwrap(),
            "decision": "accept"
        }))
        .send()
        .await
        .unwrap();
    let timestamp = now_stamp();
    let nonce = format!("n-{}", timestamp);
    let input = leveler_remote_protocol::auth::SessionAuthRequest::signing_input(
        DEVICE_ID, RUNTIME_ID, &timestamp, &nonce,
    );
    use base64::Engine as _;
    let sig = base64::engine::general_purpose::STANDARD
        .encode(device_key.sign_detached(input.as_bytes()));
    let auth: serde_json::Value = client
        .post(format!("{http}/v1/auth/session"))
        .json(&json!({
            "device_id": DEVICE_ID, "runtime_id": RUNTIME_ID,
            "timestamp": timestamp, "nonce": nonce, "sig": sig,
        }))
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
        .accept(
            DEVICE_ID,
            &device_key.verifying_key(),
            "iPhone",
            scope,
            &now_stamp(),
        )
        .unwrap();

    let bridge = Arc::new(AgentBridge::new(
        routes,
        devices,
        RUNTIME_ID,
        runtime_key,
        false,
    ));

    let ws_base = format!("ws://{base}");
    tokio::spawn(async move {
        let _ = run_tunnel(
            &ws_base,
            RUNTIME_ID,
            // A space and non-ASCII on purpose: this is what a user types when
            // asked to name their machine, and it used to make the tunnel URL
            // invalid.
            "我的 Mac",
            bridge,
            std::time::Duration::from_secs(120),
            now_stamp,
        )
        .await;
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
        events,
        runtime_key: public_runtime_key,
        _relay_state: state,
    }
}

async fn connect_app(chain: &Chain) -> Socket {
    connect_app_to(chain, None).await
}

/// Connect naming a project, the way a phone that has switched projects does.
async fn connect_app_to(chain: &Chain, project_id: Option<&str>) -> Socket {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let query = project_id
        .map(|id| format!("?project_id={id}"))
        .unwrap_or_default();
    let mut request = format!("ws://{}/v1/hosts/{RUNTIME_ID}/session{query}", chain.base)
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
        &now_stamp(),
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
            now: &now_stamp(),
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
        &now_stamp(),
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
        &now_stamp(),
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

/// A stream can only be opened against a project the host actually has. The
/// relay names the project; the agent decides whether it exists, and closes the
/// stream rather than falling back to whichever repository happens to be first.
#[tokio::test]
async fn a_stream_for_an_unknown_project_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let chain = build_chain(&dir, PairingScope::Interactive).await;
    let mut app = connect_app_to(&chain, Some("no_such_project")).await;

    send_command(
        &mut app,
        &ClientCommand::SubmitMessage {
            session_id: SessionId::new("s1"),
            content: "错的项目".to_string(),
            attachments: Vec::new(),
        },
    )
    .await;

    // The agent refused the stream, so the relay dropped the route and the
    // device's socket ends rather than silently going nowhere.
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(message) = app.next().await {
            if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                return true;
            }
        }
        true
    })
    .await
    .expect("the stream should end");
    assert!(closed);
    assert!(
        chain.delivered.lock().unwrap().is_empty(),
        "nothing may reach a runtime through a stream that was never accepted"
    );
}

/// Naming the host's one open project works, so the check is not simply
/// "refuse everything".
#[tokio::test]
async fn a_stream_naming_the_hosts_project_works() {
    let dir = tempfile::tempdir().unwrap();
    let chain = build_chain(&dir, PairingScope::Interactive).await;
    let mut app = connect_app_to(&chain, Some(PROJECT_ID)).await;

    send_command(
        &mut app,
        &ClientCommand::SubmitMessage {
            session_id: SessionId::new("s1"),
            content: "对的项目".to_string(),
            attachments: Vec::new(),
        },
    )
    .await;

    let ack = recv_verified(&mut app, &chain).await;
    assert_eq!(ack["type"], "ack");
    assert_eq!(chain.delivered.lock().unwrap().len(), 1);
}

/// The half the phone actually reads: the runtime speaking unprompted.
///
/// Without this the chain acknowledges commands and shows nothing — a phone
/// could submit a message and never see the answer.
#[tokio::test]
async fn runtime_events_reach_the_phone_signed() {
    let dir = tempfile::tempdir().unwrap();
    let chain = build_chain(&dir, PairingScope::Interactive).await;
    let mut app = connect_app(&chain).await;

    // The pump subscribes when the stream opens; emitting until it lands avoids
    // depending on that having happened by any particular instant.
    let event = RuntimeEvent::AssistantTextDelta {
        message_id: leveler_client_protocol::MessageId::new("m1"),
        delta: "助手输出".to_string(),
    };
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        // Wait for the pump to subscribe, then emit: a broadcast delivers only
        // to receivers that already exist.
        while chain.events.receiver_count() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        chain.events.send(event.clone()).unwrap();
        recv_verified(&mut app, &chain).await
    })
    .await
    .expect("an event should reach the device");

    assert_eq!(frame["type"], "event");
    assert_eq!(frame["event"]["type"], "assistant_text_delta");
    assert_eq!(frame["event"]["delta"], "助手输出");
}

/// An observe pairing exists to watch: it delivers nothing, but it must still
/// see the stream. A read-only pairing that also saw nothing would be pointless.
#[tokio::test]
async fn an_observe_pairing_still_receives_events() {
    let dir = tempfile::tempdir().unwrap();
    let chain = build_chain(&dir, PairingScope::Observe).await;
    let mut app = connect_app(&chain).await;

    let event = RuntimeEvent::AgentActivity {
        label: "运行测试".to_string(),
    };
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while chain.events.receiver_count() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        chain.events.send(event.clone()).unwrap();
        recv_verified(&mut app, &chain).await
    })
    .await
    .expect("an observer should still see events");

    assert_eq!(frame["type"], "event");
    assert_eq!(frame["event"]["label"], "运行测试");
}

/// A device that fell behind is told so and its stream ends. Continuing to feed
/// it would render a transcript with holes, which reads as fact rather than as
/// the gap it is.
#[tokio::test]
async fn a_lagged_subscriber_is_told_to_resync_and_the_stream_ends() {
    let dir = tempfile::tempdir().unwrap();
    let chain = build_chain_with(&dir, PairingScope::Interactive, true).await;
    let mut app = connect_app(&chain).await;

    let frame = recv_verified(&mut app, &chain).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["code"], "resync_required");

    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(message) = app.next().await {
            if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                return true;
            }
        }
        true
    })
    .await
    .expect("the stream should end after a resync demand");
    assert!(ended);
}

/// Two projects open, two streams from one phone: the events of each reach only
/// the stream bound to it.
///
/// This is the failure that would make multi-project remote control worse than
/// none — a phone showing one repository's screen while another's output scrolls
/// into it.
#[tokio::test]
async fn events_do_not_cross_between_two_open_projects() {
    /// Two projects, each with its own runtime and its own event stream.
    struct TwoProjects {
        alpha: Arc<FakeRuntime>,
        beta: Arc<FakeRuntime>,
    }

    #[async_trait]
    impl leveler_remote_agent::ProjectRoutes for TwoProjects {
        async fn projects(&self) -> Vec<leveler_remote_agent::ProjectInfo> {
            ["alpha_project_id", "beta_project_id"]
                .iter()
                .map(|id| leveler_remote_agent::ProjectInfo {
                    project_id: id.to_string(),
                    path_display: id.to_string(),
                    status: leveler_session_wire::ProjectStatus::Online,
                })
                .collect()
        }

        async fn runtime(
            &self,
            project_id: &str,
        ) -> Result<Arc<dyn LocalRuntimeService>, leveler_remote_agent::RouteError> {
            match project_id {
                "alpha_project_id" => Ok(self.alpha.clone()),
                "beta_project_id" => Ok(self.beta.clone()),
                _ => Err(leveler_remote_agent::RouteError::UnknownProject),
            }
        }

        async fn implied_project(&self) -> Result<String, leveler_remote_agent::RouteError> {
            Err(leveler_remote_agent::RouteError::ProjectRequired)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let alpha = Arc::new(FakeRuntime::new());
    let beta = Arc::new(FakeRuntime::new());
    let alpha_events = alpha.events.clone();
    let beta_events = beta.events.clone();
    let chain = serve_chain(
        &dir,
        PairingScope::Interactive,
        Arc::new(TwoProjects {
            alpha: alpha.clone(),
            beta: beta.clone(),
        }),
        alpha.delivered.clone(),
        alpha_events.clone(),
    )
    .await;

    let mut on_alpha = connect_app_to(&chain, Some("alpha_project_id")).await;
    let mut on_beta = connect_app_to(&chain, Some("beta_project_id")).await;

    // Both pumps attached.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while alpha_events.receiver_count() == 0 || beta_events.receiver_count() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both streams should subscribe");

    alpha_events
        .send(RuntimeEvent::AgentActivity {
            label: "alpha 在跑".to_string(),
        })
        .unwrap();

    let frame = recv_verified(&mut on_alpha, &chain).await;
    assert_eq!(frame["event"]["label"], "alpha 在跑");

    // The other project's stream saw nothing at all.
    let quiet = tokio::time::timeout(std::time::Duration::from_millis(300), on_beta.next()).await;
    assert!(
        quiet.is_err(),
        "the other project's stream must stay silent, got {quiet:?}"
    );

    // And it works the other way round, so the isolation is not just "beta is
    // broken".
    beta_events
        .send(RuntimeEvent::AgentActivity {
            label: "beta 在跑".to_string(),
        })
        .unwrap();
    let frame = recv_verified(&mut on_beta, &chain).await;
    assert_eq!(frame["event"]["label"], "beta 在跑");
    let quiet = tokio::time::timeout(std::time::Duration::from_millis(300), on_alpha.next()).await;
    assert!(quiet.is_err(), "alpha must not see beta's output");
}
