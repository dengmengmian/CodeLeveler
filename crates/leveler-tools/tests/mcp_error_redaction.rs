//! MCP server text is untrusted. A server can echo the environment or headers
//! it received inside any error; that text must not reach the client, the
//! doctor, the transcript or a log with the value intact.
#![cfg(unix)]

use leveler_tools::mcp::{McpClient, McpServerConfig, connect_all_detailed};

const SECRET: &str = "e2e-stdio-secret-value-9f3a";

/// One fake server. `mode` selects which response carries the secret.
const SERVER: &str = r#"import json, os, sys
mode = sys.argv[1]
secret = os.environ.get("E2E_MCP_SECRET", "absent")
def send(payload):
    print(json.dumps(payload), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method", "")
    if "id" not in request:
        continue
    if method == "initialize":
        if mode == "init":
            send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "handshake rejected token=" + secret}})
            continue
        send({"jsonrpc": "2.0", "id": request["id"], "result": {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fixture", "version": "1"}}})
    elif method == "tools/list":
        if mode == "list":
            send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "catalog failed secret=" + secret}})
        elif mode == "pattern":
            send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "Authorization: Bearer abcdef123456789 and api_key=plain-value-9876"}})
        else:
            send({"jsonrpc": "2.0", "id": request["id"], "result": {"tools": [{"name": "echo", "description": "echo " + secret, "inputSchema": {"type": "object", "properties": {"x": {"default": secret}}}}]}})
    elif method == "tools/call":
        if mode == "call":
            send({"jsonrpc": "2.0", "id": request["id"], "result": {"isError": True, "content": [{"type": "text", "text": "tool failed: api_key=" + secret}]}})
        elif mode == "success":
            send({"jsonrpc": "2.0", "id": request["id"], "result": {"content": [{"type": "text", "text": "ok " + secret}]}})
        elif mode == "pattern":
            send({"jsonrpc": "2.0", "id": request["id"], "result": {"isError": True, "content": [{"type": "text", "text": "Authorization: Bearer abcdef123456789\napi_key=plain-value-9876"}]}})
        else:
            send({"jsonrpc": "2.0", "id": request["id"], "result": {"content": [{"type": "text", "text": "ok"}]}})
    else:
        send({"jsonrpc": "2.0", "id": request["id"], "result": {}})
"#;

fn config(mode: &str) -> McpServerConfig {
    McpServerConfig::stdio(
        "redaction-fixture",
        "/usr/bin/python3",
        vec!["-u".into(), "-c".into(), SERVER.into(), mode.into()],
        vec![("E2E_MCP_SECRET".into(), SECRET.into())],
    )
}

#[tokio::test]
async fn an_initialize_error_cannot_echo_the_configured_environment() {
    let error = match McpClient::connect(&config("init")).await {
        Ok(_) => panic!("the fixture rejects the handshake"),
        Err(error) => error,
    };
    assert!(!error.contains(SECRET), "initialize error leaked: {error}");
    assert!(
        error.contains("[redacted]"),
        "value was not redacted: {error}"
    );
    assert!(
        error.contains("handshake rejected"),
        "category lost: {error}"
    );
    assert!(error.contains("token="), "stable field name lost: {error}");
}

#[tokio::test]
async fn a_tools_list_error_cannot_echo_the_configured_environment() {
    let connections =
        connect_all_detailed(&[config("list")], leveler_core::environment(), false).await;
    let error = connections[0]
        .error
        .as_deref()
        .expect("the fixture fails tools/list");
    assert!(!error.contains(SECRET), "tools/list error leaked: {error}");
    assert!(
        error.contains("[redacted]"),
        "value was not redacted: {error}"
    );
    assert!(
        error.contains("tools/list failed"),
        "category lost: {error}"
    );
    assert!(error.contains("secret="), "stable field name lost: {error}");
}

#[tokio::test]
async fn a_tool_call_error_cannot_echo_the_configured_environment() {
    let client = McpClient::connect(&config("call"))
        .await
        .expect("the fixture accepts the handshake");
    let error = client
        .call_tool("echo", serde_json::json!({}))
        .await
        .expect_err("isError=true is a failure");
    assert!(!error.contains(SECRET), "tool error leaked: {error}");
    assert!(
        error.contains("[redacted]"),
        "value was not redacted: {error}"
    );
    assert!(
        error.contains("api_key="),
        "stable field name lost: {error}"
    );
}

#[tokio::test]
async fn a_successful_catalog_and_call_cannot_echo_the_configured_environment() {
    let connections =
        connect_all_detailed(&[config("success")], leveler_core::environment(), false).await;
    let tool = connections[0].tools.first().expect("one discovered tool");
    assert!(
        !tool.description().contains(SECRET),
        "tool description leaked: {}",
        tool.description()
    );
    assert!(
        !tool.input_schema().to_string().contains(SECRET),
        "tool schema leaked: {}",
        tool.input_schema()
    );
    let client = McpClient::connect(&config("success"))
        .await
        .expect("the fixture accepts the handshake");
    let content = client
        .call_tool("echo", serde_json::json!({}))
        .await
        .expect("a successful call");
    assert!(
        !content.contains(SECRET),
        "successful result leaked: {content}"
    );
    assert!(
        content.contains("[redacted]"),
        "value not marked: {content}"
    );
}

#[tokio::test]
async fn common_credential_shapes_are_redacted_without_a_configured_value() {
    let client = McpClient::connect(&config("pattern"))
        .await
        .expect("the fixture accepts the handshake");
    let error = client
        .call_tool("echo", serde_json::json!({}))
        .await
        .expect_err("isError=true is a failure");
    assert!(
        !error.contains("abcdef123456789"),
        "bearer token leaked: {error}"
    );
    assert!(
        !error.contains("plain-value-9876"),
        "api_key value leaked: {error}"
    );
    assert!(
        error.contains("Authorization:"),
        "header name lost: {error}"
    );
    assert!(error.contains("api_key="), "field name lost: {error}");
}
