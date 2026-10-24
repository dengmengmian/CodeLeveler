//! Global, project and local pending records are one review queue.
//!
//! `(scope, id)` is the record's identity: the listing shows the scope of
//! every row, and a decision carries that scope back to the store it came
//! from. Accepting one scope must never touch another.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, CommandId, InteractiveRuntimeClient, NotificationLevel, RuntimeEvent,
    UiMemoryScope,
};
use leveler_core::{SessionId, ToolCallId, TurnId};
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

fn assemble() -> (tempfile::TempDir, Arc<Application>) {
    let tmp = tempfile::tempdir().unwrap();
    let app = assemble_at(tmp.path());
    (tmp, app)
}

fn assemble_at(root: &std::path::Path) -> Arc<Application> {
    isolate_global_config();
    let repo = root.join("repo");
    std::fs::create_dir_all(root.join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.join("configs/models")).unwrap();
    std::fs::create_dir_all(repo.join(".leveler")).unwrap();
    // Local memory is only writable inside a Git repository whose ignore
    // rules cover it; the store checks that boundary for real.
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .arg("init")
        .arg("-q")
        .status()
        .expect("git must be available to build a local-scope fixture");
    assert!(status.success(), "git init failed");
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        "id: mock\nprotocol: openai_chat\nbase_url: http://127.0.0.1:9\n",
    )
    .unwrap();
    std::fs::write(
        root.join("configs/models/m.yaml"),
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
        "memory:\n  enabled: true\n  backend: markdown\n",
    )
    .unwrap();
    let layout = Layout::from_parts(repo, root.join("configs"), root.join("state"));
    Arc::new(Application::assemble(layout).unwrap())
}

fn proposing_actor() -> Actor {
    Actor::Model {
        session: SessionId::new("scope-session"),
        turn: TurnId::new("scope-turn"),
        tool_call: ToolCallId::new("scope-call"),
    }
}

fn propose(app: &Application, scope: ServiceScope, body: &str) -> String {
    let request = leveler_memory::RememberRequest {
        op_id: format!(
            "propose:{}:{}",
            scope_label(scope),
            &leveler_memory::v2::sha256_hex_text(body)[..16]
        ),
        scope,
        title: leveler_memory::title_from_body(body),
        body: body.to_string(),
        kind: "preference".to_string(),
        subject: Some(format!("{}.convention", scope_label(scope))),
        value: None,
        tags: Vec::new(),
        evidence: None,
        expires_at: None,
        replaces_id: None,
    };
    match app
        .memory_service()
        .propose(&proposing_actor(), request)
        .unwrap()
    {
        leveler_memory::WriteOutcome::Pending { id, .. } => id,
        other => panic!("a proposal must stay pending: {other:?}"),
    }
}

fn scope_label(scope: ServiceScope) -> &'static str {
    match scope {
        ServiceScope::Global => "global",
        ServiceScope::Project => "project",
        ServiceScope::Local => "local",
    }
}

async fn list(
    runtime: &InProcessRuntimeClient,
    session: &SessionId,
    events: &mut broadcast::Receiver<RuntimeEvent>,
) -> leveler_client_protocol::RuntimeEvent {
    let query_id = CommandId::generate();
    runtime
        .send(ClientCommand::ListMemory {
            session_id: session.clone(),
            query_id: Some(query_id.clone()),
            include_archived: true,
        })
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Ok(event @ RuntimeEvent::MemoryList { .. })) => return event,
            Ok(Ok(RuntimeEvent::Notification {
                level: NotificationLevel::Warning,
                message,
            })) => {
                panic!("listing failed: {message}")
            }
            Ok(Ok(_)) => continue,
            Ok(Err(error)) => panic!("event stream closed: {error}"),
            Err(_) => panic!("the runtime never answered the listing"),
        }
    }
}

fn pending_scopes(event: &leveler_client_protocol::RuntimeEvent) -> Vec<(UiMemoryScope, String)> {
    match event {
        RuntimeEvent::MemoryList { pending, .. } => pending
            .iter()
            .map(|candidate| (candidate.scope, candidate.id.clone()))
            .collect(),
        other => panic!("expected a listing, got {other:?}"),
    }
}

async fn next_notification(
    events: &mut broadcast::Receiver<RuntimeEvent>,
) -> (NotificationLevel, String) {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Ok(RuntimeEvent::Notification { level, message })) => return (level, message),
            Ok(Ok(_)) => continue,
            Ok(Err(error)) => panic!("event stream closed: {error}"),
            Err(_) => panic!("the decision produced no notification"),
        }
    }
}

