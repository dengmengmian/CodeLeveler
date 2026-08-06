//! Who decides that a finished turn should be followed by another one.
//!
//! The engine owns the MECHANISM (start a persisted turn, merge its outcome,
//! stop on cancellation or a hard limit). Whether a turn *deserves* a
//! successor is a product judgement — "a quiet goal should be nudged along",
//! "a budget stop with real progress may buy a little more" — so it lives
//! behind [`SupervisorPolicy`] instead of being hard-coded in the supervisor
//! (convergence plan phases 4 and 5).
//!
//! [`DefaultSupervisorPolicy`] reproduces the historical behavior exactly;
//! [`SupervisorPolicy::none`] is the minimal supervisor that never re-drives a
//! turn on its own. Neither can weaken a boundary: cancellation, hard budgets,
//! the absolute round ceiling, and the eval round limit are checked by the
//! engine before a policy is ever consulted.

use leveler_agent::{
    BudgetExhaustion, ContinuationPolicy as RoundBudget, MAX_BUDGET_EXTENSIONS, StepLimits,
    StopReason, budget_extension_allowed, stop_detail_indicates_no_progress,
};
use leveler_lifecycle::{ProgressCaps, ProgressLedger};

/// What the engine should do after a turn reaches a terminal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Continuation {
    /// The task is finished as far as supervision is concerned.
    Stop,
    /// Re-drive the active goal from the latest context (the model went quiet
    /// without resolving it).
    DriveGoalAgain,
    /// Grant one bounded budget extension and resume the transcript.
    ExtendBudget(BudgetExhaustion),
}

/// Everything a policy may look at. Deliberately a plain snapshot of facts the
/// engine already has: a policy reads outcomes, never storage or tools.
pub struct TurnEnded<'a> {
    pub stop_reason: StopReason,
    pub stop_detail: Option<&'a str>,
    pub progress: &'a ProgressLedger,
    pub budget_exhaustion: Option<&'a BudgetExhaustion>,
    pub modified_files: &'a [String],
    /// Budget extensions already granted in this task.
    pub extensions_granted: u32,
    /// The caller pinned a fixed round budget (eval reproducibility), so no
    /// supervisor-initiated continuation may happen at all.
    pub round_budget: RoundBudget,
}

/// Decides whether a finished turn gets a successor.
pub trait SupervisorPolicy: Send + Sync {
    fn after_turn(&self, ended: &TurnEnded<'_>) -> Continuation;
}

/// The historical behavior, unchanged: nudge a stalled goal while the progress
/// ledger still permits it, and extend an exhausted budget up to
/// [`MAX_BUDGET_EXTENSIONS`] when the turn made real progress.
#[derive(Default)]
pub struct DefaultSupervisorPolicy {
    caps: ProgressCaps,
}

impl SupervisorPolicy for DefaultSupervisorPolicy {
    fn after_turn(&self, ended: &TurnEnded<'_>) -> Continuation {
        // A pinned round budget means the caller owns pacing; never add turns.
        if ended.round_budget.round_limit().is_some() {
            return Continuation::Stop;
        }
        if ended.stop_reason == StopReason::Stalled
            && ended.progress.allows_engine_continue(self.caps)
        {
            return Continuation::DriveGoalAgain;
        }
        if budget_extension_allowed(
            ended.stop_reason,
            ended.extensions_granted,
            !ended.modified_files.is_empty(),
            stop_detail_indicates_no_progress(ended.stop_detail),
        ) && let Some(exhaustion) = ended.budget_exhaustion
        {
            return Continuation::ExtendBudget(exhaustion.clone());
        }
        Continuation::Stop
    }
}

/// A supervisor that never re-drives a turn: one turn in, one outcome out.
/// Useful for hosts that own their own pacing (evals, batch runners, a future
/// NPC scheduler) without reimplementing the engine.
pub struct NoContinuation;

impl SupervisorPolicy for NoContinuation {
    fn after_turn(&self, _ended: &TurnEnded<'_>) -> Continuation {
        Continuation::Stop
    }
}

/// How much extra budget one extension grants (mechanism, not policy).
pub fn extended_limits(limits: StepLimits, exhaustion: &BudgetExhaustion) -> StepLimits {
    leveler_agent::grant_budget_extension(limits, exhaustion)
}

/// The cap the engine enforces regardless of policy — a policy that keeps
/// asking for extensions cannot exceed it.
pub const MAX_EXTENSIONS: u32 = MAX_BUDGET_EXTENSIONS;

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_agent::{BudgetDimension, ContinuationPolicy};
    use leveler_lifecycle::TurnPhase;

