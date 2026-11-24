//! MCP management served from the runtime.
//!
//! The runtime's [`McpToolSource`](crate::McpToolSource) owns every live
//! connection; the configuration file owns the declarations. This module is
//! the one place that combines them into the wire projection, so the TUI, the
//! doctor page and any later client read the same answer.
//!
//! Management writes go through the same config editor the CLI uses and are
//! followed by the runtime's normal configuration reload. A write that the
//! running generation could not apply is reported as an error — never as a
//! saved success.

use std::sync::Arc;

use leveler_client_protocol::mcp::{
    McpConnectionState, McpRequest, McpResponse, McpServerSpec, McpServerView, McpToolAdmission,
    McpToolView, McpTransport,
};
use leveler_execution::PermissionProfile;
use leveler_tools::tool::Tool;

use crate::Application;

/// The maximum number of tools a single server contributes to one answer.
pub(crate) const MAX_TOOLS_PER_SERVER: usize = 200;
/// The maximum description length kept from a server's tool metadata.
const MAX_DESCRIPTION_CHARS: usize = 1024;
/// The maximum serialized JSON Schema length kept from a server's tool.
const MAX_SCHEMA_BYTES: usize = 16 * 1024;

/// One discovered tool, bounded and with display-safe metadata.
#[derive(Debug, Clone)]
pub(crate) struct McpToolDescriptor {
    pub remote_name: String,
    pub exposed_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub truncated: bool,
    pub risk: String,
}

/// The runtime's current facts about one configured server.
#[derive(Debug, Clone)]
pub(crate) struct McpServerRuntime {
    pub state: McpConnectionState,
    pub last_error: Option<String>,
    pub last_attempt_at: Option<String>,
    pub connected_at: Option<String>,
    pub updated_at: String,
    pub generation: u64,
    pub protocol: Option<String>,
    pub initialized: bool,
    pub tools: Vec<McpToolDescriptor>,
    /// The count before the per-answer cap was applied.
    pub tool_total: usize,
    pub tools_truncated: bool,
}

impl McpServerRuntime {
    pub(crate) fn not_connected() -> Self {
        Self {
            state: McpConnectionState::NotConnected,
            last_error: None,
            last_attempt_at: None,
            connected_at: None,
            updated_at: leveler_core::now().to_rfc3339(),
            generation: 0,
            protocol: None,
            initialized: false,
            tools: Vec::new(),
            tool_total: 0,
            tools_truncated: false,
        }
    }

    pub(crate) fn connecting(generation: u64) -> Self {
        Self {
            state: McpConnectionState::Connecting,
            updated_at: leveler_core::now().to_rfc3339(),
            generation,
            ..Self::not_connected()
        }
    }

    pub(crate) fn connected(
        generation: u64,
        protocol: Option<String>,
        tools: Vec<McpToolDescriptor>,
        tool_total: usize,
    ) -> Self {
        let now = leveler_core::now().to_rfc3339();
        let tools_truncated =
            tool_total > MAX_TOOLS_PER_SERVER || tools.len() > MAX_TOOLS_PER_SERVER;
        Self {
            state: McpConnectionState::Connected,
            last_error: None,
            last_attempt_at: Some(now.clone()),
            connected_at: Some(now.clone()),
            updated_at: now,
            generation,
            protocol,
            initialized: true,
            tools: tools.into_iter().take(MAX_TOOLS_PER_SERVER).collect(),
            tool_total,
            tools_truncated,
        }
    }

    pub(crate) fn failed(generation: u64, reason: String) -> Self {
        let now = leveler_core::now().to_rfc3339();
        Self {
            state: McpConnectionState::Failed,
            last_error: Some(redact_reason(&reason)),
            last_attempt_at: Some(now.clone()),
            connected_at: None,
            updated_at: now,
            generation,
            protocol: None,
            initialized: false,
            tools: Vec::new(),
            tool_total: 0,
            tools_truncated: false,
        }
    }

    pub(crate) fn degraded(&self) -> Self {
        Self {
            state: McpConnectionState::Degraded,
            connected_at: None,
            tools: Vec::new(),
            tool_total: 0,
            tools_truncated: false,
            updated_at: leveler_core::now().to_rfc3339(),
            ..self.clone()
        }
    }
}

/// Bound one live tool's metadata for the wire.
pub(crate) fn descriptor_for(server: &str, tool: &Arc<dyn Tool>) -> McpToolDescriptor {
    let exposed_name = tool.name().to_string();
    let remote_name = exposed_name
        .strip_prefix(&format!("mcp__{server}__"))
        .unwrap_or(&exposed_name)
        .to_string();
    let mut truncated = false;
    let description = tool.description().to_string();
    let description = if description.chars().count() > MAX_DESCRIPTION_CHARS {
        truncated = true;
        description
            .chars()
            .take(MAX_DESCRIPTION_CHARS)
            .collect::<String>()
    } else {
        description
    };
    let schema = tool.input_schema();
    let input_schema = match serde_json::to_string(&schema) {
        Ok(text) if text.len() > MAX_SCHEMA_BYTES => {
            truncated = true;
            serde_json::json!({
                "type": "object",
                "x-leveler-truncated": true,
            })
        }
        _ => schema,
    };
    McpToolDescriptor {
        remote_name,
        exposed_name,
        description,
        input_schema,
        truncated,
        risk: "network".to_string(),
    }
}

