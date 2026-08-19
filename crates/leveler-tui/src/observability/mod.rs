//! `/trace` screen: protocol observability projection. TUI never opens SQLite.

mod model;
mod render;

pub use model::{TraceTab, TraceView};
pub use render::render_trace_screen;

/// Events that should refresh an open `/trace` from durable query (not deltas).
pub fn should_refresh_trace(event: &leveler_client_protocol::RuntimeEvent) -> bool {
    use leveler_client_protocol::RuntimeEvent::*;
    matches!(
        event,
        ToolCallStarted { .. }
            | ToolCallCompleted { .. }
            | TokenUsage { .. }
            | VerificationUpdated { .. }
            | SubAgentUpdated { .. }
            | TurnCompleted
            | TurnAnswered
            | TurnFailed { .. }
            | TurnIncomplete { .. }
            | TurnCompletedUnverified { .. }
            | TurnCancelled
            | ContextCompacted { .. }
            | CheckpointCreated { .. }
            | BackgroundTaskStarted { .. }
            | BackgroundTaskExited { .. }
    )
}
