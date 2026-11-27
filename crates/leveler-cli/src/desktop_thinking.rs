//! Host-side global Thinking default for the desktop bridge.
//!
//! The value is a config fact, not session state. A save shares the locked
//! config transaction with every other editor; the runtime reload is reported
//! back so the page never claims a change reached a process that did not read it.

use std::sync::Arc;

use leveler_app::thinking_config;
use leveler_core::LevelerHome;
use leveler_local_transport::LocalSocketRuntimeClient;
use leveler_model::ThinkingLevel;
use serde_json::Value;

pub(crate) fn view(home: &LevelerHome) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(thinking_config::view(
        &home.config_file(),
    )?)?)
}

pub(crate) async fn save(
    home: &LevelerHome,
    client: Option<&Arc<LocalSocketRuntimeClient>>,
    params: &Value,
) -> anyhow::Result<Value> {
    let level = match params.get("level") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => Some(
            ThinkingLevel::parse(raw)
                .ok_or_else(|| anyhow::anyhow!("unknown Thinking level: {raw}"))?,
        ),
        Some(_) => anyhow::bail!("level must be a string or null"),
    };
    thinking_config::save(&home.config_file(), level)?;
    let reloaded = crate::desktop_provider::reload(client).await;
    let mut payload = view(home)?;
    payload["runtime_reload"] = reloaded;
    Ok(payload)
}
