//! An MCP server that is not up when CodeLeveler starts must be discovered
//! once it is, and the model must be able to see why it was not.
//!
//! The ordinary scene: the user opens CodeLeveler, then asks the agent to start
//! the project. The configured MCP server is unreachable for the first turns and
//! available later. These pin three facts:
//!
//! * a failed attempt is not remembered as "already connected";
//! * a later turn reconnects without a config change, and an available server is
//!   not reconnected on every turn;
//! * while it is down, the capability catalog says which server is unreachable
//!   and why, instead of presenting the gap as "no external tools configured".

use leveler_agent::AutoClarify;
use leveler_app::Application;
use leveler_execution::{AutoApprove, PermissionProfile};
use leveler_model::ModelRef;
use leveler_project::Layout;
use std::sync::Arc;

fn isolate_global_config(home: &std::path::Path) {
    unsafe {
        std::env::set_var("LEVELER_HOME", home);
    }
}

/// `LEVELER_HOME` is process-global: the tests in this binary must not read
/// each other's config. The guard stays alive across awaits on purpose, so it
/// is an async mutex rather than a blocking one.
async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

fn layout(tmp: &tempfile::TempDir) -> Layout {
    Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    )
}

const STDIO_SERVER: &str = r#"import json, sys
for line in sys.stdin:
    if not line.strip():
        continue
    request = json.loads(line)
    method = request.get("method", "")
    if method == "initialize":
        result = {}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "echo", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "late-server-ok"}]}
    else:
        result = {}
    if "id" in request:
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#;

async fn engine(app: &Application) -> leveler_agent::coding::CodingRuntime {
    app.engine_for_session(
        &ModelRef::new("mock", "m"),
        PermissionProfile::FullAccess,
        false,
        Arc::new(AutoApprove),
        Arc::new(AutoClarify),
        false,
        Some("mcp-lifecycle"),
    )
    .await
    .unwrap()
}

async fn mcp_tool_names(app: &Application) -> Vec<String> {
    engine(app)
        .await
        .factory
        .registry
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .filter(|name| name.starts_with("mcp__"))
        .collect()
}

fn external_tools_entry(catalog: &serde_json::Value) -> serde_json::Value {
    catalog
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "external_tools")
        .cloned()
        .unwrap()
}

fn server_config(name: &str, script: &std::path::Path, python: &str) -> String {
    // TOML basic strings use backslash escapes, and Windows paths are full of
    // them. Escape both the interpreter and the script path.
    let python = python.replace('\\', "\\\\");
    let script = script.display().to_string().replace('\\', "\\\\");
    format!(
        "[[mcp_servers]]\nname = \"{name}\"\ncommand = \"{python}\"\nargs = [\"-u\", \"{script}\"]\n"
    )
}

/// The server command does not exist yet on the first turn (the project has not
/// been started), then it appears. The next turn must find its tools.
#[tokio::test]
async fn a_server_that_starts_after_the_first_turn_is_discovered_later() {
    let _env = env_lock().await;
    let Some(python) = python() else {
        eprintln!("skipping: no Python interpreter is available");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(home.path());
    let script = home.path().join("late-server.py");
    std::fs::write(
        home.path().join("config.toml"),
        server_config("late", &script, &python),
    )
    .unwrap();
    let app = Application::assemble(layout(&tmp)).unwrap();

    assert!(
        mcp_tool_names(&app).await.is_empty(),
        "the server cannot be reached yet, so no tool may be registered"
    );
    // The model can tell why the pack is missing instead of guessing.
    let catalog = engine(&app).await;
    let entry = external_tools_entry(&catalog.factory.capabilities.as_ref().unwrap().catalog());
    assert_eq!(entry["available"], false);
    // "Not reachable yet" is not "not allowed": permission is an authority
    // fact that must survive a late catalog, or `capability enable` refuses a
    // pack this mode permits.
    assert_eq!(entry["permitted"], true);
    let detail = entry["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("late"),
        "the catalog must name the unreachable server: {detail}"
    );

    std::fs::write(&script, STDIO_SERVER).unwrap();
    // The first retry waits for the backoff, the way a real second turn does.
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;

    assert_eq!(
        mcp_tool_names(&app).await,
        vec!["mcp__late__echo".to_string()],
        "a server that came up after startup must be discovered on a later turn"
    );
}

/// A server that is reachable from the first turn keeps its live connection and
/// is not reconnected on every later turn.
#[tokio::test]
async fn an_available_server_is_not_reconnected_every_turn() {
    let _env = env_lock().await;
    let Some(python) = python() else {
        eprintln!("skipping: no Python interpreter is available");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    isolate_global_config(home.path());
    let script = home.path().join("server.py");
    std::fs::write(&script, STDIO_SERVER).unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        server_config("up", &script, &python),
    )
    .unwrap();
    let app = Application::assemble(layout(&tmp)).unwrap();
    assert_eq!(
        mcp_tool_names(&app).await,
        vec!["mcp__up__echo".to_string()]
    );
    assert_eq!(
        mcp_tool_names(&app).await,
        vec!["mcp__up__echo".to_string()],
        "the cached connection is reused"
    );
}

/// The first interpreter that actually runs. Windows CI has no `/usr/bin`,
/// and a host without Python skips these tests instead of failing the suite.
fn python() -> Option<String> {
    [
        "/usr/bin/python3",
        "/opt/homebrew/bin/python3",
        "python3",
        "python",
    ]
    .into_iter()
    .find(|candidate| {
        std::process::Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    })
    .map(str::to_string)
}
