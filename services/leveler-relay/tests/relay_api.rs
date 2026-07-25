//! The relay's guarantees, stated as the failures that would matter.
//!
//! Isolation is the theme. Two phones paired to two machines must not be able
//! to reach each other's hosts, and a withdrawn phone must lose access on its
//! next request rather than whenever a token would have expired.

use leveler_relay::{RelayState, build_router};
use leveler_remote_protocol::SigningKey;
use leveler_remote_protocol::auth::{
    RUNTIME_AUTH_HEADER, RuntimeAssertion, SessionAuthRequest, runtime_action,
};
use serde_json::json;
use tokio::sync::mpsc;

const DEVICE_SEED: [u8; 32] = [70u8; 32];
const RUNTIME_SEED: [u8; 32] = [71u8; 32];
/// What an operator configures on their own relay.
const ENROLLMENT_SECRET: &str = "operator-secret";

fn device_key() -> SigningKey {
    SigningKey::from_seed(&DEVICE_SEED).unwrap()
}

/// The key a developer machine enrolls and then signs its control-plane
/// requests with. Distinct per machine, so a test that crosses machines cannot
/// pass by accident on a shared key.
fn runtime_key(runtime_id: &str) -> SigningKey {
    let mut seed = RUNTIME_SEED;
    seed[0] = runtime_id
        .bytes()
        .fold(0u8, |accumulated, byte| accumulated.wrapping_add(byte));
    SigningKey::from_seed(&seed).unwrap()
}

/// A fresh nonce per assertion, since the relay spends each one exactly once.
fn nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("n{}", NEXT.fetch_add(1, Ordering::SeqCst))
}

/// The header a runtime attaches to prove a control-plane request is its own.
fn runtime_auth(key: &SigningKey, action: &str, runtime_id: &str) -> String {
    RuntimeAssertion::header_value(key, action, runtime_id, &now_stamp(), &nonce())
}

