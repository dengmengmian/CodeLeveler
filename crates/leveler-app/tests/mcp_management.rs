//! MCP management through the client protocol.
//!
//! The runtime's one connection authority is exercised end to end with a real
//! local stdio server: list, reconnect, disable/enable, add, update and remove,
//! plus the `/doctor` findings that must agree with it. The server is a Python
//! process that speaks the MCP handshake; nothing here fabricates a connection
//! state.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::mcp::{
    McpConnectionState, McpNameRef, McpRequest, McpResponse, McpServerSpec, McpToolAdmission,
    McpTransport,
};
use leveler_client_protocol::{
    ClientCommand, CommandId, DiagnosticReport, DiagnosticStatus, InteractiveRuntimeClient,
    RuntimeEvent, SessionId, finding_id,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;

const SERVER: &str = r#"import json, os, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    method = request.get("method", "")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "echo", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "echo tool", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "ok"}]}
    else:
        result = {}
    if "id" in request:
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#;

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

fn toml_path(path: &Path) -> String {
    // TOML basic strings treat `\` as an escape; a Windows path is written
    // with forward slashes so the fixture parses on every host.
    path.display().to_string().replace('\\', "/")
}

fn isolate_home(home: &Path) {
    unsafe {
        std::env::set_var("LEVELER_HOME", home);
    }
}

fn layout(tmp: &tempfile::TempDir) -> Layout {
    Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    )
}

struct Fixture {
    _tmp: tempfile::TempDir,
    _home: tempfile::TempDir,
    client: Arc<InProcessRuntimeClient>,
    session: SessionId,
    config: PathBuf,
    script: PathBuf,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let python = python()?;
        let home = tempfile::tempdir().unwrap();
        isolate_home(home.path());
        let tmp = tempfile::tempdir().unwrap();
        let script = home.path().join("echo.py");
        std::fs::write(&script, SERVER).unwrap();
        let config = home.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[[mcp_servers]]\nname = \"echo\"\ncommand = \"{}\"\nargs = [\"-u\", \"{}\"]\nenv = {{ SERVICE_API_KEY = \"SERVICE_API_KEY\" }}\n",
                toml_path(Path::new(&python)),
                toml_path(&script)
            ),
        )
        .unwrap();
        let app = Arc::new(Application::assemble(layout(&tmp)).unwrap());
        let model = ModelRef::parse("deepseek/test-model").unwrap();
        let session = app.create_session(&model, "mcp-management").await.unwrap();
        let client = Arc::new(InProcessRuntimeClient::new(
            app,
            model,
            PermissionProfile::Assisted,
            false,
        ));
        Some(Self {
            _tmp: tmp,
            _home: home,
            client,
            session,
            config,
            script,
        })
    }

    async fn manage(&self, request: McpRequest) -> McpResponse {
        let query_id = CommandId::generate();
        let mut events = self.client.subscribe();
        self.client
            .send(ClientCommand::ManageMcp {
                requester_session_id: self.session.clone(),
                query_id: query_id.clone(),
                request: Box::new(request),
            })
            .await
            .expect("send ManageMcp");
        loop {
            match tokio::time::timeout(Duration::from_secs(15), events.recv()).await {
                Ok(Ok(RuntimeEvent::McpManaged {
                    query_id: answered,
                    response,
                    ..
                })) if answered == query_id => return *response,
                Ok(Ok(_)) | Ok(Err(_)) => continue,
                Err(error) => panic!("no MCP answer: {error}"),
            }
        }
    }

    async fn diagnostics(&self) -> DiagnosticReport {
        let query_id = CommandId::generate();
        let mut events = self.client.subscribe();
        self.client
            .send(ClientCommand::QueryDiagnostics {
                requester_session_id: self.session.clone(),
                query_id: query_id.clone(),
            })
            .await
            .expect("send QueryDiagnostics");
        loop {
            match tokio::time::timeout(Duration::from_secs(15), events.recv()).await {
                Ok(Ok(RuntimeEvent::DiagnosticsLoaded {
                    query_id: answered,
                    report,
                    ..
                })) if answered == query_id => return *report,
                Ok(Ok(_)) | Ok(Err(_)) => continue,
                Err(error) => panic!("no diagnostics answer: {error}"),
            }
        }
    }
}

