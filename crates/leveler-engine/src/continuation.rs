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
    /// Consecutive work windows that made no material progress before this turn.
    /// The supervisor counts them across `DriveGoalAgain` windows (each window is
    /// a fresh turn whose per-turn progress ledger resets), so a goal that keeps
    /// hitting the round ceiling without moving the workspace is stopped instead
    /// of burning every supervised window.
    pub windows_without_progress: u32,
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
        // A pinned round budget means the caller owns pacing; never add turns —
        // with ONE narrow exception (settlement × continuation seam, FA-2 /
        // ORC-B1): a local window ceiling reached with a settled child result
        // the parent never acted on is NOT goal exhaustion. While the pinned
        // TOTAL budget still has rounds left, open one bounded integration
        // window; the engine clamps that window to the remaining budget, so
        // the total is consumed, never topped up.
        if let Some(total) = ended.round_budget.round_limit() {
            if ended.stop_reason == StopReason::TurnLimitReached
                && ended.progress.unconsumed_child_settlements > 0
                && ended.progress.cumulative_rounds < total
                && !ended.progress.human_boundary_seen()
                // A Closing/Terminal ledger would not seed into the next
                // window (fresh-epoch accounting) — the integration window
                // only opens where the clamp's epoch spend survives.
                && !ended.progress.is_terminal_for_inheritance()
                && ended.windows_without_progress < MAX_NO_PROGRESS_WINDOWS
            {
                return Continuation::DriveGoalAgain;
            }
            return Continuation::Stop;
        }
        if ended.stop_reason == StopReason::Stalled
            && ended.progress.allows_engine_continue(self.caps)
        {
            return Continuation::DriveGoalAgain;
        }
        // Human denial is already in allows_engine_continue; Blocked never
        // continues. Keep TurnLimitReached from opening a new window that
        // would re-ask a permission the user just refused.
        if ended.progress.human_boundary_seen()
            && matches!(
                ended.stop_reason,
                StopReason::Stalled | StopReason::TurnLimitReached
            )
        {
            return Continuation::Stop;
        }
        // The per-turn round ceiling ends a WORK WINDOW, not the goal: open the
        // next bounded window (a fresh objective-restated turn), unless the goal
        // has been spinning without material workspace progress across windows.
        // A pinned round budget already returned Stop above, so evals are unaffected.
        if ended.stop_reason == StopReason::TurnLimitReached
            && ended.windows_without_progress < MAX_NO_PROGRESS_WINDOWS
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