/// Register a machine's public key with the relay, the way an operator would.
async fn enroll(client: &reqwest::Client, base: &str, runtime_id: &str, key: &SigningKey) {
    let response = client
        .post(format!("{base}/v1/runtimes/enroll"))
        .bearer_auth(ENROLLMENT_SECRET)
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(key, runtime_action::ENROLL, runtime_id),
        )
        .json(&json!({
            "runtime_id": runtime_id,
            "runtime_pubkey": key.verifying_key().to_base64url(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204, "enrollment should succeed");
}

/// Begin a pairing as the enrolled runtime does, returning the one-shot secret.
async fn begin_pairing(client: &reqwest::Client, base: &str, runtime_id: &str) -> String {
    let response = client
        .post(format!("{base}/v1/pair/begin"))
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(
                &runtime_key(runtime_id),
                runtime_action::PAIR_BEGIN,
                runtime_id,
            ),
        )
        .json(&json!({"runtime_id": runtime_id}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "pair/begin should succeed");
    let body: serde_json::Value = response.json().await.unwrap();
    body["pairing_secret"].as_str().unwrap().to_string()
}

/// Accept a pending pairing as the host user does.
async fn confirm_pairing(
    client: &reqwest::Client,
    base: &str,
    runtime_id: &str,
    pairing_id: &str,
) -> reqwest::Response {
    client
        .post(format!("{base}/v1/pair/confirm"))
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(
                &runtime_key(runtime_id),
                runtime_action::PAIR_CONFIRM,
                runtime_id,
            ),
        )
        .json(&json!({
            "runtime_id": runtime_id,
            "pairing_id": pairing_id,
            "decision": "accept"
        }))
        .send()
        .await
        .unwrap()
}

fn now_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Sign the device assertion `/v1/auth/session` now requires. The nonce varies
/// per call so a test can authenticate more than once.
fn device_auth_body(device_id: &str, runtime_id: &str, nonce: &str) -> serde_json::Value {
    let timestamp = now_stamp();
    let input = SessionAuthRequest::signing_input(device_id, runtime_id, &timestamp, nonce);
    json!({
        "device_id": device_id,
        "runtime_id": runtime_id,
        "timestamp": timestamp,
        "nonce": nonce,
        "sig": b64(&device_key().sign_detached(input.as_bytes())),
    })
}

/// The refresh body, including the device assertion the relay requires.
fn refresh_body(device_id: &str, refresh_token: &str) -> serde_json::Value {
    let timestamp = now_stamp();
    let input =
        leveler_remote_protocol::auth::DeviceAssertion::signing_input(device_id, &timestamp);
    json!({
        "refresh_token": refresh_token,
        "device_assertion": {
            "device_id": device_id,
            "timestamp": timestamp,
            "sig": b64(&device_key().sign_detached(input.as_bytes())),
        }
    })
}

/// Probe a host with a device-signed RPC: 503 when the token is good but no
/// agent is attached, 401 when the token is not good for that host. Used to ask
/// "is this token still authorized here" without needing a live agent.
async fn probe(
    client: &reqwest::Client,
    base: &str,
    host_id: &str,
    device_id: &str,
    token: &str,
) -> reqwest::StatusCode {
    let envelope = leveler_remote_protocol::SignedEnvelope::sign(
        &device_key(),
        leveler_remote_protocol::Sender::Device,
        device_id,
        host_id,
        &leveler_remote_protocol::tunnel::rpc_stream_id("probe"),
        1,
        &now_stamp(),
        leveler_remote_protocol::ContentType::RpcRequest,
        b"{}",
    )
    .unwrap();
    client
        .post(format!("{base}/v1/hosts/{host_id}/rpc"))
        .bearer_auth(token)
        .json(&envelope)
        .send()
        .await
        .unwrap()
        .status()
}

/// Register a runtime as online the way the tunnel does, keeping the receiver
/// alive so the route stays valid for the duration of the test.
fn bring_online(
    state: &RelayState,
    runtime_id: &str,
    display_name: &str,
) -> mpsc::UnboundedReceiver<leveler_remote_protocol::tunnel::RelayToAgent> {
    let (tx, rx) = mpsc::unbounded_channel();
    state.register_runtime(runtime_id, display_name, tx);
    rx
}

/// Start the router on an ephemeral port and return its base URL.
async fn serve() -> (String, RelayState) {
    let state = RelayState::with_enrollment_secret(ENROLLMENT_SECRET);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = build_router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{address}"), state)
}

/// Walk one phone through pairing and return its access token.
async fn pair_device(
    client: &reqwest::Client,
    base: &str,
    runtime_id: &str,
    device_id: &str,
) -> String {
    enroll(client, base, runtime_id, &runtime_key(runtime_id)).await;
    let secret = begin_pairing(client, base, runtime_id).await;

    let complete = client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": device_id,
            "device_pubkey": device_key().verifying_key().to_base64url(),
            "device_name": "iPhone",
            "platform": "ios",
            "pairing_secret": secret,
            "scope": "interactive"
        }))
        .send()
        .await
        .unwrap();
    assert!(complete.status().is_success(), "claim should succeed");
    let complete: serde_json::Value = complete.json().await.unwrap();
    let pairing_id = complete["pairing_id"].as_str().unwrap().to_string();

    let confirm = confirm_pairing(client, base, runtime_id, &pairing_id).await;
    assert_eq!(confirm.status(), 204);

    let auth: serde_json::Value = client
        .post(format!("{base}/v1/auth/session"))
        .json(&device_auth_body(
            device_id,
            runtime_id,
            &format!("n{}", line!()),
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    auth["access_token"].as_str().unwrap().to_string()
}

/// A device cannot promote its own pairing: the host has to accept it.
#[tokio::test]
async fn a_device_cannot_activate_its_own_pairing() {
    let (base, state) = serve().await;
    let client = reqwest::Client::new();

    enroll(&client, &base, "rt_a", &runtime_key("rt_a")).await;
    let secret = begin_pairing(&client, &base, "rt_a").await;

    client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_a", "device_pubkey": device_key().verifying_key().to_base64url(), "device_name": "iPhone",
            "platform": "ios", "pairing_secret": secret, "scope": "interactive"
        }))
        .send()
        .await
        .unwrap();

    // Claimed but not confirmed: no device record, so no token can be issued.
    assert!(state.device("dev_a").is_none());
    let auth = client
        .post(format!("{base}/v1/auth/session"))
        .json(&device_auth_body("dev_a", "rt_a", &format!("n{}", line!())))
        .send()
        .await
        .unwrap();
    assert_eq!(auth.status(), 401, "an unconfirmed device gets no token");
}

