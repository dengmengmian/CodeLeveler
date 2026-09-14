//! A client stops one running child through the interactive runtime, the way
//! the TUI, Web and phone do: `ClientCommand::CancelChild` while the turn runs.
//! The child must settle as cancelled; its parent turn keeps going.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ChildStop, ClientCommand, InteractiveRuntimeClient, NotificationLevel, RuntimeEvent,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

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

fn spawn_call(background: bool) -> MockResponse {
    let arguments = serde_json::json!({
        "role": "explorer",
        "task": "read everything slowly",
        "run_in_background": background,
    });
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": "call-spawn",
            "function": {"name": "spawn_agent", "arguments": arguments.to_string()}
        }]}}]})
        .to_string(),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string(),
    ])
}

fn text(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

async fn client_with(
    responses: Vec<MockResponse>,
) -> (
    tempfile::TempDir,
    MockServer,
    Arc<InProcessRuntimeClient>,
    leveler_core::SessionId,
) {
    isolate_global_config();
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
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
limits: { context_window: 8192, reliable_context: 4096, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "goal").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app,
        model,
        PermissionProfile::Assisted,
        false,
    ));
    (tmp, server, client, session)
}

async fn next_child_event(
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    done: bool,
) -> Option<(String, Option<ChildStop>)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::SubAgentUpdated {
                id, done: d, stop, ..
            })) if d == done => {
                return Some((id, stop));
            }
            Ok(Ok(RuntimeEvent::Notification {
                level: NotificationLevel::Error,
                message,
            })) => {
                panic!("runtime reported an error: {message}");
            }
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return None,
        }
    }
}

/// Foreground only. A background child's terminal is settled at its parent's
/// next round boundary, and this mock serves responses in arrival order, so
/// whether the parent or the child gets the silent response is a race: a
/// background variant tests the mock, not the product (it failed that way on
/// one CI host). The background path is pinned by
/// `a_host_can_cancel_one_child_without_cancelling_its_parent` in
/// leveler-agent and was accepted end to end from a phone (MA5 §4).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_stops_a_running_foreground_child() {
    let (_tmp, _server, client, session) = client_with(vec![
        spawn_call(false),
        // The child's first model call: silent long enough that only a
        // cancel can end it inside the test's deadline.
        MockResponse::SilentThenJson {
            silent_ms: 60_000,
            body: "{}".into(),
        },
        text("the child was stopped"),
        text("the child was stopped"),
    ])
    .await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "delegate".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    // Cancellable from the moment it is seen: the host holds the child's
    // handle before its start is observed.
    let (child_id, _) = next_child_event(&mut rx, false)
        .await
        .expect("a child starts");
    client
        .send(ClientCommand::CancelChild {
            session_id: session.clone(),
            child_id: child_id.clone(),
        })
        .await
        .unwrap();

    let (settled, stop) = next_child_event(&mut rx, true)
        .await
        .expect("the cancelled child settles well before its model call would return");
    assert_eq!(settled, child_id);
    assert_eq!(stop, Some(ChildStop::Cancelled));

    let _ = client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: session,
        })
        .await;
}

fn tool_call(name: &str, arguments: serde_json::Value) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": "call-tool",
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]})
        .to_string(),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string(),
    ])
}

/// The same host carries a user's mid-turn text. Sent while a chat turn
/// runs, it must reach the conversation, not wait in a queue nobody drains.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_sent_during_a_chat_turn_reaches_the_conversation() {
    let (_tmp, _server, client, session) = client_with(vec![
        tool_call("list_files", serde_json::json!({"path": "."})),
        text("noted"),
        text("noted"),
    ])
    .await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "look around".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    client
        .send(ClientCommand::SteerCurrentTurn {
            session_id: session.clone(),
            content: "also keep the API stable".into(),
        })
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(left, rx.recv())
            .await
            .expect("the turn ends")
            .expect("event stream open");
        let kind = serde_json::to_value(&event).unwrap()["type"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if kind.starts_with("turn_") && kind != "turn_progress" && kind != "turn_started" {
            break;
        }
    }

    client
        .send(ClientCommand::OpenSession {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    let snapshot = loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Ok(RuntimeEvent::SessionOpened { session })) => break session,
            Ok(Ok(_)) => continue,
            other => panic!("no snapshot: {other:?}"),
        }
    };
    assert!(
        snapshot
            .messages
            .iter()
            .any(|m| m.text.contains("also keep the API stable")),
        "the steer never reached the conversation"
    );
}
