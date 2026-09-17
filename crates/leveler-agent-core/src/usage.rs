//! The loop's own view of what the run has spent.
//!
//! There is exactly one factual authority for spend: the finalized usage each
//! model call reported. This type is that authority folded as the calls
//! happen, so a budget guard and a later ledger answer with the same number.
//! Nothing here re-derives a token count or re-applies a price table.

use leveler_model::{ContentPart, Message, TokenUsage};

/// Aggregation of one run's model spend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageProjection {
    /// Logical calls folded so far. One record is one call, retries included
    /// in it rather than counted beside it.
    pub requests: u64,
    /// Prompt tokens as the provider reported them, cached share included.
    pub input_tokens: u64,
    /// The cached subset of [`Self::input_tokens`].
    pub cached_input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Summed over calls that carried a price. Calls without one are counted
    /// in [`Self::records_without_cost`] rather than read as free.
    pub cost_usd_micros: u64,
    /// Calls the model had no pricing for. A cost budget cannot bind on
    /// these, and saying so is not the same as saying they cost nothing.
    pub records_without_cost: u64,
    /// Tokens the loop estimated for a call whose provider reported none.
    ///
    /// A gateway that returns zero usage must not silently disable the token
    /// budget, so the loop falls back to a transcript estimate. That estimate
    /// is an admission input, never an accounting fact: it is kept apart from
    /// [`Self::input_tokens`] / [`Self::output_tokens`] so a comparison with a
    /// durable ledger stays exact, and the count of such calls is
    /// [`Self::records_without_usage`].
    pub estimated_model_tokens: u64,
    /// Calls whose provider reported no usage at all.
    pub records_without_usage: u64,
}

impl UsageProjection {
    /// Fold one finalized call.
    ///
    /// `estimated_tokens` is the transcript estimate to fall back on when the
    /// provider reported no usage; pass `None` where no estimate exists.
    pub fn record(
        &mut self,
        usage: TokenUsage,
        cost_usd_micros: Option<u64>,
        estimated_tokens: Option<u64>,
    ) {
        self.requests = self.requests.saturating_add(1);
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        match cost_usd_micros {
            Some(cost) => self.cost_usd_micros = self.cost_usd_micros.saturating_add(cost),
            None => self.records_without_cost = self.records_without_cost.saturating_add(1),
        }
        if usage.total() == 0 {
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

/// Coarse token estimate over a transcript's textual content. A fallback for
/// providers/gateways that don't report streaming usage, so token budgets and
/// compaction still bind on a growing conversation.
///
/// ASCII averages ~4 bytes/token; CJK and other non-ASCII text spends ~1 token
/// per character (~3 UTF-8 bytes), so those bytes are weighted at 3 bytes/token
/// — a flat ÷4 under-counts Chinese-heavy transcripts by ~25%.
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    // A conservative flat cost (in ASCII byte-equivalents, ÷4 below) for one
    // image, so a vision turn isn't counted as ~free. Real vision billing is
    // tile-based and model-specific; ~1000 tokens/image is a safe floor.
    const IMAGE_BYTE_EQUIV: u64 = 4096;
    let mut ascii_text: u64 = 0;
    let mut ascii_tool: u64 = 0;
    let mut wide_bytes: u64 = 0;
    let mut flat: u64 = 0;
    let split = |s: &str| -> (u64, u64) {
        let ascii = s.bytes().filter(u8::is_ascii).count() as u64;
        (ascii, s.len() as u64 - ascii)
    };
    for part in messages.iter().flat_map(|m| &m.content) {
        match part {
            ContentPart::Text { text } => {
                let (a, w) = split(text);
                ascii_text += a;
                wide_bytes += w;
            }
            // Tool payloads are JSON/log shaped — brackets, quotes, repeated
            // keys, hex ids — and tokenize far denser than prose: measured
            // ~2.5–2.9 bytes/token against DeepSeek-reported usage, where a
            // flat ÷4 under-counted tool-heavy transcripts by 27–38%.
            // Weighted at 2.5 so the residual error sits on the safe
            // (slightly over-estimating) side.
            ContentPart::ToolCall { call } => {
                let (a, w) = split(&call.name);
                ascii_tool += a;
                wide_bytes += w;
                let (a, w) = split(&call.arguments.to_string());
                ascii_tool += a;
                wide_bytes += w;
            }
            ContentPart::ToolResult { result } => {
                let (a, w) = split(&result.content);
                ascii_tool += a;
                wide_bytes += w;
            }
            ContentPart::Image { .. } => flat += IMAGE_BYTE_EQUIV / 4,
            _ => {}
        }
    }
    ascii_text / 4 + ascii_tool * 2 / 5 + wide_bytes / 3 + flat
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_model::Role;

    fn usage(input: u64, cached: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
        }
    }

    #[test]
    fn folds_reported_usage_and_cost() {
        let mut p = UsageProjection::default();
        p.record(usage(1_000, 900, 100), Some(50), None);
        p.record(usage(2_000, 1_800, 200), Some(70), None);
        assert_eq!(p.requests, 2);
        assert_eq!(p.input_tokens, 3_000);
        assert_eq!(p.cached_input_tokens, 2_700);
        assert_eq!(p.output_tokens, 300);
        assert_eq!(p.cost_usd_micros, 120);
        assert_eq!(p.reported_model_tokens(), 3_300);
    }

    /// An unpriced call is counted as unpriced, never as free.
    #[test]
    fn unpriced_record_is_counted_not_zeroed() {
        let mut p = UsageProjection::default();
        p.record(usage(10, 0, 5), None, None);
        assert_eq!(p.cost_usd_micros, 0);
        assert_eq!(p.records_without_cost, 1);
    }

    /// A provider that reports no usage does not disable the token budget,
    /// and its estimate stays out of the reported totals.
    #[test]
    fn zero_usage_falls_back_to_estimate_without_polluting_reported_totals() {
        let mut p = UsageProjection::default();
        p.record(usage(0, 0, 0), Some(3), Some(1_234));
        assert_eq!(p.reported_model_tokens(), 0, "nothing was reported");
        assert_eq!(p.admission_model_tokens(), 1_234, "the budget still binds");
        assert_eq!(p.records_without_usage, 1);
    }

    #[test]
    fn tool_payloads_are_weighted_denser_than_prose() {
        let body = "{\"path\":\"src/lib.rs\",\"exit\":0}".repeat(100);
        let as_text = estimate_tokens(&[Message::text(Role::User, body.clone())]);
        let as_tool = estimate_tokens(&[Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult {
                result: leveler_model::ToolResultContent {
                    call_id: leveler_core::ToolCallId::new("c"),
                    content: body,
                    is_error: false,
                },
            }],
        }]);
        assert!(
            as_tool > as_text * 3 / 2,
            "tool weighting missing: text={as_text} tool={as_tool}"
        );
    }

    #[test]
    fn images_are_not_free() {
        use leveler_model::ImageSource;
        let with_image = vec![Message {
            role: Role::User,
            content: vec![ContentPart::Image {
                source: ImageSource::Url {
                    url: "https://x/y.png".to_string(),
                },
            }],
        }];
        assert!(estimate_tokens(&with_image) >= 256);
    }
}
