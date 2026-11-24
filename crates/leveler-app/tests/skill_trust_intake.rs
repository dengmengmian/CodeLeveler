//! An explicit `/name` for an untrusted Skill asks the human through the
//! runtime's normal trust surface before any turn starts.
//!
//! This is the product interaction, not a unit shim: the client sends the
//! message, receives `DefinitionTrustRequested`, answers with the same
//! structured decision the TUI's overlay emits, and only then does the model
//! request carry the approved procedure body.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, DefinitionTrustDecision, InteractiveRuntimeClient, RuntimeEvent,
    UiDefinitionTrustRequest,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

/// One isolated home per test binary. `Application::assemble` snapshots the
/// environment, so this must run before every assemble in this file.
fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn sse(frames: Vec<String>) -> MockResponse {
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(&frame);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    MockResponse::Sse { body }
}

fn text(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

fn write_skill(repo: &std::path::Path) {
    let dir = repo.join(".leveler/skills/deploy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: Ship safely.\nargument-hint: <env>\n---\n\n\
         DEPLOY-BODY-MARKER\n",
    )
    .unwrap();
}

async fn client_with(
    responses: Vec<MockResponse>,
    profile: PermissionProfile,
) -> (
    tempfile::TempDir,
    MockServer,
    Arc<InProcessRuntimeClient>,
    leveler_core::SessionId,
) {
    isolate_global_config();
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
    write_skill(tmp.path());
    std::fs::create_dir_all(tmp.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/models")).unwrap();
    std::fs::write(
        tmp.path().join("configs/providers/mock.yaml"),
        format!(
            "id: mock\nprotocol: openai_chat\nbase_url: {}\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("configs/models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let model = ModelRef::new("mock", "m");
    // The session row owns the effective mode; the runtime reads it back.
    // Creating the session under the requested profile also captures the
    // definition baseline FullAccess may adopt.
    let session = app
        .create_session_with_mode(&model, "skill trust", profile)
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(app, model, profile, false));
    (tmp, server, client, session)
}

async fn next_trust_request(
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
) -> UiDefinitionTrustRequest {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::DefinitionTrustRequested { request })) => return request,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("no definition-trust request reached the client"),
        }
    }
}

async fn wait_for_requests(server: &MockServer, wanted: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let bodies = server.request_bodies().await;
        if bodies.len() >= wanted {
            return bodies;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the model request never arrived"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn submit(
    client: &Arc<InProcessRuntimeClient>,
    session: leveler_core::SessionId,
    content: &str,
) -> tokio::task::JoinHandle<Result<(), leveler_client_protocol::ClientError>> {
    let client = client.clone();
    let content = content.to_string();
    tokio::spawn(async move {
        client
            .send(ClientCommand::SubmitMessage {
                session_id: session,
                content,
                attachments: vec![],
            })
            .await
    })
}

/// Assisted mode: the request is the user's own `/deploy`, but its body is
/// still untrusted input. The runtime asks about these exact bytes; allowing
/// once admits the package for this session, and the model request carries the
/// body together with the user's literal message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_explicit_untrusted_skill_asks_then_delivers_the_approved_body() {
    let (_tmp, server, client, session) =
        client_with(vec![text("DEPLOY-DONE")], PermissionProfile::Assisted).await;
    let mut rx = client.subscribe();
    let sender = submit(&client, session.clone(), "/deploy staging");

    let request = next_trust_request(&mut rx).await;
    assert_eq!(request.kind, "skill");
    assert!(
        request.identity.contains("deploy"),
        "the request names the exact definition: {request:?}"
    );
    assert!(
        request.current_content.contains("DEPLOY-BODY-MARKER"),
        "the human reviews the bytes the decision binds: {request:?}"
    );

    client
        .send(ClientCommand::DefinitionTrustDecision {
            request_id: request.id,
            decision: DefinitionTrustDecision::AllowOnce,
            session_id: Some(session.clone()),
        })
        .await
        .unwrap();
    sender
        .await
        .unwrap()
        .expect("the turn starts after approval");

    let bodies = wait_for_requests(&server, 1).await;
    assert!(
        bodies[0].contains("DEPLOY-BODY-MARKER"),
        "the approved procedure reaches the model: {}",
        bodies[0]
    );
    assert!(
        bodies[0].contains("/deploy staging"),
        "the user's literal message is preserved: {}",
        bodies[0]
    );
}

/// Denying the request rejects the submission before any message row or model
/// call exists, so the client can put the draft back where it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn denying_an_explicit_untrusted_skill_opens_no_turn() {
    let (_tmp, server, client, session) =
        client_with(Vec::new(), PermissionProfile::Assisted).await;
    let mut rx = client.subscribe();
    let sender = submit(&client, session.clone(), "/deploy staging");

    let request = next_trust_request(&mut rx).await;
    client
        .send(ClientCommand::DefinitionTrustDecision {
            request_id: request.id,
            decision: DefinitionTrustDecision::Deny,
            session_id: Some(session.clone()),
        })
        .await
        .unwrap();

    let result = sender.await.unwrap();
    assert!(
        result.is_err(),
        "a denied request is a rejected submission, not a silent turn"
    );
    assert!(
        server.request_bodies().await.is_empty(),
        "no model request may be made for a denied package"
    );
}

/// FullAccess never adds an approval dialog: the pre-existing package is part
/// of the session baseline, so the turn runs directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_access_delivers_without_a_trust_prompt() {
    let (_tmp, server, client, session) =
        client_with(vec![text("DEPLOY-DONE")], PermissionProfile::FullAccess).await;
    let mut rx = client.subscribe();
    let sender = submit(&client, session.clone(), "/deploy staging");
    sender.await.unwrap().expect("Full needs no approval");

    let bodies = wait_for_requests(&server, 1).await;
    assert!(
        bodies[0].contains("DEPLOY-BODY-MARKER"),
        "Full adopts the session baseline: {}",
        bodies[0]
    );
    // Anything already queued is fine; an approval request is not.
    while let Ok(event) = rx.try_recv() {
        assert!(
            !matches!(event, RuntimeEvent::DefinitionTrustRequested { .. }),
            "FullAccess must not ask"
        );
    }
}
