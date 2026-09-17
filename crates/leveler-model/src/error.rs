//! Unified, normalized model errors (spec §6.6, §31 model-error slice).
//!
//! Every provider failure must be mapped onto one of these kinds so recovery
//! logic never has to string-match vendor messages.
//!
//! Beyond the *kind* (what went wrong), an error carries the **delivery
//! truth** (what the transport could prove about whether the request reached
//! the provider). Retry policy is derived from both, never from the message:
//! a request that provably never left may be re-sent automatically, while one
//! that may already have been processed must not be blind-replayed.

use serde::{Deserialize, Serialize};

/// A coarse classification of what went wrong talking to a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    /// Authentication/authorization failure (bad or missing API key).
    Auth,
    /// The request was malformed or rejected as invalid.
    InvalidRequest,
    /// Provider rate limit (HTTP 429).
    RateLimit,
    /// Provider is unavailable (5xx, connection refused).
    ProviderUnavailable,
    /// A network/transport failure (DNS, TLS, connection reset).
    Transport,
    /// The stream was interrupted before completion.
    StreamInterrupted,
    /// The provider sent a body we could not decode.
    Decode,
    /// The model hit its output-token cap, truncating the response (e.g. a
    /// tool-call payload cut off mid-JSON).
    Truncated,
    /// The provider stopped generation because its content policy filtered the
    /// response. This is terminal, but it is not a successful model answer.
    ContentFiltered,
    /// A timeout elapsed.
    Timeout,
    /// The request was cancelled by the caller.
    Cancelled,
    /// Anything not covered above.
    Other,
}

/// How much output a stream produced before it was cut.
///
/// Only meaningful for [`DeliveryState::StreamInterrupted`]. Both flags are
/// recorded because they answer different questions: text already shown to a
/// person, and a tool call the model had begun describing (which, unfinished,
/// is never executable). Whether *any* output exists is what retry policy
/// needs; the split is what presentation needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamProgress {
    /// Assistant text was received before the cut.
    pub text: bool,
    /// Tool-call arguments began streaming before the cut. Such a call did not
    /// complete and must never be executed.
    pub tool_args: bool,
}

impl StreamProgress {
    /// Whether any output at all was produced. A stream cut before any output
    /// carries nothing to duplicate, so re-requesting is still safe.
    pub fn any(self) -> bool {
        self.text || self.tool_args
    }
}

/// What the transport could establish about whether a request reached the
/// provider. Produced where the fact is actually known — the HTTP transport
/// and the stream assembler — and consumed by retry policy and presentation.
///
/// This is deliberately coarse: it records only what can be *proven*. When the
/// stack cannot tell whether the request was processed, the honest answer is
/// [`DeliveryState::Unknown`], never a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// The request provably never left this process: connect/DNS/TLS failed
    /// before any request byte was written.
    NotSent,
    /// The request was written; no response was observed. Whether the provider
    /// processed it is not knowable here.
    SentNoResponse,
    /// The provider returned a complete HTTP status (the request reached the
    /// gateway). Whether the model itself ran is a function of the status.
    Responded,
    /// The response stream began and ended before a terminal event.
    StreamInterrupted {
        /// How far the stream got before the cut.
        progress: StreamProgress,
    },
    /// Nothing about delivery could be established.
    Unknown,
}

impl Default for DeliveryState {
    /// Legacy rows and errors built without delivery evidence read as
    /// [`DeliveryState::Unknown`] — the conservative answer, never a claim.
    fn default() -> Self {
        DeliveryState::Unknown
    }
}

/// Whether a failed attempt may be retried, derived from what the transport
/// proved about delivery and the kind of failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retryability {
    /// The request provably did no provider-side work, or the provider
    /// explicitly asked to be retried. A bounded automatic retry cannot
    /// duplicate anything.
    Safe,
    /// The request may have reached the provider. Retrying could duplicate
    /// generation, cost, or a tool call, so it is never automatic: the runtime
    /// surfaces a recoverable state and a person decides.
    Caution,
    /// Delivery could not be established at all. Never automatic, and reported
    /// as "unknown" rather than "interrupted".
    Unknown,
    /// Retrying the identical request cannot help (auth, invalid request,
    /// decode, truncation).
    Never,
}

