//! Provider configuration and environment-variable expansion.
//!
//! This is the *only* place allowed to read process environment for provider
//! wiring (spec §53.16-17): business code never touches `std::env`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use leveler_model::ProtocolKind;

/// A provider endpoint definition (spec §15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub protocol: ProtocolKind,
    pub base_url: String,
    /// Name of the environment variable holding the API key.
    /// Used when [`Self::api_key`] is unset/empty.
    #[serde(default)]
    pub api_key_env: String,
    /// Optional plaintext API key (e.g. local `~/.leveler/config.toml`).
    /// Preferred over [`Self::api_key_env`] when non-empty. Never serialized
    /// back out (so dumps / round-trips do not rewrite secrets).
    #[serde(default, skip_serializing)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub timeouts: Timeouts,
    #[serde(default)]
    pub retry: RetryConfig,
}

/// Connection / streaming timeouts in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timeouts {
    pub connect_seconds: u64,
    pub request_seconds: u64,
    pub idle_stream_seconds: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect_seconds: 20,
            request_seconds: 120,
            idle_stream_seconds: 60,
        }
    }
}

#[cfg(test)]
mod default_timeout_tests {
    use super::*;

    #[test]
    fn defaults_bound_silent_waits_and_retry_amplification() {
        let timeouts = Timeouts::default();
        assert_eq!(timeouts.request_seconds, 120);
        assert_eq!(timeouts.idle_stream_seconds, 60);
        assert_eq!(RetryConfig::default().max_attempts, 2);
    }
}

/// Retry/backoff configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 2,
            initial_backoff_ms: 500,
            max_backoff_ms: 10_000,
        }
    }
}

/// Errors from loading provider/model/policy config.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: String,
        source: serde_yaml::Error,
    },
    #[error("environment variable `{0}` is required but not set")]
    MissingEnv(String),
    #[error(
        "config file {path} uses the retired key `{key}`: {reason} (model policy tiers are retired; execution is resolved from model facts + role + task)"
    )]
    RetiredKey {
        path: String,
        key: String,
        reason: String,
    },
}

/// Expand `${VAR}` and `${VAR:-default}` references in a string using a lookup
/// function (injected so it is testable and env access stays isolated).
///
/// A reference with no default whose variable is unset is an error — silently
/// expanding to an empty string turns a config typo into e.g. an empty
/// `base_url` that only fails much later, far from the cause.
pub fn expand_env_with<F>(input: &str, lookup: F) -> Result<String, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'{'
            && let Some(end) = input[i + 2..].find('}')
        {
            let expr = &input[i + 2..i + 2 + end];
            let (name, default) = match expr.split_once(":-") {
                Some((n, d)) => (n, Some(d)),
                None => (expr, None),
            };
            let value = lookup(name)
                .or_else(|| default.map(str::to_owned))
                .ok_or_else(|| ConfigError::MissingEnv(name.to_string()))?;
            out.push_str(&value);
            i = i + 2 + end + 1;
            continue;
        }
        // Copy this UTF-8 char whole.
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&input[i..i + ch_len]);
        i += ch_len;
    }
    Ok(out)
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// Expand env references using the real process environment.
pub fn expand_env(input: &str) -> Result<String, ConfigError> {
    expand_env_with(input, |k| std::env::var(k).ok())
}

/// Resolve a provider's API key.
///
/// Priority: non-empty plaintext [`ProviderConfig::api_key`], then the
/// environment variable named by [`ProviderConfig::api_key_env`].
///
/// Returns `Ok(None)` when neither is configured (local / keyless models),
/// `Ok(Some(key))` when a key is available, and [`ConfigError::MissingEnv`]
/// when only an env var name is declared but that variable is unset/empty.
pub fn resolve_api_key(config: &ProviderConfig) -> Result<Option<String>, ConfigError> {
    if let Some(key) = config
        .api_key
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return Ok(Some(key.to_string()));
    }
    if config.api_key_env.trim().is_empty() {
        return Ok(None);
    }
    match std::env::var(&config.api_key_env) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        _ => Err(ConfigError::MissingEnv(config.api_key_env.clone())),
    }
}

