//! Mechanical limits on a run and the decision whether another model step
//! may start.
//!
//! A hard limit is hard: nothing here decides that a run deserves more. The
//! predicates and their order are the loop's own; naming them gives one place
//! to ask, one input type to extend, and one thing to test.
//!
//! # Two kinds of step bound, and the one thing neither of them is
//!
//! A **model step** is one admitted loop iteration: one logical model request
//! (its provider retries included) plus the tool batch it produced. It is a
//! property of the model's request cadence.
//!
//! - [`ModelStepLimits::model_step_ceiling`] is the **safety breaker**: an
//!   unconditional bound that only exists so a runaway model↔tool loop cannot
//!   spin forever. It must sit far outside the normal operating range.
//! - [`ModelStepLimits::model_step_window_limit`] is a **deliberately bounded
//!   unit of work**: a host that owns a piece of delegated work (an eval case,
//!   a sub-agent's manifest budget) asked for a hard edge.
//!
//! Neither is a task budget. A step count says nothing about how much of the
//! user's task is left, so it can never be what decides that a task is over.
//! Task lifetime belongs to the resource limits below (
//! [`ModelStepLimits::max_model_tokens`], [`ModelStepLimits::max_cost_usd_micros`],
//! [`ModelStepLimits::max_duration`]) and to the host's own semantic lifecycle
//! (goal complete / blocked, user cancel).

use std::time::Duration;

/// The default model-step safety ceiling, applied when a host pins none — the
/// unconditional circuit breaker that guarantees an unbounded run terminates.
///
/// Chosen to sit far outside the normal operating range rather than to bound
/// useful work: measured task runs close in 9–25 model steps, so this is an
/// order of magnitude of headroom. It deliberately matches the previous
/// effective production default so that separating the two duties never
/// tightened a bound.
pub const DEFAULT_MODEL_STEP_CEILING: u32 = 200;

/// The safety ceiling must stay far outside the range real tasks close in
/// (measured: 9–25 model steps). A ceiling inside that range would be a task
/// budget wearing a circuit breaker's name — exactly what this constant exists
/// to prevent — so it is a build-time invariant, not a comment.
const _: () = assert!(DEFAULT_MODEL_STEP_CEILING >= 100);

/// Legacy spelling of [`DEFAULT_MODEL_STEP_CEILING`].
///
/// Kept so an out-of-workspace caller still compiles; new code should use the
/// model-step name. Same value, one source of truth.
pub const DEFAULT_ROUND_CEILING: u32 = DEFAULT_MODEL_STEP_CEILING;

/// Which resource limit terminated a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDimension {
    ModelTokens,
    Cost,
    Duration,
    Commands,
    ModifiedFiles,
}

impl BudgetDimension {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelTokens => "model_tokens",
            Self::Cost => "cost",
            Self::Duration => "duration",
            Self::Commands => "commands",
            Self::ModifiedFiles => "modified_files",
        }
    }
}

/// Structured budget-exhaust facts: which dimension fired, spent vs cap.
///
/// `spent` / `cap` units:
/// - model tokens: total provider (or estimated) tokens
/// - cost: micro-USD
/// - duration: milliseconds of wall clock
/// - commands / modified files: counts
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetExhaustion {
    pub dimension: BudgetDimension,
    pub spent: u64,
    pub cap: u64,
}

impl BudgetExhaustion {
    pub fn new(dimension: BudgetDimension, spent: u64, cap: u64) -> Self {
        Self {
            dimension,
            spent,
            cap,
        }
    }

    /// Parseable stop-detail contract used by logs and older consumers.
    /// Format: `budget_exhausted dimension=<name> spent=<n> cap=<n>`.
    pub fn stop_detail(&self) -> String {
        format!(
            "budget_exhausted dimension={} spent={} cap={}",
            self.dimension.as_str(),
            self.spent,
            self.cap
        )
    }
}

/// Spend a host already made against the same limits before this run began
/// (a continuation of an earlier window). The loop measures every cap against
/// `spent_before + this run`, so the limits stay task-level, not per-run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpentBefore {
    pub model_tokens: u64,
    pub cost_usd_micros: u64,
    pub duration: Duration,
}

