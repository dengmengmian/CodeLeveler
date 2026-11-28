//! Goal/epoch budget status: the ONE projection of what the runtime's budget
//! gate has counted for the session's current task epoch, plus the caps it
//! counts against.
//!
//! The spend figures come from the durable cross-epoch `ProgressLedger` the
//! drive persists as `progress_updated` rows — the same totals the enforcer
//! seeds on resume and admits on. Nothing is accumulated on the wire, so a
//! reconnect, a restart or a second client computes the same answer.
//!
//! Two honesty rules are structural:
//!
//! - `model_tokens` is the enforcer total (reported + estimated); the estimate
//!   travels separately so no client presents it as measured usage.
//! - cost is a lower bound when any attempt had no auditable price
//!   (`cost_known = false`) and must not be rendered as a complete bill.
//!
//! The soft level is a reminder computed from the same figures. It never stops
//! a run; only the enforcer's hard caps do.

use leveler_agent::StepLimits;
use leveler_client_protocol::{UiBudgetDimension, UiBudgetLevel, UiBudgetLimits, UiBudgetStatus};
use leveler_core::SessionId;
use leveler_engine::EngineEvent;
use leveler_lifecycle::ProgressLedger;
use leveler_storage::{Database, EventStore as _};

use crate::AppError;

/// The newest durable progress ledger for this session, regardless of scope.
///
/// The ledger is a full snapshot and carries its own `budget_scope`; the most
/// recent row is the current task epoch (a goal after a `/goal` upgrade, else
/// the root turn). `None` means no epoch has durable progress yet — never a
/// zero spend.
pub(crate) async fn latest_ledger(
    db: &Database,
    session_id: &SessionId,
) -> Result<Option<ProgressLedger>, AppError> {
    for row in db
        .load_by_types(session_id, &["progress_updated"])
        .await?
        .into_iter()
        .rev()
    {
        match EngineEvent::from_payload(&row.payload) {
            Ok(EngineEvent::ProgressUpdated { ledger }) => return Ok(Some(ledger)),
            // Another concurrent writer can advance the log, but a malformed
            // progress row is corruption, not absence: report it.
            Ok(_) => continue,
            Err(error) => return Err(AppError::Engine(error.to_string())),
        }
    }
    Ok(None)
}

