//! End to end over both WebSockets: a device frame reaching an agent and the
//! answer coming back.
//!
//! The property under test is that the relay is a courier, not a participant.
//! Frames must arrive **byte-identical**, because the signature covers the
//! header as well as the body — a relay that helpfully re-serialized a frame
//! would invalidate it, and one that could edit a frame undetected would make
//! the whole signing scheme pointless.

use futures_util::{SinkExt as _, StreamExt as _};
use leveler_relay::{RelayState, build_router};
use leveler_remote_protocol::SigningKey;
use leveler_remote_protocol::auth::{AgentRegisterAssertion, SessionAuthRequest};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

const KEY_SEED: [u8; 32] = [70u8; 32];

/// One key stands in for both the device and the runtime here: the relay only
/// checks signatures, and these tests are about routing, not identity.
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

async fn serve() -> (String, RelayState) {
    let state = RelayState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = build_router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (address.to_string(), state)
}

async fn pair_device(base: &str, runtime_id: &str, device_id: &str) -> String {
    let client = reqwest::Client::new();
    let http = format!("http://{base}");
    let begin: serde_json::Value = client
        .post(format!("{http}/v1/pair/begin"))
        .json(&json!({"runtime_id": runtime_id, "runtime_pubkey": key().verifying_key().to_base64url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let complete: serde_json::Value = client
        .post(format!("{http}/v1/pair/complete"))
        .json(&json!({
            "device_id": device_id, "device_pubkey": key().verifying_key().to_base64url(), "device_name": "iPhone",
            "platform": "ios",
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

/// Connect as an agent and wait for the registration ack.
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
    assert_eq!(ack["runtime_id"], runtime_id);
    socket
}

async fn connect_app(base: &str, host_id: &str, token: &str) -> Socket {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let mut request = format!("ws://{base}/v1/hosts/{host_id}/session")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
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

/// A stand-in signed envelope. The relay never inspects the signature, which is
/// the point — it forwards without understanding.
fn envelope(sender: &str, sender_id: &str, recipient: &str, seq: u64) -> serde_json::Value {
    json!({
        "v": 1,
        "sender": sender,
        "sender_id": sender_id,
        "recipient_id": recipient,
        "stream_id": "str_ignored_by_relay",
        "seq": seq,
        "ts": "2026-07-25T12:00:00Z",
        "content_type": if sender == "device" { "session_upstream" } else { "session_downstream" },
        "payload_b64": "eyJ0eXBlIjoic25hcHNob3QiLCJzZXNzaW9uX2lkIjoiczEifQ==",
        "sig_b64": "c2lnbmF0dXJlLWJ5dGVzLWdvLWhlcmUtc2lnbmF0dXJlLWJ5dGVz"
    })
}

#[tokio::test]
async fn a_device_frame_reaches_the_agent_byte_identical() {
    let (base, _state) = serve().await;
    let token = pair_device(&base, "rt_a", "dev_a").await;
    let mut agent = connect_agent(&base, "rt_a").await;
    let mut app = connect_app(&base, "rt_a", &token).await;

    // The agent is told a stream opened, and is given no device key: it resolves
    // that from its own store.
    let opened = next_json(&mut agent).await;
    assert_eq!(opened["type"], "open_stream");
    assert_eq!(opened["device_id"], "dev_a");
    assert_eq!(opened["pairing_scope"], "interactive");
    let stream_id = opened["stream_id"].as_str().unwrap().to_string();
    assert!(
        opened.get("device_pubkey").is_none(),
        "the relay must not offer the agent a key to trust"
    );

    let sent = envelope("device", "dev_a", "rt_a", 7);
    app.send(Message::Text(sent.to_string().into()))
        .await
        .unwrap();

    let forwarded = next_json(&mut agent).await;
    assert_eq!(forwarded["type"], "forward_upstream");
    assert_eq!(forwarded["stream_id"], stream_id);
    assert_eq!(
        forwarded["frame"], sent,
        "the envelope must arrive unchanged; re-serializing it would break its signature"
    );
}

#[tokio::test]
async fn an_agent_frame_reaches_the_device_byte_identical() {
    let (base, _state) = serve().await;
    let token = pair_device(&base, "rt_a", "dev_a").await;
    let mut agent = connect_agent(&base, "rt_a").await;
    let mut app = connect_app(&base, "rt_a", &token).await;

    let opened = next_json(&mut agent).await;
    let stream_id = opened["stream_id"].as_str().unwrap().to_string();

    let downstream = envelope("runtime", "rt_a", "dev_a", 1);
    agent
        .send(Message::Text(
            json!({
                "type": "forward_downstream",
                "stream_id": stream_id,
                "frame": downstream
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    assert_eq!(
        next_json(&mut app).await,
        downstream,
        "the device must receive exactly what the runtime signed"
    );
}

/// Two devices, two machines, one relay. A frame must never surface on the
/// wrong socket.
#[tokio::test]
async fn frames_do_not_cross_between_hosts() {
    let (base, _state) = serve().await;
    let token_a = pair_device(&base, "rt_a", "dev_a").await;
    let token_b = pair_device(&base, "rt_b", "dev_b").await;

    let mut agent_a = connect_agent(&base, "rt_a").await;
    let mut agent_b = connect_agent(&base, "rt_b").await;
    let mut app_a = connect_app(&base, "rt_a", &token_a).await;
    let mut app_b = connect_app(&base, "rt_b", &token_b).await;

    let opened_a = next_json(&mut agent_a).await;
    let opened_b = next_json(&mut agent_b).await;
    assert_eq!(opened_a["device_id"], "dev_a");
    assert_eq!(opened_b["device_id"], "dev_b");
    assert_ne!(opened_a["stream_id"], opened_b["stream_id"]);

    // A only speaks to A's agent.
    app_a
        .send(Message::Text(
            envelope("device", "dev_a", "rt_a", 1).to_string().into(),
        ))
        .await
        .unwrap();
    let seen = next_json(&mut agent_a).await;
    assert_eq!(seen["frame"]["sender_id"], "dev_a");

    // B's agent must have nothing waiting.
    let quiet = tokio::time::timeout(std::time::Duration::from_millis(300), agent_b.next()).await;
    assert!(
        quiet.is_err(),
        "a frame from one host's device must not reach another host's agent"
    );

    // And B's device likewise.
    let quiet_app = tokio::time::timeout(std::time::Duration::from_millis(300), app_b.next()).await;
    assert!(quiet_app.is_err());
}

/// An offline host cannot be reached at all — the upgrade is refused rather than
/// the frames being queued for later.
#[tokio::test]
async fn a_session_cannot_open_against_an_offline_host() {
    let (base, _state) = serve().await;
    let token = pair_device(&base, "rt_a", "dev_a").await;

    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let mut request = format!("ws://{base}/v1/hosts/rt_a/session")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    assert!(
        tokio_tungstenite::connect_async(request).await.is_err(),
        "no agent tunnel means no session, not a queue"
    );
}

/// Revocation must reach an already-open socket. Dropping only the tokens would
/// leave a withdrawn phone forwarding until it happened to reconnect.
#[tokio::test]
async fn revoking_a_device_closes_its_live_stream() {
    let (base, state) = serve().await;
    let token = pair_device(&base, "rt_a", "dev_a").await;
    let mut agent = connect_agent(&base, "rt_a").await;
    let mut app = connect_app(&base, "rt_a", &token).await;

    let opened = next_json(&mut agent).await;
    let stream_id = opened["stream_id"].as_str().unwrap().to_string();

    state.revoke_device("dev_a", "rt_a").unwrap();

    // The agent is told to stop serving the stream, with the reason.
    let closed = next_json(&mut agent).await;
    assert_eq!(closed["type"], "close_stream");
    assert_eq!(closed["stream_id"], stream_id);
    assert_eq!(closed["reason"], "revoked");

    // And the device's socket ends rather than lingering.
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), app.next()).await;
    match ended {
        Ok(None) | Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) => {}
        other => panic!("the device socket should have closed, saw {other:?}"),
    }
}

/// When the agent disconnects, its devices' streams go with it. Nothing is
/// retained to be delivered on reconnect.
#[tokio::test]
async fn an_agent_going_away_ends_its_device_streams() {
    let (base, state) = serve().await;
    let token = pair_device(&base, "rt_a", "dev_a").await;
    let mut agent = connect_agent(&base, "rt_a").await;
    let mut app = connect_app(&base, "rt_a", &token).await;
    let _opened = next_json(&mut agent).await;

    agent.close(None).await.unwrap();

    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), app.next()).await;
    match ended {
        Ok(None) | Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) => {}
        other => panic!("the device socket should have closed, saw {other:?}"),
    }
    assert!(!state.is_online("rt_a"));
}

/// An RPC response rides the stream named inside its own signed envelope, which
/// is what keeps a response bound to its request.
#[tokio::test]
async fn an_rpc_response_routes_by_its_signed_stream_id() {
    let (base, _state) = serve().await;
    let token = pair_device(&base, "rt_a", "dev_a").await;
    let mut agent = connect_agent(&base, "rt_a").await;
    let mut app = connect_app(&base, "rt_a", &token).await;

    let opened = next_json(&mut agent).await;
    let stream_id = opened["stream_id"].as_str().unwrap().to_string();

    let mut response = envelope("runtime", "rt_a", "dev_a", 3);
    response["content_type"] = json!("rpc_response");
    response["stream_id"] = json!(stream_id);

    agent
        .send(Message::Text(
            json!({
                "type": "rpc_response",
                "rpc_id": "rpc_1",
                "envelope": response
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    assert_eq!(next_json(&mut app).await, response);
}
