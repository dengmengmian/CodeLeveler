//! Unified streaming events (spec §11). Raw provider SSE is normalized into
//! this enum before any upper layer sees it.

use serde::{Deserialize, Serialize};

use leveler_core::{RequestId, ToolCallId};

use crate::error::ModelError;
use crate::message::ToolCall;

/// Token accounting reported by the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Input tokens served from the provider's prefix cache. A subset of
    /// `input_tokens`, billed at a fraction of the price. Zero when the provider
    /// does not report it — never assume "no cache" from a zero here.
    #[serde(default)]
    pub cached_input_tokens: u64,
}

impl TokenUsage {
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
        };
        assert_eq!(u.total(), 15);
    }

    #[test]
    fn finish_reason_snake_case() {
        let json = serde_json::to_value(FinishReason::ToolCalls).unwrap();
        assert_eq!(json, "tool_calls");
    }
}
