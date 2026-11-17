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

/// Values this server already holds, plus shapes that are credentials in any
/// untrusted text. The server wrote the error; it is not trusted to have kept
/// the values it received out of it.
#[derive(Debug, Clone, Default)]
pub(super) struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    fn for_config(cfg: &McpServerConfig) -> Self {
        let mut secrets: Vec<String> = Vec::new();
        for (_, value) in &cfg.env {
            secrets.push(value.clone());
        }
        for (_, value) in &cfg.headers {
            secrets.push(value.clone());
        }
        if let Some(token) = &cfg.bearer_token {
            secrets.push(token.clone());
        }
        // A short value is more likely an ordinary word than a credential, and
        // replacing it everywhere would destroy the diagnostic. Four is the
        // same floor the HTTP client has always used.
        secrets.retain(|value| value.len() >= 4);
        // Longest first: a secret that contains another must be removed whole.
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.dedup();
        Self { secrets }
    }

    fn redact(&self, text: &str) -> String {
        redact_sensitive_text(text, &self.secrets)
    }

    /// Only the values this server was given. Used on a successful result:
    /// the caller asked for that content, so a generated-looking token in it
    /// must not be hidden by the generic error heuristics.
    fn redact_known(&self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in &self.secrets {
            if out.contains(secret) {
                out = out.replace(secret, "[redacted]");
            }
        }
        out
    }

    /// Metadata is another channel the server controls. A description or a
    /// schema default that echoes the value the server was given is the same
    /// leak as an error message.
    fn redact_json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.redact_known(text),
            serde_json::Value::Array(items) => {
                for item in items {
                    self.redact_json(item);
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values_mut() {
                    self.redact_json(item);
                }
            }
            _ => {}
        }
    }
}

/// Redact credential material from untrusted text while keeping the stable
/// facts a reader needs: error category, server id, transport, field names.
///
/// Three layers, strongest first:
/// 1. every value this server was actually given (env, headers, bearer token);
/// 2. assignment shapes (`token=…`, `Authorization: …`, `"api_key": "…"`) and
///    bearer tokens, whatever their value;
/// 3. recognizable token prefixes (`ghp_`, `sk-`, `AKIA…`) and credentials in
///    a URL's userinfo or query.
///
/// This cannot recognize an arbitrary secret the server invented and pasted
/// with no key, prefix or URL around it. Callers must not claim it does.
pub fn redact_sensitive_text(text: &str, secrets: &[String]) -> String {
    // URL first: a query value is removed with its URL, before an assignment
    // rule could put a marker inside the query and leave punctuation behind.
    let out = redact_url_credentials(text);
    let mut out = out;
    let mut known: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|value| value.len() >= 4)
        .collect();
    known.sort_by_key(|value| std::cmp::Reverse(value.len()));
    known.dedup();
    for secret in known {
        if out.contains(secret) {
            out = out.replace(secret, "[redacted]");
        }
    }
    let out = redact_assignments(&out);
    let out = redact_bearer_tokens(&out);
    redact_token_prefixes(&out)
}

const SENSITIVE_KEYS: &[&str] = &[
    "proxy-authorization",
    "authorization",
    "x-api-key",
    "client_secret",
    "access_token",
    "refresh_token",
    "session_token",
    "api-key",
    "apikey",
    "api_key",
    "api_secret",
    "secret_key",
    "private_key",
    "credential",
    "password",
    "passwd",
    "bearer",
    "secret",
    "token",
];

fn redact_assignments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < text.len() {
        if !text.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let Some((key_start, key_len, header)) = match_sensitive_key(text, i) else {
            i += 1;
            continue;
        };
        let mut j = key_start + key_len;
        // JSON spells `"token": "…"`.
        if text[j..].starts_with('"') || text[j..].starts_with('\'') {
            j += 1;
        }
        while j < text.len() && text[j..].starts_with([' ', '\t']) {
            j += 1;
        }
        if !(text[j..].starts_with(':') || text[j..].starts_with('=')) {
            i = key_start + key_len;
            continue;
        }
        j += 1;
        while j < text.len() && text[j..].starts_with([' ', '\t']) {
            j += 1;
        }
        let (value_start, value_end) = value_span(text, j, header);
        if value_end <= value_start {
            i = j;
            continue;
        }
        // A previous layer already replaced this value.
        if text[value_start..].starts_with("[redacted") {
            i = value_end;
            continue;
        }
        out.push_str(&text[copied..value_start]);
        out.push_str("[redacted]");
        copied = value_end;
        i = value_end.max(j);
    }
    out.push_str(&text[copied..]);
    out
}

