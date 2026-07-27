//! What happens to an approval nobody answered before the runtime died.
//!
//! This is the case the remote design left open: a phone shows an approval
//! card, the developer's machine restarts, and the phone reconnects and asks
//! for a snapshot. Whatever that snapshot says is what the user sees, so the
//! answer had better be a decision rather than an accident.
//!
//! It is decided here rather than in the remote crates because there is nothing
//! remote about it — a TUI reattaching after a restart asks the same question.
//! The remote tests use a fake runtime, and a fake runtime asked about crash
//! recovery only reports what the fake was written to say.

use std::sync::Arc;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiPendingInteraction,
};
use leveler_core::SessionId;
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

/// Point `LEVELER_HOME` at an empty dir so `GlobalConfig::load()` yields the
/// default. Tests must not depend on the developer's `~/.leveler/config.toml`.
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
  context_window: 8192
  reliable_context: 4096
  max_output_tokens: 1024
  max_tool_schema_bytes: 8192
  max_parallel_tool_calls: 1
compatibility:
  middleware: []
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#,
    )
    .unwrap();
}

fn tool_call_sse(name: &str, arguments: serde_json::Value) -> MockResponse {
    let call = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": "call_1",
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()},
            }]},
        }]
    })
    .to_string();
    let finish =
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
            .to_string();
    MockResponse::sse(&[&call, &finish])
}

/// Drive a session until it is waiting for a human to approve something.
async fn session_awaiting_approval(
    root: &std::path::Path,
    base_url: &str,
) -> (Arc<Application>, Arc<InProcessRuntimeClient>, SessionId) {
    write_config(root, base_url);
    let layout = Layout {
        repo_root: root.to_path_buf(),
        config_dir: root.join("configs"),
        state_dir: root.join("state"),
    };
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::new("mock", "m");
    let session_id = app.create_session(&model, "delete a file").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::RequestApproval,
        false,
    ));

    let mut events = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: "请删除 scratch.txt".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();

    // Wait for the turn to reach the question, not merely to start.
    let waited = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::ApprovalRequested { request }) => return Some(request),
                Ok(RuntimeEvent::TurnCompleted) | Ok(RuntimeEvent::TurnFailed { .. }) => {
                    return None;
                }
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    })
    .await
    .expect("the runtime should reach an approval within 30s");
    assert!(
        waited.is_some(),
        "the turn finished without asking; the mock did not produce a tool call needing approval"
    );

    (app, client, session_id)
}

#[tokio::test]
async fn an_unanswered_approval_does_not_survive_a_restart() {
    let server = MockServer::start_one(tool_call_sse(
        "run_command",
        serde_json::json!({"program": "rm", "args": ["scratch.txt"], "reason": "清理"}),
    ))
    .await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("scratch.txt"), "x").unwrap();

    let (app, client, session_id) = session_awaiting_approval(tmp.path(), &server.base_url()).await;

    // Before the restart: the snapshot a client renders really does carry it.
    let before = client.snapshot(&session_id).await.unwrap();
    assert!(
        before
            .pending_interactions
            .iter()
            .any(|item| matches!(item, UiPendingInteraction::Approval(_))),
        "the running runtime should report the approval it is waiting on"
    );

    // The restart. Dropping the client is what a process death does to the
    // in-memory table of who is waiting for what; the state directory is all
    // that carries over.
    drop(client);
    drop(app);

    let layout = Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: tmp.path().join("state"),
    };
    let app = Arc::new(Application::assemble(layout).unwrap());
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::RequestApproval,
        false,
    ));

    let after = client.snapshot(&session_id).await.unwrap();

    // The decision this test records: a pending approval is **not** rebuilt.
    //
    // It could not be answered if it were. The approval is a question asked by
    // a turn that no longer exists — the future awaiting the answer died with
    // the process — so an "Allow" tapped afterwards would resolve nothing and
    // the phone would wait forever on a button that does something the first
    // time and nothing after. Showing no approval is the honest state: the tool
    // call was interrupted, and interrupted work is recovered deliberately
    // (`run --resume --confirm-recovery`), not by answering a dead prompt.
    assert!(
        after
            .pending_interactions
            .iter()
            .all(|item| !matches!(item, UiPendingInteraction::Approval(_))),
        "a restarted runtime must not offer an approval it cannot resolve: {:?}",
        after.pending_interactions
    );

    // And the transcript still holds what led there, so the session is
    // resumable rather than mysteriously empty.
    assert!(
        !after.messages.is_empty(),
        "the conversation before the restart should still be there"
    );

    // The command was never approved, so it must never have run.
    assert!(
        tmp.path().join("scratch.txt").exists(),
        "an approval that was never given must not have executed anything"
    );
}
