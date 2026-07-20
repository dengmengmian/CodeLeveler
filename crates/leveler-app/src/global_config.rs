//! Global, single-file user config (`~/.leveler/config.toml`).
//!
//! A machine-global resident config so `leveler` works in any directory without a
//! per-repo `configs/` bundle. It defines the default model plus one or more
//! providers/models in one compact TOML. API keys may be set via
//! `api_key_env` (environment variable name) and/or plaintext `api_key`
//! (local convenience; preferred when non-empty). It is loaded first and then
//! the repo's `configs/` bundle (if any) is merged over it, so a project can
//! still override or add models.
//!
//! Example `~/.leveler/config.toml`:
//! ```toml
//! default_model = "deepseek/deepseek-v4-pro"
//! lang = "zh"                 # optional UI language: zh | en (LEVELER_LANG overrides)
//!
//! [ui]
//! theme = "ion"                # optional TUI theme: ion | night | day
//!
//! [vcs]
//! co_author = true             # optional; append the CodeLeveler/model commit trailer
//!
//! [providers.deepseek]
//! base_url = "https://api.deepseek.com"
//! api_key_env = "DEEPSEEK_API_KEY"
//! # api_key = "sk-..."        # optional plaintext; preferred over api_key_env
//!
//! [models."deepseek-v4-pro"]
//! provider = "deepseek"
//! context_window = 131072
//! max_output_tokens = 16384   # optional; default 8192
//! # parallel_tool_calls = false # optional: force this model serial
//! # max_parallel_tool_calls = 1 # optional known hard limit
//!
//! [[mcp_servers]]             # optional; external MCP tool servers
//! name = "fs"
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "/path"]
//! # env values are *names* of environment variables to forward (never secrets):
//! # env = { GITHUB_TOKEN = "GITHUB_TOKEN" }
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use toml_edit::{DocumentMut, value};

use leveler_model::{
    CompatibilityConfig, ModelCapabilities, ModelLimits, ModelProfile, ProtocolKind,
    ReasoningConfig, ReasoningEffort, ReasoningStyle,
};
use leveler_provider::{ModelConfigFile, ProviderConfig, RetryConfig, Timeouts};
use leveler_tools::mcp::McpServerConfig;

