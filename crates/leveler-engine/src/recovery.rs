//! The host's reconciliation entry: the ONE place the engine executes a tool.
//!
//! Recovery is the single case where the engine runs a tool itself rather than
//! through the agent's ToolHost — it is replaying a call whose original
//! admission already happened and is on record. Keeping it in one named file
//! makes that exception auditable instead of scattered, and
//! `leveler-agent/tests/tool_host_boundary.rs` fails the build if
//! `registry.execute` appears anywhere else in this crate.
//!
//! Only a call the tool itself declares replay-safe reaches here (see
//! [`crate::engine`]'s replay gate). Everything else stops for human
//! reconciliation rather than guessing.

use tokio_util::sync::CancellationToken;

use leveler_tools::{ToolContext, ToolRegistry};

/// Re-run a dangling call. Returns `(is_error, preview)`; a failure is a
/// recorded errored result, never a fake success and never a hard stop —
/// recovery reports what happened and lets the model re-drive.
pub(crate) async fn replay_tool(
    registry: &ToolRegistry,
    context: ToolContext,
    name: &str,
    args: serde_json::Value,
    cancellation: &CancellationToken,
) -> (bool, String) {
    match registry
        .execute(name, args, context, cancellation.child_token())
        .await
    {
        Ok(output) => (output.is_error, preview(&output.content)),
        Err(error) => (true, preview(&error.to_string())),
    }
}

/// Bound a replayed tool's output for the event-log preview (the full result
/// is not needed — the model re-drives from the clean turn boundary).
pub(crate) fn preview(text: &str) -> String {
    text.chars().take(200).collect()
}
