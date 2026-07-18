//! Live end-to-end check of the MCP client against a real server
//! (`@modelcontextprotocol/server-everything` over npx). Needs node/npx +
//! network, so it is #[ignore]d; run explicitly:
//!   cargo test -p leveler-tools --test mcp_live -- --ignored --nocapture

use leveler_execution::{PermissionProfile, Workspace};
use leveler_tools::mcp::{McpServerConfig, connect_all};
use leveler_tools::tool::ToolContext;
use tokio_util::sync::CancellationToken;

fn npx_available() -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|d| d.join("npx").is_file()))
        .unwrap_or(false)
}

#[ignore = "live: needs node/npx + network; run with --ignored"]
#[tokio::test]
async fn connects_to_real_mcp_server_and_calls_a_tool() {
    if !npx_available() {
        eprintln!("skipping: npx not installed");
        return;
    }

    let config = McpServerConfig {
        name: "everything".to_string(),
        command: "npx".to_string(),
        args: vec![
            "-y".to_string(),
            "@modelcontextprotocol/server-everything".to_string(),
        ],
        env: Vec::new(),
    };

    // 1) Connect + tools/list through our client.
    let tools = connect_all(std::slice::from_ref(&config)).await;
    assert!(
        !tools.is_empty(),
        "expected the MCP server to advertise tools"
    );
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    eprintln!("MCP tools discovered: {names:?}");
    assert!(
        names.iter().all(|n| n.starts_with("mcp__everything__")),
        "tools should be namespaced: {names:?}"
    );

    // 2) Actually call the `echo` tool and check the round-trip.
    let echo = tools
        .iter()
        .find(|t| t.name() == "mcp__everything__echo")
        .expect("server-everything exposes an `echo` tool");

    let dir = std::env::temp_dir().join(format!("leveler-mcp-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = ToolContext::new(
        Workspace::new(&dir).unwrap(),
        PermissionProfile::RequestApproval,
    );

    let out = echo
        .execute(
            serde_json::json!({ "message": "leveler-mcp-ok" }),
            ctx,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    eprintln!("echo output: {}", out.content);
    assert!(!out.is_error, "echo call errored: {}", out.content);
    assert!(
        out.content.contains("leveler-mcp-ok"),
        "expected the echoed message, got: {}",
        out.content
    );

    std::fs::remove_dir_all(&dir).ok();
}