/// The parsed global config.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    /// Default model reference (`provider/model`) when none is given.
    #[serde(default)]
    pub default_model: Option<String>,
    /// TUI language: `zh` or `en`. Overridden by `LEVELER_LANG` when set.
    #[serde(default)]
    pub lang: Option<String>,
    /// UI preferences (theme, …).
    #[serde(default)]
    ui: GlobalUi,
    /// Git commit attribution. Enabled unless explicitly disabled.
    #[serde(default)]
    vcs: GlobalVcs,
    #[serde(default)]
    providers: BTreeMap<String, GlobalProvider>,
    #[serde(default)]
    models: BTreeMap<String, GlobalModel>,
    /// External MCP (Model Context Protocol) servers whose tools are exposed to
    /// the model (spawned over stdio).
    #[serde(default)]
    mcp_servers: Vec<GlobalMcpServer>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalUi {
    /// TUI theme id: `ion` | `night` | `day` (aliases: dark, light).
    #[serde(default)]
    theme: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalVcs {
    #[serde(default = "default_true")]
    co_author: bool,
}

impl Default for GlobalVcs {
    fn default() -> Self {
        Self { co_author: true }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalMcpServer {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalProvider {
    #[serde(default = "default_protocol")]
    protocol: String,
    base_url: String,
    /// Name of an environment variable holding the API key.
    #[serde(default)]
    api_key_env: Option<String>,
    /// Optional plaintext API key (local convenience). Preferred over
    /// `api_key_env` when non-empty.
    #[serde(default)]
    api_key: Option<String>,
    /// Extra HTTP headers sent with every request to *this* provider only
    /// (e.g. a `user-agent` an endpoint gates on). Empty → nothing extra.
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalModel {
    provider: String,
    /// Provider-side model id; defaults to the table key.
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    /// Factual model capabilities. Defaults preserve the pre-existing compact
    /// global-config behavior; set them explicitly when the endpoint differs.
    #[serde(default = "default_true")]
    streaming: bool,
    #[serde(default = "default_true")]
    tool_calling: bool,
    /// Default true means "leave the provider's native behavior alone": the
    /// OpenAI-compatible adapter then omits the wire flag. Set false only for a
    /// model that must be forced to emit tool calls serially.
    #[serde(default = "default_true")]
    parallel_tool_calls: bool,
    #[serde(default = "default_true")]
    structured_output: bool,
    #[serde(default)]
    vision: bool,
    #[serde(default)]
    reasoning: bool,
    /// How to ask this model to reason: `none` (default), `openai_effort`, or
    /// `thinking_flag` (DeepSeek/GLM). Unset means we send no reasoning field.
    #[serde(default)]
    reasoning_style: ReasoningStyle,
    /// `minimal` | `low` | `medium` | `high` | `max`. Omitted → provider default.
    #[serde(default)]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    context_window: Option<u32>,
    /// Usable context before compaction. Defaults to half the hard window.
    #[serde(default)]
    reliable_context: Option<u32>,
    /// Optional gateway pricing (USD per million tokens) for eval cost
    /// accounting: `pricing: { input_usd_per_mtok: 0.27, output_usd_per_mtok: 1.10 }`.
    /// Omit and no cost is ever computed.
    #[serde(default)]
    pricing: Option<leveler_model::ModelPricing>,
    /// Max output tokens per request. Large tool-call payloads (e.g. apply_patch)
    /// truncate mid-JSON if this is too small. Defaults to 8192.
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    max_tool_schema_bytes: Option<usize>,
    /// Known provider/model limit. Zero means unspecified when parallel tool
    /// calls are supported; non-parallel models default to one.
    #[serde(default)]
    max_parallel_tool_calls: Option<usize>,
    /// Whether the provider accepts a caller-chosen `temperature`. Kimi For
    /// Coding rejects every value but its own default (HTTP 400), so set this
    /// false there. Defaults to true.
    #[serde(default = "default_true")]
    supports_temperature: bool,
}

fn default_true() -> bool {
    true
}

fn default_protocol() -> String {
    "openai_chat".to_string()
}

/// The bundle a [`GlobalConfig`] expands into.
pub struct GlobalBundle {
    pub providers: Vec<ProviderConfig>,
    pub models: Vec<ModelConfigFile>,
    pub default_model: Option<String>,
    pub lang: Option<String>,
    /// TUI theme id from `[ui].theme`, if set.
    pub theme: Option<String>,
    pub vcs_co_author: bool,
    pub mcp_servers: Vec<McpServerConfig>,
}

/// A typed error from loading the global config, so callers and tests can tell
/// MCP secret rejections apart from ordinary parse failures.
#[derive(Debug, thiserror::Error)]
pub enum GlobalConfigError {
    #[error("{0}")]
    Parse(String),
    #[error(
        "global config MCP server `{server}` env `{key}` stores a plaintext secret; replace the value with an environment variable name reference (e.g. `{key} = \"{key}\"`)"
    )]
    McpSecretInConfig { server: String, key: String },
    #[error(
        "global config MCP server `{server}` env `{key}` must be a string UPPER_SNAKE environment variable name reference (e.g. `{key} = \"{key}\"`)"
    )]
    McpEnvNotString { server: String, key: String },
}
impl GlobalConfig {
    /// The config path: `<leveler-home>/config.toml`, or `None` when no home is
    /// known. Home resolution (incl. the Windows `USERPROFILE` fallback) is
    /// shared via [`leveler_core::leveler_home_dir_from`].
    pub fn path() -> Option<PathBuf> {
        leveler_core::leveler_home_dir_from(|k| std::env::var_os(k))
            .map(|home| home.join("config.toml"))
    }

    /// Load the global config, or an empty one if the file is absent. A present
    /// but malformed file is a hard error (never silently ignored).
    pub fn load() -> Result<Self, GlobalConfigError> {
        let Some(path) = Self::path() else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_toml_str(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(GlobalConfigError::Parse(format!("{}: {e}", path.display()))),
        }
    }

    /// Parse global config from TOML text. Provider `api_key` may be set as
    /// plaintext (local convenience) and/or via `api_key_env`. MCP `env`
    /// values must still be env-name references (never raw secrets).
    fn from_toml_str(text: &str) -> Result<Self, GlobalConfigError> {
        let value: toml::Value =
            toml::from_str(text).map_err(|e| GlobalConfigError::Parse(e.to_string()))?;
        reject_mcp_env_secrets(&value)?;
        toml::from_str(text).map_err(|e| GlobalConfigError::Parse(e.to_string()))
    }

    /// Persist `default_model` in the global config (format-preserving).
    ///
    /// Used when the user switches model in the TUI/CLI so the next launch
    /// picks the same model without re-selecting.
    pub fn save_default_model(model: &str) -> Result<(), GlobalConfigError> {
        let path = Self::path().ok_or_else(|| {
            GlobalConfigError::Parse(
                "no config path (set HOME, USERPROFILE, or LEVELER_HOME)".to_string(),
            )
        })?;
        save_default_model_at(&path, model)
    }
}

/// Write `default_model = "…"` to a specific config path (tests + production).
/// Render the starter config `leveler init` writes. Lives next to the parser
/// so the generated document can never drift from the accepted schema (the
/// round-trip is unit-tested in this module).
pub fn render_init_config(
    provider_id: &str,
    base_url: &str,
    api_key_env: &str,
    model_id: &str,
    context_window: u64,
) -> String {
    // Built with toml_edit so user-supplied values are always escaped
    // correctly; the round-trip through the strict parser is unit-tested.
    // Explicit `[providers.x]` tables (not inline) — humans edit this file.
    let mut doc = DocumentMut::new();
    doc["default_model"] = value(format!("{provider_id}/{model_id}"));
    let mut provider = toml_edit::Table::new();
    provider["base_url"] = value(base_url);
    provider["api_key_env"] = value(api_key_env);
    let mut providers = toml_edit::Table::new();
    providers.set_implicit(true);
    providers[provider_id] = toml_edit::Item::Table(provider);
    doc["providers"] = toml_edit::Item::Table(providers);
    let mut model = toml_edit::Table::new();
    model["provider"] = value(provider_id);
    model["context_window"] = value(context_window as i64);
    let mut models = toml_edit::Table::new();
    models.set_implicit(true);
    models[model_id] = toml_edit::Item::Table(model);
    doc["models"] = toml_edit::Item::Table(models);
    format!(
        "# CodeLeveler global config — created by `leveler init`.\n\
         # Reference: https://github.com/dengmengmian/CodeLeveler#configuration\n\
         {doc}"
    )
}

pub fn save_default_model_at(path: &Path, model: &str) -> Result<(), GlobalConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| GlobalConfigError::Parse(e.to_string()))?;
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc = if text.trim().is_empty() {
        DocumentMut::new()
    } else {
        text.parse::<DocumentMut>()
            .map_err(|e| GlobalConfigError::Parse(format!("config is not valid TOML: {e}")))?
    };
    doc["default_model"] = value(model);
    std::fs::write(path, doc.to_string()).map_err(|e| GlobalConfigError::Parse(e.to_string()))?;
    Ok(())
}

