//! Generic **runtime lifecycle** vocabulary — the states and verdicts any
//! domain (Coding today; others later) can understand.
//!
//! This module must stay free of Coding workflow concepts: it does not (and
//! must not) reference [`crate::workflow`]. A runtime consumer — engine
//! lifecycle writes, storage projections, client status displays — can depend
//! on these types without importing Coding phase semantics. The reverse edge
//! (workflow refining runtime states) is allowed; this direction is not.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A persisted enum value that does not match any known variant. Storage maps
/// this to its `InvalidData` corruption error; the engine to `Corrupt`. Never
/// guess a default — an unknown persisted value is a hard, named error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} value `{value}`")]
pub struct UnknownVariant {
    pub kind: &'static str,
    pub value: String,
}

/// A session's operational position in its lifecycle — *not* its terminal
/// verdict (that is [`TaskOutcome`], persisted separately). Kept coarse: the
/// authoritative "how did it end" lives in the outcome column.
///
/// This is the runtime lifecycle axis: created / running / blocked /
/// interrupted / terminal. It carries no Coding phase information — that
/// lives in [`crate::workflow::AgentState`] as a separate breadcrumb column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Persisted, not yet started.
    Created,
    /// A turn is actively executing.
    Running,
    /// The run concluded normally (see the outcome column for the verdict).
    Completed,
    /// The model stopped without finishing the work (budget/stall/audit).
    Incomplete,
    /// Goal mode declared the task blocked.
    Blocked,
    /// Cancelled or crashed; resumable.
    Interrupted,
    /// The user explicitly cancelled the logical task. Terminal: unlike
    /// [`SessionStatus::Interrupted`] this is not resumable, because "stop this
    /// task" was the instruction, not "pause it".
    Cancelled,
    /// The run errored before producing a verdict.
    Failed,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Created => "created",
            SessionStatus::Running => "running",
            SessionStatus::Completed => "completed",
            SessionStatus::Incomplete => "incomplete",
            SessionStatus::Blocked => "blocked",
            SessionStatus::Interrupted => "interrupted",
            SessionStatus::Cancelled => "cancelled",
            SessionStatus::Failed => "failed",
        }
    }
}

impl FromStr for SessionStatus {
    type Err = UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "created" => SessionStatus::Created,
            "running" => SessionStatus::Running,
            "completed" => SessionStatus::Completed,
            "incomplete" => SessionStatus::Incomplete,
            "blocked" => SessionStatus::Blocked,
            "interrupted" => SessionStatus::Interrupted,
            "cancelled" | "canceled" => SessionStatus::Cancelled,
            "failed" => SessionStatus::Failed,
            other => {
                return Err(UnknownVariant {
                    kind: "session status",
                    value: other.to_string(),
                });
            }
        })
    }
}

/// A task's terminal status: how the run ENDED. It says nothing about whether
/// the project's checks passed — that is the orthogonal [`VerificationStatus`]
/// — and nothing about whether the user's intent was met, which no runtime
/// can mechanically establish.
///
/// The wire values `verified` and `completed_unverified` were written by
/// older runtimes for what is now `Completed`; they are accepted on read as a
/// legacy alias and never written again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    /// The model declared the goal complete (or a conversational turn ended
    /// normally). Look at [`VerificationStatus`] for the project's checks.
    #[serde(alias = "verified", alias = "completed_unverified")]
    Completed,
    /// The model declared the goal unreachable as stated.
    Blocked,
    /// Execution stopped at an explicit resource boundary. The task is
    /// incomplete and resumable; this is not evidence of model failure.
    BudgetLimited,
    /// The run ended in failure. Unlike [`TaskOutcome::Blocked`], the model did
    /// not declare the goal unreachable; the task simply did not succeed.
    Failed,
    /// The run was interrupted, for example by an abnormal process exit or
    /// user cancellation. Like [`TaskOutcome::Failed`], this is not the model
    /// declaring the goal unreachable — that is [`TaskOutcome::Blocked`].
    Interrupted,
    /// The user explicitly cancelled the logical task. Unlike
    /// [`TaskOutcome::Interrupted`] this is terminal: it is not resumable, and
    /// a continuation must not silently reopen the work.
    Cancelled,
}

