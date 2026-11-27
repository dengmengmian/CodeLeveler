//! The Desktop's view and editor for `[web.search]` in the global config.
//!
//! Search is deliberately independent of every model provider: the section is
//! the only way to enable `web_search`, and an absent table means the tool is
//! unavailable. This module never returns the stored key, and a save that
//! omits the key keeps the existing one.

use std::path::Path;

use serde::Serialize;

use crate::global_config::GlobalConfig;

/// Errors from reading or editing `[web.search]`.
#[derive(Debug, thiserror::Error)]
pub enum SearchConfigError {
    #[error("cannot locate the config path (set HOME or LEVELER_HOME)")]
    NoPath,
    #[error("config is not valid TOML: {0}")]
    Parse(String),
    #[error("{0}")]
    Rejected(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("the configuration kept changing while it was being updated; nothing was written")]
    ConcurrentModification,
}

/// The display-safe search settings the Desktop renders.
#[derive(Debug, Clone, Serialize)]
pub struct SearchSettingsView {
    pub config_path: String,
    pub config_exists: bool,
    pub configured: bool,
    pub provider: Option<String>,
    pub api_key_env: Option<String>,
    /// A plaintext key exists in the file. The value is never returned.
    pub api_key_configured: bool,
    /// The effective credential variable resolves in this process right now.
    pub env_present: bool,
    /// `stored` | `env` | `none`.
    pub key_source: &'static str,
    pub base_url: Option<String>,
    pub max_results: Option<usize>,
    pub timeout_ms: Option<u64>,
    /// Per-provider defaults the form can offer without writing them.
    pub defaults: Vec<SearchProviderDefault>,
}

/// One selectable search backend.
#[derive(Debug, Clone, Serialize)]
pub struct SearchProviderDefault {
    pub id: &'static str,
    pub label: &'static str,
    pub key_env: &'static str,
    pub base_url: &'static str,
}

/// A validated save request. `None` keeps the stored value; an empty string
/// removes an optional override so the runtime default applies again.
#[derive(Debug, Clone, Default)]
pub struct SearchDraft {
    pub enabled: bool,
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub clear_api_key: bool,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub max_results: Option<usize>,
    pub timeout_ms: Option<u64>,
}

pub const DEEPSEEK_PROVIDER: &str = "deepseek";
pub const TAVILY_PROVIDER: &str = "tavily";
pub const DEEPSEEK_KEY_ENV: &str = "DEEPSEEK_API_KEY";
pub const TAVILY_KEY_ENV: &str = "LEVELER_SEARCH_API_KEY";
pub const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1/tools";
pub const TAVILY_BASE_URL: &str = "https://api.tavily.com";

/// Read the current section, or an empty view when it is absent.
pub fn view(path: &Path) -> Result<SearchSettingsView, SearchConfigError> {
    let text = read_text(path)?;
    let exists = path.is_file();
    let config = GlobalConfig::from_toml_str(&text)
        .map_err(|error| SearchConfigError::Parse(error.to_string()))?;
    let bundle = config.into_bundle();
    let search = bundle.web_search;
    let provider = search
        .as_ref()
        .map(|search| search.provider.as_str().to_string());
    let api_key_env = search.as_ref().map(|search| search.api_key_env.clone());
    let stored_key = search
        .as_ref()
        .is_some_and(|search| search.has_configured_api_key());
    let env_present = search.as_ref().is_some_and(|search| {
        std::env::var(&search.api_key_env)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    });
    let key_source = if stored_key {
        "stored"
    } else if env_present {
        "env"
    } else {
        "none"
    };
    Ok(SearchSettingsView {
        config_path: path.display().to_string(),
        config_exists: exists,
        configured: search.is_some(),
        provider,
        api_key_env,
        api_key_configured: stored_key,
        env_present,
        key_source,
        base_url: search.as_ref().and_then(|search| search.base_url.clone()),
        max_results: search.as_ref().map(|search| search.max_results),
        timeout_ms: search
            .as_ref()
            .map(|search| search.timeout.as_millis() as u64),
        defaults: vec![
            SearchProviderDefault {
                id: DEEPSEEK_PROVIDER,
                label: "DeepSeek 搜索",
                key_env: DEEPSEEK_KEY_ENV,
                base_url: DEEPSEEK_BASE_URL,
            },
            SearchProviderDefault {
                id: TAVILY_PROVIDER,
                label: "Tavily",
                key_env: TAVILY_KEY_ENV,
                base_url: TAVILY_BASE_URL,
            },
        ],
    })
}

/// Apply a save request under the shared configuration lock; the candidate is
/// validated by the same parser the runtime uses before it replaces the file.
pub fn save(path: &Path, draft: &SearchDraft) -> Result<(), SearchConfigError> {
    crate::config_edit::edit(
        path,
        |doc| apply(doc, draft),
        |candidate| {
            GlobalConfig::from_toml_str(candidate)
                .map(|_| ())
                .map_err(|error| SearchConfigError::Rejected(error.to_string()))
        },
    )
    .map_err(|error| match error {
        crate::config_edit::EditError::Apply(error)
        | crate::config_edit::EditError::Validate(error) => error,
        crate::config_edit::EditError::Parse(message) => SearchConfigError::Parse(message),
        crate::config_edit::EditError::Io(message) => SearchConfigError::Io(message),
        crate::config_edit::EditError::ConcurrentModification => {
            SearchConfigError::ConcurrentModification
        }
    })
}

