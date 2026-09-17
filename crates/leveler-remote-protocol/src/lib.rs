//! `leveler-remote-protocol` — the control-plane contract for remote APP control.
//!
//! Covers device pairing, routing tokens, the agent tunnel frames, and the
//! end-to-end [`SignedEnvelope`] that carries session traffic through a relay
//! the design does not trust.
//!
//! This crate defines *framing only*. It deliberately does not redefine the
//! business types the frames carry (`CreateSessionRequest`, `SessionBootstrap`,
//! `UiSessionSnapshot`, …) — a second definition of those would be a second
//! source of truth, and they would drift. RPC bodies are therefore carried as
//! opaque JSON here and typed by whichever side owns them.
//!
//! ## Trust boundaries, in one place
//!
//! - A **relay** routes and can read (until AEAD lands) but cannot author: it
//!   holds no device or runtime key, and every security-relevant payload is
//!   inside a [`SignedEnvelope`].
//! - A **routing token** ([`auth::TokenClaims`]) authorizes reaching a runtime.
//!   It is never command authority — that comes only from a device signature.
//! - An **agent** trusts device keys from its own store
//!   ([`pairing::DeviceStore`]), never a key delivered alongside a frame.
//! - An **APP** trusts the `runtime_pubkey` it anchored from the pairing QR,
//!   never one a relay asserts later.
#![forbid(unsafe_code)]

pub mod auth;
mod envelope;
mod keys;
pub mod pairing;
#[cfg(feature = "policy")]
pub mod policy;
pub mod tunnel;

pub use envelope::{
    ContentType, ENVELOPE_VERSION, EnvelopeError, ID_CHARSET_DESCRIPTION, Sender, SignedEnvelope,
    TIMESTAMP_WINDOW_SECS, VerifyParams, id_is_valid,
};
pub use keys::{SigningKey, VerifyingKey};

/// A truncated `SHA-256` of an identifier, as lowercase hex.
///
/// The observability convention for both sides: a log line should be
/// correlatable with other lines about the same device or machine without the
/// log itself becoming a list of a user's devices. Eight bytes, the same width
/// as the pairing fingerprint, so one shape covers every id a human might see.
pub fn hashed_label(value: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, value.as_bytes());
    digest.as_ref()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Verify a detached signature over `message`. See
/// [`VerifyingKey::verify_detached`].
pub fn verify_detached(key: &VerifyingKey, message: &[u8], signature: &[u8]) -> bool {
    key.verify_detached(message, signature)
}
