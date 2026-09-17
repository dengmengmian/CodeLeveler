//! Model catalog loading.
//!
//! A model config file is a [`leveler_model::ModelProfile`] on disk. Model
//! policy tiers are retired; the `policy` key is kept only as a tombstone so a
//! leftover binding fails loudly instead of being silently ignored.

use std::path::Path;

use serde::{Deserialize, Serialize};

use leveler_model::ModelProfile;

use crate::config::ConfigError;

/// On-disk model definition: the profile (plus a tombstone for the retired
/// `policy` binding — present only so old configs fail with a clear pointer).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelConfigFile {
    #[serde(flatten)]
    pub profile: ModelProfile,
    /// RETIRED. Model→policy-tier bindings no longer exist; any value here is
    /// a hard config error (`load_model_config`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
}

fn load_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;
    serde_yaml::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })
}

/// Load a model config file. A leftover `policy:` tier binding is a hard
/// error — the tiers are retired and silently ignoring the key would let a
/// user believe a binding is in effect.
pub fn load_model_config(path: &Path) -> Result<ModelConfigFile, ConfigError> {
    let cfg: ModelConfigFile = load_yaml(path)?;
    if let Some(policy) = &cfg.policy {
        return Err(ConfigError::RetiredKey {
            path: path.display().to_string(),
            key: "policy".to_string(),
            reason: format!(
                "model policy tiers are retired, `policy: {policy}` has no effect — \
                 remove the key; execution configuration is resolved from model \
                 facts, role, task and safety constraints"
            ),
        });
    }
    if let Err(reason) = leveler_model::validate_reasoning_config(
        cfg.profile.capabilities.reasoning,
        &cfg.profile.reasoning,
    ) {
        return Err(ConfigError::InvalidReasoning {
            path: path.display().to_string(),
            reason,
        });
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_model::ProtocolKind;

    #[test]
    fn parses_model_file_without_policy_key() {
        let yaml = r#"
id: default
provider: deepseek
model_id: deepseek-chat
protocol: openai_chat
capabilities:
  streaming: true
  tool_calling: true
  parallel_tool_calls: false
  structured_output: true
  reasoning: false
  vision: false
limits:
  context_window: 65536
  reliable_context: 32000
  max_output_tokens: 8192
  max_tool_schema_bytes: 32768
  max_parallel_tool_calls: 1
compatibility:
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#;
        let cfg: ModelConfigFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.profile.id, "default");
        assert_eq!(cfg.profile.provider, "deepseek");
        assert_eq!(cfg.profile.protocol, ProtocolKind::OpenAiChat);
        assert!(cfg.profile.capabilities.tool_calling);
        assert_eq!(cfg.policy, None);
    }

    /// A user can configure a native Claude model end-to-end just by naming the
    /// `anthropic_messages` protocol — the adapter is wired for it (registry
    /// `adapter_for`), so this parse must round-trip to the right ProtocolKind.
    #[test]
    fn parses_anthropic_messages_protocol() {
        let yaml = r#"
id: claude
provider: anthropic
model_id: claude-sonnet-5
protocol: anthropic_messages
capabilities:
  streaming: true
  tool_calling: true
  parallel_tool_calls: true
  structured_output: true
  reasoning: true
  vision: true
limits:
  context_window: 200000
  reliable_context: 180000
  max_output_tokens: 8192
  max_tool_schema_bytes: 32768
  max_parallel_tool_calls: 4
compatibility:
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#;
        let cfg: ModelConfigFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.profile.provider, "anthropic");
        assert_eq!(cfg.profile.protocol, ProtocolKind::AnthropicMessages);
    }

    /// Model tiers are retired: a leftover `policy:` binding must fail loudly
    /// with a pointer to the migration doc, never be silently ignored.
    #[test]
    fn model_file_with_retired_policy_binding_errors_with_migration_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bound.yaml");
        std::fs::write(
            &path,
            r#"
id: bound
provider: deepseek
model_id: deepseek-chat
protocol: openai_chat
policy: weak
capabilities:
  streaming: true
  tool_calling: true
  parallel_tool_calls: false
  structured_output: true
  reasoning: false
  vision: false
limits:
  context_window: 65536
  reliable_context: 32000
  max_output_tokens: 8192
  max_tool_schema_bytes: 32768
  max_parallel_tool_calls: 1
"#,
        )
        .unwrap();
        let err = load_model_config(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("retired") || msg.contains("policy tiers"),
            "error must explain retirement: {msg}"
        );
        assert!(
            msg.contains("policy"),
            "error must name the retired key: {msg}"
        );
    }

    #[test]
    fn invalid_default_effort_is_a_load_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(
            &path,
            r#"
id: bad
provider: deepseek
model_id: deepseek-v4-pro
protocol: openai_chat
capabilities:
  streaming: true
  tool_calling: true
  parallel_tool_calls: false
  structured_output: true
  reasoning: true
  vision: false
reasoning:
  style: thinking_flag
  supported_efforts: [high, max]
  default_effort: medium
limits:
  context_window: 1000
  reliable_context: 800
  max_output_tokens: 100
  max_tool_schema_bytes: 1000
  max_parallel_tool_calls: 1
"#,
        )
        .unwrap();
        let err = load_model_config(&path).unwrap_err().to_string();
        assert!(err.contains("not in supported_efforts"), "{err}");
    }

    /// Built-in profiles are the product contract: a reasoning-capable model
    /// must declare a determinate CodeLeveler default (or explicitly have no
    /// effort knob). Forgetting either fails CI.
    #[test]
    fn builtin_reasoning_models_declare_supported_and_default() {
        use leveler_model::{ReasoningStyle, resolve_reasoning_effort};

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs/models");
        let mut seen = 0usize;
        let mut reasoning = 0usize;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
                continue;
            }
            let cfg =
                load_model_config(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            seen += 1;
            let profile = &cfg.profile;
            if !profile.capabilities.reasoning {
                continue;
            }
            reasoning += 1;
            match profile.reasoning.style {
                ReasoningStyle::None => {
                    assert!(
                        profile.reasoning.supported_efforts.is_empty()
                            && profile.reasoning.default_effort.is_none(),
                        "{}: style=none cannot invent an effort ({:?})",
                        profile.id,
                        profile.reasoning
                    );
                    let resolved = resolve_reasoning_effort(None, &profile.reasoning);
                    assert_eq!(
                        resolved.effective, None,
                        "{}: always-on models have no effective effort",
                        profile.id
                    );
                }
                ReasoningStyle::OpenAiEffort | ReasoningStyle::ThinkingFlag => {
                    assert!(
                        !profile.reasoning.supported_efforts.is_empty(),
                        "{}: reasoning model must declare supported_efforts",
                        profile.id
                    );
                    let default = profile.reasoning.default_effort.unwrap_or_else(|| {
                        panic!(
                            "{}: reasoning model must declare default_effort",
                            profile.id
                        )
                    });
                    assert!(
                        profile.reasoning.supported_efforts.contains(&default),
                        "{}: default {:?} not in {:?}",
                        profile.id,
                        default,
                        profile.reasoning.supported_efforts
                    );
                    let resolved = resolve_reasoning_effort(None, &profile.reasoning);
                    assert_eq!(
                        resolved.effective,
                        Some(default),
                        "{}: default must resolve to itself",
                        profile.id
                    );
                }
            }
        }
        assert!(seen > 0, "expected yaml files in {}", dir.display());
        assert!(
            reasoning > 0,
            "expected at least one reasoning-capable builtin"
        );
    }
}
