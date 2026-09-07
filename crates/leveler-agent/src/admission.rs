//! Whether the runtime may spend another main-task model call.
//!
//! The drive loop's top already decided this — it just never said so. Six
//! guards sat inline at the head of the round loop, each with its own early
//! return, and every path that wanted another round (`continue` from a tool
//! batch, a refused close, a settled child, a nudge) reached them by falling
//! back to the top. That made the loop's shape the authority: correct, but
//! unnamed and unreadable, and impossible to hand a fact it did not already
//! have in a local variable.
//!
//! This is that decision, named. It is deliberately NOT a new policy: the
//! predicates and their order are the ones the loop already ran, so the same
//! state yields the same verdict. What changes is that there is now one place
//! to ask, one input type to extend, and one thing to test.
//!
//! Three kinds of model call are **not** admitted here, each for a reason:
//!
//! - **Protocol repair** — a malformed tool call, a truncated response, a
//!   `tool_calls` finish with no calls. These are one logical round failing to
//!   complete, not the runtime choosing to spend another. They carry their own
//!   tight consecutive-only bounds.
//! - **Human input** — a steering message is not a runtime decision, and
//!   admission may not veto it.
//! - **Runtime overhead** — folds, contract derivation, the reconciliation
//!   judge. These are the runtime's own spend, governed by the usage
//!   projection, not by whether the task should take another turn.

use std::time::Duration;

use crate::budget::{BudgetDimension, BudgetExhaustion};

/// Everything the round-admission decision reads. All of it already exists;
/// nothing here is derived for the purpose.
#[derive(Debug, Clone, Copy)]
pub struct RoundAdmissionInput {
    /// Rounds completed so far in this drive.
    pub round: u32,
    /// The absolute per-turn ceiling — the unconditional circuit breaker.
    pub round_ceiling: u32,
    /// The window's own round limit, when the continuation policy pins one.
    pub window_round_limit: Option<u32>,
    /// Epoch tokens as the usage projection reports them, and the cap if set.
    pub model_tokens_spent: u64,
    pub max_model_tokens: Option<u64>,
    /// Epoch cost in micro-USD from the same projection, and the cap if set.
    pub cost_spent_micros: u64,
    pub max_cost_usd_micros: Option<u64>,
    /// Wall clock consumed by this epoch, and the cap if set.
    pub elapsed: Duration,
    pub max_duration: Option<Duration>,
    /// The turn was cancelled from outside — as opposed to the deadline timer
    /// cancelling it, which is a budget stop and reports as one.
    pub cancelled: bool,
    pub deadline_expired: bool,
}

/// The verdict. Each non-admitting arm names what the drive loop already did
/// for that case, so the mapping back to an outcome stays mechanical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundAdmission {
    /// Spend another main-task model call.
    Admit,
    /// A resource cap is spent. Reports as `BudgetExhausted` with the
    /// dimension that fired.
    StopBudget(BudgetExhaustion),
    /// The absolute per-turn round ceiling. Not a budget: a circuit breaker
    /// that fires regardless of progress or policy.
    StopRoundCeiling { ceiling: u32 },
    /// This window's pinned round limit is reached. The turn ends normally and
    /// the supervisor decides whether another window opens.
    StopWindowLimit,
    /// Cancelled from outside the turn.
    Cancelled,
}

/// Admit — or refuse — the next main-task model call.
///
/// Order matters and is the loop's own: token cap, cost cap, round ceiling,
/// window limit, cancellation, duration cap. The first three are checked
/// against the round just completed; the last two against the round about to
/// start, which is why `elapsed` is read after the round counter advances.
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
    // An external cancel ends the turn as cancelled; the deadline timer
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

    /// A window limit ends the turn without claiming a budget was exhausted —
    /// the supervisor still gets to open the next window.
    #[test]
    fn a_window_limit_is_not_a_budget_exhaustion() {
        let mut input = open();
        input.round = 2;
        input.window_round_limit = Some(2);
        assert_eq!(admit_next_round(&input), RoundAdmission::StopWindowLimit);
    }

    /// A token cap that is already spent outranks the window limit, exactly as
    /// the inline guards did: the budget is reported, not the window boundary.
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
}
