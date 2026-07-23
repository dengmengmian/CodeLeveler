//! Model profile — *what a model can do* (spec §16). Loaded from config; never
//! hard-coded in business logic.

use serde::{Deserialize, Serialize};

/// The wire protocol a model speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolKind {
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    #[serde(rename = "gemini_generate_content")]
    GeminiGenerateContent,
}

/// Capability flags describing what the model supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub parallel_tool_calls: bool,
    pub structured_output: bool,
    pub reasoning: bool,
    pub vision: bool,
}

/// Hard numeric limits for the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLimits {
    pub context_window: u32,
    pub reliable_context: u32,
    pub max_output_tokens: u32,
    pub max_tool_schema_bytes: usize,
    pub max_parallel_tool_calls: usize,
    /// Per-model byte budget for a single tool result (the executor's central
    /// cap). Omitted → the global default (48 KiB). Configure lower for weak
    /// models with small reliable contexts.
    #[serde(default)]
    pub max_tool_output_bytes: Option<usize>,
}

/// How the provider expects a reasoning request to be spelled on the wire.
/// Capability (`ModelCapabilities::reasoning`) says the model *can* reason;
/// this says how to *ask* it to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningStyle {
    /// Send nothing. The model either has no knob or reasons unconditionally.
    #[default]
    None,
    /// OpenAI-style: a top-level `reasoning_effort` string.
    OpenAiEffort,
    /// DeepSeek/GLM-style: `thinking: {"type": "enabled"}`, plus
    /// `reasoning_effort` when an effort is configured.
    ThinkingFlag,
}

/// How hard the model should think. Serialized verbatim as `reasoning_effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Max,
}

impl ReasoningEffort {
    /// The wire value. Kept explicit so a rename of the variant cannot silently
    /// change what we send upstream.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

/// Reasoning request configuration for a model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningConfig {
    #[serde(default)]
    pub style: ReasoningStyle,
    /// Omitted from the request when `None`, letting the provider default apply.
    #[serde(default)]
    pub effort: Option<ReasoningEffort>,
}

/// Provider-quirk configuration consumed by the compatibility middleware.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityConfig {
    /// Middleware names to apply, in order.
    #[serde(default)]
    pub middleware: Vec<String>,
    /// Synthesize stable tool-call ids when the provider omits them.
    #[serde(default)]
    pub synthesize_tool_call_ids: bool,
    /// Drop fields the provider rejects rather than erroring.
    #[serde(default)]
    pub drop_unsupported_fields: bool,
    /// Whether the provider accepts a caller-chosen `temperature`. Kimi For
    /// Coding rejects every value but its own default, so a caller asking for a
    /// deterministic `0.0` would get a hard 400. Set false to omit the field.
    #[serde(default = "default_true")]
    pub supports_temperature: bool,
}

fn default_true() -> bool {
    true
}

// Hand-written (not derived) so `supports_temperature` defaults to true: a
// derived Default would make it false and silently drop temperature for every
// provider that does accept it.
impl Default for CompatibilityConfig {
    fn default() -> Self {
        Self {
            middleware: Vec::new(),
            synthesize_tool_call_ids: false,
            drop_unsupported_fields: false,
            supports_temperature: true,
        }
    }
}

/// The full description of a model, loaded from `configs/models/*.yaml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelProfile {
    pub id: String,
    pub provider: String,
    pub model_id: String,
    pub protocol: ProtocolKind,
    pub capabilities: ModelCapabilities,
    pub limits: ModelLimits,
    #[serde(default)]
    pub reasoning: ReasoningConfig,
    #[serde(default)]
    pub compatibility: CompatibilityConfig,
    /// This model's own system prompt, replacing the agent's default. One prompt
    /// cannot serve every model: a weak model needs the long form with worked
    /// examples, and a strong one is degraded by that same verbosity. Omit to
    /// use the default.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Optional provider pricing for cost accounting. Absent means cost is
    /// never computed — the harness must not invent a price.
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
}

/// Provider pricing in USD per million tokens, as billed by the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
}

