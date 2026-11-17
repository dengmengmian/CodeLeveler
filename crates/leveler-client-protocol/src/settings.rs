//! The runtime's effective settings, projected for a client.
//!
//! This is a projection, never a second configuration store. Every value is
//! read from the authority that already owns it — the resolved configuration
//! generation, the session row, or the client's own local preference — and the
//! client sends the existing typed command to change an editable value.
//!
//! The response deliberately carries no secret: provider API keys appear as a
//! boolean "configured" fact and the name of the environment variable, never a
//! value.

use serde::{Deserialize, Serialize};

use leveler_model::{ModelRef, ThinkingLevel};

/// Where a settings value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiSettingSource {
    /// The user-global `config.toml`.
    GlobalConfig,
    /// The project's configuration bundle.
    ProjectConfig,
    /// An environment variable (never its value).
    Environment,
    /// The current session row or override.
    Session,
    /// A built-in default because no source set the value.
    Default,
    /// A client-local preference (for example the TUI theme).
    ClientLocal,
    /// The runtime cannot attribute the value to a source.
    Unknown,
}

/// What a value applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiSettingScope {
    Global,
    Session,
    Client,
}

/// When an edit takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiSettingEffect {
    /// The next request or render uses the new value.
    Immediate,
    /// The next turn uses the new value; the running turn keeps its generation.
    NextTurn,
    /// A runtime restart is required.
    Restart,
    /// Only the client's presentation changes.
    ClientOnly,
}

/// One configured provider, without secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiProviderSetting {
    pub id: String,
    pub protocol: String,
    /// A display-safe base URL (credentials and query removed).
    pub base_url: String,
    /// The environment variable name the key is read from, when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Whether a key is resolvable right now. The value itself never leaves
    /// the runtime.
    pub api_key_configured: bool,
    /// How many model entries this provider serves.
    pub model_count: usize,
    /// Where this provider entry was configured.
    pub source: UiSettingSource,
}

/// One configuration source the runtime reads, in precedence order (later
/// entries override earlier ones).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiConfigSource {
    pub path: String,
    /// `global`, `project` or `environment`.
    pub kind: String,
    pub present: bool,
    /// 0 = lowest precedence.
    pub precedence: u32,
}

/// The runtime's effective settings projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiSettingsSnapshot {
    /// RFC3339, from the runtime clock.
    pub generated_at: String,
    /// The configuration generation these values describe.
    pub config_version: u64,
    /// The user-global config path, when the runtime knows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    /// The project configuration directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_config_dir: Option<String>,
    /// The explicitly configured default model, unresolved text as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// The default model resolved against this environment's registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_default_model: Option<ModelRef>,
    /// The global Thinking Level configured for this environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_default: Option<ThinkingLevel>,
    /// The global TUI theme id configured for future clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_default: Option<String>,
    /// The global soft-compaction percent, when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_soft_percent: Option<u8>,
    pub providers: Vec<UiProviderSetting>,
    pub sources: Vec<UiConfigSource>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_settings_never_carry_a_key_value() {
        let provider = UiProviderSetting {
            id: "deepseek".into(),
            protocol: "openai_chat".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            api_key_env: Some("DEEPSEEK_API_KEY".into()),
            api_key_configured: true,
            model_count: 2,
            source: UiSettingSource::GlobalConfig,
        };
        let json = serde_json::to_string(&provider).unwrap();
        assert!(!json.contains("sk-"), "{json}");
        assert!(json.contains("\"api_key_configured\":true"));
    }

    #[test]
    fn a_snapshot_roundtrips_without_optional_fields() {
        let snapshot = UiSettingsSnapshot {
            generated_at: "2026-11-17T00:00:00Z".into(),
            config_version: 7,
            config_path: None,
            project_config_dir: None,
            default_model: None,
            resolved_default_model: None,
            thinking_default: None,
            theme_default: None,
            context_soft_percent: None,
            providers: Vec::new(),
            sources: Vec::new(),
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("default_model"), "{json}");
        let back: UiSettingsSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snapshot);
    }
}
