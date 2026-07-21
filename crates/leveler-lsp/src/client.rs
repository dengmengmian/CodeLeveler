//! An async LSP client over a language server's stdio.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

use crate::codec::{FrameReader, encode};

/// LSP client errors.
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("failed to spawn language server `{0}`")]
    Spawn(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("request `{0}` timed out")]
    Timeout(String),
    #[error("server closed the connection")]
    Closed,
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// A resolved symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: i64,
    pub container: Option<String>,
}

/// A diagnostic (error/warning) reported by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: u64,
    pub severity: i64,
    pub message: String,
}

/// A document symbol with position info: enough to map a reference line to its
/// enclosing symbol, and to locate that symbol's name for a next-hop query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolSpan {
    pub name: String,
    pub kind: i64,
    /// 0-based line/character of the symbol's NAME (the `references` anchor).
    pub name_line: u64,
    pub name_character: u64,
    /// 0-based inclusive line range the symbol's body spans.
    pub start_line: u64,
    pub end_line: u64,
}

/// A symbol with its definition location (from `workspace/symbol`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolLocation {
    pub name: String,
    pub kind: i64,
    /// Filesystem path (the `file://` scheme stripped).
    pub path: String,
    /// 0-based line of the definition.
    pub line: u64,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;
type DiagStore = Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>;

/// A running language-server session.
pub struct LspClient {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: AtomicI64,
    pending: Pending,
    diagnostics: DiagStore,
    root: PathBuf,
}

impl LspClient {
    /// Launch `program args...` as a language server rooted at `root` and run
    /// the LSP initialize handshake.
    pub async fn start(program: &str, args: &[String], root: &Path) -> Result<Self, LspError> {
        Self::start_with_environment(program, args, root, leveler_core::environment()).await
    }