/// Find one sensitive key starting at `at`, case-insensitively, with a real
/// word boundary on both sides.
fn match_sensitive_key(text: &str, at: usize) -> Option<(usize, usize, bool)> {
    let before = text[..at].chars().next_back();
    if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    for key in SENSITIVE_KEYS {
        if text
            .get(at..at + key.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(key))
            && text
                .get(at + key.len()..)
                .and_then(|rest| rest.chars().next())
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        {
            let header = matches!(*key, "authorization" | "proxy-authorization");
            return Some((at, key.len(), header));
        }
    }
    None
}

/// Where a value starts and ends. Quoted values keep their quotes; a header
/// value runs to the end of its line because `Bearer <token>` has a space.
fn value_span(text: &str, at: usize, header: bool) -> (usize, usize) {
    let Some(first) = text.get(at..).and_then(|rest| rest.chars().next()) else {
        return (at, at);
    };
    if first == '"' || first == '\'' {
        let start = at + first.len_utf8();
        let mut end = start;
        while end < text.len() {
            if !text.is_char_boundary(end) {
                end += 1;
                continue;
            }
            let Some(c) = text[end..].chars().next() else {
                break;
            };
            if c == first {
                return (start, end);
            }
            end += c.len_utf8();
        }
        return (start, end);
    }
    let mut end = at;
    while end < text.len() {
        if !text.is_char_boundary(end) {
            end += 1;
            continue;
        }
        let Some(c) = text[end..].chars().next() else {
            break;
        };
        let stops = if header {
            matches!(c, '\n' | '\r' | ',' | ';' | ')' | ']' | '}' | '"' | '\'')
        } else {
            c.is_whitespace()
                || matches!(
                    c,
                    ',' | ';' | ')' | ']' | '}' | '&' | '"' | '\'' | '\n' | '\r'
                )
        };
        if end > at && stops {
            break;
        }
        end += c.len_utf8();
    }
    (at, end)
}

fn redact_bearer_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < text.len() {
        if !text.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let before = text[..i].chars().next_back();
        let boundary = before.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        if !boundary
            || text
                .get(i..i + 6)
                .is_none_or(|word| !word.eq_ignore_ascii_case("bearer"))
            || text
                .get(i + 6..)
                .and_then(|rest| rest.chars().next())
                .is_none_or(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            i += 1;
            continue;
        }
        let mut start = i + 6;
        while start < text.len() && text[start..].starts_with([' ', '\t']) {
            start += 1;
        }
        let mut end = start;
        while end < text.len() {
            let c = text[end..].chars().next().unwrap_or(' ');
            if !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '/' | '='))
            {
                break;
            }
            end += c.len_utf8();
        }
        if end >= start + 8 {
            out.push_str(&text[copied..start]);
            out.push_str("[redacted]");
            copied = end;
        }
        i = end.max(i + 6);
    }
    out.push_str(&text[copied..]);
    out
}

const TOKEN_PREFIXES: &[&str] = &[
    "github_pat_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "sk-",
    "sk_",
    "AKIA",
    "eyJ",
];

fn redact_token_prefixes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < text.len() {
        if !text.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let before = text[..i].chars().next_back();
        let boundary = before.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        if !boundary {
            i += 1;
            continue;
        }
        let Some(prefix) = TOKEN_PREFIXES.iter().find(|prefix| {
            text.get(i..i + prefix.len())
                .is_some_and(|candidate| candidate.starts_with(*prefix))
        }) else {
            i += 1;
            continue;
        };
        let mut end = i + prefix.len();
        while end < text.len() {
            let c = text[end..].chars().next().unwrap_or(' ');
            if !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '/')) {
                break;
            }
            end += c.len_utf8();
        }
        // A bare prefix (`sk-` in prose) is not a credential.
        if end >= i + prefix.len() + 4 {
            out.push_str(&text[copied..i]);
            out.push_str("[redacted]");
            copied = end;
        }
        i = end.max(i + prefix.len());
    }
    out.push_str(&text[copied..]);
    out
}