    fn ended<'a>(
        stop_reason: StopReason,
        progress: &'a ProgressLedger,
        modified: &'a [String],
        exhaustion: Option<&'a BudgetExhaustion>,
    ) -> TurnEnded<'a> {
        TurnEnded {
            stop_reason,
            stop_detail: None,
            progress,
            budget_exhaustion: exhaustion,
            modified_files: modified,
            extensions_granted: 0,
            round_budget: ContinuationPolicy::UntilTerminal,
        }
    }

    fn active() -> ProgressLedger {
        ProgressLedger {
            phase: TurnPhase::Active,
            closing: false,
            ..Default::default()
        }
    }

    #[test]
    fn a_stalled_goal_with_progress_left_is_driven_again() {
        let progress = active();
        let policy = DefaultSupervisorPolicy::default();
        assert_eq!(
            policy.after_turn(&ended(StopReason::Stalled, &progress, &[], None)),
            Continuation::DriveGoalAgain
        );
    }

    #[test]
    fn a_stalled_goal_past_the_no_progress_cap_stops() {
        let mut progress = active();
        let caps = ProgressCaps::default();
        for round in 0..(caps.no_progress_rounds + 2) {
            progress.note_no_progress_round(round);
        }
        let policy = DefaultSupervisorPolicy::default();
        assert_eq!(
            policy.after_turn(&ended(StopReason::Stalled, &progress, &[], None)),
            Continuation::Stop,
            "the no-progress cap must not be evadable by a policy consult"
        );
    }

    #[test]
    fn an_exhausted_budget_with_real_work_buys_one_extension() {
        let progress = active();
        let exhaustion = BudgetExhaustion::new(BudgetDimension::Commands, 10, 10);
        let modified = vec!["src/lib.rs".to_string()];
        let policy = DefaultSupervisorPolicy::default();
        assert_eq!(
            policy.after_turn(&ended(
                StopReason::BudgetExhausted,
                &progress,
                &modified,
                Some(&exhaustion),
            )),
            Continuation::ExtendBudget(exhaustion)
        );
    }

    #[test]
    fn an_exhausted_budget_without_work_stops() {
        let progress = active();
        let exhaustion = BudgetExhaustion::new(BudgetDimension::Commands, 10, 10);
        let policy = DefaultSupervisorPolicy::default();
        assert_eq!(
            policy.after_turn(&ended(
                StopReason::BudgetExhausted,
                &progress,
                &[],
                Some(&exhaustion),
            )),
            Continuation::Stop
        );
    }

    #[test]
    fn a_pinned_round_budget_disables_supervisor_continuation() {
        let progress = active();
        let policy = DefaultSupervisorPolicy::default();
        let mut e = ended(StopReason::Stalled, &progress, &[], None);
        e.round_budget = ContinuationPolicy::bounded(5);
        assert_eq!(
            policy.after_turn(&e),
            Continuation::Stop,
            "an eval's fixed round budget must not be topped up by the supervisor"
        );
    }

    #[test]
    fn the_none_policy_never_continues() {
        let progress = active();
        let exhaustion = BudgetExhaustion::new(BudgetDimension::Commands, 10, 10);
        let modified = vec!["src/lib.rs".to_string()];
        for reason in [StopReason::Stalled, StopReason::BudgetExhausted] {
            assert_eq!(
                NoContinuation.after_turn(&ended(reason, &progress, &modified, Some(&exhaustion))),
                Continuation::Stop
            );
        }
    }

    #[test]
    fn a_clean_finish_never_continues() {
        let progress = active();
        let policy = DefaultSupervisorPolicy::default();
        for reason in [
            StopReason::Completed,
            StopReason::Answered,
            StopReason::Blocked,
            StopReason::CloseoutForced,
            StopReason::TurnLimitReached,
        ] {
            assert_eq!(
                policy.after_turn(&ended(reason, &progress, &[], None)),
                Continuation::Stop,
                "{reason:?} must not be re-driven"
            );
        }
    }
}