    pub async fn start_with_environment(
        program: &str,
        args: &[String],
        root: &Path,
        environment: &leveler_core::EnvSnapshot,
    ) -> Result<Self, LspError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        command.env_clear();
        for (name, value) in environment.vars_os() {
            if !name
                .to_str()
                .is_some_and(leveler_execution::is_credential_env_name)
            {
                command.env(name, value);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|_| LspError::Spawn(program.to_string()))?;

        let stdin = Arc::new(Mutex::new(child.stdin.take().ok_or(LspError::Closed)?));
        let stdout = child.stdout.take().ok_or(LspError::Closed)?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: DiagStore = Arc::new(Mutex::new(HashMap::new()));

        spawn_reader(stdout, stdin.clone(), pending.clone(), diagnostics.clone());

        let client = Self {
            child,
            stdin,
            next_id: AtomicI64::new(1),
            pending,
            diagnostics,
            root: root.to_path_buf(),
        };

        let init = json!({
            "processId": null,
            "rootUri": path_to_uri(root),
            "capabilities": { "textDocument": { "documentSymbol": {}, "references": {}, "publishDiagnostics": {} } },
            "workspaceFolders": null,
        });
        client.request("initialize", init).await?;
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    async fn write_frame(&self, json: &str) -> Result<(), LspError> {
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(&encode(json))
            .await
            .map_err(|e| LspError::Io(e.to_string()))?;
        stdin.flush().await.map_err(|e| LspError::Io(e.to_string()))
    }

    /// Send a request and await its result.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.write_frame(&msg.to_string()).await?;

        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(LspError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(LspError::Timeout(method.to_string()))
            }
        }
    }

    /// Send a notification (no response expected).
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write_frame(&msg.to_string()).await
    }

    /// Open a document so the server indexes it.
    pub async fn open(&self, path: &Path, language_id: &str) -> Result<(), LspError> {
        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| LspError::Io(e.to_string()))?;
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": path_to_uri(path), "languageId": language_id,
                "version": 1, "text": text,
            }}),
        )
        .await
    }

    /// List the symbols defined in a document.
    pub async fn document_symbols(&self, path: &Path) -> Result<Vec<SymbolInfo>, LspError> {
        let result = self
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": path_to_uri(path) } }),
            )
            .await?;
        Ok(parse_symbols(&result))
    }

    /// List document symbols with their line ranges and name positions, so a
    /// caller can map a reference line to its enclosing symbol and re-query.
    pub async fn document_symbol_spans(&self, path: &Path) -> Result<Vec<SymbolSpan>, LspError> {
        let result = self
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": path_to_uri(path) } }),
            )
            .await?;
        Ok(parse_symbol_spans(&result))
    }

    /// Query workspace symbols by name (`workspace/symbol`), returning each
    /// symbol's definition location.
    pub async fn workspace_symbols(&self, query: &str) -> Result<Vec<SymbolLocation>, LspError> {
        let result = self
            .request("workspace/symbol", json!({ "query": query }))
            .await?;
        Ok(parse_workspace_symbols(&result))
    }

    /// Find references to the symbol at `(line, character)` in `path`
    /// (`textDocument/references`, 0-based position).
    pub async fn references(
        &self,
        path: &Path,
        line: u64,
        character: u64,
        include_declaration: bool,
    ) -> Result<Vec<SymbolLocation>, LspError> {
        let result = self
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": path_to_uri(path) },
                    "position": { "line": line, "character": character },
                    "context": { "includeDeclaration": include_declaration },
                }),
            )
            .await?;
        Ok(parse_locations(&result))
    }

    /// Diagnostics published for a document so far (call after `open` + a beat).
    pub async fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        let uri = path_to_uri(path);
        self.diagnostics
            .lock()
            .await
            .get(&uri)
            .cloned()
            .unwrap_or_default()
    }

    /// Wait up to `timeout` for the server to publish diagnostics for `path`.
    pub async fn wait_for_diagnostics(&self, path: &Path, timeout: Duration) -> Vec<Diagnostic> {
        let uri = path_to_uri(path);
        let deadline = timeout;
        let step = Duration::from_millis(100);
        let mut waited = Duration::ZERO;
        loop {
            if let Some(d) = self.diagnostics.lock().await.get(&uri)
                && !d.is_empty()
            {
                return d.clone();
            }
            if waited >= deadline {
                return self
                    .diagnostics
                    .lock()
                    .await
                    .get(&uri)
                    .cloned()
                    .unwrap_or_default();
            }
            tokio::time::sleep(step).await;
            waited += step;
        }
    }

    /// The workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Politely shut the server down.
    pub async fn shutdown(mut self) {
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", Value::Null).await;
        let _ = self.child.start_kill();
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Background task: read frames, dispatch responses, collect diagnostics, and
/// answer server-initiated requests with a null result so it doesn't stall.
fn spawn_reader(
    mut stdout: tokio::process::ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    diagnostics: DiagStore,
) {
    tokio::spawn(async move {
        let mut reader = FrameReader::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            reader.feed(&buf[..n]);
            while let Some(body) = reader.next_message() {
                let Ok(msg) = serde_json::from_str::<Value>(&body) else {
                    continue;
                };
                let has_id = msg.get("id").is_some();
                let method = msg.get("method").and_then(|m| m.as_str());
                match (has_id, method) {
                    // Response to one of our requests.
                    (true, None) => {
                        if let Some(id) = msg.get("id").and_then(|i| i.as_i64())
                            && let Some(tx) = pending.lock().await.remove(&id)
                        {
                            let result = msg.get("result").cloned().unwrap_or(Value::Null);
                            let _ = tx.send(result);
                        }
                    }
                    // Request from the server — reply with null so it proceeds.
                    (true, Some(_)) => {
                        if let Some(id) = msg.get("id").cloned() {
                            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": null });
                            let mut si = stdin.lock().await;
                            let _ = si.write_all(&encode(&reply.to_string())).await;
                            let _ = si.flush().await;
                        }
                    }
                    // Notification.
                    (false, Some("textDocument/publishDiagnostics")) => {
                        if let Some(params) = msg.get("params") {
                            store_diagnostics(params, &diagnostics).await;
                        }
                    }
                    _ => {}
                }
            }
        }
    });
}