/// Redact a connection reason at the wire boundary. The transport already
/// strips the values this server was given; this second boundary also handles
/// generic credential shapes in text produced before a client existed, and
/// keeps the page from carrying an unbounded server string.
fn redact_reason(reason: &str) -> String {
    let mut out = leveler_tools::mcp::redact_sensitive_text(reason, &[]);
    // A server-provided failure text is untrusted output, not an instruction
    // and not a place to carry a megabyte into the page.
    const MAX_REASON_CHARS: usize = 2048;
    if out.chars().count() > MAX_REASON_CHARS {
        out = out.chars().take(MAX_REASON_CHARS).collect::<String>() + "…";
    }
    out
}

/// The admission projection for the session's current profile.
pub(crate) fn admission_for(
    contract: leveler_client_protocol::PermissionContract,
    mode: PermissionProfile,
) -> McpToolAdmission {
    let kind = leveler_execution::PolicyKind::of(
        match contract {
            leveler_client_protocol::PermissionContract::V1 => {
                leveler_execution::PermissionContract::V1
            }
            leveler_client_protocol::PermissionContract::V2 => {
                leveler_execution::PermissionContract::V2
            }
        },
        mode,
    );
    match kind {
        leveler_execution::PolicyKind::LegacyRestricted => McpToolAdmission::RequiresNetworkGrant,
        leveler_execution::PolicyKind::LegacyAuto
        | leveler_execution::PolicyKind::EffectAuto
        | leveler_execution::PolicyKind::Full => McpToolAdmission::Admitted,
    }
}

