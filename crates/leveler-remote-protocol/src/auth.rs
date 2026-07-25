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
