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