impl TaskOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskOutcome::Completed => "completed",
            TaskOutcome::Blocked => "blocked",
            TaskOutcome::BudgetLimited => "budget_limited",
            TaskOutcome::Failed => "failed",
            TaskOutcome::Interrupted => "interrupted",
            TaskOutcome::Cancelled => "cancelled",
        }
    }

    /// Whether the run reached its declared end. Automation that also needs
    /// the project's checks green must look at [`VerificationStatus`] too.
    pub fn is_completed(self) -> bool {
        self == Self::Completed
    }
}

impl FromStr for TaskOutcome {
    type Err = UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "completed" => TaskOutcome::Completed,
            // Legacy rows written before the status/verification split.
            "verified" | "completed_unverified" => TaskOutcome::Completed,
            "blocked" => TaskOutcome::Blocked,
            "budget_limited" => TaskOutcome::BudgetLimited,
            "failed" => TaskOutcome::Failed,
            "interrupted" => TaskOutcome::Interrupted,
            "cancelled" | "canceled" => TaskOutcome::Cancelled,
            other => {
                return Err(UnknownVariant {
                    kind: "task outcome",
                    value: other.to_string(),
                });
            }
        })
    }
}

/// What the project's own mechanical checks said about the tree at the end of
/// the task. Orthogonal to [`TaskOutcome`]: a task can be `Completed` with
/// checks `Failed`, and the runtime reports both rather than folding them
/// into one word.
///
/// `Passed` means the configured build/test/lint commands exited 0 over the
/// edited tree. It does not mean the user's request was satisfied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// Every gating check passed over the final tree.
    Passed,
    /// At least one gating check failed over the final tree.
    Failed,
    /// No check ran: nothing was modified, no checks are configured, or the
    /// run ended before the terminal boundary.
    #[default]
    NotRun,
    /// Checks were configured but could not produce a verdict (tool missing,
    /// environment unavailable, timeout).
    Unavailable,
}

impl VerificationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            VerificationStatus::Passed => "passed",
            VerificationStatus::Failed => "failed",
            VerificationStatus::NotRun => "not_run",
            VerificationStatus::Unavailable => "unavailable",
        }
    }
}

impl FromStr for VerificationStatus {
    type Err = UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "passed" => VerificationStatus::Passed,
            "failed" => VerificationStatus::Failed,
            "not_run" => VerificationStatus::NotRun,
            "unavailable" => VerificationStatus::Unavailable,
            other => {
                return Err(UnknownVariant {
                    kind: "verification status",
                    value: other.to_string(),
                });
            }
        })
    }
}

/// A turn's terminal execution status. This is distinct from [`TaskOutcome`]:
/// a turn may complete normally while the task later fails verification.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    /// Kept as the serde default for legacy `TurnFinished` events that predate
    /// the explicit terminal-status field.
    #[default]
    Completed,
    Failed,
    Interrupted,
}

impl TurnOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnOutcome::Completed => "completed",
            TurnOutcome::Failed => "failed",
            TurnOutcome::Interrupted => "interrupted",
        }
    }
}

/// Which bound stopped a child whose stop is [`ChildStop::Budget`]. Carried
/// beside the stop so a wall-clock cap, a token or cost budget and a round
/// limit are told apart without reading the settlement prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildLimit {
    Duration,
    ModelTokens,
    Cost,
    Commands,
    ModifiedFiles,
    /// The child's own round window.
    RoundWindow,
    /// The absolute round ceiling.
    RoundCeiling,
}

/// How one delegated child's activation ended, mechanically. Carried on the
/// child's terminal event so no reader has to recover it from prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStop {
    /// Ran to its own clean end.
    Completed,
    /// Ended on its own without finishing (blocked, stalled, refused).
    Incomplete,
    /// A wall, round, token or cost bound stopped it.
    Budget,
    /// Its parent, its task or a user cancelled it.
    Cancelled,
    /// Its run broke (provider, persistence, panic).
    Failed,
    /// Its activation died with a runtime window and it was not continued.
    Lost,
}

impl ChildStop {
    pub fn as_str(self) -> &'static str {
        match self {
            ChildStop::Completed => "completed",
            ChildStop::Incomplete => "incomplete",
            ChildStop::Budget => "budget",
            ChildStop::Cancelled => "cancelled",
            ChildStop::Failed => "failed",
            ChildStop::Lost => "lost",
        }
    }
}