/// A wrong secret is refused, and indistinguishably from an unknown one so the
/// endpoint cannot be used to enumerate live pairings.
#[tokio::test]
async fn a_wrong_pairing_secret_is_refused() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();
    enroll(&client, &base, "rt_a", &runtime_key("rt_a")).await;
    begin_pairing(&client, &base, "rt_a").await;

    let wrong = client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_a", "device_pubkey": device_key().verifying_key().to_base64url(), "device_name": "iPhone",
            "platform": "ios", "pairing_secret": "not-the-secret", "scope": "interactive"
        }))
        .send()
        .await
        .unwrap();
    let status = wrong.status();
    let body: serde_json::Value = wrong.json().await.unwrap();
    assert_eq!(status, 400);
    assert_eq!(body["code"], "invalid_pairing");

    let absent = client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_b", "device_pubkey": device_key().verifying_key().to_base64url(), "device_name": "iPad",
            "platform": "ios", "pairing_secret": "also-not-it", "scope": "interactive"
        }))
        .send()
        .await
        .unwrap();
    let absent_status = absent.status();
    let absent_body: serde_json::Value = absent.json().await.unwrap();
    assert_eq!(absent_status, status, "failures must look alike");
    assert_eq!(absent_body["code"], body["code"]);
}

/// The isolation the design requires: two phones, two machines, no crossing.
#[tokio::test]
async fn a_token_for_one_host_is_inert_against_another() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();

    let token_a = pair_device(&client, &base, "rt_a", "dev_a").await;
    let token_b = pair_device(&client, &base, "rt_b", "dev_b").await;

    // Each is authorized against its own host. Neither host has an agent, so the
    // answer is "offline" rather than a result — which is exactly the point: it
    // got past authorization.
    for (token, host, device) in [(&token_a, "rt_a", "dev_a"), (&token_b, "rt_b", "dev_b")] {
        assert_eq!(
            probe(&client, &base, host, device, token).await,
            503,
            "a device's own host is reachable, just not attached"
        );
    }

    // Neither reaches the other's.
    for (token, host, device) in [(&token_a, "rt_b", "dev_a"), (&token_b, "rt_a", "dev_b")] {
        assert_eq!(
            probe(&client, &base, host, device, token).await,
            401,
            "a token minted for one host must not reach another"
        );
    }
}

/// A device only ever sees the host it is paired to.
#[tokio::test]
async fn a_device_only_sees_its_own_host() {
    let (base, state) = serve().await;
    let client = reqwest::Client::new();

    let token_a = pair_device(&client, &base, "rt_a", "dev_a").await;
    pair_device(&client, &base, "rt_b", "dev_b").await;
    let _agent_rt_a = bring_online(&state, "rt_a", "machine A");
    let _agent_rt_b = bring_online(&state, "rt_b", "machine B");

    let hosts: serde_json::Value = client
        .get(format!("{base}/v1/hosts"))
        .bearer_auth(&token_a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let hosts = hosts.as_array().unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0]["host_id"], "rt_a");
}

