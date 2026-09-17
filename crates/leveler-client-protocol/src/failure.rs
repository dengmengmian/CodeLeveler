//! The product-level failure contract: how a client learns *what kind* of
//! failure happened without parsing a provider's raw error string.
//!
//! This is the presentation-facing projection of a runtime failure. It keeps
//! machine semantics (`category`, `source`, `retryability`, `delivery`) apart
//! from the raw diagnostic `detail`, so the primary line never leaks a vendor
//! payload and the detail is still available on disclosure.
//!
//! It is deliberately *not* a second error hierarchy: it is derived from the
//! runtime's own typed errors (today `leveler_model::ModelError`) at the
//! composition layer, and it invents no fact that layer did not prove.

use serde::{Deserialize, Serialize};

/// What kind of failure this is, in product terms. Vendor error codes are not
/// part of this vocabulary — `Moonshot invalid_request_error`, `OpenAI
/// invalid_request_error` and `Anthropic invalid_request_error` are all just
/// [`FailureCategory::InvalidRequest`] here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// A network/transport failure reaching a provider or service.
    Network,
    /// A timeout elapsed.
    Timeout,
    /// Authentication or authorization failed.
    Authentication,
    /// A rate limit was hit.
    RateLimit,
    /// The provider itself is unavailable or failed.
    Provider,
    /// The request was malformed or rejected as invalid.
    InvalidRequest,
    /// A tool or command failed.
    Tool,
    /// A runtime/infrastructure failure that is not a provider call.
    Runtime,
    /// Permission was refused.
    Permission,
    /// The work was cancelled.
    Cancelled,
    /// Anything not covered above.
    Internal,
}

/// Which boundary produced the failure. Sources share retry primitives but
/// never share error truth: a provider disconnect, an MCP disconnect, a failed
/// `curl` command and a runtime-IPC drop are different facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FailureSource {
    /// The model provider transport.
    Provider,
    /// The runtime itself (persistence, lifecycle, IPC).
    Runtime,
    /// An MCP server transport.
    Mcp,
    /// A tool or command's own network/execution.
    Tool,
    /// Local execution that did not touch the network.
    Local,
}

/// Whether an automatic retry is possible, from the delivery truth. Mirrors
/// `leveler_model::Retryability` in product terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FailureRetryability {
    /// The request provably did no provider-side work, or the provider asked
    /// to be retried: a bounded automatic retry is safe.
    Safe,
    /// The request may have reached the provider: retrying could duplicate
    /// work, so it is never automatic.
    Caution,
    /// Delivery could not be established: never automatic, reported as unknown.
    Unknown,
    /// Retrying the identical request cannot help.
    Never,
}

/// What the transport could prove about whether the request reached the
/// provider. Mirrors `leveler_model::DeliveryState` in product terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FailureDelivery {
    /// Provably never sent.
    NotSent,
    /// Written, but no response was observed.
    SentNoResponse,
    /// A complete HTTP status was returned.
    Responded,
    /// The response stream began and ended early. `text`/`tool_args` are what
    /// it produced before the cut.
    StreamInterrupted {
        /// Assistant text arrived before the cut.
        text: bool,
        /// Tool-call arguments began arriving before the cut.
        tool_args: bool,
    },
    /// Delivery could not be established.
    Unknown,
}

impl FailureCategory {
    /// A stable, locale-independent code for display and logging
    /// (`invalid_request`, `authentication`, …).
    pub fn code(self) -> &'static str {
        match self {
            FailureCategory::Network => "network",
            FailureCategory::Timeout => "timeout",
            FailureCategory::Authentication => "authentication",
            FailureCategory::RateLimit => "rate_limit",
            FailureCategory::Provider => "provider",
            FailureCategory::InvalidRequest => "invalid_request",
            FailureCategory::Tool => "tool",
            FailureCategory::Runtime => "runtime",
            FailureCategory::Permission => "permission",
            FailureCategory::Cancelled => "cancelled",
            FailureCategory::Internal => "internal",
        }
    }

    /// Whether the provider produced this failure (so a `provider · code`
    /// subtitle is meaningful).
    pub fn is_provider(self) -> bool {
        matches!(
            self,
            FailureCategory::Network
                | FailureCategory::Timeout
                | FailureCategory::Authentication
                | FailureCategory::RateLimit
                | FailureCategory::Provider
                | FailureCategory::InvalidRequest
        )
    }
}

/// A structured product failure attached to a terminal turn failure.
///
/// `detail` is the raw diagnostic and belongs on disclosure only; every other
/// field is machine semantics suitable for choosing an icon, a title, and an
/// action without reading the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiFailure {
    pub category: FailureCategory,
    pub source: FailureSource,
    /// The provider id, when the failure came from a provider call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model id the failing request was addressed to, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The provider's own error code, when it reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
    /// The provider's correlation id for the failing request, when the
    /// response exposed one. The single most useful field for a support
    /// report against a vendor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// HTTP status, when the failure had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub retryability: FailureRetryability,
    pub delivery: FailureDelivery,
    /// A short, provider-agnostic one-line product explanation for the primary
    /// line (e.g. "模型服务拒绝了当前请求。"). Clients may override it with
    /// their own localization keyed on `category`.
    pub summary: String,
    /// The raw technical detail. Disclosure only — never the primary line.
    pub detail: String,
}

impl UiFailure {
    /// Whether this failure's raw detail would add anything beyond the
    /// category. Detail equal to a generic category message says nothing new.
    pub fn has_detail(&self) -> bool {
        !self.detail.trim().is_empty()
    }
}
