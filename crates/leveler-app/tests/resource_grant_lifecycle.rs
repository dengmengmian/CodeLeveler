//! Windows resource-grant lifecycle through the real interactive surface.
//!
//! The grant is created by an `ApproveSession` decision in `Assisted` mode,
//! used by the approved write, then revoked through the same store the CLI
//! management surface uses. After revocation the same call asks again and a
//! Deny is honored. Nothing here injects a grant into the policy directly:
//! the approval, the persistence and the re-verification all run through the
//! runtime the TUI talks to.
//!
//! The write itself atomically replaces the file, so its incarnation changes.
//! A later call therefore has to ask again even with a session grant: the
//! execution layer refuses a resource whose object no longer matches the one
//! the user approved (the same fail-closed rule as Unix).

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ApprovalDecision, ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiApprovalRequest,
};
use leveler_core::{Capability, GrantScope, ResourceIdentity};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::ResourceGrantStore;
use leveler_test_support::{MockResponse, MockServer};
use tokio::sync::broadcast;

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn sse(frames: &[String]) -> MockResponse {
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(frame);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    MockResponse::Sse { body }
}

fn text(content: &str) -> MockResponse {
    sse(&[serde_json::json!({
        "choices": [{"delta": {"content": content}, "finish_reason": "stop"}]
    })
    .to_string()])
}

fn patch_call(id: &str, path: &std::path::Path, old: &str, new: &str) -> MockResponse {
    let path = path.to_string_lossy().replace('\\', "/");
    let patch =
        format!("*** Begin Patch\n*** Update File: {path}\n@@\n-{old}\n+{new}\n*** End Patch");
    sse(&[
        serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": {"name": "apply_patch", "arguments": serde_json::json!({"patch": patch}).to_string()},
            }]}}]
        })
        .to_string(),
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
            .to_string(),
    ])
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
  max_tool_schema_bytes: 65536
  max_parallel_tool_calls: 1
compatibility:
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#,
    )
    .unwrap();
}

async fn await_approval(events: &mut broadcast::Receiver<RuntimeEvent>) -> UiApprovalRequest {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::ApprovalRequested { request }) => return request,
                Ok(RuntimeEvent::TurnFailed { error, .. }) => {
                    panic!("turn failed before asking: {error}")
                }
                Ok(_) => continue,
                Err(error) => panic!("session event stream closed: {error}"),
            }
        }
    })
    .await
    .expect("the runtime should reach an approval within 30s")
}

