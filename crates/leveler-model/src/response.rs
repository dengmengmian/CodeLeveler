//! Unified non-streaming response.

use serde::{Deserialize, Serialize};

use leveler_core::RequestId;

use crate::event::{FinishReason, TokenUsage};
use crate::message::Message;

/// A complete model response assembled either from a non-streaming call or by
/// folding a stream of [`crate::event::ModelEvent`]s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub request_id: RequestId,
    pub message: Message,
    pub finish_reason: FinishReason,
    pub usage: TokenUsage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{FinishReason, TokenUsage};
    use crate::message::{Message, Role};

    #[test]
    fn model_response_roundtrips_through_serde() {
        let response = ModelResponse {
            request_id: RequestId::new("req-1"),
            message: Message::text(Role::Assistant, "hello"),
            finish_reason: FinishReason::Stop,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 0,
            },
        };
        let json = serde_json::to_string(&response).unwrap();
        let back: ModelResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, response);
    }

    #[test]
    fn model_response_serializes_request_id() {
        let response = ModelResponse {
            request_id: RequestId::new("req-1"),
            message: Message::text(Role::Assistant, "hello"),
            finish_reason: FinishReason::ToolCalls,
            usage: TokenUsage::default(),
        };
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["request_id"], "req-1");
        assert_eq!(value["finish_reason"], "tool_calls");
    }
}
