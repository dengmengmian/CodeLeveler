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
         vision: {vision}\nlimits:\n  context_window: 8192\n  reliable_context: 4096\n  \
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
