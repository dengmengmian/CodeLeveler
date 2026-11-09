//! Unified closeout decision point — the single place that decides what
//! happens when the model goes quiet (a round with no tool calls).
//!
//! Historically the quiet branch of `drive` chained four nudge mechanisms with
//! their own counters (3 + 2 + 2 + 1), each able to re-invoke the model. This
//! module is what is left: ONE decision per quiet round.
//!
//! Three mechanically different things happen here, and conflating them was a
//! bug:
//!
//! - **Turn finished with an answer.** The model produced final text in a
//!   non-Goal turn. That is an answer, and the turn ends [`CloseoutAction::Finish`].
//! - **Goal still active.** A Goal turn went quiet without calling
//!   `update_goal`. Nothing about that establishes completion, so the harness
//!   drives another round [`CloseoutAction::ContinueGoal`] with a lifecycle
//!   fact only. Past the no-progress bound it ends [`CloseoutAction::Stall`] —
//!   never as a success.
//! - **The model's own plan is still open.** A Chat turn that published an
//!   explicit plan and then went quiet has left its own durable declaration
//!   contradicting its answer: those steps are still `pending`/`in_progress`.
//!   The harness answers that contradiction with [`CloseoutAction::ContinuePlan`]
//!   — one lifecycle fact, never an instruction to do the steps — bounded by
//!   the same no-progress guard. The model decides whether the work continues,
//!   whether the plan no longer describes what is needed (it rewrites the
//!   plan), or what blocks it. Nothing here promotes the plan to a completion
//!   authority: a fully completed plan is not evidence either, and a turn that
//!   still answers with open steps ends as `Answered`, never as a success.
//!
//! A separate PROTOCOL repair covers an empty answer, and that one is budgeted:
//! the model said nothing at all, so one re-prompt is warranted and a second
//! would be coaching. Nothing here reads the work for meaning, and nothing here
//! decides the model deserves another turn.

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
    /// A Goal round went quiet without resolving the goal. The goal is still
    /// active, so the harness drives one more model round with a lifecycle
    /// fact ("goal remains active; keep working toward the original goal").
    ///
    /// This is NOT a protocol repair and does NOT spend the one-shot repair
    /// budget: the no-progress guard is its bound. A model that resumes real
    /// work resets that guard; a model that only re-emits a premature summary
    /// exhausts it and terminates as [`CloseoutAction::Stall`].
    ContinueGoal,
    /// A Chat turn went quiet while the model's own latest plan still listed
    /// steps it had not declared completed.
    ///
    /// Like [`Self::ContinueGoal`] this is a lifecycle fact, not a protocol
    /// repair: it does not spend the one-shot repair budget, it states the open
    /// steps and the legitimate ways to resolve them, and the no-progress guard
    /// is its bound. A model that resumes real work (including rewriting the
    /// plan) resets that guard; one that only re-emits the same summary
    /// exhausts it and the turn ends `Answered`.
    ContinuePlan,
    /// A Chat turn went quiet after its `commit_task` declaration was refused.
    ///
    /// Unlike the two continuations above this is not a lifecycle the turn is
    /// bound to — the user's request never became a goal. It is the model's own
    /// declaration that the request is work to carry to completion, which the
    /// harness refused only because the acceptance list did not cover the
    /// user's deliverables. Treating the refusal as "the task is over" would
    /// lose the contract silently, so the harness states the refusal once and
    /// lets the model continue, re-declare, or name the obstruction. Bounded by
    /// the same no-progress guard.
    ContinueTask,
    /// A resolution was warranted but no further round may be bought — the
    /// goal is out of continuations (no-progress bound), the round limit is
    /// spent, or the user denied a required permission. Goal mode terminates
    /// as `StopReason::Stalled`/`Blocked` with this reason recorded in the
    /// stop detail; non-goal turns treat this as `Finish`.
    Stall(CloseoutReason),
}