/// What re-creates a delegated child's activation, plus the spawn-time
/// identity fixed with it: everything its spawn determined that is not in its
/// own transcript. Recorded on the child's durable start so a later window can
/// continue the same child instead of guessing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildSpawnSpec {
    /// The child's short task title, fixed at spawn: a stable, scannable name
    /// for the delegated task, distinct from the full `task` instructions the
    /// child runs on. Persisted with the spawn record so replay and resume
    /// reconstruct the same identity. `None` on children recorded before
    /// titles existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The exclusive write scope fixed at spawn (empty for late-bound or
    /// read-only children).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// A pinned `provider/model`, when the child does not run on its parent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A tool subset the child was restricted to (empty = its role's set).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// A round cap the child was given (0 = its role's default).
    #[serde(default)]
    pub max_rounds: u32,
    /// Whether the parent continued while the child ran.
    #[serde(default)]
    pub background: bool,
    /// The declarative agent definition this child was spawned from, as
    /// resolved at spawn. A restarted child continues under this snapshot and
    /// never re-reads the definition's files: editing or deleting the agent
    /// affects future spawns only. `None` for built-in role spawns and for
    /// children recorded before agent definitions existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<Box<ChildAgentSnapshot>>,
}

/// The spawn-time identity and bounds of a declarative agent. The rest of the
/// resolved definition travels in [`ChildSpawnSpec`] (`model`, `tools`,
/// `max_rounds`) and in the child's own transcript (its instructions and bound
/// skills, which are part of its first system message).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildAgentSnapshot {
    /// The agent's name, e.g. `security-reviewer`.
    pub name: String,
    /// `project`, `user` or `builtin`.
    pub source: String,
    /// `sha256:<hex>` of the resolved definition.
    pub fingerprint: String,
    /// `read_only`, `writer` or `scoped_writer`.
    pub capability: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// The most this child may ever claim, repository-relative.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_secs: Option<u64>,
}

