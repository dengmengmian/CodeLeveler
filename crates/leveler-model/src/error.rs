//! Unified, normalized model errors (spec §6.6, §31 model-error slice).
//!
//! Every provider failure must be mapped onto one of these kinds so recovery
//! logic never has to string-match vendor messages.

use serde::{Deserialize, Serialize};

/// A coarse classification of what went wrong talking to a model. `retryable`
/// is derived from this via [`ModelErrorKind::is_retryable`].
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

impl ModelErrorKind {
    /// Whether an error of this kind is worth retrying by default. The policy
    /// layer may still override per provider.
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            ModelErrorKind::RateLimit
                | ModelErrorKind::ProviderUnavailable
                | ModelErrorKind::Transport
                | ModelErrorKind::StreamInterrupted
                | ModelErrorKind::Timeout
        )
    }
}

/// A normalized model error carrying its kind, a human message, and an optional
/// upstream HTTP status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("model error [{kind:?}]: {message}")]
pub struct ModelError {
    pub kind: ModelErrorKind,
    pub message: String,
    pub status: Option<u16>,
    pub retryable: bool,
}

impl ModelError {
    /// Build an error, defaulting `retryable` from the kind.
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            retryable: kind.is_retryable(),
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn cancelled() -> Self {
        Self::new(ModelErrorKind::Cancelled, "request cancelled")
    }

    /// Map an HTTP status code to a normalized error kind.
    pub fn from_status(status: u16, message: impl Into<String>) -> Self {
        let kind = match status {
            401 | 403 => ModelErrorKind::Auth,
            400 | 404 | 422 => ModelErrorKind::InvalidRequest,
            429 => ModelErrorKind::RateLimit,
            500..=599 => ModelErrorKind::ProviderUnavailable,
            _ => ModelErrorKind::Other,
        };
        Self::new(kind, message).with_status(status)
    }
}

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
    fn retryable_derived_from_kind() {
        assert!(ModelError::new(ModelErrorKind::RateLimit, "x").retryable);
        assert!(!ModelError::new(ModelErrorKind::Auth, "x").retryable);
    }
}
