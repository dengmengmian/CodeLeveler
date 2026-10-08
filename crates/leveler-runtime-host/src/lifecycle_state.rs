//! The lifecycle vocabulary a CLIENT displays.
//!
//! # Why this is a projection, not a state machine
//!
//! The runtime lifecycle already has exactly one source of truth per fact:
//! [`EnsureError`] for what a shell failed to obtain, [`RuntimeHealth`] for what
//! a runtime reports about itself, and [`HandoffEvent`] for what a handover
//! observed while it ran. Terminals, the Desktop bridge and the Web server all
//! read those types, and each of them used to render them its own way — which
//! is how "the runtime is busy and the upgrade is deferred" could reach a person
//! as "upgrade done", and how one shell could call a runtime unusable while
//! another happily attached to it.
//!
//! [`RuntimeLifecycleState`] adds no state and owns no transition. Every variant
//! is DERIVED from one of those existing types by an exhaustive match, so it
//! cannot disagree with them, and it exists so that three shells describe the
//! same runtime the same way. It is deliberately a projection a caller can
//! branch on (rather than a rendered string): a state name is a contract, and a
//! `Debug` rendering is not.

use crate::client::EnsureError;
use leveler_client_protocol::RuntimeHealth;

/// What a client can say about the runtime it is looking at, in the one
/// vocabulary every shell shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeLifecycleState {
    /// A runtime is coming up and has not answered yet.
    Starting,
    /// This generation's runtime is serving. The only state in which a client
    /// may attach and serve new work.
    Ready,
    /// The previous runtime answered that it still owes work, and this shell
    /// cannot ask an operator whether to wait.
    Busy {
        active_turns: u32,
        active_background_tasks: u32,
    },
    /// The previous runtime committed to retiring and is draining. It no longer
    /// admits work.
    Retiring,
    /// The previous runtime accepted the connection and did not answer. Nothing
    /// is known about it, including whether it is alive.
    Unresponsive,
    /// A runtime is answering, but not as a generation this client can share an
    /// endpoint with — and it could not be replaced.
    Incompatible,
    /// The upgrade is deferred, not refused: the previous runtime is unchanged,
    /// still admitting work, and still serving every other client.
    UpgradeDeferred {
        active_turns: u32,
        active_background_tasks: u32,
    },
    /// A replacement was attempted and abandoned. Nothing was signalled on a
    /// guess, so the previous runtime is untouched.
    MigrationFailed,
}

/// What the reader of a [`RuntimeLifecycleState`] should do next.
///
/// Every state has one, because a state a person cannot act on is just a
/// complaint. Text is rendered by each shell from THIS value, so the advice
/// cannot drift from the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    /// Attach and use this runtime.
    None,
    /// Wait for the runtime to answer.
    WaitForReadiness,
    /// Wait for the running work to finish.
    WaitForWork,
    /// Wait for the draining runtime to exit.
    WaitForDrain,
    /// Exit the previous CodeLeveler manually, then start again.
    ExitPreviousVersion,
    /// Try again after the cause is resolved.
    Retry,
}

impl LifecycleAction {
    /// The stable name a non-terminal shell puts in a structured result. It is
    /// a fixed vocabulary, so a client branches on it rather than on prose.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::WaitForReadiness => "wait_for_readiness",
            Self::WaitForWork => "wait_for_work",
            Self::WaitForDrain => "wait_for_drain",
            Self::ExitPreviousVersion => "exit_previous_version",
            Self::Retry => "retry",
        }
    }
}

impl RuntimeLifecycleState {
    /// The state a failed handover ended in.
    pub fn from_ensure_error(error: &EnsureError) -> Self {
        match error {
            EnsureError::UnknownGeneration => Self::Incompatible,
            EnsureError::RetireBlocked {
                waited_secs,
                active_turns,
                active_background_tasks,
            } => {
                // A blocked retire is two different facts. `waited_secs == 0`
                // is the runtime's OWN answer that work is owed — the upgrade is
                // the thing that was deferred. A non-zero wait means the budget
                // ran out while the runtime said nothing at all.
                if *waited_secs == 0 {
                    Self::UpgradeDeferred {
                        active_turns: *active_turns,
                        active_background_tasks: *active_background_tasks,
                    }
                } else {
                    Self::Busy {
                        active_turns: *active_turns,
                        active_background_tasks: *active_background_tasks,
                    }
                }
            }
            EnsureError::LegacyMigrationRefused { .. } => Self::MigrationFailed,
            EnsureError::ReadyTimeout { .. } | EnsureError::StartupFailed { .. } => {
                Self::Unresponsive
            }
        }
    }