async fn await_turn_end(events: &mut broadcast::Receiver<RuntimeEvent>, server: &MockServer) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::TurnCompleted)
                | Ok(RuntimeEvent::TurnCompletedWithWarnings { .. })
                | Ok(RuntimeEvent::TurnAnswered)
                | Ok(RuntimeEvent::TurnTruncated { .. })
                | Ok(RuntimeEvent::TurnIncomplete { .. }) => return,
                Ok(RuntimeEvent::TurnFailed { error, .. }) => panic!("turn failed: {error}"),
                Ok(RuntimeEvent::TurnCancelled) => panic!("turn cancelled"),
                Ok(RuntimeEvent::ApprovalRequested { request }) => {
                    let bodies = server.request_bodies().await;
                    panic!(
                        "unexpected second approval: {request:?}\nrequests:\n{}",
                        bodies.join("\n---\n")
                    )
                }
                Ok(_) => continue,
                Err(error) => panic!("session event stream closed: {error}"),
            }
        }
    })
    .await
    .expect("the turn should settle within 30s");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_grant_is_persisted_then_revocation_makes_the_next_call_ask_again() {
    // The identity gate refuses an inherited Git override. This harness runs
    // under a shell that exports GIT_EDITOR; the product on a normal shell
    // does not, and the E2E test clears it so the reusable path is exercised.
    unsafe {
        std::env::remove_var("GIT_EDITOR");
    }
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("approved.txt");
    std::fs::write(&target, "original\n").unwrap();

    let responses = vec![
        patch_call("call_one", &target, "original", "first"),
        text("第一次修改完成。"),
        patch_call("call_two", &target, "first", "denied"),
        text("第二次修改被拒绝。"),
    ];
    let server = MockServer::start(responses).await;
    let repo = tempfile::tempdir().unwrap();
    // A non-repository workspace still has a project identity: the canonical
    // directory and its object incarnation.
    write_config(repo.path(), &server.base_url());
    let layout = Layout::from_parts(
        repo.path().to_path_buf(),
        repo.path().join("configs"),
        repo.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "修改外部文件").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    let mut events = client.subscribe_session(&session);

    let submit =
        |client: Arc<InProcessRuntimeClient>, session: leveler_core::SessionId, text: &str| {
            let content = text.to_string();
            async move {
                client
                    .send(ClientCommand::SubmitMessage {
                        session_id: session,
                        content,
                        attachments: vec![],
                    })
                    .await
                    .unwrap();
            }
        };

    // First call: the approval request must carry the exact filesystem
    // binding, and ApproveSession persists it through the runtime.
    submit(client.clone(), session.clone(), "第一次修改").await;
    let request = await_approval(&mut events).await;
    let grant = request
        .grant
        .as_ref()
        .unwrap_or_else(|| panic!("a reusable resource request: {request:?}"));
    let binding = grant
        .bindings
        .iter()
        .find(|binding| binding.capability == Capability::FilesystemWrite)
        .expect("a write binding");
    let ResourceIdentity::FilesystemPath {
        canonical_path,
        object_identity,
    } = &binding.resource
    else {
        panic!("expected a filesystem resource, got {:?}", binding.resource);
    };
    assert_eq!(
        std::path::Path::new(canonical_path),
        target.canonicalize().unwrap()
    );
    #[cfg(windows)]
    assert!(
        object_identity.starts_with("windows:"),
        "the identity must come from the Windows object, not a path: {object_identity}"
    );
    #[cfg(not(windows))]
    assert!(
        object_identity.starts_with("unix:"),
        "the identity must come from the Unix object, not a path: {object_identity}"
    );
    client
        .send(ClientCommand::ApprovalDecision {
            request_id: request.id.clone(),
            decision: ApprovalDecision::ApproveSession,
        })
        .await
        .unwrap();
    await_turn_end(&mut events, &server).await;
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "first\n");

    let db = app.open_database().await.unwrap();
    let grants = ResourceGrantStore::list(&db, &grant.project_identity, None)
        .await
        .unwrap();
    // The call needs read and write; each exact capability is its own row.
    assert_eq!(grants.len(), 2, "{grants:?}");
    let stored = grants
        .iter()
        .find(|stored| stored.binding == *binding)
        .unwrap_or_else(|| panic!("the write binding must be persisted: {grants:?}"));
    assert_eq!(stored.scope, GrantScope::Session);
    let mut grant_ids: Vec<String> = grants.iter().map(|grant| grant.id.clone()).collect();

    // Revoke through the same store the CLI management surface uses.
    for id in grant_ids.drain(..) {
        assert!(
            ResourceGrantStore::revoke(&db, &grant.project_identity, &id)
                .await
                .unwrap()
        );
    }
    assert!(
        ResourceGrantStore::list(&db, &grant.project_identity, None)
            .await
            .unwrap()
            .is_empty()
    );

    // The granted object was replaced by the approved write, so the next call
    // re-resolves a different incarnation. Revocation is therefore not the
    // only reason this call asks again, but it must still ask and the Deny
    // must be honored.
    submit(client.clone(), session.clone(), "第二次修改").await;
    let request = await_approval(&mut events).await;
    assert!(
        request.grant.is_some(),
        "the call must still offer exact resource consent: {request:?}"
    );
    client
        .send(ClientCommand::ApprovalDecision {
            request_id: request.id,
            decision: ApprovalDecision::Deny,
        })
        .await
        .unwrap();
    await_turn_end(&mut events, &server).await;
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "first\n");
}
