//! Host-side search settings for the desktop bridge.
//!
//! The renderer receives only the display-safe projection: provider, endpoint,
//! bounds and whether a credential exists. A key is accepted once and written
//! through the shared configuration transaction; it is never sent back.

use std::sync::Arc;

use leveler_app::search_config::{self, SearchDraft};
use leveler_core::LevelerHome;
use leveler_local_transport::LocalSocketRuntimeClient;
use serde_json::Value;

/// A saved request is bounded before it reaches the config parser.
const MAX_PROVIDER: usize = 64;
const MAX_KEY: usize = 16 * 1024;
const MAX_ENV: usize = 256;
const MAX_BASE_URL: usize = 2048;

pub(crate) fn view(home: &LevelerHome) -> anyhow::Result<Value> {
    let path = home.config_file();
    let view = search_config::view(&path)?;
    Ok(serde_json::to_value(view)?)
}

pub(crate) async fn save(
    home: &LevelerHome,
    client: Option<&Arc<LocalSocketRuntimeClient>>,
    params: &Value,
) -> anyhow::Result<Value> {
    let draft = parse_draft(params)?;
    search_config::save(&home.config_file(), &draft)?;
    let reloaded = crate::desktop_provider::reload(client).await;
    let mut payload = view(home)?;
    payload["runtime_reload"] = reloaded;
    Ok(payload)
}

fn parse_draft(params: &Value) -> anyhow::Result<SearchDraft> {
    let enabled = params
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow::anyhow!("enabled must be a boolean"))?;
    let provider = optional_text(params, "provider", MAX_PROVIDER)?;
    let api_key = optional_text(params, "api_key", MAX_KEY)?;
    let clear_api_key = params
        .get("clear_api_key")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if api_key.as_deref().is_some_and(|key| key.trim().is_empty()) {
        anyhow::bail!("API Key 不能是空白");
    }
    let api_key_env = optional_text(params, "api_key_env", MAX_ENV)?;
    let base_url = optional_text(params, "base_url", MAX_BASE_URL)?;
    let max_results = optional_usize(params, "max_results", 1..=50)?;
    let timeout_ms = optional_u64(params, "timeout_ms", 1..=600_000)?;
    Ok(SearchDraft {
        enabled,
        provider,
        api_key,
        clear_api_key,
        api_key_env,
        base_url,
        max_results,
        timeout_ms,
    })
}

fn optional_text(params: &Value, key: &str, max: usize) -> anyhow::Result<Option<String>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.chars().count() <= max => Ok(Some(value.clone())),
        Some(Value::String(_)) => anyhow::bail!("{key} is too long"),
        Some(_) => anyhow::bail!("{key} must be a string or null"),
    }
}

fn optional_usize(
    params: &Value,
    key: &str,
    range: std::ops::RangeInclusive<usize>,
) -> anyhow::Result<Option<usize>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let number = value
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("{key} must be an integer or null"))?;
            let number =
                usize::try_from(number).map_err(|_| anyhow::anyhow!("{key} is out of range"))?;
            if !range.contains(&number) {
                anyhow::bail!("{key} is out of range");
            }
            Ok(Some(number))
        }
    }
}

fn optional_u64(
    params: &Value,
    key: &str,
    range: std::ops::RangeInclusive<u64>,
) -> anyhow::Result<Option<u64>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let number = value
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("{key} must be an integer or null"))?;
            if !range.contains(&number) {
                anyhow::bail!("{key} is out of range");
            }
            Ok(Some(number))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base(enabled: bool) -> Value {
        json!({"enabled": enabled, "provider": "deepseek"})
    }

    #[test]
    fn drafts_are_bounded_and_typed() {
        assert!(parse_draft(&base(true)).is_ok());
        assert!(parse_draft(&json!({"enabled": "yes"})).is_err());
        assert!(parse_draft(&json!({"enabled": true, "provider": 3})).is_err());
        assert!(parse_draft(&json!({"enabled": true, "max_results": 51})).is_err());
        assert!(parse_draft(&json!({"enabled": true, "max_results": "5"})).is_err());
        assert!(parse_draft(&json!({"enabled": true, "timeout_ms": 0})).is_err());
        assert!(parse_draft(&json!({"enabled": true, "api_key": "   "})).is_err());
        assert!(parse_draft(&json!({"enabled": false, "provider": null})).is_ok());
    }
}
