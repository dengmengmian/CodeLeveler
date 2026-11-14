//! Memory rollout `off`, driven through the real in-process runtime.
//!
//! This lives in its own test binary because `LEVELER_HOME` is process-wide
//! and the enabled suite installs an `active` rollout. The contract under test:
//! while memory is off, a turn behaves exactly as before memory existed — no
//! prompt context, no capture, no store directory — and every memory action
//! reports that it is disabled. A user's explicit `/remember` is the one
//! documented exception: it persists the note so a later opt-in can expose it.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiMemoryActionResult,
};
use leveler_core::SessionId;
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

fn install_config() {
    use std::sync::OnceLock;
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            r#"
[assist]
prompt_suggestions = false
away_summary = false

[memory]
rollout = "off"
"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("LEVELER_HOME", dir.path());
        }
        dir
    });
    let _ = dir;
}

fn text_response(content: &str) -> MockResponse {
    let frame = serde_json::json!({
        "choices": [{"delta": {"content": content}, "finish_reason": "stop"}]
    })
    .to_string();
    MockResponse::sse(&[&frame])
}

struct Harness {
    root: tempfile::TempDir,
    server: MockServer,
    app: Arc<Application>,
    client: Arc<InProcessRuntimeClient>,
    session: SessionId,
}

async fn harness() -> Harness {
    install_config();
    let target_tmp = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&target_tmp).unwrap();
    let root = tempfile::Builder::new()
        .prefix("memory-disabled-")
        .tempdir_in(&target_tmp)
        .unwrap();
    std::fs::create_dir_all(root.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.path().join("configs/models")).unwrap();
    let server = MockServer::start(vec![text_response("the answer")]).await;
    std::fs::write(
        root.path().join("configs/providers/mock.yaml"),
        format!(
            "id: mock\nprotocol: openai_chat\nbase_url: {}\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::write(
        root.path().join("configs/models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 4096, max_tool_schema_bytes: 65536, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
    let layout = Layout::from_parts(
        root.path().to_path_buf(),
        root.path().join("configs"),
        root.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "disabled memory").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    Harness {
        root,
        server,
        app,
        client,
        session,
    }
}

impl Harness {
    async fn submit(&self, content: &str) -> RuntimeEvent {
        let mut rx = self.client.subscribe();
        self.client
            .send(ClientCommand::SubmitMessage {
                session_id: self.session.clone(),
                content: content.to_owned(),
                attachments: vec![],
            })
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(
                    event @ (RuntimeEvent::TurnCompleted
                    | RuntimeEvent::TurnCompletedWithWarnings { .. }
                    | RuntimeEvent::TurnAnswered
                    | RuntimeEvent::TurnTruncated { .. }
                    | RuntimeEvent::TurnIncomplete { .. }
                    | RuntimeEvent::TurnFailed { .. }
                    | RuntimeEvent::TurnCancelled),
                )) => return event,
                Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                _ => panic!("the turn never settled"),
            }
        }
    }

    async fn action(&self, command: ClientCommand) -> UiMemoryActionResult {
        let mut rx = self.client.subscribe();
        self.client.send(command).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(RuntimeEvent::MemoryActionFinished { result, .. })) => return result,
                Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                _ => panic!("the memory action never finished"),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_off_rollout_does_not_touch_prompt_store_or_automatic_work() {
    let harness = harness().await;
    let terminal = harness.submit("answer without memory").await;
    assert!(
        matches!(
            terminal,
            RuntimeEvent::TurnAnswered | RuntimeEvent::TurnCompleted
        ),
        "memory off does not change the turn: {terminal:?}"
    );

    let bodies = harness.server.request_bodies().await;
    assert_eq!(bodies.len(), 1, "one provider call for one turn");
    assert!(
        !bodies[0].contains("Cross-session memory"),
        "no memory index is injected: {}",
        bodies[0]
    );
    assert_eq!(
        harness.server.request_count(),
        1,
        "no capture call is made while memory is off"
    );
    assert!(
        !harness.root.path().join("memory").exists(),
        "an off rollout creates no memory directory"
    );
    let activity = harness.app.memory().activity(harness.session.as_str());
    assert_eq!(
        activity,
        leveler_app::memory::MemoryActivity::default(),
        "no capture activity exists"
    );

    // Read projections report the rollout without creating the store.
    let mut rx = harness.client.subscribe();
    harness
        .client
        .send(ClientCommand::QueryMemory {
            session_id: harness.session.clone(),
            query_id: None,
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::MemoryLoaded { status, .. })) => break status,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            _ => panic!("the memory status never arrived"),
        }
    };
    assert_eq!(status.rollout, "off");
    assert!(!status.capture_enabled);
    assert!(
        !harness.root.path().join("memory").exists(),
        "querying status does not create the store"
    );

    let dream = harness
        .action(ClientCommand::RunMemoryDream {
            session_id: harness.session.clone(),
            query_id: None,
            batch: false,
        })
        .await;
    assert!(!dream.ok);
    assert!(dream.message.contains("Dream is disabled"), "{dream:?}");
    let flush = harness
        .action(ClientCommand::FlushMemory {
            session_id: harness.session.clone(),
            query_id: None,
        })
        .await;
    assert!(!flush.ok);
    assert!(
        flush.message.contains("disabled"),
        "flush reports the disabled pipeline: {flush:?}"
    );

    // A read is refused with the reason, never rendered as an empty file.
    let mut rx = harness.client.subscribe();
    harness
        .client
        .send(ClientCommand::ReadMemoryFile {
            session_id: harness.session.clone(),
            query_id: None,
            scope: "global".to_owned(),
            path: "topics/anything.md".to_owned(),
        })
        .await
        .unwrap();
    let file = loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::MemoryFileLoaded { file, .. })) => break file,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            _ => panic!("the memory file result never arrived"),
        }
    };
    assert_eq!(
        file.error.as_deref(),
        Some("memory is off for this configuration")
    );
    assert!(file.content.is_empty());
    assert!(
        !harness.root.path().join("memory").exists(),
        "a refused read does not create the store"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_remember_is_the_one_write_that_survives_an_off_rollout() {
    let harness = harness().await;
    let remembered = harness
        .action(ClientCommand::RememberMemory {
            session_id: harness.session.clone(),
            query_id: None,
            content: "The team ships on Fridays.".to_owned(),
        })
        .await;
    assert!(
        remembered.ok,
        "an explicit /remember is persisted even while off: {remembered:?}"
    );

    // The note is durable in the memory store root the runtime uses.
    let root = harness.root.path().join("memory");
    let mut notes = Vec::new();
    fn walk(dir: &std::path::Path, notes: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, notes);
            } else if path.extension().is_some_and(|extension| extension == "md") {
                notes.push(path);
            }
        }
    }
    walk(&root, &mut notes);
    assert!(
        notes.iter().any(|path| std::fs::read_to_string(path)
            .is_ok_and(|text| text.contains("The team ships on Fridays."))),
        "the explicit note exists on disk: {notes:?}"
    );
    assert_eq!(
        harness.server.request_count(),
        0,
        "remember performs no model call and starts no automatic pipeline"
    );

    // A normal turn still ignores memory entirely.
    let terminal = harness.submit("answer after remember").await;
    assert!(
        !matches!(terminal, RuntimeEvent::TurnFailed { .. }),
        "{terminal:?}"
    );
    let bodies = harness.server.request_bodies().await;
    assert_eq!(bodies.len(), 1);
    assert!(
        !bodies[0].contains("Cross-session memory"),
        "the note is not advertised while the rollout is off"
    );
}