/// A normalized model error carrying its kind, a human message, an optional
/// upstream HTTP status, and the delivery truth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("model error [{kind:?}]: {message}")]
pub struct ModelError {
    pub kind: ModelErrorKind,
    pub message: String,
    pub status: Option<u16>,
    /// Provider-specific diagnostic facts (vendor code, correlation id, model).
    /// Boxed because `ModelError` is the `Err` of many `Result`s and every
    /// extra inline field is paid on the happy path too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Box<ProviderDiagnostics>>,
    /// Provider-advertised wait (`Retry-After`) in milliseconds. Retry layers
    /// must prefer this over their own backoff schedule when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// The provider transport already spent its own retry budget on this
    /// request-start failure. Outer layers may still retry a `Safe` failure,
    /// but must use a slower, smaller budget (R006 R6-P3: this used to be
    /// encoded by destroying retryability, which silently made every
    /// request-start timeout terminal for the whole goal).
    #[serde(default)]
    pub provider_retries_exhausted: bool,
    /// What the transport could prove about delivery. Defaults to
    /// [`DeliveryState::Unknown`] so a construction site that never decided
    /// cannot accidentally claim a safer state than it has evidence for.
    #[serde(default)]
    pub delivery_state: DeliveryState,
    /// The provider this request was addressed to, when the failure came from a
    /// provider call. Diagnostic and presentational (so a client can say
    /// "Moonshot"); never an authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

impl ModelError {
    /// Build an error with unknown delivery. Call sites that know more should
    /// set [`Self::delivery_state`] (or use [`Self::with_delivery_state`]).
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            diagnostics: None,
            retry_after_ms: None,
            provider_retries_exhausted: false,
            delivery_state: DeliveryState::Unknown,
            provider: None,
        }
    }

    /// The provider's own error code/type, when its response carried one.
    pub fn with_provider_code(mut self, code: impl Into<String>) -> Self {
        let code = code.into();
        if !code.trim().is_empty() {
            self.diagnostics_mut().code = Some(sanitize_provider_text(&code));
        }
        self
    }

    /// The provider's correlation id for this request, when one was exposed.
    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        let id = id.into();
        if !id.trim().is_empty() {
            self.diagnostics_mut().request_id = Some(sanitize_provider_text(&id));
        }
        self
    }

    /// The model id this request was addressed to.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        let model = model.into();
        if !model.trim().is_empty() {
            self.diagnostics_mut().model = Some(model);
        }
        self
    }

    /// The provider's own error code/type, when it reported one.
    pub fn provider_code(&self) -> Option<&str> {
        self.diagnostics.as_deref()?.code.as_deref()
    }

    /// The provider's correlation id for the failing request, when known.
    pub fn request_id(&self) -> Option<&str> {
        self.diagnostics.as_deref()?.request_id.as_deref()
    }

    /// The model id this request was addressed to, when known.
    pub fn model(&self) -> Option<&str> {
        self.diagnostics.as_deref()?.model.as_deref()
    }

    fn diagnostics_mut(&mut self) -> &mut ProviderDiagnostics {
        self.diagnostics
            .get_or_insert_with(|| Box::new(ProviderDiagnostics::default()))
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_retry_after_ms(mut self, ms: u64) -> Self {
        self.retry_after_ms = Some(ms);
        self
    }

    pub fn with_delivery_state(mut self, delivery_state: DeliveryState) -> Self {
        self.delivery_state = delivery_state;
        self
    }

    /// Stamp the provider this request was addressed to. Set where the
    /// provider is actually known (the registry), so downstream presentation
    /// can name it without re-deriving it from the raw message.
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    pub fn cancelled() -> Self {
        Self::new(ModelErrorKind::Cancelled, "request cancelled")
    }

    /// Whether a failed attempt may be retried, from the kind and the delivery
    /// truth. This is THE retry policy: recovery layers must not consult
    /// `message`, and must not retry anything this returns as anything other
    /// than [`Retryability::Safe`].
    pub fn retryability(&self) -> Retryability {
        use ModelErrorKind as K;
        // Re-sending the identical request cannot help, whatever the delivery
        // truth says: the provider will reject it the same way.
        match self.kind {
            K::Auth
            | K::InvalidRequest
            | K::Decode
            | K::Truncated
            | K::ContentFiltered
            | K::Cancelled
            | K::Other => return Retryability::Never,
            K::RateLimit
            | K::ProviderUnavailable
            | K::Transport
            | K::StreamInterrupted
            | K::Timeout => {}
        }
        match self.delivery_state {
            // The request never left: nothing to duplicate.
            DeliveryState::NotSent => Retryability::Safe,
            DeliveryState::Responded => match self.status {
                // An explicit "come back later".
                Some(429) => Retryability::Safe,
                // The upstream may have started before the gateway gave up.
                Some(504) => Retryability::Caution,
                // Gateway/upstream refused before the model ran.
                Some(500..=599) => Retryability::Safe,
                // Any other status is not a transient delivery problem.
                _ => Retryability::Never,
            },
            // The stream began but produced nothing: safe to re-request.
            DeliveryState::StreamInterrupted { progress } if !progress.any() => Retryability::Safe,
            // The model already produced output; a re-send would duplicate it.
            DeliveryState::StreamInterrupted { .. } => Retryability::Caution,
            // Written, but no response: whether the provider processed it is
            // unknown, so this is never automatic.
            DeliveryState::SentNoResponse => Retryability::Unknown,
            DeliveryState::Unknown => Retryability::Unknown,
        }
    }

    /// Whether this failure is worth a bounded automatic retry. Convenience
    /// over [`Self::retryability`]; the single definition stays there.
    pub fn is_safe_to_retry(&self) -> bool {
        self.retryability() == Retryability::Safe
    }

    /// Map an HTTP status code to a normalized error kind. A complete status
    /// means the request was delivered to the provider's gateway.
    pub fn from_status(status: u16, message: impl Into<String>) -> Self {
        let kind = match status {
            401 | 403 => ModelErrorKind::Auth,
            400 | 404 | 422 => ModelErrorKind::InvalidRequest,
            429 => ModelErrorKind::RateLimit,
            500..=599 => ModelErrorKind::ProviderUnavailable,
            _ => ModelErrorKind::Other,
        };
        // A provider HTTP body is untrusted external content: sanitize it at
        // this one boundary so every downstream store/render is safe.
        Self::new(kind, sanitize_provider_text(&message.into()))
            .with_status(status)
            .with_delivery_state(DeliveryState::Responded)
    }
}