/// Every mechanical bound the loop enforces on its own.
///
/// **Semantics:** `None` = unlimited; `Some(0)` = hard exhausted (no further
/// spend allowed); `Some(n)` = the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelStepLimits {
    /// Model-step safety ceiling for this run. Fires regardless of progress;
    /// the host cannot lift it mid-run. This is a circuit breaker, never a
    /// task budget.
    ///
    /// `None` means no step count ends this run on its own — reserved for a
    /// run whose lifetime is defined semantically (a top-level interactive
    /// turn). Such a run is still bounded by every other mechanical guard:
    /// cancellation, the token/cost/duration budgets, and the loop's own
    /// repeated-call and no-progress watchdogs.
    pub model_step_ceiling: Option<u32>,
    /// A host-pinned window of model steps: the hard edge of a deliberately
    /// bounded unit of work the host owns (an eval case, a sub-agent's
    /// manifest budget). Reaching it ends the run normally
    /// (`StopReason::ModelStepWindowLimit`), not as a resource-budget stop.
    pub model_step_window_limit: Option<u32>,
    /// Max provider-reported (or estimated) input + output tokens.
    pub max_model_tokens: Option<u64>,
    /// Max auditable model cost in micro-USD. Requires pricing on the agent;
    /// the loop refuses to start with a cost cap it cannot measure.
    pub max_cost_usd_micros: Option<u64>,
    /// Max wall-clock duration. The loop arms a deadline timer that cancels
    /// in-flight work through the run's cancellation token.
    pub max_duration: Option<Duration>,
    /// Wall-clock point (measured on the same axis as `max_duration`, prior
    /// spend included) at which the run is told to stop expanding and turn
    /// what it has into a final result. This is a request, not a stop: the
    /// run keeps working, and `max_duration` above remains the only bound
    /// that cancels it. `None` skips the finalization request; a point at or
    /// beyond `max_duration` is ignored, so the soft deadline can never
    /// extend a run.
    pub finalize_at: Option<Duration>,
    /// Spend already made against these caps before this run.
    pub spent_before: SpentBefore,
}

impl Default for ModelStepLimits {
    fn default() -> Self {
        Self {
            model_step_ceiling: Some(DEFAULT_MODEL_STEP_CEILING),
            model_step_window_limit: None,
            max_model_tokens: None,
            max_cost_usd_micros: None,
            max_duration: None,
            finalize_at: None,
            spent_before: SpentBefore::default(),
        }
    }
}

impl ModelStepLimits {
    /// Whether the run may start another model step after `model_steps`
    /// completed steps, as far as the pinned window limit is concerned.
    pub fn allows_model_step_after(&self, model_steps: u32) -> bool {
        self.model_step_window_limit
            .is_none_or(|max| model_steps < max)
    }
}

/// Everything the model-step admission decision reads. All of it already
/// exists; nothing here is derived for the purpose.
#[derive(Debug, Clone, Copy)]
pub struct ModelStepAdmissionInput {
    /// Model steps completed so far in this run.
    pub model_steps: u32,
    /// The model-step safety ceiling, when the host pins one.
    pub model_step_ceiling: Option<u32>,
    /// The run's pinned window of model steps, when one is pinned.
    pub model_step_window_limit: Option<u32>,
    /// Tokens spent against the cap (prior spend included), and the cap.
    pub model_tokens_spent: u64,
    pub max_model_tokens: Option<u64>,
    /// Cost spent against the cap in micro-USD, and the cap.
    pub cost_spent_micros: u64,
    pub max_cost_usd_micros: Option<u64>,
    /// Wall clock consumed against the cap, and the cap.
    pub elapsed: Duration,
    pub max_duration: Option<Duration>,
    /// The run was cancelled from outside — as opposed to the deadline timer
    /// cancelling it, which is a budget stop and reports as one.
    pub cancelled: bool,
    pub deadline_expired: bool,
}

/// The verdict. Each non-admitting arm names what the loop does for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStepAdmission {
    /// Spend another model call.
    Admit,
    /// A resource cap is spent, with the dimension that fired. This is the
    /// only arm that reports a task-level budget.
    StopBudget(BudgetExhaustion),
    /// The model-step safety ceiling was reached. Not a budget: a circuit
    /// breaker that fires regardless of progress.
    StopModelStepCeiling { ceiling: u32 },
    /// The host's pinned window of model steps is used up. The bounded unit
    /// of work the host asked for ended normally; the host decides whether
    /// another run opens.
    StopModelStepWindowLimit,
    /// Cancelled from outside the run.
    Cancelled,
}