/// How many consecutive no-material-progress work windows a goal may open before
/// the supervisor stops driving it. Bounds the multi-window continuation so a
/// stuck goal converges in a couple of windows rather than the absolute
/// `MAX_SUPERVISED_TURNS` ceiling. A window that moves the workspace resets it.
pub const MAX_NO_PROGRESS_WINDOWS: u32 = 2;

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
            windows_without_progress: 0,
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
    fn human_denial_stops_stalled_and_turn_limit_continuation() {
        let mut progress = active();
        progress.record_human_denial(false, true);
        let policy = DefaultSupervisorPolicy::default();
        assert_eq!(
            policy.after_turn(&ended(StopReason::Stalled, &progress, &[], None)),
            Continuation::Stop
        );
        assert_eq!(
            policy.after_turn(&ended(StopReason::TurnLimitReached, &progress, &[], None)),
            Continuation::Stop
        );
        assert_eq!(
            policy.after_turn(&ended(StopReason::Blocked, &progress, &[], None)),
            Continuation::Stop
        );
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
        // TurnLimitReached is deliberately NOT here: a goal that exhausted a work
        // window's round budget opens the next window (see the ceiling tests).
        for reason in [
            StopReason::Completed,
            StopReason::Answered,
            StopReason::Blocked,
            StopReason::CloseoutForced,
        ] {
            assert_eq!(
                policy.after_turn(&ended(reason, &progress, &[], None)),
                Continuation::Stop,
                "{reason:?} must not be re-driven"
            );
        }
    }

    #[test]
    fn a_goal_that_hit_the_round_ceiling_opens_the_next_window() {
        // R2 (G2) — the per-turn round ceiling ends a WORK WINDOW, not the goal.
        let progress = active();
        let policy = DefaultSupervisorPolicy::default();
        assert_eq!(
            policy.after_turn(&ended(StopReason::TurnLimitReached, &progress, &[], None)),
            Continuation::DriveGoalAgain,
        );
    }

    #[test]
    fn a_goal_spinning_across_windows_finally_stops() {
        // R2 (G11) — a goal that keeps hitting the ceiling without material
        // workspace progress across windows stops instead of burning all 32.
        let progress = active();
        let policy = DefaultSupervisorPolicy::default();
        let mut e = ended(StopReason::TurnLimitReached, &progress, &[], None);
        e.windows_without_progress = MAX_NO_PROGRESS_WINDOWS;
        assert_eq!(
            policy.after_turn(&e),
            Continuation::Stop,
            "the cross-window no-progress cap must bound multi-window continuation"
        );
    }

    #[test]
    fn a_pinned_budget_ceiling_never_opens_a_window() {
        // An eval's fixed round budget owns pacing: no multi-window continuation
        // — when there is no orchestration debt (the common case).
        let progress = active();
        let policy = DefaultSupervisorPolicy::default();
        let mut e = ended(StopReason::TurnLimitReached, &progress, &[], None);
        e.round_budget = ContinuationPolicy::bounded(5);
        assert_eq!(policy.after_turn(&e), Continuation::Stop);
    }

    // ── settlement × continuation seam (FA-2 / ORC-B1 accident) ────────────

    /// A ledger shaped like the accident: the local window closed with a
    /// settled-but-unconsumed child result and epoch rounds under the total.
    fn debted(cumulative_rounds: u32, debt: u32) -> ProgressLedger {
        ProgressLedger {
            cumulative_rounds,
            unconsumed_child_settlements: debt,
            ..active()
        }
    }

    #[test]
    fn a_pinned_budget_with_unconsumed_settlement_debt_opens_an_integration_window() {
        // THE accident regression (FA-2, ORC-B1): parent hit the 100-round
        // local window ceiling, a child result settled unconsumed, and 180
        // rounds of the pinned 280 total remain — the goal must get a bounded
        // integration window, not terminalize with the result stranded.
        let progress = debted(100, 1);
        let policy = DefaultSupervisorPolicy::default();
        let mut e = ended(StopReason::TurnLimitReached, &progress, &[], None);
        e.round_budget = ContinuationPolicy::bounded(280);
        assert_eq!(
            policy.after_turn(&e),
            Continuation::DriveGoalAgain,
            "a settled-unconsumed child result with global budget remaining must \
             open a bounded integration window"
        );
    }

    #[test]
    fn settlement_debt_cannot_manufacture_budget_past_the_pinned_total() {
        // Negative B: global budget exhausted — debt buys nothing.
        let progress = debted(280, 1);
        let policy = DefaultSupervisorPolicy::default();
        let mut e = ended(StopReason::TurnLimitReached, &progress, &[], None);
        e.round_budget = ContinuationPolicy::bounded(280);
        assert_eq!(policy.after_turn(&e), Continuation::Stop);
    }

    #[test]
    fn a_consumed_settlement_does_not_reopen_a_window() {
        // Negative C at the policy layer: once the parent acted on the notice
        // (debt reset to 0), a later ceiling is the ordinary pinned Stop.
        let progress = debted(100, 0);
        let policy = DefaultSupervisorPolicy::default();
        let mut e = ended(StopReason::TurnLimitReached, &progress, &[], None);
        e.round_budget = ContinuationPolicy::bounded(280);
        assert_eq!(policy.after_turn(&e), Continuation::Stop);
    }

    #[test]
    fn settlement_debt_respects_the_human_boundary() {
        let mut progress = debted(100, 1);
        progress.record_human_denial(false, true);
        let policy = DefaultSupervisorPolicy::default();
        let mut e = ended(StopReason::TurnLimitReached, &progress, &[], None);
        e.round_budget = ContinuationPolicy::bounded(280);
        assert_eq!(policy.after_turn(&e), Continuation::Stop);
    }

    #[test]
    fn settlement_debt_respects_the_no_progress_window_cap() {
        // A debt window that keeps moving nothing converges like any other.
        let progress = debted(100, 1);
        let policy = DefaultSupervisorPolicy::default();
        let mut e = ended(StopReason::TurnLimitReached, &progress, &[], None);
        e.round_budget = ContinuationPolicy::bounded(280);
        e.windows_without_progress = MAX_NO_PROGRESS_WINDOWS;
        assert_eq!(policy.after_turn(&e), Continuation::Stop);
    }

    #[test]
    fn settlement_debt_does_not_open_a_window_for_closing_progress() {
        // Review 建议B: a ledger already Closing/Terminal would NOT seed into
        // the next window (fresh-epoch accounting) — the exception must not
        // fire there, or the clamp would restart from zero rounds.
        let mut progress = debted(100, 1);
        progress.enter_closing();
        let policy = DefaultSupervisorPolicy::default();
        let mut e = ended(StopReason::TurnLimitReached, &progress, &[], None);
        e.round_budget = ContinuationPolicy::bounded(280);
        assert_eq!(policy.after_turn(&e), Continuation::Stop);
    }

    #[test]
    fn settlement_debt_never_reopens_a_terminal_or_non_ceiling_stop() {
        // Terminal monotonicity (negative G): only the local-window ceiling
        // qualifies; a completed, stalled, or budget-exhausted pinned run
        // stays terminal no matter what debt reads.
        let progress = debted(100, 1);
        let policy = DefaultSupervisorPolicy::default();
        for reason in [
            StopReason::Completed,
            StopReason::Answered,
            StopReason::Blocked,
            StopReason::CloseoutForced,
            StopReason::Stalled,
            StopReason::BudgetExhausted,
            StopReason::Incomplete,
        ] {
            let mut e = ended(reason, &progress, &[], None);
            e.round_budget = ContinuationPolicy::bounded(280);
            assert_eq!(
                policy.after_turn(&e),
                Continuation::Stop,
                "{reason:?} must not be reopened by settlement debt"
            );
        }
    }
}
