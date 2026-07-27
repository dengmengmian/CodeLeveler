//! Random input against every parser a relay can reach.
//!
//! The other negative tests are aimed: each one names an attack and checks that
//! exact thing. That is the wrong shape for the question "does this parser fall
//! over on input nobody thought of", because the inputs come from the same head
//! that wrote the code. So this one does not think — it takes known-good frames
//! and mangles them, tens of thousands of times, and asserts only the two
//! properties that must hold for *any* input:
//!
//! 1. Nothing panics. A panic in a relay-facing parser is a remote crash.
//! 2. A frame that verifies has an untouched payload. Corruption must not be
//!    able to produce a signature that still checks out.
//!
//! This is **not** structured fuzzing. `cargo-fuzz` drives libFuzzer with
//! coverage feedback and would explore paths a blind mutator never reaches; it
//! needs a nightly toolchain, which this repository does not require of anyone.
//! What this buys instead is a deterministic, dependency-free mutation pass
//! that runs in CI on every commit — a weaker instrument that is actually
//! pointed at the code, rather than a stronger one nobody runs.
//!
//! Deterministic on purpose: a failure here reproduces from its seed instead of
//! being a story about a run that once went red.

use leveler_remote_protocol::auth::{DeviceAssertion, RuntimeAssertion};
use leveler_remote_protocol::tunnel::{AgentToRelay, RelayToAgent, RpcRequestPayload};
use leveler_remote_protocol::{
    ContentType, Sender, SignedEnvelope, SigningKey, VerifyParams, VerifyingKey,
};

const RUNTIME_SEED: [u8; 32] = [7u8; 32];
const DEVICE_SEED: [u8; 32] = [9u8; 32];
const AT: &str = "2026-07-25T12:00:00Z";
const PAYLOAD: &[u8] = br#"{"type":"deliver","command_id":"c1","session_id":"s1","command":{"type":"submit_message","session_id":"s1","content":"hello"}}"#;

/// A small deterministic generator. Not cryptographic — it only has to produce
/// the same haystack on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*: two lines, no dependency, good enough to shake a parser.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

/// Damage `input` in one of a few ways a wire actually gets damaged.
fn mutate(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    let mut bytes = input.to_vec();
    if bytes.is_empty() {
        return bytes;
    }
    match rng.below(6) {
        0 => {
            // Flip a bit.
            let at = rng.below(bytes.len());
            bytes[at] ^= 1 << rng.below(8);
        }
        1 => {
            // Truncate — the shape a dropped connection leaves.
            let keep = rng.below(bytes.len());
            bytes.truncate(keep);
        }
        2 => {
            // Insert a byte, favouring the ones that mean something in JSON and
            // in the canonical signing string.
            let at = rng.below(bytes.len());
            let byte = *b"|{}[]\",:\\\x00\xff".get(rng.below(11)).unwrap_or(&b'|');
            bytes.insert(at, byte);
        }
        3 => {
            // Delete a byte.
            let at = rng.below(bytes.len());
            bytes.remove(at);
        }
        4 => {
            // Duplicate a run — how a framing bug looks from the far side.
            let at = rng.below(bytes.len());
            let len = rng.below(bytes.len() - at).min(32);
            let run = bytes[at..at + len].to_vec();
            bytes.splice(at..at, run);
        }
        _ => {
            // Replace a byte with an arbitrary one, including invalid UTF-8.
            let at = rng.below(bytes.len());
            bytes[at] = (rng.next() & 0xff) as u8;
        }
    }
    bytes
}

/// Check a base64 detached signature the way the relay does, so a mangled
/// signature field goes through the same decode-then-verify path.
fn verify_detached_b64(key: &VerifyingKey, message: &[u8], signature_b64: &str) -> bool {
    use base64::Engine as _;
    match base64::engine::general_purpose::STANDARD.decode(signature_b64.as_bytes()) {
        Ok(signature) => key.verify_detached(message, &signature),
        Err(_) => false,
    }
}

fn runtime_key() -> SigningKey {
    SigningKey::from_seed(&RUNTIME_SEED).expect("seed is valid")
}

fn device_key() -> SigningKey {
    SigningKey::from_seed(&DEVICE_SEED).expect("seed is valid")
}

fn good_envelope() -> SignedEnvelope {
    SignedEnvelope::sign(
        &device_key(),
        Sender::Device,
        "dev_phone",
        "rt_host",
        "str_1",
        1,
        AT,
        ContentType::SessionUpstream,
        PAYLOAD,
    )
    .expect("signing succeeds")
}

