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
    /// A build-relevant mutation has no fresh passing verification behind it.
    MissingEvidence,
    /// The model ended with an empty answer.
    EmptyAnswer,
    /// The completeness audit named branches the answer skipped.
    AnswerIncomplete,
}

impl CloseoutReason {
    /// Stable machine key (used inside the Stalled stop detail so the engine
    /// can tell a continuation turn what the previous closeout stalled on).
    pub fn as_key(&self) -> &'static str {
        match self {
            Self::GoalUnresolved => "goal_unresolved",
            Self::MissingEvidence => "missing_evidence",
            Self::EmptyAnswer => "empty_answer",
            Self::AnswerIncomplete => "answer_incomplete",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "goal_unresolved" => Some(Self::GoalUnresolved),
            "missing_evidence" => Some(Self::MissingEvidence),
            "empty_answer" => Some(Self::EmptyAnswer),
            "answer_incomplete" => Some(Self::AnswerIncomplete),
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
    /// The completion-evidence gate is assembled for this turn.
    pub require_completion_evidence: bool,
    /// What the turn touched and whether it is evidence-backed yet.
    pub impact: &'a ChangeImpact,
    /// Real progress happened since the last evidence nudge (re-arm guard, so
    /// a model that idles after being nudged is not nudged again for free).
    pub progress_since_evidence_nudge: bool,
    /// This round's completeness-audit verdict (false when the audit did not
    /// run, passed, or was unavailable).
    pub audit_incomplete: bool,
    /// The turn is being cancelled — an empty answer is not worth a nudge.
    pub cancelled: bool,
    /// The continuation policy allows at least one more model round.
    pub can_continue: bool,
    /// Nudges left in the shared per-turn budget.
    pub budget_remaining: u8,
}

/// Decide the fate of one quiet round. At most one nudge per round, chosen by
/// priority: EmptyAnswer > GoalUnresolved > MissingEvidence > AnswerIncomplete
/// (an empty answer means the model said nothing at all, so it outranks even
/// the goal-mode prompt; the audit is advisory and always comes last).
pub fn decide(input: &CloseoutInput) -> CloseoutAction {
    let candidate = if !input.has_final_text && !input.cancelled {
        Some(CloseoutReason::EmptyAnswer)
    } else if input.goal_mode && (!input.impact.has_mutation || input.impact.build_relevant) {
        // One carve-out: a goal turn whose only changes are inert (docs, config
        // text — the same `build_relevant` line the evidence gate uses) and that
        // produced a real answer is done. `update_goal` is bookkeeping, not
        // evidence of unfinished work; nudging such a turn only makes the model
        // repeat its summary, and once the budget is spent the Stalled result
        // drags the engine into a continuation turn — an invisible extra turn
        // after a visible answer.
        //
        // A turn that changed NOTHING keeps nudging: that is the long-running
        // goal still thinking out loud, not a finished one.
        Some(CloseoutReason::GoalUnresolved)
    } else if input.require_completion_evidence
        && input.impact.has_mutation
        && input.impact.build_relevant
        && !input.impact.verified_after_last_mutation
        && input.progress_since_evidence_nudge
    {
        Some(CloseoutReason::MissingEvidence)
    } else if input.audit_incomplete {
        Some(CloseoutReason::AnswerIncomplete)
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
            require_completion_evidence: false,
            impact,
            progress_since_evidence_nudge: true,
            audit_incomplete: false,
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
        // Even in goal mode with missing evidence, an empty answer means the
        // model said nothing — that nudge comes first.
        let impact = relevant_unverified_impact();
        let mut i = input(&impact);
        i.goal_mode = true;
        i.require_completion_evidence = true;
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
    fn goal_mode_inert_change_with_an_answer_finishes() {
        // Docs-only edits take the same path as the evidence gate: inert work
        // plus a real answer is a finished turn. Re-prompting it only makes the
        // model repeat its summary and then drags the engine into an invisible
        // continuation turn.
        let inert = inert_impact();
        let mut i = input(&inert);
        i.goal_mode = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    #[test]
    fn goal_mode_with_no_change_at_all_still_nudges() {
        // A goal turn that touched nothing is the long-running goal thinking
        // out loud — it must keep its nudge and its engine continuation. The
        // inert carve-out above must not swallow this case.
        let quiet = quiet_impact();
        let mut i = input(&quiet);
        i.goal_mode = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::GoalUnresolved)
        );
    }

    #[test]
    fn goal_mode_empty_answer_still_nudges_even_when_inert() {
        // The relaxation is keyed on a real answer. Nothing said at all is
        // still worth one nudge (and EmptyAnswer outranks GoalUnresolved).
        let impact = inert_impact();
        let mut i = input(&impact);
        i.goal_mode = true;
        i.has_final_text = false;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::EmptyAnswer)
        );
    }

    #[test]
    fn goal_mode_build_relevant_change_still_nudges() {
        // Touching code without resolving the goal is the case GoalUnresolved
        // exists for — the relaxation must not swallow it.
        let impact = relevant_unverified_impact();
        let mut i = input(&impact);
        i.goal_mode = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::GoalUnresolved)
        );
    }

    #[test]
    fn conversational_goal_turn_finishes_on_a_real_answer() {
        // Step 2 leaves the evidence gate and audit off for conversational
        // tasks; with the goal nudge now keyed on build-relevant work, a
        // conversational goal turn that answered has no nudge left at all.
        let impact = inert_impact();
        let mut i = input(&impact);
        i.goal_mode = true;
        i.require_completion_evidence = false;
        // The audit never runs in goal mode (drive gates it on `!goal_mode`),
        // so `audit_incomplete` stays false here by construction.
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    #[test]
    fn missing_evidence_needs_relevant_unverified_mutation() {
        let impact = relevant_unverified_impact();
        let mut i = input(&impact);
        i.require_completion_evidence = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::MissingEvidence)
        );

        // Gate off → no nudge.
        i.require_completion_evidence = false;
        assert_eq!(decide(&i), CloseoutAction::Finish);

        // Inert change (docs only) → no evidence demanded, same answer as the
        // readiness gate gives.
        let inert = inert_impact();
        let mut i = input(&inert);
        i.require_completion_evidence = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);

        // Fresh verification after the last mutation → satisfied.
        let mut verified = relevant_unverified_impact();
        verified.verified_after_last_mutation = true;
        let mut i = input(&verified);
        i.require_completion_evidence = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    #[test]
    fn evidence_nudge_rearms_only_after_progress() {
        let impact = relevant_unverified_impact();
        let mut i = input(&impact);
        i.require_completion_evidence = true;
        i.progress_since_evidence_nudge = false;
        // Blocked by the re-arm guard: no candidate (and no audit verdict)
        // means Finish, even with budget left.
        assert_eq!(decide(&i), CloseoutAction::Finish);
        // …but a pending audit verdict still gets its say behind the guard.
        i.audit_incomplete = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::AnswerIncomplete)
        );
    }

    #[test]
    fn priority_goal_over_evidence_over_audit() {
        let impact = relevant_unverified_impact();
        let mut i = input(&impact);
        i.require_completion_evidence = true;
        i.audit_incomplete = true;
        // Non-goal: evidence beats audit.
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::MissingEvidence)
        );
        // Goal: goal beats both (audit is not even assembled in goal mode,
        // but the priority holds regardless).
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

        i.goal_mode = false;
        i.require_completion_evidence = true;
        let impact = relevant_unverified_impact();
        let mut i = CloseoutInput {
            budget_remaining: 0,
            ..input(&impact)
        };
        i.require_completion_evidence = true;
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
    fn budget_is_shared_across_mechanisms() {
        let mut budget = CloseoutBudget::new(CLOSEOUT_NUDGE_BUDGET);
        let impact = relevant_unverified_impact();

        // Nudge 1: missing evidence.
        let mut i = input(&impact);
        i.require_completion_evidence = true;
        i.budget_remaining = budget.remaining();
        assert!(matches!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::MissingEvidence)
        ));
        budget.consume();

        // Nudge 2: an audit repair draws from the SAME budget.
        let impact = quiet_impact();
        let mut i = input(&impact);
        i.audit_incomplete = true;
        i.budget_remaining = budget.remaining();
        assert!(matches!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::AnswerIncomplete)
        ));
        budget.consume();

        // Nudge 3 would be goal resolution — budget is spent, so a goal turn
        // stalls instead of nudging a third mechanism.
        let mut i = input(&impact);
        i.goal_mode = true;
        i.budget_remaining = budget.remaining();
        assert_eq!(
            decide(&i),
            CloseoutAction::Stall(CloseoutReason::GoalUnresolved)
        );
    }

    #[test]
    fn stalled_detail_round_trips_the_reason() {
        let detail = stalled_detail(
            CloseoutReason::MissingEvidence,
            "目标模式结束但未调用 update_goal(complete/blocked)",
        );
        assert!(detail.starts_with("closeout_reason=missing_evidence; "));
        assert_eq!(
            reason_from_stalled_detail(&detail),
            Some(CloseoutReason::MissingEvidence)
        );
        assert_eq!(reason_from_stalled_detail("no marker here"), None);
    }
}
