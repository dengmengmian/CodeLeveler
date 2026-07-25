//! The agent tunnel: one outbound WebSocket from the developer machine to the
//! relay, multiplexing every APP stream and RPC.
//!
//! Outbound because the alternative — a listener on the developer's machine —
//! is the remote-code-execution gateway the whole design exists to avoid. The
//! relay never connects *to* the agent.
//!
//! Frames here are routing metadata. Anything that carries session meaning or
//! influences a security decision travels as a [`SignedEnvelope`] inside
//! `forward_*` / `rpc_*`, so a relay can route these frames but cannot author
//! the content they carry.

use serde::{Deserialize, Serialize};

use crate::auth::ProtocolVersionDto;
use crate::envelope::SignedEnvelope;
use crate::pairing::{PairDecision, PairingPending, PairingScope};

/// Relay → agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayToAgent {
    RegisterAck {
        runtime_id: String,
        protocol: ProtocolVersionDto,
    },
    RegisterNack {
        code: String,
        message: String,
    },
    /// A device is waiting for this host to accept it.
    PairingPending {
        #[serde(flatten)]
        pending: PairingPending,
    },
    /// A new APP session stream.
    ///
    /// Carries no device public key on purpose: the agent resolves the key from
    /// its own `devices.json`. A relay that supplied one here would be choosing
    /// what the agent trusts.
    OpenStream {
        stream_id: String,
        device_id: String,
        pairing_scope: PairingScope,
        access_jti: String,
    },
    CloseStream {
        stream_id: String,
        reason: String,
    },
    ForwardUpstream {
        stream_id: String,
        frame: SignedEnvelope,
    },
    RpcRequest {
        rpc_id: String,
        envelope: SignedEnvelope,
    },
    HeartbeatAck {
        ts: String,
    },
}

/// Agent → relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentToRelay {
    Register {
        runtime_id: String,
        display_name: String,
        protocol: ProtocolVersionDto,
        pubkey: String,
        ts: String,
        sig: String,
    },
    Heartbeat {
        ts: String,
        active_streams: u32,
    },
    /// Sent only after an accepted device's key has been persisted locally, so
    /// a crash between the two cannot leave the agent trusting a key it never
    /// wrote down.
    PairConfirm {
        pairing_id: String,
        decision: PairDecision,
    },
    StreamAccepted {
        stream_id: String,
    },
    StreamRejected {
        stream_id: String,
        code: String,
    },
    ForwardDownstream {
        stream_id: String,
        frame: SignedEnvelope,
    },
    /// A business result is always a runtime-signed envelope; `error` is for
    /// routing-level failures only and carries no business body.
    RpcResponse {
        rpc_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        envelope: Option<SignedEnvelope>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<RoutingError>,
    },
    RuntimeOfflineHint {
        reason: String,
    },
}

/// A failure with no signed body: the runtime never produced a result, so there
/// is nothing for it to have signed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingError {
    pub code: String,
    pub message: String,
}

/// The RPC methods the APP reaches through the tunnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcMethod {
    CreateSession,
    Snapshot,
    UploadAttachment,
}

/// The payload inside an `rpc_request` envelope.
///
/// `body` stays opaque here. The business types it carries
/// (`CreateSessionRequest`, `SessionBootstrap`, …) are owned by
/// `leveler-local-transport` and the client protocol; redefining them in this
/// crate would create a second source of truth that drifts from the first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcRequestPayload {
    pub method: RpcMethod,
    pub body: serde_json::Value,
}

/// Stream ids for RPC envelopes are `rpc:{uuid}`, and a response reuses its
/// request's id. That is what binds the two cryptographically: the id sits
/// inside the signed canonical string, so a relay cannot pair a genuine
/// response with a different request.
pub fn rpc_stream_id(rpc_uuid: &str) -> String {
    format!("rpc:{rpc_uuid}")
}

/// Whether `stream_id` denotes an RPC exchange rather than a session stream.
pub fn is_rpc_stream_id(stream_id: &str) -> bool {
    stream_id.starts_with("rpc:")
}