/// Why the harness wants to re-prompt the model once more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseoutReason {
    /// Goal mode: the model went quiet without calling `update_goal`. The goal
    /// is mechanically unresolved; what the model said is not a terminal.
    GoalUnresolved,
    /// Chat: the model's own explicit plan still lists steps it has not
    /// declared completed.
    PlanUnresolved,
    /// Chat: the model declared the request as a task to finish and the
    /// declaration was refused for missing acceptance coverage.
    TaskUnregistered,
    /// The model ended with an empty answer.
    EmptyAnswer,
    /// The goal is settled and the model has not written the person's
    /// delivery yet. Unlike [`Self::GoalUnresolved`] nothing is unresolved:
    /// the only missing thing is the user-facing answer.
    GoalSettledDelivery,
}

impl CloseoutReason {
    /// Stable machine key (used inside the Stalled stop detail so the engine
    /// can tell a continuation turn what the previous closeout stalled on).
    pub fn as_key(&self) -> &'static str {
        match self {
            Self::GoalUnresolved => "goal_unresolved",
            Self::PlanUnresolved => "plan_unresolved",
            Self::TaskUnregistered => "task_unregistered",
            Self::EmptyAnswer => "empty_answer",
            Self::GoalSettledDelivery => "goal_settled_delivery",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "goal_unresolved" => Some(Self::GoalUnresolved),
            "plan_unresolved" => Some(Self::PlanUnresolved),
            "task_unregistered" => Some(Self::TaskUnregistered),
            "empty_answer" => Some(Self::EmptyAnswer),
            "goal_settled_delivery" => Some(Self::GoalSettledDelivery),
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

/// Stable key identifying a closeout that ended at the user's own pause
/// boundary. It rides in the `Answered` stop detail so a durable replay, the
/// event bridge and the next window can all tell a user-authorized pause from
/// an ordinary answer without re-reading the transcript.
pub const USER_PAUSE_DETAIL_PREFIX: &str = "user_pause_boundary";

/// The stop detail of a turn that ended because the user paused the goal.
pub fn user_pause_detail() -> String {
    format!(
        "{USER_PAUSE_DETAIL_PREFIX}: 用户已要求在后续阶段前暂停，目标仍未完成，等待用户授权继续"
    )
}

/// Whether a terminal detail states the user's pause boundary.
pub fn is_user_pause_detail(detail: &str) -> bool {
    detail.trim_start().starts_with(USER_PAUSE_DETAIL_PREFIX)
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
    /// The consecutive no-progress bound is already reached, so no further
    /// continuation may be bought. Mechanical and model-independent — it counts
    /// consecutive quiet/refused rounds, not how the work looks.
    pub continuations_exhausted: bool,
    /// How many steps of the model's OWN latest explicit plan are not marked
    /// `completed`. `0` when there is no model-declared plan (an empty plan, or
    /// a host-implicit one the model never wrote).
    ///
    /// This is the model's durable declaration, not a judgement of the work:
    /// it says the model left its own record open, nothing about whether those
    /// steps are still needed.
    pub open_plan_steps: usize,
    /// Whether the model's `commit_task` declaration was refused this turn.
    /// A refused declaration is the model's own statement that this request is
    /// work to finish, so a quiet round after it is the same class of
    /// contradiction as an open plan.
    pub registration_refused: bool,
    /// The model's own latest plan has reached the [`leveler_lifecycle::PauseBoundary`]
    /// the user authorized through a validated intent declaration.
    ///
    /// This is an authorization fact read from the durable boundary and the
    /// model's plan, never a model claim. When true, the harness must not buy
    /// another round to pressure past the pause: the turn ends normally with
    /// the remaining steps still recorded as open.
    pub user_pause_reached: bool,
    /// The bound goal is durably paused awaiting the user's continuation
    /// authorization, and this turn's instruction did not release it.
    ///
    /// A paused Goal still owes its work, but the user withheld authorization
    /// to continue it. The harness states that fact and ends the turn instead
    /// of driving `update_goal` again; only the user's own continuation
    /// releases the pause. This is the cross-turn half of
    /// [`Self::user_pause_reached`], which can only be read while the model's
    /// plan is in front of it.
    pub goal_user_paused: bool,
}

/// Decide the fate of one quiet round. At most one action per round, chosen by
/// priority: EmptyAnswer > GoalUnresolved > PlanUnresolved
/// (an empty answer means the model said nothing at all, so it outranks every
/// other prompt; a Goal outranks a plan step list because the goal is the
/// lifecycle the turn is bound to).
///
/// `has_final_text` alone never resolves a Goal — a Goal only ends through an
/// explicit `update_goal` terminal or a mechanical bound. The outcomes here
/// are: drive another round ([`CloseoutAction::ContinueGoal`] /
/// [`CloseoutAction::ContinuePlan`]), give up honestly ([`CloseoutAction::Stall`],
/// which the drive reports as `StopReason::Stalled`), or yield to the user's
/// denial.
pub fn decide(input: &CloseoutInput) -> CloseoutAction {
    let candidate = if !input.has_final_text && !input.cancelled {
        Some(CloseoutReason::EmptyAnswer)
    } else if input.goal_mode {
        Some(CloseoutReason::GoalUnresolved)
    } else if input.open_plan_steps > 0 {
        Some(CloseoutReason::PlanUnresolved)
    } else if input.registration_refused {
        Some(CloseoutReason::TaskUnregistered)
    } else {
        None
    };
    let Some(reason) = candidate else {
        return CloseoutAction::Finish;
    };
    // Human denial is authoritative: the model may adapt on its own next
    // tool-using round, but the harness must not inject more lifecycle
    // pressure after the user said no. Empty-answer still gets one chance
    // (the model said nothing at all).
    if input.human_boundary_seen && reason != CloseoutReason::EmptyAnswer {
        return if input.goal_mode {
            CloseoutAction::Stall(reason)
        } else {
            CloseoutAction::Finish
        };
    }
    // The user's own explicit pause boundary outranks the open-plan and
    // refused-registration continuations. The plan is still open and the
    // refusal still happened, but the authorized work ended where the user
    // said it ended; another round would pressure the model past that
    // boundary. Goal mode is included on purpose: a Goal is a lifecycle the
    // turn is bound to, but it is still the USER's goal, and a pause they set
    // outranks the harness's drive to resolve it. The goal stays running and
    // recoverable; only the user's own continuation releases the pause.
    // An empty answer is a protocol defect, not a decision to pressure past a
    // boundary: it keeps its one repair, like it does after a human denial.
    if reason != CloseoutReason::EmptyAnswer
        && (input.user_pause_reached || (input.goal_mode && input.goal_user_paused))
    {
        return CloseoutAction::Finish;
    }
    // A Goal quiet round is a lifecycle continuation, not a protocol repair:
    // the goal is still active and the only fact the harness establishes is
    // that the model stopped talking. It gets another round while the
    // no-progress guard allows; past that it stalls (never a success).
    if reason == CloseoutReason::GoalUnresolved {
        return if input.can_continue && !input.continuations_exhausted {
            CloseoutAction::ContinueGoal
        } else {
            CloseoutAction::Stall(reason)
        };
    }
    // A plan the model published and then left open is the same class of fact
    // one level down: nothing resolves it by ending the turn, and the model is
    // the only one who can say whether a step is still owed. Bounded by the
    // same no-progress guard, so a model that keeps re-emitting the same
    // answer ends the turn instead of looping.
    if reason == CloseoutReason::PlanUnresolved {
        // A cancelled turn is the user's decision, not a quiet stop: no extra
        // round may be bought for it.
        return if input.can_continue && !input.continuations_exhausted && !input.cancelled {
            CloseoutAction::ContinuePlan
        } else {
            CloseoutAction::Finish
        };
    }
    // A refused registration is the model's own declaration that the user's
    // request is work to carry to completion. The refusal was about the
    // acceptance list, so it is neither a completion nor a failure: the same
    // bounded continuation applies, and the model may finish without a goal.
    if reason == CloseoutReason::TaskUnregistered {
        return if input.can_continue && !input.continuations_exhausted && !input.cancelled {
            CloseoutAction::ContinueTask
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
            continuations_exhausted: false,
            open_plan_steps: 0,
            registration_refused: false,
            user_pause_reached: false,
            goal_user_paused: false,
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

    /// A stale plan is an observation, not a closeout gate: a chat turn with
    /// real work after its latest plan declaration still ends `Finish`.
    #[test]
    fn a_stale_plan_never_forces_a_chat_closeout() {
        let i = input();
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    /// The other half of the same rule: a plan the model left OPEN is not
    /// stale-ness, it is its own durable record contradicting the answer.
    #[test]
    fn an_open_model_plan_buys_one_more_round_in_chat() {
        let mut i = input();
        i.open_plan_steps = 2;
        assert_eq!(decide(&i), CloseoutAction::ContinuePlan);

        // No next round / exhausted progress bound: end the turn honestly.
        i.can_continue = false;
        assert_eq!(decide(&i), CloseoutAction::Finish);
        i.can_continue = true;
        i.continuations_exhausted = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    /// A refused task registration is not a completion: the user's request
    /// never became a goal, and the refusal was about the model's acceptance
    /// list. The turn gets one bounded continuation, then ends.
    #[test]
    fn a_refused_registration_buys_one_more_round_in_chat() {
        let mut i = input();
        i.registration_refused = true;
        assert_eq!(decide(&i), CloseoutAction::ContinueTask);

        i.can_continue = false;
        assert_eq!(decide(&i), CloseoutAction::Finish);
        i.can_continue = true;
        i.continuations_exhausted = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);
        i.continuations_exhausted = false;
        i.cancelled = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);

        // A user denial is authoritative: no extra round over the boundary.
        i.cancelled = false;
        i.human_boundary_seen = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    /// An open plan is the more actionable fact: it names the model's own
    /// steps, so it outranks a refused declaration.
    #[test]
    fn an_open_plan_outranks_a_refused_registration() {
        let mut i = input();
        i.open_plan_steps = 1;
        i.registration_refused = true;
        assert_eq!(decide(&i), CloseoutAction::ContinuePlan);
    }

    /// A user pause boundary is authoritative for the plan/task continuations:
    /// the plan stays open on the record, but the harness buys no round to
    /// pressure past a boundary the person set.
    #[test]
    fn a_reached_user_pause_ends_the_plan_continuation() {
        let mut i = input();
        i.open_plan_steps = 2;
        i.user_pause_reached = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);

        let mut i = input();
        i.registration_refused = true;
        i.user_pause_reached = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    /// A user pause is authority over BOTH lifecycles the turn could be bound
    /// to. A paused Goal stays owed — but the harness does not buy a round to
    /// pressure past the pause. An empty answer is still a protocol defect and
    /// gets its one repair.
    #[test]
    fn a_user_pause_ends_the_goal_continuation_too() {
        let mut i = input();
        i.goal_mode = true;
        i.user_pause_reached = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);

        let mut i = input();
        i.goal_mode = true;
        i.goal_user_paused = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);

        let mut i = input();
        i.goal_mode = true;
        i.has_final_text = false;
        i.user_pause_reached = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::EmptyAnswer)
        );
    }

    /// The durable pause covers a later turn that has no plan to prove the
    /// boundary (a status question after the paused window). Without it, the
    /// goal continuation would pressure the model into the withheld stage.
    #[test]
    fn a_durable_pause_survives_a_turn_without_a_plan() {
        let mut i = input();
        i.goal_mode = true;
        i.open_plan_steps = 0;
        i.goal_user_paused = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);

        // The same inactive goal is a normal continuation when nothing paused it.
        i.goal_user_paused = false;
        assert_eq!(decide(&i), CloseoutAction::ContinueGoal);
    }

