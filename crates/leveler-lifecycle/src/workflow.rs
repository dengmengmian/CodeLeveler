//! **Coding workflow** vocabulary — the domain-specific phases a Coding run
//! moves through.
//!
//! These states refine the generic runtime lifecycle in [`crate::runtime`];
//! they never replace it. `AgentState` is a Coding-domain breadcrumb
//! (persisted in `sessions.state`), not the runtime's operational status —
//! that is [`crate::runtime::SessionStatus`], and the two are written as
//! separate columns by the engine's single lifecycle writer. A future domain
//! (NPC, container-hosted work) gets its own workflow module; it must not
//! grow new variants here or reinterpret runtime semantics through them.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::runtime::UnknownVariant;

/// The explicit Coding phases the orchestrator moves through (spec §22).
/// Modeled as a real enum rather than an implicit loop. Persisted in
/// `sessions.state` as an agent-phase breadcrumb.
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

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn unknown_persisted_value_is_a_named_error_not_a_default() {
        assert!(AgentState::from_str("").is_err());
    }
}
