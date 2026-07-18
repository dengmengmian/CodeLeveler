//! `leveler-model` — the model-agnostic core of CodeLeveler.
//!
//! Everything above the protocol layer speaks *only* the unified vocabulary
//! defined here: [`ModelRequest`], [`ModelResponse`], [`ModelEvent`],
//! [`ModelError`]. No crate that consumes these types is ever allowed to know
//! which vendor produced them (spec §2.2).
//!
//! The three central traits also live here so they sit next to the types they
//! reference:
//! - [`ModelRuntime`] — what the agent calls to talk to a model.
//! - [`ProtocolAdapter`] — encodes/decodes a specific wire protocol (impl in
//!   `leveler-protocol`).
//! - [`CompatibilityMiddleware`] — patches per-provider quirks.
#![forbid(unsafe_code)]

pub mod error;
pub mod event;
pub mod message;
pub mod middleware;
pub mod profile;
pub mod protocol;
pub mod request;
pub mod response;
pub mod runtime;

pub use error::{ModelError, ModelErrorKind};
pub use event::{FinishReason, ModelEvent, TokenUsage};
pub use message::{
    ContentPart, ImageSource, Message, Role, ToolCall, ToolChoice, ToolDefinition,
    ToolResultContent,
};
pub use middleware::{CompatibilityContext, CompatibilityError, CompatibilityMiddleware};
pub use profile::{
    CompatibilityConfig, ModelCapabilities, ModelLimits, ModelPricing, ModelProfile, ProtocolKind,
    ReasoningConfig, ReasoningEffort, ReasoningStyle,
};
pub use protocol::{
    EncodedRequest, ProtocolAdapter, ProtocolContext, ProtocolError, RawByteStream,
};
pub use request::{ModelRef, ModelRequest, RequestMetadata};
pub use response::ModelResponse;
pub use runtime::{ModelEventStream, ModelRuntime};
