//! `/trace` screen: protocol observability projection. TUI never opens SQLite.

mod model;
mod render;

pub use model::{TraceTab, TraceView};
pub use render::render_trace_screen;

use leveler_client_protocol::{ClientCommand, SessionId};

/// Issue a QueryObservability the TUI owns. Later [`RuntimeEvent::ObservabilityLoaded`]
/// is applied only when `query_id` matches.
pub fn issue_query(
    trace: &mut TraceView,
    session_id: SessionId,
    center_seq: Option<i64>,
    before: u32,
    after: u32,
) -> ClientCommand {
    let query_id = trace.track.issue();
    trace.last_query_at = Some(std::time::Instant::now());
    ClientCommand::QueryObservability {
        session_id,
        query_id: Some(query_id),
        center_seq,
        before,
        after,
    }
}

/// How long an open `/trace` may go without asking again while a turn runs.
///
/// Not every durable row produces a client event: a single long tool appends a
/// `command_progress` row on its own cadence, so a page that refreshed only on
/// fact events would keep the rows it had when the tool started until the tool
/// finished — minutes for a build or a test suite. The cadence is bounded and
/// the query is single-flight (`QueryTrack`), so this can never stack reads.
pub const LIVE_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Events that should refresh an open `/trace` from durable query (not deltas).
pub fn should_refresh_trace(event: &leveler_client_protocol::RuntimeEvent) -> bool {
    use leveler_client_protocol::RuntimeEvent::*;
    matches!(
        event,
        ToolCallStarted { .. }
            | ToolCallCompleted { .. }
            | TokenUsage { .. }
            | SubAgentUpdated { .. }
            | TurnCompleted
            | TurnAnswered
            | TurnFailed { .. }
            | TurnIncomplete { .. }
            | TurnCancelled
            | ContextCompacted { .. }
            | CheckpointCreated { .. }
            | BackgroundTaskStarted { .. }
            | BackgroundTaskExited { .. }
    )
}
