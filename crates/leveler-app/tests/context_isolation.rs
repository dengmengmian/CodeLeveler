//! `/context` is a local inspection command: it answers from the runtime's
//! cached accounting and must never reach the model request. This drives two
//! real turns through the in-process client against a recording mock server,
//! asks for the context in between, and reads the exact bodies the provider
//! received.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_core::{CommandId, SessionId};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::MessageRepository;
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

fn text(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

async fn harness(
    responses: Vec<MockResponse>,
) -> (
    tempfile::TempDir,
    MockServer,
    Arc<Application>,
    Arc<InProcessRuntimeClient>,
    SessionId,
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
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    (tmp, server, app, client, session)
}

/// Drain events until the turn reaches a terminal state.
async fn wait_for_turn_end(rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(event)) => match event {
                RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnCompletedWithWarnings { .. }
                | RuntimeEvent::TurnAnswered
                | RuntimeEvent::TurnTruncated { .. }
                | RuntimeEvent::TurnIncomplete { .. }
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnCancelled => return,
                _ => continue,
            },
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the turn never settled"),
        }
    }
}

/// Ask for `/context` and read the answer the runtime cached from the last
/// request it assembled.
async fn query_context(
    client: &Arc<InProcessRuntimeClient>,
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    session: &SessionId,
) -> leveler_model::ContextAccounting {
    let query_id = CommandId::generate();
    client
        .send(ClientCommand::QueryContext {
            session_id: session.clone(),
            query_id: Some(query_id.clone()),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::ContextLoaded {
                accounting: Some(accounting),
                ..
            })) => return accounting,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the context query was never answered"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_inspection_is_local_and_never_enters_the_conversation() {
    let (_tmp, server, app, client, session) = harness(vec![text("first"), text("second")]).await;
    let mut rx = client.subscribe();

    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "first question".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_turn_end(&mut rx).await;

    // The runtime published the accounting of the request it just sent.
    let accounting = query_context(&client, &mut rx, &session).await;
    assert!(
        accounting.used_tokens > 0,
        "an assembled request must account for something"
    );
    assert_eq!(accounting.context_window_tokens, Some(8192));

    // `/context` produced no model request at all.
    assert_eq!(
        server.request_count(),
        1,
        "the inspection must not call the provider"
    );

    // And it left no mark on the conversation the next turn will send.
    let db = app.open_database().await.unwrap();
    let messages_after_query = MessageRepository::new(&db).count(&session).await.unwrap();

    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "second question".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_turn_end(&mut rx).await;

    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), 2, "exactly one model request per turn");
    for body in &bodies {
        for marker in [
            "/context",
            "Context Inspector",
            "Breakdown",
            "Compact at",
            "Last compact",
            "●",
            "○",
        ] {
            assert!(
                !body.contains(marker),
                "inspection output leaked into the provider request: {marker:?}\n{body}"
            );
        }
    }
    // The second request is the conversation the user actually had.
    assert!(bodies[1].contains("first question"));
    assert!(bodies[1].contains("second question"));

    let messages_after_turn = MessageRepository::new(&db).count(&session).await.unwrap();
    assert_eq!(
        messages_after_query + 2,
        messages_after_turn,
        "the second turn adds its user + assistant message; the query added none"
    );
}
