//! Host-side global option toggles for the desktop bridge.
//!
//! These are live runtime/Git facts, not session state. The renderer sends a
//! bounded boolean draft; the write shares the locked config transaction with
//! MCP, provider and search edits.

use std::sync::Arc;

use leveler_app::agent_options::{self, AgentOptionsDraft};
use leveler_core::LevelerHome;
use leveler_local_transport::LocalSocketRuntimeClient;
use serde_json::Value;

pub(crate) fn view(home: &LevelerHome) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(agent_options::view(
        &home.config_file(),
    )?)?)
}

pub(crate) async fn save(
    home: &LevelerHome,
    client: Option<&Arc<LocalSocketRuntimeClient>>,
    params: &Value,
) -> anyhow::Result<Value> {
    let boolean = |key: &str| -> anyhow::Result<bool> {
        params
            .get(key)
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow::anyhow!("{key} must be a boolean"))
    };
    let draft = AgentOptionsDraft {
        delegation: boolean("delegation")?,
        co_author: boolean("co_author")?,
        prompt_suggestions: boolean("prompt_suggestions")?,
        away_summary: boolean("away_summary")?,
    };
    agent_options::save(&home.config_file(), &draft)?;
    let reloaded = crate::desktop_provider::reload(client).await;
    let mut payload = view(home)?;
    payload["runtime_reload"] = reloaded;
    Ok(payload)
}
