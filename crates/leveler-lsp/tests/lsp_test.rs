//! End-to-end LSP client test against a mock language server (Python), so it
//! runs without a real rust-analyzer/gopls installed.

use std::time::Duration;

use leveler_lsp::LspClient;

static PROCESS_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const MOCK_SERVER: &str = r#"
import os, sys, json

capture = os.environ.get("LVTEST_LSP_CAPTURE")
if capture:
    with open(capture, "w") as f:
        f.write(os.environ.get("LVTEST_LSP_API_KEY", ""))

def read_msg():
    headers = b""
    while b"\r\n\r\n" not in headers:
        b = sys.stdin.buffer.read(1)
        if not b:
            return None
        headers += b
    length = 0
    for line in headers.decode().split("\r\n"):
        if line.lower().startswith("content-length:"):
            length = int(line.split(":")[1].strip())
    body = sys.stdin.buffer.read(length)
    return json.loads(body.decode())

def send(obj):
    data = json.dumps(obj).encode()
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(data)).encode())
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()

while True:
    msg = read_msg()
    if msg is None:
        break
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {}}})
    elif method == "textDocument/didOpen":
        uri = msg["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
              "params": {"uri": uri, "diagnostics": [
                  {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 1}},
                   "severity": 1, "message": "mock diagnostic"}]}})
    elif method == "textDocument/documentSymbol":
        send({"jsonrpc": "2.0", "id": mid, "result": [
            {"name": "greet", "kind": 12},
            {"name": "Service", "kind": 5, "children": [{"name": "run", "kind": 6}]}]})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": mid, "result": None})
    elif method == "exit":
        break
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": None})
"#;

fn python() -> Option<String> {
    for p in ["python3", "python"] {
        if let Ok(path) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path) {
                if dir.join(p).is_file() {
                    return Some(p.to_string());
                }
            }
        }
    }
    None
}

#[tokio::test]
async fn lsp_client_handshake_symbols_and_diagnostics() {
    let _env_guard = PROCESS_ENV_LOCK.lock().await;
    let Some(py) = python() else {
        eprintln!("skipping: no python interpreter");
        return;
    };

    let dir = std::env::temp_dir().join(format!("leveler-lsp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("mock_lsp.py");
    std::fs::write(&script, MOCK_SERVER).unwrap();
    let source = dir.join("code.txt");
    std::fs::write(&source, "fn greet() {}\nstruct Service;\nbad line\n").unwrap();

    let environment = leveler_core::EnvSnapshot::new(
        std::env::vars_os(),
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    );
    let client = LspClient::start_with_environment(
        &py,
        &[script.to_string_lossy().into_owned()],
        &dir,
        &environment,
    )
    .await
    .expect("start LSP");

    client.open(&source, "text").await.expect("didOpen");

    // Symbols (flat + hierarchical, flattened).
    let symbols = client.document_symbols(&source).await.expect("symbols");
    assert!(symbols.iter().any(|s| s.name == "greet"));
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "run" && s.container.as_deref() == Some("Service"))
    );

    // Diagnostics pushed by the server on open.
    let diags = client
        .wait_for_diagnostics(&source, Duration::from_secs(3))
        .await;
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].message, "mock diagnostic");
    assert_eq!(diags[0].line, 2);

    client.shutdown().await;
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn lsp_process_does_not_inherit_credential_like_environment() {
    let _env_guard = PROCESS_ENV_LOCK.lock().await;
    let Some(py) = python() else {
        eprintln!("skipping: no python interpreter");
        return;
    };

    let dir = std::env::temp_dir().join(format!("leveler-lsp-env-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("mock_lsp.py");
    let captured = dir.join("captured.txt");
    std::fs::write(&script, MOCK_SERVER).unwrap();
    unsafe {
        std::env::set_var("LVTEST_LSP_CAPTURE", &captured);
        std::env::set_var("LVTEST_LSP_API_KEY", "must-not-leak");
    }

    let environment = leveler_core::EnvSnapshot::new(
        std::env::vars_os(),
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    );
    let client = LspClient::start_with_environment(
        &py,
        &[script.to_string_lossy().into_owned()],
        &dir,
        &environment,
    )
    .await
    .expect("start LSP");
    client.shutdown().await;

    let leaked = std::fs::read_to_string(&captured).unwrap();
    assert_eq!(leaked, "", "LSP child inherited a provider credential");
    unsafe {
        std::env::remove_var("LVTEST_LSP_CAPTURE");
        std::env::remove_var("LVTEST_LSP_API_KEY");
    }
    std::fs::remove_dir_all(&dir).ok();
}