#[tokio::test]
async fn the_listing_shows_each_pending_record_with_its_own_scope() {
    let (_tmp, app) = assemble();
    let global = propose(&app, ServiceScope::Global, "全局约定");
    let project = propose(&app, ServiceScope::Project, "项目约定");
    let local = propose(&app, ServiceScope::Local, "本地约定");

    let runtime = InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );
    let session = SessionId::new("scope-listing");
    let mut events = runtime.subscribe_session(&session);
    let event = list(&runtime, &session, &mut events).await;

    let mut rows = pending_scopes(&event);
    rows.sort();
    let mut expected = vec![
        (UiMemoryScope::Global, global),
        (UiMemoryScope::Project, project),
        (UiMemoryScope::Local, local),
    ];
    expected.sort();
    assert_eq!(rows, expected);
}

#[tokio::test]
async fn a_decision_applies_only_to_the_scope_it_names() {
    let (_tmp, app) = assemble();
    let global = propose(&app, ServiceScope::Global, "全局约定");
    let project = propose(&app, ServiceScope::Project, "项目约定");

    let runtime = InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );
    let session = SessionId::new("scope-decision");
    let mut events = runtime.subscribe_session(&session);

    runtime
        .send(ClientCommand::AcceptMemory {
            session_id: session.clone(),
            scope: UiMemoryScope::Global,
            id: global.clone(),
        })
        .await
        .unwrap();
    let (level, message) = next_notification(&mut events).await;
    assert_eq!(level, NotificationLevel::Info, "{message}");
    assert!(message.contains(&format!("[{global}]")), "{message}");

    let event = list(&runtime, &session, &mut events).await;
    assert_eq!(
        pending_scopes(&event),
        vec![(UiMemoryScope::Project, project.clone())],
        "accepting the global record must not touch the project queue"
    );
    let global_active = app
        .memory_service()
        .listing_for_user(ServiceScope::Global, MemoryLanes::ALL)
        .unwrap();
    assert_eq!(global_active.len(), 1);
    assert_eq!(global_active[0].id, global);
    let project_active = app
        .memory_service()
        .listing_for_user(ServiceScope::Project, MemoryLanes::ALL)
        .unwrap();
    assert!(
        project_active.is_empty(),
        "the project record is still pending: {project_active:?}"
    );

    runtime
        .send(ClientCommand::RejectMemory {
            session_id: session.clone(),
            scope: UiMemoryScope::Project,
            id: project,
        })
        .await
        .unwrap();
    let (level, message) = next_notification(&mut events).await;
    assert_eq!(level, NotificationLevel::Info, "{message}");
    let event = list(&runtime, &session, &mut events).await;
    assert!(
        pending_scopes(&event).is_empty(),
        "both decisions were applied"
    );
}

/// A pending record is durable: a fresh process reads the same identity and
/// can still review it. Nothing about the queue lives only in memory.
#[tokio::test]
async fn a_pending_record_survives_a_restart_and_is_still_reviewable() {
    let tmp = tempfile::tempdir().unwrap();
    let global = {
        let app = assemble_at(tmp.path());
        propose(&app, ServiceScope::Global, "全局约定")
    };

    let app = assemble_at(tmp.path());
    let runtime = InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );
    let session = SessionId::new("scope-restart");
    let mut events = runtime.subscribe_session(&session);
    let event = list(&runtime, &session, &mut events).await;
    assert_eq!(
        pending_scopes(&event),
        vec![(UiMemoryScope::Global, global.clone())],
        "a restarted process must see the durable pending queue"
    );

    runtime
        .send(ClientCommand::AcceptMemory {
            session_id: session.clone(),
            scope: UiMemoryScope::Global,
            id: global.clone(),
        })
        .await
        .unwrap();
    let (level, message) = next_notification(&mut events).await;
    assert_eq!(level, NotificationLevel::Info, "{message}");
    let active = app
        .memory_service()
        .listing_for_user(ServiceScope::Global, MemoryLanes::ALL)
        .unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, global, "the reviewed id is the stored id");
}

#[tokio::test]
async fn a_failed_decision_is_reported_and_changes_nothing() {
    let (_tmp, app) = assemble();
    let global = propose(&app, ServiceScope::Global, "全局约定");

    let runtime = InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    );
    let session = SessionId::new("scope-failure");
    let mut events = runtime.subscribe_session(&session);

    // The id is real, the scope is wrong: the review must say so instead of
    // silently accepting the record from another store.
    runtime
        .send(ClientCommand::AcceptMemory {
            session_id: session.clone(),
            scope: UiMemoryScope::Local,
            id: global.clone(),
        })
        .await
        .unwrap();
    let (level, message) = next_notification(&mut events).await;
    assert_eq!(level, NotificationLevel::Warning, "{message}");
    assert!(message.contains("accept failed"), "{message}");

    let event = list(&runtime, &session, &mut events).await;
    assert_eq!(
        pending_scopes(&event),
        vec![(UiMemoryScope::Global, global)],
        "a failed decision must leave the pending record alone"
    );
}
