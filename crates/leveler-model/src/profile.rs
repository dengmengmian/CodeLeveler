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

/// Hard numeric limits for the model. These describe FACTS about the model —
/// what the provider accepts and what the model can do — never runtime policy.
/// How the runtime USES these facts (fold thresholds, budgets) is
/// `ContextPolicy` in the engine's policy resolver (C5-S1 separation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLimits {
    pub context_window: u32,
    /// A QUALITY declaration, not a runtime cap (C5-S1 migration note): the
    /// point past which long-context recall is expected to degrade. Policy
    /// may knowingly exceed it; nothing may treat it as a hard limit and
    /// refuse a request. Until a model carries a measured `context_quality`,
    /// this is the configured best estimate.
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

/// Measured long-context quality for a model. Only ever filled from a real
/// measurement — `measured_at` names when and forces the question "how do you
/// know?" at review time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextQuality {
    /// Estimated tokens at which recall degradation was first observed.
    pub degradation_onset: u32,
    /// ISO date of the measurement (plus a short method note if useful).
    pub measured_at: String,
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
    ///
    /// Serde's snake_case of this variant is `open_ai_effort`. `openai_effort`
    /// is accepted as an alias because that is the spelling people write.
    #[serde(alias = "openai_effort")]
    OpenAiEffort,
    /// DeepSeek/GLM-style: `thinking: {"type": "enabled"}`, plus
    /// `reasoning_effort` when an effort is configured.
    ThinkingFlag,
}

/// How hard the model should think. Serialized verbatim as `reasoning_effort`.
///
/// `None` is **not** a level: `Option<ReasoningEffort>::None` means “no
/// explicit request / not applicable”, never “turn thinking off”.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ReasoningEffort {
    /// Canonical order, weakest → strongest.
    pub const ALL: [Self; 6] = [
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// The wire value. Kept explicit so a rename of the variant cannot silently
    /// change what we send upstream.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Minimal => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::XHigh => 4,
            Self::Max => 5,
        }
    }
}

/// Why `effective` was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffortSource {
    UserOverride,
    ModelDefault,
}

/// Output of the single reasoning-effort resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedReasoning {
    pub requested: Option<ReasoningEffort>,
    pub default: Option<ReasoningEffort>,
    pub effective: Option<ReasoningEffort>,
    pub source: Option<ReasoningEffortSource>,
}

/// Reasoning request configuration for a model.
///
/// `supported_efforts` is capability. `default_effort` is CodeLeveler policy.
/// The serde alias `effort` keeps existing yaml/toml working.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningConfig {
    #[serde(default)]
    pub style: ReasoningStyle,
    #[serde(default)]
    pub supported_efforts: Vec<ReasoningEffort>,
    #[serde(default, alias = "effort")]
    pub default_effort: Option<ReasoningEffort>,
}

impl ReasoningConfig {
    /// Older call sites that still say `effort`.
    pub fn effort(&self) -> Option<ReasoningEffort> {
        self.default_effort
    }
}

/// Map `requested` onto `supported` (nearest supported ≥ requested, else max).
pub fn normalize_reasoning_effort(
    requested: ReasoningEffort,
    supported: &[ReasoningEffort],
) -> Option<ReasoningEffort> {
    if supported.is_empty() {
        return None;
    }
    let mut ordered: Vec<ReasoningEffort> = supported.to_vec();
    ordered.sort_by_key(|e| e.rank());
    ordered.dedup();
    ordered
        .iter()
        .copied()
        .find(|e| e.rank() >= requested.rank())
        .or_else(|| ordered.last().copied())
}

