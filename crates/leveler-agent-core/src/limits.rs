//! Mechanical limits on a run and the decision whether another model round
//! may start.
//!
//! A hard limit is hard: nothing here decides that a run deserves more. The
//! predicates and their order are the loop's own; naming them gives one place
//! to ask, one input type to extend, and one thing to test.

use std::time::Duration;

/// The absolute per-run round ceiling when the host pins none — the
/// unconditional circuit breaker that guarantees an unbounded run terminates.
pub const DEFAULT_ROUND_CEILING: u32 = 100;

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
pub struct RoundLimits {
    /// Absolute round ceiling for this run. Fires regardless of progress or
    /// policy; the host cannot lift it mid-run.
    pub round_ceiling: u32,
    /// This run's own round limit, when the host pins one. Reaching it ends
    /// the run normally (`StopReason::WindowLimit`), not as a budget stop.
    pub window_round_limit: Option<u32>,
    /// Max provider-reported (or estimated) input + output tokens.
    pub max_model_tokens: Option<u64>,
    /// Max auditable model cost in micro-USD. Requires pricing on the agent;
    /// the loop refuses to start with a cost cap it cannot measure.
    pub max_cost_usd_micros: Option<u64>,
    /// Max wall-clock duration. The loop arms a deadline timer that cancels
    /// in-flight work through the run's cancellation token.
    pub max_duration: Option<Duration>,
    /// Spend already made against these caps before this run.
    pub spent_before: SpentBefore,
}

impl Default for RoundLimits {
    fn default() -> Self {
        Self {
            round_ceiling: DEFAULT_ROUND_CEILING,
            window_round_limit: None,
            max_model_tokens: None,
            max_cost_usd_micros: None,
            max_duration: None,
            spent_before: SpentBefore::default(),
        }
    }
}

impl RoundLimits {
    /// Whether the run may start another round after `round` completed
    /// rounds, as far as the pinned window limit is concerned.
    pub fn allows_round_after(&self, round: u32) -> bool {
        self.window_round_limit.is_none_or(|max| round < max)
    }
}

