//! The Desktop creates sessions through the identified-receipt path. That
//! path must capture the same FullAccess creation baseline as the ordinary
//! `Application::create_session_*` entry points; otherwise switching the new
//! session to Full cannot adopt a pre-session skill and every project skill is
//! refused as untrusted.
//!
//! This test uses the production daemon service (`InProcessRuntimeClient`
//! behind the real local socket) and a mock provider, then asserts the exact
//! skill body reached the model after the switch.

#![cfg(unix)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, CommandEnvelope, CommandId, InteractiveRuntimeClient,
    PermissionProfile as WirePermission, RuntimeEvent,
};
use leveler_execution::PermissionProfile;
use leveler_local_transport::{
    CreateSessionRequest, CreateWorkspaceSelection, LocalSocketRuntimeClient, LocalSocketServer,
};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};
use tokio_util::sync::CancellationToken;

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
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\nretry:\n  max_attempts: 1\n  initial_backoff_ms: 10\n  max_backoff_ms: 10\n"),
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

fn sse(text: &str) -> MockResponse {
    let chunk = |delta: serde_json::Value, finish: Option<&str>| {
        serde_json::json!({
            "id": "r3b", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
        .to_string()
    };
    let body = [
        format!(
            "data: {}",
            chunk(
                serde_json::json!({ "role": "assistant", "content": text }),
                None
            )
        ),
        format!("data: {}", chunk(serde_json::json!({}), Some("stop"))),
        "data: [DONE]".to_string(),
    ]
    .join("\n\n")
        + "\n\n";
    MockResponse::Sse { body }
}

#[tokio::test]
async fn identified_creation_keeps_the_full_baseline() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    unsafe {
        std::env::set_var("LEVELER_HOME", &home);
    }
    let skill = repo.join(".leveler/skills/demo");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo.\n---\n\nDEMO_BODY_MARKER\n",
    )
    .unwrap();

    let mock = MockServer::start_one(sse("R3B_TEST_DONE")).await;
    write_config(&repo, &mock.base_url());
    let layout = Layout::from_parts(repo.clone(), repo.join("configs"), home.join("state"));
    let app = Arc::new(Application::assemble(layout).unwrap());
    let runtime = Arc::new(InProcessRuntimeClient::new(
        app,
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ));
    let socket = tmp.path().join("daemon.sock");
    let server = LocalSocketServer::bind(&socket, runtime.clone())
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let serve = tokio::spawn(server.serve(shutdown.clone()));
    let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();

    // Exactly the Desktop's create: identified, workspace-scoped, Assisted.
    let bootstrap = client
        .create_session(CreateSessionRequest {
            request_id: Some(CommandId::generate()),
            collaboration: leveler_local_transport::CollaborationMode::Chat,
            workspace: CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "identified full adoption".to_string(),
            model: Some(ModelRef::new("mock", "m")),
            mode: WirePermission::Assisted,
            permission_contract: None,
        })
        .await
        .unwrap();
    let session = bootstrap.session.id.clone();
    let mut events = client.subscribe_session(&session);

    let envelope = CommandEnvelope {
        command_id: CommandId::generate(),
        session_id: session.clone(),
        expected_version: Some(
            client
                .snapshot(&session)
                .await
                .unwrap()
                .last_sequence
                .unwrap_or(0),
        ),
        issued_at: leveler_core::now().to_rfc3339(),
        command: ClientCommand::SetPermissionProfile {
            session_id: session.clone(),
            mode: WirePermission::FullAccess,
            contract: None,
        },
    };
    client.deliver(envelope).await.unwrap();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "/demo run the procedure".to_string(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut bodies = Vec::new();
    while Instant::now() < deadline {
        bodies = mock.request_bodies().await;
        if bodies.iter().any(|body| body.contains("DEMO_BODY_MARKER")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        bodies.iter().any(|body| body.contains("DEMO_BODY_MARKER")),
        "the pre-session skill must be adopted after switching to Full; bodies: {bodies:?}"
    );
    assert!(
        !bodies
            .iter()
            .any(|body| body.contains("present but not trusted")),
        "Full must not ask or refuse the baseline skill: {bodies:?}"
    );
    let mut asked = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, RuntimeEvent::DefinitionTrustRequested { .. }) {
            asked = true;
        }
    }
    assert!(!asked, "Full must never raise a definition-trust prompt");

    shutdown.cancel();
    let _ = serve.await;
}
