//! MCP servers are third-party subprocesses and must not inherit provider or
//! credential-like environment variables unless explicitly configured.
//!
//! Unix-only: the probe spawns an absolute `/bin/sh` that does not exist on
//! Windows (bare `sh` would PATH-resolve, but this test pins the interpreter).
#![cfg(unix)]

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

/// `Tool::replay_is_side_effect_free` promises "no write, no process, no
/// external call" — crash recovery re-runs these automatically, with nobody
/// watching. An earlier version granted it to the LSP-backed tools and the
/// git tools, both of which start a process. Keep the set small and explicit:
/// adding a tool here means asserting a replay cannot touch anything.
#[test]
fn only_pure_file_readers_claim_replay_safety() {
    const EXPECTED: &[&str] = &[
        "find_files",
        "grep",
        "list_files",
        "read_file",
        "view_image",
    ];

    let registry = leveler_tools::full_registry();
    let mut claimed: Vec<&str> = registry
        .definitions()
        .iter()
        .map(|d| d.name.as_str())
        .filter(|name| registry.replay_is_side_effect_free(name))
        .map(|name| {
            // Leak-free: names are &'static in the registry's tools.
            EXPECTED
                .iter()
                .copied()
                .find(|e| *e == name)
                .unwrap_or("UNEXPECTED")
        })
        .collect();
    claimed.sort_unstable();

    let unexpected = claimed.iter().filter(|n| **n == "UNEXPECTED").count();
    assert_eq!(
        unexpected, 0,
        "a tool outside the pure-reader set claims replay safety; \
         if it starts a process, opens a language server, or touches the \
         network, recovery must not re-run it unattended"
    );

    let mut expected = EXPECTED.to_vec();
    expected.sort_unstable();
    assert_eq!(
        claimed, expected,
        "the replay-safe set changed; every entry must be a pure in-process read"
    );
}
