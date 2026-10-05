//! An image the user attached either reaches the model or is refused out loud.
//!
//! The one outcome that must never happen is the quiet one: the turn goes to
//! the model carrying only the text, and the model answers a question about a
//! picture it was never shown. Both ways that can happen are covered here —
//! a model that cannot read images at all, and bytes that cannot be read back
//! out of the media store.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    AttachmentId, AttachmentKind, AttachmentRef, ClientCommand, InteractiveRuntimeClient,
    NotificationLevel, RuntimeEvent,
};
use leveler_core::SessionId;
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;

/// Point `LEVELER_HOME` at an empty dir so the developer's own
/// `~/.leveler/config.toml` cannot decide what these tests see.
fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn model_yaml(id: &str, vision: bool) -> String {
    format!(
        "id: {id}\nprovider: mock\nmodel_id: mock-model\nprotocol: openai_chat\n\
         capabilities:\n  streaming: true\n  tool_calling: true\n  \
         parallel_tool_calls: false\n  structured_output: true\n  reasoning: false\n  \
         vision: {vision}\nlimits:\n  context_window: 131072\n  reliable_context: 65536\n  \
         max_output_tokens: 1024\n  max_tool_schema_bytes: 8192\n  \
         max_parallel_tool_calls: 1\ncompatibility:\n  synthesize_tool_call_ids: true\n  \
         drop_unsupported_fields: true\n"
    )
}

/// A runtime whose provider is unreachable: these tests observe the decision
/// taken before the request is built, never a model answer.
async fn build_client(
    model_id: &str,
    vision: bool,
) -> (tempfile::TempDir, Arc<InProcessRuntimeClient>, SessionId) {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.join("configs/models")).unwrap();
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        "id: mock\nprotocol: openai_chat\nbase_url: http://127.0.0.1:9\n\
         timeouts:\n  connect_seconds: 5\n  request_seconds: 30\n  idle_stream_seconds: 30\n\
         retry:\n  max_attempts: 1\n  initial_backoff_ms: 10\n  max_backoff_ms: 10\n",
    )
    .unwrap();
    std::fs::write(
        root.join(format!("configs/models/{model_id}.yaml")),
        model_yaml(model_id, vision),
    )
    .unwrap();
    let layout = Layout::from_parts(root.to_path_buf(), root.join("configs"), root.join("state"));
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::new("mock", model_id);
    let session_id = app.create_session(&model, "chat").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app,
        model,
        PermissionProfile::Assisted,
        false,
    ));
    (tmp, client, session_id)
}

/// An attachment pointing at bytes that are not in the media store.
fn attachment(sha256: &str) -> AttachmentRef {
    AttachmentRef {
        id: AttachmentId::new("att-1"),
        kind: AttachmentKind::Image,
        name: "clipboard.png".to_string(),
        mime_type: "image/png".to_string(),
        size_bytes: 1024,
        sha256: sha256.to_string(),
        width: Some(10),
        height: Some(10),
    }
}

async fn next_error(rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) -> Option<String> {
    loop {
        match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            Ok(Ok(RuntimeEvent::Notification {
                level: NotificationLevel::Error,
                message,
            })) => return Some(message),
            Ok(Ok(_)) => continue,
            _ => return None,
        }
    }
}

/// The TUI blocks this in its own reducer, but the reducer is one client. A
/// headless run, the web client, or a model switched between staging and
/// sending all reach the runtime, and the runtime is where the answer has to
/// be the same: refuse, and say why.
#[tokio::test]
async fn an_image_for_a_model_that_cannot_see_is_refused_not_dropped() {
    let (_tmp, client, session_id) = build_client("blind", false).await;
    let mut rx = client.subscribe();

    let error = client
        .send(ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: "这张图里写了什么？".to_string(),
            attachments: vec![attachment("deadbeef")],
        })
        .await
        .expect_err("a model without vision must refuse the image");

    let text = error.to_string();
    assert!(
        text.contains("blind") && text.contains("图"),
        "the refusal must name the model and say it is about images: {text}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "a refused submit must not start a turn"
    );
}

/// The image is staged, the model can see, and then the bytes cannot be read
/// back. Sending the text alone would let the model guess at a picture it was
/// never given — the turn fails instead.
#[tokio::test]
async fn an_image_whose_bytes_are_gone_fails_the_turn() {
    let (_tmp, client, session_id) = build_client("seeing", true).await;
    let mut rx = client.subscribe();

    let _ = client
        .send(ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: "这张图里写了什么？".to_string(),
            attachments: vec![attachment(&"a".repeat(64))],
        })
        .await;

    let message = next_error(&mut rx).await.expect("an error notification");
    assert!(
        message.contains("附件"),
        "the failure must name the attachment: {message}"
    );
}