impl GlobalConfig {
    /// Expand into provider/model/policy configs with sensible defaults filled.
    pub fn into_bundle(self) -> GlobalBundle {
        let providers = self
            .providers
            .into_iter()
            .map(|(id, p)| ProviderConfig {
                id,
                protocol: parse_protocol(&p.protocol),
                base_url: p.base_url,
                api_key_env: p.api_key_env.unwrap_or_default(),
                api_key: p
                    .api_key
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                headers: p.headers,
                timeouts: Timeouts::default(),
                retry: RetryConfig::default(),
            })
            .collect();

        let models = self
            .models
            .into_iter()
            .map(|(id, m)| {
                let context = m.context_window.unwrap_or(131_072);
                ModelConfigFile {
                    profile: ModelProfile {
                        model_id: m.model_id.unwrap_or_else(|| id.clone()),
                        id,
                        provider: m.provider,
                        protocol: m
                            .protocol
                            .as_deref()
                            .map(parse_protocol)
                            .unwrap_or(ProtocolKind::OpenAiChat),
                        capabilities: ModelCapabilities {
                            streaming: m.streaming,
                            tool_calling: m.tool_calling,
                            parallel_tool_calls: m.parallel_tool_calls,
                            structured_output: m.structured_output,
                            reasoning: m.reasoning,
                            vision: m.vision,
                        },
                        limits: ModelLimits {
                            context_window: context,
                            reliable_context: m.reliable_context.unwrap_or(context / 2),
                            max_output_tokens: m.max_output_tokens.unwrap_or(8192),
                            max_tool_schema_bytes: m.max_tool_schema_bytes.unwrap_or(32768),
                            max_parallel_tool_calls: m
                                .max_parallel_tool_calls
                                .unwrap_or(usize::from(!m.parallel_tool_calls)),
                        },
                        reasoning: ReasoningConfig {
                            style: m.reasoning_style,
                            effort: m.reasoning_effort,
                        },
                        compatibility: CompatibilityConfig {
                            supports_temperature: m.supports_temperature,
                            ..CompatibilityConfig::default()
                        },
                        // Global models use the default prompt; a per-model prompt
                        // is a repo-config concern (configs/models/*.yaml).
                        instructions: None,
                        pricing: m.pricing,
                    },
                    policy: None,
                }
            })
            .collect::<Vec<_>>();

        let mcp_servers = self
            .mcp_servers
            .into_iter()
            .map(|s| {
                let env = resolve_mcp_env_refs(&s.name, s.env);
                McpServerConfig {
                    name: s.name,
                    command: s.command,
                    args: s.args,
                    // Config stores env-name *references*; resolve to real values
                    // from the process environment (secrets never live in the file).
                    env,
                }
            })
            .collect();

        GlobalBundle {
            providers,
            models,
            default_model: self.default_model,
            lang: self.lang,
            theme: self.ui.theme,
            vcs_co_author: self.vcs.co_author,
            mcp_servers,
        }
    }
}

