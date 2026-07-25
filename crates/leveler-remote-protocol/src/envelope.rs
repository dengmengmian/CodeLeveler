//! The end-to-end signed envelope: what a relay is structurally unable to forge.
//!
//! Every payload that a paired device and a runtime exchange through a relay
//! travels inside a [`SignedEnvelope`]. The relay routes it and cannot alter it:
//! it holds neither key, and the signature covers the routing-relevant header
//! fields, not just the body. That is the whole point — the design's threat
//! model assumes a compromised relay, so "the relay said so" is never evidence.
//!
//! ## What the signature covers, and why each field is in it
//!
//! The canonical string is the exact byte sequence that gets signed:
//!
//! ```text
//! {v}|{sender}|{sender_id}|{recipient_id}|{stream_id}|{seq}|{ts}|{content_type}|{digest_hex}
//! ```
//!
//! - `recipient_id` binds the envelope to **one** peer. Without it a device
//!   paired to two hosts could have a frame it signed for host A replayed by a
//!   malicious relay into host B, which would verify it happily. Verification
//!   therefore rejects any envelope not addressed to the verifier.
//! - `stream_id` and `seq` bind it to a position in a stream, so a frame cannot
//!   be lifted into a different conversation or replayed within one.
//! - `digest_hex` is `SHA-256(payload_raw)` — the raw bytes, *not* the base64
//!   text. Hashing the encoding instead would let two different encodings of
//!   one payload share a signature.
//!
//! Ids are restricted to [`ID_CHARSET_DESCRIPTION`] before they ever reach the
//! canonical string. `|` is the field separator, so an id allowed to contain it
//! could shift the field boundaries and make two different envelopes produce
//! one identical signing input.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::keys::{SigningKey, VerifyingKey};

/// What a verifier knows about itself and its peer.
///
/// `expected_recipient_id` is the verifier's own id — not a value taken from
/// the envelope. Reading it from the frame would defeat the audience binding
/// entirely.
#[derive(Debug, Clone, Copy)]
pub struct VerifyParams<'a> {
    pub expected_recipient_id: &'a str,
    /// The peer's key from a trusted local source: the agent's TOFU
    /// `devices.json`, or the `runtime_pubkey` the APP anchored from the QR.
    /// Never a key the relay supplied alongside the frame.
    pub public_key: &'a VerifyingKey,
    /// The verifier's clock, in the envelope timestamp format.
    pub now: &'a str,
}

/// Human-readable form of the id rule, for error messages and docs.
pub const ID_CHARSET_DESCRIPTION: &str = "1-64 chars from [A-Za-z0-9_.:-]";

/// Maximum accepted clock skew, in seconds, between an envelope's `ts` and the
/// verifier's clock. Both directions.
pub const TIMESTAMP_WINDOW_SECS: i64 = 120;

/// Which side signed the envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sender {
    Device,
    Runtime,
}

impl Sender {
    /// The literal used in the canonical string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Sender::Device => "device",
            Sender::Runtime => "runtime",
        }
    }
}

/// What the payload is. Signed, so a relay cannot re-label a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    SessionUpstream,
    SessionDownstream,
    RpcRequest,
    RpcResponse,
}

impl ContentType {
    pub const fn as_str(self) -> &'static str {
        match self {
            ContentType::SessionUpstream => "session_upstream",
            ContentType::SessionDownstream => "session_downstream",
            ContentType::RpcRequest => "rpc_request",
            ContentType::RpcResponse => "rpc_response",
        }
    }
}

/// Why an envelope was refused. These map to the design's error-code catalogue;
/// [`EnvelopeError::code`] is the wire spelling.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    #[error("invalid {field}: expected {ID_CHARSET_DESCRIPTION}")]
    InvalidId { field: &'static str },
    #[error("invalid timestamp: expected YYYY-MM-DDTHH:MM:SSZ")]
    InvalidTimestamp,
    #[error("invalid base64 in {field}")]
    InvalidBase64 { field: &'static str },
    #[error("unsupported envelope version: {got}")]
    UnsupportedVersion { got: u8 },
    #[error("envelope addressed to {addressed_to}, not {expected}")]
    RecipientMismatch {
        expected: String,
        addressed_to: String,
    },
    #[error("timestamp outside the ±{TIMESTAMP_WINDOW_SECS}s window")]
    TimestampOutOfWindow,
    #[error("signature does not verify")]
    SignatureInvalid,
    #[error("invalid Ed25519 key material")]
    InvalidKey,
}

impl EnvelopeError {
    /// The wire error code from the design's catalogue.
    pub fn code(&self) -> &'static str {
        match self {
            EnvelopeError::InvalidId { .. }
            | EnvelopeError::InvalidTimestamp
            | EnvelopeError::InvalidBase64 { .. }
            | EnvelopeError::UnsupportedVersion { .. }
            | EnvelopeError::InvalidKey => "invalid_frame",
            EnvelopeError::RecipientMismatch { .. } => "recipient_mismatch",
            EnvelopeError::TimestampOutOfWindow => "replay",
            EnvelopeError::SignatureInvalid => "signature_invalid",
        }
    }
}

/// The only envelope version this build speaks.
pub const ENVELOPE_VERSION: u8 = 1;

