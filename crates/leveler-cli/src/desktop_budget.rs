//! Host-side Goal resource-budget settings for the desktop bridge.
//!
//! The page edits the USER-GLOBAL `[limits]` table; project overrides are read
//! from `.leveler/config.yaml` and reported, never rewritten here. The view and
//! write share the same locked config transaction as every other global
//! setting, and the effective merge is the same one the runtime resolves for a
//! run — the page cannot present a different arithmetic.
//!
//! Caps are read when a run is constructed, so a save governs the next turn.
//! The payload says so; it never claims a running turn changed.

use std::path::PathBuf;
use std::sync::Arc;

use leveler_app::budget_config::{self, BudgetValues};
use leveler_local_transport::LocalSocketRuntimeClient;
use leveler_project::Layout;
use serde_json::Value;

/// The global config file this page edits: the layout's home when a workspace
/// is open, otherwise the process's resolved home.
fn config_file(layout: Option<&Layout>) -> PathBuf {
    layout
        .map(|layout| layout.home().config_file())
        .unwrap_or_else(|| {
            leveler_core::LevelerHome::resolve(leveler_core::environment()).config_file()
        })
}

pub(crate) fn view(layout: Option<&Layout>) -> anyhow::Result<Value> {
    let project_root = layout
        .and_then(Layout::primary_workspace)
        .map(std::path::Path::to_path_buf);
    Ok(serde_json::to_value(budget_config::view(
        &config_file(layout),
        project_root.as_deref(),
    ))?)
}

pub(crate) async fn save(
    layout: Option<&Layout>,
    client: Option<&Arc<LocalSocketRuntimeClient>>,
    params: &Value,
) -> anyhow::Result<Value> {
    let optional = |key: &str| -> anyhow::Result<Option<u64>> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(number)) => number
                .as_u64()
                .map(Some)
                .ok_or_else(|| anyhow::anyhow!("{key} 必须是非负整数或留空")),
            Some(_) => anyhow::bail!("{key} 必须是非负整数或留空"),
        }
    };
    let soft = match params.get("soft_limit_percent") {
        None | Some(Value::Null) => None,
        Some(Value::Number(number)) => Some(
            number
                .as_u64()
                .and_then(|value| u8::try_from(value).ok())
                .ok_or_else(|| anyhow::anyhow!("提醒阈值必须是 1 到 100 之间的整数"))?,
        ),
        Some(_) => anyhow::bail!("soft_limit_percent 必须是 1 到 100 之间的整数"),
    };
    let draft = BudgetValues {
        max_model_tokens: optional("max_model_tokens")?,
        max_cost_usd_micros: optional("max_cost_usd_micros")?,
        max_duration_seconds: optional("max_duration_seconds")?,
        soft_limit_percent: soft,
    };
    budget_config::save(&config_file(layout), &draft)?;
    let reloaded = crate::desktop_provider::reload(client).await;
    let mut payload = view(layout)?;
    payload["runtime_reload"] = reloaded;
    Ok(payload)
}