/// An offline host produces 503 with a retry hint — never a queued command.
#[tokio::test]
async fn an_offline_host_is_503_with_a_retry_hint() {
    let (base, state) = serve().await;
    let client = reqwest::Client::new();
    let token = pair_device(&client, &base, "rt_a", "dev_a").await;

    // Never registered.
    let envelope = leveler_remote_protocol::SignedEnvelope::sign(
        &device_key(),
        leveler_remote_protocol::Sender::Device,
        "dev_a",
        "rt_a",
        &leveler_remote_protocol::tunnel::rpc_stream_id("probe"),
        1,
        &now_stamp(),
        leveler_remote_protocol::ContentType::RpcRequest,
        b"{}",
    )
    .unwrap();
    let response = client
        .post(format!("{base}/v1/hosts/rt_a/rpc"))
        .bearer_auth(&token)
        .json(&envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.headers().get("Retry-After").unwrap(),
        "5",
        "a client should be told how long to wait, not left guessing"
    );

    // Comes online, then goes away again: still 503, and nothing was retained
    // from the earlier attempt.
    let _agent_rt_a = bring_online(&state, "rt_a", "machine A");
    assert!(state.is_online("rt_a"));
    state.unregister_runtime("rt_a");
    assert_eq!(
        probe(&client, &base, "rt_a", "dev_a", &token).await,
        503,
        "nothing was retained from the earlier attempt"
    );
}

/// Revocation takes effect on the next request. Dropping the tokens with the
/// device is what makes that true without an access-token denylist.
#[tokio::test]
async fn revoking_a_device_invalidates_its_tokens_at_once() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();
    let token = pair_device(&client, &base, "rt_a", "dev_a").await;

    assert_eq!(
        probe(&client, &base, "rt_a", "dev_a", &token).await,
        503,
        "authorized before revocation"
    );

    let revoke = client
        .delete(format!("{base}/v1/devices/dev_a?runtime_id=rt_a"))
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(&runtime_key("rt_a"), runtime_action::DEVICE_REVOKE, "rt_a"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status(), 204);

    assert_eq!(
        probe(&client, &base, "rt_a", "dev_a", &token).await,
        401,
        "the same token must stop working immediately"
    );

    // And it cannot simply ask for a new one.
    let reauth = client
        .post(format!("{base}/v1/auth/session"))
        .json(&device_auth_body("dev_a", "rt_a", &format!("n{}", line!())))
        .send()
        .await
        .unwrap();
    assert_eq!(reauth.status(), 401);
}

/// One machine cannot revoke another's devices.
#[tokio::test]
async fn a_runtime_cannot_revoke_another_runtimes_device() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();
    pair_device(&client, &base, "rt_a", "dev_a").await;

    enroll(&client, &base, "rt_b", &runtime_key("rt_b")).await;
    let response = client
        .delete(format!("{base}/v1/devices/dev_a?runtime_id=rt_b"))
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(&runtime_key("rt_b"), runtime_action::DEVICE_REVOKE, "rt_b"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        404,
        "rt_b is a legitimate machine, but this is not its device"
    );
}

/// Refresh rotates, and replaying a spent token is treated as theft rather than
/// as a retry — the device loses everything and must authenticate again.
#[tokio::test]
async fn a_replayed_refresh_token_costs_the_device_its_session() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();

    enroll(&client, &base, "rt_a", &runtime_key("rt_a")).await;
    let secret = begin_pairing(&client, &base, "rt_a").await;
    let complete: serde_json::Value = client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_a", "device_pubkey": device_key().verifying_key().to_base64url(), "device_name": "iPhone",
            "platform": "ios", "pairing_secret": secret, "scope": "interactive"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    confirm_pairing(
        &client,
        &base,
        "rt_a",
        complete["pairing_id"].as_str().unwrap(),
    )
    .await;
    let auth: serde_json::Value = client
        .post(format!("{base}/v1/auth/session"))
        .json(&device_auth_body("dev_a", "rt_a", &format!("n{}", line!())))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first_refresh = auth["refresh_token"].as_str().unwrap().to_string();

    let rotated: serde_json::Value = client
        .post(format!("{base}/v1/auth/refresh"))
        .json(&refresh_body("dev_a", &first_refresh))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fresh_access = rotated["access_token"].as_str().unwrap().to_string();
    assert_ne!(
        rotated["refresh_token"].as_str().unwrap(),
        first_refresh,
        "the refresh token must rotate"
    );

    // Replay the spent one.
    let replay = client
        .post(format!("{base}/v1/auth/refresh"))
        .json(&refresh_body("dev_a", &first_refresh))
        .send()
        .await
        .unwrap();
    let replay_status = replay.status();
    let replay_body: serde_json::Value = replay.json().await.unwrap();
    assert_eq!(replay_status, 401);
    assert_eq!(replay_body["code"], "reuse_detected");

    // The access token handed out moments ago is gone too: a captured refresh
    // token means the device's whole session is suspect.
    assert_eq!(
        probe(&client, &base, "rt_a", "dev_a", &fresh_access).await,
        401
    );
}

