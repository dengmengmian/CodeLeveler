//! The relay carrying an RPC from a phone's HTTP request to the agent tunnel
//! and back.
//!
//! This is the one place the relay bridges two transports, so it is the one
//! place it could quietly become a participant: unwrap the device's envelope,
//! answer from itself, or hand back a body it composed. Each test states the
//! version of that failure it rules out.

use futures_util::{SinkExt as _, StreamExt as _};
use leveler_relay::{RelayState, build_router};
use leveler_remote_protocol::auth::{
    AgentRegisterAssertion, RUNTIME_AUTH_HEADER, RuntimeAssertion, SessionAuthRequest,
    runtime_action,
};
use leveler_remote_protocol::tunnel::{RpcMethod, RpcRequestPayload, rpc_stream_id};
use leveler_remote_protocol::{ContentType, Sender, SignedEnvelope, SigningKey};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

const KEY_SEED: [u8; 32] = [90u8; 32];
const ENROLLMENT_SECRET: &str = "operator-secret";
const RUNTIME_ID: &str = "rt_a";
const DEVICE_ID: &str = "dev_a";

/// One key stands in for the device and the runtime alike: the relay verifies
/// neither signature — that is the agent's and the phone's job — so these tests
/// are about routing.
fn key() -> SigningKey {
    SigningKey::from_seed(&KEY_SEED).unwrap()
}

fn now_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn urlencode(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '+' => "%2B".to_string(),
            '/' => "%2F".to_string(),
            '=' => "%3D".to_string(),
            other => other.to_string(),
        })
        .collect()
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn runtime_auth(action: &str, runtime_id: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = format!("n{}", NEXT.fetch_add(1, Ordering::SeqCst));
    RuntimeAssertion::header_value(&key(), action, runtime_id, &now_stamp(), &nonce)
}

async fn serve() -> (String, RelayState) {
    let state = RelayState::with_enrollment_secret(ENROLLMENT_SECRET);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = build_router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (address.to_string(), state)
}

