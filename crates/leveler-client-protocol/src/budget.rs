//! The runtime's resource-budget status for the session's current task epoch.
//!
//! Unlike [`crate::UiUsageReport`], which aggregates durable provider rows for
//! a caller-chosen window, this projection describes the accounting the
//! runtime's own budget gate admits on: the cross-epoch `ProgressLedger` of the
//! current budget scope plus the caps configured for a top-level run. It is
//! recomputed from durable state on demand, so reconnect, restart and a second
//! client see the same figures without accumulating anything on the wire.
//!
//! `model_tokens` is the enforcer total: provider-reported usage **plus** the
//! transcript estimate a gateway that reported no usage is billed for. The
//! estimate is carried separately so a client never presents it as measured
//! usage. Cost is `cost_known = false` when any attempt had no auditable price:
//! the known sum is then a lower bound, never a total.

use serde::{Deserialize, Serialize};

/// Soft-threshold percentage used when the configuration does not name one.
/// A percentage is product policy, not a budget: it never stops a run and it
/// never invents a spend cap.
pub const DEFAULT_SOFT_LIMIT_PERCENT: u8 = 80;

/// A configurable hard-budget dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiBudgetDimension {
    ModelTokens,
    Cost,
    Duration,
}

impl UiBudgetDimension {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelTokens => "model_tokens",
            Self::Cost => "cost",
            Self::Duration => "duration",
        }
    }
}

/// How close the current epoch is to a configured hard cap.
///
/// `warning` is a reminder only: the run continues. `exhausted` means a hard
/// cap has been reached, so the next admission would stop; the task's own
/// terminal state still decides whether it is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiBudgetLevel {
    Normal,
    Warning,
    Exhausted,
}

/// The caps that govern a top-level run, in the enforcer's units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiBudgetLimits {
    /// Provider-reported + estimated model tokens. `None` = no token cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_tokens: Option<u64>,
    /// Auditable model cost in micro-USD. Requires model pricing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd_micros: Option<u64>,
    /// Wall-clock run duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_ms: Option<u64>,
    /// The resolved soft reminder percentage (1..=100).
    pub soft_limit_percent: u8,
}

/// One session's current budget epoch: what the enforcer counted and the caps
/// it counts against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiBudgetStatus {
    /// The budget scope this epoch belongs to: the goal id when the turn is
    /// goal-bound, else the root turn id. Opaque to clients.
    pub scope: String,
    /// Enforcer-accounted model tokens (reported + estimated).
    pub model_tokens: u64,
    /// The share of `model_tokens` no provider reported.
    pub estimated_model_tokens: u64,
    /// Known priced spend in micro-USD. A lower bound when `cost_known` is
    /// false; never present it as a total in that case.
    pub cost_usd_micros: u64,
    /// False when at least one attempt had no auditable price.
    pub cost_known: bool,
    /// Accumulated drive duration for this epoch.
    pub duration_ms: u64,
    pub limits: UiBudgetLimits,
    pub level: UiBudgetLevel,
    /// The dimension the level was decided by, when any cap exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<UiBudgetDimension>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_round_trips_without_inventing_unknowns() {
        let status = UiBudgetStatus {
            scope: "goal-1".into(),
            model_tokens: 1_200,
            estimated_model_tokens: 200,
            cost_usd_micros: 0,
            cost_known: false,
            duration_ms: 90_000,
            limits: UiBudgetLimits {
                max_model_tokens: Some(2_000),
                max_cost_usd_micros: None,
                max_duration_ms: None,
                soft_limit_percent: 80,
            },
            level: UiBudgetLevel::Warning,
            dimension: Some(UiBudgetDimension::ModelTokens),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"cost_known\":false"));
        assert!(!json.contains("\"max_cost_usd_micros\""));
        let back: UiBudgetStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
    }
}
