//! Minimal MCP (Model Context Protocol) stdio client.
//!
//! Spawns a server process, completes the JSON-RPC `initialize` handshake, lists
//! its tools, and proxies tool calls. Each discovered tool is exposed to the
//! model as a [`Tool`] named `mcp__<server>__<tool>` so it can't collide with a
//! built-in. A failed server (won't start, times out) is skipped with a log —
//! it never aborts startup.

use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

mod mcp_http;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Test-only override for a live-server timeout test. Debug builds only; a
/// release build ignores the variable and keeps the production timeout.
fn request_timeout() -> Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("LEVELER_TEST_MCP_TIMEOUT_MS")
        && let Ok(ms) = raw.parse::<u64>()
        && ms > 0
    {
        return Duration::from_millis(ms);
    }
    REQUEST_TIMEOUT
}
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Configuration for one MCP server.
///
/// A server with [`Self::url`] set uses Streamable HTTP. Otherwise it is the
/// original stdio server (`command` / `args` / `env`). Old configs leave `url`
/// empty and keep working.
#[derive(Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Streamable HTTP endpoint. `None` means stdio.
    pub url: Option<String>,
    /// Resolved request headers. Values are secrets and are not printed.
    pub headers: Vec<(String, String)>,
    /// Bearer token without the `Bearer ` prefix. Not printed.
    pub bearer_token: Option<String>,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("args", &self.args)
            .field(
                "env_keys",
                &self.env.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .field("url", &self.url)
            .field(
                "header_names",
                &self.headers.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .field("bearer_token", &self.bearer_token.as_ref().map(|_| "[set]"))
            .finish()
    }
}

impl McpServerConfig {
    /// A stdio server. This is the pre-HTTP configuration shape.
    pub fn stdio(
        name: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args,
            env,
            url: None,
            headers: Vec::new(),
            bearer_token: None,
        }
    }

    /// A Streamable HTTP server. Header values and the bearer token are already
    /// resolved; they must not be written back to config or logs.
    pub fn streamable_http(
        name: impl Into<String>,
        url: impl Into<String>,
        headers: Vec<(String, String)>,
        bearer_token: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            command: String::new(),
            args: Vec::new(),
            env: Vec::new(),
            url: Some(url.into()),
            headers,
            bearer_token,
        }
    }

    pub fn is_streamable_http(&self) -> bool {
        self.url.is_some()
    }

    /// Endpoint safe to show: the URL, or the stdio command. No header values.
    pub fn endpoint_label(&self) -> String {
        if let Some(url) = &self.url {
            url.clone()
        } else {
            std::iter::once(self.command.as_str())
                .chain(self.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        }
    }
}

/// Reject a Streamable HTTP URL that is not `http`/`https`, or that carries
/// credentials in the userinfo or the query. The error text never echoes the
/// rejected value.
pub fn validate_streamable_http_url(url: &str) -> Result<(), String> {
    mcp_http::validate_mcp_url(url)
}

/// A discovered MCP tool's advertised shape.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>>;

/// A live connection to one MCP server.
pub struct McpClient {
    config: McpServerConfig,
    environment: leveler_core::EnvSnapshot,
    unrestricted: bool,
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    /// Set when the server's stdout closed: every discovered tool now points at
    /// a dead pipe and the next turn must reconnect this server.
    closed: Arc<std::sync::atomic::AtomicBool>,
    _child: Child,
}

impl McpClient {
    /// Spawn the server and complete the `initialize` handshake.
    pub async fn connect(cfg: &McpServerConfig) -> Result<Arc<Self>, String> {
        Self::connect_with_authority(cfg, leveler_core::environment(), false).await
    }

