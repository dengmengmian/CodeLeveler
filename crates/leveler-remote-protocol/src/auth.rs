//! Routing tokens: permission to reach a runtime through the relay, and
//! nothing more.
//!
//! A relay mints these, so they authorize *routing* only. They are deliberately
//! not command authority: possessing one lets a caller open a stream, but every
//! frame inside that stream still has to carry a device signature the relay
//! cannot produce. Keeping the two separate is what stops a compromised relay
//! from promoting itself into a control channel by issuing itself a token.

use serde::{Deserialize, Serialize};

use crate::pairing::PairingScope;

/// Access-token lifetime. Short, because revocation of a stolen phone should
/// take effect quickly without a relay-side lookup on every frame.
pub const ACCESS_TOKEN_TTL_SECS: i64 = 15 * 60;

/// Refresh-token lifetime.
pub const REFRESH_TOKEN_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// Which kind of token a JWT is, so an access token cannot be presented where a
/// refresh token is expected or the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenUse {
    Access,
    Refresh,
}

/// Claims carried by a routing token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenClaims {
    pub iss: String,
    /// The device.
    pub sub: String,
    /// The runtime this token may reach — checked on every use, so a token for
    /// one host is useless against another.
    pub aud: String,
    pub jti: String,
    pub iat: i64,
    pub exp: i64,
    pub scope: String,
    pub pairing_scope: PairingScope,
    pub token_use: TokenUse,
}

/// The `scope` value for an interactive session token.
pub const SCOPE_REMOTE_SESSION: &str = "remote.session";

/// `POST /v1/auth/session` — the device proves it holds the paired private key.
///
/// The signature covers a caller-supplied `nonce` and `timestamp` so a captured
/// assertion cannot be replayed; the relay is expected to reject a repeated
/// `(device_id, nonce)` inside the timestamp window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAuthRequest {
    pub device_id: String,
    pub runtime_id: String,
    pub timestamp: String,
    pub nonce: String,
    pub sig: String,
}

impl SessionAuthRequest {
    /// The exact bytes a device signs: the same `|`-joined shape as the
    /// envelope's canonical string, and subject to the same id rules so a
    /// separator inside an id cannot shift the field boundaries.
    pub fn signing_input(
        device_id: &str,
        runtime_id: &str,
        timestamp: &str,
        nonce: &str,
    ) -> String {
        format!("{device_id}|{runtime_id}|{timestamp}|{nonce}")
    }
}

/// What an agent signs to prove it owns the runtime id it is claiming.
///
/// Without this, a relay's `runtime_id` is a bare name and anyone who can reach
/// the relay could register as someone else's machine and be handed their
/// devices' streams.
pub struct AgentRegisterAssertion;

impl AgentRegisterAssertion {
    /// `{runtime_id}|{timestamp}` — the same `|`-joined shape and id rules as
    /// the envelope's canonical string, so one set of constraints covers both.
    pub fn signing_input(runtime_id: &str, timestamp: &str) -> String {
        format!("{runtime_id}|{timestamp}")
    }
}

/// Header carrying a runtime's proof on the relay's control plane.
///
/// Value shape: `{runtime_id}|{timestamp}|{nonce}|{sig_b64}`. Ids exclude `|`
/// by the envelope's charset rule and standard base64 never contains it, so the
/// four fields cannot be made ambiguous by their contents.
pub const RUNTIME_AUTH_HEADER: &str = "x-leveler-runtime-auth";

/// What a runtime signs to act on its own control-plane resources.
///
/// Pairing, confirmation and revocation all decide who may reach a developer's
/// machine. Without a signature they would be authorized by nothing but a
/// `runtime_id`, which is a name rather than a secret: anyone who learned one
/// could mint a pairing secret for that host, claim it with their own device
/// key, accept it on the host's behalf and hold a routing token to it. End-to-end
/// signing still stops their frames at the agent, but the control plane would be
/// theirs.
///
/// The `action` is inside the signed bytes so an assertion captured from one
/// operation cannot be replayed against a different one.
pub struct RuntimeAssertion;