/// Real app ingress must retain Goal authority even when the user sends media.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_image_keeps_its_goal_identity_through_submit_and_continue() {
    use leveler_agent::CollaborationMode;
    use leveler_model::{ContentPart, ImageSource};
    use leveler_storage::{GoalCheckpointStore, GoalStore, TurnRepository};
    use leveler_test_support::{MockResponse, MockServer};

    isolate_global_config();
    let quiet = || {
        MockResponse::Sse {
        body: "data: {\"choices\":[{\"delta\":{\"content\":\"I have examined the image.\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into(),
    }
    };
    let completed = serde_json::json!({"choices":[{"delta":{"tool_calls":[{
        "index":0,"id":"finish-goal","function":{"name":"update_goal",
        "arguments":serde_json::json!({"status":"complete","summary":"image task complete"}).to_string()}
    }]},"finish_reason":"tool_calls"}]}).to_string();
    let server = MockServer::start(vec![
        quiet(),
        quiet(),
        // The Goal continuation bound (ProgressCaps::default) buys two rounds;
        // a third quiet round is the stall the test then resumes from.
        quiet(),
        MockResponse::Sse {
            body: format!("data: {completed}\n\ndata: [DONE]\n\n"),
        },
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.join("configs/models")).unwrap();
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        format!(
            "id: mock\nprotocol: openai_chat\nbase_url: {}\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("configs/models/seeing.yaml"),
        model_yaml("seeing", true),
    )
    .unwrap();
    let app = Arc::new(
        Application::assemble(Layout::from_parts(
            root.to_path_buf(),
            root.join("configs"),
            root.join("state"),
        ))
        .unwrap()
        .with_collaboration(CollaborationMode::Goal),
    );
    let model = ModelRef::new("mock", "seeing");
    let session = app
        .create_session(&model, "inspect the supplied image")
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    let mut rx = client.subscribe();
    client.send(ClientCommand::AddAttachmentData {
        session_id: session.clone(), name: "pixel.png".into(),
        data_base64: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4//8/AAX+Av4N70a4AAAAAElFTkSuQmCC".into(),
    }).await.unwrap();
    let attachment = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await.unwrap() {
                RuntimeEvent::AttachmentAdded { attachment, .. } => break attachment,
                RuntimeEvent::AttachmentProcessingFailed { error, .. } => panic!("{error}"),
                RuntimeEvent::Notification {
                    level: NotificationLevel::Error,
                    message,
                } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "inspect the supplied image".into(),
            attachments: vec![attachment],
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match rx.recv().await.unwrap() {
                RuntimeEvent::TurnIncomplete { .. } => break,
                RuntimeEvent::TurnAnswered | RuntimeEvent::TurnCompleted => {
                    panic!("Goal text alone cannot resolve the task")
                }
                RuntimeEvent::Notification {
                    level: NotificationLevel::Error,
                    message,
                } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let db = app.open_database().await.unwrap();
    let goals = GoalStore::unfinished(&db).await.unwrap();
    assert_eq!(goals.len(), 1);
    assert!(
        !GoalCheckpointStore::for_goal(&db, &goals[0].id)
            .await
            .unwrap()
            .is_empty()
    );
    let turns = TurnRepository::new(&db).list(&session).await.unwrap();
    assert_eq!(turns.len(), 1);
    let continuation =
        leveler_engine::decode_turn_continuation(turns[0].payload.as_deref().unwrap()).unwrap();
    assert_eq!(continuation.goal_id.as_ref(), Some(&goals[0].id));
    let original = continuation.initiating_message.unwrap();
    let image_url = original
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::Image {
                source: ImageSource::Base64 { media_type, data },
            } => Some(format!("data:{media_type};base64,{data}")),
            _ => None,
        })
        .expect("WAL must contain original image bytes");
    let requests = server.request_bodies().await;
    assert_eq!(
        requests.len(),
        3,
        "two Goal continuations plus the round that proves the bound is spent"
    );
    let first: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
    assert!(
        first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "update_goal")
    );
    assert!(first["messages"].as_array().unwrap().iter().any(|message| {
        message["content"].as_array().is_some_and(|parts| {
            parts
                .iter()
                .any(|part| part["image_url"]["url"] == image_url)
        })
    }));
    client
        .send(ClientCommand::ResumeTask {
            session_id: session.clone(),
            content: "继续".into(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match rx.recv().await.unwrap() {
                RuntimeEvent::TurnCompleted => break,
                RuntimeEvent::TurnAnswered | RuntimeEvent::TurnIncomplete { .. } => {
                    panic!("explicit update_goal must resolve the resumed goal")
                }
                RuntimeEvent::Notification {
                    level: NotificationLevel::Error,
                    message,
                } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(GoalStore::unfinished(&db).await.unwrap().is_empty());
    let turns = TurnRepository::new(&db).list(&session).await.unwrap();
    assert_eq!(turns.len(), 2);
    let resumed =
        leveler_engine::decode_turn_continuation(turns[1].payload.as_deref().unwrap()).unwrap();
    assert_eq!(resumed.goal_id.as_ref(), Some(&goals[0].id));
    assert_eq!(server.request_count(), 4);
}

/// Upload acknowledgement is not the import result. Observe the authoritative
/// event, then read the stored bytes back through the existing service seam.
#[tokio::test]
async fn uploaded_generic_file_is_stored_and_reported_without_claiming_model_reading() {
    use leveler_client_protocol::{CommandEnvelope, CommandId};
    use leveler_local_transport::LocalRuntimeService;
    let (_tmp, client, session_id) = build_client("blind-upload", false).await;
    let mut events = client.subscribe_session(&session_id);
    let envelope = CommandEnvelope {
        command_id: CommandId::new("generic-upload-1"),
        session_id: session_id.clone(),
        expected_version: None,
        issued_at: leveler_core::now().to_rfc3339(),
        command: ClientCommand::AddAttachmentData {
            session_id: session_id.clone(),
            name: "note.txt".into(),
            data_base64: "aGVsbG8=".into(),
        },
    };
    client.deliver(envelope.clone()).await.unwrap();
    let attachment = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.unwrap();
            match &event {
                RuntimeEvent::AttachmentAdded { attachment, .. } => {
                    assert_eq!(
                        serde_json::to_value(&event).unwrap()["command_id"],
                        "generic-upload-1"
                    );
                    break attachment.clone();
                }
                RuntimeEvent::AttachmentProcessingFailed { error, .. } => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("the runtime must report an actual stored upload");
    assert_eq!(attachment.kind, AttachmentKind::TextFile);
    assert_eq!(attachment.name, "note.txt");
    assert_eq!(attachment.mime_type, "text/plain");
    assert_eq!(attachment.size_bytes, 5);
    let stored = client.fetch_attachment(&attachment.sha256).await.unwrap();
    assert_eq!(stored.bytes, b"hello");
    assert_eq!(stored.mime_type, "text/plain");
    // A completed receipt acknowledges replay without re-importing or changing
    // the original object. It does not promise to replay the result event.
    client.deliver(envelope.clone()).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), events.recv())
            .await
            .is_err()
    );
    let mut conflict = envelope;
    if let ClientCommand::AddAttachmentData { data_base64, .. } = &mut conflict.command {
        *data_base64 = "bmV3".into();
    }
    assert!(client.deliver(conflict).await.is_err());
}

#[tokio::test]
async fn uploaded_file_failure_reports_original_command_id() {
    use leveler_client_protocol::{CommandEnvelope, CommandId};
    let (_tmp, client, session_id) = build_client("blind-upload-failure", false).await;
    let mut events = client.subscribe_session(&session_id);
    let mut envelope = CommandEnvelope {
        command_id: CommandId::new("failed-upload-1"),
        session_id: session_id.clone(),
        expected_version: None,
        issued_at: leveler_core::now().to_rfc3339(),
        command: ClientCommand::AddAttachmentData {
            session_id,
            name: "invalid.png".into(),
            data_base64: "***invalid***".into(),
        },
    };
    let mut wrong_session = envelope.clone();
    wrong_session.session_id = SessionId::new("foreign-session");
    assert!(client.deliver(wrong_session).await.is_err());
    client.deliver(envelope.clone()).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event, RuntimeEvent::AttachmentProcessingFailed { .. }) {
                break event;
            }
            assert!(!matches!(event, RuntimeEvent::AttachmentAdded { .. }));
        }
    })
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(&event).unwrap()["command_id"],
        "failed-upload-1"
    );
    // Raw send remains compatible and explicitly has no envelope correlation.
    envelope.command_id = CommandId::new("not-used-by-raw-send");
    client.send(envelope.command).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event, RuntimeEvent::AttachmentProcessingFailed { .. }) {
                break event;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        serde_json::to_value(event)
            .unwrap()
            .get("command_id")
            .is_none()
    );
}
