//! Drives `testdata/signed_envelope.golden.json`, the cross-language contract.
//!
//! A Swift or Kotlin implementation of the envelope has no Rust to compare
//! against, so this file is the shared answer key: fixed seeds, a fixed
//! canonical string, a fixed signature, and — the part that matters — cases
//! that must be *refused*. An implementation that only checks the positive
//! vector passes while accepting forgeries, so every negative here names the
//! attack it stands for.
//!
//! Regenerate with:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p leveler-remote-protocol --test golden_vectors
//! ```

use std::path::{Path, PathBuf};

use leveler_remote_protocol::{
    ContentType, Sender, SignedEnvelope, SigningKey, VerifyParams, VerifyingKey,
};
use serde_json::json;

const DEVICE_SEED: [u8; 32] = [7u8; 32];
const RUNTIME_SEED: [u8; 32] = [9u8; 32];
const DEVICE_ID: &str = "dev_golden";
const RUNTIME_ID: &str = "rt_golden";
const AT: &str = "2026-07-25T12:00:00Z";
const PAYLOAD: &[u8] = b"{\"type\":\"snapshot\",\"session_id\":\"s1\"}";

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("testdata")
        .join("signed_envelope.golden.json")
}

fn device_key() -> SigningKey {
    SigningKey::from_seed(&DEVICE_SEED).expect("seed is valid")
}

fn valid_envelope() -> SignedEnvelope {
    SignedEnvelope::sign(
        &device_key(),
        Sender::Device,
        DEVICE_ID,
        RUNTIME_ID,
        "str_1",
        7,
        AT,
        ContentType::SessionUpstream,
        PAYLOAD,
    )
    .expect("signing succeeds")
}

/// Build the document: the key material, then one accepted case and the
/// refusals, each tagged with the wire code a verifier must produce.
fn build_golden() -> serde_json::Value {
    let device = device_key();
    let public = device.verifying_key();
    let valid = valid_envelope();

    let mut forged_signature = valid.clone();
    // A relay holds no device key, so the best it can do is attach a signature
    // made with its own.
    let relay_key = SigningKey::from_seed(&RUNTIME_SEED).expect("seed is valid");
    let canonical = valid.canonical_string().expect("canonicalizes");
    forged_signature.sig_b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(relay_key_sign(&relay_key, &canonical))
    };

    let mut wrong_recipient = valid.clone();
    wrong_recipient.recipient_id = "rt_other".to_string();

    // The same attack after the relay tries to repair the audience field. It
    // has to be a separate vector: the first proves the check exists, only this
    // one proves `recipient_id` is inside the signing input.
    let recipient_rewritten = SignedEnvelope::sign(
        &device,
        Sender::Device,
        DEVICE_ID,
        "rt_other",
        "str_1",
        7,
        AT,
        ContentType::SessionUpstream,
        PAYLOAD,
    )
    .expect("signing succeeds");
    let mut recipient_rewritten_to_target = recipient_rewritten;
    recipient_rewritten_to_target.recipient_id = RUNTIME_ID.to_string();

    let mut illegal_id = valid.clone();
    illegal_id.sender_id = "dev|evil".to_string();

    let mut stale = valid.clone();
    stale.ts = "2026-07-25T11:00:00Z".to_string();

    json!({
        "note": "Answer key for the CodeLeveler signed envelope. Seeds are test-only. \
                 A conforming implementation must accept every `accept` case and refuse \
                 every `reject` case with the stated code.",
        "algorithm": {
            "signature": "Ed25519 over the canonical string, UTF-8",
            "canonical_string": "{v}|{sender}|{sender_id}|{recipient_id}|{stream_id}|{seq}|{ts}|{content_type}|{digest_hex}",
            "digest": "lowercase hex SHA-256 of the raw payload bytes, not of payload_b64",
            "payload_b64": "standard base64 with padding",
            "id_charset": "^[A-Za-z0-9_.:-]{1,64}$",
            "timestamp": "YYYY-MM-DDTHH:MM:SSZ, verified within ±120s"
        },
        "keys": {
            "device_seed_hex": hex(&DEVICE_SEED),
            "device_pubkey_b64url": public.to_base64url(),
            "device_fingerprint": public.fingerprint(),
            "device_fingerprint_display": public.fingerprint_display()
        },
        "verifier": {
            "recipient_id": RUNTIME_ID,
            "now": AT
        },
        "cases": [
            {
                "name": "valid_session_upstream",
                "expect": "accept",
                "why": "Baseline: correct key, correct audience, fresh timestamp.",
                "canonical_string": canonical,
                "payload_utf8": String::from_utf8_lossy(PAYLOAD),
                "envelope": valid
            },
            {
                "name": "forged_signature",
                "expect": "reject",
                "code": "signature_invalid",
                "why": "A compromised relay signs with a key of its own; it never holds the device's.",
                "envelope": forged_signature
            },
            {
                "name": "wrong_recipient_unchanged",
                "expect": "reject",
                "code": "recipient_mismatch",
                "why": "Cross-runtime replay: a genuine frame this device signed for another host, \
                        forwarded here untouched. The signature is valid — only the audience is \
                        wrong — so a verifier that does not compare recipient_id against its own \
                        id will wrongly accept it.",
                "envelope": wrong_recipient
            },
            {
                "name": "recipient_rewritten_to_target",
                "expect": "reject",
                "code": "signature_invalid",
                "why": "The same replay after the relay edits recipient_id to name this host, to \
                        get past the audience check. It fails because recipient_id is inside the \
                        signed canonical string. An implementation that omits recipient_id from \
                        its signing input will wrongly accept this one.",
                "envelope": recipient_rewritten_to_target
            },
            {
                "name": "illegal_id_character",
                "expect": "reject",
                "code": "invalid_frame",
                "why": "The separator inside an id could shift the canonical string's field \
                        boundaries; such an id must be refused before verification, not escaped.",
                "envelope": illegal_id
            },
            {
                "name": "stale_timestamp",
                "expect": "reject",
                "code": "replay",
                "why": "One hour old: outside the ±120s window even though the signature over the \
                        original timestamp is intact.",
                "envelope": stale
            }
        ]
    })
}

