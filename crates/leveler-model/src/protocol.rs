//! The `ProtocolAdapter` trait (spec §13). The protocol layer is independent:
//! it only knows how to turn a unified [`ModelRequest`] into an HTTP request and
//! a provider HTTP response back into unified types. It must never reference a
//! concrete agent or provider.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::profile::{ProtocolKind, ReasoningConfig};
use crate::request::ModelRequest;
use crate::response::ModelResponse;
use crate::runtime::ModelEventStream;

/// Context threaded into protocol encoding/decoding (base url, model id, auth).
#[derive(Clone)]
pub struct ProtocolContext {
    pub base_url: String,
    pub model_id: String,
    pub api_key: Option<String>,
    pub extra_headers: Vec<(String, String)>,
    /// How to spell a reasoning request and the profile's fallback effort.
    /// A [`ModelRequest`] may override only the effort for one execution.
    pub reasoning: ReasoningConfig,
    /// Whether the model may emit parallel tool calls. When false, the request
    /// tells the provider to disable them (from the profile's capabilities).
    pub parallel_tool_calls: bool,
    /// Whether the provider accepts a caller-chosen `temperature`. Some endpoints
    /// (Kimi For Coding) reject every value but their own default, so the field
    /// is omitted rather than forwarded — see `CompatibilityConfig`.
    pub supports_temperature: bool,
    /// Whether the provider accepts a forced `tool_choice` while thinking mode
    /// is active. When false, the thinking-flag adapter explicitly disables
    /// thinking on forced-choice requests — see `CompatibilityConfig`.
    pub thinking_supports_forced_tool_choice: bool,
    /// The route's resolved reasoning-replay contract: WHEN captured
    /// reasoning is carried, and WHAT a turn that captured none carries. A
    /// route's protocol and declared compatibility resolve it once, here, so
    /// no adapter decides either question for itself.
    pub reasoning_replay: crate::projection::ReasoningReplayContract,
}

impl std::fmt::Debug for ProtocolContext {
    /// A protocol context carries the provider key and per-route auth headers.
    /// Debug is used in failure paths, so it names the credential's presence
    /// and header names but never their values.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProtocolContext")
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field(
                "api_key",
                &self
                    .api_key
                    .as_ref()
                    .map(|_| leveler_core::SECRET_PLACEHOLDER),
            )
            .field(
                "extra_headers",
                &self
                    .extra_headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("reasoning", &self.reasoning)
            .field("parallel_tool_calls", &self.parallel_tool_calls)
            .field("supports_temperature", &self.supports_temperature)
            .field(
                "thinking_supports_forced_tool_choice",
                &self.thinking_supports_forced_tool_choice,
            )
            .field("reasoning_replay", &self.reasoning_replay)
            .finish()
    }
}

/// A protocol-layer failure. Kept separate from `ModelError` because a protocol
/// adapter is transport-agnostic; the provider layer maps these onto
/// `ModelError`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
pub enum ProtocolError {
    #[error("failed to encode request: {0}")]
    Encode(String),
    #[error("failed to decode response: {0}")]
    Decode(String),
    #[error("provider returned status {status}: {message}")]
    Status { status: u16, message: String },
    #[error("malformed stream: {0}")]
    Stream(String),
}

/// Encodes unified requests to a wire protocol and decodes responses back.
///
/// Note: the encode step returns the raw building blocks (the provider layer
/// owns the `reqwest::Client` and actually sends), while decode consumes a
/// `reqwest::Response`. To keep this crate free of a hard `reqwest` dependency
/// in its public surface for encoding, we hand back a serialized JSON body plus
/// method/path; the provider assembles the final request.
#[async_trait]
pub trait ProtocolAdapter: Send + Sync {
    /// Which protocol this adapter implements.
    fn protocol(&self) -> ProtocolKind;

    /// Build the request body (JSON) and endpoint path for a unified request.
    fn encode_request(
        &self,
        request: &ModelRequest,
        context: &ProtocolContext,
        stream: bool,
    ) -> Result<EncodedRequest, ProtocolError>;

    /// Decode a complete (non-streaming) response body into unified form.
    fn decode_response(
        &self,
        body: &[u8],
        context: &ProtocolContext,
    ) -> Result<ModelResponse, ProtocolError>;

    /// Decode a streaming SSE byte stream into unified events.
    fn decode_stream(
        &self,
        stream: RawByteStream,
        context: &ProtocolContext,
    ) -> Result<ModelEventStream, ProtocolError>;
}

/// A serialized, protocol-specific HTTP request ready for the transport layer.
#[derive(Debug, Clone)]
pub struct EncodedRequest {
    /// Endpoint path appended to the provider base url (e.g. `/chat/completions`).
    pub path: String,
    /// JSON request body.
    pub body: serde_json::Value,
    /// Additional headers this protocol requires.
    pub headers: Vec<(String, String)>,
}

/// A raw byte stream (as delivered by the HTTP transport) fed to the stream
/// decoder. Errors are already normalized to `ModelError` by the transport.
pub type RawByteStream = std::pin::Pin<
    Box<dyn futures::Stream<Item = Result<bytes::Bytes, crate::error::ModelError>> + Send>,
>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_error_encode_display() {
        let err = ProtocolError::Encode("bad schema".to_string());
        assert_eq!(err.to_string(), "failed to encode request: bad schema");
    }

    #[test]
    fn protocol_error_decode_display() {
        let err = ProtocolError::Decode("bad json".to_string());
        assert_eq!(err.to_string(), "failed to decode response: bad json");
    }

    #[test]
    fn protocol_error_status_display() {
        let err = ProtocolError::Status {
            status: 500,
            message: "server error".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "provider returned status 500: server error"
        );
    }

    #[test]
    fn protocol_error_stream_display() {
        let err = ProtocolError::Stream("truncated".to_string());
        assert_eq!(err.to_string(), "malformed stream: truncated");
    }

    #[test]
    fn encoded_request_holds_parts() {
        let req = EncodedRequest {
            path: "/chat/completions".to_string(),
            body: serde_json::json!({"model":"gpt-4o"}),
            headers: vec![("Authorization".to_string(), "Bearer x".to_string())],
        };
        assert_eq!(req.path, "/chat/completions");
        assert_eq!(req.body["model"], "gpt-4o");
        assert_eq!(req.headers.len(), 1);
    }

    #[test]
    fn protocol_context_debug_never_prints_the_key_or_header_value() {
        let context = ProtocolContext {
            base_url: "https://example.test/v1".to_string(),
            model_id: "m".to_string(),
            api_key: Some("sk-live-canary-value".to_string()),
            extra_headers: vec![("x-api-key".to_string(), "header-canary".to_string())],
            reasoning: ReasoningConfig::default(),
            parallel_tool_calls: true,
            supports_temperature: true,
            thinking_supports_forced_tool_choice: true,
            reasoning_replay: crate::projection::ReasoningReplayContract::default(),
        };
        let rendered = format!("{context:?}");
        assert!(!rendered.contains("sk-live-canary-value"), "{rendered}");
        assert!(!rendered.contains("header-canary"), "{rendered}");
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
    }
}
