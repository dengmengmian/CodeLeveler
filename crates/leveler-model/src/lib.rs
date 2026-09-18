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

pub mod context_accounting;
pub mod error;
pub mod event;
pub mod message;
pub mod profile;
pub mod protocol;
pub mod request;
pub mod response;
pub mod runtime;
pub mod tool_exchange;

pub use context_accounting::{
    COMPACTION_BREADCRUMB_MARKER, CompactionRecord, ContextAccounting, ContextCategory,
    ContextPressure, TokenCountKind,
};
pub use error::{DeliveryState, ModelError, ModelErrorKind, Retryability, StreamProgress};
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
pub use tool_exchange::{ToolExchangeViolation, validate_tool_exchange};