/// Admit — or refuse — the next model call.
///
/// Order matters: token cap, cost cap, model-step ceiling, model-step window,
/// cancellation, duration cap. The first four are checked against the step
/// just completed; the last two against the step about to start, which is
/// why `elapsed` is read after the model-step counter advances.
pub fn admit_next_model_step(input: &ModelStepAdmissionInput) -> ModelStepAdmission {
    if let Some(max) = input.max_model_tokens
        && input.model_tokens_spent >= max
    {
        return ModelStepAdmission::StopBudget(BudgetExhaustion::new(
            BudgetDimension::ModelTokens,
            input.model_tokens_spent,
            max,
        ));
    }
    if let Some(max) = input.max_cost_usd_micros
        && input.cost_spent_micros >= max
    {
        return ModelStepAdmission::StopBudget(BudgetExhaustion::new(
            BudgetDimension::Cost,
            input.cost_spent_micros,
            max,
        ));
    }
    if let Some(ceiling) = input.model_step_ceiling
        && input.model_steps >= ceiling
    {
        return ModelStepAdmission::StopModelStepCeiling { ceiling };
    }
    if input
        .model_step_window_limit
        .is_some_and(|max| input.model_steps >= max)
    {
        return ModelStepAdmission::StopModelStepWindowLimit;
    }
    // An external cancel ends the run as cancelled; the deadline timer
    // cancels through the same token but must report as the duration budget,
    // so it falls through to the duration check below.
    if input.cancelled && !input.deadline_expired {
        return ModelStepAdmission::Cancelled;
    }
    if let Some(max) = input.max_duration {
        // `Some(0)` is a hard-exhausted residual handed to a child; for a
        // positive cap the step may start only while still inside it.
        if max.is_zero() || input.elapsed > max {
            return ModelStepAdmission::StopBudget(BudgetExhaustion::new(
                BudgetDimension::Duration,
                input.elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
                max.as_millis().min(u128::from(u64::MAX)) as u64,
            ));
        }
    }
    ModelStepAdmission::Admit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_detail_is_parseable() {
        let e = BudgetExhaustion::new(BudgetDimension::Commands, 10, 10);
        assert_eq!(
            e.stop_detail(),
            "budget_exhausted dimension=commands spent=10 cap=10"
        );
    }

    fn open() -> ModelStepAdmissionInput {
        ModelStepAdmissionInput {
            model_steps: 1,
            model_step_ceiling: Some(100),
            model_step_window_limit: None,
            model_tokens_spent: 0,
            max_model_tokens: None,
            cost_spent_micros: 0,
            max_cost_usd_micros: None,
            elapsed: Duration::ZERO,
            max_duration: None,
            cancelled: false,
            deadline_expired: false,
        }
    }

    #[test]
    fn an_unconstrained_model_step_is_admitted() {
        assert_eq!(admit_next_model_step(&open()), ModelStepAdmission::Admit);
    }

    /// The token cap is measured against the usage projection, and it binds at
    /// equality — the model step that reaches the cap is the last one.
    #[test]
    fn the_token_cap_binds_at_equality() {
        let mut input = open();
        input.max_model_tokens = Some(100);
        input.model_tokens_spent = 99;
        assert_eq!(admit_next_model_step(&input), ModelStepAdmission::Admit);
        input.model_tokens_spent = 100;
        assert!(matches!(
            admit_next_model_step(&input),
            ModelStepAdmission::StopBudget(e) if e.dimension == BudgetDimension::ModelTokens
        ));
    }

    #[test]
    fn the_cost_cap_binds_at_equality() {
        let mut input = open();
        input.max_cost_usd_micros = Some(30);
        input.cost_spent_micros = 30;
        assert!(matches!(
            admit_next_model_step(&input),
            ModelStepAdmission::StopBudget(e) if e.dimension == BudgetDimension::Cost
        ));
    }

    /// A pinned ceiling is unconditional: no progress signal moves it, which
    /// is the whole reason it exists.
    #[test]
    fn a_pinned_model_step_ceiling_is_unconditional() {
        let mut input = open();
        input.model_steps = 100;
        assert_eq!(
            admit_next_model_step(&input),
            ModelStepAdmission::StopModelStepCeiling { ceiling: 100 }
        );
    }

    /// No pinned ceiling means no model-step count ends the run. The other
    /// mechanical guards still do — this only removes the count.
    #[test]
    fn an_unpinned_model_step_ceiling_never_stops_the_run() {
        let mut input = open();
        input.model_step_ceiling = None;
        input.model_steps = 10_000;
        assert_eq!(admit_next_model_step(&input), ModelStepAdmission::Admit);
    }

    /// A window limit ends the run without claiming a budget was exhausted —
    /// the host still gets to open the next run.
    #[test]
    fn a_window_limit_is_not_a_budget_exhaustion() {
        let mut input = open();
        input.model_steps = 2;
        input.model_step_window_limit = Some(2);
        assert_eq!(
            admit_next_model_step(&input),
            ModelStepAdmission::StopModelStepWindowLimit
        );
    }

    /// A token cap that is already spent outranks the window limit: the
    /// budget is reported, not the window boundary.
    #[test]
    fn a_spent_budget_outranks_the_window_boundary() {
        let mut input = open();
        input.model_steps = 2;
        input.model_step_window_limit = Some(2);
        input.max_model_tokens = Some(10);
        input.model_tokens_spent = 10;
        assert!(matches!(
            admit_next_model_step(&input),
            ModelStepAdmission::StopBudget(_)
        ));
    }

    /// The deadline timer cancels through the same token as a user's Ctrl-C.
    /// Reporting that as `Cancelled` would hide a budget stop as a user
    /// action, so an expired deadline falls through to the duration cap.
    #[test]
    fn an_expired_deadline_reports_as_duration_not_cancellation() {
        let mut input = open();
        input.cancelled = true;
        input.deadline_expired = true;
        input.max_duration = Some(Duration::from_secs(10));
        input.elapsed = Duration::from_secs(11);
        assert!(matches!(
            admit_next_model_step(&input),
            ModelStepAdmission::StopBudget(e) if e.dimension == BudgetDimension::Duration
        ));

        input.deadline_expired = false;
        assert_eq!(admit_next_model_step(&input), ModelStepAdmission::Cancelled);
    }

    /// A zero residual is a hard block, not an unlimited budget: a child
    /// §E: reaching the safety ceiling reports the CEILING arm — never the
    /// resource-budget arm. A model-step count must not be reported as a
    /// budget, or an operator cannot tell a runaway loop from a spent budget.
    #[test]
    fn a_model_step_ceiling_is_not_a_resource_budget() {
        let mut input = open();
        input.model_steps = 5;
        input.model_step_ceiling = Some(5);
        match admit_next_model_step(&input) {
            ModelStepAdmission::StopModelStepCeiling { ceiling } => assert_eq!(ceiling, 5),
            other => panic!("a step count must not report as a budget, got {other:?}"),
        }
    }

    /// §E: the safety ceiling and a deliberately bounded work window are two
    /// different limits with two different arms. The ceiling is checked first.
    #[test]
    fn the_ceiling_and_the_window_are_distinct_arms() {
        let mut input = open();
        input.model_steps = 5;
        input.model_step_ceiling = Some(5);
        input.model_step_window_limit = Some(5);
        assert_eq!(
            admit_next_model_step(&input),
            ModelStepAdmission::StopModelStepCeiling { ceiling: 5 }
        );
        input.model_step_ceiling = None;
        assert_eq!(
            admit_next_model_step(&input),
            ModelStepAdmission::StopModelStepWindowLimit
        );
    }

    /// The default breaker must stay far outside the range real tasks close in
    /// (measured: 9–25 model steps), and the legacy spelling must not drift
    /// away from the model-step one: one concept, one value.
    #[test]
    fn the_default_ceiling_has_exactly_one_value() {
        // The looseness invariant is a compile-time assertion next to the
        // constant; what a test can still check is that the legacy spelling
        // never drifts to a second value.
        assert_eq!(DEFAULT_ROUND_CEILING, DEFAULT_MODEL_STEP_CEILING);
        assert_eq!(
            ModelStepLimits::default().model_step_ceiling,
            Some(DEFAULT_MODEL_STEP_CEILING)
        );
    }

    /// handed `Some(0)` may not run a model step at all.
    #[test]
    fn a_zero_duration_residual_admits_nothing() {
        let mut input = open();
        input.max_duration = Some(Duration::ZERO);
        assert!(matches!(
            admit_next_model_step(&input),
            ModelStepAdmission::StopBudget(_)
        ));
    }

    #[test]
    fn the_window_limit_bounds_the_next_model_step() {
        let limits = ModelStepLimits {
            model_step_window_limit: Some(3),
            ..ModelStepLimits::default()
        };
        assert!(limits.allows_model_step_after(2));
        assert!(!limits.allows_model_step_after(3));
        assert!(ModelStepLimits::default().allows_model_step_after(1_000));
    }
}
