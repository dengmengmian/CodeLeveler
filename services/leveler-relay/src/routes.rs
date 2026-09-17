//! HTTP surface.
//!
//! Two audiences with different authentication, kept apart on purpose. A
//! *runtime* enrolls its public key once against the operator's secret and
//! signs every later request about its own pairings and devices. A *device*
//! presents a routing token and can reach only the one host it is paired to.
//!
//! Exactly one endpoint is reachable without a credential: `/pair/complete`,
//! because a fresh phone has none — that is what the pairing secret is for. It
//! compares secrets in constant time and answers failures identically whether
//! the secret was wrong or absent. Everything else on the runtime side is
//! signed, since a `runtime_id` is a name and naming one must not be enough to
//! mint a pairing secret for that machine, accept a pairing on its behalf, or
//! revoke its devices.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use leveler_remote_protocol::auth::{
    ACCESS_TOKEN_TTL_SECS, DeviceAssertion, EnrollRequest, ProtocolVersionDto, RUNTIME_AUTH_HEADER,
    RefreshRequest, RuntimeAssertion, SessionAuthRequest, SessionAuthResponse, runtime_action,
};
use leveler_remote_protocol::pairing::{
    PairCompleteRequest, PairDecision, PairingPending, PairingScope,
};
use leveler_remote_protocol::{SignedEnvelope, VerifyingKey};

use crate::state::{ClaimedBy, RelayError, RelayState, RpcOutcome};

/// The relay's HTTP router.
pub fn build_router(state: RelayState) -> Router {
    Router::new()
        .route("/v1/runtimes/enroll", post(enroll_runtime))
        .route("/v1/pair/begin", post(pair_begin))
        .route("/v1/pair/complete", post(pair_complete))
        .route("/v1/pair/pending", get(pair_pending))
        .route("/v1/pair/confirm", post(pair_confirm))
        .route("/v1/devices", get(list_devices))
        .route("/v1/devices/{device_id}", delete(revoke_device))
        .route("/v1/auth/session", post(auth_session))
        .route("/v1/auth/refresh", post(auth_refresh))
        .route("/v1/hosts", get(list_hosts))
        .route("/v1/agent/tunnel", get(crate::tunnel::agent_tunnel))
        .route(
            "/v1/hosts/{host_id}/session",
            get(crate::tunnel::app_session),
        )
        .route("/v1/hosts/{host_id}/rpc", post(host_rpc))
        .with_state(state)
}

/// A refusal, rendered with the design's error code so a client can branch on
/// it rather than on prose.
pub(crate) struct Failure(pub(crate) RelayError);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let mut response = (
            self.0.http_status(),
            Json(serde_json::json!({
                "code": self.0.code(),
                "message": self.0.to_string(),
            })),
        )
            .into_response();
        // A device that retries immediately against an offline host achieves
        // nothing; tell it how long to wait instead of leaving it to guess.
        if self.0 == RelayError::RuntimeOffline {
            response
                .headers_mut()
                .insert("Retry-After", "5".parse().expect("static value"));
        }
        response
    }
}

impl From<RelayError> for Failure {
    fn from(error: RelayError) -> Self {
        Self(error)
    }
}

pub(crate) fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Check a base64 Ed25519 signature over `message`.
pub(crate) fn verify_signature(
    key: &VerifyingKey,
    message: &[u8],
    sig_b64: &str,
) -> Result<(), RelayError> {
    use base64::Engine as _;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(sig_b64)
        .map_err(|_| RelayError::Unauthorized)?;
    if leveler_remote_protocol::verify_detached(key, message, &signature) {
        Ok(())
    } else {
        Err(RelayError::Unauthorized)
    }
}

/// Reject a timestamp outside the envelope's window, so a signature cannot be
/// presented indefinitely.
pub(crate) fn within_window(timestamp: &str) -> Result<(), RelayError> {
    let parsed = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%SZ")
        .map_err(|_| RelayError::Unauthorized)?;
    let skew = (chrono::Utc::now().timestamp() - parsed.and_utc().timestamp()).abs();
    if skew > leveler_remote_protocol::TIMESTAMP_WINDOW_SECS {
        return Err(RelayError::Unauthorized);
    }
    Ok(())
}