/// Provider-specific diagnostic facts attached to a [`ModelError`]. Never read
/// by retry policy; the single source a client projects into a failure report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDiagnostics {
    /// The provider's structured error code or type (OpenAI-compatible
    /// `error.code` / `error.type`, Anthropic `error.type`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// The provider's correlation id for the failing request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The model id the request was addressed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Secret-shaped prefixes that must never leave the provider boundary inside a
/// diagnostic string. Mirrors the MCP env-name check's vocabulary so one set of
/// shapes is redacted everywhere.
const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "xoxe-",
    "aiza",
    "bearer",
];

/// The one boundary provider-originated diagnostic text crosses before it can
/// be stored or shown: secret-shaped tokens are redacted and the result is
/// bounded. A provider error body is untrusted external content, so nothing
/// downstream may assume it is safe to echo verbatim.
pub fn sanitize_provider_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_PROVIDER_TEXT_BYTES + 8));
    for (i, token) in text.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let lower = token.to_ascii_lowercase();
        let trimmed = lower
            .trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ':' | '{' | '}' | '[' | ']'));
        if SECRET_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
            out.push_str("[redacted]");
        } else {
            out.push_str(token);
        }
    }
    leveler_core::truncate_head_bytes(&out, MAX_PROVIDER_TEXT_BYTES, "…")
}