    /// The application owns this launch authority; MCP configuration cannot
    /// promote its own inherited environment to FullAccess.
    pub async fn connect_with_authority(
        cfg: &McpServerConfig,
        environment: &leveler_core::EnvSnapshot,
        unrestricted: bool,
    ) -> Result<Arc<Self>, String> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        cmd.env_clear();
        if unrestricted {
            cmd.envs(environment.vars_os());
        } else {
            cmd.envs(environment.scrubbed_vars_os());
        }
        // Explicit configuration overrides either inherited authority environment.
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("启动 MCP 服务 `{}` 失败:{e}", cfg.name))?;
        let stdin = child.stdin.take().ok_or("MCP: 无 stdin")?;
        let stdout = child.stdout.take().ok_or("MCP: 无 stdout")?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Reader task: route each JSON-RPC response line to its waiting request;
        // fail everything pending when the stream closes.
        {
            let pending = pending.clone();
            let closed = closed.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    // Notifications carry no id — ignore them.
                    let Some(id) = msg.get("id").and_then(|v| v.as_u64()) else {
                        continue;
                    };
                    if let Some(tx) = pending.lock().await.remove(&id) {
                        let result = if let Some(err) = msg.get("error") {
                            Err(err
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("MCP error")
                                .to_string())
                        } else {
                            Ok(msg
                                .get("result")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null))
                        };
                        let _ = tx.send(result);
                    }
                }
                for (_, tx) in pending.lock().await.drain() {
                    let _ = tx.send(Err("MCP 连接已关闭".to_string()));
                }
                closed.store(true, Ordering::SeqCst);
            });
        }

        let client = Arc::new(Self {
            config: cfg.clone(),
            environment: environment.clone(),
            unrestricted,
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            closed,
            _child: child,
        });

        client
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "leveler", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        client
            .notify("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(client)
    }

    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let msg =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.write_line(&msg).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(request_timeout(), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("MCP 响应通道关闭".to_string()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                // Best effort: the protocol has a cancellation notification.
                // Sending it does not prove the server stopped; the request
                // may already have produced a side effect, so the outcome
                // stays Unknown.
                let cancelled = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": { "requestId": id, "reason": "timeout" },
                });
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    self.write_line(&cancelled),
                )
                .await;
                Err(format!(
                    "UNKNOWN_OUTCOME:MCP 请求 `{method}` 超时；请求可能已送达服务端并产生副作用，结果未知，未自动重试"
                ))
            }
        }
    }

    async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), String> {
        let msg = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write_line(&msg).await
    }

    async fn write_line(&self, msg: &serde_json::Value) -> Result<(), String> {
        let mut line = serde_json::to_string(msg).map_err(|e| e.to_string())?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        stdin.flush().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Best-effort `notifications/cancelled` for every in-flight request.
    /// A cancellation notification is a request to stop, never confirmation
    /// that the server stopped or that the call had no side effect.
    pub async fn cancel_pending(&self, reason: &str) {
        let ids: Vec<u64> = self.pending.lock().await.keys().copied().collect();
        for id in ids {
            self.pending.lock().await.remove(&id);
            let cancelled = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": id, "reason": reason },
            });
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                self.write_line(&cancelled),
            )
            .await;
        }
    }

    /// List the server's tools, following `nextCursor` up to 64 pages.
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, String> {
        let mut tools: Vec<McpToolInfo> = Vec::new();
        let mut seen_cursors = HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..64 {
            let params = match &cursor {
                Some(cursor) => serde_json::json!({ "cursor": cursor }),
                None => serde_json::json!({}),
            };
            let result = self.request("tools/list", params).await?;
            for info in parse_tools(&result) {
                if info.name.is_empty() || info.name.chars().any(|c| c.is_control()) {
                    continue;
                }
                if tools.iter().any(|existing| existing.name == info.name) {
                    tracing::warn!(
                        tool = %info.name,
                        "MCP server returned a duplicate tool name; keeping the first"
                    );
                    continue;
                }
                tools.push(info);
            }
            match result.get("nextCursor").and_then(|v| v.as_str()) {
                Some(next) if seen_cursors.insert(next.to_string()) => {
                    cursor = Some(next.to_string());
                }
                _ => break,
            }
        }
        Ok(tools)
    }

    /// Call a tool by its remote name, returning its text content.
    pub async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<String, String> {
        let result = self
            .request(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": args }),
            )
            .await?;
        let content = format_tool_result(&result);
        if result.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
            Err(content)
        } else {
            Ok(content)
        }
    }
}

