//! Did this round make progress? One pure decision, four watchdogs.
//!
//! The drive loop used to answer this inline with a five-branch `if/else if`
//! chain interleaved with the terminating side effects, so the *policy* (what
//! counts as thrash) could only be exercised by running a whole turn against a
//! scripted model. Splitting the verdict out makes the policy a table you can
//! read and unit-test; the caller keeps the side effects.
//!
//! Two independent streaks come out of this, because they measure different
//! failure modes and carry different caps:
//!
//! - the **no-progress** streak — nothing moved (repeated observations, or every
//!   call refused before it ran);
//! - the **stagnation** streak — commands ran and all of them failed, the
//!   "edit → re-run the check that keeps failing" spin the first streak misses.
//!
//! They are deliberately NOT merged: a round can be neutral for one and adverse
//! for the other, and collapsing them would change when turns stop.

/// What one completed round did for the turn's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundVerdict {
    /// The plan is complete, yet the model kept doing substantive work —
    /// re-running builds/tests or re-inspecting files. Redundant closeout
    /// thrash, which is what makes a finished task look like an endless
    /// re-audit.
    CloseoutThrash,
    /// A read-only round whose observations repeated ones already made.
    ObserveThrash,
    /// Every call this round was refused before it ran.
    AllRefused,
    /// A read-only round that turned up something new. Exploration is neither
    /// thrash nor progress: it must not grow the streak, and it must not reset
    /// one that earlier rounds earned.
    ObserveExploring,
    /// Refused, but by the plan-repair gate — deliberately not penalized, so the
    /// generic all-refused watchdog cannot preempt a later valid plan. Absolute
    /// round and resource budgets still bound the retries.
    NeutralPolicyBlocked,
    /// Real work happened; the no-progress streak resets.
    Progress,
}

impl RoundVerdict {
    /// Whether this verdict leaves the no-progress streak untouched. Neither a
    /// penalty nor a reset — the two exploration/exemption cases.
    pub fn is_neutral(self) -> bool {
        matches!(self, Self::ObserveExploring | Self::NeutralPolicyBlocked)
    }
}

/// Everything [`classify`] needs about a finished round. Pure data.
#[derive(Debug, Clone, Copy)]
pub struct RoundInput {
    /// The plan is complete and the turn has entered closing.
    pub closing: bool,
    /// The round ran or attempted a tool that is not plan/goal bookkeeping.
    pub substantive: bool,
    /// Read-only tools ran, nothing else succeeded, and no verification ran.
    pub pure_observe: bool,
    /// Some observation this round repeated one already seen.
    pub repeated_observation: bool,
    /// The round issued at least one call.
    pub had_calls: bool,
    /// Every issued call was refused before execution.
    pub all_denied: bool,
    /// Every refusal came from a HARNESS POLICY gate (plan gate, budgets,
    /// allowlist) — the harness blocked itself; not agent stagnation.
    pub policy_blocked: bool,
}

/// Grade one finished round. Order matters: closeout thrash outranks observe
/// thrash (a finished plan being re-audited is the more specific diagnosis),
/// and the plan-repair exemption must be tested before the generic
/// all-refused rule that would otherwise swallow it.
pub fn classify(input: &RoundInput) -> RoundVerdict {
    if input.closing && input.substantive {
        RoundVerdict::CloseoutThrash
    } else if input.pure_observe {
        // A fresh grep with new hits is exploration, not thrash; only a repeat
        // of an observation already made counts against the streak.
        if input.repeated_observation {
            RoundVerdict::ObserveThrash
        } else {
            RoundVerdict::ObserveExploring
        }
    } else if input.had_calls && input.all_denied {
        if input.policy_blocked {
            RoundVerdict::NeutralPolicyBlocked
        } else {
            RoundVerdict::AllRefused
        }
    } else {
        RoundVerdict::Progress
    }
}

/// Whether a round where commands ran counts as forward motion for the
/// stagnation streak. Any one real signal clears it; a round with no command at
/// all is neutral and never reaches this (goal/plan/edit work is not penalized).
pub fn made_progress(
    verify_passed: bool,
    novel_observe: bool,
    command_succeeded: bool,
    touched_new_file: bool,
) -> bool {
    verify_passed || novel_observe || command_succeeded || touched_new_file
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> RoundInput {
        RoundInput {
            closing: false,
            substantive: false,
            pure_observe: false,
            repeated_observation: false,
            had_calls: false,
            all_denied: false,
            policy_blocked: false,
        }
    }

    #[test]
    fn substantive_work_after_the_plan_is_done_is_closeout_thrash() {
        let i = RoundInput {
            closing: true,
            substantive: true,
            ..input()
        };
        assert_eq!(classify(&i), RoundVerdict::CloseoutThrash);
        // Bookkeeping-only rounds while closing are not thrash: marking the plan
        // complete is exactly what the model is supposed to be doing.
        let bookkeeping = RoundInput {
            substantive: false,
            ..i
        };
        assert_eq!(classify(&bookkeeping), RoundVerdict::Progress);
    }

    #[test]
    fn closeout_thrash_outranks_observe_thrash() {
        let i = RoundInput {
            closing: true,
            substantive: true,
            pure_observe: true,
            repeated_observation: true,
            ..input()
        };
        assert_eq!(classify(&i), RoundVerdict::CloseoutThrash);
    }

    #[test]
    fn only_repeated_observation_is_thrash() {
        let exploring = RoundInput {
            pure_observe: true,
            repeated_observation: false,
            ..input()
        };
        assert_eq!(
            classify(&exploring),
            RoundVerdict::ObserveExploring,
            "a fresh grep with new hits is exploration"
        );
        // Exploration must not reset a streak earlier rounds earned, which is
        // why it is neutral rather than Progress.
        assert!(classify(&exploring).is_neutral());
        let repeating = RoundInput {
            repeated_observation: true,
            ..exploring
        };
        assert_eq!(classify(&repeating), RoundVerdict::ObserveThrash);
        assert!(!classify(&repeating).is_neutral());
    }

    #[test]
    fn policy_blocked_refusals_are_exempt_from_the_all_refused_streak() {
        let refused = RoundInput {
            had_calls: true,
            all_denied: true,
            ..input()
        };
        assert_eq!(classify(&refused), RoundVerdict::AllRefused);
        assert!(!classify(&refused).is_neutral());
        let repair = RoundInput {
            policy_blocked: true,
            ..refused
        };
        assert_eq!(classify(&repair), RoundVerdict::NeutralPolicyBlocked);
        assert!(classify(&repair).is_neutral());
    }

    #[test]
    fn partial_refusal_is_progress() {
        // Some calls ran: the model is iterating on real results. Identical
        // failures are the loop guard's business, not this streak's.
        let i = RoundInput {
            had_calls: true,
            all_denied: false,
            ..input()
        };
        assert_eq!(classify(&i), RoundVerdict::Progress);
    }

    #[test]
    fn a_round_with_no_calls_at_all_is_progress() {
        assert_eq!(classify(&input()), RoundVerdict::Progress);
    }

    #[test]
    fn any_single_signal_clears_the_stagnation_streak() {
        assert!(!made_progress(false, false, false, false));
        for signal in 0..4 {
            let s = |n: usize| n == signal;
            assert!(
                made_progress(s(0), s(1), s(2), s(3)),
                "signal {signal} must clear the streak on its own"
            );
        }
    }
}
