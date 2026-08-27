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

/// One durable goal checkpoint, projected for history presentation (long-goal
/// P3). Every Recap a client renders maps to exactly one persisted checkpoint
/// (`checkpoint_id`) — the client presents these fields, it never rebuilds its
/// own summary, and it never parses `display_summary` to reconstruct facts.
///
/// Truth rules ride the shape: `findings_total == None` means the ledger was
/// not readable when the checkpoint was cut — UNKNOWN, which a client must
/// never render as zero. `verification` is `"unmeasured"` when nothing was
/// proven — never a pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiGoalRecap {
    /// The persisted `GoalCheckpointId` this Recap presents.
    pub checkpoint_id: String,
    pub goal_id: String,
    /// `manual` | `milestone` | `context_compaction` | `interrupted`.
    pub reason: String,
    /// RFC3339. When the checkpoint was cut.
    pub created_at: String,
    /// Transcript position the checkpoint represents (messages `[0..n)`), so
    /// a reopened session can interleave the Recap where it happened.
    /// `None` = unknown; append after existing history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_ordinal: Option<u64>,
    /// The 1–2 line presentation. Runtime-rendered: the semantic summary when
    /// one exists, otherwise the deterministic structured fallback.
    pub display_summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    /// Plan progress; both `None` when no plan was recorded (absent, not 0/0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_completed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_total: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_milestones: Vec<String>,
    /// `passed` | `failed` | `unmeasured`.
    pub verification: String,
    /// Evidence for a pass, or the failure detail. Absent when unmeasured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_detail: Option<String>,
    /// `None` = UNKNOWN (ledger unreadable) — never render as 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings_total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings_open: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings_blocking: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_limitations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_work: Vec<String>,
}
