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
        // A fresh grep with new hits is exploration, not thrash; only an
        // observation THIS ROUND re-made counts against the streak (R007 F1 —
        // `repeated_observation` is a per-round fact, never a latch over the
        // turn's whole history).
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

/// Tool-using rounds with no material progress before the engagement advisory
/// is due.
///
/// Calibrated on 28 recorded long-task runs. The eight never-engaged ones ran to
/// the turn ceiling (75-101 rounds) without ever registering a plan or modifying
/// a file; all twenty healthy/KEEP runs reached their first material progress by
/// round 74, median 28. 45 therefore reaches every spiral with most of the
/// budget still unspent, and the four healthy runs it also reaches pay one
/// factual sentence. The threshold is corpus-calibrated, not derived — it is
/// deliberately advisory-only so a false positive costs a message, never a run.
pub const ENGAGEMENT_ADVISORY_AFTER_ROUNDS: u32 = 45;

/// At most this many advisories per drive; the second lands at twice the
/// threshold, mirroring the policy-blocked track's escalate-then-stop shape.
pub const ENGAGEMENT_ADVISORY_MAX: u32 = 2;

/// Everything [`engagement_advisory_due`] needs. Pure data.
#[derive(Debug, Clone, Copy)]
pub struct EngagementInput {
    /// Rounds so far in which at least one tool ran.
    pub tool_rounds: u32,
    /// The run has registered a plan or modified a file at any point. Material
    /// progress is a latch, not a per-round fact: a run that has ever engaged is
    /// never told it has not. A passing verification is deliberately excluded —
    /// running the repository's existing suite says nothing about what this run
    /// produced, and would be a one-command way to silence the advisory.
    pub any_material_progress: bool,
    /// Advisories already injected this drive.
    pub advisories_sent: u32,
    /// Background children are still running. An orchestrating parent
    /// legitimately lands no edits of its own while they work, so the advisory
    /// is suspended rather than firing on successful delegation.
    pub children_outstanding: bool,
}

