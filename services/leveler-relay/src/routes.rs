//! HTTP surface.
//!
//! Two audiences with different authentication, kept apart on purpose. A
//! *runtime* proves itself by naming its id on the agent tunnel and manages its
//! own pairings and devices. A *device* presents a routing token and can reach
//! only the one host it is paired to.
//!
//! The pairing endpoints are the sensitive ones: `/pair/complete` is reachable
//! without any credential (that is the point — a fresh phone has none), so it
//! compares secrets in constant time and answers failures identically whether
//! the secret was wrong or absent.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use leveler_remote_protocol::auth::{
    ACCESS_TOKEN_TTL_SECS, ProtocolVersionDto, SessionAuthResponse,
};
use leveler_remote_protocol::pairing::{
    PairCompleteRequest, PairDecision, PairingPending, PairingScope,
};

use crate::state::{ClaimedBy, RelayError, RelayState};

/// The relay's HTTP router.
pub fn build_router(state: RelayState) -> Router {
    Router::new()
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
        .route("/v1/hosts/{host_id}/leveler-sessions", post(create_session))
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

fn now() -> i64 {
    chrono::Utc::now().timestamp()
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

async fn pair_begin(
    State(state): State<RelayState>,
    Json(request): Json<PairBeginRequest>,
) -> Result<Json<PairBeginResponse>, Failure> {
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
    axum::extract::Query(query): axum::extract::Query<RuntimeQuery>,
) -> Result<Json<Option<PairingPending>>, Failure> {
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

async fn pair_confirm(
    State(state): State<RelayState>,
    Json(request): Json<PairConfirmRequest>,
) -> Result<StatusCode, Failure> {
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
    axum::extract::Query(query): axum::extract::Query<RuntimeQuery>,
) -> Json<Vec<DeviceView>> {
    Json(
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
    )
}

async fn revoke_device(
    State(state): State<RelayState>,
    Path(device_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<RuntimeQuery>,
) -> Result<StatusCode, Failure> {
    state.revoke_device(&device_id, &query.runtime_id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- auth

#[derive(Debug, Deserialize)]
struct SessionAuthBody {
    device_id: String,
    runtime_id: String,
}

/// Issue routing tokens.
///
/// The design has the device sign an assertion here; this MVP checks the pairing
/// record only, which is noted as a gap rather than presented as complete.
async fn auth_session(
    State(state): State<RelayState>,
    Json(request): Json<SessionAuthBody>,
) -> Result<Json<SessionAuthResponse>, Failure> {
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

#[derive(Debug, Deserialize)]
struct RefreshBody {
    refresh_token: String,
}

#[derive(Debug, Serialize)]
struct RefreshView {
    access_token: String,
    expires_in_secs: i64,
    refresh_token: String,
}

async fn auth_refresh(
    State(state): State<RelayState>,
    Json(request): Json<RefreshBody>,
) -> Result<Json<RefreshView>, Failure> {
    let (access_token, refresh_token) = state.rotate_refresh(&request.refresh_token, now())?;
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

/// Create a session on a host.
///
/// Refuses with 503 when the host has no live tunnel. It does not queue: a
/// command accepted now and run later could outlive the revocation of the device
/// that sent it, so the device retries instead.
async fn create_session(
    State(state): State<RelayState>,
    Path(host_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, Failure> {
    let token = bearer(&headers)?;
    // The audience check is what stops a token minted for one host from
    // reaching another.
    state.authorize(&token, Some(&host_id), now())?;
    state.require_online(&host_id)?;
    // Reaching here means authorized and online. Forwarding needs the agent
    // tunnel, which does not exist yet — so say so rather than returning 503,
    // which would misreport a live host as unreachable.
    Ok(StatusCode::NOT_IMPLEMENTED)
}
