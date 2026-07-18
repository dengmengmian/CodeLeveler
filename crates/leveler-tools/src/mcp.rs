//! Minimal MCP (Model Context Protocol) stdio client.
//!
//! Spawns a server process, completes the JSON-RPC `initialize` handshake, lists
//! its tools, and proxies tool calls. Each discovered tool is exposed to the
//! model as a [`Tool`] named `mcp__<server>__<tool>` so it can't collide with a
//! built-in. A failed server (won't start, times out) is skipped with a log —
//! it never aborts startup.

use std::collections::HashMap;
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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Configuration for one MCP server (stdio transport).
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
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
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    _child: Child,
}

impl McpClient {
    /// Spawn the server and complete the `initialize` handshake.
    pub async fn connect(cfg: &McpServerConfig) -> Result<Arc<Self>, String> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        cmd.env_clear();
        for (name, value) in leveler_core::environment().vars_os() {
            if !name
                .to_str()
                .is_some_and(leveler_execution::is_credential_env_name)
            {
                cmd.env(name, value);
            }
        }
        // Explicit MCP configuration is the only way to grant a credential to
        // the server; apply it after removing inherited secrets.
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("启动 MCP 服务 `{}` 失败:{e}", cfg.name))?;
        let stdin = child.stdin.take().ok_or("MCP: 无 stdin")?;
        let stdout = child.stdout.take().ok_or("MCP: 无 stdout")?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        // Reader task: route each JSON-RPC response line to its waiting request;
        // fail everything pending when the stream closes.
        {
            let pending = pending.clone();
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
            });
        }

        let client = Arc::new(Self {
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
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
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("MCP 响应通道关闭".to_string()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("MCP 请求 `{method}` 超时"))
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

    /// List the server's tools.
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, String> {
        let result = self.request("tools/list", serde_json::json!({})).await?;
        Ok(parse_tools(&result))
    }

    /// Call a tool by its remote name, returning its text content.
    pub async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<String, String> {
        let result = self
            .request(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": args }),
            )
            .await?;
        Ok(format_tool_result(&result))
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
    out.trim_end().to_string()
}

/// Leak a runtime string to `&'static str` (tools live for the whole process).
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// A model-facing tool that proxies to an MCP server.
pub struct McpTool {
    client: Arc<McpClient>,
    remote_name: String,
    exposed_name: &'static str,
    description: &'static str,
    input_schema: serde_json::Value,
}

impl McpTool {
    fn new(client: Arc<McpClient>, server: &str, info: McpToolInfo) -> Self {
        let exposed_name = leak(format!("mcp__{server}__{}", info.name));
        let description = leak(if info.description.is_empty() {
            format!("MCP tool `{}` from server `{server}`.", info.name)
        } else {
            info.description
        });
        Self {
            client,
            remote_name: info.name,
            exposed_name,
            description,
            input_schema: info.input_schema,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.exposed_name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    fn risk(&self) -> RiskLevel {
        // MCP servers can do anything; treat their tools as network-risk so a
        // sandboxed/plan mode gates them.
        RiskLevel::Network
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let call = self.client.call_tool(&self.remote_name, input);
        let out = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(ToolOutput::error("MCP 调用已取消。")),
            r = call => r,
        };
        match out {
            Ok(text) => Ok(ToolOutput::ok(text)),
            Err(reason) => Ok(ToolOutput::error(format!("MCP 调用失败:{reason}"))),
        }
    }
}

/// Connect to each configured server and return its tools. Servers that fail to
/// start or list are skipped (logged), never aborting the caller.
pub async fn connect_all(configs: &[McpServerConfig]) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for cfg in configs {
        match McpClient::connect(cfg).await {
            Ok(client) => match client.list_tools().await {
                Ok(infos) => {
                    for info in infos {
                        tools.push(Arc::new(McpTool::new(client.clone(), &cfg.name, info)));
                    }
                }
                Err(e) => tracing::warn!("MCP `{}` tools/list 失败:{e}", cfg.name),
            },
            Err(e) => tracing::warn!("{e}"),
        }
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(leak("mcp__fs__read".to_string()), "mcp__fs__read");
    }
}
