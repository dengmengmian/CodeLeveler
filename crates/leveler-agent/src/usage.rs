//! The runtime's projection of the durable model-request accounting.
//!
//! There is exactly one factual authority for what a session spent: the
//! `model_requests` rows the engine writes. This type is that authority seen
//! from inside the agent loop — the same finalized [`ModelRequestRecord`]s,
//! folded as they are handed to the transcript sink, so a budget guard and a
//! reconciliation query answer with the same number.
//!
//! It is deliberately NOT a second ledger: nothing here re-derives a token
//! count or re-applies a price table. Every field is a sum over records that
//! were already priced once, at the point the provider reported them.

use crate::executor::ModelRequestRecord;

/// Runtime-visible aggregation of one epoch's model spend.
///
/// Field-for-field the shape of `leveler_storage::SessionUsageTotals`, which is
/// what makes the conformance assertion a comparison rather than a
/// translation. The one field with no durable counterpart is
/// [`Self::estimated_model_tokens`] — see its docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeUsageProjection {
    /// Logical calls folded so far. One record is one call, retries included
    /// in it rather than counted beside it.
    pub requests: u64,
    /// Prompt tokens as the provider reported them, cached share included.
    pub input_tokens: u64,
    /// The cached subset of [`Self::input_tokens`].
    pub cached_input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Summed over records that carried a price. Records without one are
    /// counted in [`Self::records_without_cost`] rather than read as free.
    pub cost_usd_micros: u64,
    /// Records the model had no pricing for. A cost budget cannot bind on
    /// these, and saying so is not the same as saying they cost nothing.
    pub records_without_cost: u64,
    /// Tokens the runtime estimated for a call whose provider reported none.
    ///
    /// A gateway that returns zero usage must not silently disable the token
    /// budget, so the loop falls back to a transcript estimate. That estimate
    /// is a runtime admission input, never an accounting fact: it is kept
    /// apart from [`Self::input_tokens`] / [`Self::output_tokens`] precisely
    /// so the durable comparison stays exact, and the count of such calls is
    /// [`Self::records_without_usage`].
    pub estimated_model_tokens: u64,
    /// Records whose provider reported no usage at all.
    pub records_without_usage: u64,
}

impl RuntimeUsageProjection {
    /// Fold one finalized record — the same value being handed to the sink.
    ///
    /// `estimated_tokens` is the transcript estimate to fall back on when the
    /// provider reported no usage; pass `None` where no estimate exists.
    pub fn record(&mut self, record: &ModelRequestRecord, estimated_tokens: Option<u64>) {
        self.requests = self.requests.saturating_add(1);
        self.input_tokens = self.input_tokens.saturating_add(record.usage.input_tokens);
        self.output_tokens = self
            .output_tokens
            .saturating_add(record.usage.output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(record.usage.cached_input_tokens);
        match record.cost_usd_micros {
            Some(cost) => self.cost_usd_micros = self.cost_usd_micros.saturating_add(cost),
            None => self.records_without_cost = self.records_without_cost.saturating_add(1),
        }
        if record.usage.total() == 0 {
            self.records_without_usage = self.records_without_usage.saturating_add(1);
            if let Some(estimate) = estimated_tokens {
                self.estimated_model_tokens = self.estimated_model_tokens.saturating_add(estimate);
            }
        }
    }

    /// Tokens a `max_model_tokens` cap is measured against: what the providers
    /// reported, plus the estimate standing in for what they did not.
    pub fn admission_model_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.estimated_model_tokens)
    }

    /// Tokens with a durable counterpart — the half that must reconcile.
    pub fn reported_model_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Cost a `max_cost_usd_micros` cap is measured against.
    pub fn admission_cost_usd_micros(&self) -> u64 {
        self.cost_usd_micros
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_model::{FinishReason, TokenUsage};

    fn record(input: u64, cached: u64, output: u64, cost: Option<u64>) -> ModelRequestRecord {
        ModelRequestRecord {
            provider_request_id: None,
            provider: "deepseek".to_string(),
            model: "deepseek-v4-flash".to_string(),
            usage: TokenUsage {
                input_tokens: input,
                cached_input_tokens: cached,
                output_tokens: output,
            },
            finish_reason: FinishReason::Stop,
            latency_ms: 1,
            retry_count: 0,
            kind: crate::ModelCallKind::Round,
            agent_id: None,
            cost_usd_micros: cost,
        }
    }

    /// U1. Parent-only: the projection is the sum of what was recorded.
    #[test]
    fn folds_reported_usage_and_cost() {
        let mut p = RuntimeUsageProjection::default();
        p.record(&record(1_000, 900, 100, Some(50)), None);
        p.record(&record(2_000, 1_800, 200, Some(70)), None);
        assert_eq!(p.requests, 2);
        assert_eq!(p.input_tokens, 3_000);
        assert_eq!(p.cached_input_tokens, 2_700);
        assert_eq!(p.output_tokens, 300);
        assert_eq!(p.cost_usd_micros, 120);
        assert_eq!(p.reported_model_tokens(), 3_300);
    }

    /// U7. An unpriced record is counted as unpriced, never as free.
    #[test]
    fn unpriced_record_is_counted_not_zeroed() {
        let mut p = RuntimeUsageProjection::default();
        p.record(&record(10, 0, 5, None), None);
        assert_eq!(p.cost_usd_micros, 0);
        assert_eq!(p.records_without_cost, 1);
    }

    /// U7. A provider that reports no usage does not disable the token
    /// budget, and its estimate stays out of the reconciled totals.
    #[test]
    fn zero_usage_falls_back_to_estimate_without_polluting_reported_totals() {
        let mut p = RuntimeUsageProjection::default();
        p.record(&record(0, 0, 0, Some(3)), Some(1_234));
        assert_eq!(p.reported_model_tokens(), 0, "nothing was reported");
        assert_eq!(p.admission_model_tokens(), 1_234, "the budget still binds");
        assert_eq!(p.records_without_usage, 1);
    }
}