/// Build the authoritative server list.
pub(crate) async fn servers(
    app: &Application,
    contract: leveler_client_protocol::PermissionContract,
    mode: PermissionProfile,
) -> Result<Vec<McpServerView>, String> {
    let generation = app.config_snapshot();
    let states = app
        .mcp_source()
        .states(&generation.loaded.mcp_servers)
        .await;
    let entries = crate::mcp_config::list().map_err(|error| error.to_string())?;
    let admission = admission_for(contract, mode);
    Ok(entries
        .into_iter()
        .map(|entry| {
            let runtime = states.get(&entry.name);
            let enabled = entry.enabled;
            let state = if !enabled {
                McpConnectionState::Disabled
            } else {
                runtime
                    .map(|runtime| runtime.state)
                    .unwrap_or(McpConnectionState::NotConnected)
            };
            let tools = if enabled && state.is_connected() {
                runtime
                    .map(|runtime| {
                        runtime
                            .tools
                            .iter()
                            .map(|tool| McpToolView {
                                remote_name: tool.remote_name.clone(),
                                exposed_name: tool.exposed_name.clone(),
                                description: tool.description.clone(),
                                input_schema: tool.input_schema.clone(),
                                truncated: tool.truncated,
                                risk: tool.risk.clone(),
                                admission,
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            McpServerView {
                name: entry.name.clone(),
                transport: if entry.transport == "streamable_http" {
                    McpTransport::StreamableHttp
                } else {
                    McpTransport::Stdio
                },
                endpoint: entry.endpoint_label(),
                source: crate::mcp_config::config_path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                spec: McpServerSpec {
                    name: entry.name.clone(),
                    transport: if entry.transport == "streamable_http" {
                        McpTransport::StreamableHttp
                    } else {
                        McpTransport::Stdio
                    },
                    command: (!entry.command.is_empty()).then(|| entry.command.clone()),
                    args: entry.args.clone(),
                    env: entry
                        .env_keys
                        .iter()
                        .zip(entry.env_sources.iter())
                        .map(|(key, source)| leveler_client_protocol::mcp::McpNameRef {
                            key: key.clone(),
                            source: source.clone(),
                        })
                        .collect(),
                    url: entry.url.clone(),
                    headers: entry
                        .header_names
                        .iter()
                        .zip(entry.header_sources.iter())
                        .map(|(key, source)| leveler_client_protocol::mcp::McpNameRef {
                            key: key.clone(),
                            source: source.clone(),
                        })
                        .collect(),
                    bearer_token_env: entry.bearer_token_env.clone(),
                    enabled: entry.enabled,
                },
                enabled,
                state,
                protocol: runtime.and_then(|runtime| runtime.protocol.clone()),
                initialized: runtime.is_some_and(|runtime| runtime.initialized),
                tool_count: runtime
                    .filter(|runtime| runtime.initialized && state.is_connected())
                    .map(|runtime| runtime.tool_total),
                tools,
                tools_truncated: runtime.is_some_and(|runtime| runtime.tools_truncated),
                last_error: runtime.and_then(|runtime| runtime.last_error.clone()),
                last_attempt_at: runtime.and_then(|runtime| runtime.last_attempt_at.clone()),
                connected_at: runtime.and_then(|runtime| runtime.connected_at.clone()),
                updated_at: runtime.map(|runtime| runtime.updated_at.clone()),
                generation: runtime.map(|runtime| runtime.generation).unwrap_or(0),
                retry_in_seconds: None,
            }
        })
        .collect())
}

/// Run one management request against the runtime.
pub(crate) async fn manage(
    app: &Application,
    contract: leveler_client_protocol::PermissionContract,
    mode: PermissionProfile,
    request: McpRequest,
) -> McpResponse {
    if matches!(request, McpRequest::List) {
        return list_response(app, contract, mode).await;
    }
    let result: Result<String, String> = async {
        match request {
            McpRequest::List => unreachable!("handled above"),
            McpRequest::Reload => {
                reload(app).await?;
                Ok("configuration reloaded".to_string())
            }
            McpRequest::Reconnect { server } => reconnect(app, mode, &server).await,
            McpRequest::Add { spec } => {
                validate_spec(&spec)?;
                let path = crate::mcp_config::config_path().map_err(|e| e.to_string())?;
                crate::mcp_config::add_spec_at(&path, &spec).map_err(|e| e.to_string())?;
                app.mcp_source().invalidate(&spec.name).await;
                apply_written_config(app).await
            }
            McpRequest::Update {
                original_name,
                spec,
            } => {
                validate_spec(&spec)?;
                let path = crate::mcp_config::config_path().map_err(|e| e.to_string())?;
                crate::mcp_config::update_spec_at(&path, &original_name, &spec)
                    .map_err(|e| e.to_string())?;
                // The old name and the new name both stop describing the
                // cached connection; a rename must not leave old tools behind.
                app.mcp_source().invalidate(&original_name).await;
                app.mcp_source().invalidate(&spec.name).await;
                apply_written_config(app).await
            }
            McpRequest::Remove { server } => {
                let path = crate::mcp_config::config_path().map_err(|e| e.to_string())?;
                crate::mcp_config::remove_at(&path, &server).map_err(|e| e.to_string())?;
                app.mcp_source().invalidate(&server).await;
                apply_written_config(app).await
            }
            McpRequest::SetEnabled { server, enabled } => {
                let path = crate::mcp_config::config_path().map_err(|e| e.to_string())?;
                crate::mcp_config::set_enabled_at(&path, &server, enabled)
                    .map_err(|e| e.to_string())?;
                app.mcp_source().invalidate(&server).await;
                apply_written_config(app).await
            }
        }
    }
    .await;
    match result {
        Ok(_) => match servers(app, contract, mode).await {
            Ok(servers) => McpResponse::Changed {
                servers,
                message: "updated".to_string(),
            },
            Err(message) => McpResponse::Error {
                servers: Vec::new(),
                message,
            },
        },
        Err(message) => McpResponse::Error {
            servers: servers(app, contract, mode).await.unwrap_or_default(),
            message,
        },
    }
}

async fn list_response(
    app: &Application,
    contract: leveler_client_protocol::PermissionContract,
    mode: PermissionProfile,
) -> McpResponse {
    match servers(app, contract, mode).await {
        Ok(servers) => McpResponse::Snapshot { servers },
        Err(message) => McpResponse::Error {
            servers: Vec::new(),
            message,
        },
    }
}

async fn reload(app: &Application) -> Result<(), String> {
    match app
        .reload_config(crate::config_manager::ReloadOrigin::Command, true)
        .await
    {
        crate::config_manager::ReloadOutcome::Applied(_) => {
            sync_cache(app).await;
            Ok(())
        }
        crate::config_manager::ReloadOutcome::Unchanged { .. } => {
            sync_cache(app).await;
            Ok(())
        }
        crate::config_manager::ReloadOutcome::Failed { error, .. } => {
            Err(format!("configuration could not be applied: {error}"))
        }
    }
}

async fn apply_written_config(app: &Application) -> Result<String, String> {
    // The file write is not application. Re-read through the runtime's own
    // manager; a failure here is reported, never painted as saved success.
    reload(app).await.map_err(|error| {
        format!(
            "the configuration file was written, but the running runtime did not apply it: {error}"
        )
    })?;
    Ok("configuration applied".to_string())
}

async fn sync_cache(app: &Application) {
    let generation = app.config_snapshot();
    app.mcp_source().sync(&generation.loaded.mcp_servers).await;
}

async fn reconnect(
    app: &Application,
    mode: PermissionProfile,
    server: &str,
) -> Result<String, String> {
    let generation = app.config_snapshot();
    let Some(config) = generation
        .loaded
        .mcp_servers
        .iter()
        .find(|config| config.name == server)
    else {
        let entries = crate::mcp_config::list().map_err(|error| error.to_string())?;
        let Some(entry) = entries.into_iter().find(|entry| entry.name == server) else {
            return Err(format!("no MCP server named `{server}`"));
        };
        if !entry.enabled {
            return Err(format!("MCP server `{server}` is disabled"));
        }
        return Err(format!(
            "MCP server `{server}` is configured but the running generation has not loaded it; reload the configuration first"
        ));
    };
    app.mcp_source()
        .reconnect(config, mode)
        .await
        .map(|_| format!("server `{server}` reconnected"))
}

/// Validate a spec before the config editor sees it. The editor repeats the
/// checks it needs; this is the boundary that gives the client one message.
pub(crate) fn validate_spec(spec: &McpServerSpec) -> Result<(), String> {
    crate::mcp_config::validate_name(&spec.name)?;
    match spec.transport {
        McpTransport::Stdio => {
            if spec.command.as_deref().unwrap_or("").trim().is_empty() {
                return Err("a stdio server requires a command".into());
            }
            if spec
                .url
                .as_deref()
                .is_some_and(|url| !url.trim().is_empty())
                || !spec.headers.is_empty()
                || spec.bearer_token_env.is_some()
            {
                return Err("a stdio server does not use a URL, headers or a bearer token".into());
            }
        }
        McpTransport::StreamableHttp => {
            let url = spec.url.as_deref().unwrap_or("").trim();
            if url.is_empty() {
                return Err("a streamable HTTP server requires a URL".into());
            }
            leveler_tools::mcp::validate_streamable_http_url(url)
                .map_err(|error| format!("invalid URL: {error}"))?;
            if spec
                .command
                .as_deref()
                .is_some_and(|c| !c.trim().is_empty())
                || !spec.args.is_empty()
                || !spec.env.is_empty()
            {
                return Err("a streamable HTTP server does not use a command, args or env".into());
            }
        }
    }
    for reference in spec.env.iter().chain(spec.headers.iter()) {
        crate::mcp_config::normalize_env_ref(&reference.key, &reference.source)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_follows_the_contract_and_tier() {
        use leveler_client_protocol::PermissionContract;
        assert_eq!(
            admission_for(PermissionContract::V2, PermissionProfile::Assisted),
            McpToolAdmission::Admitted
        );
        assert_eq!(
            admission_for(PermissionContract::V2, PermissionProfile::FullAccess),
            McpToolAdmission::Admitted
        );
        assert_eq!(
            admission_for(PermissionContract::V1, PermissionProfile::RequestApproval),
            McpToolAdmission::RequiresNetworkGrant
        );
        // The first tier inherits the old Auto: MCP is admitted.
        assert_eq!(
            admission_for(PermissionContract::V2, PermissionProfile::RequestApproval),
            McpToolAdmission::Admitted
        );
    }

    #[test]
    fn a_reason_never_keeps_url_credentials_or_query() {
        let redacted =
            redact_reason("connect to https://user:secret@example.com/mcp?token=abc failed");
        assert!(!redacted.contains("secret"), "{redacted}");
        assert!(!redacted.contains("token"), "{redacted}");
        assert!(redacted.contains("https://example.com/mcp"));
    }

    #[test]
    fn a_connected_runtime_carries_bounded_tools() {
        let runtime = McpServerRuntime::connected(3, Some("2024-11-05".into()), Vec::new(), 0);
        assert_eq!(runtime.state, McpConnectionState::Connected);
        assert!(runtime.initialized);
        assert_eq!(runtime.generation, 3);
        assert_eq!(runtime.protocol.as_deref(), Some("2024-11-05"));
    }

    #[test]
    fn a_large_catalog_is_bounded_but_keeps_its_true_count() {
        let tools = (0..(MAX_TOOLS_PER_SERVER + 5))
            .map(|i| McpToolDescriptor {
                remote_name: format!("tool-{i}"),
                exposed_name: format!("mcp__s__tool-{i}"),
                description: "d".into(),
                input_schema: serde_json::json!({}),
                truncated: false,
                risk: "network".into(),
            })
            .collect::<Vec<_>>();
        let runtime = McpServerRuntime::connected(1, None, tools, MAX_TOOLS_PER_SERVER + 5);
        assert_eq!(runtime.tools.len(), MAX_TOOLS_PER_SERVER);
        assert!(runtime.tools_truncated);
        assert_eq!(runtime.tool_total, MAX_TOOLS_PER_SERVER + 5);
    }
}
