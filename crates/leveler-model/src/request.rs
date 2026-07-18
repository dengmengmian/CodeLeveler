//! Unified model request (spec §10.1, §10.2).

use serde::{Deserialize, Serialize};

use leveler_core::{RequestId, SessionId, TurnId};

use crate::message::{Message, ToolChoice, ToolDefinition};
use crate::profile::ReasoningEffort;

/// A provider + model pair. The rest of the system routes on this, never on a
/// bare model-name string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

impl ModelRef {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }

    /// Parse a `provider/model` reference. The model portion may itself contain
    /// slashes; only the first separator splits.
    pub fn parse(reference: &str) -> Option<Self> {
        let (provider, model) = reference.split_once('/')?;
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self::new(provider, model))
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

/// Correlation metadata attached to a request for tracing and persistence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestMetadata {
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
}

/// A fully-formed, provider-agnostic model request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub request_id: RequestId,
    pub model: ModelRef,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Per-request reasoning effort selected by the execution-policy resolver.
    /// `None` falls back to the model profile's recommendation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub stop: Vec<String>,
    #[serde(default)]
    pub metadata: RequestMetadata,
}

impl ModelRequest {
    /// Start a minimal request with a fresh request id.
    pub fn new(model: ModelRef, messages: Vec<Message>) -> Self {
        Self {
            request_id: RequestId::generate(),
            model,
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            reasoning_effort: None,
            stop: Vec::new(),
            metadata: RequestMetadata::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ref_parses_provider_and_model() {
        let r = ModelRef::parse("deepseek/deepseek-chat").unwrap();
        assert_eq!(r.provider, "deepseek");
        assert_eq!(r.model, "deepseek-chat");
        assert_eq!(r.to_string(), "deepseek/deepseek-chat");
    }

    #[test]
    fn model_ref_rejects_malformed() {
        assert!(ModelRef::parse("deepseek").is_none());
        assert!(ModelRef::parse("/model").is_none());
        assert!(ModelRef::parse("provider/").is_none());
    }

    #[test]
    fn model_ref_keeps_trailing_slashes_in_model() {
        let r = ModelRef::parse("openai/org/model").unwrap();
        assert_eq!(r.provider, "openai");
        assert_eq!(r.model, "org/model");
    }

    #[test]
    fn model_request_preserves_a_reasoning_effort_override() {
        let value = serde_json::json!({
            "request_id": "req-test",
            "model": {"provider": "openai", "model": "m"},
            "messages": [],
            "max_output_tokens": null,
            "temperature": null,
            "reasoning_effort": "high"
        });
        let request: ModelRequest = serde_json::from_value(value).unwrap();
        let encoded = serde_json::to_value(request).unwrap();
        assert_eq!(encoded["reasoning_effort"], "high");
    }
}