/// Enroll a host and pair one phone to it, returning the phone's access token.
async fn pair_device(base: &str, runtime_id: &str, device_id: &str) -> String {
    let client = reqwest::Client::new();
    let http = format!("http://{base}");
    client
        .post(format!("{http}/v1/runtimes/enroll"))
        .bearer_auth(ENROLLMENT_SECRET)
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(runtime_action::ENROLL, runtime_id),
        )
        .json(&json!({
            "runtime_id": runtime_id,
            "runtime_pubkey": key().verifying_key().to_base64url()
        }))
        .send()
        .await
        .unwrap();
    let begin: serde_json::Value = client
        .post(format!("{http}/v1/pair/begin"))
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(runtime_action::PAIR_BEGIN, runtime_id),
        )
        .json(&json!({"runtime_id": runtime_id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let complete: serde_json::Value = client
        .post(format!("{http}/v1/pair/complete"))
        .json(&json!({
            "device_id": device_id, "device_pubkey": key().verifying_key().to_base64url(),
            "device_name": "iPhone", "platform": "ios",
            "pairing_secret": begin["pairing_secret"].as_str().unwrap(),
            "scope": "interactive"
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
            runtime_auth(runtime_action::PAIR_CONFIRM, runtime_id),
        )
        .json(&json!({
            "runtime_id": runtime_id,
            "pairing_id": complete["pairing_id"].as_str().unwrap(),
            "decision": "accept"
        }))
        .send()
        .await
        .unwrap();

    let timestamp = now_stamp();
    let nonce = format!("n-{device_id}-{runtime_id}");
    let input = SessionAuthRequest::signing_input(device_id, runtime_id, &timestamp, &nonce);
    let auth: serde_json::Value = client
        .post(format!("{http}/v1/auth/session"))
        .json(&json!({
            "device_id": device_id, "runtime_id": runtime_id,
            "timestamp": timestamp, "nonce": nonce,
            "sig": b64(&key().sign_detached(input.as_bytes())),
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    auth["access_token"].as_str().unwrap().to_string()
}

async fn connect_agent(base: &str, runtime_id: &str) -> Socket {
    let timestamp = now_stamp();
    let assertion = AgentRegisterAssertion::signing_input(runtime_id, &timestamp);
    let sig = urlencode(&b64(&key().sign_detached(assertion.as_bytes())));
    let url = format!(
        "ws://{base}/v1/agent/tunnel?runtime_id={runtime_id}&display_name=dev-box&timestamp={timestamp}&sig={sig}"
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let ack = next_json(&mut socket).await;
    assert_eq!(ack["type"], "register_ack");
    socket
}

async fn next_json(socket: &mut Socket) -> serde_json::Value {
    let message = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("a frame should arrive")
        .expect("stream is open")
        .expect("no socket error");
    match message {
        Message::Text(text) => serde_json::from_str(&text).expect("json frame"),
        other => panic!("expected text, got {other:?}"),
    }
}

/// A device-signed RPC request, as the phone builds it.
fn rpc_request(method: RpcMethod, project_id: Option<&str>, uuid: &str) -> SignedEnvelope {
    let payload = serde_json::to_vec(&RpcRequestPayload {
        method,
        project_id: project_id.map(|id| id.to_string()),
        body: json!({}),
    })
    .unwrap();
    SignedEnvelope::sign(
        &key(),
        Sender::Device,
        DEVICE_ID,
        RUNTIME_ID,
        &rpc_stream_id(uuid),
        1,
        &now_stamp(),
        ContentType::RpcRequest,
        &payload,
    )
    .unwrap()
}

/// What an agent answers with: a runtime-signed envelope on the request's own
/// stream id.
fn rpc_response(uuid: &str, body: &serde_json::Value) -> SignedEnvelope {
    SignedEnvelope::sign(
        &key(),
        Sender::Runtime,
        RUNTIME_ID,
        DEVICE_ID,
        &rpc_stream_id(uuid),
        1,
        &now_stamp(),
        ContentType::RpcResponse,
        body.to_string().as_bytes(),
    )
    .unwrap()
}

/// The path the phone's project list and session creation travel.
#[tokio::test]
async fn an_rpc_reaches_the_agent_and_its_signed_answer_comes_back() {
    let (base, _state) = serve().await;
    let token = pair_device(&base, RUNTIME_ID, DEVICE_ID).await;
    let mut agent = connect_agent(&base, RUNTIME_ID).await;

    let request = rpc_request(RpcMethod::ListProjects, None, "aaaa");
    let posted = tokio::spawn({
        let base = base.clone();
        let token = token.clone();
        let request = request.clone();
        async move {
            reqwest::Client::new()
                .post(format!("http://{base}/v1/hosts/{RUNTIME_ID}/rpc"))
                .bearer_auth(token)
                .json(&request)
                .send()
                .await
                .unwrap()
        }
    });

    // The agent sees the device's envelope, untouched.
    let forwarded = next_json(&mut agent).await;
    assert_eq!(forwarded["type"], "rpc_request");
    assert_eq!(
        forwarded["envelope"],
        serde_json::to_value(&request).unwrap(),
        "the relay must not edit the envelope it carries"
    );

    let answer = rpc_response("aaaa", &json!([{"project_id": "abc", "status": "online"}]));
    agent
        .send(Message::Text(
            json!({
                "type": "rpc_response",
                "rpc_id": forwarded["rpc_id"],
                "envelope": answer,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let response = posted.await.unwrap();
    assert_eq!(response.status(), 200);
    let body: SignedEnvelope = response.json().await.unwrap();
    assert_eq!(
        body, answer,
        "the phone must receive the runtime's own signed envelope"
    );
}

/// An RPC aimed at a host with no tunnel is refused, not held for later: a
/// command accepted now and run after a revocation is the queue the design
/// forbids.
#[tokio::test]
async fn an_rpc_for_an_offline_host_is_refused_immediately() {
    let (base, _state) = serve().await;
    let token = pair_device(&base, RUNTIME_ID, DEVICE_ID).await;

    let response = reqwest::Client::new()
        .post(format!("http://{base}/v1/hosts/{RUNTIME_ID}/rpc"))
        .bearer_auth(&token)
        .json(&rpc_request(RpcMethod::CreateSession, None, "bbbb"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers().get("Retry-After").unwrap(), "5");
}

/// A routing failure carries no body. The runtime never produced a result, so
/// there is nothing it could have signed — and the relay must not invent one.
#[tokio::test]
async fn an_agents_routing_error_arrives_without_a_body() {
    let (base, _state) = serve().await;
    let token = pair_device(&base, RUNTIME_ID, DEVICE_ID).await;
    let mut agent = connect_agent(&base, RUNTIME_ID).await;

    let posted = tokio::spawn({
        let base = base.clone();
        let token = token.clone();
        async move {
            reqwest::Client::new()
                .post(format!("http://{base}/v1/hosts/{RUNTIME_ID}/rpc"))
                .bearer_auth(token)
                .json(&rpc_request(RpcMethod::Snapshot, Some("nope"), "cccc"))
                .send()
                .await
                .unwrap()
        }
    });

    let forwarded = next_json(&mut agent).await;
    agent
        .send(Message::Text(
            json!({
                "type": "rpc_response",
                "rpc_id": forwarded["rpc_id"],
                "error": {"code": "unknown_project", "message": "no such project"},
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let response = posted.await.unwrap();
    assert_eq!(response.status(), 502);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["code"], "unknown_project");
    assert!(
        body.get("envelope").is_none(),
        "a routing error must not carry a business body"
    );
}

/// An agent that goes away mid-RPC leaves nothing pending: the phone is told the
/// host is gone rather than waiting on an answer that will never come.
#[tokio::test]
async fn an_agent_disappearing_mid_rpc_ends_the_request() {
    let (base, _state) = serve().await;
    let token = pair_device(&base, RUNTIME_ID, DEVICE_ID).await;
    let mut agent = connect_agent(&base, RUNTIME_ID).await;

    let posted = tokio::spawn({
        let base = base.clone();
        let token = token.clone();
        async move {
            reqwest::Client::new()
                .post(format!("http://{base}/v1/hosts/{RUNTIME_ID}/rpc"))
                .bearer_auth(token)
                .json(&rpc_request(RpcMethod::Snapshot, None, "dddd"))
                .send()
                .await
                .unwrap()
        }
    });

    next_json(&mut agent).await;
    agent.close(None).await.unwrap();
    drop(agent);

    let response = posted.await.unwrap();
    assert_eq!(response.status(), 503);
}

/// A token for one host cannot drive an RPC at another.
#[tokio::test]
async fn an_rpc_cannot_be_aimed_at_another_host() {
    let (base, _state) = serve().await;
    let token = pair_device(&base, RUNTIME_ID, DEVICE_ID).await;
    let _other = pair_device(&base, "rt_b", "dev_b").await;
    let _agent = connect_agent(&base, "rt_b").await;

    let response = reqwest::Client::new()
        .post(format!("http://{base}/v1/hosts/rt_b/rpc"))
        .bearer_auth(&token)
        .json(&rpc_request(RpcMethod::ListProjects, None, "eeee"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

/// The envelope's sender must be the device the token was minted for. The
/// agent's signature check would catch this too; refusing here keeps one
/// device's traffic from being carried under another's authorization at all.
#[tokio::test]
async fn an_envelope_from_another_device_is_refused() {
    let (base, _state) = serve().await;
    let token = pair_device(&base, RUNTIME_ID, DEVICE_ID).await;
    let mut agent = connect_agent(&base, RUNTIME_ID).await;

    let foreign = SignedEnvelope::sign(
        &key(),
        Sender::Device,
        "dev_someone_else",
        RUNTIME_ID,
        &rpc_stream_id("ffff"),
        1,
        &now_stamp(),
        ContentType::RpcRequest,
        b"{}",
    )
    .unwrap();

    let response = reqwest::Client::new()
        .post(format!("http://{base}/v1/hosts/{RUNTIME_ID}/rpc"))
        .bearer_auth(&token)
        .json(&foreign)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    // And the agent was never bothered with it.
    let idle = tokio::time::timeout(std::time::Duration::from_millis(200), agent.next()).await;
    assert!(idle.is_err(), "nothing should have been forwarded");
}