fn find<'a>(
    response: &'a McpResponse,
    name: &str,
) -> Option<&'a leveler_client_protocol::mcp::McpServerView> {
    let servers = match response {
        McpResponse::Snapshot { servers }
        | McpResponse::Changed { servers, .. }
        | McpResponse::Error { servers, .. } => servers,
    };
    servers.iter().find(|server| server.name == name)
}

fn diagnostic<'a>(
    report: &'a DiagnosticReport,
    subject: &str,
) -> &'a leveler_client_protocol::DiagnosticFinding {
    report
        .findings
        .iter()
        .find(|finding| {
            finding.id == finding_id::MCP_CONNECTION && finding.subject.as_deref() == Some(subject)
        })
        .expect("MCP connection finding")
}

#[tokio::test]
async fn mcp_management_reads_and_changes_the_runtime_authority() {
    let Some(fixture) = Fixture::new().await else {
        eprintln!("skipping: no Python interpreter is available");
        return;
    };
    // Plant a value the response must never carry. The server would echo it if
    // the launch inherited it; the test asserts the wire answer, not the
    // server's environment.
    unsafe {
        std::env::set_var("SERVICE_API_KEY", "planted-secret-value");
    }

    // --- configured is not connected ---------------------------------------
    let listed = fixture.manage(McpRequest::List).await;
    let echo = find(&listed, "echo").expect("configured server is listed");
    assert_eq!(echo.state, McpConnectionState::NotConnected);
    assert!(!echo.initialized);
    assert!(echo.tool_count.is_none());

    let report = fixture.diagnostics().await;
    assert_eq!(diagnostic(&report, "echo").status, DiagnosticStatus::Warn);

    // --- reconnect is a real attempt ---------------------------------------
    let reconnected = fixture
        .manage(McpRequest::Reconnect {
            server: "echo".into(),
        })
        .await;
    let echo = find(&reconnected, "echo").expect("server still listed");
    assert_eq!(echo.state, McpConnectionState::Connected, "{reconnected:?}");
    assert!(echo.initialized);
    assert_eq!(echo.tool_count, Some(1));
    assert_eq!(echo.protocol.as_deref(), Some("2024-11-05"));
    assert_eq!(echo.tools.len(), 1);
    assert_eq!(echo.tools[0].exposed_name, "mcp__echo__echo");
    assert_eq!(echo.tools[0].admission, McpToolAdmission::Admitted);
    assert_eq!(echo.spec.env.len(), 1);
    assert_eq!(echo.spec.env[0].source, "SERVICE_API_KEY");
    let wire = serde_json::to_string(&reconnected).unwrap();
    assert!(
        !wire.contains("planted-secret-value"),
        "the management answer leaked a secret: {wire}"
    );

    // --- doctor reads the same authority -----------------------------------
    let report = fixture.diagnostics().await;
    assert_eq!(diagnostic(&report, "echo").status, DiagnosticStatus::Pass);
    assert!(
        diagnostic(&report, "echo")
            .evidence
            .join(" ")
            .contains("connected")
    );

    // --- an edit drops the connection the old declaration owned -------------
    let mut edited_spec = find(&reconnected, "echo").unwrap().spec.clone();
    edited_spec.args.push("--noop".into());
    let edited = fixture
        .manage(McpRequest::Update {
            original_name: "echo".into(),
            spec: edited_spec,
        })
        .await;
    let edited_row = find(&edited, "echo").expect("edited server is listed");
    assert_eq!(
        edited_row.state,
        McpConnectionState::NotConnected,
        "an edited declaration must not keep the old connection: {edited:?}"
    );
    assert!(edited_row.tools.is_empty());
    let reconnected = fixture
        .manage(McpRequest::Reconnect {
            server: "echo".into(),
        })
        .await;
    assert_eq!(
        find(&reconnected, "echo").unwrap().state,
        McpConnectionState::Connected
    );

    // --- disable, then a reconnect is refused ------------------------------
    let disabled = fixture
        .manage(McpRequest::SetEnabled {
            server: "echo".into(),
            enabled: false,
        })
        .await;
    assert_eq!(
        find(&disabled, "echo").unwrap().state,
        McpConnectionState::Disabled
    );
    assert!(!find(&disabled, "echo").unwrap().enabled);
    let refused = fixture
        .manage(McpRequest::Reconnect {
            server: "echo".into(),
        })
        .await;
    assert!(matches!(refused, McpResponse::Error { .. }), "{refused:?}");
    assert_eq!(
        find(&refused, "echo").unwrap().state,
        McpConnectionState::Disabled
    );

    // --- re-enable, reconnect ----------------------------------------------
    let enabled = fixture
        .manage(McpRequest::SetEnabled {
            server: "echo".into(),
            enabled: true,
        })
        .await;
    assert_eq!(
        find(&enabled, "echo").unwrap().state,
        McpConnectionState::NotConnected
    );
    let reconnected = fixture
        .manage(McpRequest::Reconnect {
            server: "echo".into(),
        })
        .await;
    assert_eq!(
        find(&reconnected, "echo").unwrap().state,
        McpConnectionState::Connected
    );

    // --- add / update / remove ---------------------------------------------
    let added = fixture
        .manage(McpRequest::Add {
            spec: McpServerSpec {
                name: "second".into(),
                transport: McpTransport::Stdio,
                command: python(),
                args: vec!["-u".into(), toml_path(&fixture.script)],
                env: Vec::new(),
                url: None,
                headers: Vec::new(),
                bearer_token_env: None,
                enabled: true,
            },
        })
        .await;
    assert!(find(&added, "second").is_some(), "{added:?}");
    assert!(
        std::fs::read_to_string(&fixture.config)
            .unwrap()
            .contains("name = \"second\"")
    );

    let renamed = fixture
        .manage(McpRequest::Update {
            original_name: "second".into(),
            spec: McpServerSpec {
                name: "renamed".into(),
                transport: McpTransport::StreamableHttp,
                command: None,
                args: Vec::new(),
                env: Vec::new(),
                url: Some("https://example.invalid/mcp".into()),
                headers: vec![McpNameRef {
                    key: "X-API-Key".into(),
                    source: "SERVICE_API_KEY".into(),
                }],
                bearer_token_env: None,
                enabled: true,
            },
        })
        .await;
    let renamed_row = find(&renamed, "renamed").expect("renamed server is listed");
    assert_eq!(renamed_row.transport, McpTransport::StreamableHttp);
    assert!(find(&renamed, "second").is_none());
    let wire = serde_json::to_string(&renamed).unwrap();
    assert!(!wire.contains("planted-secret-value"));

    let removed = fixture
        .manage(McpRequest::Remove {
            server: "renamed".into(),
        })
        .await;
    assert!(find(&removed, "renamed").is_none(), "{removed:?}");
    assert!(
        !std::fs::read_to_string(&fixture.config)
            .unwrap()
            .contains("name = \"renamed\"")
    );

    // Removing a name that does not exist is an error, and the authoritative
    // rows survive it.
    let missing = fixture
        .manage(McpRequest::Remove {
            server: "renamed".into(),
        })
        .await;
    assert!(matches!(missing, McpResponse::Error { .. }));
    assert!(find(&missing, "echo").is_some());

    // --- reload -------------------------------------------------------------
    let reloaded = fixture.manage(McpRequest::Reload).await;
    assert!(
        matches!(
            reloaded,
            McpResponse::Changed { .. } | McpResponse::Snapshot { .. }
        ),
        "{reloaded:?}"
    );

    unsafe {
        std::env::remove_var("SERVICE_API_KEY");
    }
}
