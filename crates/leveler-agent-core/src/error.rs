//! Errors that abort the kernel loop. Tool failures are never errors here —
//! they are results the model reads.

use leveler_model::ModelError;

use crate::tool_runtime::ToolRuntimeError;

/// Why the loop stopped without a stop reason.
#[derive(Debug, thiserror::Error)]
pub enum AgentCoreError {
    /// The model failed in a way the loop could not recover from.
    // `ModelError` already renders as "model error [Kind]: …".
    #[error("{0}")]
    Model(#[from] ModelError),
    /// The run was cancelled from outside.
    #[error("cancelled")]
    Cancelled,
    /// The tool runtime itself failed (not a tool returning an error result).
    #[error("tool runtime error: {0}")]
    ToolRuntime(#[from] ToolRuntimeError),
    /// The limits handed to the loop cannot be enforced as given.
    #[error("invalid limits: {0}")]
    InvalidLimits(String),
}
