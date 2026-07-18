//! Orchestrator errors.

use leveler_agent::AgentError;
use leveler_model::ModelError;

/// Errors from driving the agent state machine.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("model error: {0}")]
    Model(#[from] ModelError),
    #[error("agent error: {0}")]
    Agent(#[from] AgentError),
    #[error("structured output error: {0}")]
    Json(String),
    #[error("invalid task graph: {0}")]
    InvalidPlan(String),
    #[error("cancelled")]
    Cancelled,
}