/// Load a provider config from YAML, expanding env references in `base_url` and
/// header values.
pub fn load_provider_config(path: &Path) -> Result<ProviderConfig, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let expanded = expand_env(&raw)?;
    // Env references (including in header values) are already expanded above.
    parse_provider_config(&expanded, &path.display().to_string())
}

/// Parse a provider config from already-env-expanded YAML text.
///
/// Accepts optional plaintext `api_key` (preferred by [`resolve_api_key`])
/// and/or `api_key_env`. Prefer env for shared/repo configs; plaintext is for
/// local convenience (e.g. a personal machine config).
pub fn parse_provider_config(raw: &str, source: &str) -> Result<ProviderConfig, ConfigError> {
    serde_yaml::from_str(raw).map_err(|err| ConfigError::Parse {
        path: source.to_string(),
        source: err,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_simple_var() {
        let out = expand_env_with("x=${FOO}", |k| (k == "FOO").then(|| "bar".into())).unwrap();
        assert_eq!(out, "x=bar");
    }

    #[test]
    fn uses_default_when_missing() {
        let out = expand_env_with("${BASE:-https://api.deepseek.com}", |_| None).unwrap();
        assert_eq!(out, "https://api.deepseek.com");
    }

    #[test]
    fn prefers_value_over_default() {
        let out = expand_env_with("${BASE:-def}", |_| Some("real".into())).unwrap();
        assert_eq!(out, "real");
    }

    #[test]
    fn missing_without_default_is_an_error_not_empty() {
        let err = expand_env_with("[${X}]", |_| None).unwrap_err();
        assert!(
            matches!(err, ConfigError::MissingEnv(name) if name == "X"),
            "must name the missing variable"
        );
    }

    #[test]
    fn leaves_plain_text_untouched() {
        let out = expand_env_with("no vars here $ {notvar}", |_| Some("x".into())).unwrap();
        assert_eq!(out, "no vars here $ {notvar}");
    }

    #[test]
    fn plaintext_api_key_is_accepted_and_preferred_over_env() {
        let yaml = "id: x\nprotocol: openai_chat\nbase_url: https://e\napi_key: sk-inline\napi_key_env: SOME_ENV\n";
        let cfg = parse_provider_config(yaml, "provider.yaml").unwrap();
        assert_eq!(cfg.api_key.as_deref(), Some("sk-inline"));
        assert_eq!(cfg.api_key_env, "SOME_ENV");
        // Prefer inline even when the env var is unset/empty.
        let key = resolve_api_key(&cfg).unwrap();
        assert_eq!(key.as_deref(), Some("sk-inline"));
    }

    #[test]
    fn resolve_falls_back_to_env_when_inline_key_empty() {
        let mut cfg = ProviderConfig {
            id: "x".into(),
            protocol: ProtocolKind::OpenAiChat,
            base_url: "https://e".into(),
            api_key_env: String::new(),
            api_key: Some("   ".into()),
            headers: Default::default(),
            timeouts: Default::default(),
            retry: Default::default(),
        };
        // Whitespace-only inline is treated as absent; no env name → keyless.
        assert_eq!(resolve_api_key(&cfg).unwrap(), None);

        cfg.api_key = None;
        cfg.api_key_env = "DEFINITELY_UNSET_LEVELER_PROVIDER_TEST_KEY".into();
        let err = resolve_api_key(&cfg).unwrap_err();
        assert!(matches!(err, ConfigError::MissingEnv(name) if name == cfg.api_key_env));
    }

    #[test]
    fn parses_provider_yaml() {
        let yaml = r#"
id: deepseek
protocol: openai_chat
base_url: https://api.deepseek.com
api_key_env: DEEPSEEK_API_KEY
headers:
  user-agent: CodeLeveler/test
timeouts:
  connect_seconds: 5
  request_seconds: 30
  idle_stream_seconds: 10
retry:
  max_attempts: 2
  initial_backoff_ms: 100
  max_backoff_ms: 1000
"#;
        let cfg: ProviderConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.id, "deepseek");
        assert_eq!(cfg.protocol, ProtocolKind::OpenAiChat);
        assert_eq!(cfg.timeouts.connect_seconds, 5);
        assert_eq!(cfg.retry.max_attempts, 2);
    }
}
