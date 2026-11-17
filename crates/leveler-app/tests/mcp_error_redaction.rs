//! A real MCP server can echo the credentials it received inside any error.
//! The runtime's projection, the wire answer and `/doctor` must carry the
//! diagnosis without the value.
//!
//! The server here is a real Python stdio process; the secret is a test value
//! placed in its environment through the documented name-reference config.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::mcp::{McpConnectionState, McpRequest, McpResponse};
use leveler_client_protocol::{
    ClientCommand, CommandId, DiagnosticReport, InteractiveRuntimeClient, RuntimeEvent, SessionId,
    finding_id,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;

const SECRET: &str = "e2e-app-secret-value-7c41";

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
        send({"jsonrpc": "2.0", "id": request["id"], "result": {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "leaky", "version": "1"}}})
    elif method == "tools/list":
        if mode == "list":
            send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "catalog failed secret=" + secret}})
        else:
            send({"jsonrpc": "2.0", "id": request["id"], "result": {"tools": []}})
    else:
        send({"jsonrpc": "2.0", "id": request["id"], "result": {}})
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

fn toml_path(path: &std::path::Path) -> String {
    path.display().to_string().replace('\\', "/")
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
}

impl Fixture {
    async fn new(mode: &str) -> Option<Self> {
        let python = python()?;
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("LEVELER_HOME", home.path());
            std::env::set_var("E2E_MCP_SECRET", SECRET);
        }
        let tmp = tempfile::tempdir().unwrap();
        let script = home.path().join("leaky.py");
        std::fs::write(&script, SERVER).unwrap();
        // The env entry is a NAME REFERENCE, exactly as the product requires.
        std::fs::write(
            home.path().join("config.toml"),
            format!(
                "[[mcp_servers]]\nname = \"leaky\"\ncommand = \"{}\"\nargs = [\"-u\", \"{}\", \"{mode}\"]\nenv = {{ E2E_MCP_SECRET = \"E2E_MCP_SECRET\" }}\n",
                toml_path(std::path::Path::new(&python)),
                toml_path(&script)
            ),
        )
        .unwrap();
        let app = Arc::new(Application::assemble(layout(&tmp)).unwrap());
        let model = ModelRef::parse("deepseek/test-model").unwrap();
        let session = app.create_session(&model, "mcp-redaction").await.unwrap();
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

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            std::env::remove_var("E2E_MCP_SECRET");
        }
    }
}

fn row(response: &McpResponse) -> &leveler_client_protocol::mcp::McpServerView {
    let servers = match response {
        McpResponse::Snapshot { servers }
        | McpResponse::Changed { servers, .. }
        | McpResponse::Error { servers, .. } => servers,
    };
    servers
        .iter()
        .find(|server| server.name == "leaky")
        .expect("the configured server is listed")
}

fn doctor_evidence(report: &DiagnosticReport) -> String {
    report
        .findings
        .iter()
        .filter(|finding| {
            finding.id == finding_id::MCP_CONNECTION && finding.subject.as_deref() == Some("leaky")
        })
        .flat_map(|finding| finding.evidence.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn failed_attempt(mode: &str, expected_category: &str) {
    let Some(fixture) = Fixture::new(mode).await else {
        eprintln!("skipping: no Python interpreter is available");
        return;
    };
    let response = fixture
        .manage(McpRequest::Reconnect {
            server: "leaky".into(),
        })
        .await;
    let wire = serde_json::to_string(&response).unwrap();
    let server = row(&response);
    assert_eq!(server.state, McpConnectionState::Failed);
    let error = server.last_error.as_deref().expect("a failure reason");
    assert!(!wire.contains(SECRET), "the wire leaked the secret: {wire}");
    assert!(
        !error.contains(SECRET),
        "the runtime projection leaked the secret: {error}"
    );
    assert!(error.contains("[redacted]"), "value not marked: {error}");
    assert!(
        error.contains(expected_category),
        "the diagnostic category was lost: {error}"
    );
    assert_eq!(server.name, "leaky", "the server id is the row's name");
    assert_eq!(
        server.transport,
        leveler_client_protocol::mcp::McpTransport::Stdio,
        "the transport is a separate fact"
    );

    let report = fixture.diagnostics().await;
    let evidence = doctor_evidence(&report);
    assert!(
        !evidence.contains(SECRET),
        "the doctor leaked the secret: {evidence}"
    );
    assert!(
        evidence.contains("[redacted]"),
        "the doctor did not show a redacted reason: {evidence}"
    );
    assert!(
        evidence.contains("last attempt failed"),
        "the doctor lost the failure fact: {evidence}"
    );
}

#[tokio::test]
async fn every_failed_attempt_is_redacted_before_the_client_or_doctor_sees_it() {
    // The two fixtures share process environment (HOME and the test secret),
    // so one test drives them in order.
    failed_attempt("init", "handshake rejected").await;
    failed_attempt("list", "tools/list failed").await;
}