/// Refuse an identifier that could not appear in a signed envelope.
///
/// One rule for every id the relay stores or signs over: `^[A-Za-z0-9_.:-]{1,64}$`.
/// The envelope layer already enforces it at signing time; enforcing it at
/// entry means an id that could never be used is never recorded either.
fn require_valid_id(id: &str) -> Result<(), RelayError> {
    if leveler_remote_protocol::id_is_valid(id) {
        Ok(())
    } else {
        Err(RelayError::InvalidPairing)
    }
}

// ------------------------------------------------------- runtime identity

/// Check the [`RUNTIME_AUTH_HEADER`] assertion on a runtime's own request.
///
/// Every control-plane operation below decides who may reach a developer's
/// machine, so each one is proved by the machine's key rather than by naming its
/// id. The `action` is inside the signed bytes, so an assertion lifted from one
/// request cannot authorize a different operation; the nonce is spent, so it
/// cannot authorize the same one twice.
fn runtime_auth(
    state: &RelayState,
    headers: &HeaderMap,
    action: &str,
    runtime_id: &str,
) -> Result<(), RelayError> {
    let outcome = runtime_auth_inner(state, headers, action, runtime_id);
    if outcome.is_err() {
        tracing::info!(
            runtime = %leveler_remote_protocol::hashed_label(runtime_id),
            action,
            "auth_fail: runtime assertion refused"
        );
    }
    outcome
}

