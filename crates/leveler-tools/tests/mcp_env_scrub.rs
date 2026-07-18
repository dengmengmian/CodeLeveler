//! MCP servers are third-party subprocesses and must not inherit provider or
//! credential-like environment variables unless explicitly configured.

use leveler_tools::mcp::{McpClient, McpServerConfig};

#[tokio::test]
async fn mcp_process_does_not_inherit_credential_like_environment() {
    let dir = tempfile::tempdir().unwrap();
    let captured = dir.path().join("captured.txt");
    unsafe {
        std::env::set_var("LVTEST_MCP_API_KEY", "must-not-leak");
    }
    let config = McpServerConfig {
        name: "env-probe".to_string(),
        command: "/bin/sh".to_string(),
        args: vec![
            "-c".to_string(),
            "printf %s \"$LVTEST_MCP_API_KEY\" > \"$1\"; exit 1".to_string(),
            "sh".to_string(),
            captured.display().to_string(),
        ],
        env: Vec::new(),
    };

    let _ = McpClient::connect(&config).await;
    let leaked = std::fs::read_to_string(&captured).unwrap();
    assert_eq!(leaked, "", "MCP child inherited a provider credential");
}
