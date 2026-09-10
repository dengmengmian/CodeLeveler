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

use leveler_agent::PriorlyAdmitted;
use leveler_tools::{ToolContext, ToolRegistry};

/// Re-run a dangling call through the host's reconciliation entry.
///
/// The engine no longer executes tools itself: it hands a
/// [`PriorlyAdmitted`] call to `leveler_agent::reconcile`, so the whole
/// system has ONE place that runs a tool, not one per crate. Returns
/// `(is_error, preview)`; `None` when the tool refuses reconstruction
/// (unknown to this build, or never declared replay-safe), which the caller
/// must treat as "stop for human reconciliation", never as a skip.
pub(crate) async fn replay_tool(
    registry: &ToolRegistry,
    context: ToolContext,
    name: &str,
    args: serde_json::Value,
    cancellation: &CancellationToken,
) -> Option<(bool, String)> {
    let admitted = PriorlyAdmitted::from_persisted(registry, name, args)?;
    let (is_error, output) =
        leveler_agent::reconcile(registry, context, &admitted, cancellation).await;
    Some((is_error, preview(&output)))
}

/// Bound a replayed tool's output for the event-log preview (the full result
/// is not needed — the model re-drives from the clean turn boundary).
pub(crate) fn preview(text: &str) -> String {
    text.chars().take(200).collect()
}
