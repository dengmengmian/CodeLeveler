//! Validation failures raised by the pure candidate and experience rules.

/// A candidate or experience record cannot be stored as proposed.
///
/// Every variant is a refusal of the payload itself, not a storage failure:
/// the stores report their own errors.
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("invalid memory: {0}")]
    Invalid(String),
}
