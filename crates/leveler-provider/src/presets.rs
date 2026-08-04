//! Built-in provider presets.
//!
//! These exist so a first run does not ask a newcomer for a base URL, a
//! protocol, and a context window — three questions nobody can answer before
//! they have used the tool once.
//!
//! **What a preset promises.** Only the parts that are stable and verifiable:
//! the API endpoint, the wire protocol, and the conventional key variable.
//! These change rarely and break loudly when wrong.
//!
//! **What it does not promise.** `suggested_model` and `suggested_context` are
//! *starting points*, not facts — model names and context windows move faster
//! than a release cycle, and which model an account can actually reach depends
//! on the plan behind the key. Setup offers them as an editable default and
//! points at `leveler models list` to confirm. Never present them as verified.

use leveler_model::ProtocolKind;

/// A known provider, pre-filled so setup can ask two questions instead of six.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPreset {
    /// Config key and `provider/model` prefix.
    pub id: &'static str,
    /// Human name for the picker.
    pub label: &'static str,
    pub base_url: &'static str,
    pub protocol: ProtocolKind,
    /// Conventional environment variable, kept as a fallback alongside a
    /// stored key so existing shell setups keep working.
    pub key_env: &'static str,
    /// Where to get a key. Shown when the user has none.
    pub console_url: &'static str,
    /// Editable starting point — see the module docs. Not a verified claim.
    pub suggested_model: &'static str,
    /// Conservative default; the real window is model-specific.
    pub suggested_context: u64,
}

/// Presets offered during setup, in the order they are listed.
pub const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "deepseek",
        label: "DeepSeek",
        base_url: "https://api.deepseek.com",
        protocol: ProtocolKind::OpenAiChat,
        key_env: "DEEPSEEK_API_KEY",
        console_url: "https://platform.deepseek.com/api_keys",
        suggested_model: "deepseek-chat",
        suggested_context: 131_072,
    },
    ProviderPreset {
        id: "moonshot",
        label: "Moonshot / Kimi",
        base_url: "https://api.moonshot.cn/v1",
        protocol: ProtocolKind::OpenAiChat,
        key_env: "MOONSHOT_API_KEY",
        console_url: "https://platform.moonshot.cn/console/api-keys",
        suggested_model: "kimi-k2-0905-preview",
        suggested_context: 131_072,
    },
    ProviderPreset {
        id: "openai",
        label: "OpenAI",
        base_url: "https://api.openai.com/v1",
        protocol: ProtocolKind::OpenAiChat,
        key_env: "OPENAI_API_KEY",
        console_url: "https://platform.openai.com/api-keys",
        suggested_model: "gpt-4o",
        suggested_context: 128_000,
    },
    ProviderPreset {
        id: "anthropic",
        label: "Anthropic",
        base_url: "https://api.anthropic.com",
        protocol: ProtocolKind::AnthropicMessages,
        key_env: "ANTHROPIC_API_KEY",
        console_url: "https://console.anthropic.com/settings/keys",
        suggested_model: "claude-sonnet-4-5",
        suggested_context: 200_000,
    },
];

/// Look up a preset by id.
pub fn preset(id: &str) -> Option<&'static ProviderPreset> {
    PRESETS.iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate preset id");
    }

    /// A typo in an endpoint sends someone's API key to the wrong host, so the
    /// shape is checked rather than assumed.
    #[test]
    fn every_endpoint_is_https_and_has_no_trailing_slash() {
        for p in PRESETS {
            assert!(
                p.base_url.starts_with("https://"),
                "{}: keys must never travel over plaintext http",
                p.id
            );
            assert!(
                !p.base_url.ends_with('/'),
                "{}: trailing slash produces `//` when joined",
                p.id
            );
            assert!(
                p.console_url.starts_with("https://"),
                "{}: console link",
                p.id
            );
        }
    }

    #[test]
    fn key_variables_follow_the_conventional_shape() {
        for p in PRESETS {
            assert!(p.key_env.ends_with("_API_KEY"), "{}: {}", p.id, p.key_env);
            assert!(
                p.key_env
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()),
                "{}: {}",
                p.id,
                p.key_env
            );
        }
    }

    #[test]
    fn presets_are_complete_enough_to_write_a_config() {
        for p in PRESETS {
            assert!(!p.label.is_empty(), "{}", p.id);
            assert!(!p.suggested_model.is_empty(), "{}", p.id);
            assert!(p.suggested_context > 0, "{}", p.id);
        }
    }

    #[test]
    fn lookup_finds_presets_by_id() {
        assert_eq!(preset("deepseek").map(|p| p.label), Some("DeepSeek"));
        assert!(preset("nope").is_none());
    }
}