impl RuntimeAssertion {
    /// `{action}|{runtime_id}|{timestamp}|{nonce}` — the same `|`-joined shape
    /// and id rules as the envelope's canonical string.
    pub fn signing_input(action: &str, runtime_id: &str, timestamp: &str, nonce: &str) -> String {
        format!("{action}|{runtime_id}|{timestamp}|{nonce}")
    }

    /// Build the [`RUNTIME_AUTH_HEADER`] value for one request.
    pub fn header_value(
        key: &crate::SigningKey,
        action: &str,
        runtime_id: &str,
        timestamp: &str,
        nonce: &str,
    ) -> String {
        use base64::Engine as _;
        let input = Self::signing_input(action, runtime_id, timestamp, nonce);
        let sig =
            base64::engine::general_purpose::STANDARD.encode(key.sign_detached(input.as_bytes()));
        format!("{runtime_id}|{timestamp}|{nonce}|{sig}")
    }

    /// Split a header value into `(runtime_id, timestamp, nonce, sig_b64)`.
    pub fn parse_header(value: &str) -> Option<(&str, &str, &str, &str)> {
        let mut parts = value.splitn(4, '|');
        let runtime_id = parts.next()?;
        let timestamp = parts.next()?;
        let nonce = parts.next()?;
        let sig = parts.next()?;
        if runtime_id.is_empty() || timestamp.is_empty() || nonce.is_empty() || sig.is_empty() {
            return None;
        }
        Some((runtime_id, timestamp, nonce, sig))
    }
}

/// The control-plane operations a [`RuntimeAssertion`] can authorize. Named
/// constants rather than free strings so both sides sign the same word.
pub mod runtime_action {
    pub const ENROLL: &str = "enroll";
    pub const PAIR_BEGIN: &str = "pair_begin";
    pub const PAIR_PENDING: &str = "pair_pending";
    pub const PAIR_CONFIRM: &str = "pair_confirm";
    pub const DEVICES_LIST: &str = "devices_list";
    pub const DEVICE_REVOKE: &str = "device_revoke";
}

/// `POST /v1/runtimes/enroll` — a machine registers its public key with the
/// relay it will connect to.
///
/// Enrollment is the one moment the relay learns a `runtime_id → pubkey`
/// binding, so it is the one moment worth authenticating. It takes the relay's
/// configured enrollment secret (in the `Authorization: Bearer` header) *and* a
/// signature from the key being registered: the secret says an operator
/// permitted this relay to gain a host, the signature says the caller actually
/// holds the key it is claiming. Trust-on-first-use would grant both to whoever
/// asked first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub runtime_id: String,
    pub runtime_pubkey: String,
}

/// The protocol version a peer speaks, mirrored from the client protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersionDto {
    pub major: u16,
    pub minor: u16,
}

/// `POST /v1/auth/session` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAuthResponse {
    pub access_token: String,
    pub expires_in_secs: i64,
    pub refresh_token: String,
    pub runtime_id: String,
    pub protocol: ProtocolVersionDto,
    pub pairing_scope: PairingScope,
}

/// A device assertion attached to a refresh, proving the caller still holds the
/// private key rather than merely a stolen refresh token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAssertion {
    pub device_id: String,
    pub timestamp: String,
    pub sig: String,
}

impl DeviceAssertion {
    /// `{device_id}|{timestamp}` — what the device signs on a refresh.
    pub fn signing_input(device_id: &str, timestamp: &str) -> String {
        format!("{device_id}|{timestamp}")
    }
}

/// `POST /v1/auth/refresh`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
    pub device_assertion: DeviceAssertion,
}

/// `POST /v1/auth/refresh` response. The refresh token is rotated on every use,
/// so a replayed one signals theft rather than a retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshResponse {
    pub access_token: String,
    pub expires_in_secs: i64,
    pub refresh_token: String,
}