/// Keep the scheme, host and path; drop userinfo, query and fragment. The URL
/// itself stays readable so the diagnostic still names the destination.
fn redact_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut search = 0;
    while let Some(relative) = text[search..].find("://") {
        let scheme_end = search + relative;
        let authority_start = scheme_end + 3;
        let mut url_end = authority_start;
        while url_end < text.len() {
            let c = text[url_end..].chars().next().unwrap_or(' ');
            if c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '}' | ',' | ';') {
                break;
            }
            url_end += c.len_utf8();
        }
        let authority_end = text[authority_start..url_end]
            .find(['/', '?', '#'])
            .map(|index| authority_start + index)
            .unwrap_or(url_end);
        let authority = &text[authority_start..authority_end];
        let (userinfo, _host) = match authority.rsplit_once('@') {
            Some((userinfo, host)) => (Some(userinfo), host),
            None => (None, authority),
        };
        let path_start = authority_end;
        let sensitive_start = text[path_start..url_end]
            .find(['?', '#'])
            .map(|index| path_start + index);
        let (host_start, host_end) = match userinfo {
            Some(userinfo) => (authority_start + userinfo.len() + 1, authority_end),
            None => (authority_start, authority_end),
        };
        // Copy up to the host (skipping any userinfo), then the host/path up
        // to the query, then continue after the URL.
        out.push_str(&text[copied..authority_start]);
        out.push_str(&text[host_start..host_end]);
        let keep_end = sensitive_start.unwrap_or(url_end);
        out.push_str(&text[authority_end..keep_end]);
        copied = url_end;
        search = url_end;
    }
    out.push_str(&text[copied..]);
    out
}

