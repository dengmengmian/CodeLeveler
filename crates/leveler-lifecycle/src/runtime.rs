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

/// A task's terminal verdict. `Completed` without evidence is
/// `CompletedUnverified`, never silently `Verified`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Verified,
    CompletedUnverified,
    /// Execution stopped at an explicit resource boundary. The task is
    /// incomplete and resumable; this is not evidence of model failure.
    BudgetLimited,
    Failed,
    Interrupted,
}

impl TaskOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskOutcome::Verified => "verified",
            TaskOutcome::CompletedUnverified => "completed_unverified",
            TaskOutcome::BudgetLimited => "budget_limited",
            TaskOutcome::Failed => "failed",
            TaskOutcome::Interrupted => "interrupted",
        }
    }

    /// Whether automation may treat this task as successful and ship it.
    pub fn is_success(self) -> bool {
        self == Self::Verified
    }
}

impl FromStr for TaskOutcome {
    type Err = UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "verified" => TaskOutcome::Verified,
            "completed_unverified" => TaskOutcome::CompletedUnverified,
            "budget_limited" => TaskOutcome::BudgetLimited,
            "failed" => TaskOutcome::Failed,
            "interrupted" => TaskOutcome::Interrupted,
            other => {
                return Err(UnknownVariant {
                    kind: "task outcome",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_verified_is_automation_success() {
        assert!(TaskOutcome::Verified.is_success());
        assert!(!TaskOutcome::CompletedUnverified.is_success());
        assert!(!TaskOutcome::BudgetLimited.is_success());
        assert!(!TaskOutcome::Failed.is_success());
        assert!(!TaskOutcome::Interrupted.is_success());
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
            TaskOutcome::Verified,
            TaskOutcome::CompletedUnverified,
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
}