/// Why the loop stopped. Serialized (snake_case) into terminal engine events
/// so blocked/budget/complete stay machine-discriminable after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The task has explicit completion evidence, rather than merely a natural
    /// end to one model response.
    Completed,
    /// The model naturally ended its answer. This closes the conversational
    /// turn but does not prove that an external task is complete.
    ///
    /// `closeout_forced` is a legacy wire value from the deleted closeout
    /// watchdog; it read as "the plan was done, the turn ended abnormally"
    /// and decodes here.
    #[serde(alias = "closeout_forced")]
    Answered,
    /// The run ended without finishing: every attempted action was refused
    /// for several rounds in a row.
    Incomplete,
    /// A token or cost budget was exhausted first.
    BudgetExhausted,
    /// The absolute per-turn round ceiling was hit. This is the unconditional
    /// circuit breaker that fires even when every progress watchdog was evaded
    /// (a "busy" loop that fakes progress each round). It guarantees termination
    /// but is not a budget the user can lift by saying "继续" — so it is kept
    /// distinct from `BudgetExhausted` for honest logs/telemetry. Maps to the
    /// same session outcome as `BudgetExhausted` (Incomplete / Execute).
    TurnLimitReached,
    /// Goal mode: the model declared the goal unreachable via `update_goal(blocked)`.
    ///
    /// `policy_blocked` is a legacy wire value from the deleted plan-gate
    /// escalation track and decodes here.
    #[serde(alias = "policy_blocked")]
    Blocked,
    /// Goal mode: the model went quiet without ever resolving the goal via
    /// `update_goal`, even after the quiet-nudge cap. Not a success.
    Stalled,
    /// The run finished its work, but the project's checks did not run or
    /// could not produce a verdict. Synthesized by the app's verification
    /// mapping — the token loop never emits this. It means "done, checks not
    /// run", NOT "failed" or "gave up".
    CompletedUnverified,
    /// The run finished its work and the project's checks then FAILED over
    /// the final tree. Synthesized by the app's verification mapping — the
    /// token loop never emits this. Both facts are reported; neither is
    /// laundered into the other.
    CompletedChecksFailed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_completed_is_a_declared_end() {
        assert!(TaskOutcome::Completed.is_completed());
        assert!(!TaskOutcome::Blocked.is_completed());
        assert!(!TaskOutcome::BudgetLimited.is_completed());
        assert!(!TaskOutcome::Failed.is_completed());
        assert!(!TaskOutcome::Interrupted.is_completed());
    }

    /// Rows and events written before the status/verification split carry
    /// `verified` / `completed_unverified`. Both were "the run ended as
    /// declared" with a verification verdict folded in; they read back as
    /// `Completed` and are never written again.
    #[test]
    fn legacy_outcome_strings_read_as_completed() {
        assert_eq!(
            TaskOutcome::from_str("verified").unwrap(),
            TaskOutcome::Completed
        );
        assert_eq!(
            TaskOutcome::from_str("completed_unverified").unwrap(),
            TaskOutcome::Completed
        );
        assert_eq!(
            serde_json::from_str::<TaskOutcome>("\"verified\"").unwrap(),
            TaskOutcome::Completed
        );
        assert_eq!(TaskOutcome::Completed.as_str(), "completed");
    }

    #[test]
    fn verification_status_round_trips_and_defaults_to_not_run() {
        assert_eq!(VerificationStatus::default(), VerificationStatus::NotRun);
        for v in [
            VerificationStatus::Passed,
            VerificationStatus::Failed,
            VerificationStatus::NotRun,
            VerificationStatus::Unavailable,
        ] {
            assert_eq!(VerificationStatus::from_str(v.as_str()), Ok(v));
        }
        assert!(VerificationStatus::from_str("verified").is_err());
    }

    #[test]
    fn budget_limited_round_trips_without_becoming_failed() {
        assert_eq!(TaskOutcome::BudgetLimited.as_str(), "budget_limited");
        assert_eq!(
            TaskOutcome::from_str("budget_limited").unwrap(),
            TaskOutcome::BudgetLimited
        );
    }

    #[test]
    fn round_trips_through_str() {
        for s in [
            SessionStatus::Created,
            SessionStatus::Running,
            SessionStatus::Completed,
            SessionStatus::Incomplete,
            SessionStatus::Blocked,
            SessionStatus::Interrupted,
            SessionStatus::Failed,
        ] {
            assert_eq!(SessionStatus::from_str(s.as_str()), Ok(s));
        }
        for o in [
            TaskOutcome::Completed,
            TaskOutcome::Blocked,
            TaskOutcome::BudgetLimited,
            TaskOutcome::Failed,
            TaskOutcome::Interrupted,
        ] {
            assert_eq!(TaskOutcome::from_str(o.as_str()), Ok(o));
        }
        for o in [
            TurnOutcome::Completed,
            TurnOutcome::Failed,
            TurnOutcome::Interrupted,
        ] {
            let encoded = serde_json::to_value(o).unwrap();
            assert_eq!(serde_json::from_value::<TurnOutcome>(encoded).unwrap(), o);
        }
    }

    #[test]
    fn unknown_persisted_value_is_a_named_error_not_a_default() {
        let err = SessionStatus::from_str("bogus").unwrap_err();
        assert_eq!(err.kind, "session status");
        assert_eq!(err.value, "bogus");
        assert!(TaskOutcome::from_str("done").is_err());
    }

    /// The child's short task title is part of the durable spawn record, so a
    /// `SubAgentStarted` replayed after restart still names the task. It is
    /// optional: rows written before titles existed deserialize with `None`
    /// rather than failing, and `None` is skipped on the wire.
    #[test]
    fn child_spawn_spec_carries_an_optional_task_title() {
        let with_title = ChildSpawnSpec {
            title: Some("audit the refund path".into()),
            ..ChildSpawnSpec::default()
        };
        let json = serde_json::to_value(&with_title).unwrap();
        assert_eq!(json["title"], "audit the refund path");
        assert_eq!(
            serde_json::from_value::<ChildSpawnSpec>(json).unwrap(),
            with_title
        );

        let legacy: ChildSpawnSpec = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(legacy.title, None);
        let encoded = serde_json::to_value(&legacy).unwrap();
        assert!(
            encoded.get("title").is_none(),
            "an absent title is not written as null: {encoded}"
        );
    }
}
