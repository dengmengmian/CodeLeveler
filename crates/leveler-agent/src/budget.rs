//! Structured resource-budget exhaustion and bounded epoch extension.
//!
//! Hard limits still terminate a drive as [`crate::StopReason::BudgetExhausted`].
//! This module records *which* dimension fired (spent vs cap) and decides whether
//! an outer epoch may grant a small extra allowance — never the absolute round
//! ceiling, and never after stagnation / no-progress stops.

use std::time::Duration;

use crate::executor::{StepLimits, StopReason};

/// Maximum automatic budget extensions at the task/engine epoch boundary.
pub const MAX_BUDGET_EXTENSIONS: u32 = 2;

/// Which resource limit terminated a drive.
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

/// Structured budget-exhaust facts carried on [`crate::AgentOutcome`].
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

    /// Parseable stop_detail contract used by logs and older consumers.
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

/// Whether a finished drive may receive another resource grant.
///
/// Rules (deliberately conservative):
/// - only [`StopReason::BudgetExhausted`] (never `TurnLimitReached`)
/// - at most [`MAX_BUDGET_EXTENSIONS`] grants
/// - require hard-to-fake progress (workspace mutation)
/// - refuse when the stop already indicated no-progress / stagnation thrash
pub fn budget_extension_allowed(
    stop_reason: StopReason,
    extension_count: u32,
    had_real_progress: bool,
    no_progress_or_stagnation: bool,
) -> bool {
    if !matches!(stop_reason, StopReason::BudgetExhausted) {
        return false;
    }
    if matches!(stop_reason, StopReason::TurnLimitReached) {
        return false;
    }
    if extension_count >= MAX_BUDGET_EXTENSIONS {
        return false;
    }
    if no_progress_or_stagnation || !had_real_progress {
        return false;
    }
    true
}

/// Infer no-progress / stagnation from stop_detail text when no structured flag.
pub fn stop_detail_indicates_no_progress(detail: Option<&str>) -> bool {
    let Some(d) = detail.map(str::to_ascii_lowercase) else {
        return false;
    };
    d.contains("no-progress")
        || d.contains("stagnation")
        || d.contains("stalled")
        || d.contains("observe thrash")
        || d.contains("identical repeat")
}

/// Raise the absolute cap for the dimension that fired.
///
/// Grant is ~50% of the previous cap (minimum 1 unit) so a near-miss can finish
/// without reopening an unlimited budget. The absolute per-turn round ceiling is
/// never touched here.
pub fn grant_budget_extension(limits: StepLimits, exhaustion: &BudgetExhaustion) -> StepLimits {
    let mut limits = limits;
    let grant = exhaustion.cap.saturating_div(2).max(1);
    let new_cap = exhaustion.cap.saturating_add(grant);
    // Ensure the new cap is strictly above spent so the next drive can proceed.
    let new_cap = new_cap.max(exhaustion.spent.saturating_add(1));
    match exhaustion.dimension {
        BudgetDimension::ModelTokens => {
            limits.max_model_tokens = Some(new_cap);
        }
        BudgetDimension::Cost => {
            limits.max_cost_usd_micros = Some(new_cap);
        }
        BudgetDimension::Duration => {
            limits.max_duration = Some(Duration::from_millis(new_cap));
        }
        BudgetDimension::Commands => {
            limits.max_commands = Some(new_cap.min(u64::from(u32::MAX)) as u32);
        }
        BudgetDimension::ModifiedFiles => {
            limits.max_modified_files = Some(usize::try_from(new_cap).unwrap_or(usize::MAX));
        }
    }
    limits
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

    #[test]
    fn extension_allowed_only_for_budget_with_progress() {
        assert!(budget_extension_allowed(
            StopReason::BudgetExhausted,
            0,
            true,
            false
        ));
        assert!(!budget_extension_allowed(
            StopReason::BudgetExhausted,
            0,
            false,
            false
        ));
        assert!(!budget_extension_allowed(
            StopReason::BudgetExhausted,
            0,
            true,
            true
        ));
        assert!(!budget_extension_allowed(
            StopReason::TurnLimitReached,
            0,
            true,
            false
        ));
        assert!(!budget_extension_allowed(
            StopReason::Incomplete,
            0,
            true,
            false
        ));
    }

    #[test]
    fn extension_refused_after_max() {
        assert!(!budget_extension_allowed(
            StopReason::BudgetExhausted,
            MAX_BUDGET_EXTENSIONS,
            true,
            false
        ));
        assert!(budget_extension_allowed(
            StopReason::BudgetExhausted,
            MAX_BUDGET_EXTENSIONS - 1,
            true,
            false
        ));
    }

    #[test]
    fn grant_raises_fired_dimension_above_spent() {
        let base = StepLimits {
            max_commands: Some(10),
            ..StepLimits::default()
        };
        let e = BudgetExhaustion::new(BudgetDimension::Commands, 10, 10);
        let next = grant_budget_extension(base, &e);
        assert_eq!(next.max_commands, Some(15));
        assert!(next.max_commands.unwrap() > e.spent as u32);
    }

    #[test]
    fn grant_duration_uses_millis() {
        let base = StepLimits {
            max_duration: Some(Duration::from_millis(10)),
            ..StepLimits::default()
        };
        let e = BudgetExhaustion::new(BudgetDimension::Duration, 12, 10);
        let next = grant_budget_extension(base, &e);
        assert_eq!(next.max_duration, Some(Duration::from_millis(15)));
    }
}