/// Should the engagement advisory be injected at this round boundary?
///
/// NEVER_ENGAGED_EXPLORATION_SPIRAL: eight recorded qualified runs spent the
/// whole turn budget on successful, novel observation and produced zero
/// material progress. Neither streak above can see that. [`classify`] grades a
/// round containing any successful non-observe call as `Progress`, and
/// [`made_progress`] accepts a command that merely exited 0 — both correct for
/// what they measure, and both load-bearing (dropping the command term
/// force-stops healthy runs around round 5). What was missing is upstream of
/// both: across 100 rounds nothing ever told the model it had written nothing.
///
/// This is an advisory gate, NOT a kill switch. It removes no tool, forces no
/// `ToolChoice`, refuses no call, and never terminates a turn.
pub fn engagement_advisory_due(input: &EngagementInput) -> bool {
    if input.any_material_progress
        || input.children_outstanding
        || input.advisories_sent >= ENGAGEMENT_ADVISORY_MAX
    {
        return false;
    }
    input.tool_rounds
        >= ENGAGEMENT_ADVISORY_AFTER_ROUNDS.saturating_mul(input.advisories_sent.saturating_add(1))
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

    /// R007 F1: `repeated_observation` is a fact about THIS round. The caller
    /// must never pass "some key in the whole turn history repeated once" —
    /// that latch made every later novel observation classify as thrash and
    /// killed a healthy Large-repo discovery phase nine minutes in.
    #[test]
    fn a_round_that_only_observed_new_things_is_exploring_however_old_the_turn_is() {
        let round = RoundInput {
            pure_observe: true,
            // The turn repeated something earlier, but THIS round did not.
            repeated_observation: false,
            ..input()
        };
        assert_eq!(classify(&round), RoundVerdict::ObserveExploring);
        assert!(
            classify(&round).is_neutral(),
            "novel observation must neither grow nor reset the kill streak"
        );
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

    fn engagement(tool_rounds: u32) -> EngagementInput {
        EngagementInput {
            tool_rounds,
            any_material_progress: false,
            advisories_sent: 0,
            children_outstanding: false,
        }
    }

    /// E1/E2/E3: rounds of observation — reads, greps, `cat`/`ls`/`sed`
    /// pipelines, whatever the spelling — never amount to material progress, so
    /// the advisory eventually comes due. The predicate deliberately does not
    /// look at command names at all: that is what made the original classifier
    /// miss every non-`git` observation.
    #[test]
    fn observation_alone_eventually_makes_the_advisory_due() {
        assert!(!engagement_advisory_due(&engagement(
            ENGAGEMENT_ADVISORY_AFTER_ROUNDS - 1
        )));
        assert!(engagement_advisory_due(&engagement(
            ENGAGEMENT_ADVISORY_AFTER_ROUNDS
        )));
    }

    /// E4/E5: any material progress — a landed mutation, a registered plan, a
    /// passing verification — silences the advisory permanently. A run that is
    /// working is never told it is not.
    #[test]
    fn material_progress_silences_the_advisory_for_good() {
        let working = EngagementInput {
            any_material_progress: true,
            ..engagement(ENGAGEMENT_ADVISORY_AFTER_ROUNDS * 10)
        };
        assert!(!engagement_advisory_due(&working));
    }

    /// E6: exploration that is still on its way to a mutation must not be cut
    /// short. Below the threshold nothing happens, and even at the threshold the
    /// only consequence is a message — no tool is removed and no turn ends.
    #[test]
    fn exploration_below_the_threshold_is_untouched() {
        for round in 0..ENGAGEMENT_ADVISORY_AFTER_ROUNDS {
            assert!(
                !engagement_advisory_due(&engagement(round)),
                "round {round} must be left alone"
            );
        }
    }

    /// A parent whose background children are working has legitimately landed
    /// nothing itself. Firing here would penalize exactly the delegation the
    /// product is trying to make dependable.
    #[test]
    fn an_orchestrating_parent_is_not_advised_while_children_run() {
        let orchestrating = EngagementInput {
            children_outstanding: true,
            ..engagement(ENGAGEMENT_ADVISORY_AFTER_ROUNDS * 3)
        };
        assert!(!engagement_advisory_due(&orchestrating));
    }

    /// Bounded escalation, never a nag: the second advisory waits for twice the
    /// threshold and there is no third.
    #[test]
    fn the_advisory_is_bounded_and_escalates_once() {
        let after_first = EngagementInput {
            advisories_sent: 1,
            ..engagement(ENGAGEMENT_ADVISORY_AFTER_ROUNDS)
        };
        assert!(
            !engagement_advisory_due(&after_first),
            "the second must wait for twice the threshold, not repeat immediately"
        );
        let due_again = EngagementInput {
            advisories_sent: 1,
            ..engagement(ENGAGEMENT_ADVISORY_AFTER_ROUNDS * 2)
        };
        assert!(engagement_advisory_due(&due_again));
        let exhausted = EngagementInput {
            advisories_sent: ENGAGEMENT_ADVISORY_MAX,
            ..engagement(ENGAGEMENT_ADVISORY_AFTER_ROUNDS * 100)
        };
        assert!(
            !engagement_advisory_due(&exhausted),
            "there is no third advisory however long the run goes"
        );
    }

    /// E7: an unknown, successful, non-mutating tool (a custom shell wrapper, an
    /// MCP tool) must not read as material progress. The predicate takes the
    /// progress FACT, so a future tool cannot buy progress by exiting 0 — the
    /// exact way the original classifier was fooled.
    #[test]
    fn an_unclassified_successful_tool_does_not_count_as_progress() {
        // The caller passes `any_material_progress` from durable facts (plan
        // registered / file modified / verification passed). Nothing about the
        // tool's name or exit status can set it, so 200 successful rounds of an
        // unknown tool still come due.
        assert!(engagement_advisory_due(&engagement(200)));
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
