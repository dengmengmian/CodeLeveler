//! `/trace` screen: protocol observability projection. TUI never opens SQLite.

mod model;
mod render;

pub use model::{TraceTab, TraceView};
pub use render::render_trace_screen;

use leveler_client_protocol::{ClientCommand, CommandId, SessionId};

/// Issue a QueryObservability the TUI owns. Later [`RuntimeEvent::ObservabilityLoaded`]
/// is applied only when `query_id` matches.
pub fn issue_query(
    trace: &mut TraceView,
    session_id: SessionId,
    center_seq: Option<i64>,
    before: u32,
    after: u32,
) -> ClientCommand {
    let query_id = CommandId::generate();
    trace.pending_query_id = Some(query_id.clone());
    ClientCommand::QueryObservability {
        session_id,
        query_id: Some(query_id),
        center_seq,
        before,
        after,
    }
}

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