async fn store_diagnostics(params: &Value, store: &DiagStore) {
    let Some(uri) = params.get("uri").and_then(|u| u.as_str()) else {
        return;
    };
    let diags = params
        .get("diagnostics")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .map(|d| Diagnostic {
                    line: d
                        .pointer("/range/start/line")
                        .and_then(|l| l.as_u64())
                        .unwrap_or(0),
                    severity: d.get("severity").and_then(|s| s.as_i64()).unwrap_or(1),
                    message: d
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    store.lock().await.insert(uri.to_string(), diags);
}

/// Parse a documentSymbol result (either `DocumentSymbol[]` or
/// `SymbolInformation[]`), flattening nested symbols.
fn parse_symbols(result: &Value) -> Vec<SymbolInfo> {
    let mut out = Vec::new();
    if let Some(arr) = result.as_array() {
        for item in arr {
            collect_symbol(item, None, &mut out);
        }
    }
    out
}

fn collect_symbol(item: &Value, container: Option<&str>, out: &mut Vec<SymbolInfo>) {
    let Some(name) = item.get("name").and_then(|n| n.as_str()) else {
        return;
    };
    let kind = item.get("kind").and_then(|k| k.as_i64()).unwrap_or(0);
    // SymbolInformation carries `containerName`; DocumentSymbol carries `children`.
    let container = item
        .get("containerName")
        .and_then(|c| c.as_str())
        .or(container);
    out.push(SymbolInfo {
        name: name.to_string(),
        kind,
        container: container.map(String::from),
    });
    if let Some(children) = item.get("children").and_then(|c| c.as_array()) {
        for child in children {
            collect_symbol(child, Some(name), out);
        }
    }
}

/// Parse a documentSymbol result (`DocumentSymbol[]` or `SymbolInformation[]`)
/// into positioned spans, flattening nested symbols.
fn parse_symbol_spans(result: &Value) -> Vec<SymbolSpan> {
    let mut out = Vec::new();
    if let Some(arr) = result.as_array() {
        for item in arr {
            collect_symbol_span(item, &mut out);
        }
    }
    out
}

fn collect_symbol_span(item: &Value, out: &mut Vec<SymbolSpan>) {
    let Some(name) = item.get("name").and_then(|n| n.as_str()) else {
        return;
    };
    let kind = item.get("kind").and_then(|k| k.as_i64()).unwrap_or(0);
    // DocumentSymbol carries `range` (body) + `selectionRange` (name);
    // SymbolInformation carries `location.range` for both.
    let body = item
        .get("range")
        .or_else(|| item.get("location").and_then(|l| l.get("range")));
    let name_range = item.get("selectionRange").or(body);
    let (start_line, end_line) = line_span(body);
    let (name_line, name_character) = position(name_range);
    out.push(SymbolSpan {
        name: name.to_string(),
        kind,
        name_line,
        name_character,
        start_line,
        end_line,
    });
    if let Some(children) = item.get("children").and_then(|c| c.as_array()) {
        for child in children {
            collect_symbol_span(child, out);
        }
    }
}

fn line_span(range: Option<&Value>) -> (u64, u64) {
    let line_at = |key: &str| {
        range
            .and_then(|r| r.get(key))
            .and_then(|p| p.get("line"))
            .and_then(|l| l.as_u64())
    };
    let start = line_at("start").unwrap_or(0);
    (start, line_at("end").unwrap_or(start))
}

fn position(range: Option<&Value>) -> (u64, u64) {
    let start = range.and_then(|r| r.get("start"));
    let line = start.and_then(|s| s.get("line")).and_then(|l| l.as_u64()).unwrap_or(0);
    let ch = start
        .and_then(|s| s.get("character"))
        .and_then(|c| c.as_u64())
        .unwrap_or(0);
    (line, ch)
}

/// Parse a `workspace/symbol` result (`SymbolInformation[]` or
/// `WorkspaceSymbol[]`) into located symbols.
fn parse_workspace_symbols(result: &Value) -> Vec<SymbolLocation> {
    let mut out = Vec::new();
    let Some(arr) = result.as_array() else {
        return out;
    };
    for item in arr {
        let Some(name) = item.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let kind = item.get("kind").and_then(|k| k.as_i64()).unwrap_or(0);
        let uri = item
            .pointer("/location/uri")
            .and_then(|u| u.as_str())
            .unwrap_or("");
        let line = item
            .pointer("/location/range/start/line")
            .and_then(|l| l.as_u64())
            .unwrap_or(0);
        out.push(SymbolLocation {
            name: name.to_string(),
            kind,
            path: uri_to_path(uri),
            line,
        });
    }
    out
}

/// Parse a `Location[]` result (`textDocument/references`).
fn parse_locations(result: &Value) -> Vec<SymbolLocation> {
    let Some(arr) = result.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let uri = item.pointer("/uri").and_then(|u| u.as_str())?;
            let line = item
                .pointer("/range/start/line")
                .and_then(|l| l.as_u64())
                .unwrap_or(0);
            Some(SymbolLocation {
                name: String::new(),
                kind: 0,
                path: uri_to_path(uri),
                line,
            })
        })
        .collect()
}

