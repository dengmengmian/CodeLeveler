//! The `ModelRuntime` trait — the single seam the agent uses to talk to models
//! (spec §12). Concrete implementations live in `leveler-provider`.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use tokio_util::sync::CancellationToken;

use crate::error::ModelError;
use crate::event::ModelEvent;
use crate::profile::ModelProfile;
use crate::provenance::RouteIdentity;
use crate::request::{ModelRef, ModelRequest};
use crate::response::ModelResponse;

/// A pinned, boxed stream of normalized model events.
pub type ModelEventStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, ModelError>> + Send>>;

/// Automatic retries one logical model request may spend after its first
/// attempt fails transiently. The retry lifecycle is sized for this count.
pub const DEFAULT_MAX_RETRIES: u32 = 12;

/// Wall-clock ceiling for one logical request's recovery, measured from its
/// first attempt. It bounds the whole retry lifecycle, so a provider that is
/// slow *and* failing cannot multiply attempts into an unbounded wait.
pub const DEFAULT_RECOVERY_SECONDS: u64 = 600;

/// The bounded recovery budget a route advertises for one logical model
/// request.
///
/// This is the overall bound, not a per-attempt one: a single physical attempt
/// is bounded by the provider's own request/stream timeouts. A retry is only
/// spent on failures the delivery truth classifies as recoverable; a permanent
/// error (`Auth`, `InvalidRequest`, `Decode`, …) ends the request immediately
/// whatever this budget says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryBudget {
    /// Automatic retries after the first attempt of one logical request fails
    /// transiently. `0` disables automatic recovery.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Wall-clock ceiling for the whole recovery, measured from the logical
    /// request's first attempt. `None` = the retry count alone bounds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_seconds: Option<u64>,
}

fn default_max_retries() -> u32 {
    DEFAULT_MAX_RETRIES
}

impl Default for RetryBudget {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            recovery_seconds: Some(DEFAULT_RECOVERY_SECONDS),
        }
    }
}

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

    /// The recovery budget this route advertises for one logical request.
    ///
    /// The route owns its outage tolerance: the default is the product budget,
    /// and a configured provider may raise or lower it. Scripted and in-process
    /// runtimes keep the default rather than inventing a route fact.
    fn retry_budget(&self, _model: &ModelRef) -> RetryBudget {
        RetryBudget::default()
    }

    /// The identity of the route `model` addresses, when this runtime knows it.
    ///
    /// The runtime that assembled an assistant turn stamps it with this so a
    /// later replay can tell the route's own reasoning from another model's.
    /// The default reports "unknown", which is the truth for a scripted or
    /// in-process runtime: an unrecorded route must never be invented.
    fn route_identity(&self, _model: &ModelRef) -> Option<RouteIdentity> {
        None
    }
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
    let canonical_content = response.message.content.clone();
    for part in response.message.content {
        match part {
            crate::message::ContentPart::Text { text } => {
                events.push(Ok(ModelEvent::TextDelta { delta: text }));
            }
            crate::message::ContentPart::Reasoning { text } => {
                events.push(Ok(ModelEvent::ReasoningDelta { delta: text }));
            }
            crate::message::ContentPart::SignedReasoning { text, signature } => {
                if !text.is_empty() {
                    events.push(Ok(ModelEvent::ReasoningDelta { delta: text }));
                }
                if !signature.is_empty() {
                    events.push(Ok(ModelEvent::ReasoningMetadata {
                        bytes: signature.len(),
                    }));
                }
            }
            crate::message::ContentPart::RedactedReasoning { data } => {
                if !data.is_empty() {
                    events.push(Ok(ModelEvent::ReasoningMetadata { bytes: data.len() }));
                }
            }
            crate::message::ContentPart::ToolCall { call } => {
                events.push(Ok(ModelEvent::ToolCallCompleted { call }));
            }
            // Images and tool results never occur in an assistant response.
            _ => {}
        }
    }
    events.push(Ok(ModelEvent::MessageContent {
        content: canonical_content,
    }));
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
                origin: None,
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
                route: None,
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
            &events[4],
            ModelEvent::UsageUpdated { usage } if usage.input_tokens == 10
        ));
        assert!(matches!(
            events[5],
            ModelEvent::MessageCompleted {
                finish_reason: FinishReason::ToolCalls
            }
        ));
        assert_eq!(events.len(), 6);
        assert!(matches!(&events[3], ModelEvent::MessageContent { content } if content.len() == 2));
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
        assert_eq!(
            events.len(),
            4,
            "started + delta + canonical content + completed: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ModelEvent::UsageUpdated { .. }))
        );
    }
}
