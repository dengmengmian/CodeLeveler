//! Host-side Agent browser default for the desktop bridge.
//!
//! `[browser].default` is consumed once when the runtime assembles its browser
//! handle, so the payload says `restart_required` instead of implying the live
//! handle changed.

use std::sync::Arc;

use leveler_app::browser_config;
use leveler_app::browser_config::BrowserProduct;
use leveler_core::LevelerHome;
use leveler_local_transport::LocalSocketRuntimeClient;
use serde_json::Value;

pub(crate) fn view(home: &LevelerHome) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(browser_config::view(
        &home.config_file(),
        leveler_core::environment(),
    )?)?)
}

pub(crate) async fn save(
    home: &LevelerHome,
    client: Option<&Arc<LocalSocketRuntimeClient>>,
    params: &Value,
) -> anyhow::Result<Value> {
    let product = match params.get("product") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => Some(
            BrowserProduct::parse(raw)
                .ok_or_else(|| anyhow::anyhow!("unknown browser product: {raw}"))?,
        ),
        Some(_) => anyhow::bail!("product must be a string or null"),
    };
    browser_config::save(&home.config_file(), product)?;
    let reloaded = crate::desktop_provider::reload(client).await;
    let mut payload = view(home)?;
    payload["runtime_reload"] = reloaded;
    Ok(payload)
}