/// A second `begin` cancels the first, so a stale QR on a screen cannot be
/// claimed later.
#[tokio::test]
async fn beginning_a_second_pairing_retires_the_first_secret() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();

    enroll(&client, &base, "rt_a", &runtime_key("rt_a")).await;
    let stale = begin_pairing(&client, &base, "rt_a").await;
    begin_pairing(&client, &base, "rt_a").await;

    let response = client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_a", "device_pubkey": device_key().verifying_key().to_base64url(), "device_name": "iPhone",
            "platform": "ios", "pairing_secret": stale, "scope": "interactive"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400, "the retired secret must not work");
}

/// The host's confirmation prompt needs the device's public key, because the
/// fingerprint the user compares is derived from it. A name alone would let a
/// relay swap the key while the user recognised the label.
#[tokio::test]
async fn the_pending_confirmation_exposes_the_device_public_key() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();

    enroll(&client, &base, "rt_a", &runtime_key("rt_a")).await;
    let secret = begin_pairing(&client, &base, "rt_a").await;
    client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_a", "device_pubkey": "the-real-key", "device_name": "iPhone",
            "platform": "ios",
            "pairing_secret": secret,
            "scope": "interactive"
        }))
        .send()
        .await
        .unwrap();

    let pending: serde_json::Value = client
        .get(format!("{base}/v1/pair/pending?runtime_id=rt_a"))
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(&runtime_key("rt_a"), runtime_action::PAIR_PENDING, "rt_a"),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pending["device_pubkey"], "the-real-key");
    assert_eq!(pending["device_id"], "dev_a");
}

