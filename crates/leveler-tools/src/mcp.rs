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
    config: McpServerConfig,
    environment: leveler_core::EnvSnapshot,
    unrestricted: bool,
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
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
            config: cfg.clone(),
            environment: environment.clone(),
            unrestricted,
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

/// A model-facing tool that proxies to an MCP server. Owns its
/// runtime-discovered metadata — reconnect/reload can rebuild these without
/// leaking the previous generation's strings.
pub struct McpTool {
    client: Arc<McpClient>,
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
        let exposed_name = exposed_tool_name(server, &info.name);
        let description = if info.description.is_empty() {
            format!("MCP tool `{}` from server `{server}`.", info.name)
        } else {
            info.description
        };
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
        // sandboxed/plan mode gates them.
        RiskLevel::Network
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
            // A frozen exact-call approval must not mutate the authority of the
            // cached, process-lived server. A mismatch gets a call-owned server
            // with the same configured program; dropping it ends that authority.
            let client = if self.client.unrestricted == unrestricted {
                self.client.clone()
            } else {
                McpClient::connect_with_authority(
                    &self.client.config,
                    &self.client.environment,
                    unrestricted,
                )
                .await?
            };
            client.call_tool(&self.remote_name, input).await
        };
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
    connect_all_with_authority(configs, leveler_core::environment(), false).await
}

/// Connect under application-owned startup authority, preserving credential
/// inheritance only for a FullAccess launch.
pub async fn connect_all_with_authority(
    configs: &[McpServerConfig],
    environment: &leveler_core::EnvSnapshot,
    unrestricted: bool,
) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for cfg in configs {
        match McpClient::connect_with_authority(cfg, environment, unrestricted).await {
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
            let cfg = McpServerConfig {
                name: "authority-test".into(),
                command: "/usr/bin/python3".into(),
                args: vec![
                    "-u".into(),
                    "-c".into(),
                    script.into(),
                    observed.to_string_lossy().into_owned(),
                ],
                env: vec![],
            };
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
        let cfg = McpServerConfig {
            name: "approved-authority".into(),
            command: "/usr/bin/python3".into(),
            args: vec!["-u".into(), "-c".into(), script.into()],
            env: vec![],
        };
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
            config: McpServerConfig {
                name: "test".into(),
                command: "/usr/bin/true".into(),
                args: vec![],
                env: vec![],
            },
            environment: leveler_core::EnvSnapshot::default(),
            unrestricted: false,
            stdin: Mutex::new(child.stdin.take().unwrap()),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
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