/// A signed, relay-opaque frame. Field order here is also the JSON order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedEnvelope {
    pub v: u8,
    pub sender: Sender,
    pub sender_id: String,
    /// The peer this envelope is for. Verification fails unless it names the
    /// verifier — this is what stops cross-runtime replay by a relay.
    pub recipient_id: String,
    /// The session stream, or `rpc:{uuid}` for a request/response pair.
    pub stream_id: String,
    pub seq: u64,
    /// UTC RFC3339, seconds precision, `Z`-suffixed, no fractional part.
    pub ts: String,
    pub content_type: ContentType,
    /// Standard base64 (with padding) of the raw payload bytes.
    pub payload_b64: String,
    /// Standard base64 of the 64-byte Ed25519 signature.
    pub sig_b64: String,
}

/// Whether `id` may appear in a canonical string.
///
/// Rejecting `|` is the load-bearing part: it is the field separator, so an id
/// containing one could make two distinct envelopes canonicalize identically.
/// The rest of the restriction keeps ids to a shape every client can round-trip
/// without escaping questions.
pub fn id_is_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

impl SignedEnvelope {
    /// Rebuild the exact byte sequence that the signature covers.
    ///
    /// Fails if any id would corrupt the field structure, or if `ts` is not in
    /// the one accepted timestamp format — both are checked here so that no
    /// caller can sign or verify a string it did not fully constrain.
    pub fn canonical_string(&self) -> Result<String, EnvelopeError> {
        if self.v != ENVELOPE_VERSION {
            return Err(EnvelopeError::UnsupportedVersion { got: self.v });
        }
        for (field, value) in [
            ("sender_id", &self.sender_id),
            ("recipient_id", &self.recipient_id),
            ("stream_id", &self.stream_id),
        ] {
            if !id_is_valid(value) {
                return Err(EnvelopeError::InvalidId { field });
            }
        }
        parse_timestamp(&self.ts)?;

        let payload = self.payload()?;
        let digest = ring::digest::digest(&ring::digest::SHA256, &payload);
        let digest_hex = hex_lower(digest.as_ref());

        Ok(format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}",
            self.v,
            self.sender.as_str(),
            self.sender_id,
            self.recipient_id,
            self.stream_id,
            self.seq,
            self.ts,
            self.content_type.as_str(),
            digest_hex,
        ))
    }

    /// Build and sign an envelope over `payload_raw`.
    ///
    /// Refuses to produce an envelope whose ids or timestamp would not
    /// canonicalize, so a malformed frame never reaches the wire.
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        key: &SigningKey,
        sender: Sender,
        sender_id: &str,
        recipient_id: &str,
        stream_id: &str,
        seq: u64,
        ts: &str,
        content_type: ContentType,
        payload_raw: &[u8],
    ) -> Result<Self, EnvelopeError> {
        let mut envelope = Self {
            v: ENVELOPE_VERSION,
            sender,
            sender_id: sender_id.to_string(),
            recipient_id: recipient_id.to_string(),
            stream_id: stream_id.to_string(),
            seq,
            ts: ts.to_string(),
            content_type,
            payload_b64: base64::engine::general_purpose::STANDARD.encode(payload_raw),
            sig_b64: String::new(),
        };

        // Canonicalizing first is what enforces the id and timestamp rules on
        // the signing path: an envelope that cannot canonicalize is never signed.
        let canonical = envelope.canonical_string()?;
        let signature = key.sign_bytes(canonical.as_bytes());
        envelope.sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature);
        Ok(envelope)
    }

    /// Check the envelope against `params` and return the payload it carries.
    ///
    /// Order matters: structure, then audience, then freshness, then the
    /// signature. A caller must treat the payload as untrusted until this
    /// returns `Ok`.
    pub fn verify(&self, params: &VerifyParams<'_>) -> Result<Vec<u8>, EnvelopeError> {
        let canonical = self.canonical_string()?;

        if self.recipient_id != params.expected_recipient_id {
            return Err(EnvelopeError::RecipientMismatch {
                expected: params.expected_recipient_id.to_string(),
                addressed_to: self.recipient_id.clone(),
            });
        }

        let sent_at = parse_timestamp(&self.ts)?;
        let now = parse_timestamp(params.now)?;
        if (now - sent_at).num_seconds().abs() > TIMESTAMP_WINDOW_SECS {
            return Err(EnvelopeError::TimestampOutOfWindow);
        }

        let signature = base64::engine::general_purpose::STANDARD
            .decode(&self.sig_b64)
            .map_err(|_| EnvelopeError::InvalidBase64 { field: "sig_b64" })?;
        if !params
            .public_key
            .verify_bytes(canonical.as_bytes(), &signature)
        {
            return Err(EnvelopeError::SignatureInvalid);
        }

        self.payload()
    }

    /// The raw payload bytes this envelope carries.
    pub fn payload(&self) -> Result<Vec<u8>, EnvelopeError> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.payload_b64)
            .map_err(|_| EnvelopeError::InvalidBase64 {
                field: "payload_b64",
            })
    }
}

/// Accepts exactly `YYYY-MM-DDTHH:MM:SSZ`, returning the instant it names.
///
/// Deliberately strict. A looser parser would accept offsets or fractional
/// seconds, and two peers disagreeing on how to render one instant would
/// disagree on the canonical string — an interoperability failure that presents
/// as an invalid signature.
fn parse_timestamp(ts: &str) -> Result<chrono::DateTime<chrono::Utc>, EnvelopeError> {
    use chrono::TimeZone as _;

    let naive = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ")
        .map_err(|_| EnvelopeError::InvalidTimestamp)?;
    chrono::Utc
        .from_local_datetime(&naive)
        .single()
        .ok_or(EnvelopeError::InvalidTimestamp)
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