/// Resolve requested vs CodeLeveler default vs supported set.
///
/// Does not invent an effort when the model has no controllable knob
/// (`supported` empty and no default).
pub fn resolve_reasoning_effort(
    requested: Option<ReasoningEffort>,
    config: &ReasoningConfig,
) -> ResolvedReasoning {
    let supported = if config.supported_efforts.is_empty() {
        config.default_effort.map(|e| vec![e]).unwrap_or_default()
    } else {
        config.supported_efforts.clone()
    };
    let default = config.default_effort;
    if supported.is_empty() {
        // No capability set: do not invent a remap, but do not drop an
        // explicit request either (eval overrides / incomplete custom TOML).
        let (effective, source) = match requested {
            Some(req) => (Some(req), Some(ReasoningEffortSource::UserOverride)),
            None => (
                default,
                default.map(|_| ReasoningEffortSource::ModelDefault),
            ),
        };
        return ResolvedReasoning {
            requested,
            default,
            effective,
            source,
        };
    }
    let (pick, source) = match requested {
        Some(req) => (req, ReasoningEffortSource::UserOverride),
        None => match default {
            Some(def) => (def, ReasoningEffortSource::ModelDefault),
            None => {
                return ResolvedReasoning {
                    requested,
                    default,
                    effective: None,
                    source: None,
                };
            }
        },
    };
    ResolvedReasoning {
        requested,
        default,
        effective: normalize_reasoning_effort(pick, &supported),
        source: Some(source),
    }
}

/// Load-time checks. `reasoning_capable` is `ModelCapabilities.reasoning`.
pub fn validate_reasoning_config(
    reasoning_capable: bool,
    config: &ReasoningConfig,
) -> Result<(), String> {
    if !reasoning_capable {
        if !config.supported_efforts.is_empty() || config.default_effort.is_some() {
            return Err("reasoning is disabled but supported_efforts/default_effort is set".into());
        }
        return Ok(());
    }
    match config.style {
        ReasoningStyle::None => {
            if !config.supported_efforts.is_empty() || config.default_effort.is_some() {
                return Err(
                    "style=none (no effort knob) cannot declare supported_efforts or default_effort"
                        .into(),
                );
            }
            Ok(())
        }
        ReasoningStyle::OpenAiEffort | ReasoningStyle::ThinkingFlag => {
            let supported = if config.supported_efforts.is_empty() {
                match config.default_effort {
                    Some(def) => vec![def],
                    None => {
                        return Err(
                            "reasoning model must declare supported_efforts or default_effort"
                                .into(),
                        );
                    }
                }
            } else {
                config.supported_efforts.clone()
            };
            let Some(default) = config.default_effort else {
                return Err("reasoning model must declare default_effort".into());
            };
            if !supported.contains(&default) {
                return Err(format!(
                    "default_effort `{}` is not in supported_efforts",
                    default.as_wire()
                ));
            }
            Ok(())
        }
    }
}

