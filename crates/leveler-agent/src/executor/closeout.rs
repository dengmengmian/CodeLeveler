//! Unified closeout decision point — the single place that decides what
//! happens when the model goes quiet (a round with no tool calls).
//!
//! Historically the quiet branch of `drive` chained three independent nudge
//! mechanisms (goal resolution, completion evidence, answer-completeness
//! repair) plus a one-shot empty-answer nudge, each with its own counter
//! (3 + 2 + 2 + 1). Every one of them could re-invoke the model, and each
//! re-invocation let the model repeat its "task complete" summary. This
//! module replaces all of them with ONE decision and ONE shared per-turn
//! budget: a quiet round produces at most one nudge, chosen by a fixed
//! priority, and the whole turn allows at most [`CLOSEOUT_NUDGE_BUDGET`]
//! nudges across all mechanisms combined.

use leveler_lifecycle::ChangeImpact;

/// Total nudges one turn may inject across ALL closeout mechanisms.
pub const CLOSEOUT_NUDGE_BUDGET: u8 = 2;

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
#[derive(Debug, Clone, Copy)]
pub struct CloseoutInput<'a> {
    /// Goal mode: quiet never means done; the model must call `update_goal`.
    pub goal_mode: bool,
    /// The model produced non-empty final text this turn.
    pub has_final_text: bool,
    /// What the turn touched and whether it is evidence-backed yet.
    pub impact: &'a ChangeImpact,
    /// The turn is being cancelled — an empty answer is not worth a nudge.
    pub cancelled: bool,
    /// The continuation policy allows at least one more model round.
    pub can_continue: bool,
    /// Nudges left in the shared per-turn budget.
    pub budget_remaining: u8,
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

    fn inert_impact() -> ChangeImpact {
        ChangeImpact {
            modified_files: vec!["README.md".to_string()],
            has_mutation: true,
            verified_after_last_mutation: false,
            build_relevant: false,
        }
    }

    fn relevant_unverified_impact() -> ChangeImpact {
        ChangeImpact {
            modified_files: vec!["src/lib.rs".to_string()],
            has_mutation: true,
            verified_after_last_mutation: false,
            build_relevant: true,
        }
    }

    fn quiet_impact() -> ChangeImpact {
        ChangeImpact {
            modified_files: Vec::new(),
            has_mutation: false,
            verified_after_last_mutation: false,
            build_relevant: false,
        }
    }

    fn input<'a>(impact: &'a ChangeImpact) -> CloseoutInput<'a> {
        CloseoutInput {
            goal_mode: false,
            has_final_text: true,
            impact,
            cancelled: false,
            can_continue: true,
            budget_remaining: CLOSEOUT_NUDGE_BUDGET,
        }
    }

    #[test]
    fn clean_quiet_round_finishes() {
        let impact = quiet_impact();
        assert_eq!(decide(&input(&impact)), CloseoutAction::Finish);
    }

    #[test]
    fn empty_answer_outranks_everything() {
        // Even in goal mode, an empty answer means the model said nothing —
        // that nudge outranks everything else.
        let impact = relevant_unverified_impact();
        let mut i = input(&impact);
        i.goal_mode = true;
        i.has_final_text = false;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::EmptyAnswer)
        );
    }

    #[test]
    fn cancelled_empty_answer_is_not_nudged() {
        let impact = quiet_impact();
        let mut i = input(&impact);
        i.has_final_text = false;
        i.cancelled = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    #[test]
    fn goal_mode_quiet_is_goal_unresolved() {
        let impact = quiet_impact();
        let mut i = input(&impact);
        i.goal_mode = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::GoalUnresolved)
        );
    }

    #[test]
    fn conversational_goal_turn_only_has_goal_nudge() {
        // GoalUnresolved is the only nudge a goal turn with a real answer can
        // draw.
        let impact = inert_impact();
        let mut i = input(&impact);
        i.goal_mode = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::GoalUnresolved)
        );
    }

    #[test]
    fn exhausted_budget_stalls_goal_and_finishes_non_goal() {
        let impact = quiet_impact();
        let mut i = input(&impact);
        i.budget_remaining = 0;

        i.goal_mode = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::Stall(CloseoutReason::GoalUnresolved)
        );

        let impact = relevant_unverified_impact();
        let i = CloseoutInput {
            budget_remaining: 0,
            goal_mode: false,
            ..input(&impact)
        };
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    #[test]
    fn no_next_round_behaves_like_exhausted_budget() {
        let impact = quiet_impact();
        let mut i = input(&impact);
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