    /// A pause that has not been reached (stage 1 still in progress) is not a
    /// pause yet: the normal continuation still applies.
    #[test]
    fn an_unreached_user_pause_keeps_the_plan_continuation() {
        let mut i = input();
        i.open_plan_steps = 2;
        i.user_pause_reached = false;
        assert_eq!(decide(&i), CloseoutAction::ContinuePlan);
    }

    /// The existing D5 boundary: the plan continuation is not a protocol repair,
    /// so the one-shot repair budget cannot swallow it — and it cannot spend it either.
    #[test]
    fn the_repair_budget_does_not_gate_the_plan_continuation() {
        let mut i = input();
        i.budget_remaining = 0;
        i.open_plan_steps = 1;
        assert_eq!(decide(&i), CloseoutAction::ContinuePlan);
    }

    /// A Goal outranks a plan list: the turn is bound to a goal lifecycle, and
    /// that is the terminal it must resolve. The plan adds nothing there.
    #[test]
    fn a_goal_quiet_round_is_still_a_goal_continuation() {
        let mut i = input();
        i.goal_mode = true;
        i.open_plan_steps = 3;
        assert_eq!(decide(&i), CloseoutAction::ContinueGoal);
    }

    /// Cancelling is the user's decision: it is not a quiet round to answer.
    #[test]
    fn a_cancelled_turn_never_buys_a_plan_continuation() {
        let mut i = input();
        i.open_plan_steps = 2;
        i.cancelled = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    /// The user's denial outranks the plan fact: the harness must not pressure
    /// past a boundary the person just set.
    #[test]
    fn human_denial_silences_the_plan_continuation() {
        let mut i = input();
        i.open_plan_steps = 4;
        i.human_boundary_seen = true;
        assert_eq!(decide(&i), CloseoutAction::Finish);
    }

    /// An empty answer outranks the plan fact: the model said nothing, so the
    /// protocol repair comes first.
    #[test]
    fn an_empty_answer_outranks_an_open_plan() {
        let mut i = input();
        i.has_final_text = false;
        i.open_plan_steps = 2;
        assert_eq!(
            decide(&i),
            CloseoutAction::NudgeOnce(CloseoutReason::EmptyAnswer)
        );
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
    fn goal_mode_quiet_continues_the_goal() {
        let mut i = input();
        i.goal_mode = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::ContinueGoal,
            "a Goal that went quiet without resolving is still active"
        );
    }

    /// The continuation is bounded by the no-progress guard, not by the
    /// one-shot protocol-repair budget: a model that resumes real work resets
    /// the guard, while a model that only re-emits a premature summary does
    /// not.
    #[test]
    fn the_repair_budget_does_not_gate_goal_continuation() {
        assert_eq!(CLOSEOUT_NUDGE_BUDGET, 1);
        let mut i = input();
        i.goal_mode = true;
        i.budget_remaining = 0;
        assert_eq!(decide(&i), CloseoutAction::ContinueGoal);
    }

    #[test]
    fn exhausted_no_progress_stalls_goal_and_finishes_non_goal() {
        let mut i = input();
        i.continuations_exhausted = true;

        i.goal_mode = true;
        assert_eq!(
            decide(&i),
            CloseoutAction::Stall(CloseoutReason::GoalUnresolved),
            "past the no-progress bound the goal must not buy another round"
        );

        let i = CloseoutInput {
            continuations_exhausted: true,
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

    /// The plan reason has its own stable key so a persisted injection and the
    /// TUI label cannot drift apart.
    #[test]
    fn the_plan_reason_round_trips_its_key() {
        assert_eq!(CloseoutReason::PlanUnresolved.as_key(), "plan_unresolved");
        assert_eq!(
            CloseoutReason::from_key("plan_unresolved"),
            Some(CloseoutReason::PlanUnresolved)
        );
    }
}