    /// The state a runtime reports about ITSELF.
    ///
    /// Health is not ownership and never becomes an authorization: this only
    /// names the fact the runtime published.
    pub fn from_health(health: &RuntimeHealth) -> Self {
        if health.shutting_down {
            Self::Retiring
        } else if health.quiescent() {
            Self::Ready
        } else {
            Self::Busy {
                active_turns: health.active_turns,
                active_background_tasks: health.active_background_tasks,
            }
        }
    }

    /// The wire/diagnostic name. Additive: a client that does not know a name
    /// must treat it as "unknown", never as `ready`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Busy { .. } => "busy",
            Self::Retiring => "retiring",
            Self::Unresponsive => "unresponsive",
            Self::Incompatible => "incompatible",
            Self::UpgradeDeferred { .. } => "upgrade_deferred",
            Self::MigrationFailed => "migration_failed",
        }
    }

    /// What to do about it.
    pub fn next_step(self) -> LifecycleAction {
        match self {
            Self::Ready => LifecycleAction::None,
            Self::Starting => LifecycleAction::WaitForReadiness,
            Self::Busy { .. } => LifecycleAction::WaitForWork,
            Self::Retiring => LifecycleAction::WaitForDrain,
            Self::UpgradeDeferred { .. } => LifecycleAction::WaitForWork,
            Self::Unresponsive => LifecycleAction::Retry,
            Self::Incompatible | Self::MigrationFailed => LifecycleAction::ExitPreviousVersion,
        }
    }

    /// Whether a client may attach and serve new work right now.
    pub fn admits_work(self) -> bool {
        matches!(self, Self::Ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deferred_upgrade_never_reads_as_ready() {
        for state in [
            RuntimeLifecycleState::Busy {
                active_turns: 1,
                active_background_tasks: 0,
            },
            RuntimeLifecycleState::UpgradeDeferred {
                active_turns: 1,
                active_background_tasks: 0,
            },
            RuntimeLifecycleState::Retiring,
            RuntimeLifecycleState::Unresponsive,
            RuntimeLifecycleState::Incompatible,
            RuntimeLifecycleState::MigrationFailed,
            RuntimeLifecycleState::Starting,
        ] {
            assert!(!state.admits_work(), "{state:?} must not admit work");
            assert_ne!(state.as_str(), "ready");
        }
        assert!(RuntimeLifecycleState::Ready.admits_work());
    }

    #[test]
    fn every_state_has_a_next_step_that_is_not_a_dead_end() {
        for state in [
            RuntimeLifecycleState::Starting,
            RuntimeLifecycleState::Ready,
            RuntimeLifecycleState::Busy {
                active_turns: 0,
                active_background_tasks: 0,
            },
            RuntimeLifecycleState::Retiring,
            RuntimeLifecycleState::Unresponsive,
            RuntimeLifecycleState::Incompatible,
            RuntimeLifecycleState::UpgradeDeferred {
                active_turns: 0,
                active_background_tasks: 0,
            },
            RuntimeLifecycleState::MigrationFailed,
        ] {
            let step = state.next_step();
            if state == RuntimeLifecycleState::Ready {
                assert_eq!(step, LifecycleAction::None);
            } else {
                assert_ne!(
                    step,
                    LifecycleAction::None,
                    "{state:?} must tell the reader what to do"
                );
            }
        }
    }

    #[test]
    fn a_runtime_that_answered_busy_is_deferred_not_unanswered() {
        // waited_secs == 0 is the fast path: the runtime's own `Busy` answer.
        let answered = EnsureError::RetireBlocked {
            waited_secs: 0,
            active_turns: 2,
            active_background_tasks: 1,
        };
        assert_eq!(
            RuntimeLifecycleState::from_ensure_error(&answered),
            RuntimeLifecycleState::UpgradeDeferred {
                active_turns: 2,
                active_background_tasks: 1,
            }
        );
        // A non-zero wait is the budget running out, which is not a decision.
        let unanswered = EnsureError::RetireBlocked {
            waited_secs: 30,
            active_turns: 0,
            active_background_tasks: 0,
        };
        assert_eq!(
            RuntimeLifecycleState::from_ensure_error(&unanswered),
            RuntimeLifecycleState::Busy {
                active_turns: 0,
                active_background_tasks: 0,
            }
        );
    }

    #[test]
    fn a_shutting_down_runtime_reports_retiring_not_ready() {
        let health = RuntimeHealth {
            shutting_down: true,
            quiescent: true,
            accepting_work: false,
            ..RuntimeHealth::default()
        };
        assert_eq!(
            RuntimeLifecycleState::from_health(&health),
            RuntimeLifecycleState::Retiring
        );
    }
}
