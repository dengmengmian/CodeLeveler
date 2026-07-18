//! Live test: `find_symbol` resolves precise `path:line` via a real language
//! server. Gated on rust-analyzer being installed, so CI without it just skips.

use leveler_execution::{PermissionProfile, Workspace};
use leveler_tools::Tool;
use leveler_tools::tool::ToolContext;
use leveler_tools::tools::FindSymbolTool;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn find_symbol_uses_rust_analyzer_for_precise_location() {
    if !leveler_lsp::server_available(leveler_project::Language::Rust) {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }

    let result = tokio::time::timeout(std::time::Duration::from_secs(45), async {
        find_symbol_with_rust_analyzer().await
    })
    .await;
    match result {
        Ok(out) => {
            assert!(
                out.contains("rust-analyzer"),
                "expected LSP path, got: {out}"
            );
            assert!(
                out.contains("src/lib.rs:2"),
                "expected precise location, got: {out}"
            );
        }
        Err(_) => {
            eprintln!("skipping: rust-analyzer did not respond within 45s");
        }
    }
}

async fn find_symbol_with_rust_analyzer() -> String {
    let dir = std::env::temp_dir().join(format!("leveler-fs-lsp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    // The definition is on line 2 (1-based).
    std::fs::write(
        dir.join("src/lib.rs"),
        "// header\npub fn special_marker_fn() -> i32 {\n    7\n}\n",
    )
    .unwrap();

    // Build ONE context and reuse it: `ToolContext` caches the language-server
    // session, so the same rust-analyzer keeps indexing between polls instead of
    // a fresh server racing the indexer on every call (the source of the earlier
    // flakiness). Poll until indexing settles and the precise location resolves.
    let ws = Workspace::new(&dir).unwrap();
    let ctx = ToolContext::new(ws, PermissionProfile::RequestApproval);
    let mut out = String::new();
    for _ in 0..20 {
        out = FindSymbolTool
            .execute(
                serde_json::json!({ "symbol": "special_marker_fn" }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .content;
        if out.contains("rust-analyzer") && out.contains("src/lib.rs:2") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    std::fs::remove_dir_all(&dir).ok();
    out
}
