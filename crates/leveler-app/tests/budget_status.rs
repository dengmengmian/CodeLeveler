//! The Goal/epoch budget status is a projection of the runtime's own durable
//! progress ledger and the caps it admits on.
//!
//! These tests drive a real turn against a scripted provider and ask the
//! runtime for the status, so the numbers are the ones the Desktop dock shows:
//!
//! - before any turn there is no epoch (absence, not a zero spend);
//! - after a turn the status reports the enforcer's token total, the configured
//!   cap and the soft level;
//! - asking twice is a recomputation, never an accumulation;
//! - a fresh runtime over the same durable log sees the same spend.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, CommandId, InteractiveRuntimeClient, RuntimeEvent, UiBudgetDimension,
    UiBudgetLevel, UiBudgetStatus,
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

fn text_with_usage(content: &str, prompt: u64, completion: u64) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
        serde_json::json!({
            "choices": [],
            "usage": {"prompt_tokens": prompt, "completion_tokens": completion,
                      "total_tokens": prompt + completion}
        })
        .to_string(),
    ])
}

struct Harness {
    _tmp: tempfile::TempDir,
    _server: MockServer,
    app: Arc<Application>,
    client: Arc<InProcessRuntimeClient>,
    session: leveler_core::SessionId,
    model: ModelRef,
}

/// A real app over an isolated home and a scratch workspace whose project
/// config carries the caps under test.
async fn harness(responses: Vec<MockResponse>, limits_yaml: &str) -> Harness {
    isolate_global_config();
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/models")).unwrap();
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(tmp.path().join(".leveler/config.yaml"), limits_yaml).unwrap();
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
limits: { context_window: 65536, reliable_context: 32768, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
pricing: { input_usd_per_mtok: 1.0, cached_input_usd_per_mtok: 0.5, output_usd_per_mtok: 2.0 }
"#,
    )
    .unwrap();
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
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "budget").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::Assisted,
        false,
    ));
    Harness {
        _tmp: tmp,
        _server: server,
        app,
        client,
        session,
        model,
    }
}

async fn budget(
    client: &Arc<InProcessRuntimeClient>,
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    session: &leveler_core::SessionId,
) -> Option<UiBudgetStatus> {
    let query_id = CommandId::generate();
    client
        .send(ClientCommand::QueryBudget {
            session_id: session.clone(),
            query_id: Some(query_id.clone()),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::BudgetLoaded {
                query_id: answered,
                status,
            })) if answered.as_ref() == Some(&query_id) => {
                return status.map(|status| *status);
            }
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the budget query was never answered"),
        }
    }
}

async fn one_turn(
    client: &Arc<InProcessRuntimeClient>,
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    session: &leveler_core::SessionId,
) {
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "answer briefly".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::TurnCompleted))
            | Ok(Ok(RuntimeEvent::TurnAnswered))
            | Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { .. }))
            | Ok(Ok(RuntimeEvent::TurnCompletedPartiallyUnverified { .. }))
            | Ok(Ok(RuntimeEvent::TurnIncomplete { .. })) => return,
            Ok(Ok(RuntimeEvent::TurnFailed { .. })) => panic!("the turn failed"),
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the turn never settled"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_status_is_the_durable_epoch_and_its_caps() {
    // A cap large enough for the turn and its closeout, with a soft threshold
    // low enough that the first settled spend reaches it: the reminder must
    // not stop the run.
    let harness = harness(
        vec![text_with_usage("done", 800, 200)],
        "limits:\n  max_model_tokens: 100000\n  soft_limit_percent: 1\n",
    )
    .await;
    let Harness {
        _tmp,
        _server,
        app,
        client,
        session,
        model,
    } = harness;
    let mut rx = client.subscribe();

    assert!(
        budget(&client, &mut rx, &session).await.is_none(),
        "an epoch with no durable progress is absent, not zero spend"
    );

    one_turn(&client, &mut rx, &session).await;
    let status = budget(&client, &mut rx, &session).await.expect("status");
    assert_eq!(status.model_tokens, 1_000);
    assert_eq!(status.estimated_model_tokens, 0);
    assert!(status.cost_known, "the model carries pricing");
    assert!(status.cost_usd_micros > 0);
    assert_eq!(status.limits.max_model_tokens, Some(100_000));
    assert_eq!(status.limits.soft_limit_percent, 1);
    assert_eq!(status.level, UiBudgetLevel::Warning);
    assert_eq!(status.dimension, Some(UiBudgetDimension::ModelTokens));

    let again = budget(&client, &mut rx, &session).await.expect("status");
    assert_eq!(again, status, "a query recomputes; it never accumulates");

    // A fresh runtime over the same durable log reads the same epoch.
    drop(client);
    drop(rx);
    let app = Arc::try_unwrap(app).ok().expect("the test owns the app");
    drop(app);
    let layout = Layout::from_parts(
        _tmp.path().to_path_buf(),
        _tmp.path().join("configs"),
        _tmp.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    let mut rx = client.subscribe();
    let after_restart = budget(&client, &mut rx, &session).await.expect("status");
    assert_eq!(after_restart, status, "restart keeps the durable epoch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edited_cap_is_reevaluated_while_spend_stays_durable() {
    // The same shape the Desktop shows: the durable spend is stable, while a
    // configuration edit changes the caps and the soft level the status is
    // judged against. The edit governs the next run; the display recomputes
    // immediately from the same ledger.
    let harness = harness(
        vec![text_with_usage("done", 800, 200)],
        "limits:\n  max_model_tokens: 100000\n  soft_limit_percent: 1\n",
    )
    .await;
    let Harness {
        _tmp,
        _server,
        app,
        client,
        session,
        ..
    } = harness;
    let mut rx = client.subscribe();
    one_turn(&client, &mut rx, &session).await;
    let warning = budget(&client, &mut rx, &session).await.expect("status");
    assert_eq!(warning.model_tokens, 1_000);
    assert_eq!(warning.level, UiBudgetLevel::Warning);

    std::fs::write(
        _tmp.path().join(".leveler/config.yaml"),
        "limits:\n  max_model_tokens: 1000000\n  soft_limit_percent: 90\n",
    )
    .unwrap();
    let normal = budget(&client, &mut rx, &session).await.expect("status");
    assert_eq!(
        normal.model_tokens, warning.model_tokens,
        "spend is durable"
    );
    assert_eq!(normal.limits.max_model_tokens, Some(1_000_000));
    assert_eq!(normal.limits.soft_limit_percent, 90);
    assert_eq!(normal.level, UiBudgetLevel::Normal);
    drop(app);
}
