//! State transition records over the shared [`AgentState`] machine (spec §22).
//!
//! `AgentState` itself lives in `leveler-lifecycle` so storage/engine/clients
//! can speak it without depending on this high-level crate.

use serde::{Deserialize, Serialize};

pub use leveler_lifecycle::AgentState;

/// Why a transition happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionReason {
    /// Normal forward progress.
    Advance,
    /// A recoverable failure routed to repair/relocalize.
    Recover(String),
    /// An unrecoverable failure.
    Fatal(String),
    /// Cancelled by the user.
    Cancelled,
}

/// A recorded state transition (spec §23).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateTransition {
    pub from: AgentState,
    pub to: AgentState,
    pub reason: TransitionReason,
}

impl StateTransition {
    pub fn advance(from: AgentState, to: AgentState) -> Self {
        Self {
            from,
            to,
            reason: TransitionReason::Advance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_records_forward_progress() {
        let t = StateTransition::advance(AgentState::Plan, AgentState::Execute);
        assert_eq!(t.from, AgentState::Plan);
        assert_eq!(t.to, AgentState::Execute);
        assert_eq!(t.reason, TransitionReason::Advance);
    }
}
