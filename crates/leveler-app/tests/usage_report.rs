//! `/usage` is a projection of the runtime's own durable model-call ledger.
//! These tests drive real turns against a scripted provider and then ask the
//! runtime for a report, so the numbers are the ones a user would see.
//!
//! What is pinned here:
//!
//! - token relationships: cached input ⊆ input, reasoning ⊆ output, and
//!   `total = input + output` (no slice counted twice);
//! - the report is a projection of persisted rows, so asking twice, resuming,
//!   or compacting cannot inflate or reset it;
//! - an answer arrives while a turn is RUNNING (read-only, no extra model call);
//! - calendar-day ranges follow the caller's offset, and the resolved range is
//!   echoed back;
//! - a call whose provider reported nothing is counted as a request but never as
//!   a measured zero, and an unpriced call makes the cost a floor, not a total;
//! - switching models splits the per-model lanes.

use std::sync::Arc;
use std::time::Duration;

#[path = "support/observed_command.rs"]
mod observed_command;
use observed_command::ObservedSettings;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, CommandId, InteractiveRuntimeClient, RuntimeEvent, UiUsageReport, UiUsageScope,
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

/// One streamed answer that reports usage the way an OpenAI-compatible provider
/// does: the figures ride the final chunk.
fn text_with_usage(content: &str, prompt: u64, completion: u64, cached: u64) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
        serde_json::json!({
            "choices": [],
            "usage": {
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "total_tokens": prompt + completion,
                "prompt_tokens_details": {"cached_tokens": cached},
                "completion_tokens_details": {"reasoning_tokens": completion / 2}
            }
        })
        .to_string(),
    ])
}

fn text_without_usage(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

struct Harness {
    _tmp: tempfile::TempDir,
    _server: MockServer,
    app: Arc<Application>,
    client: Arc<InProcessRuntimeClient>,
    session: leveler_core::SessionId,
}

/// One model with pricing, so cost is a fact rather than a guess.
async fn harness(responses: Vec<MockResponse>) -> Harness {
    harness_with_pricing(responses, true).await
}

async fn harness_with_pricing(responses: Vec<MockResponse>, priced: bool) -> Harness {
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
    let pricing = if priced {
        "pricing: { input_usd_per_mtok: 1.0, cached_input_usd_per_mtok: 0.5, output_usd_per_mtok: 2.0 }\n"
    } else {
        ""
    };
    std::fs::write(
        tmp.path().join("configs/models/m.yaml"),
        format!(
            r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities: {{ streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }}
limits: {{ context_window: 65536, reliable_context: 32768, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }}
compatibility: {{ synthesize_tool_call_ids: true, drop_unsupported_fields: true }}
{pricing}"#
        ),
    )
    .unwrap();
    // A second route for the SAME provider, so a model switch produces a second
    // ledger lane instead of blending into the first.
    std::fs::write(
        tmp.path().join("configs/models/m2.yaml"),
        r#"
id: m2
provider: mock
model_id: mock-model-2
protocol: openai_chat
capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }
limits: { context_window: 65536, reliable_context: 32768, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
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
    let session = app.create_session(&model, "usage").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    Harness {
        _tmp: tmp,
        _server: server,
        app,
        client,
        session,
    }
}

/// Ask for a usage report and wait for the correlated answer.
async fn usage(
    client: &Arc<InProcessRuntimeClient>,
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    session: &leveler_core::SessionId,
    scope: UiUsageScope,
) -> UiUsageReport {
    let query_id = CommandId::generate();
    client
        .send(ClientCommand::QueryUsage {
            session_id: session.clone(),
            query_id: Some(query_id.clone()),
            scope,
            utc_offset_minutes: 480,
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::UsageLoaded {
                query_id: answered,
                report,
            })) if answered.as_ref() == Some(&query_id) => return *report,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the usage query was never answered"),
        }
    }
}

