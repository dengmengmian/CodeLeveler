//! Unified streaming events (spec §11). Raw provider SSE is normalized into
//! this enum before any upper layer sees it.

use serde::{Deserialize, Serialize};

use leveler_core::{RequestId, ToolCallId};

use crate::error::ModelError;
use crate::message::ToolCall;

/// Token accounting reported by the provider.
///
/// # Canonical contract
///
/// Every provider this harness speaks to reports a *total* completion count
/// plus, at best, a breakdown of it. That shape is the contract here:
///
/// ```text
/// input_tokens          prompt tokens; INCLUDES cached_input_tokens
/// cached_input_tokens   subset of input_tokens
/// output_tokens         completion tokens; INCLUDES reasoning_tokens
/// reasoning_tokens      subset of output_tokens, when the provider says so
/// ```
///
/// The inclusion is what keeps accounting additive: a provider that reports
/// `completion_tokens = 1000` with `reasoning_tokens = 700` spent 1000 output
/// tokens, 300 of them visible. Adding the two would invent a 1700-token bill
/// that no provider charged, so [`Self::total`], pricing, and every budget
/// guard read `output_tokens` alone and never add `reasoning_tokens` on top.
///
/// `reasoning_tokens` is `Option` because a provider that does not break out
/// reasoning is not the same as one that reported zero: absent means unknown,
/// and nothing here estimates it. Use [`Self::visible_output_tokens`] to get
/// the non-reasoning share, which is only derivable when the breakdown was
/// reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Input tokens served from the provider's prefix cache. A subset of
    /// `input_tokens`, billed at a fraction of the price. Zero when the provider
    /// does not report it — never assume "no cache" from a zero here.
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// Output tokens the provider attributed to reasoning/thinking. A subset of
    /// `output_tokens`. `None` = the provider did not report a breakdown
    /// (unknown, not zero); `Some(0)` = it reported that none were spent.
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

impl TokenUsage {
    /// Tokens a provider would bill as prompt + completion.
    ///
    /// `reasoning_tokens` is deliberately not added: it is a breakdown of
    /// `output_tokens`, not a second bucket.
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Share of input tokens served from cache, in `0.0..=1.0`. Zero input
    /// tokens yields `0.0`.
    pub fn cache_hit_rate(&self) -> f64 {
        if self.input_tokens == 0 {
            return 0.0;
        }
        self.cached_input_tokens as f64 / self.input_tokens as f64
    }

    /// Output tokens that were not reasoning: `output_tokens - reasoning_tokens`.
    ///
    /// `Some` only when the provider reported a reasoning breakdown that fits
    /// inside its own completion count. `None` means the split is unknown —
    /// either nothing was reported, or the report contradicted the contract
    /// (`reasoning_tokens > output_tokens`), in which case subtracting would
    /// silently fabricate a visible count instead of admitting ignorance.
    pub fn visible_output_tokens(&self) -> Option<u64> {
        self.reasoning_tokens
            .filter(|reasoning| *reasoning <= self.output_tokens)
            .map(|reasoning| self.output_tokens - reasoning)
    }

    /// Share of output tokens attributed to reasoning, in `0.0..=1.0`.
    ///
    /// `None` under the same conditions as [`Self::visible_output_tokens`];
    /// zero output tokens with a reported breakdown yields `0.0`.
    pub fn reasoning_share(&self) -> Option<f64> {
        let reasoning = self
            .reasoning_tokens
            .filter(|reasoning| *reasoning <= self.output_tokens)?;
        if self.output_tokens == 0 {
            return Some(0.0);
        }
        Some(reasoning as f64 / self.output_tokens as f64)
    }
}

/// Why a model turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Model produced a natural end of message.
    Stop,
    /// Output hit the max token limit.
    Length,
    /// Model requested one or more tool calls.
    ToolCalls,
    /// Content was filtered by the provider.
    ContentFilter,
    /// Provider returned a reason we do not recognize.
    Other,
}

