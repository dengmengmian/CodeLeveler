//! The relay's guarantees, stated as the failures that would matter.
//!
//! Isolation is the theme. Two phones paired to two machines must not be able
//! to reach each other's hosts, and a withdrawn phone must lose access on its
//! next request rather than whenever a token would have expired.

use leveler_relay::{RelayState, build_router};
use serde_json::json;

/// Start the router on an ephemeral port and return its base URL.
async fn serve() -> (String, RelayState) {
    let state = RelayState::new();
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
    let begin: serde_json::Value = client
        .post(format!("{base}/v1/pair/begin"))
        .json(&json!({"runtime_id": runtime_id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret = begin["pairing_secret"].as_str().unwrap().to_string();

    let complete = client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": device_id,
            "device_pubkey": "cHVia2V5cHVia2V5cHVia2V5cHVia2V5cHVi",
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

    let confirm = client
        .post(format!("{base}/v1/pair/confirm"))
        .json(&json!({
            "runtime_id": runtime_id,
            "pairing_id": pairing_id,
            "decision": "accept"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(confirm.status(), 204);

    let auth: serde_json::Value = client
        .post(format!("{base}/v1/auth/session"))
        .json(&json!({"device_id": device_id, "runtime_id": runtime_id}))
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

    let begin: serde_json::Value = client
        .post(format!("{base}/v1/pair/begin"))
        .json(&json!({"runtime_id": "rt_a"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret = begin["pairing_secret"].as_str().unwrap();

    client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_a", "device_pubkey": "k", "device_name": "iPhone",
            "platform": "ios", "pairing_secret": secret, "scope": "interactive"
        }))
        .send()
        .await
        .unwrap();

    // Claimed but not confirmed: no device record, so no token can be issued.
    assert!(state.device("dev_a").is_none());
    let auth = client
        .post(format!("{base}/v1/auth/session"))
        .json(&json!({"device_id": "dev_a", "runtime_id": "rt_a"}))
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
    client
        .post(format!("{base}/v1/pair/begin"))
        .json(&json!({"runtime_id": "rt_a"}))
        .send()
        .await
        .unwrap();

    let wrong = client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_a", "device_pubkey": "k", "device_name": "iPhone",
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
            "device_id": "dev_b", "device_pubkey": "k", "device_name": "iPad",
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
    let (base, state) = serve().await;
    let client = reqwest::Client::new();

    let token_a = pair_device(&client, &base, "rt_a", "dev_a").await;
    let token_b = pair_device(&client, &base, "rt_b", "dev_b").await;
    state.register_runtime("rt_a", "machine A");
    state.register_runtime("rt_b", "machine B");

    // Each reaches its own host.
    for (token, host) in [(&token_a, "rt_a"), (&token_b, "rt_b")] {
        let response = client
            .post(format!("{base}/v1/hosts/{host}/leveler-sessions"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            501,
            "authorized and online: forwarding is simply not built yet"
        );
    }

    // Neither reaches the other's.
    for (token, host) in [(&token_a, "rt_b"), (&token_b, "rt_a")] {
        let response = client
            .post(format!("{base}/v1/hosts/{host}/leveler-sessions"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
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
    state.register_runtime("rt_a", "machine A");
    state.register_runtime("rt_b", "machine B");

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
    let response = client
        .post(format!("{base}/v1/hosts/rt_a/leveler-sessions"))
        .bearer_auth(&token)
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
    state.register_runtime("rt_a", "machine A");
    assert!(state.is_online("rt_a"));
    state.unregister_runtime("rt_a");
    let again = client
        .post(format!("{base}/v1/hosts/rt_a/leveler-sessions"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 503);
}

/// Revocation takes effect on the next request. Dropping the tokens with the
/// device is what makes that true without an access-token denylist.
#[tokio::test]
async fn revoking_a_device_invalidates_its_tokens_at_once() {
    let (base, state) = serve().await;
    let client = reqwest::Client::new();
    let token = pair_device(&client, &base, "rt_a", "dev_a").await;
    state.register_runtime("rt_a", "machine A");

    assert_eq!(
        client
            .post(format!("{base}/v1/hosts/rt_a/leveler-sessions"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        501,
        "works before revocation"
    );

    let revoke = client
        .delete(format!("{base}/v1/devices/dev_a?runtime_id=rt_a"))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status(), 204);

    assert_eq!(
        client
            .post(format!("{base}/v1/hosts/rt_a/leveler-sessions"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        401,
        "the same token must stop working immediately"
    );

    // And it cannot simply ask for a new one.
    let reauth = client
        .post(format!("{base}/v1/auth/session"))
        .json(&json!({"device_id": "dev_a", "runtime_id": "rt_a"}))
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

    let response = client
        .delete(format!("{base}/v1/devices/dev_a?runtime_id=rt_b"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

/// Refresh rotates, and replaying a spent token is treated as theft rather than
/// as a retry — the device loses everything and must authenticate again.
#[tokio::test]
async fn a_replayed_refresh_token_costs_the_device_its_session() {
    let (base, state) = serve().await;
    let client = reqwest::Client::new();

    let begin: serde_json::Value = client
        .post(format!("{base}/v1/pair/begin"))
        .json(&json!({"runtime_id": "rt_a"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret = begin["pairing_secret"].as_str().unwrap().to_string();
    let complete: serde_json::Value = client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_a", "device_pubkey": "k", "device_name": "iPhone",
            "platform": "ios", "pairing_secret": secret, "scope": "interactive"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/pair/confirm"))
        .json(&json!({
            "runtime_id": "rt_a",
            "pairing_id": complete["pairing_id"].as_str().unwrap(),
            "decision": "accept"
        }))
        .send()
        .await
        .unwrap();
    let auth: serde_json::Value = client
        .post(format!("{base}/v1/auth/session"))
        .json(&json!({"device_id": "dev_a", "runtime_id": "rt_a"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first_refresh = auth["refresh_token"].as_str().unwrap().to_string();
    state.register_runtime("rt_a", "machine A");

    let rotated: serde_json::Value = client
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": first_refresh}))
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
        .json(&json!({"refresh_token": first_refresh}))
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
        client
            .post(format!("{base}/v1/hosts/rt_a/leveler-sessions"))
            .bearer_auth(&fresh_access)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

/// A second `begin` cancels the first, so a stale QR on a screen cannot be
/// claimed later.
#[tokio::test]
async fn beginning_a_second_pairing_retires_the_first_secret() {
    let (base, _state) = serve().await;
    let client = reqwest::Client::new();

    let first: serde_json::Value = client
        .post(format!("{base}/v1/pair/begin"))
        .json(&json!({"runtime_id": "rt_a"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let stale = first["pairing_secret"].as_str().unwrap().to_string();

    client
        .post(format!("{base}/v1/pair/begin"))
        .json(&json!({"runtime_id": "rt_a"}))
        .send()
        .await
        .unwrap();

    let response = client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_a", "device_pubkey": "k", "device_name": "iPhone",
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

    let begin: serde_json::Value = client
        .post(format!("{base}/v1/pair/begin"))
        .json(&json!({"runtime_id": "rt_a"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/pair/complete"))
        .json(&json!({
            "device_id": "dev_a", "device_pubkey": "the-real-key", "device_name": "iPhone",
            "platform": "ios",
            "pairing_secret": begin["pairing_secret"].as_str().unwrap(),
            "scope": "interactive"
        }))
        .send()
        .await
        .unwrap();

    let pending: serde_json::Value = client
        .get(format!("{base}/v1/pair/pending?runtime_id=rt_a"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pending["device_pubkey"], "the-real-key");
    assert_eq!(pending["device_id"], "dev_a");
}
