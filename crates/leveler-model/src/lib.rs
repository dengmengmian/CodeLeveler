//! `leveler-model` — the model-agnostic core of CodeLeveler.
//!
//! Everything above the protocol layer speaks *only* the unified vocabulary
//! defined here: [`ModelRequest`], [`ModelResponse`], [`ModelEvent`],
//! [`ModelError`]. No crate that consumes these types is ever allowed to know
//! which vendor produced them (spec §2.2).
//!
//! The two central traits also live here so they sit next to the types they
//! reference:
//! - [`ModelRuntime`] — what the agent calls to talk to a model.
//! - [`ProtocolAdapter`] — encodes/decodes a specific wire protocol (impl in
//!   `leveler-protocol`).
#![forbid(unsafe_code)]

pub mod error;
pub mod event;
pub mod message;
pub mod profile;
pub mod protocol;
pub mod request;
pub mod response;
pub mod runtime;
pub mod tool_catalog;

pub use error::{ModelError, ModelErrorKind};
pub use event::{FinishReason, ModelEvent, TokenUsage};
pub use message::{
    ContentPart, ImageSource, Message, Role, ToolCall, ToolChoice, ToolDefinition,
    ToolResultContent,
};
pub use profile::{
    CompatibilityConfig, ModelCapabilities, ModelLimits, ModelPricing, ModelProfile, ProtocolKind,
    ReasoningConfig, ReasoningEffort, ReasoningEffortSource, ReasoningStyle, ResolvedReasoning,
    normalize_reasoning_effort, resolve_reasoning_effort, validate_reasoning_config,
};
pub use protocol::{
    EncodedRequest, ProtocolAdapter, ProtocolContext, ProtocolError, RawByteStream,
};
pub use request::{ModelRef, ModelRequest, RequestMetadata, TransportPolicy};
pub use response::ModelResponse;
pub use runtime::{ModelEventStream, ModelRuntime, stream_from_response};
pub use tool_catalog::{
    BuiltinToolClass, BuiltinToolMetadata, builtin_observe_key, builtin_tool_metadata,
    is_safe_replay_tool, is_search_tool,
};
