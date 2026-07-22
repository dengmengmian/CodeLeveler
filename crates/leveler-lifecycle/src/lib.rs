//! The execution lifecycle vocabulary.
//!
//! Session status, agent state, and task outcome are persisted by
//! `leveler-storage`, produced by `leveler-engine`, driven by
//! `leveler-orchestrator`, projected by clients, and mapped by the app. They
//! are one shared, typed vocabulary rather than strings passed across layer
//! boundaries — so every layer speaks the same language and the low-level
//! storage crate can persist them without a back-edge to a high-level crate.
//!
//! Three axes are kept deliberately distinct (see the M1A ADR):
//! - [`SessionStatus`] — the *operational* position in the lifecycle.
//! - [`TaskOutcome`] — the *terminal* verdict; `Verified` is the only
//!   automation success.
//! - [`TurnOutcome`] — whether one engine turn completed, failed, or was
//!   interrupted, independent of the task's later verification verdict.
//!
//! Each enum round-trips through a lowercase wire string: `as_str` for
//! persistence, [`std::str::FromStr`] for decode. An unknown persisted value is
//! a named [`UnknownVariant`] error — never a guessed default.

#![forbid(unsafe_code)]

mod axes;
mod contract;
mod impact;
mod ledger;
mod objective;
mod plan;
mod progress;
mod readiness;

pub use axes::{CollaborationMode, DepthUseMetrics, ToolSurface, WorkProfile};
pub use contract::TaskContract;
pub use impact::{ChangeImpact, is_build_relevant};
pub use ledger::{
    CompleteStepReceipt, EvidenceLedger, InterceptRecord, MutationRecord, VerifyRecord,
};
pub use objective::{ObjectiveAnchor, ObjectiveSource};
pub use plan::{PlanOrigin, PlanState, PlanStep};
pub use progress::{ProgressCaps, ProgressLedger, TurnPhase};
pub use readiness::{
    GateConfig, ProcessEvidence, ReadinessFailure, TaskClass, check, check_goal_complete,
    classify_task, task_looks_like_implementation,
};

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

/// The explicit states the orchestrator moves through (spec §22). Modeled as a
/// real enum rather than an implicit loop. Persisted in `sessions.state` as an
/// agent-phase breadcrumb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Understand,
    Localize,
    Plan,
    CheckPlan,
    Execute,
    VerifyStep,
    Repair,
    VerifyTask,
    Review,
    Complete,
    Failed,
    Cancelled,
}

impl AgentState {
    /// The lowercase state name, used for persistence.
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentState::Understand => "understand",
            AgentState::Localize => "localize",
            AgentState::Plan => "plan",
            AgentState::CheckPlan => "check_plan",
            AgentState::Execute => "execute",
            AgentState::VerifyStep => "verify_step",
            AgentState::Repair => "repair",
            AgentState::VerifyTask => "verify_task",
            AgentState::Review => "review",
            AgentState::Complete => "complete",
            AgentState::Failed => "failed",
            AgentState::Cancelled => "cancelled",
        }
    }

    /// Whether this is a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AgentState::Complete | AgentState::Failed | AgentState::Cancelled
        )
    }
}

impl FromStr for AgentState {
    type Err = UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "understand" => AgentState::Understand,
            "localize" => AgentState::Localize,
            "plan" => AgentState::Plan,
            "check_plan" => AgentState::CheckPlan,
            "execute" => AgentState::Execute,
            "verify_step" => AgentState::VerifyStep,
            "repair" => AgentState::Repair,
            "verify_task" => AgentState::VerifyTask,
            "review" => AgentState::Review,
            "complete" => AgentState::Complete,
            "failed" => AgentState::Failed,
            "cancelled" => AgentState::Cancelled,
            other => {
                return Err(UnknownVariant {
                    kind: "agent state",
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
    fn terminal_states() {
        assert!(AgentState::Complete.is_terminal());
        assert!(!AgentState::Execute.is_terminal());
    }

    #[test]
    fn state_serialization_is_snake_case() {
        assert_eq!(
            serde_json::to_value(AgentState::CheckPlan).unwrap(),
            "check_plan"
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
        for s in [
            AgentState::Understand,
            AgentState::Localize,
            AgentState::Plan,
            AgentState::CheckPlan,
            AgentState::Execute,
            AgentState::VerifyStep,
            AgentState::Repair,
            AgentState::VerifyTask,
            AgentState::Review,
            AgentState::Complete,
            AgentState::Failed,
            AgentState::Cancelled,
        ] {
            assert_eq!(AgentState::from_str(s.as_str()), Ok(s));
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
        assert!(AgentState::from_str("").is_err());
        assert!(TaskOutcome::from_str("done").is_err());
    }
}
