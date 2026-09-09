//! Unified closeout decision point — the single place that decides what
//! happens when the model goes quiet (a round with no tool calls).
//!
//! Historically the quiet branch of `drive` chained four nudge mechanisms with
//! their own counters (3 + 2 + 2 + 1), each able to re-invoke the model. This
//! module is what is left: ONE decision, and ONE repair per turn.
//!
//! The repair is a PROTOCOL repair. Two things can be established
//! mechanically about a quiet round — a goal run did not call `update_goal`,
//! and a round produced no text at all — and each buys exactly one more model
//! round to fix the protocol. A model that still does not resolve after that
//! is not coached again: the turn ends on an honest terminal. Nothing here
//! reads the work for meaning, and nothing here decides the model deserves
//! another turn.

/// Repairs one turn may inject. ONE: a second attempt at the same reminder is
/// coaching, and the harness has nothing new to say.
pub const CLOSEOUT_NUDGE_BUDGET: u8 = 1;

/// What the closeout decided for a quiet round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseoutAction {
    /// Accept the quiet round and terminate the turn.
    Finish,
    /// Inject exactly one nudge for this reason, consuming shared budget.
    NudgeOnce(CloseoutReason),
    /// A nudge is still warranted but the budget (or the round limit) is
    /// spent. Goal mode terminates as `StopReason::Stalled` with this reason
    /// recorded in the stop detail; non-goal turns treat this as `Finish`.
    Stall(CloseoutReason),
}

/// Why the harness wants to re-prompt the model once more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseoutReason {
    /// Goal mode: the model went quiet without calling `update_goal`.
    GoalUnresolved,
    /// The model ended with an empty answer.
    EmptyAnswer,
}

impl CloseoutReason {
    /// Stable machine key (used inside the Stalled stop detail so the engine
    /// can tell a continuation turn what the previous closeout stalled on).
    pub fn as_key(&self) -> &'static str {
        match self {
            Self::GoalUnresolved => "goal_unresolved",
            Self::EmptyAnswer => "empty_answer",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "goal_unresolved" => Some(Self::GoalUnresolved),
            "empty_answer" => Some(Self::EmptyAnswer),
            _ => None,
        }
    }
}

/// Marker prefix embedding the [`CloseoutReason`] key in a Stalled detail.
pub const STALLED_REASON_PREFIX: &str = "closeout_reason=";

/// Build the `StopReason::Stalled` detail carrying the closeout reason.
pub fn stalled_detail(reason: CloseoutReason, base: &str) -> String {
    format!("{STALLED_REASON_PREFIX}{}; {base}", reason.as_key())
}

/// Recover the closeout reason from a Stalled detail built by
/// [`stalled_detail`] (engine continuations consume this).
pub fn reason_from_stalled_detail(detail: &str) -> Option<CloseoutReason> {
    let rest = detail.strip_prefix(STALLED_REASON_PREFIX)?;
    let key = rest.split(';').next()?.trim();
    CloseoutReason::from_key(key)
}

/// Shared per-turn nudge budget. One counter for every mechanism — the old
/// per-mechanism caps are deliberately gone.
#[derive(Debug, Clone, Copy)]
pub struct CloseoutBudget {
    remaining: u8,
}

impl CloseoutBudget {
    pub fn new(total: u8) -> Self {
        Self { remaining: total }
    }

    pub fn remaining(&self) -> u8 {
        self.remaining
    }

    /// Spend one nudge. Callers must only nudge when `remaining() > 0`
    /// (guaranteed by [`decide`] returning `NudgeOnce`).
    pub fn consume(&mut self) {
        debug_assert!(self.remaining > 0, "nudge without budget");
        self.remaining = self.remaining.saturating_sub(1);
    }
}

/// Everything [`decide`] needs to know about a quiet round. Pure data — the
/// function stays unit-testable without a model, tools, or a transcript.
///
/// What the turn touched is deliberately absent: the decision is about the
/// protocol, not about whether the work looks finished.
#[derive(Debug, Clone, Copy)]
pub struct CloseoutInput {
    /// Goal mode: quiet never means done; the model must call `update_goal`.
    pub goal_mode: bool,
    /// The model produced non-empty final text this turn.
    pub has_final_text: bool,
    /// The turn is being cancelled — an empty answer is not worth a nudge.
    pub cancelled: bool,
    /// The continuation policy allows at least one more model round.
    pub can_continue: bool,
    /// Nudges left in the shared per-turn budget.
    pub budget_remaining: u8,
    /// The user explicitly denied a permission this task epoch.
    /// Harness must not buy extra model rounds to pressure past that boundary.
    pub human_boundary_seen: bool,
}