fn relay_key_sign(key: &SigningKey, canonical: &str) -> Vec<u8> {
    // Sign the victim's canonical string with the wrong key: the shape a forged
    // frame actually takes.
    let envelope = SignedEnvelope::sign(
        key,
        Sender::Device,
        DEVICE_ID,
        RUNTIME_ID,
        "str_1",
        7,
        AT,
        ContentType::SessionUpstream,
        PAYLOAD,
    )
    .expect("signing succeeds");
    assert_eq!(
        envelope.canonical_string().expect("canonicalizes"),
        canonical,
        "the forgery must differ only in the signing key"
    );
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(&envelope.sig_b64)
        .expect("valid base64")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn golden_file_is_current() {
    let generated = format!(
        "{}\n",
        serde_json::to_string_pretty(&build_golden()).expect("serializes")
    );
    let path = golden_path();

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("create testdata dir");
        std::fs::write(&path, generated).expect("write golden");
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{} is missing ({error}).\n\
             Regenerate: UPDATE_GOLDEN=1 cargo test -p leveler-remote-protocol --test golden_vectors",
            path.display()
        )
    });
    assert_eq!(
        committed,
        generated,
        "\n{} is stale.\nRegenerate and commit: UPDATE_GOLDEN=1 cargo test -p leveler-remote-protocol --test golden_vectors\n",
        path.display()
    );
}

/// The file is only a contract if its stated verdicts are the real ones. This
/// replays every case through the actual verifier.
#[test]
fn every_golden_case_behaves_as_documented() {
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        // The sibling test is rewriting the file in this same run; reading it
        // here would race. The check runs on the next ordinary invocation.
        return;
    }

    let golden: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(golden_path()).expect("golden exists"))
            .expect("golden parses");

    let public = VerifyingKey::from_base64url(
        golden["keys"]["device_pubkey_b64url"]
            .as_str()
            .expect("pubkey present"),
    )
    .expect("pubkey decodes");
    let recipient = golden["verifier"]["recipient_id"]
        .as_str()
        .expect("recipient present");
    let now = golden["verifier"]["now"].as_str().expect("now present");

    let cases = golden["cases"].as_array().expect("cases present");
    assert!(
        cases.len() >= 6,
        "the negative vectors are the point; do not shrink this set"
    );

    for case in cases {
        let name = case["name"].as_str().expect("name present");
        let envelope: SignedEnvelope =
            serde_json::from_value(case["envelope"].clone()).expect("envelope parses");
        let outcome = envelope.verify(&VerifyParams {
            expected_recipient_id: recipient,
            public_key: &public,
            now,
        });

        match case["expect"].as_str().expect("expect present") {
            "accept" => {
                let payload = outcome.unwrap_or_else(|error| {
                    panic!("{name} should verify, got {}", error.code());
                });
                assert_eq!(
                    payload,
                    case["payload_utf8"]
                        .as_str()
                        .expect("payload present")
                        .as_bytes(),
                    "{name} payload mismatch"
                );
            }
            "reject" => {
                let error = outcome
                    .err()
                    .unwrap_or_else(|| panic!("{name} must be refused but verified"));
                assert_eq!(
                    error.code(),
                    case["code"].as_str().expect("code present"),
                    "{name} refused for the wrong reason"
                );
            }
            other => panic!("{name}: unknown expectation {other}"),
        }
    }
}

/// The fingerprint is what a user compares between phone and terminal, so its
/// exact rendering is a contract, not a display detail.
#[test]
fn fingerprint_rendering_is_pinned() {
    let public = device_key().verifying_key();
    let fingerprint = public.fingerprint();

    assert_eq!(fingerprint.len(), 16, "8 bytes rendered as 16 hex chars");
    assert!(
        fingerprint
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "lowercase hex only: {fingerprint}"
    );
    assert_eq!(
        public.fingerprint_display(),
        format!(
            "{} {} {} {}",
            &fingerprint[0..4],
            &fingerprint[4..8],
            &fingerprint[8..12],
            &fingerprint[12..16]
        )
    );
}