/// Everything the round-admission decision reads. All of it already exists;
/// nothing here is derived for the purpose.
#[derive(Debug, Clone, Copy)]
pub struct RoundAdmissionInput {
    /// Rounds completed so far in this run.
    pub round: u32,
    /// The absolute round ceiling — the unconditional circuit breaker.
    pub round_ceiling: u32,
    /// The run's own round limit, when one is pinned.
    pub window_round_limit: Option<u32>,
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
pub enum RoundAdmission {
    /// Spend another model call.
    Admit,
    /// A resource cap is spent, with the dimension that fired.
    StopBudget(BudgetExhaustion),
    /// The absolute round ceiling. Not a budget: a circuit breaker that fires
    /// regardless of progress or policy.
    StopRoundCeiling { ceiling: u32 },
    /// The run's pinned round limit is reached. The run ends normally; the
    /// host decides whether another run opens.
    StopWindowLimit,
    /// Cancelled from outside the run.
    Cancelled,
}

/// Admit — or refuse — the next model call.
///
/// Order matters: token cap, cost cap, round ceiling, window limit,
/// cancellation, duration cap. The first four are checked against the round
/// just completed; the last two against the round about to start, which is
/// why `elapsed` is read after the round counter advances.
pub fn admit_next_round(input: &RoundAdmissionInput) -> RoundAdmission {
    if let Some(max) = input.max_model_tokens
        && input.model_tokens_spent >= max
    {
        return RoundAdmission::StopBudget(BudgetExhaustion::new(
            BudgetDimension::ModelTokens,
            input.model_tokens_spent,
            max,
        ));
    }
    if let Some(max) = input.max_cost_usd_micros
        && input.cost_spent_micros >= max
    {
        return RoundAdmission::StopBudget(BudgetExhaustion::new(
            BudgetDimension::Cost,
            input.cost_spent_micros,
            max,
        ));
    }
    if input.round >= input.round_ceiling {
        return RoundAdmission::StopRoundCeiling {
            ceiling: input.round_ceiling,
        };
    }
    if input
        .window_round_limit
        .is_some_and(|max| input.round >= max)
    {
        return RoundAdmission::StopWindowLimit;
    }
    // An external cancel ends the run as cancelled; the deadline timer
    // cancels through the same token but must report as the duration budget,
    // so it falls through to the duration check below.
    if input.cancelled && !input.deadline_expired {
        return RoundAdmission::Cancelled;
    }
    if let Some(max) = input.max_duration {
        // `Some(0)` is a hard-exhausted residual handed to a child; for a
        // positive cap the round may start only while still inside it.
        if max.is_zero() || input.elapsed > max {
            return RoundAdmission::StopBudget(BudgetExhaustion::new(
                BudgetDimension::Duration,
                input.elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
                max.as_millis().min(u128::from(u64::MAX)) as u64,
            ));
        }
    }
    RoundAdmission::Admit
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

    fn open() -> RoundAdmissionInput {
        RoundAdmissionInput {
            round: 1,
            round_ceiling: 100,
            window_round_limit: None,
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
    fn an_unconstrained_round_is_admitted() {
        assert_eq!(admit_next_round(&open()), RoundAdmission::Admit);
    }

    /// The token cap is measured against the usage projection, and it binds at
    /// equality — the round that reaches the cap is the last one.
    #[test]
    fn the_token_cap_binds_at_equality() {
        let mut input = open();
        input.max_model_tokens = Some(100);
        input.model_tokens_spent = 99;
        assert_eq!(admit_next_round(&input), RoundAdmission::Admit);
        input.model_tokens_spent = 100;
        assert!(matches!(
            admit_next_round(&input),
            RoundAdmission::StopBudget(e) if e.dimension == BudgetDimension::ModelTokens
        ));
    }

    #[test]
    fn the_cost_cap_binds_at_equality() {
        let mut input = open();
        input.max_cost_usd_micros = Some(30);
        input.cost_spent_micros = 30;
        assert!(matches!(
            admit_next_round(&input),
            RoundAdmission::StopBudget(e) if e.dimension == BudgetDimension::Cost
        ));
    }

    /// The ceiling is unconditional: no progress signal and no policy can move
    /// it, which is the whole reason it exists.
    #[test]
    fn the_round_ceiling_is_unconditional() {
        let mut input = open();
        input.round = 100;
        assert_eq!(
            admit_next_round(&input),
            RoundAdmission::StopRoundCeiling { ceiling: 100 }
        );
    }

    /// A window limit ends the run without claiming a budget was exhausted —
    /// the host still gets to open the next run.
    #[test]
    fn a_window_limit_is_not_a_budget_exhaustion() {
        let mut input = open();
        input.round = 2;
        input.window_round_limit = Some(2);
        assert_eq!(admit_next_round(&input), RoundAdmission::StopWindowLimit);
    }

    /// A token cap that is already spent outranks the window limit: the
    /// budget is reported, not the window boundary.
    #[test]
    fn a_spent_budget_outranks_the_window_boundary() {
        let mut input = open();
        input.round = 2;
        input.window_round_limit = Some(2);
        input.max_model_tokens = Some(10);
        input.model_tokens_spent = 10;
        assert!(matches!(
            admit_next_round(&input),
            RoundAdmission::StopBudget(_)
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
            admit_next_round(&input),
            RoundAdmission::StopBudget(e) if e.dimension == BudgetDimension::Duration
        ));

        input.deadline_expired = false;
        assert_eq!(admit_next_round(&input), RoundAdmission::Cancelled);
    }

    /// A zero residual is a hard block, not an unlimited budget: a child
    /// handed `Some(0)` may not run a round at all.
    #[test]
    fn a_zero_duration_residual_admits_nothing() {
        let mut input = open();
        input.max_duration = Some(Duration::ZERO);
        assert!(matches!(
            admit_next_round(&input),
            RoundAdmission::StopBudget(_)
        ));
    }

    #[test]
    fn the_window_limit_bounds_the_next_round() {
        let limits = RoundLimits {
            window_round_limit: Some(3),
            ..RoundLimits::default()
        };
        assert!(limits.allows_round_after(2));
        assert!(!limits.allows_round_after(3));
        assert!(RoundLimits::default().allows_round_after(1_000));
    }
}