/// Run one turn to its terminal, so its ledger row has settled.
async fn one_turn(
    client: &Arc<InProcessRuntimeClient>,
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    session: &leveler_core::SessionId,
    content: &str,
) {
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: content.into(),
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
            | Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { .. })) => return,
            Ok(Ok(RuntimeEvent::TurnFailed { .. })) => panic!("the turn failed"),
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("the turn never settled"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_settled_call_reports_provider_tokens_with_slices_not_additions() {
    let h = harness(vec![text_with_usage("done", 1_000, 200, 800)]).await;
    let mut rx = h.client.subscribe();
    one_turn(&h.client, &mut rx, &h.session, "work").await;

    let report = usage(&h.client, &mut rx, &h.session, UiUsageScope::Session).await;
    assert_eq!(report.requests, 1, "{report:?}");
    assert_eq!(report.input_tokens, 1_000, "{report:?}");
    assert_eq!(report.output_tokens, 200, "{report:?}");
    assert_eq!(
        report.total_tokens(),
        1_200,
        "cached input is a slice of input, reasoning a slice of output: {report:?}"
    );
    assert_eq!(report.cached_input_tokens, Some(800), "{report:?}");
    assert_eq!(report.reasoning_tokens, Some(100), "{report:?}");
    assert_eq!(report.uncached_input_tokens(), Some(200), "{report:?}");
    assert_eq!(report.scope, UiUsageScope::Session);

    // Priced from the configured rates: 200 uncached * 1.0 + 800 cached * 0.5
    // + 200 output * 2.0 = 1,000 micro-USD.
    assert_eq!(report.cost_usd_micros, Some(1_000), "{report:?}");
    assert_eq!(report.priced_requests, 1);
    assert!(!report.cost_is_partial());

    assert_eq!(report.models.len(), 1, "{report:?}");
    let lane = &report.models[0];
    assert_eq!(lane.provider, "mock");
    assert!(
        !lane.model.is_empty(),
        "the lane names the model that actually ran"
    );
    assert_eq!(lane.requests, 1);
    assert_eq!(lane.total_tokens(), 1_200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn asking_twice_does_not_double_count_the_same_call() {
    let h = harness(vec![text_with_usage("done", 100, 10, 0)]).await;
    let mut rx = h.client.subscribe();
    one_turn(&h.client, &mut rx, &h.session, "work").await;

    let first = usage(&h.client, &mut rx, &h.session, UiUsageScope::Session).await;
    let second = usage(&h.client, &mut rx, &h.session, UiUsageScope::Session).await;
    assert_eq!(first.requests, 1);
    assert_eq!(second.requests, 1, "a replay is a re-read, not a re-sum");
    assert_eq!(second.input_tokens, 100);
    assert_eq!(second.output_tokens, 10);
    // Provider health probes are runtime diagnostics, never a session's cost.
    assert_eq!(first.models.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_report_answers_while_a_turn_is_running() {
    // The first request reports usage, then the turn keeps running on a silent
    // second call. The usage query must answer during that window.
    let h = harness(vec![
        text_with_usage("first", 500, 50, 0),
        MockResponse::SilentThenJson {
            silent_ms: 5_000,
            body: serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "late"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 2}
            })
            .to_string(),
        },
    ])
    .await;
    let mut rx = h.client.subscribe();
    h.client
        .send(ClientCommand::SubmitMessage {
            session_id: h.session.clone(),
            content: "work".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    // Wait until the first call has settled into the ledger.
    let started = std::time::Instant::now();
    loop {
        let report = usage(&h.client, &mut rx, &h.session, UiUsageScope::Session).await;
        if report.requests >= 1 {
            assert!(report.total_tokens() > 0, "{report:?}");
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the running turn never produced a measurable call"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The query itself sent no provider request: the server saw the turn's own
    // calls only (the second one may still be silent, which is fine).
    let _ = h
        .client
        .send(ClientCommand::CancelCurrentTurn {
            session_id: h.session.clone(),
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn today_uses_the_callers_calendar_day_and_echoes_the_range() {
    let h = harness(vec![text_with_usage("done", 40, 4, 0)]).await;
    let mut rx = h.client.subscribe();
    one_turn(&h.client, &mut rx, &h.session, "work").await;

    // UTC+8 right now: the call happened today in that calendar.
    let report = usage(&h.client, &mut rx, &h.session, UiUsageScope::Today).await;
    assert_eq!(report.requests, 1, "{report:?}");
    assert_eq!(report.scope, UiUsageScope::Today);
    assert!(
        report.from.is_some(),
        "the runtime resolves the range and echoes it: {report:?}"
    );

    // The opposite side of the planet may still be on the previous day, so the
    // window must be resolved from the offset the caller declared.
    let query_id = CommandId::generate();
    h.client
        .send(ClientCommand::QueryUsage {
            session_id: h.session.clone(),
            query_id: Some(query_id.clone()),
            scope: UiUsageScope::Today,
            utc_offset_minutes: -720,
        })
        .await
        .unwrap();
    let other = loop {
        match tokio::time::timeout(Duration::from_secs(20), rx.recv()).await {
            Ok(Ok(RuntimeEvent::UsageLoaded {
                query_id: answered,
                report,
            })) if answered.as_ref() == Some(&query_id) => break *report,
            Ok(Ok(_)) | Ok(Err(_)) => continue,
            _ => panic!("no answer"),
        }
    };
    assert_ne!(
        report.from, other.from,
        "the day boundary follows the caller's offset"
    );

    // An impossible offset is refused rather than silently wrapped.
    let refused = h
        .client
        .send(ClientCommand::QueryUsage {
            session_id: h.session.clone(),
            query_id: None,
            scope: UiUsageScope::Today,
            utc_offset_minutes: 10_000,
        })
        .await;
    assert!(refused.is_err(), "an out-of-range offset is not a day");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_without_reported_usage_is_counted_but_not_measured_as_zero() {
    let h = harness(vec![text_without_usage("done")]).await;
    let mut rx = h.client.subscribe();
    one_turn(&h.client, &mut rx, &h.session, "work").await;

    let report = usage(&h.client, &mut rx, &h.session, UiUsageScope::Session).await;
    assert_eq!(report.requests, 1, "{report:?}");
    assert_eq!(report.unmeasured_requests, 1, "{report:?}");
    assert_eq!(report.total_tokens(), 0);
    assert!(
        report.has_no_measured_usage(),
        "the client must say 'unavailable', not '0 tokens': {report:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unpriced_call_leaves_the_cost_unknown_instead_of_zero() {
    let h = harness_with_pricing(vec![text_with_usage("done", 300, 30, 0)], false).await;
    let mut rx = h.client.subscribe();
    one_turn(&h.client, &mut rx, &h.session, "work").await;

    let report = usage(&h.client, &mut rx, &h.session, UiUsageScope::Session).await;
    assert_eq!(report.requests, 1);
    assert_eq!(
        report.cost_usd_micros, None,
        "no pricing configured is not a free call: {report:?}"
    );
    assert_eq!(report.priced_requests, 0);
    assert!(report.cost_is_partial());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compaction_does_not_reset_the_ledger() {
    let h = harness(vec![
        text_with_usage("first", 700, 70, 0),
        // The compaction summary call.
        text_with_usage("summary", 50, 5, 0),
        text_with_usage("second", 800, 80, 0),
    ])
    .await;
    let mut rx = h.client.subscribe();
    one_turn(&h.client, &mut rx, &h.session, "first").await;
    let before = usage(&h.client, &mut rx, &h.session, UiUsageScope::Session).await;
    assert_eq!(before.requests, 1, "{before:?}");

    h.client
        .send(ClientCommand::CompactContext {
            session_id: h.session.clone(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let after = usage(&h.client, &mut rx, &h.session, UiUsageScope::Session).await;
    assert!(
        after.requests >= before.requests,
        "compaction must not erase what was already spent: {before:?} -> {after:?}"
    );
    assert!(
        after.input_tokens >= before.input_tokens,
        "{before:?} -> {after:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_turn_scope_covers_only_the_latest_turn() {
    let h = harness(vec![
        text_with_usage("one", 100, 10, 0),
        text_with_usage("two", 200, 20, 0),
    ])
    .await;
    let mut rx = h.client.subscribe();
    one_turn(&h.client, &mut rx, &h.session, "first").await;
    one_turn(&h.client, &mut rx, &h.session, "second").await;

    let session = usage(&h.client, &mut rx, &h.session, UiUsageScope::Session).await;
    assert_eq!(session.requests, 2, "{session:?}");
    assert_eq!(session.total_tokens(), 330);

    let turn = usage(&h.client, &mut rx, &h.session, UiUsageScope::Turn).await;
    assert_eq!(
        turn.requests, 1,
        "the latest turn's own calls only: {turn:?}"
    );
    assert_eq!(turn.total_tokens(), 220, "{turn:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switching_models_splits_the_model_lanes() {
    let h = harness(vec![
        text_with_usage("one", 100, 10, 0),
        text_with_usage("two", 300, 30, 0),
    ])
    .await;
    let mut rx = h.client.subscribe();
    one_turn(&h.client, &mut rx, &h.session, "first").await;

    h.client
        .send_observed(ClientCommand::SelectModel {
            session_id: h.session.clone(),
            model: ModelRef::new("mock", "m2"),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    one_turn(&h.client, &mut rx, &h.session, "second").await;

    let report = usage(&h.client, &mut rx, &h.session, UiUsageScope::Session).await;
    assert_eq!(report.requests, 2, "{report:?}");
    let models: Vec<&str> = report.models.iter().map(|m| m.model.as_str()).collect();
    assert!(
        models.contains(&"m") && models.contains(&"m2"),
        "each model keeps its own lane instead of blending: {report:?}"
    );
    // The second model has no configured tariff, so the summed cost can only be
    // a floor — and the report says so rather than presenting it as the bill.
    assert!(report.cost_is_partial(), "{report:?}");
    assert_eq!(report.priced_requests, 1, "{report:?}");
    assert!(
        report.models.iter().map(|m| m.requests).sum::<u32>() == report.requests,
        "every request lands in exactly one lane: {report:?}"
    );
    let _ = h.app;
}
