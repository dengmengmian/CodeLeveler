//! MCP tool execution errors must survive the client boundary.
#![cfg(unix)]

use leveler_tools::mcp::{McpClient, McpServerConfig};

#[tokio::test]
async fn server_tool_error_is_not_reported_as_success() {
    let config = McpServerConfig {
        name: "tool-error".to_string(),
        command: "/bin/sh".to_string(),
        args: vec![
            "-c".to_string(),
            r#"read -r init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fixture","version":"1"}}}'
read -r notification
read -r call
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"isError":true,"content":[{"type":"text","text":"{\"error\":{\"code\":\"not_found\",\"message\":\"task not found\"}}"}]}}'
read -r done
"#
            .to_string(),
        ],
        env: Vec::new(),
    };
    let client = McpClient::connect(&config).await.unwrap();
    let result = client
        .call_tool("agent_status", serde_json::json!({"task_id":"missing"}))
        .await;
    let error = result.expect_err("MCP isError=true must not become a successful tool result");
    assert!(error.contains("not_found"), "error code lost: {error}");
    assert!(
        error.contains("task not found"),
        "error message lost: {error}"
    );
}
