//! One physical model invocation, including failures. Answer content and task
//! spend are different facts: retries never erase an earlier invocation.
use crate::{FinishReason, ModelError, TokenUsage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelAttempt {
    pub request_id: String,
    /// One-based ordinal within the logical round.
    pub attempt: u32,
    /// None means the provider supplied no usage, not a free invocation.
    pub usage: Option<TokenUsage>,
    pub finish_reason: Option<FinishReason>,
    pub error: Option<ModelError>,
    /// Text already emitted to the observer before this invocation failed.
    /// This is incomplete output, never a successful response or executable
    /// tool content. Successful attempts leave it empty to avoid a second
    /// persistence path for the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_text: Option<String>,
    pub latency_ms: u64,
    pub connect_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub max_event_gap_ms: Option<u64>,
    pub cost_usd_micros: Option<u64>,
    /// Admission-only fallback, never reported provider usage.
    pub estimated_tokens: Option<u64>,
    pub projected_input_tokens: u64,
}
