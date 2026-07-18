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
  middleware: []
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
}
