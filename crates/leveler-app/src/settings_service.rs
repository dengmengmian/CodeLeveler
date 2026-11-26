//! The runtime's settings projection.
//!
//! One reader over the authorities that already exist: the resolved config
//! generation, the application layout, and the session row (the latter is
//! projected by the client, which already holds it). Nothing here writes
//! configuration; edits travel back through the existing typed commands.
//!
//! The snapshot is deliberately bounded and secret-free. Provider keys appear
//! as a boolean plus the environment variable name; base URLs are stripped of
//! credentials, query and fragment.

use leveler_client_protocol::settings::{
    UiConfigSource, UiProviderSetting, UiSettingSource, UiSettingsSnapshot,
};

use crate::Application;

/// Build one settings answer from the current generation.
pub(crate) fn snapshot(app: &Application) -> UiSettingsSnapshot {
    let generation = app.config_snapshot();
    let loaded = &generation.loaded;

    let providers = loaded
        .providers
        .iter()
        .map(|provider| {
            let api_key_configured = crate::provider_config::api_key_present(provider);
            let model_count = loaded
                .models
                .iter()
                .filter(|model| model.profile.provider == provider.id)
                .count();
            UiProviderSetting {
                id: provider.id.clone(),
                protocol: serde_json::to_value(provider.protocol)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_else(|| "unknown".to_string()),
                base_url: display_safe_url(&provider.base_url),
                api_key_env: (!provider.api_key_env.is_empty())
                    .then(|| provider.api_key_env.clone()),
                api_key_configured,
                model_count,
                source: if loaded.global_provider_ids.contains(&provider.id) {
                    UiSettingSource::GlobalConfig
                } else {
                    UiSettingSource::ProjectConfig
                },
            }
        })
        .collect();

    let mut sources = Vec::new();
    if let Some(path) = crate::global_config::GlobalConfig::path() {
        sources.push(UiConfigSource {
            present: path.is_file(),
            path: path.display().to_string(),
            kind: "global".into(),
            precedence: 10,
        });
    }
    sources.push(UiConfigSource {
        present: app.layout.config_dir.is_dir(),
        path: app.layout.config_dir.display().to_string(),
        kind: "project".into(),
        precedence: 20,
    });
    if let Some(repo) = app.layout.primary_workspace() {
        let project = repo.join(".leveler").join("config.yaml");
        sources.push(UiConfigSource {
            present: project.is_file(),
            path: project.display().to_string(),
            kind: "project".into(),
            precedence: 30,
        });
    }

    UiSettingsSnapshot {
        generated_at: leveler_core::now().to_rfc3339(),
        config_version: generation.version,
        config_path: crate::global_config::GlobalConfig::path()
            .map(|path| path.display().to_string()),
        project_config_dir: Some(app.layout.config_dir.display().to_string()),
        default_model: loaded.default_model.clone(),
        resolved_default_model: app.configured_default_model(),
        thinking_default: loaded.thinking_default,
        theme_default: loaded.theme_default.clone(),
        context_soft_percent: loaded.context_soft_percent,
        providers,
        sources,
    }
}

/// A provider base URL without credentials, query or fragment.
///
/// The provider's endpoint is useful context; the userinfo of a URL is a
/// credential and a query string commonly carries a token. Neither belongs in
/// a client answer.
pub(crate) fn display_safe_url(raw: &str) -> String {
    let raw = raw.trim();
    let Some(scheme_end) = raw.find("://") else {
        return raw.to_string();
    };
    let authority_start = scheme_end + 3;
    let rest = &raw[authority_start..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let tail = &rest[end..];
    let tail = tail.split(['?', '#']).next().unwrap_or("");
    format!("{}://{}{}", &raw[..scheme_end], host, tail)
}

#[cfg(test)]
mod tests {
    use super::display_safe_url;

    #[test]
    fn a_url_never_leaks_userinfo_query_or_fragment() {
        assert_eq!(
            display_safe_url("https://user:secret@example.com/v1?token=abc#x"),
            "https://example.com/v1"
        );
        assert_eq!(
            display_safe_url("https://api.deepseek.com/v1"),
            "https://api.deepseek.com/v1"
        );
        assert_eq!(display_safe_url("local-config"), "local-config");
    }
}