/// Provider-quirk configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityConfig {
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
    /// Whether the provider accepts a *forced* `tool_choice` (`required` or a
    /// named function) while thinking mode is active. DeepSeek's endpoint
    /// rejects that combination with a hard 400 ("Thinking mode does not
    /// support this tool_choice") — and thinking is its server-side *default*,
    /// so omitting the `thinking` field does not avoid the rejection. Set
    /// false to have the thinking-flag protocol adapter send an explicit
    /// `thinking: {"type": "disabled"}` on exactly those requests, preserving
    /// the forced ToolChoice contract instead of downgrading it.
    #[serde(default = "default_true")]
    pub thinking_supports_forced_tool_choice: bool,
    /// Whether the provider requires `reasoning_content` echoed back on
    /// assistant tool-call messages. DeepSeek's thinking mode validates the
    /// tool-call *id*: an id it does not recognize as one of its own
    /// thinking-mode generations (a foreign id, or an id generated while
    /// thinking was explicitly disabled) is rejected with HTTP 400 ("The
    /// `reasoning_content` in the thinking mode must be passed back to the
    /// API") unless the message carries a `reasoning_content` key — the
    /// captured reasoning, or the empty string when the round produced none.
    /// Measured 2026-08-07. Default false: the field is never sent.
    #[serde(default)]
    pub passback_reasoning_content: bool,
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
            synthesize_tool_call_ids: false,
            drop_unsupported_fields: false,
            supports_temperature: true,
            thinking_supports_forced_tool_choice: true,
            passback_reasoning_content: false,
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
    /// Measured long-context quality, when someone has actually measured it
    /// (C5 spec §Model Capability Layer). Absent means "not measured" — the
    /// field must never be filled from guesswork, which is why it carries its
    /// measurement date. Consumers fall back to `limits.reliable_context`.
    #[serde(default)]
    pub context_quality: Option<ContextQuality>,
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
            context_quality: None,
            reasoning: ReasoningConfig {
                style: ReasoningStyle::ThinkingFlag,
                supported_efforts: vec![ReasoningEffort::High, ReasoningEffort::Max],
                default_effort: Some(ReasoningEffort::High),
            },
            compatibility: CompatibilityConfig {
                synthesize_tool_call_ids: true,
                drop_unsupported_fields: false,
                // Non-default on purpose: a roundtrip that only ever sees the
                // default value can't catch a field dropped from (de)serialization.
                supports_temperature: false,
                thinking_supports_forced_tool_choice: false,
                passback_reasoning_content: true,
            },
            // Likewise non-default: a model's own prompt must survive the trip.
            instructions: Some("You are a terse agent.".to_string()),
            pricing: None,
        };
        let json = serde_json::to_string(&profile).unwrap();
        let back: ModelProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, profile);
    }

    /// Every profile written before the flag existed must keep its exact
    /// behavior: forced tool_choice stays compatible with thinking by default.
    #[test]
    fn legacy_compatibility_block_defaults_to_forced_choice_compatible() {
        let compat: CompatibilityConfig = serde_json::from_value(serde_json::json!({
            "synthesize_tool_call_ids": true
        }))
        .unwrap();
        assert!(compat.thinking_supports_forced_tool_choice);
        assert!(compat.supports_temperature);
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
        assert!(!config.synthesize_tool_call_ids);
        assert!(!config.drop_unsupported_fields);
    }

    #[test]
    fn effort_alias_still_loads_as_default_effort() {
        let cfg: ReasoningConfig = serde_json::from_value(serde_json::json!({
            "style": "thinking_flag",
            "effort": "max"
        }))
        .unwrap();
        assert_eq!(cfg.default_effort, Some(ReasoningEffort::Max));
        assert_eq!(cfg.effort(), Some(ReasoningEffort::Max));
    }

    #[test]
    fn reasoning_effort_order_is_canonical() {
        let ranks: Vec<u8> = ReasoningEffort::ALL.iter().map(|e| e.rank()).collect();
        let mut sorted = ranks.clone();
        sorted.sort();
        assert_eq!(ranks, sorted);
        assert!(ReasoningEffort::Minimal.rank() < ReasoningEffort::Low.rank());
        assert!(ReasoningEffort::High.rank() < ReasoningEffort::XHigh.rank());
        assert!(ReasoningEffort::XHigh.rank() < ReasoningEffort::Max.rank());
    }

    #[test]
    fn resolve_exact_match() {
        let cfg = ReasoningConfig {
            style: ReasoningStyle::OpenAiEffort,
            supported_efforts: vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
            default_effort: Some(ReasoningEffort::Medium),
        };
        let r = resolve_reasoning_effort(Some(ReasoningEffort::Medium), &cfg);
        assert_eq!(r.effective, Some(ReasoningEffort::Medium));
        assert_eq!(r.source, Some(ReasoningEffortSource::UserOverride));
    }

    #[test]
    fn resolve_upgrades_to_nearest_supported() {
        let cfg = ReasoningConfig {
            style: ReasoningStyle::ThinkingFlag,
            supported_efforts: vec![ReasoningEffort::High, ReasoningEffort::Max],
            default_effort: Some(ReasoningEffort::High),
        };
        let r = resolve_reasoning_effort(Some(ReasoningEffort::Medium), &cfg);
        assert_eq!(r.effective, Some(ReasoningEffort::High));
    }

    #[test]
    fn resolve_clamps_above_supported_max() {
        let cfg = ReasoningConfig {
            style: ReasoningStyle::OpenAiEffort,
            supported_efforts: vec![ReasoningEffort::Low, ReasoningEffort::High],
            default_effort: Some(ReasoningEffort::High),
        };
        let r = resolve_reasoning_effort(Some(ReasoningEffort::Max), &cfg);
        assert_eq!(r.effective, Some(ReasoningEffort::High));
    }

    #[test]
    fn resolve_none_uses_model_default() {
        let cfg = ReasoningConfig {
            style: ReasoningStyle::ThinkingFlag,
            supported_efforts: vec![ReasoningEffort::High, ReasoningEffort::Max],
            default_effort: Some(ReasoningEffort::High),
        };
        let r = resolve_reasoning_effort(None, &cfg);
        assert_eq!(r.effective, Some(ReasoningEffort::High));
        assert_eq!(r.source, Some(ReasoningEffortSource::ModelDefault));
    }

    #[test]
    fn validate_rejects_default_outside_supported() {
        let cfg = ReasoningConfig {
            style: ReasoningStyle::ThinkingFlag,
            supported_efforts: vec![ReasoningEffort::High, ReasoningEffort::Max],
            default_effort: Some(ReasoningEffort::Medium),
        };
        let err = validate_reasoning_config(true, &cfg).unwrap_err();
        assert!(err.contains("not in supported_efforts"), "{err}");
    }

    #[test]
    fn validate_allows_always_on_style_none() {
        let cfg = ReasoningConfig {
            style: ReasoningStyle::None,
            supported_efforts: vec![],
            default_effort: None,
        };
        validate_reasoning_config(true, &cfg).unwrap();
    }

    #[test]
    fn openai_effort_alias_parses() {
        let a: ReasoningStyle = serde_json::from_value(serde_json::json!("openai_effort")).unwrap();
        let b: ReasoningStyle =
            serde_json::from_value(serde_json::json!("open_ai_effort")).unwrap();
        assert_eq!(a, ReasoningStyle::OpenAiEffort);
        assert_eq!(b, ReasoningStyle::OpenAiEffort);
    }

    #[test]
    fn resolve_upgrades_xhigh_to_max_when_xhigh_is_absent() {
        let cfg = ReasoningConfig {
            style: ReasoningStyle::ThinkingFlag,
            supported_efforts: vec![ReasoningEffort::High, ReasoningEffort::Max],
            default_effort: Some(ReasoningEffort::High),
        };
        let r = resolve_reasoning_effort(Some(ReasoningEffort::XHigh), &cfg);
        assert_eq!(r.effective, Some(ReasoningEffort::Max));
    }

    #[test]
    fn validate_rejects_efforts_when_reasoning_disabled() {
        let cfg = ReasoningConfig {
            style: ReasoningStyle::None,
            supported_efforts: vec![ReasoningEffort::High],
            default_effort: Some(ReasoningEffort::High),
        };
        let err = validate_reasoning_config(false, &cfg).unwrap_err();
        assert!(err.contains("reasoning is disabled"), "{err}");
    }

    #[test]
    fn validate_rejects_style_none_with_default() {
        let cfg = ReasoningConfig {
            style: ReasoningStyle::None,
            supported_efforts: vec![],
            default_effort: Some(ReasoningEffort::High),
        };
        let err = validate_reasoning_config(true, &cfg).unwrap_err();
        assert!(err.contains("style=none"), "{err}");
    }

    #[test]
    fn resolve_without_supported_passes_through_request() {
        let cfg = ReasoningConfig {
            style: ReasoningStyle::None,
            supported_efforts: vec![],
            default_effort: None,
        };
        let r = resolve_reasoning_effort(Some(ReasoningEffort::Max), &cfg);
        assert_eq!(r.effective, Some(ReasoningEffort::Max));
        assert_eq!(r.source, Some(ReasoningEffortSource::UserOverride));
        let none = resolve_reasoning_effort(None, &cfg);
        assert_eq!(none.effective, None);
    }
}
