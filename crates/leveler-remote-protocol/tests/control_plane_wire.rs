//! Serde fixtures for the control plane: tunnel frames, token claims, pairing.
//!
//! The relay is written against these shapes and the APP against the same
//! bytes, so the JSON spelling is the contract. Pinning it here means a rename
//! shows up as a failing test rather than as a client that silently stops
//! understanding a frame.

use leveler_remote_protocol::auth::{
    ProtocolVersionDto, SCOPE_REMOTE_SESSION, SessionAuthRequest, TokenClaims, TokenUse,
};
use leveler_remote_protocol::pairing::{
    DeviceStore, PairDecision, PairedDevice, PairingScope, PairingState,
};
use leveler_remote_protocol::tunnel::{
    AgentToRelay, RelayToAgent, RpcMethod, RpcRequestPayload, is_rpc_stream_id, rpc_stream_id,
};

#[test]
fn token_claims_match_the_documented_shape() {
    let claims = TokenClaims {
        iss: "leveler-relay".to_string(),
        sub: "dev_1".to_string(),
        aud: "rt_7".to_string(),
        jti: "jti_1".to_string(),
        iat: 1_785_326_400,
        exp: 1_785_327_300,
        scope: SCOPE_REMOTE_SESSION.to_string(),
        pairing_scope: PairingScope::Interactive,
        token_use: TokenUse::Access,
    };

    assert_eq!(
        serde_json::to_string(&claims).unwrap(),
        r#"{"iss":"leveler-relay","sub":"dev_1","aud":"rt_7","jti":"jti_1","iat":1785326400,"exp":1785327300,"scope":"remote.session","pairing_scope":"interactive","token_use":"access"}"#
    );

    let back: TokenClaims = serde_json::from_str(
        r#"{"iss":"leveler-relay","sub":"dev_1","aud":"rt_7","jti":"jti_1","iat":0,"exp":0,"scope":"remote.session","pairing_scope":"observe","token_use":"refresh"}"#,
    )
    .unwrap();
    assert_eq!(back.pairing_scope, PairingScope::Observe);
    assert_eq!(back.token_use, TokenUse::Refresh);
}

/// `aud` names the one runtime a token may reach, so a token minted for one
/// host is inert against another even before signatures are considered.
#[test]
fn token_audience_is_a_single_runtime() {
    let json = r#"{"iss":"leveler-relay","sub":"dev_1","aud":"rt_7","jti":"j","iat":0,"exp":0,"scope":"remote.session","pairing_scope":"interactive","token_use":"access"}"#;
    let claims: TokenClaims = serde_json::from_str(json).unwrap();
    assert_eq!(claims.aud, "rt_7");
}

#[test]
fn session_auth_signing_input_is_pipe_joined() {
    assert_eq!(
        SessionAuthRequest::signing_input("dev_1", "rt_7", "2026-07-25T12:00:00Z", "nonce_1"),
        "dev_1|rt_7|2026-07-25T12:00:00Z|nonce_1"
    );
}

#[test]
fn relay_to_agent_frames_round_trip() {
    let open = RelayToAgent::OpenStream {
        stream_id: "str_1".to_string(),
        device_id: "dev_1".to_string(),
        pairing_scope: PairingScope::Interactive,
        access_jti: "jti_1".to_string(),
    };
    assert_eq!(
        serde_json::to_string(&open).unwrap(),
        r#"{"type":"open_stream","stream_id":"str_1","device_id":"dev_1","pairing_scope":"interactive","access_jti":"jti_1"}"#
    );

    let ack = RelayToAgent::RegisterAck {
        runtime_id: "rt_7".to_string(),
        protocol: ProtocolVersionDto { major: 1, minor: 3 },
    };
    assert_eq!(
        serde_json::to_string(&ack).unwrap(),
        r#"{"type":"register_ack","runtime_id":"rt_7","protocol":{"major":1,"minor":3}}"#
    );
}

/// `open_stream` must not carry a device key. The agent resolves it from its
/// own store; a key on this frame would let the relay choose what is trusted.
#[test]
fn open_stream_carries_no_device_public_key() {
    let open = RelayToAgent::OpenStream {
        stream_id: "str_1".to_string(),
        device_id: "dev_1".to_string(),
        pairing_scope: PairingScope::Interactive,
        access_jti: "jti_1".to_string(),
    };
    let json = serde_json::to_string(&open).unwrap();
    assert!(
        !json.contains("pubkey") && !json.contains("public_key"),
        "open_stream must not offer key material: {json}"
    );
}