/// Decide the fate of one quiet round. At most one nudge per round, chosen by
/// priority: EmptyAnswer > GoalUnresolved
/// (an empty answer means the model said nothing at all, so it outranks even
/// the goal-mode prompt).
pub fn decide(input: &CloseoutInput) -> CloseoutAction {
    let candidate = if !input.has_final_text && !input.cancelled {
        Some(CloseoutReason::EmptyAnswer)
    } else if input.goal_mode {
        Some(CloseoutReason::GoalUnresolved)
    } else {
        None
    };
    let Some(reason) = candidate else {
        return CloseoutAction::Finish;
    };
    // Human denial is authoritative: the model may adapt on its own next
    // tool-using round, but the harness must not inject GoalUnresolved
    // pressure after the user said no. Empty-answer still gets one chance
    // (the model said nothing at all).
    if input.human_boundary_seen && reason == CloseoutReason::GoalUnresolved {
        return if input.goal_mode {
            CloseoutAction::Stall(reason)
        } else {
            CloseoutAction::Finish
        };
    }
    if input.can_continue && input.budget_remaining > 0 {
        CloseoutAction::NudgeOnce(reason)
    } else if input.goal_mode {
        CloseoutAction::Stall(reason)
    } else {
        CloseoutAction::Finish
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> CloseoutInput {
        CloseoutInput {
            goal_mode: false,
            has_final_text: true,
            cancelled: false,
            can_continue: true,
            budget_remaining: CLOSEOUT_NUDGE_BUDGET,
            human_boundary_seen: false,
        }
    }

    #[test]
    fn human_denial_blocks_goal_unresolved_nudge() {
        let mut i = input();
        i.goal_mode = true;
        i.human_boundary_seen = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::Stall(CloseoutReason::GoalUnresolved),
            "goal + human denial + quiet must not buy another model round"
        );

        i.goal_mode = false;
        assert_eq!(
            decide(&i),
            CloseoutAction::Finish,
            "chat + human denial + quiet yields to the user"
        );
    }

    #[test]
    fn human_denial_still_allows_empty_answer_nudge() {
        let mut i = input();
        i.human_boundary_seen = true;
        i.has_final_text = false;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::EmptyAnswer)
        );
    }

    #[test]
    fn clean_quiet_round_finishes() {
        assert_eq!(decide(&input()), CloseoutAction::Finish);
    }

    #[test]
    fn empty_answer_outranks_everything() {
        // Even in goal mode, an empty answer means the model said nothing —
        // that repair outranks the goal-protocol one.
        let mut i = input();
        i.goal_mode = true;
        i.has_final_text = false;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::EmptyAnswer)
        );
    }

    #[test]
    fn cancelled_empty_answer_is_not_nudged() {
        let mut i = input();
        i.has_final_text = false;
        i.cancelled = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    #[test]
    fn goal_mode_quiet_is_goal_unresolved() {
        let mut i = input();
        i.goal_mode = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::GoalUnresolved)
        );
    }

    /// The budget is ONE. A second quiet round after the repair does not get a
    /// second reminder — it ends.
    #[test]
    fn the_repair_is_offered_once_per_turn() {
        assert_eq!(CLOSEOUT_NUDGE_BUDGET, 1);
        let mut i = input();
        i.goal_mode = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::GoalUnresolved)
        );
        i.budget_remaining = 0;
        assert_eq!(
            decide(&i),
            CloseoutAction::Stall(CloseoutReason::GoalUnresolved)
        );
    }

    #[test]
    fn exhausted_budget_stalls_goal_and_finishes_non_goal() {
        let mut i = input();
        i.budget_remaining = 0;

        i.goal_mode = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::Stall(CloseoutReason::GoalUnresolved)
        );

        let i = CloseoutInput {
            budget_remaining: 0,
            goal_mode: false,
            ..input()
        };
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    #[test]
    fn no_next_round_behaves_like_exhausted_budget() {
        let mut i = input();
        i.goal_mode = true;
        i.can_continue = false;
        assert_eq!(
            decide(&i),
            CloseoutAction::Stall(CloseoutReason::GoalUnresolved)
        );
    }

    #[test]
    fn stalled_detail_round_trips_the_reason() {
        let detail = stalled_detail(
            CloseoutReason::GoalUnresolved,
            "目标模式结束但未调用 update_goal(complete/blocked)",
        );
        assert!(detail.starts_with("closeout_reason=goal_unresolved; "));
        assert_eq!(
            reason_from_stalled_detail(&detail),
            Some(CloseoutReason::GoalUnresolved)
        );
        assert_eq!(reason_from_stalled_detail("no marker here"), None);
    }
}
