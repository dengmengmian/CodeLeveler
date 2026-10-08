//! A reconnect hands a client back the SAME running tool, with the same
//! execution-round identity.
//!
//! This is the runtime half of the Release Gate `reconnect_round_identity`: it
//! drives two real `LocalSocketRuntimeClient`s against a real
//! `LocalSocketServer` over ONE session with a real running tool, so the
//! `model_step` and the `ToolCallId` the snapshot returns are the runtime's
//! own facts, not a test double's.
//!
//! What it proves:
//!
//! ```text
//! live snapshot.active_tools[0]      == { id, model_step = Some(_) }
//! detach (socket closed, server torn down)
//! reconnect snapshot.active_tools[0] == the SAME id, the SAME model_step
//! settle (cancel)                    => no active tools left behind
//! ```
//!
//! A snapshot that dropped the round would still show the tool — that is why
//! the assertion is on the identity, not on the row's existence.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ApprovalPolicy, ClientCommand, InteractiveRuntimeClient, PermissionProfile as WirePermission,
    UiActiveToolCall,
};
use leveler_core::SessionId;
use leveler_execution::PermissionProfile;
use leveler_local_transport::{
    CollaborationMode, CreateSessionRequest, CreateWorkspaceSelection, LocalSocketRuntimeClient,
    LocalSocketServer,
};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};
use tokio_util::sync::CancellationToken;

/// Long enough that it never settles on its own inside the test window.
const COMMAND: &str = "sleep 60";

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn write_config(root: &std::path::Path, base_url: &str) {
    isolate_global_config();
    std::fs::create_dir_all(root.join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.join("configs/models")).unwrap();
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("configs/models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities:
  streaming: true
  tool_calling: true
  parallel_tool_calls: false
  structured_output: true
  reasoning: false
  vision: false
limits:
  context_window: 131072
  reliable_context: 65536
  max_output_tokens: 1024
  max_tool_schema_bytes: 8192
  max_parallel_tool_calls: 1
compatibility:
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#,
    )
    .unwrap();
}

/// The model answers with one real command call, then keeps repeating it.
fn command_call() -> MockResponse {
    let frames = [
        serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "shell_command",
                        "arguments": serde_json::json!({"cmd": COMMAND}).to_string(),
                    },
                }]},
            }]
        })
        .to_string(),
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
            .to_string(),
    ];
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(&frame);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    MockResponse::Sse { body }
}

struct Harness {
    _tmp: tempfile::TempDir,
    _server: MockServer,
    runtime: Arc<InProcessRuntimeClient>,
    socket: std::path::PathBuf,
}

async fn harness() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let model_server = MockServer::start(vec![command_call()]).await;
    write_config(tmp.path(), &model_server.base_url());
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let runtime = Arc::new(InProcessRuntimeClient::new(
        app,
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ));
    let socket = tmp.path().join("reconnect.sock");
    Harness {
        _tmp: tmp,
        _server: model_server,
        runtime,
        socket,
    }
}

/// Start a fresh socket server + client pair on `socket`.
async fn attach(
    runtime: &Arc<InProcessRuntimeClient>,
    socket: &std::path::Path,
) -> (
    LocalSocketRuntimeClient,
    CancellationToken,
    tokio::task::JoinHandle<Result<(), leveler_local_transport::TransportError>>,
) {
    let server = LocalSocketServer::bind(socket, runtime.clone())
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(server.serve(shutdown.clone()));
    let client = LocalSocketRuntimeClient::connect(socket).await.unwrap();
    (client, shutdown, task)
}

/// Wait until the session reports exactly one running tool, and return it.
async fn wait_for_one_running_tool(
    client: &LocalSocketRuntimeClient,
    session: &SessionId,
) -> UiActiveToolCall {
    for _ in 0..600 {
        let snapshot = client.snapshot(session).await.unwrap();
        match snapshot.active_tools.as_slice() {
            [tool] => return tool.clone(),
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    panic!("no single running tool ever appeared in the snapshot");
}

async fn wait_until_no_active_tools(client: &LocalSocketRuntimeClient, session: &SessionId) {
    for _ in 0..600 {
        let snapshot = client.snapshot(session).await.unwrap();
        if snapshot.active_tools.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the cancelled tool never left the snapshot");
}

#[tokio::test]
async fn a_reconnect_returns_the_same_running_tool_with_its_round() {
    let h = harness().await;

    // ── client A: a detached TUI over the socket transport ───────────────
    let (client_a, shutdown_a, task_a) = attach(&h.runtime, &h.socket).await;
    let bootstrap = client_a
        .create_session(CreateSessionRequest {
            request_id: None,
            collaboration: CollaborationMode::Chat,
            workspace: CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: ApprovalPolicy::Interactive,
            goal: "keep the round across a reconnect".to_string(),
            model: None,
            mode: WirePermission::Assisted,
        })
        .await
        .unwrap();
    let session = bootstrap.session.id.clone();

    client_a
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "run it".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let before = wait_for_one_running_tool(&client_a, &session).await;
    assert_eq!(before.name, "shell_command");
    assert_eq!(
        before.model_step,
        Some(1),
        "the live snapshot must state the round the runtime requested the call in"
    );

    // ── A disappears entirely: connection dropped AND server torn down ───
    drop(client_a);
    shutdown_a.cancel();
    let _ = task_a.await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    // ── client B: a fresh socket server + connection, same runtime ───────
    let socket_b = h.socket.with_extension("b");
    let (client_b, shutdown_b, task_b) = attach(&h.runtime, &socket_b).await;

    let after = wait_for_one_running_tool(&client_b, &session).await;
    assert_eq!(
        after.id, before.id,
        "the reconnected snapshot must name the same ToolCallId"
    );
    assert_eq!(
        after.model_step, before.model_step,
        "and must not re-derive the execution round"
    );
    assert_eq!(after.name, before.name);
    assert!(
        after.elapsed_ms > 0,
        "the runtime's own clock must survive the reconnect, got {}",
        after.elapsed_ms
    );
    assert_eq!(
        after.output_tail, before.output_tail,
        "and so must what the call already printed"
    );

    // ── settle: the call the reconnected client owns can be ended ────────
    client_b
        .send(ClientCommand::CancelCurrentTurn {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    wait_until_no_active_tools(&client_b, &session).await;

    shutdown_b.cancel();
    let _ = task_b.await;
}