/// The frames a relay hands an agent, and an agent hands a relay.
fn tunnel_corpus() -> Vec<String> {
    vec![
        serde_json::to_string(&RelayToAgent::OpenStream {
            stream_id: "str_1".to_string(),
            device_id: "dev_phone".to_string(),
            project_id: Some("0123456789abcdef".to_string()),
            pairing_scope: leveler_remote_protocol::pairing::PairingScope::Interactive,
            access_jti: "jti_1".to_string(),
        })
        .unwrap(),
        serde_json::to_string(&RelayToAgent::CloseStream {
            stream_id: "str_1".to_string(),
            reason: "revoked".to_string(),
        })
        .unwrap(),
        serde_json::to_string(&AgentToRelay::StreamRejected {
            stream_id: "str_1".to_string(),
            code: "unauthorized".to_string(),
        })
        .unwrap(),
        serde_json::to_string(&RpcRequestPayload {
            method: leveler_remote_protocol::tunnel::RpcMethod::CreateSession,
            project_id: Some("0123456789abcdef".to_string()),
            body: serde_json::json!({"goal": "x"}),
        })
        .unwrap(),
    ]
}

/// Every parser that sees bytes chosen by someone else, given 20k mangled
/// inputs. The assertion is the absence of a panic; `verify` additionally must
/// never bless a changed payload.
#[test]
fn no_relay_facing_parser_falls_over_on_mangled_input() {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let envelope_json = serde_json::to_string(&good_envelope()).expect("serialises");
    let runtime_public = runtime_key().verifying_key();
    let device_public = device_key().verifying_key();
    let corpus = tunnel_corpus();

    let mut verified_after_mutation = 0usize;
    for round in 0..20_000u32 {
        // Envelope JSON: what a relay forwards.
        let bytes = mutate(&mut rng, envelope_json.as_bytes());
        if let Ok(envelope) = serde_json::from_slice::<SignedEnvelope>(&bytes) {
            let outcome = envelope.verify(&VerifyParams {
                expected_recipient_id: "rt_host",
                public_key: &device_public,
                now: AT,
            });
            if let Ok(payload) = outcome {
                // A mutation that still verifies is only legitimate if it left
                // the signed bytes alone — a changed `sender`, a changed
                // digest, anything inside the canonical string must fail.
                assert_eq!(
                    payload, PAYLOAD,
                    "round {round}: a mutated frame verified with a different payload"
                );
                verified_after_mutation += 1;
            }
        }

        // Tunnel control frames, both directions.
        let seed = &corpus[rng.below(corpus.len())];
        let bytes = mutate(&mut rng, seed.as_bytes());
        let _ = serde_json::from_slice::<RelayToAgent>(&bytes);
        let _ = serde_json::from_slice::<AgentToRelay>(&bytes);
        let _ = serde_json::from_slice::<RpcRequestPayload>(&bytes);

        // The inner session message, which is parsed only after a signature
        // checks out — but the payload it covers is still attacker-chosen.
        let bytes = mutate(&mut rng, PAYLOAD);
        let _ = serde_json::from_slice::<leveler_session_wire::UpstreamMessage>(&bytes);

        // Header parsing: these arrive as raw strings on control-plane requests,
        // before anything has been authenticated.
        let header =
            RuntimeAssertion::header_value(&runtime_key(), "pair_begin", "rt_host", AT, "n1");
        let bytes = mutate(&mut rng, header.as_bytes());
        if let Ok(text) = std::str::from_utf8(&bytes)
            && let Some((runtime_id, timestamp, nonce, sig)) = RuntimeAssertion::parse_header(text)
        {
            // The relay's own sequence: rebuild the signing input from the
            // parsed fields and check it, which is where a mangled id or
            // timestamp would have to be survived.
            let input = RuntimeAssertion::signing_input("pair_begin", runtime_id, timestamp, nonce);
            let _ = verify_detached_b64(&runtime_public, input.as_bytes(), sig);
        }

        // Base64 public keys, straight off the wire.
        let bytes = mutate(&mut rng, device_public.to_base64url().as_bytes());
        if let Ok(text) = std::str::from_utf8(&bytes) {
            let _ = VerifyingKey::from_base64url(text);
        }
    }

    // A sanity check on the haystack itself: if *every* mutation had produced
    // an unparseable frame, this test would pass while exercising nothing.
    // Some mutations land in unsigned fields and still verify, and that is the
    // path the payload assertion above guards.
    assert!(
        verified_after_mutation > 0,
        "no mutated frame ever reached verification — the corpus is not being parsed at all"
    );
}

/// Device assertions come from an unauthenticated caller at `/v1/auth/session`.
#[test]
fn device_assertions_survive_mangled_input() {
    let mut rng = Rng(0x5EED_1234_ABCD_0002);
    let good = DeviceAssertion {
        device_id: "dev_phone".to_string(),
        timestamp: AT.to_string(),
        sig: "not-a-real-signature".to_string(),
    };
    let json = serde_json::to_string(&good).expect("serialises");
    let public = device_key().verifying_key();

    for _ in 0..15_000u32 {
        let bytes = mutate(&mut rng, json.as_bytes());
        if let Ok(assertion) = serde_json::from_slice::<DeviceAssertion>(&bytes) {
            // Must return a verdict rather than panic, whatever the fields hold.
            let input = DeviceAssertion::signing_input(&assertion.device_id, &assertion.timestamp);
            let _ = verify_detached_b64(&public, input.as_bytes(), &assertion.sig);
        }
    }
}