fn apply(doc: &mut toml_edit::DocumentMut, draft: &SearchDraft) -> Result<(), SearchConfigError> {
    if !draft.enabled {
        let emptied = if let Some(web) = doc.get_mut("web").and_then(|item| item.as_table_mut()) {
            web.remove("search");
            web.is_empty()
        } else {
            false
        };
        if emptied {
            doc.remove("web");
        }
        return Ok(());
    }
    let provider = draft
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
        .ok_or_else(|| SearchConfigError::Rejected("请选择搜索服务".into()))?;
    if !matches!(provider, DEEPSEEK_PROVIDER | TAVILY_PROVIDER) {
        return Err(SearchConfigError::Rejected(format!(
            "不支持的搜索服务：{provider}"
        )));
    }
    if !doc.contains_key("web") {
        doc["web"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let web = doc["web"].as_table_mut().ok_or_else(|| {
        SearchConfigError::Rejected("`web` 不是 TOML 表，无法写入搜索设置".into())
    })?;
    if !web.contains_key("search") {
        web["search"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let search = web["search"].as_table_mut().ok_or_else(|| {
        SearchConfigError::Rejected("`web.search` 不是 TOML 表，无法写入搜索设置".into())
    })?;
    search["provider"] = toml_edit::value(provider);
    match draft.api_key_env.as_deref() {
        None => {}
        Some("") => {
            search.remove("api_key_env");
        }
        Some(env) => {
            let env = env.trim();
            if env.is_empty() || env.chars().any(|c| c.is_whitespace()) {
                return Err(SearchConfigError::Rejected(
                    "环境变量名不能为空或包含空白".into(),
                ));
            }
            search["api_key_env"] = toml_edit::value(env);
        }
    }
    if draft.clear_api_key {
        search.remove("api_key");
    } else if let Some(key) = draft.api_key.as_deref().map(str::trim) {
        if !key.is_empty() {
            if key.chars().count() > 16 * 1024 {
                return Err(SearchConfigError::Rejected("API Key 过长".into()));
            }
            search["api_key"] = toml_edit::value(key);
        }
    }
    match draft.base_url.as_deref() {
        None => {}
        Some("") => {
            search.remove("base_url");
        }
        Some(base_url) => {
            search["base_url"] = toml_edit::value(base_url.trim());
        }
    }
    // A missing numeric keeps the stored override; sending the resolved
    // default explicitly is how the form returns to the default behaviour.
    if let Some(max_results) = draft.max_results {
        search["max_results"] = toml_edit::value(max_results as i64);
    }
    if let Some(timeout_ms) = draft.timeout_ms {
        search["timeout_ms"] = toml_edit::value(timeout_ms as i64);
    }
    Ok(())
}

fn read_text(path: &Path) -> Result<String, SearchConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(SearchConfigError::Io(format!(
            "{}: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    fn draft(provider: &str) -> SearchDraft {
        SearchDraft {
            enabled: true,
            provider: Some(provider.into()),
            ..SearchDraft::default()
        }
    }

    #[test]
    fn an_absent_section_is_unconfigured_and_enabling_it_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        config(&path, "default_model = \"p/m\"\n");
        let before = view(&path).unwrap();
        assert!(!before.configured);
        assert_eq!(before.key_source, "none");
        save(&path, &draft("deepseek")).unwrap();
        let after = view(&path).unwrap();
        assert!(after.configured);
        assert_eq!(after.provider.as_deref(), Some("deepseek"));
        assert_eq!(after.api_key_env.as_deref(), Some(DEEPSEEK_KEY_ENV));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[web.search]"), "{text}");
    }

    #[test]
    fn saving_the_same_key_twice_never_loses_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        config(&path, "");
        let mut first = draft("tavily");
        first.api_key = Some("tvly-secret".into());
        save(&path, &first).unwrap();
        // The view masks the key, so the editor saves without it;
        // a key-less save must keep the stored value.
        let mut second = draft("tavily");
        second.max_results = Some(8);
        save(&path, &second).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("api_key = \"tvly-secret\""), "{text}");
        assert!(text.contains("max_results = 8"), "{text}");
        assert!(view(&path).unwrap().api_key_configured);
    }

    #[test]
    fn disabling_removes_only_the_search_section_and_preserves_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        config(
            &path,
            "default_model = \"p/m\"\n[web.search]\nprovider = \"deepseek\"\napi_key = \"k\"\nmax_results = 9\n[ui]\ntheme = \"dark\"\n",
        );
        save(
            &path,
            &SearchDraft {
                enabled: false,
                ..SearchDraft::default()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("[web.search]"), "{text}");
        assert!(text.contains("default_model"), "{text}");
        assert!(text.contains("[ui]"), "{text}");
        assert!(!view(&path).unwrap().configured);
    }

    #[test]
    fn unknown_providers_and_invalid_bounds_are_rejected_without_a_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        config(&path, "default_model = \"p/m\"\n");
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(matches!(
            save(&path, &draft("google")),
            Err(SearchConfigError::Rejected(_))
        ));
        let mut too_many = draft("deepseek");
        too_many.max_results = Some(51);
        assert!(matches!(
            save(&path, &too_many),
            Err(SearchConfigError::Rejected(_))
        ));
        let mut too_slow = draft("deepseek");
        too_slow.timeout_ms = Some(600_001);
        assert!(matches!(
            save(&path, &too_slow),
            Err(SearchConfigError::Rejected(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_key_without_an_explicit_env_uses_the_provider_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        config(&path, "");
        let mut request = draft("tavily");
        request.api_key = Some("tvly-secret".into());
        save(&path, &request).unwrap();
        assert_eq!(
            view(&path).unwrap().api_key_env.as_deref(),
            Some(TAVILY_KEY_ENV)
        );
    }
}