/// Reject MCP `env` map values that are not UPPER_SNAKE name references.
/// Never echoes secret values.
fn reject_mcp_env_secrets(value: &toml::Value) -> Result<(), GlobalConfigError> {
    let Some(servers) = value.get("mcp_servers").and_then(|v| v.as_array()) else {
        return Ok(());
    };
    for server in servers {
        let name = server
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unnamed>");
        let Some(env) = server.get("env").and_then(|v| v.as_table()) else {
            continue;
        };
        for (key, val) in env {
            let Some(raw) = val.as_str() else {
                return Err(GlobalConfigError::McpEnvNotString {
                    server: name.to_string(),
                    key: key.clone(),
                });
            };
            // Same rules as `mcp add --env`: UPPER_SNAKE refs only; no tokens,
            // no lowercase literals like `password`.
            if crate::mcp_config::looks_like_secret_token(raw)
                || !crate::mcp_config::is_env_name_ref(raw)
            {
                return Err(GlobalConfigError::McpSecretInConfig {
                    server: name.to_string(),
                    key: key.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Resolve MCP env map (`dest → source_env_name`) from the process environment.
/// Unset sources are omitted (doctor reports the gap; a warn log names key only).
fn resolve_mcp_env_refs(server: &str, env: BTreeMap<String, String>) -> Vec<(String, String)> {
    resolve_mcp_env_refs_with(server, env, |source| std::env::var(source).ok())
}

/// Testable resolver: looks up each source name via `lookup`.
fn resolve_mcp_env_refs_with(
    server: &str,
    env: BTreeMap<String, String>,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Vec<(String, String)> {
    env.into_iter()
        .filter_map(|(dest, source)| {
            if !crate::mcp_config::is_env_name_ref(&source) {
                tracing::warn!(
                    server,
                    key = %dest,
                    "MCP env value is not a valid UPPER_SNAKE name reference; skipped"
                );
                return None;
            }
            match lookup(&source) {
                Some(v) => Some((dest, v)),
                None => {
                    tracing::warn!(
                        server,
                        key = %dest,
                        source = %source,
                        "MCP env source is unset; variable will not be forwarded"
                    );
                    None
                }
            }
        })
        .collect()
}

fn parse_protocol(s: &str) -> ProtocolKind {
    match s {
        "openai_responses" => ProtocolKind::OpenAiResponses,
        "anthropic_messages" => ProtocolKind::AnthropicMessages,
        "gemini_generate_content" => ProtocolKind::GeminiGenerateContent,
        _ => ProtocolKind::OpenAiChat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_rendering_round_trips_through_the_parser() {
        let text = render_init_config(
            "deepseek",
            "https://api.deepseek.com",
            "DEEPSEEK_API_KEY",
            "deepseek-chat",
            131_072,
        );
        let parsed = GlobalConfig::from_toml_str(&text)
            .expect("init output must parse with the strict (deny_unknown_fields) parser");
        assert_eq!(
            parsed.default_model.as_deref(),
            Some("deepseek/deepseek-chat")
        );
        let bundle = parsed.into_bundle();
        assert_eq!(bundle.providers.len(), 1, "one provider: {text}");
        assert_eq!(bundle.providers[0].id, "deepseek");
        assert_eq!(bundle.providers[0].base_url, "https://api.deepseek.com");
        assert_eq!(bundle.providers[0].api_key_env, "DEEPSEEK_API_KEY");
        assert_eq!(bundle.models.len(), 1, "one model: {text}");
        assert_eq!(bundle.models[0].profile.id, "deepseek-chat");
        assert_eq!(bundle.models[0].profile.limits.context_window, 131_072);
    }

    #[test]
    fn init_rendering_escapes_awkward_values() {
        // Values are user input; a quote or backslash must not corrupt the TOML.
        let text = render_init_config(
            "my-provider",
            "https://例子.example/v1?a=\"b\"",
            "MY_KEY",
            "model.v1",
            8192,
        );
        GlobalConfig::from_toml_str(&text).expect("escaped values must still parse");
    }

    #[test]
    fn expands_a_compact_toml_into_a_full_bundle() {
        let toml = r#"
            default_model = "deepseek/deepseek-v4-pro"

            [providers.deepseek]
            base_url = "https://api.deepseek.com"
            api_key_env = "DEEPSEEK_API_KEY"

            [models."deepseek-v4-pro"]
            provider = "deepseek"
            context_window = 131072
            vision = true
        "#;
        let cfg: GlobalConfig = toml::from_str(toml).unwrap();
        let bundle = cfg.into_bundle();

        assert_eq!(
            bundle.default_model.as_deref(),
            Some("deepseek/deepseek-v4-pro")
        );
        assert_eq!(bundle.providers.len(), 1);
        assert_eq!(bundle.providers[0].id, "deepseek");
        assert_eq!(bundle.providers[0].protocol, ProtocolKind::OpenAiChat);

        assert_eq!(bundle.models.len(), 1);
        let m = &bundle.models[0];
        assert_eq!(m.profile.id, "deepseek-v4-pro");
        assert_eq!(m.profile.model_id, "deepseek-v4-pro");
        assert_eq!(m.profile.provider, "deepseek");
        assert!(m.profile.capabilities.vision);
        assert!(m.profile.capabilities.streaming);
        assert!(m.profile.capabilities.tool_calling);
        assert!(m.profile.capabilities.parallel_tool_calls);
        assert!(m.profile.capabilities.structured_output);
        assert_eq!(m.profile.limits.context_window, 131072);
        assert_eq!(m.profile.limits.reliable_context, 65536);
        assert_eq!(m.profile.limits.max_tool_schema_bytes, 32768);
        assert_eq!(m.profile.limits.max_parallel_tool_calls, 0);
        // Not set in the TOML above → defaults to 8192.
        assert_eq!(m.profile.limits.max_output_tokens, 8192);
        // Tier bindings are retired: global models never carry a policy ref.
        assert_eq!(m.policy, None);
    }

    #[test]
    fn reasoning_style_and_effort_reach_the_profile() {
        let toml = r#"
            [providers.deepseek]
            base_url = "https://api.deepseek.com"

            [models."deepseek-v4-pro"]
            provider = "deepseek"
            reasoning = true
            reasoning_style = "thinking_flag"
            reasoning_effort = "high"
        "#;
        let cfg: GlobalConfig = toml::from_str(toml).unwrap();
        let bundle = cfg.into_bundle();
        let r = bundle.models[0].profile.reasoning;
        assert_eq!(r.style, ReasoningStyle::ThinkingFlag);
        assert_eq!(r.effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn reasoning_defaults_to_sending_nothing() {
        let toml = r#"
            [providers.deepseek]
            base_url = "https://api.deepseek.com"

            [models."deepseek-chat"]
            provider = "deepseek"
        "#;
        let cfg: GlobalConfig = toml::from_str(toml).unwrap();
        let bundle = cfg.into_bundle();
        let r = bundle.models[0].profile.reasoning;
        assert_eq!(r.style, ReasoningStyle::None);
        assert_eq!(r.effort, None);
    }

    #[test]
    fn global_model_accepts_complete_capability_and_limit_facts() {
        let toml = r#"
            [providers.openai]
            base_url = "https://api.openai.com"

            [models."capable"]
            provider = "openai"
            streaming = false
            tool_calling = false
            parallel_tool_calls = true
            structured_output = false
            context_window = 200000
            reliable_context = 180000
            max_output_tokens = 32000
            max_tool_schema_bytes = 65536
            max_parallel_tool_calls = 8
        "#;
        let bundle = toml::from_str::<GlobalConfig>(toml).unwrap().into_bundle();
        let profile = &bundle.models[0].profile;
        assert!(!profile.capabilities.streaming);
        assert!(!profile.capabilities.tool_calling);
        assert!(profile.capabilities.parallel_tool_calls);
        assert!(!profile.capabilities.structured_output);
        assert_eq!(profile.limits.reliable_context, 180000);
        assert_eq!(profile.limits.max_tool_schema_bytes, 65536);
        assert_eq!(profile.limits.max_parallel_tool_calls, 8);
    }

    #[test]
    fn mcp_servers_parse_env_as_name_references() {
        let toml = r#"
            [[mcp_servers]]
            name = "fs"
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
            env = { FOO = "LEVELER_TEST_MCP_FOO", MISSING = "LEVELER_TEST_MCP_MISSING_XYZ" }
        "#;
        let cfg = GlobalConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.mcp_servers.len(), 1);
        let s = &cfg.mcp_servers[0];
        assert_eq!(s.name, "fs");
        assert_eq!(s.command, "npx");
        assert_eq!(s.args.len(), 3);
        assert_eq!(
            s.env.get("FOO").map(String::as_str),
            Some("LEVELER_TEST_MCP_FOO")
        );
        assert_eq!(
            s.env.get("MISSING").map(String::as_str),
            Some("LEVELER_TEST_MCP_MISSING_XYZ")
        );
        // into_bundle resolves from process env; without those vars set, env is empty.
        // Pure resolve logic is covered by `resolve_mcp_env_refs_with_lookup`.
        let bundle = GlobalConfig::from_toml_str(toml).unwrap().into_bundle();
        assert_eq!(bundle.mcp_servers[0].name, "fs");
    }

    #[test]
    fn resolve_mcp_env_refs_with_lookup() {
        let mut refs = BTreeMap::new();
        refs.insert("FOO".into(), "SRC_FOO".into());
        refs.insert("BAR".into(), "SRC_BAR".into());
        let resolved = resolve_mcp_env_refs_with("svc", refs, |source| match source {
            "SRC_FOO" => Some("resolved-value".into()),
            _ => None,
        });
        assert_eq!(
            resolved,
            vec![("FOO".to_string(), "resolved-value".to_string())]
        );
    }

    #[test]
    fn mcp_env_plaintext_secret_is_rejected_without_echoing() {
        let secret = "sk-supersecret-mcp-value";
        let toml = format!(
            r#"
            [[mcp_servers]]
            name = "gh"
            command = "gh-mcp"
            env = {{ GITHUB_TOKEN = "{secret}" }}
        "#
        );
        let err = GlobalConfig::from_toml_str(&toml).unwrap_err();
        assert!(
            matches!(
                &err,
                GlobalConfigError::McpSecretInConfig { server, key }
                    if server == "gh" && key == "GITHUB_TOKEN"
            ),
            "expected McpSecretInConfig, got {err:?}"
        );
        assert!(
            !err.to_string().contains(secret),
            "must never echo the secret value"
        );
    }

    #[test]
    fn mcp_env_identifier_shaped_plaintext_is_rejected_without_echoing() {
        // Avoid substrings of the error template itself (e.g. the word "secret").
        for literal in ["password", "hunter2", "s3cr3tvalue"] {
            let toml = format!(
                r#"
                [[mcp_servers]]
                name = "svc"
                command = "echo"
                env = {{ API_KEY = "{literal}" }}
            "#
            );
            let err = GlobalConfig::from_toml_str(&toml).unwrap_err();
            assert!(
                matches!(
                    &err,
                    GlobalConfigError::McpSecretInConfig { server, key }
                        if server == "svc" && key == "API_KEY"
                ),
                "literal {literal:?} must be rejected, got {err:?}"
            );
            assert!(
                !err.to_string().contains(literal),
                "must never echo the literal in: {err}"
            );
        }
    }

    #[test]
    fn mcp_env_non_string_value_is_rejected_as_not_string() {
        let toml = r#"
            [[mcp_servers]]
            name = "svc"
            command = "echo"
            env = { API_KEY = 123 }
        "#;
        let err = GlobalConfig::from_toml_str(toml).unwrap_err();
        assert!(
            matches!(
                &err,
                GlobalConfigError::McpEnvNotString { server, key }
                    if server == "svc" && key == "API_KEY"
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn max_output_tokens_is_read_from_config() {
        let toml = r#"
            [providers.deepseek]
            base_url = "https://api.deepseek.com"

            [models."m"]
            provider = "deepseek"
            max_output_tokens = 16384
        "#;
        let bundle = toml::from_str::<GlobalConfig>(toml).unwrap().into_bundle();
        assert_eq!(bundle.models[0].profile.limits.max_output_tokens, 16384);
    }

    #[test]
    fn empty_config_yields_empty_bundle() {
        let bundle = GlobalConfig::default().into_bundle();
        assert!(bundle.providers.is_empty());
        assert!(bundle.models.is_empty());
        assert!(bundle.default_model.is_none());
        assert!(bundle.lang.is_none());
    }

    #[test]
    fn lang_is_read_from_config() {
        let toml = r#"
            default_model = "deepseek/m"
            lang = "zh"
            [providers.deepseek]
            base_url = "https://api.deepseek.com"
            [models.m]
            provider = "deepseek"
        "#;
        let cfg: GlobalConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.lang.as_deref(), Some("zh"));
        assert_eq!(cfg.into_bundle().lang.as_deref(), Some("zh"));
    }

    #[test]
    fn ui_theme_is_read_from_config() {
        let toml = r#"
            [ui]
            theme = "night"
        "#;
        let cfg: GlobalConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.into_bundle().theme.as_deref(), Some("night"));
    }

    #[test]
    fn vcs_co_author_can_be_disabled() {
        let config = GlobalConfig::from_toml_str("[vcs]\nco_author = false\n")
            .expect("vcs.co_author must be a valid setting");
        assert!(!config.into_bundle().vcs_co_author);
        assert!(GlobalConfig::default().into_bundle().vcs_co_author);
    }

    #[test]
    fn unknown_field_is_rejected() {
        // deny_unknown_fields catches typos rather than silently ignoring them.
        assert!(toml::from_str::<GlobalConfig>("bogus_key = 1").is_err());
    }

    #[test]
    fn provider_headers_reach_the_provider_config() {
        // Some endpoints gate on User-Agent; the global TOML must be able to set it.
        let toml = r#"
[providers.kimi]
base_url = "https://api.kimi.com/coding/v1"
headers = { user-agent = "custom-client/0.1.0" }
"#;
        let bundle = toml::from_str::<GlobalConfig>(toml).unwrap().into_bundle();
        let provider = &bundle.providers[0];
        assert_eq!(
            provider.headers.get("user-agent").map(String::as_str),
            Some("custom-client/0.1.0")
        );
    }

    #[test]
    fn save_default_model_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "default_model = \"old/model\"\n\n[providers.p]\nbase_url = \"http://x\"\n",
        )
        .unwrap();
        save_default_model_at(&path, "deepseek/deepseek-v4-pro").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("default_model = \"deepseek/deepseek-v4-pro\""),
            "{text}"
        );
        assert!(text.contains("[providers.p]"), "{text}");
        assert!(text.contains("base_url"), "{text}");
        // Reloadable as typed config.
        let cfg = GlobalConfig::from_toml_str(&text).unwrap();
        assert_eq!(
            cfg.default_model.as_deref(),
            Some("deepseek/deepseek-v4-pro")
        );
    }

    #[test]
    fn plaintext_provider_api_key_is_accepted() {
        let toml = "[providers.kimi]\nbase_url = \"https://e\"\napi_key = \"sk-supersecret\"\napi_key_env = \"KIMI_API_KEY\"\n";
        let bundle = GlobalConfig::from_toml_str(toml).unwrap().into_bundle();
        let p = &bundle.providers[0];
        assert_eq!(p.id, "kimi");
        assert_eq!(p.api_key.as_deref(), Some("sk-supersecret"));
        assert_eq!(p.api_key_env, "KIMI_API_KEY");
    }

    #[test]
    fn supports_temperature_defaults_true_and_can_be_disabled() {
        let toml = r#"
[providers.kimi]
base_url = "https://api.kimi.com/coding/v1"

[models."kimi-for-coding"]
provider = "kimi"
supports_temperature = false

[models."normal"]
provider = "kimi"
"#;
        let bundle = toml::from_str::<GlobalConfig>(toml).unwrap().into_bundle();
        let find = |id: &str| {
            bundle
                .models
                .iter()
                .find(|m| m.profile.id == id)
                .unwrap()
                .profile
                .compatibility
                .supports_temperature
        };
        assert!(!find("kimi-for-coding"), "explicit false must be honored");
        assert!(find("normal"), "omitted must default to true");
    }
}
