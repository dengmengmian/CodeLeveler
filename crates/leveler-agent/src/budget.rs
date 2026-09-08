//! Structured resource-budget exhaustion.
//!
//! Hard limits terminate a drive as [`crate::StopReason::BudgetExhausted`];
//! this module records *which* dimension fired (spent vs cap). A hard budget
//! is hard: nothing here decides that a run deserves more.

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
}