fn uri_to_path(uri: &str) -> String {
    uri.strip_prefix("file://").unwrap_or(uri).to_string()
}

fn path_to_uri(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    format!("file://{}", abs.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_symbol_information() {
        let result = json!([
            {"name": "Foo", "kind": 5, "location": {}, "containerName": "mod"},
            {"name": "bar", "kind": 12}
        ]);
        let syms = parse_symbols(&result);
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "Foo");
        assert_eq!(syms[0].container.as_deref(), Some("mod"));
    }

    #[test]
    fn parses_workspace_symbol_locations() {
        let result = json!([
            {"name": "cancel_order", "kind": 12,
             "location": {"uri": "file:///repo/src/order.rs",
                          "range": {"start": {"line": 41, "character": 0}}}}
        ]);
        let locs = parse_workspace_symbols(&result);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].name, "cancel_order");
        assert_eq!(locs[0].path, "/repo/src/order.rs");
        assert_eq!(locs[0].line, 41);
    }

    #[test]
    fn parses_symbol_spans_with_ranges_and_name_positions() {
        let result = json!([
            {"name": "Service", "kind": 5,
             "range": {"start": {"line": 10, "character": 0}, "end": {"line": 40, "character": 1}},
             "selectionRange": {"start": {"line": 10, "character": 7}, "end": {"line": 10, "character": 14}},
             "children": [
                {"name": "cancel", "kind": 6,
                 "range": {"start": {"line": 20, "character": 4}, "end": {"line": 25, "character": 5}},
                 "selectionRange": {"start": {"line": 20, "character": 8}, "end": {"line": 20, "character": 14}}}
             ]}
        ]);
        let spans = parse_symbol_spans(&result);
        assert_eq!(spans.len(), 2);
        let service = spans.iter().find(|s| s.name == "Service").unwrap();
        assert_eq!((service.start_line, service.end_line), (10, 40));
        assert_eq!((service.name_line, service.name_character), (10, 7));
        let cancel = spans.iter().find(|s| s.name == "cancel").unwrap();
        assert_eq!((cancel.start_line, cancel.end_line), (20, 25));
        assert_eq!(cancel.name_line, 20);
    }

    #[test]
    fn symbol_span_falls_back_to_location_range_for_symbolinformation() {
        let result = json!([
            {"name": "bar", "kind": 12,
             "location": {"uri": "file:///x.rs",
                          "range": {"start": {"line": 5, "character": 3},
                                    "end": {"line": 8, "character": 0}}}}
        ]);
        let spans = parse_symbol_spans(&result);
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start_line, spans[0].end_line), (5, 8));
        assert_eq!((spans[0].name_line, spans[0].name_character), (5, 3));
    }

    #[test]
    fn parses_hierarchical_document_symbols() {
        let result = json!([
            {"name": "Service", "kind": 5, "children": [
                {"name": "cancel", "kind": 6}
            ]}
        ]);
        let syms = parse_symbols(&result);
        assert_eq!(syms.len(), 2);
        assert!(
            syms.iter()
                .any(|s| s.name == "cancel" && s.container.as_deref() == Some("Service"))
        );
    }
}