impl ModelPricing {
    /// Total cost in micro-USD. USD-per-million-tokens is numerically equal to
    /// micro-USD per token, so this is a plain weighted sum, rounded.
    pub fn cost_usd_micros(&self, input_tokens: u64, output_tokens: u64) -> u64 {
        let micros = input_tokens as f64 * self.input_usd_per_mtok
            + output_tokens as f64 * self.output_usd_per_mtok;
        micros.round().max(0.0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// USD/Mtok is micro-USD/token: deepseek-style $0.27/$1.10 per Mtok over
    /// 1M in + 100k out = 270_000 + 110_000 micro-USD.
    #[test]
    fn pricing_cost_is_a_weighted_sum_in_micro_usd() {
        let pricing = ModelPricing {
            input_usd_per_mtok: 0.27,
            output_usd_per_mtok: 1.10,
        };
        assert_eq!(pricing.cost_usd_micros(1_000_000, 100_000), 380_000);
        assert_eq!(pricing.cost_usd_micros(0, 0), 0);
        // Sub-micro amounts round to nearest, not truncate to zero.
        assert_eq!(pricing.cost_usd_micros(3, 0), 1);
    }

    /// `pricing` is optional in profile files and defaults to absent — old
    /// profiles keep loading, and no price is ever invented.
    #[test]
    fn profile_pricing_defaults_absent() {
        let json = serde_json::json!({
            "id": "m",
            "provider": "p",
            "model_id": "x",
            "protocol": "openai_chat",
            "capabilities": {
                "streaming": true, "tool_calling": true, "parallel_tool_calls": false,
                "structured_output": false, "reasoning": false, "vision": false
            },
            "limits": {
                "context_window": 1000, "max_output_tokens": 100,
                "max_tool_schema_bytes": 1000, "max_parallel_tool_calls": 1,
                "reliable_context": 800
            }
        });
        let profile: ModelProfile = serde_json::from_value(json).unwrap();
        assert_eq!(profile.pricing, None);
    }

    #[test]
    fn protocol_kind_snake_case() {
        assert_eq!(
            serde_json::to_value(ProtocolKind::OpenAiChat).unwrap(),
            "openai_chat"
        );
    }

    #[test]
    fn all_protocol_kinds_roundtrip() {
        for kind in [
            ProtocolKind::OpenAiChat,
            ProtocolKind::OpenAiResponses,
            ProtocolKind::AnthropicMessages,
            ProtocolKind::GeminiGenerateContent,
        ] {
            let value = serde_json::to_value(kind).unwrap();
            let back: ProtocolKind = serde_json::from_value(value).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn model_capabilities_defaults_to_false() {
        let caps = ModelCapabilities {
            streaming: false,
            tool_calling: false,
            parallel_tool_calls: false,
            structured_output: false,
            reasoning: false,
            vision: false,
        };
        assert!(!caps.streaming);
        assert!(!caps.vision);
    }

    #[test]
    fn model_profile_roundtrips_through_serde() {
        let profile = ModelProfile {
            id: "openai/gpt-4o".to_string(),
            provider: "openai".to_string(),
            model_id: "gpt-4o".to_string(),
            protocol: ProtocolKind::OpenAiChat,
            capabilities: ModelCapabilities {
                streaming: true,
                tool_calling: true,
                parallel_tool_calls: true,
                structured_output: true,
                reasoning: false,
                vision: true,
            },
            limits: ModelLimits {
                context_window: 128_000,
                reliable_context: 64_000,
                max_output_tokens: 4096,
                max_tool_schema_bytes: 32_768,
                max_parallel_tool_calls: 16,
                max_tool_output_bytes: None,
            },
            reasoning: ReasoningConfig {
                style: ReasoningStyle::ThinkingFlag,
                effort: Some(ReasoningEffort::High),
            },
            compatibility: CompatibilityConfig {
                middleware: vec!["drop_temperature".to_string()],
                synthesize_tool_call_ids: true,
                drop_unsupported_fields: false,
                // Non-default on purpose: a roundtrip that only ever sees the
                // default value can't catch a field dropped from (de)serialization.
                supports_temperature: false,
            },
            // Likewise non-default: a model's own prompt must survive the trip.
            instructions: Some("You are a terse agent.".to_string()),
            pricing: None,
        };
        let json = serde_json::to_string(&profile).unwrap();
        let back: ModelProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, profile);
    }

    /// `instructions` is optional: every existing model config predates it and
    /// must keep loading, falling back to the agent's default prompt.
    #[test]
    fn a_profile_without_instructions_still_loads() {
        let profile: ModelProfile = serde_json::from_value(serde_json::json!({
            "id": "m", "provider": "mock", "model_id": "m", "protocol": "openai_chat",
            "capabilities": {
                "streaming": true, "tool_calling": true, "parallel_tool_calls": false,
                "structured_output": true, "reasoning": false, "vision": false
            },
            "limits": {
                "context_window": 8192, "reliable_context": 4096, "max_output_tokens": 1024,
                "max_tool_schema_bytes": 8192, "max_parallel_tool_calls": 1
            }
        }))
        .unwrap();

        assert_eq!(profile.instructions, None);
    }

    #[test]
    fn compatibility_config_defaults_are_empty() {
        let config: CompatibilityConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(config.middleware.is_empty());
        assert!(!config.synthesize_tool_call_ids);
        assert!(!config.drop_unsupported_fields);
    }
}
