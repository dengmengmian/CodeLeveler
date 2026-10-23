//! Direct memory commands must report what actually happened.
//!
//! `/remember` and `记住：…` are trusted user writes, so they go through the
//! Memory service like every other path. On the Markdown backend a new user
//! fact needs a structured `subject_key`; a keyless direct command is rejected
//! rather than given an identity derived from its text. The status line must
//! say so instead of claiming the memory was saved.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, NotificationLevel, RuntimeEvent, UiMemoryKind,
};
use leveler_execution::PermissionProfile;
use leveler_memory::v2::Actor;
use leveler_memory::{MemoryLanes, ServiceScope};
use leveler_model::ModelRef;
use leveler_project::Layout;
use tokio::sync::broadcast;

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn assemble(backend: &str) -> (tempfile::TempDir, Arc<Application>) {
    isolate_global_config();
    let tmp = tempfile::tempdir().unwrap();
    // The Markdown state root must live outside the repository; production
    // layouts keep it under the home state dir.
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(tmp.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/models")).unwrap();
    std::fs::create_dir_all(repo.join(".leveler")).unwrap();
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
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
    std::fs::write(
        repo.join(".leveler/config.yaml"),
        format!("memory:\n  enabled: true\n  backend: {backend}\n"),
    )
    .unwrap();
    let layout = Layout::from_parts(repo, tmp.path().join("configs"), tmp.path().join("state"));
    let app = Arc::new(Application::assemble(layout).unwrap());
    (tmp, app)
}

async fn next_notification(
    events: &mut broadcast::Receiver<RuntimeEvent>,
) -> (NotificationLevel, String) {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Ok(RuntimeEvent::Notification { level, message })) => return (level, message),
            Ok(Ok(_)) => continue,
            Ok(Err(error)) => panic!("event stream closed: {error}"),
            Err(_) => panic!("the command produced no notification"),
        }
    }
}

async fn remember(runtime: &InProcessRuntimeClient, session: &leveler_core::SessionId, body: &str) {
    runtime
        .send(ClientCommand::RememberMemory {
            session_id: session.clone(),
            body: body.to_string(),
            kind: Some(UiMemoryKind::Preference),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn a_rejected_direct_command_is_not_reported_as_saved() {
    let (_tmp, app) = assemble("markdown");
    let runtime = InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );
    let session = leveler_core::SessionId::new("direct-markdown");
    let mut events = runtime.subscribe_session(&session);

    remember(&runtime, &session, "以后用中文回复").await;

    let (level, message) = next_notification(&mut events).await;
    assert_eq!(level, NotificationLevel::Warning, "{message}");
    assert!(message.contains("未保存"), "{message}");
    assert!(message.contains("missing_subject_key"), "{message}");
    assert!(
        !message.contains("已保存"),
        "a rejected write must not claim success: {message}"
    );

    let active = app
        .memory_service()
        .list(
            &Actor::External,
            ServiceScope::Project,
            MemoryLanes::ALL,
            None,
        )
        .unwrap();
    assert!(
        active.is_empty(),
        "a rejected write must not become current: {active:?}"
    );
}

#[tokio::test]
async fn an_applied_direct_command_is_reported_as_saved() {
    let (_tmp, app) = assemble("legacy");
    let runtime = InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );
    let session = leveler_core::SessionId::new("direct-legacy");
    let mut events = runtime.subscribe_session(&session);

    remember(&runtime, &session, "以后用中文回复").await;

    let (level, message) = next_notification(&mut events).await;
    assert_eq!(level, NotificationLevel::Info, "{message}");
    assert!(message.starts_with("已保存记忆 ["), "{message}");

    let active = app
        .memory_service()
        .list(
            &Actor::External,
            ServiceScope::Project,
            MemoryLanes::ALL,
            None,
        )
        .unwrap();
    assert_eq!(active.len(), 1, "{active:?}");
}
