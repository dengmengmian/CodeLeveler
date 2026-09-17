//! Signing and verification, including the attacks the envelope exists to stop.
//!
//! The positive path matters less than the negatives here: a signature layer
//! that accepts everything also passes its round-trip test.

use leveler_remote_protocol::{
    ContentType, EnvelopeError, Sender, SignedEnvelope, SigningKey, VerifyParams, VerifyingKey,
};

/// Fixed seeds so every run signs the same bytes. Never used outside tests.
const DEVICE_SEED: [u8; 32] = [7u8; 32];
const RUNTIME_SEED: [u8; 32] = [9u8; 32];

const AT: &str = "2026-07-25T12:00:00Z";

fn device_key() -> SigningKey {
    SigningKey::from_seed(&DEVICE_SEED).expect("seed is valid")
}

fn signed_upstream(recipient: &str) -> SignedEnvelope {
    SignedEnvelope::sign(
        &device_key(),
        Sender::Device,
        "dev_1",
        recipient,
        "str_9",
        1,
        AT,
        ContentType::SessionUpstream,
        b"{\"type\":\"snapshot\",\"session_id\":\"s1\"}",
    )
    .expect("signing succeeds")
}

fn params<'a>(expected_recipient: &'a str, key: &'a VerifyingKey) -> VerifyParams<'a> {
    VerifyParams {
        expected_recipient_id: expected_recipient,
        public_key: key,
        now: AT,
    }
}

#[test]
fn a_signed_envelope_verifies_and_returns_its_payload() {
    let envelope = signed_upstream("rt_7");
    let public = device_key().verifying_key();

    let payload = envelope
        .verify(&params("rt_7", &public))
        .expect("verification succeeds");
    assert_eq!(payload, b"{\"type\":\"snapshot\",\"session_id\":\"s1\"}");
}

/// The cross-runtime replay this field exists to stop: a device paired to two
/// hosts signs a frame for `rt_7`, and a malicious relay offers it to `rt_8`.
/// The signature is genuine — only the audience is wrong.
#[test]
fn an_envelope_addressed_elsewhere_is_refused() {
    let envelope = signed_upstream("rt_7");
    let public = device_key().verifying_key();

    let error = envelope
        .verify(&params("rt_8", &public))
        .expect_err("a frame for another runtime must not verify");

    assert_eq!(error.code(), "recipient_mismatch");
}

/// Rewriting the recipient to the target does not help: `recipient_id` is
/// inside the signed canonical string, so the tamper invalidates the signature.
#[test]
fn rewriting_the_recipient_breaks_the_signature() {
    let mut envelope = signed_upstream("rt_7");
    envelope.recipient_id = "rt_8".to_string();
    let public = device_key().verifying_key();

    let error = envelope
        .verify(&params("rt_8", &public))
        .expect_err("a rewritten recipient must not verify");

    assert_eq!(error.code(), "signature_invalid");
}

/// A relay holds no device key, so a payload it swaps in cannot be re-signed.
#[test]
fn tampering_with_the_payload_is_detected() {
    let mut envelope = signed_upstream("rt_7");
    envelope.payload_b64 = base64_of(b"{\"type\":\"deliver\",\"command\":\"rm -rf\"}");
    let public = device_key().verifying_key();

    assert_eq!(
        envelope
            .verify(&params("rt_7", &public))
            .expect_err("swapped payload must not verify")
            .code(),
        "signature_invalid"
    );
}

/// Verifying against the wrong identity's key fails — this is what makes the
/// agent's TOFU store, rather than the relay's word, the authority.
#[test]
fn a_signature_from_another_key_is_refused() {
    let envelope = signed_upstream("rt_7");
    let other = SigningKey::from_seed(&RUNTIME_SEED)
        .expect("seed is valid")
        .verifying_key();

    assert_eq!(
        envelope
            .verify(&params("rt_7", &other))
            .expect_err("another key must not verify")
            .code(),
        "signature_invalid"
    );
}

/// Header fields are covered too, so a relay cannot re-label or re-order a
/// frame it forwards.
#[test]
fn tampering_with_signed_header_fields_is_detected() {
    let public = device_key().verifying_key();

    let mut relabelled = signed_upstream("rt_7");
    relabelled.content_type = ContentType::RpcRequest;
    assert_eq!(
        relabelled
            .verify(&params("rt_7", &public))
            .unwrap_err()
            .code(),
        "signature_invalid",
        "content_type is signed"
    );

    let mut reordered = signed_upstream("rt_7");
    reordered.seq = 2;
    assert_eq!(
        reordered
            .verify(&params("rt_7", &public))
            .unwrap_err()
            .code(),
        "signature_invalid",
        "seq is signed"
    );

    let mut moved = signed_upstream("rt_7");
    moved.stream_id = "str_other".to_string();
    assert_eq!(
        moved.verify(&params("rt_7", &public)).unwrap_err().code(),
        "signature_invalid",
        "stream_id is signed"
    );
}

/// A frame captured today must not verify tomorrow.
#[test]
fn a_timestamp_outside_the_window_is_refused() {
    let envelope = signed_upstream("rt_7");
    let public = device_key().verifying_key();

    let stale = VerifyParams {
        expected_recipient_id: "rt_7",
        public_key: &public,
        now: "2026-07-25T12:05:00Z", // +300s, outside ±120s
    };
    assert_eq!(
        envelope.verify(&stale).unwrap_err().code(),
        "replay",
        "a frame older than the window must not verify"
    );

    let edge = VerifyParams {
        expected_recipient_id: "rt_7",
        public_key: &public,
        now: "2026-07-25T12:01:59Z", // +119s, inside
    };
    assert!(
        envelope.verify(&edge).is_ok(),
        "a frame inside the window must verify"
    );
}

/// Signing refuses ids that would corrupt the canonical string, so a malformed
/// envelope is never produced in the first place.
#[test]
fn signing_refuses_an_id_that_would_corrupt_the_canonical_string() {
    let error = SignedEnvelope::sign(
        &device_key(),
        Sender::Device,
        "dev|evil",
        "rt_7",
        "str_9",
        1,
        AT,
        ContentType::SessionUpstream,
        b"{}",
    )
    .expect_err("a separator-bearing id must not be signable");

    assert!(matches!(error, EnvelopeError::InvalidId { .. }));
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