fn runtime_auth_inner(
    state: &RelayState,
    headers: &HeaderMap,
    action: &str,
    runtime_id: &str,
) -> Result<(), RelayError> {
    let header = headers
        .get(RUNTIME_AUTH_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or(RelayError::Unauthorized)?;
    let (asserted_id, timestamp, nonce, sig) =
        RuntimeAssertion::parse_header(header).ok_or(RelayError::Unauthorized)?;
    // The assertion must be about the runtime the request names, or a valid one
    // for a machine the caller does control would authorize acting on another.
    if asserted_id != runtime_id {
        return Err(RelayError::Unauthorized);
    }

    let pubkey = state
        .runtime_key(runtime_id)
        .ok_or(RelayError::Unauthorized)?;
    let key = VerifyingKey::from_base64url(&pubkey).map_err(|_| RelayError::Unauthorized)?;
    let input = RuntimeAssertion::signing_input(action, runtime_id, timestamp, nonce);
    verify_signature(&key, input.as_bytes(), sig)?;
    within_window(timestamp)?;
    state.spend_nonce(runtime_id, nonce, now())
}

/// Register a machine's public key.
///
/// Two credentials, because they answer two different questions: the operator's
/// enrollment secret says this relay is willing to gain a host at all, and the
/// signature says the caller holds the key it is registering. Neither alone is
/// enough — a leaked secret would otherwise let anyone enroll a key they control
/// under someone else's machine name.
async fn enroll_runtime(
    State(state): State<RelayState>,
    headers: HeaderMap,
    Json(request): Json<EnrollRequest>,
) -> Result<StatusCode, Failure> {
    state.rate_limit("enroll", 10, 60, now())?;
    require_valid_id(&request.runtime_id)?;
    let secret = bearer(&headers)?;

    // Verify against the key being *presented*, not one already on file: this is
    // proof of possession, and for a first enrollment there is nothing on file.
    let header = headers
        .get(RUNTIME_AUTH_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or(RelayError::Unauthorized)?;
    let (asserted_id, timestamp, nonce, sig) =
        RuntimeAssertion::parse_header(header).ok_or(RelayError::Unauthorized)?;
    if asserted_id != request.runtime_id {
        return Err(Failure(RelayError::Unauthorized));
    }
    let key = VerifyingKey::from_base64url(&request.runtime_pubkey)
        .map_err(|_| RelayError::Unauthorized)?;
    let input = RuntimeAssertion::signing_input(
        runtime_action::ENROLL,
        &request.runtime_id,
        timestamp,
        nonce,
    );
    verify_signature(&key, input.as_bytes(), sig)?;
    within_window(timestamp)?;
    state.spend_nonce(&request.runtime_id, nonce, now())?;

    state.enroll_runtime(&request.runtime_id, &request.runtime_pubkey, &secret)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- pairing

#[derive(Debug, Deserialize)]
struct PairBeginRequest {
    runtime_id: String,
}

#[derive(Debug, Serialize)]
struct PairBeginResponse {
    pairing_id: String,
    /// Returned exactly once; the relay keeps only a hash.
    pairing_secret: String,
    ttl_secs: i64,
}

/// Mint the one secret that can pair a phone to this machine.
///
/// Signed by the runtime: unauthenticated, anyone who learned a `runtime_id`
/// could take that secret and walk the rest of the pairing themselves.
async fn pair_begin(
    State(state): State<RelayState>,
    headers: HeaderMap,
    Json(request): Json<PairBeginRequest>,
) -> Result<Json<PairBeginResponse>, Failure> {
    runtime_auth(
        &state,
        &headers,
        runtime_action::PAIR_BEGIN,
        &request.runtime_id,
    )?;
    let (pairing_id, pairing_secret) = state.begin_pairing(&request.runtime_id, now())?;
    Ok(Json(PairBeginResponse {
        pairing_id,
        pairing_secret,
        ttl_secs: crate::state::PAIRING_TTL_SECS,
    }))
}

#[derive(Debug, Serialize)]
struct PairCompleteResponse {
    pairing_id: String,
    /// Always `pending_confirm`: the device cannot advance itself to active.
    state: &'static str,
}

async fn pair_complete(
    State(state): State<RelayState>,
    Json(request): Json<PairCompleteRequest>,
) -> Result<Json<PairCompleteResponse>, Failure> {
    // Guessing a 128-bit secret is hopeless, but metering it keeps a flood from
    // becoming a denial of service against the real pairing.
    state.rate_limit("pair_complete", 10, 60, now())?;
    // This endpoint takes an id from an unauthenticated caller, and that id
    // later joins `|`-separated signing inputs. A separator inside it would
    // shift the field boundaries of every assertion about that device, so it is
    // refused at the door rather than left to each signing site to remember.
    require_valid_id(&request.device_id)?;
    let pairing = state.claim_pairing(
        &request.pairing_secret,
        ClaimedBy {
            device_id: request.device_id,
            device_pubkey: request.device_pubkey,
            device_name: request.device_name,
            platform: request.platform,
            scope: request.scope,
            claimed_at: 0,
        },
        now(),
    )?;

    Ok(Json(PairCompleteResponse {
        pairing_id: pairing.pairing_id,
        state: "pending_confirm",
    }))
}

#[derive(Debug, Deserialize)]
struct RuntimeQuery {
    runtime_id: String,
}

/// What the host's CLI polls to learn a device is waiting.
///
/// Carries `device_pubkey`, because the fingerprint the user is about to compare
/// is derived from it — the name alone would let a relay swap the key while the
/// user recognised the label.
async fn pair_pending(
    State(state): State<RelayState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<RuntimeQuery>,
) -> Result<Json<Option<PairingPending>>, Failure> {
    runtime_auth(
        &state,
        &headers,
        runtime_action::PAIR_PENDING,
        &query.runtime_id,
    )?;
    let Some(pairing) = state.pending_confirm_for(&query.runtime_id) else {
        return Ok(Json(None));
    };
    let claimed = pairing.claimed.expect("pending_confirm implies claimed");
    Ok(Json(Some(PairingPending {
        pairing_id: pairing.pairing_id,
        device_id: claimed.device_id,
        device_pubkey: claimed.device_pubkey,
        device_name: claimed.device_name,
        platform: claimed.platform,
        scope: claimed.scope,
    })))
}

#[derive(Debug, Deserialize)]
struct PairConfirmRequest {
    runtime_id: String,
    pairing_id: String,
    decision: PairDecision,
}

/// The accept the whole trust model rests on: the person at the keyboard says
/// yes to a fingerprint. Signed by the runtime, so a caller who claimed a
/// pairing cannot also accept it and skip the prompt entirely.
async fn pair_confirm(
    State(state): State<RelayState>,
    headers: HeaderMap,
    Json(request): Json<PairConfirmRequest>,
) -> Result<StatusCode, Failure> {
    runtime_auth(
        &state,
        &headers,
        runtime_action::PAIR_CONFIRM,
        &request.runtime_id,
    )?;
    state.confirm_pairing(
        &request.pairing_id,
        &request.runtime_id,
        request.decision == PairDecision::Accept,
        now(),
    )?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- devices

#[derive(Debug, Serialize)]
struct DeviceView {
    device_id: String,
    device_pubkey: String,
    scope: PairingScope,
    revoked: bool,
}

async fn list_devices(
    State(state): State<RelayState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<RuntimeQuery>,
) -> Result<Json<Vec<DeviceView>>, Failure> {
    runtime_auth(
        &state,
        &headers,
        runtime_action::DEVICES_LIST,
        &query.runtime_id,
    )?;
    Ok(Json(
        state
            .devices_for(&query.runtime_id)
            .into_iter()
            .map(|device| DeviceView {
                device_id: device.device_id,
                device_pubkey: device.device_pubkey,
                scope: device.scope,
                revoked: device.revoked,
            })
            .collect(),
    ))
}

async fn revoke_device(
    State(state): State<RelayState>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<RuntimeQuery>,
) -> Result<StatusCode, Failure> {
    runtime_auth(
        &state,
        &headers,
        runtime_action::DEVICE_REVOKE,
        &query.runtime_id,
    )?;
    state.revoke_device(&device_id, &query.runtime_id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- auth

/// Issue routing tokens.
///
/// The device proves it holds the paired private key. Checking only the pairing
/// record would let anyone who learned a `device_id` — which is not a secret —
/// mint tokens for that phone.
async fn auth_session(
    State(state): State<RelayState>,
    Json(request): Json<SessionAuthRequest>,
) -> Result<Json<SessionAuthResponse>, Failure> {
    state.rate_limit("auth_session", 20, 60, now())?;
    require_valid_id(&request.device_id)?;
    require_valid_id(&request.runtime_id)?;

    let device = state
        .device(&request.device_id)
        .ok_or(RelayError::Unauthorized)?;
    if device.revoked {
        return Err(Failure(RelayError::Revoked));
    }

    let key = VerifyingKey::from_base64url(&device.device_pubkey)
        .map_err(|_| RelayError::Unauthorized)?;
    let assertion = SessionAuthRequest::signing_input(
        &request.device_id,
        &request.runtime_id,
        &request.timestamp,
        &request.nonce,
    );
    if let Err(error) = verify_signature(&key, assertion.as_bytes(), &request.sig) {
        tracing::info!(
            device = %leveler_remote_protocol::hashed_label(&request.device_id),
            runtime = %leveler_remote_protocol::hashed_label(&request.runtime_id),
            "auth_fail: bad device assertion"
        );
        return Err(error.into());
    }
    within_window(&request.timestamp)?;
    // One assertion, one use: the signature stays valid for its whole window
    // otherwise, and a captured request could be replayed inside it.
    state.spend_nonce(&request.device_id, &request.nonce, now())?;

    let (access_token, refresh_token, scope) =
        state.issue_tokens(&request.device_id, &request.runtime_id, now())?;
    Ok(Json(SessionAuthResponse {
        access_token,
        expires_in_secs: ACCESS_TOKEN_TTL_SECS,
        refresh_token,
        runtime_id: request.runtime_id,
        protocol: ProtocolVersionDto { major: 1, minor: 3 },
        pairing_scope: scope,
    }))
}

#[derive(Debug, Serialize)]
struct RefreshView {
    access_token: String,
    expires_in_secs: i64,
    refresh_token: String,
}

/// Rotate a refresh token.
///
/// The device assertion is required, not optional: a refresh token on its own is
/// a bearer credential, so a captured one would be spendable by whoever holds
/// it. Proving possession of the paired key makes the theft useless without the
/// phone's secure element.
async fn auth_refresh(
    State(state): State<RelayState>,
    body: axum::body::Bytes,
) -> Result<Json<RefreshView>, Failure> {
    state.rate_limit("auth_refresh", 30, 60, now())?;
    // Parsed here rather than by an extractor so that a body with no assertion
    // is refused the same way as one with a bad assertion: 401 either way, with
    // no shape difference to probe.
    let request: RefreshRequest =
        serde_json::from_slice(&body).map_err(|_| RelayError::Unauthorized)?;
    let device = state
        .device(&request.device_assertion.device_id)
        .ok_or(RelayError::Unauthorized)?;
    if device.revoked {
        return Err(Failure(RelayError::Revoked));
    }
    let key = VerifyingKey::from_base64url(&device.device_pubkey)
        .map_err(|_| RelayError::Unauthorized)?;
    let input = DeviceAssertion::signing_input(
        &request.device_assertion.device_id,
        &request.device_assertion.timestamp,
    );
    verify_signature(&key, input.as_bytes(), &request.device_assertion.sig)?;
    within_window(&request.device_assertion.timestamp)?;

    let (access_token, refresh_token) =
        state.rotate_refresh(&request.refresh_token, &device.device_id, now())?;
    Ok(Json(RefreshView {
        access_token,
        expires_in_secs: ACCESS_TOKEN_TTL_SECS,
        refresh_token,
    }))
}

/// Pull the bearer token from the header.
///
/// Header only: a token in a query string lands in access logs and browser
/// history, which is why the design forbids it.
pub(crate) fn bearer(headers: &HeaderMap) -> Result<String, RelayError> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|token| token.to_string())
        .ok_or(RelayError::Unauthorized)
}

// ---------------------------------------------------------------- hosts

#[derive(Debug, Serialize)]
struct HostView {
    host_id: String,
    display_name: String,
}

async fn list_hosts(
    State(state): State<RelayState>,
    headers: HeaderMap,
) -> Result<Json<Vec<HostView>>, Failure> {
    let token = bearer(&headers)?;
    let claims = state.authorize(&token, None, now())?;
    Ok(Json(
        state
            .hosts_for_device(&claims.sub)
            .into_iter()
            .map(|host| HostView {
                host_id: host.runtime_id,
                display_name: host.display_name,
            })
            .collect(),
    ))
}

/// How long the relay waits for an agent to answer an RPC before giving up. Long
/// enough for a runtime doing real work, short enough that a wedged agent does
/// not hold a phone's request open indefinitely.
const RPC_TIMEOUT_SECS: u64 = 30;

/// Carry one device-signed RPC to a host and return the runtime's signed answer.
///
/// A single endpoint rather than one per method: the method, the project and the
/// body all live *inside* the device's signature, so a URL that restated them
/// would be a second, unsigned copy for the relay to disagree with. What the URL
/// does carry is the host, which is what the token's audience is checked
/// against.
///
/// Refuses with 503 when the host has no live tunnel. It does not queue: a
/// command accepted now and run later could outlive the revocation of the device
/// that sent it, so the device retries instead.
async fn host_rpc(
    State(state): State<RelayState>,
    Path(host_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let token = bearer(&headers)?;
    // The audience check is what stops a token minted for one host from
    // reaching another.
    let claims = state.authorize(&token, Some(&host_id), now())?;

    let envelope: SignedEnvelope =
        serde_json::from_slice(&body).map_err(|_| RelayError::InvalidPairing)?;
    // The relay cannot verify the signature — it holds no device key, which is
    // the point. It can check the envelope claims to be from the device this
    // token was minted for, so one device's traffic is never carried under
    // another's authorization.
    if envelope.sender_id != claims.sub || envelope.recipient_id != host_id {
        return Err(Failure(RelayError::Unauthorized));
    }

    let waiting = state.begin_rpc(&host_id, envelope)?;
    let outcome =
        tokio::time::timeout(std::time::Duration::from_secs(RPC_TIMEOUT_SECS), waiting).await;

    match outcome {
        // The agent's own envelope, carried out whole. Unwrapping it to re-wrap
        // the payload would strip the signature that makes it worth having.
        Ok(Ok(RpcOutcome::Signed(envelope))) => Ok(Json(*envelope).into_response()),
        // A routing failure has no runtime result behind it, so it carries no
        // body the phone could verify — and the relay must not invent one.
        Ok(Ok(RpcOutcome::Failed(error))) => Ok((
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"code": error.code, "message": error.message})),
        )
            .into_response()),
        // Sender dropped: the agent went away with the request in flight.
        Ok(Err(_)) => Err(Failure(RelayError::RuntimeOffline)),
        Err(_) => Err(Failure(RelayError::RuntimeOffline)),
    }
}
