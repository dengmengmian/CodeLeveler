//! Project memory policy and first-round exposure.
//!
//! `memory.enabled = false` is a project switch, not a permission level: no
//! profile may override it. When it IS enabled, the model must see the memory
//! tools and the guidance on its FIRST request, without spending a round on
//! `capability(enable)`.

use std::sync::Arc;

use leveler_app::Application;
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

fn text_response(content: &str) -> MockResponse {
    let frame = serde_json::json!({
        "choices": [{"delta": {"content": content}, "finish_reason": "stop"}]
    });
    MockResponse::Sse {
        body: format!("data: {frame}\n\ndata: [DONE]\n\n"),
    }
}

async fn harness(memory: Option<&str>) -> (tempfile::TempDir, MockServer, Arc<Application>) {
    isolate_global_config();
    let server = MockServer::start(vec![text_response("done")]).await;
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
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
    if let Some(value) = memory {
        std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
        std::fs::write(
            tmp.path().join(".leveler/config.yaml"),
            format!("memory:\n  enabled: {value}\n"),
        )
        .unwrap();
    }
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    (tmp, server, app)
}

async fn run_chat(app: &Application, mode: PermissionProfile, task: &str) {
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, task).await.unwrap();
    app.run_in_session(
        &session,
        &model,
        mode,
        task,
        Arc::new(leveler_execution::AutoApprove),
        false,
        &mut |_| {},
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
}

fn memory_tool_names_in(body: &str) -> Vec<&'static str> {
    ["memory", "remember", "forget"]
        .into_iter()
        .filter(|name| body.contains(&format!("\"name\":\"{name}\"")))
        .collect()
}

/// The explicit project switch wins over every permission profile — Full
/// included. It is not an approval question, so Full has nothing to bypass.
#[tokio::test]
async fn disabled_memory_is_not_exposed_under_any_permission_profile() {
    for mode in [
        PermissionProfile::FullAccess,
        PermissionProfile::Assisted,
        PermissionProfile::RequestApproval,
    ] {
        let (tmp, server, app) = harness(Some("false")).await;
        run_chat(&app, mode, "just answer").await;
        let bodies = server.request_bodies().await;
        let first = bodies.first().expect("the agent request must be observed");
        assert!(
            memory_tool_names_in(first).is_empty(),
            "{mode:?} exposed memory tools while the project disabled memory: {first}"
        );
        assert!(
            !first.contains("## Memory"),
            "{mode:?} exposed memory guidance while the project disabled memory: {first}"
        );
        let db = app.open_database().await.unwrap();
        let counts = leveler_storage::MemoryInboxRepository::new(&db)
            .counts()
            .await
            .unwrap();
        assert_eq!(
            counts.pending + counts.processing + counts.failed_retryable + counts.processed,
            0,
            "{mode:?} admitted a turn to memory extraction while disabled"
        );
        assert!(
            !tmp.path().join("state/memory/pending").exists(),
            "{mode:?} created a pending store while disabled"
        );
    }
}

/// With the switch on, the first request already carries the three tools, the
/// guidance, the catalog and the recalled bodies. No `capability(enable)`
/// round trip is required.
#[tokio::test]
async fn enabled_memory_reaches_the_first_request_without_a_capability_round_trip() {
    let (tmp, server, app) = harness(None).await;
    let store = leveler_memory::MemoryStore::open(&tmp.path().join("state/memory")).unwrap();
    store
        .activate(
            "Terse output",
            "Keep terminal output compact.",
            leveler_memory::MemoryKind::Preference,
            vec![],
        )
        .unwrap();
    store
        .activate(
            "Chosen database",
            "The project uses SQLite.",
            leveler_memory::MemoryKind::Decision,
            vec![],
        )
        .unwrap();
    store
        .activate(
            "Deploy runbook",
            "The deploy runbook lives in docs/deploy.md.",
            leveler_memory::MemoryKind::Note,
            vec![],
        )
        .unwrap();

    run_chat(
        &app,
        PermissionProfile::Assisted,
        "where is the deploy runbook",
    )
    .await;
    let db = app.open_database().await.unwrap();
    let counts = leveler_storage::MemoryInboxRepository::new(&db)
        .counts()
        .await
        .unwrap();
    assert!(
        counts.pending + counts.processing + counts.failed_retryable + counts.processed > 0,
        "an enabled turn must be admitted to the durable extraction queue"
    );

    let bodies = server.request_bodies().await;
    let first = bodies.first().expect("the agent request must be observed");
    assert_eq!(
        memory_tool_names_in(first),
        vec!["memory", "remember", "forget"],
        "the first request must expose the memory tools: {first}"
    );
    assert!(first.contains("## Memory"), "{first}");
    assert!(
        first.contains("Chosen database"),
        "the catalog must reach the first request: {first}"
    );
    assert!(
        first.contains("Keep terminal output compact."),
        "a standing preference must reach the first request: {first}"
    );
    assert!(
        first.contains("The deploy runbook lives in docs/deploy.md."),
        "query recall must reach the first request: {first}"
    );
}

/// `/memory status` reads the same files the worker and the UI use. It must
/// count a legacy sensitive record without exposing it.
#[tokio::test]
async fn status_reports_store_counts_and_sensitive_records() {
    let (tmp, _server, app) = harness(None).await;
    let memory = tmp.path().join("state/memory");
    let store = leveler_memory::MemoryStore::open(&memory).unwrap();
    store
        .activate(
            "Chosen database",
            "The project uses SQLite.",
            leveler_memory::MemoryKind::Decision,
            vec![],
        )
        .unwrap();
    let mut legacy = leveler_memory::new_entry(
        "Deploy token",
        "token=ghp_abcdefghijklmnopqrstuvwxyz0123",
        vec![],
    );
    legacy.id = "legacy-deploy-token".into();
    std::fs::write(
        memory.join("active").join(format!("{}.json", legacy.id)),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();

    let status = leveler_app::memory_status::report(&app).await.unwrap();
    assert_eq!(status.active, 2);
    assert_eq!(status.pending, 0);
    assert_eq!(status.archived, 0);
    assert_eq!(status.sensitive, 1);
    assert_eq!(status.inbox_pending, 0);
    assert_eq!(status.inbox_processed, 0);
    assert!(status.last_extraction.is_none());
    assert!(status.last_error.is_none());
}

/// A Goal turn and a later turn of the same session must not lose the default
/// memory surface when the goal binding restores persisted capabilities.
#[tokio::test]
async fn goal_and_resume_first_requests_keep_memory_visible() {
    let (_tmp, server, app) = harness(None).await;
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "drive it").await.unwrap();
    for _ in 0..2 {
        // One round is enough to observe the request surface; the goal
        // lifecycle would otherwise keep asking the mock to continue.
        let _ = app
            .run_in_session_bounded(
                &session,
                &model,
                PermissionProfile::Assisted,
                "continue",
                Arc::new(leveler_execution::AutoApprove),
                false,
                &mut |_| {},
                tokio_util::sync::CancellationToken::new(),
                1,
                None,
            )
            .await;
    }
    let bodies = server.request_bodies().await;
    assert!(bodies.len() >= 2, "two turns must produce two requests");
    for (index, body) in bodies.iter().enumerate() {
        assert_eq!(
            memory_tool_names_in(body),
            vec!["memory", "remember", "forget"],
            "turn {index} lost the default memory tools: {body}"
        );
        assert!(
            body.contains("## Memory"),
            "turn {index} lost guidance: {body}"
        );
    }
}