/// A live connection to one MCP server.
pub struct McpClient {
    config: McpServerConfig,
    /// The values this server was given. Every error it writes passes through
    /// this before it can reach a client, the doctor or a log.
    redactor: Redactor,
    environment: leveler_core::EnvSnapshot,
    unrestricted: bool,
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    /// Set when the server's stdout closed: every discovered tool now points at
    /// a dead pipe and the next turn must reconnect this server.
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// The protocol version the server negotiated during `initialize`. Starts
    /// as the version this client requested so a server that omits the field
    /// still has a truthful fallback.
    protocol: std::sync::Mutex<String>,
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
        let redactor = Redactor::for_config(cfg);
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
            .map_err(|e| redactor.redact(&format!("启动 MCP 服务 `{}` 失败:{e}", cfg.name)))?;
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
            redactor,
            environment: environment.clone(),
            unrestricted,
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            closed,
            protocol: std::sync::Mutex::new(PROTOCOL_VERSION.to_string()),
            _child: child,
        });

        let initialized = client
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "leveler", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        if let Some(negotiated) = initialized
            .get("protocolVersion")
            .and_then(|value| value.as_str())
        {
            *client
                .protocol
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = negotiated.to_string();
        }
        client
            .notify("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(client)
    }

    /// The protocol version negotiated with this server.
    pub fn protocol(&self) -> String {
        self.protocol
            .lock()
            .map(|value| value.clone())
            .unwrap_or_else(|_| PROTOCOL_VERSION.to_string())
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
            // The server owns this text. Strip anything it echoed back before
            // the error can become a state fact, a page row or a log line.
            Ok(Ok(r)) => r.map_err(|reason| self.redactor.redact(&reason)),
            Ok(Err(_)) => Err(
                "UNKNOWN_OUTCOME:MCP 响应通道在请求发出后关闭；服务端可能已执行，\
                 结果未知，未自动重试"
                    .to_string(),
            ),
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
            for mut info in parse_tools(&result) {
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
                info.description = self.redactor.redact_known(&info.description);
                self.redactor.redact_json(&mut info.input_schema);
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
            Err(self.redactor.redact(&content))
        } else {
            // The server can echo the environment in a successful result too.
            Ok(self.redactor.redact_known(&content))
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
        // An MCP server is a separate process with no OS confinement, so only
        // the unfettered Internet scope is honest. A frozen `None` (including
        // an explicit `--deny-network`) is refused; exact-call consent does
        // not clear the run owner's denial.
        if !matches!(
            context.policy.network_scope(),
            leveler_execution::NetworkScope::Internet
        ) {
            return Ok(ToolOutput::error(
                "UnsupportedNetworkScope: MCP server processes require Internet scope; destination confinement is unavailable.",
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
        None if reason.contains("连接已关闭") => {
            format!("MCP 调用结果未知:{reason}；请求可能在服务端执行，未自动重试")
        }
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
    /// The negotiated protocol version when the handshake completed.
    pub protocol: Option<String>,
}

/// Connect to each configured server and report every attempt separately.
pub async fn connect_all_detailed(
    configs: &[McpServerConfig],
    environment: &leveler_core::EnvSnapshot,
    unrestricted: bool,
) -> Vec<McpServerConnection> {
    let mut connections: Vec<McpServerConnection> = Vec::new();
    for cfg in configs {
        let redactor = Redactor::for_config(cfg);
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let (error, protocol) = if cfg.is_streamable_http() {
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
                        (None, Some(protocol))
                    }
                    Err(error) => {
                        tracing::warn!(
                            server = %cfg.name,
                            transport = "streamable_http",
                            endpoint = %cfg.endpoint_label(),
                            error = %error,
                            "MCP tools/list failed"
                        );
                        (Some(format!("tools/list failed: {error}")), None)
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
                    (Some(error), None)
                }
            }
        } else {
            match McpClient::connect_with_authority(cfg, environment, unrestricted).await {
                Ok(client) => match client.list_tools().await {
                    Ok(infos) => {
                        let protocol = client.protocol();
                        tracing::info!(
                            server = %cfg.name,
                            transport = "stdio",
                            protocol = %protocol,
                            tools = infos.len(),
                            "MCP server connected"
                        );
                        for info in infos {
                            tools.push(Arc::new(McpTool::new(client.clone(), &cfg.name, info)));
                        }
                        (None, Some(protocol))
                    }
                    Err(e) => {
                        tracing::warn!("MCP `{}` tools/list 失败:{e}", cfg.name);
                        (Some(format!("tools/list failed: {e}")), None)
                    }
                },
                Err(e) => {
                    tracing::warn!("{e}");
                    (Some(e), None)
                }
            }
        };
        connections.push(McpServerConnection {
            server: cfg.name.clone(),
            tools,
            // Second boundary: every error this function returns passes the
            // same redactor, even one produced before a client existed.
            error: error.map(|error| redactor.redact(&error)),
            protocol,
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
            redactor: Redactor::default(),
            environment: leveler_core::EnvSnapshot::default(),
            unrestricted: false,
            stdin: Mutex::new(child.stdin.take().unwrap()),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            protocol: std::sync::Mutex::new(PROTOCOL_VERSION.to_string()),
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
mod redaction_tests {
    use super::redact_sensitive_text;

    #[test]
    fn a_url_keeps_its_destination_but_loses_userinfo_and_query() {
        let redacted = redact_sensitive_text(
            "connect to https://user:secret@example.com/mcp?token=abc failed",
            &[],
        );
        assert_eq!(redacted, "connect to https://example.com/mcp failed");
    }

    #[test]
    fn a_short_configured_value_is_not_replaced_everywhere() {
        let redacted = redact_sensitive_text("server abc is unavailable", &["abc".into()]);
        assert_eq!(redacted, "server abc is unavailable");
    }

    #[test]
    fn a_configured_value_is_removed_whole_and_keeps_the_category() {
        let redacted = redact_sensitive_text(
            "MCP tools/list failed for `github`: upstream said token=e2e-secret-value",
            &["e2e-secret-value".into()],
        );
        assert_eq!(
            redacted,
            "MCP tools/list failed for `github`: upstream said token=[redacted]"
        );
    }

    #[test]
    fn multibyte_text_is_scanned_without_splitting_a_character() {
        let redacted = redact_sensitive_text("MCP 连接已关闭 token=e2e-secret-value", &[]);
        assert!(redacted.starts_with("MCP 连接已关闭"), "{redacted}");
        assert!(redacted.contains("token=[redacted]"), "{redacted}");
    }

    #[test]
    fn a_successful_result_hides_only_the_configured_value() {
        let redactor = super::Redactor {
            secrets: vec!["configured-value".into()],
        };
        assert_eq!(
            redactor.redact_known("configured-value and sk-generated-looking"),
            "[redacted] and sk-generated-looking"
        );
    }

    #[test]
    fn common_credential_shapes_are_removed_without_a_configured_value() {
        let redacted = redact_sensitive_text(
            "Authorization: Bearer abcdef123456789\napi_key=plain-value-9876\nghp_abcdefghijklmnop",
            &[],
        );
        assert!(!redacted.contains("abcdef123456789"), "{redacted}");
        assert!(!redacted.contains("plain-value-9876"), "{redacted}");
        assert!(!redacted.contains("ghp_abcdefghijklmnop"), "{redacted}");
        assert!(redacted.contains("Authorization:"), "{redacted}");
        assert!(redacted.contains("api_key="), "{redacted}");
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
