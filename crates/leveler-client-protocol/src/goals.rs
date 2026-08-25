//! Read model for unfinished-goal visibility (long-goal P2).
//!
//! Deliberately carries no action. There is no resume command in this protocol
//! and no field a client could use to start one, because no resume policy
//! exists yet — and a UI that offers a button the runtime cannot honestly
//! back is worse than one that says only what is true.

use serde::{Deserialize, Serialize};

/// One goal that still owes work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiUnfinishedGoal {
    pub goal_id: String,
    /// What the user asked, verbatim.
    pub objective: String,
    /// The conversation it ran in, so the user can go read it.
    pub session_id: String,
    /// RFC3339. When the goal was opened.
    pub opened_at: String,
    /// Work windows it consumed before stopping.
    pub windows_run: u32,
    /// Whether this runtime may act on it. `false` means another runtime holds
    /// the task; the goal is still listed, because omitting work that exists
    /// is worse than naming work nobody here can act on.
    pub ours: bool,
    /// A turn is still running for it — it is being driven right now, and is
    /// therefore not unfinished work.
    pub driving: bool,
}

impl UiUnfinishedGoal {
    /// Owed, actionable here, and nobody doing it. The only claim a UI may
    /// make about a goal — and it is never "failed" or "interrupted", which
    /// are verdicts nothing has issued.
    pub fn needs_attention(&self) -> bool {
        self.ours && !self.driving
    }
}