/// Cap on the diagnostic text carried from a provider error. Long enough for a
/// real provider explanation, short enough that a hostile body cannot bloat a
/// transcript row.
const MAX_PROVIDER_TEXT_BYTES: usize = 600;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_maps_to_kind() {
        assert_eq!(
            ModelError::from_status(429, "slow down").kind,
            ModelErrorKind::RateLimit
        );
        assert_eq!(
            ModelError::from_status(401, "nope").kind,
            ModelErrorKind::Auth
        );
        assert_eq!(
            ModelError::from_status(503, "down").kind,
            ModelErrorKind::ProviderUnavailable
        );
    }

    #[test]
    fn a_status_response_is_evidence_of_delivery() {
        let err = ModelError::from_status(503, "down");
        assert_eq!(err.delivery_state, DeliveryState::Responded);
    }

    /// The diagnostic fields ride along with the error but never change the
    /// machine semantics: a 400 is still never retryable whatever code the
    /// provider named.
    #[test]
    fn provider_code_request_id_and_model_are_carried_without_touching_policy() {
        let err = ModelError::from_status(400, "bad request")
            .with_provider_code("invalid_request_error")
            .with_request_id("req_abc")
            .with_model("k3")
            .with_provider("kimi");
        assert_eq!(err.status, Some(400));
        assert_eq!(err.provider_code(), Some("invalid_request_error"));
        assert_eq!(err.request_id(), Some("req_abc"));
        assert_eq!(err.model(), Some("k3"));
        assert_eq!(err.retryability(), Retryability::Never);
        // Empty values are absent, not stored as empty strings.
        let blank = ModelError::new(ModelErrorKind::Other, "x")
            .with_provider_code("  ")
            .with_request_id("")
            .with_model("");
        assert!(blank.provider_code().is_none());
        assert!(blank.request_id().is_none());
        assert!(blank.model().is_none());
        assert!(blank.diagnostics.is_none(), "no box without a fact");
    }

    /// A provider body is untrusted external text: secret-shaped tokens are
    /// redacted and the result is bounded before it can reach a store or a UI.
    #[test]
    fn provider_text_is_sanitized_and_bounded() {
        let err = ModelError::from_status(401, "bad key: sk-supersecret-value Bearer abc123");
        assert!(
            !err.message.contains("sk-supersecret-value"),
            "{}",
            err.message
        );
        assert!(err.message.contains("[redacted]"), "{}", err.message);
        let huge = "x".repeat(5_000);
        let err = ModelError::from_status(500, &huge);
        assert!(
            err.message.len() <= 600 + "…".len(),
            "bounded: {}",
            err.message.len()
        );
    }

    /// The core of the delivery-truth contract: only a request that provably
    /// did no provider-side work (or one the provider asked to retry) is
    /// auto-retryable. Everything ambiguous is not.
    #[test]
    fn retryability_follows_delivery_truth_not_kind_alone() {
        // Never sent: safe.
        assert_eq!(
            ModelError::new(ModelErrorKind::Transport, "connect refused")
                .with_delivery_state(DeliveryState::NotSent)
                .retryability(),
            Retryability::Safe
        );
        // Written, no response: unknown, never automatic.
        assert_eq!(
            ModelError::new(ModelErrorKind::Timeout, "read timed out")
                .with_delivery_state(DeliveryState::SentNoResponse)
                .retryability(),
            Retryability::Unknown
        );
        // Stream cut before any output: safe to re-request.
        assert_eq!(
            ModelError::new(ModelErrorKind::StreamInterrupted, "cut")
                .with_delivery_state(DeliveryState::StreamInterrupted {
                    progress: StreamProgress::default(),
                })
                .retryability(),
            Retryability::Safe
        );
        // Stream cut after output: the model already generated; never automatic.
        assert_eq!(
            ModelError::new(ModelErrorKind::StreamInterrupted, "cut")
                .with_delivery_state(DeliveryState::StreamInterrupted {
                    progress: StreamProgress {
                        text: true,
                        tool_args: false,
                    },
                })
                .retryability(),
            Retryability::Caution
        );
        // An unfinished tool call is equally not safely replayable.
        assert_eq!(
            ModelError::new(ModelErrorKind::StreamInterrupted, "cut")
                .with_delivery_state(DeliveryState::StreamInterrupted {
                    progress: StreamProgress {
                        text: false,
                        tool_args: true,
                    },
                })
                .retryability(),
            Retryability::Caution
        );
    }

    #[test]
    fn gateway_statuses_are_not_all_equally_safe() {
        // A rate limit is an explicit instruction to come back.
        assert_eq!(
            ModelError::from_status(429, "slow").retryability(),
            Retryability::Safe
        );
        // 502/503: the gateway refused before the model ran.
        assert_eq!(
            ModelError::from_status(503, "down").retryability(),
            Retryability::Safe
        );
        // 504: the upstream may have started; not a safe replay.
        assert_eq!(
            ModelError::from_status(504, "gateway timeout").retryability(),
            Retryability::Caution
        );
    }

    #[test]
    fn terminal_kinds_are_never_retried() {
        for kind in [
            ModelErrorKind::Auth,
            ModelErrorKind::InvalidRequest,
            ModelErrorKind::Decode,
            ModelErrorKind::Truncated,
            ModelErrorKind::ContentFiltered,
            ModelErrorKind::Cancelled,
        ] {
            assert_eq!(
                ModelError::new(kind, "x")
                    .with_delivery_state(DeliveryState::NotSent)
                    .retryability(),
                Retryability::Never,
                "{kind:?} must never be retried"
            );
        }
    }

    /// Legacy errors (deserialized without the field) must not be treated as
    /// safer than they are.
    #[test]
    fn missing_delivery_state_deserializes_to_unknown() {
        let legacy = r#"{"kind":"transport","message":"boom","status":null,"retryable":true,
            "provider_retries_exhausted":false}"#;
        let err: ModelError = serde_json::from_str(legacy).unwrap();
        assert_eq!(err.delivery_state, DeliveryState::Unknown);
        assert_eq!(err.retryability(), Retryability::Unknown);
    }
}
