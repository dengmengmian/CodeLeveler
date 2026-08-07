//! The canonical string is the signature's input, so its exact bytes are the
//! interoperability contract with the Swift/Kotlin clients. These pin it.

use leveler_remote_protocol::{ContentType, Sender, SignedEnvelope, id_is_valid};

/// Worked example for an empty payload (SHA-256 is the well-known
/// `e3b0c442…` digest).
#[test]
fn canonical_string_matches_the_design_example() {
    let envelope = SignedEnvelope {
        v: 1,
        sender: Sender::Device,
        sender_id: "dev_1".to_string(),
        recipient_id: "rt_7".to_string(),
        stream_id: "str_9".to_string(),
        seq: 42,
        ts: "2026-07-25T12:00:00Z".to_string(),
        content_type: ContentType::SessionUpstream,
        payload_b64: String::new(),
        sig_b64: String::new(),
    };

    assert_eq!(
        envelope.canonical_string().unwrap(),
        "1|device|dev_1|rt_7|str_9|42|2026-07-25T12:00:00Z|session_upstream|\
         e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

/// `seq` renders without leading zeros, and zero renders as a single `0`.
#[test]
fn seq_renders_as_bare_decimal() {
    let base = SignedEnvelope {
        v: 1,
        sender: Sender::Runtime,
        sender_id: "rt_7".to_string(),
        recipient_id: "dev_1".to_string(),
        stream_id: "str_9".to_string(),
        seq: 0,
        ts: "2026-07-25T12:00:00Z".to_string(),
        content_type: ContentType::SessionDownstream,
        payload_b64: String::new(),
        sig_b64: String::new(),
    };

    let canonical = base.canonical_string().unwrap();
    assert!(
        canonical.contains("|0|2026-07-25T12:00:00Z|"),
        "zero seq should render as a single 0: {canonical}"
    );
}

/// The digest covers the raw payload bytes, not their base64 text. Hashing the
/// encoding would let two encodings of one payload share a signature.
#[test]
fn digest_covers_raw_payload_not_its_base64() {
    let envelope = SignedEnvelope {
        v: 1,
        sender: Sender::Device,
        sender_id: "dev_1".to_string(),
        recipient_id: "rt_7".to_string(),
        stream_id: "str_9".to_string(),
        seq: 1,
        ts: "2026-07-25T12:00:00Z".to_string(),
        content_type: ContentType::SessionUpstream,
        // base64 of the single byte 0x61 ("a").
        payload_b64: "YQ==".to_string(),
        sig_b64: String::new(),
    };

    // SHA-256("a"), not SHA-256("YQ==").
    assert!(
        envelope
            .canonical_string()
            .unwrap()
            .ends_with("ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"),
        "digest must be taken over the decoded payload"
    );
}

/// An id carrying the field separator could shift the canonical string's field
/// boundaries, so it never reaches the signing input.
#[test]
fn ids_containing_the_separator_are_rejected() {
    assert!(!id_is_valid("dev|rt"), "pipe must be rejected");
    assert!(!id_is_valid(""), "empty id must be rejected");
    assert!(
        !id_is_valid(&"a".repeat(65)),
        "over-long id must be rejected"
    );
    assert!(!id_is_valid("dev 1"), "space must be rejected");
    assert!(!id_is_valid("dev/1"), "slash must be rejected");

    assert!(id_is_valid("dev_1"));
    assert!(id_is_valid("rpc:2f8a-11"));
    assert!(id_is_valid("host.local-7"));
}

/// Canonicalization refuses an envelope whose ids would corrupt the structure,
/// rather than producing a string that happens to parse ambiguously.
#[test]
fn canonical_string_rejects_a_separator_bearing_id() {
    let envelope = SignedEnvelope {
        v: 1,
        sender: Sender::Device,
        sender_id: "dev|evil".to_string(),
        recipient_id: "rt_7".to_string(),
        stream_id: "str_9".to_string(),
        seq: 1,
        ts: "2026-07-25T12:00:00Z".to_string(),
        content_type: ContentType::SessionUpstream,
        payload_b64: String::new(),
        sig_b64: String::new(),
    };

    let error = envelope.canonical_string().unwrap_err();
    assert_eq!(error.code(), "invalid_frame");
}

/// Only one timestamp format is accepted; anything looser widens the replay
/// window in ways the ±120s check cannot see.
#[test]
fn canonical_string_rejects_non_second_precision_timestamps() {
    for bad in [
        "2026-07-25T12:00:00.500Z",  // fractional seconds
        "2026-07-25T12:00:00+08:00", // offset rather than Z
        "2026-07-25 12:00:00Z",      // space separator
        "2026-07-25T12:00Z",         // minute precision
    ] {
        let envelope = SignedEnvelope {
            v: 1,
            sender: Sender::Device,
            sender_id: "dev_1".to_string(),
            recipient_id: "rt_7".to_string(),
            stream_id: "str_9".to_string(),
            seq: 1,
            ts: bad.to_string(),
            content_type: ContentType::SessionUpstream,
            payload_b64: String::new(),
            sig_b64: String::new(),
        };
        assert_eq!(
            envelope.canonical_string().unwrap_err().code(),
            "invalid_frame",
            "{bad} must be rejected"
        );
    }
}