/// Project one ledger against the caps in force.
///
/// `limits.max_model_steps` is ignored: it is the model-step safety ceiling,
/// a mechanical circuit breaker, not a task resource budget.
pub(crate) fn status_from_ledger(
    ledger: &ProgressLedger,
    limits: &StepLimits,
    soft_limit_percent: u8,
) -> UiBudgetStatus {
    let soft_percent = soft_limit_percent.clamp(1, 100);
    let token_cap = limits.max_model_tokens;
    let cost_cap = limits.max_cost_usd_micros;
    let duration_cap = limits
        .max_duration
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));

    // The dimension closest to its cap decides the level. A ratio is scaled by
    // 10_000 so the comparison stays integer-exact.
    let mut closest: Option<(UiBudgetDimension, u64)> = None;
    let mut consider = |dimension, spent: u64, cap: Option<u64>| {
        let Some(cap) = cap else { return };
        let ratio = if cap == 0 {
            u64::MAX
        } else {
            spent.saturating_mul(10_000) / cap
        };
        if closest.is_none_or(|(_, best)| ratio > best) {
            closest = Some((dimension, ratio));
        }
    };
    consider(
        UiBudgetDimension::ModelTokens,
        ledger.cumulative_model_tokens,
        token_cap,
    );
    // A cost cap needs auditable pricing. When an attempt is unpriced the
    // known sum is a lower bound; inventing a ratio would claim a precision
    // the ledger does not have.
    if !ledger.has_unpriced_model_attempt {
        consider(
            UiBudgetDimension::Cost,
            ledger.cumulative_cost_usd_micros,
            cost_cap,
        );
    }
    consider(
        UiBudgetDimension::Duration,
        ledger.cumulative_duration_ms,
        duration_cap,
    );

    let soft = u64::from(soft_percent) * 100;
    let (level, dimension) = match closest {
        Some((dimension, ratio)) if ratio >= 10_000 => (UiBudgetLevel::Exhausted, Some(dimension)),
        Some((dimension, ratio)) if ratio >= soft => (UiBudgetLevel::Warning, Some(dimension)),
        Some((dimension, _)) => (UiBudgetLevel::Normal, Some(dimension)),
        None => (UiBudgetLevel::Normal, None),
    };

    UiBudgetStatus {
        scope: ledger
            .budget_scope
            .clone()
            .unwrap_or_else(|| "unscoped".to_string()),
        model_tokens: ledger.cumulative_model_tokens,
        estimated_model_tokens: ledger.cumulative_estimated_model_tokens,
        cost_usd_micros: ledger.cumulative_cost_usd_micros,
        cost_known: !ledger.has_unpriced_model_attempt,
        duration_ms: ledger.cumulative_duration_ms,
        limits: UiBudgetLimits {
            max_model_tokens: token_cap,
            max_cost_usd_micros: cost_cap,
            max_duration_ms: duration_cap,
            soft_limit_percent: soft_percent,
        },
        level,
        dimension,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(
        tokens: u64,
        estimated: u64,
        cost: u64,
        unpriced: bool,
        duration_ms: u64,
    ) -> ProgressLedger {
        ProgressLedger {
            budget_scope: Some("goal-1".into()),
            cumulative_model_tokens: tokens,
            cumulative_estimated_model_tokens: estimated,
            cumulative_cost_usd_micros: cost,
            has_unpriced_model_attempt: unpriced,
            cumulative_duration_ms: duration_ms,
            ..ProgressLedger::default()
        }
    }

    fn limits(tokens: Option<u64>, cost: Option<u64>, secs: Option<u64>) -> StepLimits {
        StepLimits {
            max_model_tokens: tokens,
            max_cost_usd_micros: cost,
            max_duration: secs.map(std::time::Duration::from_secs),
            ..StepLimits::default()
        }
    }

    #[test]
    fn no_caps_is_normal_and_never_invents_a_budget() {
        let status = status_from_ledger(
            &ledger(10_000, 0, 0, false, 1),
            &limits(None, None, None),
            80,
        );
        assert_eq!(status.level, UiBudgetLevel::Normal);
        assert_eq!(status.dimension, None);
        assert_eq!(status.limits.max_model_tokens, None);
    }

    #[test]
    fn the_soft_threshold_warns_without_stopping() {
        let status = status_from_ledger(
            &ledger(800, 0, 0, false, 1),
            &limits(Some(1_000), None, None),
            80,
        );
        assert_eq!(status.level, UiBudgetLevel::Warning);
        assert_eq!(status.dimension, Some(UiBudgetDimension::ModelTokens));
    }

    #[test]
    fn the_hard_cap_reads_as_exhausted() {
        let status = status_from_ledger(
            &ledger(1_000, 0, 0, false, 1),
            &limits(Some(1_000), None, None),
            80,
        );
        assert_eq!(status.level, UiBudgetLevel::Exhausted);
    }

    #[test]
    fn an_estimate_is_carried_apart_from_measured_usage() {
        let status = status_from_ledger(
            &ledger(1_200, 200, 0, false, 1),
            &limits(Some(2_000), None, None),
            80,
        );
        assert_eq!(status.model_tokens, 1_200);
        assert_eq!(status.estimated_model_tokens, 200);
    }

    #[test]
    fn an_unpriced_attempt_makes_cost_a_lower_bound_not_a_ratio() {
        let status = status_from_ledger(
            &ledger(10, 0, 1, true, 1),
            &limits(None, Some(100), None),
            80,
        );
        assert!(!status.cost_known);
        assert_eq!(
            status.level,
            UiBudgetLevel::Normal,
            "cost cannot be judged without a price"
        );
        assert_eq!(status.dimension, None);
    }

    #[test]
    fn a_zero_cap_is_immediately_exhausted() {
        let status =
            status_from_ledger(&ledger(0, 0, 0, false, 0), &limits(Some(0), None, None), 80);
        assert_eq!(status.level, UiBudgetLevel::Exhausted);
        assert_eq!(status.dimension, Some(UiBudgetDimension::ModelTokens));
    }

    #[test]
    fn duration_compares_in_milliseconds() {
        let status = status_from_ledger(
            &ledger(0, 0, 0, false, 30_000),
            &limits(None, None, Some(60)),
            50,
        );
        assert_eq!(status.limits.max_duration_ms, Some(60_000));
        assert_eq!(status.level, UiBudgetLevel::Warning);
        assert_eq!(status.dimension, Some(UiBudgetDimension::Duration));
    }

    #[test]
    fn the_closest_dimension_is_named() {
        let status = status_from_ledger(
            &ledger(100, 0, 0, false, 55_000),
            &limits(Some(1_000), None, Some(60)),
            50,
        );
        assert_eq!(status.dimension, Some(UiBudgetDimension::Duration));
    }
}