#[test]
fn agent_to_relay_frames_round_trip() {
    let confirm = AgentToRelay::PairConfirm {
        pairing_id: "pair_1".to_string(),
        decision: PairDecision::Accept,
    };
    assert_eq!(
        serde_json::to_string(&confirm).unwrap(),
        r#"{"type":"pair_confirm","pairing_id":"pair_1","decision":"accept"}"#
    );

    let rejected = AgentToRelay::StreamRejected {
        stream_id: "str_1".to_string(),
        code: "device_revoked".to_string(),
    };
    assert_eq!(
        serde_json::to_string(&rejected).unwrap(),
        r#"{"type":"stream_rejected","stream_id":"str_1","code":"device_revoked"}"#
    );
}

/// A routing-level failure carries no business body, and the success path omits
/// the error — the two are never both present.
#[test]
fn rpc_response_omits_the_absent_half() {
    let offline = AgentToRelay::RpcResponse {
        rpc_id: "rpc_1".to_string(),
        envelope: None,
        error: Some(leveler_remote_protocol::tunnel::RoutingError {
            code: "runtime_offline".to_string(),
            message: "no active tunnel".to_string(),
        }),
    };
    let json = serde_json::to_string(&offline).unwrap();
    assert!(!json.contains("\"envelope\""), "absent envelope is omitted");
    assert!(json.contains("runtime_offline"));
}

#[test]
fn rpc_request_payload_keeps_its_body_opaque() {
    let payload = RpcRequestPayload {
        method: RpcMethod::CreateSession,
        body: serde_json::json!({"repository": "/repo", "goal": "hi"}),
    };
    let json = serde_json::to_string(&payload).unwrap();
    assert!(json.starts_with(r#"{"method":"create_session","body":{"#));

    // Round-trips unchanged: this crate never interprets the body, so a field
    // it has never heard of survives.
    let back: RpcRequestPayload = serde_json::from_str(
        r#"{"method":"snapshot","body":{"session_id":"s1","unknown_future_field":42}}"#,
    )
    .unwrap();
    assert_eq!(back.method, RpcMethod::Snapshot);
    assert_eq!(back.body["unknown_future_field"], 42);
}

/// A response reuses its request's stream id, which is what binds the pair
/// cryptographically once it is inside the signed canonical string.
#[test]
fn rpc_stream_ids_are_prefixed_and_recognisable() {
    let id = rpc_stream_id("2f8a1b3c");
    assert_eq!(id, "rpc:2f8a1b3c");
    assert!(is_rpc_stream_id(&id));
    assert!(!is_rpc_stream_id("str_1"));
    assert!(
        leveler_remote_protocol::id_is_valid(&id),
        "an rpc stream id must satisfy the canonical-string id rule"
    );
}

#[test]
fn pairing_states_and_scopes_serialize_snake_case() {
    assert_eq!(
        serde_json::to_string(&PairingState::PendingConfirm).unwrap(),
        r#""pending_confirm""#
    );
    assert_eq!(
        serde_json::to_string(&PairingScope::Observe).unwrap(),
        r#""observe""#
    );
    assert_eq!(PairingScope::default(), PairingScope::Interactive);
}

/// Revocation takes effect on the next frame, not at the next reconnection.
#[test]
fn a_revoked_device_stops_resolving() {
    let store = DeviceStore {
        devices: vec![
            PairedDevice {
                device_id: "dev_live".to_string(),
                device_pubkey_b64: "AAAA".to_string(),
                fingerprint: "a1b2c3d4e5f60708".to_string(),
                name: "iPhone".to_string(),
                scope: PairingScope::Interactive,
                paired_at: "2026-07-25T12:00:00Z".to_string(),
                revoked_at: None,
            },
            PairedDevice {
                device_id: "dev_gone".to_string(),
                device_pubkey_b64: "BBBB".to_string(),
                fingerprint: "0102030405060708".to_string(),
                name: "old phone".to_string(),
                scope: PairingScope::Interactive,
                paired_at: "2026-07-01T12:00:00Z".to_string(),
                revoked_at: Some("2026-07-20T12:00:00Z".to_string()),
            },
        ],
    };

    assert!(store.active_key_for("dev_live").is_some());
    assert!(
        store.active_key_for("dev_gone").is_none(),
        "a revoked device must not resolve to a key"
    );
    assert!(store.active_key_for("dev_unknown").is_none());
}

/// Older `devices.json` files predate `revoked_at`; they must still load.
#[test]
fn device_store_reads_a_file_without_optional_fields() {
    let store: DeviceStore = serde_json::from_str(
        r#"{"devices":[{"device_id":"dev_1","device_pubkey_b64":"AAAA","fingerprint":"a1b2c3d4e5f60708","name":"iPhone","scope":"interactive","paired_at":"2026-07-25T12:00:00Z"}]}"#,
    )
    .unwrap();
    assert!(store.active_key_for("dev_1").is_some());
}
