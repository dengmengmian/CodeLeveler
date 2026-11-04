//! A deterministic action run over the real local transport.
//!
//! The scheduled-action carrier in Phase 3 is the existing user-originated
//! shell command: no model, no agent loop, no tool registry, no mail account,
//! and no external side effect. This test proves that path end to end through
//! a socket runtime, not the in-process shortcut:
//!
//! - the command runs in the session workspace and reports success;
//! - the provider server sees ZERO requests;
//! - the workspace gains no file.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ApprovalPolicy, ClientCommand, InteractiveRuntimeClient, PermissionProfile as WireProfile,
    RuntimeEvent,
};
use leveler_execution::PermissionProfile;
use leveler_local_transport::{
    CollaborationMode, CreateSessionRequest, CreateWorkspaceSelection, LocalSocketRuntimeClient,
    LocalSocketServer, LocalWaiters,
};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::MockServer;
use tokio_util::sync::CancellationToken;

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn write_config(config_dir: &std::path::Path, base_url: &str) {
    isolate_global_config();
    std::fs::create_dir_all(config_dir.join("providers")).unwrap();
    std::fs::create_dir_all(config_dir.join("models")).unwrap();
    std::fs::write(
        config_dir.join("providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
    )
    .unwrap();
    std::fs::write(
        config_dir.join("models/m.yaml"),
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

fn workspace_entries(workspace: &std::path::Path) -> Vec<String> {
    let mut entries: Vec<_> = std::fs::read_dir(workspace)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    entries
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deterministic_action_run_succeeds_without_a_model() {
    let provider = MockServer::start(Vec::new()).await;

    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let config_dir = tmp.path().join("configs");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    write_config(&config_dir, &provider.base_url());

    let layout = Layout::from_parts(workspace.clone(), config_dir, state_dir);
    let app = Arc::new(Application::assemble(layout).unwrap());
    let runtime = Arc::new(InProcessRuntimeClient::new(
        app,
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ));

    let socket = tmp.path().join("daemon.sock");
    let waiters = LocalWaiters::new();
    let server = LocalSocketServer::bind_with_waiters(&socket, runtime, waiters)
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let serve = tokio::spawn(server.serve(shutdown.clone()));

    let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    let session = client
        .create_session(CreateSessionRequest {
            request_id: None,
            workspace: CreateWorkspaceSelection::RuntimeDefault,
            goal: "deterministic action run".to_string(),
            model: Some(ModelRef::new("mock", "m")),
            mode: WireProfile::Assisted,
            approval_policy: ApprovalPolicy::Interactive,
            collaboration: CollaborationMode::Chat,
        })
        .await
        .unwrap()
        .session
        .id;
    let mut events = client.subscribe_session(&session);

    let before = workspace_entries(&workspace);
    client
        .send(ClientCommand::RunUserShell {
            session_id: session.clone(),
            command: "printf 'leveler-action-ok\\n'".to_string(),
        })
        .await
        .unwrap();

    let mut stdout = String::new();
    let mut exit = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while exit.is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, events.recv())
            .await
            .expect("the action run reported an outcome before the deadline")
            .expect("the event stream stayed open");
        match event {
            RuntimeEvent::UserShellOutput {
                stream, chunk, ..
            } if stream == "stdout" => stdout.push_str(&chunk),
            RuntimeEvent::UserShellExited {
                exit_code, status, ..
            } => exit = Some((exit_code, status)),
            _ => {}
        }
    }

    assert_eq!(exit, Some((Some(0), "success".to_string())));
    assert!(
        stdout.contains("leveler-action-ok"),
        "the action's own output was not observed: {stdout:?}"
    );
    assert_eq!(workspace_entries(&workspace), before, "the action changed its workspace");
    assert_eq!(
        provider.request_count(),
        0,
        "a deterministic action run must never reach a model"
    );

    drop(client);
    shutdown.cancel();
    let _ = serve.await;
}