/// Parse a `tools/list` result into tool descriptors.
fn parse_tools(result: &serde_json::Value) -> Vec<McpToolInfo> {
    result
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let name = t.get("name")?.as_str()?.to_string();
                    let description = t
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input_schema = t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
                    Some(McpToolInfo {
                        name,
                        description,
                        input_schema,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Flatten an MCP `tools/call` result's content array into plain text.
fn format_tool_result(result: &serde_json::Value) -> String {
    let Some(items) = result.get("content").and_then(|v| v.as_array()) else {
        return serde_json::to_string(result).unwrap_or_default();
    };
    let mut out = String::new();
    for item in items {
        match item.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
            Some(other) => out.push_str(&format!("[{other} content]\n")),
            None => {}
        }
    }
    if let Some(structured) = result.get("structuredContent") {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("structuredContent: ");
        out.push_str(&structured.to_string());
    }
    out.trim_end().to_string()
}

enum McpLink {
    Stdio(Arc<McpClient>),
    Http(Arc<mcp_http::HttpSession>),
}

/// A model-facing tool that proxies to an MCP server. Owns its
/// runtime-discovered metadata — reconnect/reload can rebuild these without
/// leaking the previous generation's strings.
pub struct McpTool {
    link: McpLink,
    remote_name: String,
    exposed_name: String,
    description: String,
    input_schema: serde_json::Value,
}

/// The model-facing name for a remote tool: `mcp__<server>__<tool>`.
fn exposed_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

impl McpTool {
    fn new(client: Arc<McpClient>, server: &str, info: McpToolInfo) -> Self {
        Self::from_link(McpLink::Stdio(client), server, info)
    }

    fn from_link(link: McpLink, server: &str, info: McpToolInfo) -> Self {
        let exposed_name = exposed_tool_name(server, &info.name);
        let description = if info.description.is_empty() {
            format!("MCP tool `{}` from server `{server}`.", info.name)
        } else {
            // The server wrote this text. It is tool metadata, not an instruction.
            info.description
        };
        Self {
            link,
            remote_name: info.name,
            exposed_name,
            description,
            input_schema: info.input_schema,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.exposed_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    fn risk(&self) -> RiskLevel {
        // MCP servers can do anything; treat their tools as network-risk so a
        // sandboxed/plan mode gates them. Untrusted annotations do not lower this.
        RiskLevel::Network
    }

    fn catalog_changed(&self) -> bool {
        match &self.link {
            McpLink::Http(session) => session.catalog_changed(),
            // One-shot, like the HTTP flag: a failed reconnect must not make
            // every following turn try again.
            McpLink::Stdio(client) => client.closed.swap(false, Ordering::SeqCst),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if !context.policy.unrestricted_execution()
            && !matches!(
                context.policy.network_scope(),
                leveler_execution::NetworkScope::Internet
            )
        {
            return Ok(ToolOutput::error(
                "UnsupportedNetworkScope: MCP server processes require Internet scope or unrestricted call authority; destination confinement is unavailable.",
            ));
        }
        let unrestricted = context.policy.unrestricted_execution();
        let call = async {
            match &self.link {
                McpLink::Stdio(client) => {
                    // A frozen exact-call approval must not mutate the authority of the
                    // cached, process-lived server. A mismatch gets a call-owned server
                    // with the same configured program; dropping it ends that authority.
                    let client = if client.unrestricted == unrestricted {
                        client.clone()
                    } else {
                        McpClient::connect_with_authority(
                            &client.config,
                            &client.environment,
                            unrestricted,
                        )
                        .await?
                    };
                    client
                        .call_tool(&self.remote_name, input)
                        .await
                        .map_err(stdio_call_error)
                }
                McpLink::Http(session) => match session.call_tool(&self.remote_name, input).await {
                    mcp_http::CallOutcome::Done(text) => Ok(text),
                    mcp_http::CallOutcome::Failed(reason) => Err(format!("MCP 调用失败:{reason}")),
                    mcp_http::CallOutcome::Unknown(reason) => {
                        Err(format!("MCP 调用结果未知:{reason}"))
                    }
                },
            }
        };
        let out = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                if let McpLink::Stdio(client) = &self.link {
                    client.cancel_pending("user cancelled").await;
                }
                return Ok(ToolOutput::error(
                    "MCP 调用已取消。请求可能已送达服务端，结果未知，未自动重试。",
                ));
            }
            r = call => r,
        };
        match out {
            Ok(text) => Ok(ToolOutput::ok(text)),
            Err(reason) => Ok(ToolOutput::error(reason)),
        }
    }
}

/// A stdio call error is a confirmed failure only when the client knows the
/// request did not produce an outcome. A timeout/cancel is Unknown: the server
/// may have executed it, and the model must not retry a side effect blind.
fn stdio_call_error(reason: String) -> String {
    match reason.strip_prefix("UNKNOWN_OUTCOME:") {
        Some(unknown) => format!("MCP 调用结果未知:{unknown}"),
        None => format!("MCP 调用失败:{reason}"),
    }
}

/// Connect to each configured server and return its tools. Servers that fail to
/// start or list are skipped (logged), never aborting the caller.
pub async fn connect_all(configs: &[McpServerConfig]) -> Vec<Arc<dyn Tool>> {
    connect_all_with_authority(configs, leveler_core::environment(), false).await
}

/// Connect under application-owned startup authority, preserving credential
/// inheritance only for a FullAccess launch.
pub async fn connect_all_with_authority(
    configs: &[McpServerConfig],
    environment: &leveler_core::EnvSnapshot,
    unrestricted: bool,
) -> Vec<Arc<dyn Tool>> {
    connect_all_detailed(configs, environment, unrestricted)
        .await
        .into_iter()
        .flat_map(|connection| connection.tools)
        .collect()
}

/// One configured server's connection attempt.
///
/// `error` is `Some` when the server could not be reached or its catalog could
/// not be listed. A failed attempt says nothing durable about the server: the
/// caller stays free to retry, and needs this outcome to tell a server that is
/// merely not up yet from one that is already connected with zero tools.
pub struct McpServerConnection {
    pub server: String,
    pub tools: Vec<Arc<dyn Tool>>,
    pub error: Option<String>,
}

/// Connect to each configured server and report every attempt separately.
pub async fn connect_all_detailed(
    configs: &[McpServerConfig],
    environment: &leveler_core::EnvSnapshot,
    unrestricted: bool,
) -> Vec<McpServerConnection> {
    let mut connections: Vec<McpServerConnection> = Vec::new();
    for cfg in configs {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let error = if cfg.is_streamable_http() {
            match mcp_http::HttpSession::connect(cfg).await {
                Ok(session) => match session.list_tools().await {
                    Ok(infos) => {
                        let protocol = session.protocol().await;
                        let count = infos.len();
                        tracing::info!(
                            server = %cfg.name,
                            transport = "streamable_http",
                            endpoint = %cfg.endpoint_label(),
                            protocol = %protocol,
                            tools = count,
                            "MCP server connected"
                        );
                        for info in infos {
                            tools.push(Arc::new(McpTool::from_link(
                                McpLink::Http(Arc::clone(&session)),
                                &cfg.name,
                                info,
                            )));
                        }
                        None
                    }
                    Err(error) => {
                        tracing::warn!(
                            server = %cfg.name,
                            transport = "streamable_http",
                            endpoint = %cfg.endpoint_label(),
                            error = %error,
                            "MCP tools/list failed"
                        );
                        Some(format!("tools/list failed: {error}"))
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        server = %cfg.name,
                        transport = "streamable_http",
                        endpoint = %cfg.endpoint_label(),
                        error = %error,
                        "MCP server unavailable"
                    );
                    Some(error)
                }
            }
        } else {
            match McpClient::connect_with_authority(cfg, environment, unrestricted).await {
                Ok(client) => match client.list_tools().await {
                    Ok(infos) => {
                        tracing::info!(
                            server = %cfg.name,
                            transport = "stdio",
                            tools = infos.len(),
                            "MCP server connected"
                        );
                        for info in infos {
                            tools.push(Arc::new(McpTool::new(client.clone(), &cfg.name, info)));
                        }
                        None
                    }
                    Err(e) => {
                        tracing::warn!("MCP `{}` tools/list 失败:{e}", cfg.name);
                        Some(format!("tools/list failed: {e}"))
                    }
                },
                Err(e) => {
                    tracing::warn!("{e}");
                    Some(e)
                }
            }
        };
        connections.push(McpServerConnection {
            server: cfg.name.clone(),
            tools,
            error,
        });
    }
    connections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[cfg(unix)]
    async fn full_mcp_launch_preserves_inherited_credentials_scoped_launch_scrubs() {
        let dir = tempfile::tempdir().unwrap();
        let environment = leveler_core::EnvSnapshot::new(
            [("SERVICE_API_KEY".into(), "synthetic-test-key".into())],
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
        );
        for unrestricted in [false, true] {
            let observed = dir.path().join(format!("{unrestricted}.txt"));
            let script = r#"import json,os,sys
with open(sys.argv[1], 'w') as f: f.write(os.environ.get('SERVICE_API_KEY', 'absent'))
for line in sys.stdin:
    request = json.loads(line)
    if 'id' in request: print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':{}}), flush=True)
"#;
            let cfg = McpServerConfig::stdio(
                "authority-test",
                "/usr/bin/python3",
                vec![
                    "-u".into(),
                    "-c".into(),
                    script.into(),
                    observed.to_string_lossy().into_owned(),
                ],
                vec![],
            );
            let client = McpClient::connect_with_authority(&cfg, &environment, unrestricted)
                .await
                .unwrap();
            let value = std::fs::read_to_string(observed).unwrap();
            assert_eq!(
                value,
                if unrestricted {
                    "synthetic-test-key"
                } else {
                    "absent"
                }
            );
            drop(client);
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_stdio_server_that_exits_asks_for_a_catalog_refresh() {
        // A child that exits leaves tools whose process is gone. The next turn
        // must reconnect it instead of calling a dead pipe forever.
        let script = r#"import json,sys
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if method == 'tools/call':
        sys.exit(0)
    result = {'tools':[{'name':'echo','description':'echo','inputSchema':{'type':'object'}}]} if method == 'tools/list' else {}
    if 'id' in request:
        print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;
        let cfg = McpServerConfig::stdio(
            "exiting",
            "/usr/bin/python3",
            vec!["-u".into(), "-c".into(), script.into()],
            vec![],
        );
        let client = McpClient::connect_with_authority(&cfg, leveler_core::environment(), true)
            .await
            .unwrap();
        let tool = McpTool::new(
            client,
            "exiting",
            McpToolInfo {
                name: "echo".into(),
                description: "echo".into(),
                input_schema: serde_json::json!({}),
            },
        );
        assert!(!tool.catalog_changed(), "a live child owes no refresh");
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(
            leveler_execution::Workspace::new(dir.path()).unwrap(),
            leveler_execution::PermissionProfile::FullAccess,
        );
        let out = tool
            .execute(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        let mut refreshed = false;
        for _ in 0..100 {
            if tool.catalog_changed() {
                refreshed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            refreshed,
            "the dead child must be reconnected on a later turn"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn approved_mcp_call_inherits_credentials_without_elevating_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let environment = leveler_core::EnvSnapshot::new(
            [("SERVICE_API_KEY".into(), "synthetic-test-key".into())],
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
        );
        let script = r#"import json,os,sys
for line in sys.stdin:
    request=json.loads(line)
    if 'id' in request:
        result={'content':[{'type':'text','text':os.environ.get('SERVICE_API_KEY','absent')}]} if request['method']=='tools/call' else {}
        print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
"#;
        let cfg = McpServerConfig::stdio(
            "approved-authority",
            "/usr/bin/python3",
            vec!["-u".into(), "-c".into(), script.into()],
            vec![],
        );
        let client = McpClient::connect_with_authority(&cfg, &environment, false)
            .await
            .unwrap();
        let tool = McpTool::new(
            client,
            "test",
            McpToolInfo {
                name: "probe".into(),
                description: "probe".into(),
                input_schema: serde_json::json!({}),
            },
        );
        let ctx = ToolContext::new(
            leveler_execution::Workspace::new(dir.path()).unwrap(),
            leveler_execution::PermissionProfile::Assisted,
        );
        let approved =
            ctx.clone()
                .with_resolved_policy(leveler_execution::ResolvedExecutionPolicy::new(
                    leveler_execution::WriteScope::Unrestricted,
                    leveler_execution::NetworkScope::Internet,
                    leveler_execution::AuthorizationEvidence::ApprovedOnce,
                ));
        let output = tool
            .execute(serde_json::json!({}), approved, CancellationToken::new())
            .await
            .unwrap();
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content, "synthetic-test-key");
        let next = tool
            .execute(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(!next.is_error, "{}", next.content);
        assert_eq!(
            next.content, "absent",
            "approveonce must not mutate cached server authority"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn direct_mcp_call_refuses_loopback_scope_before_rpc() {
        let mut child = Command::new("/usr/bin/true")
            .stdin(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let client = Arc::new(McpClient {
            config: McpServerConfig::stdio("test", "/usr/bin/true", vec![], vec![]),
            environment: leveler_core::EnvSnapshot::default(),
            unrestricted: false,
            stdin: Mutex::new(child.stdin.take().unwrap()),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            _child: child,
        });
        let tool = McpTool::new(
            client.clone(),
            "test",
            McpToolInfo {
                name: "probe".into(),
                description: "probe".into(),
                input_schema: serde_json::json!({}),
            },
        );
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = tool
            .execute(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("UnsupportedNetworkScope"));
        assert_eq!(client.next_id.load(Ordering::SeqCst), 1, "no RPC was sent");
    }

    #[test]
    fn parses_tools_list() {
        let result = serde_json::json!({
            "tools": [
                { "name": "search", "description": "web search", "inputSchema": { "type": "object" } },
                { "name": "noschema" }
            ]
        });
        let tools = parse_tools(&result);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[0].description, "web search");
        assert_eq!(tools[1].name, "noschema");
        assert_eq!(
            tools[1].input_schema,
            serde_json::json!({ "type": "object" })
        );
    }

    #[test]
    fn formats_text_content() {
        let result = serde_json::json!({
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "text", "text": "world" },
                { "type": "image", "data": "..." }
            ]
        });
        assert_eq!(format_tool_result(&result), "hello\nworld\n[image content]");
    }

    #[test]
    fn exposed_name_is_prefixed() {
        // A tool from server "fs" named "read" is exposed as mcp__fs__read.
        assert_eq!(exposed_tool_name("fs", "read"), "mcp__fs__read");
    }
}

#[cfg(test)]
mod stdio_outcome_tests {
    use super::stdio_call_error;

    #[test]
    fn a_timeout_is_unknown_not_a_confirmed_failure() {
        let text = stdio_call_error(
            "UNKNOWN_OUTCOME:MCP 请求 `tools/call` 超时；请求可能已送达服务端".to_string(),
        );
        assert!(text.starts_with("MCP 调用结果未知:"), "{text}");
        assert!(!text.contains("调用失败"), "{text}");
    }

    #[test]
    fn an_explicit_error_stays_a_confirmed_failure() {
        let text = stdio_call_error("server rejected arguments".to_string());
        assert!(text.starts_with("MCP 调用失败:"), "{text}");
        assert!(!text.contains("未知"), "{text}");
    }
}
