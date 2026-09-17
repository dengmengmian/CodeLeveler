//! The `ModelRuntime` trait — the single seam the agent uses to talk to models
//! (spec §12). Concrete implementations live in `leveler-provider`.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use tokio_util::sync::CancellationToken;

use crate::error::ModelError;
use crate::event::ModelEvent;
use crate::profile::ModelProfile;
use crate::request::{ModelRef, ModelRequest};
use crate::response::ModelResponse;

/// A pinned, boxed stream of normalized model events.
pub type ModelEventStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, ModelError>> + Send>>;

/// Everything the agent needs from a model, expressed model-agnostically.
#[async_trait]
pub trait ModelRuntime: Send + Sync {
    /// Stream a response as normalized [`ModelEvent`]s.
    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError>;

    /// Produce a fully-assembled non-streaming response.
    async fn generate(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError>;

    /// The capability profile for a model.
    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError>;
}

/// Synthesize a normalized event stream from an assembled response — the ONE
/// definition of how a non-streaming response maps onto stream semantics:
/// `MessageStarted` → text/reasoning deltas and completed tool calls in
/// content order → `UsageUpdated` (when any tokens were reported) →
/// `MessageCompleted`. Runtimes without a streaming endpoint and test doubles
/// both use this, so the two response paths cannot drift apart.
pub fn stream_from_response(response: ModelResponse) -> ModelEventStream {
    let mut events: Vec<Result<ModelEvent, ModelError>> = Vec::new();
    events.push(Ok(ModelEvent::MessageStarted {
        request_id: response.request_id,
    }));
    for part in response.message.content {
        match part {
            crate::message::ContentPart::Text { text } => {
                events.push(Ok(ModelEvent::TextDelta { delta: text }));
            }
            crate::message::ContentPart::Reasoning { text } => {
                events.push(Ok(ModelEvent::ReasoningDelta { delta: text }));
            }
            crate::message::ContentPart::ToolCall { call } => {
                events.push(Ok(ModelEvent::ToolCallCompleted { call }));
            }
            // Images and tool results never occur in an assistant response.
            _ => {}
        }
    }
    if response.usage != crate::event::TokenUsage::default() {
        events.push(Ok(ModelEvent::UsageUpdated {
            usage: response.usage,
        }));
    }
    events.push(Ok(ModelEvent::MessageCompleted {
        finish_reason: response.finish_reason,
    }));
    Box::pin(futures::stream::iter(events))
}

#[cfg(test)]
mod stream_from_response_tests {
    use super::*;
    use crate::event::{FinishReason, TokenUsage};
    use crate::message::{ContentPart, Message, Role, ToolCall};
    use crate::response::ModelResponse;
    use futures::StreamExt;
    use leveler_core::{RequestId, ToolCallId};

    fn collect(stream: ModelEventStream) -> Vec<ModelEvent> {
        futures::executor::block_on(stream.map(|e| e.unwrap()).collect())
    }

    #[test]
    fn canonical_order_started_content_usage_completed() {
        let response = ModelResponse {
            request_id: RequestId::generate(),
            message: Message {
                role: Role::Assistant,
                content: vec![
                    ContentPart::Text { text: "hi".into() },
                    ContentPart::ToolCall {
                        call: ToolCall {
                            id: ToolCallId::new("c1"),
                            name: "read_file".into(),
                            arguments: serde_json::json!({"path": "x"}),
                        },
                    },
                ],
            },
            finish_reason: FinishReason::ToolCalls,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..TokenUsage::default()
            },
        };
        let events = collect(stream_from_response(response));
        assert!(matches!(events[0], ModelEvent::MessageStarted { .. }));
        assert!(matches!(&events[1], ModelEvent::TextDelta { delta } if delta == "hi"));
        assert!(
            matches!(&events[2], ModelEvent::ToolCallCompleted { call } if call.name == "read_file")
        );
        assert!(matches!(
            &events[3],
            ModelEvent::UsageUpdated { usage } if usage.input_tokens == 10
        ));
        assert!(matches!(
            events[4],
            ModelEvent::MessageCompleted {
                finish_reason: FinishReason::ToolCalls
            }
        ));
        assert_eq!(events.len(), 5);
    }

    #[test]
    fn zero_usage_is_not_reported() {
        let response = ModelResponse {
            request_id: RequestId::generate(),
            message: Message::text(Role::Assistant, "done"),
            finish_reason: FinishReason::Stop,
            usage: TokenUsage::default(),
        };
        let events = collect(stream_from_response(response));
        assert_eq!(events.len(), 3, "started + delta + completed: {events:?}");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ModelEvent::UsageUpdated { .. }))
        );
    }
}
