//! Regression: database failures on user-visible commands must surface as an
//! error notification, never silently report success (fake rollback / delete).

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, NotificationLevel, RuntimeEvent,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;

/// A client whose state dir is blocked by a FILE, so every database open fails.
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

fn broken_db_client() -> (tempfile::TempDir, Arc<dyn InteractiveRuntimeClient>) {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    std::fs::write(&state, "not a directory").unwrap();
    let layout = Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: state,
    };
    let app = Arc::new(Application::assemble(layout).expect("assemble with empty config"));
    let model = ModelRef::parse("deepseek/test-model").unwrap();
    let client: Arc<dyn InteractiveRuntimeClient> = Arc::new(InProcessRuntimeClient::new(
        app,
        model,
        PermissionProfile::RequestApproval,
        false,
    ));
    (tmp, client)
}

async fn expect_error_notification(
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
) -> Option<String> {
    loop {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Ok(RuntimeEvent::Notification {
                level: NotificationLevel::Error,
                message,
            })) => return Some(message),
            Ok(Ok(_)) => continue,
            _ => return None,
        }
    }
}

#[tokio::test]
async fn delete_session_db_failure_notifies_the_ui() {
    let (_tmp, client) = broken_db_client();
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::DeleteSession {
            session_id: leveler_client_protocol::SessionId::new("s1"),
        })
        .await
        .unwrap();
    let msg = expect_error_notification(&mut rx).await;
    assert!(
        msg.is_some(),
        "a failed delete must produce an error notification"
    );
}

#[tokio::test]
async fn clear_conversation_db_failure_notifies_the_ui() {
    let (_tmp, client) = broken_db_client();
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::ClearConversation {
            session_id: leveler_client_protocol::SessionId::new("s1"),
        })
        .await
        .unwrap();
    let msg = expect_error_notification(&mut rx).await;
    assert!(
        msg.is_some(),
        "a failed clear must produce an error notification"
    );
}

#[tokio::test]
async fn compact_db_failure_is_not_silent() {
    let (_tmp, client) = broken_db_client();
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::CompactContext {
            session_id: leveler_client_protocol::SessionId::new("s1"),
        })
        .await
        .unwrap();
    // compact is async on a blocking pool — wait for failure surface.
    let mut saw_fail = false;
    for _ in 0..40 {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Ok(RuntimeEvent::TurnFailed { .. })) => {
                saw_fail = true;
                break;
            }
            Ok(Ok(RuntimeEvent::Notification {
                level: NotificationLevel::Error,
                ..
            })) => {
                saw_fail = true;
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    assert!(
        saw_fail,
        "compact must surface DB open failure (TurnFailed or Error notification)"
    );
}

#[tokio::test]
async fn context_ops_reject_while_turn_is_active() {
    // Real session: submit admits a turn; concurrent clear/compact must refuse.
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/models")).unwrap();
    std::fs::write(
        tmp.path().join("configs/providers/mock.yaml"),
        "id: mock\nprotocol: openai_chat\nbase_url: http://127.0.0.1:9\n",
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
limits: { context_window: 8192, reliable_context: 4096, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { middleware: [], synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
    let layout = Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: tmp.path().join("state"),
    };
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::new("mock", "m");
    let session_id = app.create_session(&model, "goal").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app,
        model,
        PermissionProfile::Assisted,
        false,
    ));

    // Submit starts a background turn (unreachable provider → hang until cancel).
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: "hold the turn".into(),
            attachments: vec![],
        })
        .await
        .expect("submit must admit");

    let clear_err = client
        .send(ClientCommand::ClearConversation {
            session_id: session_id.clone(),
        })
        .await;
    assert!(
        matches!(clear_err, Err(leveler_client_protocol::ClientError::Runtime(ref msg)) if msg.contains("进行中")),
        "clear must refuse during active turn: {clear_err:?}"
    );

    let compact_err = client
        .send(ClientCommand::CompactContext {
            session_id: session_id.clone(),
        })
        .await;
    assert!(
        matches!(compact_err, Err(leveler_client_protocol::ClientError::Runtime(ref msg)) if msg.contains("进行中")),
        "compact must refuse during active turn: {compact_err:?}"
    );

    // Release the turn so the test process does not leak work.
    let _ = client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: session_id.clone(),
        })
        .await;
}