/// A `device_id` is not a secret, so authenticating on the strength of one
/// would let anyone who saw it mint tokens for that phone.
#[tokio::test]
async fn auth_requires_a_signature_from_the_paired_key() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();
    pair_device(&client, &base, "rt_a", "dev_a").await;

    // The right shape, signed by the wrong key.
    let impostor = SigningKey::from_seed(&[123u8; 32]).unwrap();
    let timestamp = now_stamp();
    let nonce = "n-impostor";
    let input = SessionAuthRequest::signing_input("dev_a", "rt_a", &timestamp, nonce);
    let response = client
        .post(format!("{base}/v1/auth/session"))
        .json(&json!({
            "device_id": "dev_a", "runtime_id": "rt_a",
            "timestamp": timestamp, "nonce": nonce,
            "sig": b64(&impostor.sign_detached(input.as_bytes())),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

/// A captured assertion stays signature-valid for its whole window; spending the
/// nonce is what stops it being replayed inside it.
#[tokio::test]
async fn an_auth_assertion_cannot_be_replayed() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();
    pair_device(&client, &base, "rt_a", "dev_a").await;

    let body = device_auth_body("dev_a", "rt_a", "n-replay");
    assert!(
        client
            .post(format!("{base}/v1/auth/session"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );

    let replay = client
        .post(format!("{base}/v1/auth/session"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        replay.status(),
        401,
        "the same assertion must not work twice"
    );
}

/// A runtime id is a name, not a credential. Once a machine is enrolled, a claim
/// signed by a different key is refused rather than treated as a rotation.
#[tokio::test]
async fn an_enrolled_runtime_id_cannot_be_rebound_to_another_key() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();
    enroll(&client, &base, "rt_a", &runtime_key("rt_a")).await;

    let impostor = SigningKey::from_seed(&[124u8; 32]).unwrap();
    let hijack = client
        .post(format!("{base}/v1/runtimes/enroll"))
        .bearer_auth(ENROLLMENT_SECRET)
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(&impostor, runtime_action::ENROLL, "rt_a"),
        )
        .json(&json!({
            "runtime_id": "rt_a",
            "runtime_pubkey": impostor.verifying_key().to_base64url()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        hijack.status(),
        401,
        "claiming another machine's runtime id must be refused"
    );
}

/// Enrollment is the one moment the relay learns a `runtime_id → pubkey`
/// binding. Trust-on-first-use would grant that binding to whoever asked first:
/// anyone able to reach the relay could become a host on it.
#[tokio::test]
async fn enrolling_requires_the_operators_secret() {
    let (base, state) = serve().await;
    let client = reqwest::Client::new();
    let key = runtime_key("rt_a");

    for authorization in [None, Some("not-the-secret")] {
        let mut request = client
            .post(format!("{base}/v1/runtimes/enroll"))
            .header(
                RUNTIME_AUTH_HEADER,
                runtime_auth(&key, runtime_action::ENROLL, "rt_a"),
            )
            .json(&json!({
                "runtime_id": "rt_a",
                "runtime_pubkey": key.verifying_key().to_base64url()
            }));
        if let Some(secret) = authorization {
            request = request.bearer_auth(secret);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 401, "secret {authorization:?}");
    }

    assert!(
        state.runtime_key("rt_a").is_none(),
        "a refused enrollment must not leave a binding behind"
    );
}

/// The secret says an operator permitted this relay to gain a host; the
/// signature says the caller actually holds the key it is registering. Without
/// the second, anyone who learned the secret could enroll a key they control
/// under someone else's machine name.
#[tokio::test]
async fn enrolling_requires_holding_the_key_being_registered() {
    let (base, state) = serve().await;
    let client = reqwest::Client::new();
    let impostor = SigningKey::from_seed(&[125u8; 32]).unwrap();

    let response = client
        .post(format!("{base}/v1/runtimes/enroll"))
        .bearer_auth(ENROLLMENT_SECRET)
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(&impostor, runtime_action::ENROLL, "rt_a"),
        )
        // Signed by the impostor, but registering the victim's key.
        .json(&json!({
            "runtime_id": "rt_a",
            "runtime_pubkey": runtime_key("rt_a").verifying_key().to_base64url()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(state.runtime_key("rt_a").is_none());
}

/// The hole this closes: `pair/begin` hands out the one secret that can pair a
/// phone to a machine. Unauthenticated, anyone who learned a `runtime_id` — a
/// name, not a credential — could take it.
#[tokio::test]
async fn beginning_a_pairing_requires_the_runtimes_signature() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();
    enroll(&client, &base, "rt_a", &runtime_key("rt_a")).await;

    let unsigned = client
        .post(format!("{base}/v1/pair/begin"))
        .json(&json!({"runtime_id": "rt_a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unsigned.status(), 401, "no assertion, no pairing secret");

    let impostor = SigningKey::from_seed(&[126u8; 32]).unwrap();
    let forged = client
        .post(format!("{base}/v1/pair/begin"))
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(&impostor, runtime_action::PAIR_BEGIN, "rt_a"),
        )
        .json(&json!({"runtime_id": "rt_a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 401);

    // Signed for a different operation: an assertion captured from one request
    // must not authorize another.
    let wrong_action = client
        .post(format!("{base}/v1/pair/begin"))
        .header(
            RUNTIME_AUTH_HEADER,
            runtime_auth(&runtime_key("rt_a"), runtime_action::DEVICE_REVOKE, "rt_a"),
        )
        .json(&json!({"runtime_id": "rt_a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_action.status(), 401);
}

/// Confirmation is the accept the whole trust model rests on. If it were
/// unauthenticated, a caller who claimed a pairing could also accept it, and the
/// user at the keyboard would never see a prompt.
#[tokio::test]
async fn confirming_a_pairing_requires_the_runtimes_signature() {
    let (base, state) = serve().await;
    let client = reqwest::Client::new();
    enroll(&client, &base, "rt_a", &runtime_key("rt_a")).await;
    let secret = begin_pairing(&client, &base, "rt_a").await;

    let complete: serde_json::Value = client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_a", "device_pubkey": device_key().verifying_key().to_base64url(),
            "device_name": "iPhone", "platform": "ios",
            "pairing_secret": secret, "scope": "interactive"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let self_accept = client
        .post(format!("{base}/v1/pair/confirm"))
        .json(&json!({
            "runtime_id": "rt_a",
            "pairing_id": complete["pairing_id"].as_str().unwrap(),
            "decision": "accept"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(self_accept.status(), 401);
    assert!(
        state.device("dev_a").is_none(),
        "no device record may exist without the host's own accept"
    );
}

/// Revocation and the device list are the runtime's own resources.
#[tokio::test]
async fn device_administration_requires_the_runtimes_signature() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();
    pair_device(&client, &base, "rt_a", "dev_a").await;

    let revoke = client
        .delete(format!("{base}/v1/devices/dev_a?runtime_id=rt_a"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        revoke.status(),
        401,
        "an unsigned revoke is a denial of service"
    );

    let list = client
        .get(format!("{base}/v1/devices?runtime_id=rt_a"))
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 401);
}

/// A captured runtime assertion stays signature-valid for its whole window.
#[tokio::test]
async fn a_runtime_assertion_cannot_be_replayed() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();
    enroll(&client, &base, "rt_a", &runtime_key("rt_a")).await;

    let header = runtime_auth(&runtime_key("rt_a"), runtime_action::PAIR_BEGIN, "rt_a");
    let first = client
        .post(format!("{base}/v1/pair/begin"))
        .header(RUNTIME_AUTH_HEADER, header.clone())
        .json(&json!({"runtime_id": "rt_a"}))
        .send()
        .await
        .unwrap();
    assert!(first.status().is_success());

    let replay = client
        .post(format!("{base}/v1/pair/begin"))
        .header(RUNTIME_AUTH_HEADER, header)
        .json(&json!({"runtime_id": "rt_a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        replay.status(),
        401,
        "the same assertion must not work twice"
    );
}

/// A refresh token alone is a bearer credential. The design pairs it with a
/// device assertion so a captured token cannot be spent by whoever holds it.
#[tokio::test]
async fn refreshing_requires_a_device_assertion() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();
    enroll(&client, &base, "rt_a", &runtime_key("rt_a")).await;
    let secret = begin_pairing(&client, &base, "rt_a").await;
    let complete: serde_json::Value = client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_a", "device_pubkey": device_key().verifying_key().to_base64url(),
            "device_name": "iPhone", "platform": "ios",
            "pairing_secret": secret, "scope": "interactive"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    confirm_pairing(
        &client,
        &base,
        "rt_a",
        complete["pairing_id"].as_str().unwrap(),
    )
    .await;
    let auth: serde_json::Value = client
        .post(format!("{base}/v1/auth/session"))
        .json(&device_auth_body("dev_a", "rt_a", &nonce()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let refresh_token = auth["refresh_token"].as_str().unwrap().to_string();

    let bare = client
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": refresh_token}))
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), 401, "a stolen refresh token is not enough");

    // The right shape from the wrong key fails too.
    let impostor = SigningKey::from_seed(&[127u8; 32]).unwrap();
    let timestamp = now_stamp();
    let input = leveler_remote_protocol::auth::DeviceAssertion::signing_input("dev_a", &timestamp);
    let forged = client
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({
            "refresh_token": refresh_token,
            "device_assertion": {
                "device_id": "dev_a",
                "timestamp": timestamp,
                "sig": b64(&impostor.sign_detached(input.as_bytes())),
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 401);

    // And the genuine one still works, so the check is not simply "always no".
    let timestamp = now_stamp();
    let input = leveler_remote_protocol::auth::DeviceAssertion::signing_input("dev_a", &timestamp);
    let genuine = client
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({
            "refresh_token": refresh_token,
            "device_assertion": {
                "device_id": "dev_a",
                "timestamp": timestamp,
                "sig": b64(&device_key().sign_detached(input.as_bytes())),
            }
        }))
        .send()
        .await
        .unwrap();
    assert!(genuine.status().is_success());
}