/// The normalized streaming event vocabulary. Every consumer (CLI, agent loop,
/// session log) reads *only* these — never raw provider structures (spec §11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    /// The provider accepted the request and started a response.
    MessageStarted { request_id: RequestId },
    /// A chunk of assistant text.
    TextDelta { delta: String },
    /// A chunk of reasoning/thinking text.
    ReasoningDelta { delta: String },
    /// A new tool call slot opened at `index`.
    ToolCallStarted {
        index: usize,
        id: Option<ToolCallId>,
        name: Option<String>,
    },
    /// A fragment of the JSON arguments for the tool call at `index`.
    ToolCallArgumentsDelta { index: usize, delta: String },
    /// A tool call whose arguments have been fully joined and JSON-parsed.
    ToolCallCompleted { call: ToolCall },
    /// Usage figures (may arrive mid-stream or at the end).
    UsageUpdated { usage: TokenUsage },
    /// The message completed for the given reason.
    MessageCompleted { finish_reason: FinishReason },
    /// A terminal error occurred during streaming.
    Error { error: ModelError },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_type_tagged() {
        let e = ModelEvent::TextDelta { delta: "x".into() };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["type"], "text_delta");
        assert_eq!(json["delta"], "x");
    }

    #[test]
    fn cache_hit_rate_is_the_cached_share_of_input() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 5,
            cached_input_tokens: 75,
            reasoning_tokens: None,
        };
        assert!((u.cache_hit_rate() - 0.75).abs() < f64::EPSILON);
        // No input tokens must not divide by zero.
        assert_eq!(TokenUsage::default().cache_hit_rate(), 0.0);
    }

    #[test]
    fn usage_totals() {
        let u = TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 0,
            reasoning_tokens: None,
        };
        assert_eq!(u.total(), 15);
    }

    /// Reasoning is a breakdown of the completion count, so it changes no
    /// total and no bill: 1000 output at 700 reasoning is still 1000 output.
    #[test]
    fn reasoning_tokens_are_inside_the_completion_total_and_never_added() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 1_000,
            cached_input_tokens: 0,
            reasoning_tokens: Some(700),
        };
        assert_eq!(u.total(), 1_100, "total must not add reasoning twice");
        assert_eq!(u.visible_output_tokens(), Some(300));
        assert_eq!(u.reasoning_share(), Some(0.7));
    }

    /// An absent breakdown is unknown, not zero. A provider that reports no
    /// reasoning fields is not a provider that measured zero reasoning.
    #[test]
    fn absent_reasoning_breakdown_stays_unknown() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 1_000,
            cached_input_tokens: 0,
            reasoning_tokens: None,
        };
        assert_eq!(u.visible_output_tokens(), None);
        assert_eq!(u.reasoning_share(), None);
    }

    /// A reported zero is a measurement: nothing was spent on reasoning, so
    /// the whole completion count is visible.
    #[test]
    fn reported_zero_reasoning_is_not_the_same_as_absent() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 40,
            cached_input_tokens: 0,
            reasoning_tokens: Some(0),
        };
        assert_eq!(u.visible_output_tokens(), Some(40));
        assert_eq!(u.reasoning_share(), Some(0.0));
    }

    /// A provider that reports more reasoning than completion contradicts its
    /// own contract. Subtracting would fabricate a visible count, so the split
    /// is reported as unknown instead.
    #[test]
    fn reasoning_above_the_completion_total_is_reported_as_unknown() {
        let u = TokenUsage {
            input_tokens: 10,
            output_tokens: 100,
            cached_input_tokens: 0,
            reasoning_tokens: Some(120),
        };
        assert_eq!(u.visible_output_tokens(), None);
        assert_eq!(u.reasoning_share(), None);
    }

    /// Older payloads have no reasoning key; that must deserialize as unknown
    /// rather than failing or defaulting to a measured zero.
    #[test]
    fn usage_without_a_reasoning_key_deserializes_as_unknown() {
        let u: TokenUsage = serde_json::from_str(
            r#"{"input_tokens":10,"output_tokens":5,"cached_input_tokens":1}"#,
        )
        .unwrap();
        assert_eq!(u.reasoning_tokens, None);
    }

    #[test]
    fn finish_reason_snake_case() {
        let json = serde_json::to_value(FinishReason::ToolCalls).unwrap();
        assert_eq!(json, "tool_calls");
    }
}
