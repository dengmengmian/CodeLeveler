//! Bounded parallel web calls: overlap is proven at the HTTP server, results
//! keep their `tool_call_id` and original order, and one failure stays local.
//!
//! Every test here drives the real `Executor` and the real `web_fetch` /
//! `web_search` tools against a loopback HTTP server. The server counts
//! requests that are simultaneously inside the handler, so "parallel" is
//! observed on the wire rather than inferred from log timestamps.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentError, AgentEvent, Executor, NoopSink, StopReason};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{
    ApprovalDecision, ApprovalRequest, Approver, PermissionProfile, Workspace,
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall, stream_from_response,
};
use leveler_tools::{RiskLevel, Tool, ToolContext, ToolOutput};

fn registry() -> leveler_tools::ToolRegistry {
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    registry
}

#[derive(Debug, Clone)]
struct RequestRecord {
    path: String,
    arrived_us: u64,
    finished_us: u64,
    status: u16,
}

struct ServerState {
    start: Instant,
    in_flight: usize,
    max_in_flight: usize,
    requests: Vec<RequestRecord>,
}

impl ServerState {
    fn elapsed_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }
}

/// A loopback HTTP/1.1 server whose only job is to publish what really
/// arrived: path, arrival/finish time, status, and the maximum number of
/// requests that were inside the handler at the same time.
async fn spawn_server() -> (u16, Arc<Mutex<ServerState>>, tokio::task::JoinHandle<()>) {
    spawn_server_with_search_delay(0).await
}

/// The search endpoint has no query string the tool controls, so a test that
/// needs a slow provider sets its delay here.
async fn spawn_server_with_search_delay(
    search_delay_ms: u64,
) -> (u16, Arc<Mutex<ServerState>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(Mutex::new(ServerState {
        start: Instant::now(),
        in_flight: 0,
        max_in_flight: 0,
        requests: Vec::new(),
    }));
    let task = spawn_listener(listener, Arc::clone(&state), None, search_delay_ms);
    (port, state, task)
}

/// A server whose handlers park until the test releases them. Used to prove a
/// resource bound: every request that reaches the wire stays in flight, so the
/// budget — not request latency — decides when the next one may start.
struct GatedServer {
    ports: Vec<u16>,
    state: Arc<Mutex<ServerState>>,
    release: Arc<tokio::sync::Semaphore>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// One listener per port. Distinct ports are distinct host-budget keys, so a
/// test that wants the process layer to bind does not trip the per-host cap.
async fn spawn_gated_server(listeners: usize) -> GatedServer {
    let state = Arc::new(Mutex::new(ServerState {
        start: Instant::now(),
        in_flight: 0,
        max_in_flight: 0,
        requests: Vec::new(),
    }));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut ports = Vec::new();
    let mut tasks = Vec::new();
    for _ in 0..listeners {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        ports.push(listener.local_addr().unwrap().port());
        tasks.push(spawn_listener(
            listener,
            Arc::clone(&state),
            Some(Arc::clone(&release)),
            0,
        ));
    }
    GatedServer {
        ports,
        state,
        release,
        tasks,
    }
}

fn spawn_listener(
    listener: TcpListener,
    state: Arc<Mutex<ServerState>>,
    gate: Option<Arc<tokio::sync::Semaphore>>,
    search_delay_ms: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let shared = Arc::clone(&state);
            let gate = gate.clone();
            tokio::spawn(async move {
                serve_connection(stream, shared, gate, search_delay_ms).await;
            });
        }
    })
}

async fn serve_connection(
    mut stream: tokio::net::TcpStream,
    shared: Arc<Mutex<ServerState>>,
    gate: Option<Arc<tokio::sync::Semaphore>>,
    search_delay_ms: u64,
) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
        if buffer.len() > 64 * 1024 {
            return;
        }
    }
    let head = String::from_utf8_lossy(&buffer).to_string();
    let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
    let _method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default().to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let header_len = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(buffer.len());
    while buffer.len() < header_len + content_length {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    }
    let (delay_ms, status) = if path.starts_with("/v1/tools/search") {
        (search_delay_ms, 200)
    } else {
        response_plan(&path)
    };
    let slot = {
        let mut state = shared.lock().unwrap();
        state.in_flight += 1;
        state.max_in_flight = state.max_in_flight.max(state.in_flight);
        let arrived_us = state.elapsed_us();
        state.requests.push(RequestRecord {
            path: path.clone(),
            arrived_us,
            finished_us: 0,
            status,
        });
        state.requests.len() - 1
    };
    // Counted as in flight, then parked: the test observes the bound before
    // releasing every parked handler at once.
    let _gate_permit = match gate {
        Some(gate) => Some(gate.acquire_owned().await.expect("server gate is open")),
        None => None,
    };
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    let body = if path.starts_with("/v1/tools/search") {
        r#"{"results":[{"url":"https://example.test/result","title":"fixture result","snippet":"fixture snippet"}]}"#.to_string()
    } else {
        format!("body-for-{path}")
    };
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let mut state = shared.lock().unwrap();
    state.in_flight -= 1;
    let finished_us = state.elapsed_us();
    if let Some(record) = state.requests.get_mut(slot) {
        record.finished_us = finished_us;
    }
}

/// `/fetch/<id>?ms=150&status=200` controls one fetch. The search endpoint
/// shares the same delay knob.
fn response_plan(path: &str) -> (u64, u16) {
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or_default();
    let mut delay = 0;
    let mut status = 200;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("ms", value)) => delay = value.parse().unwrap_or(0),
            Some(("status", value)) => status = value.parse().unwrap_or(200),
            _ => {}
        }
    }
    (delay, status)
}

struct ScriptRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<Vec<Message>>>,
}

impl ScriptRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn last_tool_message(&self) -> Message {
        let requests = self.requests.lock().unwrap();
        requests
            .last()
            .and_then(|messages| messages.iter().find(|m| m.role == Role::Tool).cloned())
            .expect("the round after the tools carries one tool message")
    }
}

#[async_trait]
impl ModelRuntime for ScriptRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.requests.lock().unwrap().push(request.messages.clone());
        let response = self.generate(request, cancellation).await?;
        Ok(stream_from_response(response))
    }

    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id": "m", "provider": "mock", "model_id": "m", "protocol": "openai_chat",
            "capabilities": {
                "streaming": true, "tool_calling": true, "parallel_tool_calls": true,
                "structured_output": false, "reasoning": false, "vision": false
            },
            "limits": {
                "context_window": 128000, "reliable_context": 64000,
                "max_output_tokens": 4096, "max_tool_schema_bytes": 65536,
                "max_parallel_tool_calls": 8
            }
        }))
        .unwrap())
    }
}

fn tool_call_part(id: &str, name: &str, arguments: serde_json::Value) -> ContentPart {
    ContentPart::ToolCall {
        call: ToolCall {
            id: ToolCallId::new(id),
            name: name.to_string(),
            arguments,
        },
    }
}

fn tool_round(calls: Vec<(String, &'static str, serde_json::Value)>) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: calls
                .into_iter()
                .map(|(id, name, args)| tool_call_part(&id, name, args))
                .collect(),
            route: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

fn text(value: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, value),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

/// The process network budget is a production fact shared by every session.
/// These tests deliberately run many sessions in one process, so a test that
/// asserts request overlap must not compete with another test for the same
/// permits. This serializes test bodies, not the requests inside a batch.
static NETWORK_BUDGET_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serialize_network_budget() -> tokio::sync::MutexGuard<'static, ()> {
    NETWORK_BUDGET_TEST_LOCK.lock().await
}

struct Run {
    elapsed: Duration,
    tool_results: Vec<(String, bool, String)>,
}

/// Run one round of web calls at the given executor batch width and return
/// both the server's observations and the model-visible results.
async fn run_batch(
    limit: usize,
    calls: Vec<(String, &'static str, serde_json::Value)>,
    search_endpoint: Option<u16>,
) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context =
        ToolContext::new(workspace, PermissionProfile::Assisted).trust_all_definitions_for_tests();
    let mut registry = registry();
    if let Some(port) = search_endpoint {
        let provider = leveler_tools::search::DeepSeekSearchProvider::new(
            "fixture-key",
            Some(&format!("http://127.0.0.1:{port}/v1/tools")),
            5,
            Duration::from_secs(5),
        )
        .unwrap();
        registry.register(Arc::new(
            leveler_tools::tools::WebSearchTool::with_provider(Arc::new(provider)),
        ));
    }
    let runtime = Arc::new(ScriptRuntime::new(vec![
        tool_round(calls.clone()),
        text("fixture answer"),
    ]));
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(registry),
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    )
    .with_execution_controls(limit);
    // The server is spawned by the caller for its port; this helper only tells
    // it what the plan is. The port is already embedded in the URLs.
    let started = Instant::now();
    let outcome = executor
        .run(
            "run the fixture batch",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("the run completes");
    let elapsed = started.elapsed();
    let tool_message = runtime.last_tool_message();
    let tool_results = tool_message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolResult { result } => Some((
                result.call_id.as_str().to_string(),
                result.is_error,
                result.content.clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(outcome.stop_reason, StopReason::Answered);
    Run {
        elapsed,
        tool_results,
    }
}

#[tokio::test]
async fn three_web_fetches_overlap_on_the_wire_and_keep_call_order() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let calls = vec![
        (
            "c1".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/a?ms=150")}),
        ),
        (
            "c2".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/b?ms=150")}),
        ),
        (
            "c3".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/c?ms=150")}),
        ),
    ];
    let run = run_batch(4, calls, None).await;
    let (max_in_flight, requests) = {
        let state = state.lock().unwrap();
        (state.max_in_flight, state.requests.clone())
    };
    server.abort();
    assert_eq!(
        requests.len(),
        3,
        "three distinct requests reached the server"
    );
    assert_eq!(
        requests
            .iter()
            .map(|record| record.path.split('?').next().unwrap())
            .collect::<Vec<_>>(),
        ["/fetch/a", "/fetch/b", "/fetch/c"]
    );
    assert!(requests.iter().all(|record| record.status == 200));
    assert!(
        requests[0].finished_us > requests[1].arrived_us
            && requests[1].finished_us > requests[2].arrived_us,
        "successive request windows overlap on the server clock: {requests:?}"
    );
    assert_eq!(
        max_in_flight, 3,
        "all three were inside the handler at once"
    );
    assert_eq!(
        run.tool_results
            .iter()
            .map(|(id, _, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["c1", "c2", "c3"],
        "results are delivered in the model's original call order"
    );
    for (_, is_error, content) in &run.tool_results {
        assert!(!is_error, "{content}");
    }
    assert!(run.tool_results[0].2.contains("/fetch/a"));
    assert!(run.tool_results[1].2.contains("/fetch/b"));
    assert!(run.tool_results[2].2.contains("/fetch/c"));
}

#[tokio::test]
async fn the_network_bound_is_two_when_the_executor_width_is_two() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let calls = (0..3)
        .map(|index| {
            (
                format!("c{index}"),
                "web_fetch",
                serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/{index}?ms=150")}),
            )
        })
        .collect::<Vec<_>>();
    let run = run_batch(2, calls, None).await;
    let (max_in_flight, requests) = {
        let state = state.lock().unwrap();
        (state.max_in_flight, state.requests.clone())
    };
    server.abort();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        max_in_flight, 2,
        "the executed width, not the emitted count"
    );
    assert!(
        run.elapsed >= Duration::from_millis(280),
        "three 150 ms requests at width 2 take at least two waves: {:?}",
        run.elapsed
    );
    assert_eq!(run.tool_results.len(), 3);
}

#[tokio::test]
async fn a_serial_executor_never_overlaps_web_requests() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let calls = (0..3)
        .map(|index| {
            (
                format!("c{index}"),
                "web_fetch",
                serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/{index}?ms=100")}),
            )
        })
        .collect::<Vec<_>>();
    let run = run_batch(1, calls, None).await;
    let (max_in_flight, requests) = {
        let state = state.lock().unwrap();
        (state.max_in_flight, state.requests.clone())
    };
    server.abort();
    assert_eq!(requests.len(), 3);
    assert_eq!(max_in_flight, 1, "width 1 is the serial control");
    assert!(
        run.elapsed >= Duration::from_millis(290),
        "serial 3 x 100 ms: {:?}",
        run.elapsed
    );
}

#[tokio::test]
async fn a_failing_request_does_not_poison_its_siblings() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let calls = vec![
        (
            "c1".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/ok1?ms=60")}),
        ),
        (
            "c2".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/denied?ms=60&status=403")}),
        ),
        (
            "c3".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/ok2?ms=60")}),
        ),
    ];
    let run = run_batch(3, calls, None).await;
    let requests = {
        let state = state.lock().unwrap();
        state.requests.clone()
    };
    server.abort();
    assert_eq!(requests.len(), 3);
    assert_eq!(run.tool_results.len(), 3);
    assert!(!run.tool_results[0].1, "{}", run.tool_results[0].2);
    assert!(run.tool_results[1].1, "the 403 is this call's own error");
    assert!(
        run.tool_results[1].2.contains("403"),
        "{}",
        run.tool_results[1].2
    );
    assert!(!run.tool_results[2].1, "{}", run.tool_results[2].2);
}

#[tokio::test]
async fn repeated_urls_are_not_deduplicated_and_keep_their_own_identity() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let url = format!("http://127.0.0.1:{port}/fetch/repeat?ms=80");
    let calls = vec![
        (
            "c1".to_string(),
            "web_fetch",
            serde_json::json!({"url": url}),
        ),
        (
            "c2".to_string(),
            "web_fetch",
            serde_json::json!({"url": url}),
        ),
        (
            "c3".to_string(),
            "web_fetch",
            serde_json::json!({"url": url}),
        ),
    ];
    let run = run_batch(3, calls, None).await;
    let requests = {
        let state = state.lock().unwrap();
        state.requests.clone()
    };
    server.abort();
    assert_eq!(requests.len(), 3, "no cache or coalescing was introduced");
    assert_eq!(
        run.tool_results
            .iter()
            .map(|(id, _, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["c1", "c2", "c3"]
    );
}

#[tokio::test]
async fn same_domain_and_distinct_hostnames_share_the_same_bound() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let calls = vec![
        (
            "c1".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/a?ms=100")}),
        ),
        (
            "c2".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://localhost:{port}/fetch/b?ms=100")}),
        ),
        (
            "c3".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/c?ms=100")}),
        ),
    ];
    let run = run_batch(4, calls, None).await;
    let (max_in_flight, requests) = {
        let state = state.lock().unwrap();
        (state.max_in_flight, state.requests.clone())
    };
    server.abort();
    assert_eq!(requests.len(), 3);
    assert_eq!(max_in_flight, 3);
    assert_eq!(run.tool_results.len(), 3);
}

#[tokio::test]
async fn search_and_fetch_overlap_with_their_own_results() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let calls = vec![
        (
            "c1".to_string(),
            "web_search",
            serde_json::json!({"query": "fixture-search"}),
        ),
        (
            "c2".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/doc?ms=120")}),
        ),
    ];
    let run = run_batch(4, calls, Some(port)).await;
    let requests = {
        let state = state.lock().unwrap();
        state.requests.clone()
    };
    server.abort();
    assert_eq!(requests.len(), 2, "one search + one fetch");
    assert_eq!(run.tool_results.len(), 2);
    assert!(!run.tool_results[0].1, "{}", run.tool_results[0].2);
    assert!(
        run.tool_results[0]
            .2
            .contains("https://example.test/result"),
        "the search result is associated with the search call: {}",
        run.tool_results[0].2
    );
    assert!(!run.tool_results[1].1, "{}", run.tool_results[1].2);
    assert!(run.tool_results[1].2.contains("/fetch/doc"));
}

#[tokio::test]
async fn unauthorized_and_rate_limited_requests_stay_isolated() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let calls = vec![
        (
            "c1".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/unauth?ms=50&status=401")}),
        ),
        (
            "c2".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/limited?ms=50&status=429")}),
        ),
        (
            "c3".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/ok?ms=50")}),
        ),
    ];
    let run = run_batch(3, calls, None).await;
    let requests = {
        let state = state.lock().unwrap();
        state.requests.clone()
    };
    server.abort();
    assert_eq!(requests.len(), 3);
    assert_eq!(run.tool_results.len(), 3);
    assert!(run.tool_results[0].1, "401 is an error for its own call");
    assert!(run.tool_results[0].2.contains("401"));
    assert!(run.tool_results[1].1, "429 is an error for its own call");
    assert!(run.tool_results[1].2.contains("429"));
    assert!(!run.tool_results[2].1, "{}", run.tool_results[2].2);
    assert_eq!(
        run.tool_results
            .iter()
            .map(|(id, _, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["c1", "c2", "c3"]
    );
}

#[tokio::test]
async fn a_provider_timeout_does_not_cancel_an_independent_fetch() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server_with_search_delay(800).await;
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context =
        ToolContext::new(workspace, PermissionProfile::Assisted).trust_all_definitions_for_tests();
    let mut registry = registry();
    let provider = leveler_tools::search::DeepSeekSearchProvider::new(
        "fixture-key",
        Some(&format!("http://127.0.0.1:{port}/v1/tools")),
        5,
        Duration::from_millis(150),
    )
    .unwrap();
    registry.register(Arc::new(
        leveler_tools::tools::WebSearchTool::with_provider(Arc::new(provider)),
    ));
    let runtime = Arc::new(ScriptRuntime::new(vec![
        tool_round(vec![
            (
                "c1".to_string(),
                "web_search",
                serde_json::json!({"query": "slow"}),
            ),
            (
                "c2".to_string(),
                "web_fetch",
                serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/fast?ms=60")}),
            ),
        ]),
        text("fixture answer"),
    ]));
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(registry),
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    )
    .with_execution_controls(3);
    let started = Instant::now();
    let outcome = executor
        .run(
            "run the timeout fixture",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("the batch completes");
    let elapsed = started.elapsed();
    let requests = {
        let state = state.lock().unwrap();
        state.requests.clone()
    };
    server.abort();
    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(
        elapsed < Duration::from_millis(700),
        "the timed-out search did not hold the fetch's result: {elapsed:?}"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|record| record.path.starts_with("/v1/tools/search"))
            .count(),
        1,
        "a timeout is not retried"
    );
    let tool_message = runtime.last_tool_message();
    let results: Vec<_> = tool_message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolResult { result } => Some(result),
            _ => None,
        })
        .collect();
    assert!(results[0].is_error, "the search timed out");
    assert!(!results[1].is_error, "{}", results[1].content);
    assert!(results[1].content.contains("/fetch/fast"));
}

#[cfg(debug_assertions)]
struct DnsRebindGuard;

#[cfg(debug_assertions)]
impl Drop for DnsRebindGuard {
    fn drop(&mut self) {
        leveler_tools::clear_test_dns_rebind_for_tests();
    }
}

/// Admission and execution resolve separately. An answer that is allowed at
/// admission must be revalidated at execution: when the second resolution
/// returns a private address, no HTTP request may be sent — not to the
/// forbidden target and not to the admission-time address. A sibling call in
/// the same round keeps its own result.
#[cfg(debug_assertions)]
#[tokio::test]
async fn a_dns_answer_that_changes_after_admission_never_reaches_the_forbidden_target() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    for blocked in [
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 255, 255, 1)),
        std::net::IpAddr::V6(std::net::Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1)),
    ] {
        leveler_tools::install_test_dns_rebind_for_tests(blocked);
        let _rebind = DnsRebindGuard;
        let before = state.lock().unwrap().requests.len();
        let calls = vec![
            (
                "c1".to_string(),
                "web_fetch",
                serde_json::json!({
                    "url": format!("http://rebind.leveler.test:{port}/fetch/rebind?ms=10")
                }),
            ),
            (
                "c2".to_string(),
                "web_fetch",
                serde_json::json!({
                    "url": format!("http://127.0.0.1:{port}/fetch/ok?ms=10")
                }),
            ),
        ];
        let run = run_batch(3, calls, None).await;
        assert_eq!(run.tool_results.len(), 2);
        let (id, is_error, content) = &run.tool_results[0];
        assert_eq!(id, "c1", "the failed call keeps its original id");
        assert!(is_error, "{content}");
        assert!(content.contains("blocked private"), "{content}");
        let (id, is_error, content) = &run.tool_results[1];
        assert_eq!(id, "c2");
        assert!(!is_error, "the sibling still succeeds: {content}");
        let requests = state.lock().unwrap().requests.clone();
        assert_eq!(
            requests.len(),
            before + 1,
            "only the legitimate sibling reached the server: {requests:?}"
        );
        assert!(requests.last().unwrap().path.contains("/fetch/ok"));
    }
    server.abort();
}

/// Several sessions in one process share one budget. Each session's own
/// batch is still bounded, but the process total must hold at 8 in flight,
/// every session must make progress, a cancelled session must not leak its
/// permits, and no request may be duplicated.
#[tokio::test]
async fn concurrent_sessions_share_one_process_network_budget() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let dir = tempfile::tempdir().unwrap();
    let cancelled = CancellationToken::new();
    let cancel_after = {
        let token = cancelled.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            token.cancel();
        })
    };

    let mut tasks = Vec::new();
    for session in ["s0", "s1", "s2", "s3", "s4", "s5"] {
        let calls = (0..3)
            .map(|index| {
                (
                    format!("{session}-c{index}"),
                    "web_fetch",
                    serde_json::json!({
                        "url": format!(
                            "http://127.0.0.1:{port}/fetch/{session}-{index}?ms=150"
                        )
                    }),
                )
            })
            .collect();
        let runtime = Arc::new(ScriptRuntime::new(vec![tool_round(calls), text("done")]));
        let context = ToolContext::new(
            Workspace::new(dir.path()).unwrap(),
            PermissionProfile::Assisted,
        )
        .trust_all_definitions_for_tests()
        .with_session_scope(session);
        let executor = Executor::new(
            runtime.clone(),
            Arc::new(registry()),
            context,
            ModelRef::new("mock", "m"),
            5,
        )
        .with_execution_controls(3);
        let token = if session == "s5" {
            cancelled.clone()
        } else {
            CancellationToken::new()
        };
        tasks.push((
            session,
            runtime,
            tokio::spawn(async move {
                let mut sink = NoopSink;
                executor
                    .run("parallel sessions", &mut |_| {}, &mut sink, token)
                    .await
            }),
        ));
    }

    let mut completed = Vec::new();
    for (session, runtime, task) in tasks {
        let outcome = task.await.expect("the session task completes");
        // The cancelled session may never receive a tool message; only the
        // uncancelled sessions are asked for a transcript.
        let results = if session == "s5" {
            Vec::new()
        } else {
            runtime
                .last_tool_message()
                .content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::ToolResult { result } => Some((
                        result.call_id.as_str().to_string(),
                        result.is_error,
                        result.content.clone(),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        completed.push((session, outcome, results));
    }
    let _ = cancel_after.await;
    server.abort();

    for (session, outcome, results) in &completed {
        if *session == "s5" {
            // The cancelled session may have produced partial results; the
            // proof that matters is that it ended and released the budget.
            continue;
        }
        let outcome = outcome.as_ref().expect("an uncancelled session completes");
        assert_eq!(outcome.stop_reason, StopReason::Answered, "{session}");
        assert_eq!(results.len(), 3, "{session}");
        for (id, is_error, content) in results {
            assert!(!*is_error, "{session}/{id}: {content}");
        }
    }

    let requests = state.lock().unwrap().requests.clone();
    assert!(
        state.lock().unwrap().max_in_flight <= 8,
        "the process budget must cap total in-flight requests: {}",
        state.lock().unwrap().max_in_flight
    );
    assert!(
        state.lock().unwrap().max_in_flight >= 4,
        "multiple sessions must actually overlap: {}",
        state.lock().unwrap().max_in_flight
    );
    for session in ["s0", "s1", "s2", "s3", "s4"] {
        let paths = requests
            .iter()
            .filter(|record| record.path.contains(&format!("/fetch/{session}-")))
            .count();
        assert_eq!(paths, 3, "{session} must issue exactly its three calls");
    }
    // No duplicate wire requests, and every uncancelled session reached the
    // server: 15 calls plus at most the cancelled session's three.
    assert!(requests.len() <= 18, "{requests:?}");
    let first_arrival = |session: &str| {
        requests
            .iter()
            .filter(|record| record.path.contains(&format!("/fetch/{session}-")))
            .map(|record| record.arrived_us)
            .min()
    };
    let latest_first = ["s0", "s1", "s2", "s3", "s4"]
        .iter()
        .filter_map(|session| first_arrival(session))
        .max()
        .expect("every uncancelled session starts");
    assert!(
        latest_first < 2_000_000,
        "a later session was starved of a permit: {latest_first} us"
    );

    // The budget is fully released after the sessions end.
    let (port, _, follow_up) = spawn_server().await;
    let run = run_batch(
        3,
        vec![(
            "after".to_string(),
            "web_fetch",
            serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/after?ms=10")}),
        )],
        None,
    )
    .await;
    follow_up.abort();
    assert_eq!(run.tool_results.len(), 1);
    assert!(!run.tool_results[0].1, "{}", run.tool_results[0].2);
}

/// The production process budget must bind through the real HTTP path, not
/// only in the budget's own unit tests. Three sessions each start a width-3
/// batch against three distinct ports; every request that reaches the wire is
/// parked at the server. Exactly eight may be inside at once, and the ninth
/// must not open a socket until one of them finishes.
#[tokio::test]
async fn the_process_budget_reaches_eight_real_http_requests_and_queues_the_ninth() {
    let _serial = serialize_network_budget().await;
    let server = spawn_gated_server(3).await;
    let dir = tempfile::tempdir().unwrap();
    let mut sessions = Vec::new();
    for (session_index, port) in server.ports.iter().enumerate() {
        let session = format!("gate-{session_index}");
        let calls = (0..3)
            .map(|index| {
                (
                    format!("{session}-c{index}"),
                    "web_fetch",
                    serde_json::json!({
                        "url": format!("http://127.0.0.1:{port}/fetch/{session}-{index}?ms=0")
                    }),
                )
            })
            .collect();
        let runtime = Arc::new(ScriptRuntime::new(vec![tool_round(calls), text("done")]));
        let context = ToolContext::new(
            Workspace::new(dir.path()).unwrap(),
            PermissionProfile::Assisted,
        )
        .trust_all_definitions_for_tests()
        .with_session_scope(session.clone());
        let executor = Executor::new(
            runtime.clone(),
            Arc::new(registry()),
            context,
            ModelRef::new("mock", "m"),
            5,
        )
        .with_execution_controls(3);
        sessions.push((
            session,
            runtime,
            tokio::spawn(async move {
                let mut sink = NoopSink;
                executor
                    .run(
                        "process budget saturation",
                        &mut |_| {},
                        &mut sink,
                        CancellationToken::new(),
                    )
                    .await
            }),
        ));
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let in_flight = server.state.lock().unwrap().in_flight;
        if in_flight == 8 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "only {in_flight} requests reached the server before the deadline"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    {
        let state = server.state.lock().unwrap();
        assert_eq!(
            state.in_flight, 8,
            "exactly eight requests may be in flight"
        );
        assert_eq!(
            state.requests.len(),
            8,
            "the ninth request must not open a socket while the budget is full"
        );
        assert_eq!(state.max_in_flight, 8);
    }

    server.release.add_permits(9);
    for (session, runtime, task) in sessions {
        let outcome = task
            .await
            .expect("the session task completes")
            .expect("the session run completes");
        assert_eq!(outcome.stop_reason, StopReason::Answered, "{session}");
        let results: Vec<_> = runtime
            .last_tool_message()
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::ToolResult { result } => Some((
                    result.call_id.as_str().to_string(),
                    result.is_error,
                    result.content.clone(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 3, "{session}");
        for (id, is_error, content) in results {
            assert!(!is_error, "{session}/{id}: {content}");
        }
    }
    for task in server.tasks {
        task.abort();
    }
    let state = server.state.lock().unwrap();
    assert_eq!(state.requests.len(), 9, "every call is sent exactly once");
    assert_eq!(
        state.max_in_flight, 8,
        "the process budget must never exceed eight in-flight requests"
    );
}

struct ScriptedApprover {
    decision: ApprovalDecision,
    asks: Arc<std::sync::atomic::AtomicUsize>,
    wait_for_cancel: Option<CancellationToken>,
}

#[async_trait]
impl Approver for ScriptedApprover {
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        self.asks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(token) = &self.wait_for_cancel {
            token.cancelled().await;
        }
        self.decision
    }
}

fn private_executor(
    approver: Arc<ScriptedApprover>,
) -> (Executor, Arc<ScriptRuntime>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context =
        ToolContext::new(workspace, PermissionProfile::Assisted).trust_all_definitions_for_tests();
    let runtime = Arc::new(ScriptRuntime::new(vec![
        tool_round(vec![
            (
                "c1".to_string(),
                "web_fetch",
                serde_json::json!({"url": "http://10.0.0.1/private"}),
            ),
            (
                "c2".to_string(),
                "web_fetch",
                serde_json::json!({"url": "http://10.0.0.2/private"}),
            ),
        ]),
        text("fixture answer"),
    ]));
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    )
    .with_approver(approver)
    .with_execution_controls(3);
    (executor, runtime, dir)
}

#[tokio::test]
async fn a_denied_private_destination_never_reaches_execution() {
    let _serial = serialize_network_budget().await;
    let asks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let approver = Arc::new(ScriptedApprover {
        decision: ApprovalDecision::Deny,
        asks: Arc::clone(&asks),
        wait_for_cancel: None,
    });
    let (executor, runtime, _dir) = private_executor(approver);
    let outcome = executor
        .run(
            "run the private fixture",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("a denial is a model-visible tool error, not a run failure");
    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(
        asks.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "each destination is its own authorization unit; a denial is not a standing rule"
    );
    let tool_message = runtime.last_tool_message();
    let results: Vec<_> = tool_message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolResult { result } => Some(result),
            _ => None,
        })
        .collect();
    assert!(results.iter().all(|result| result.is_error));
    assert!(
        results[0].content.contains("denied")
            || results[0].content.contains("拒绝")
            || results[0].content.contains("not permitted"),
        "{}",
        results[0].content
    );
}

#[tokio::test]
async fn cancelling_while_an_approval_is_pending_returns_promptly() {
    let _serial = serialize_network_budget().await;
    let cancellation = CancellationToken::new();
    let asks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let approver = Arc::new(ScriptedApprover {
        decision: ApprovalDecision::ApproveOnce,
        asks: Arc::clone(&asks),
        wait_for_cancel: Some(cancellation.clone()),
    });
    let (executor, _runtime, _dir) = private_executor(approver);
    let token = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
    });
    let started = Instant::now();
    let result = executor
        .run(
            "cancel the pending approval",
            &mut |_| {},
            &mut NoopSink,
            cancellation,
        )
        .await;
    assert!(
        matches!(result, Err(AgentError::Cancelled)),
        "cancellation wins over the pending question: {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "the run did not wait for the approver: {:?}",
        started.elapsed()
    );
    assert_eq!(asks.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// A serial probe that records what the fixture had observed when it ran.
/// It lets a test prove the group flush really happened before the next
/// serial call, rather than inferring order from timestamps.
struct ProbeTool {
    state: Arc<Mutex<ServerState>>,
    seen: Arc<Mutex<Vec<(usize, usize)>>>,
}

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &'static str {
        "probe"
    }
    fn description(&self) -> &'static str {
        "test-only serial probe"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, leveler_tools::ToolError> {
        let state = self.state.lock().unwrap();
        let finished = state
            .requests
            .iter()
            .filter(|record| record.finished_us != 0)
            .count();
        self.seen
            .lock()
            .unwrap()
            .push((state.requests.len(), finished));
        Ok(ToolOutput::ok("probed"))
    }
}

#[tokio::test]
async fn a_deferred_network_group_finishes_before_the_next_serial_call() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context =
        ToolContext::new(workspace, PermissionProfile::Assisted).trust_all_definitions_for_tests();
    let mut registry = registry();
    registry.register(Arc::new(ProbeTool {
        state: Arc::clone(&state),
        seen: Arc::clone(&seen),
    }));
    let runtime = Arc::new(ScriptRuntime::new(vec![
        tool_round(vec![
            (
                "c1".to_string(),
                "web_fetch",
                serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/first?ms=150")}),
            ),
            ("p1".to_string(), "probe", serde_json::json!({})),
            (
                "c2".to_string(),
                "web_fetch",
                serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/second?ms=60")}),
            ),
        ]),
        text("fixture answer"),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(registry),
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    )
    .with_execution_controls(4);
    let outcome = executor
        .run(
            "run the ordered fixture",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("the ordered run completes");
    server.abort();
    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [(1, 1)],
        "the probe saw exactly the first fetch, already finished, and not the second"
    );
}

#[tokio::test]
async fn a_refused_network_call_does_not_stop_its_admitted_sibling() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let asks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let approver = Arc::new(ScriptedApprover {
        decision: ApprovalDecision::Deny,
        asks: Arc::clone(&asks),
        wait_for_cancel: None,
    });
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context =
        ToolContext::new(workspace, PermissionProfile::Assisted).trust_all_definitions_for_tests();
    let runtime = Arc::new(ScriptRuntime::new(vec![
        tool_round(vec![
            (
                "c1".to_string(),
                "web_fetch",
                serde_json::json!({"url": "http://10.0.0.1/private"}),
            ),
            (
                "c2".to_string(),
                "web_fetch",
                serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/public?ms=60")}),
            ),
        ]),
        text("fixture answer"),
    ]));
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    )
    .with_approver(approver)
    .with_execution_controls(3);
    let outcome = executor
        .run(
            "run the mixed-denial fixture",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("a refused call is a model-visible result");
    let requests = {
        let state = state.lock().unwrap();
        state.requests.clone()
    };
    server.abort();
    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(asks.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(requests.len(), 1, "only the admitted public fetch ran");
    let tool_message = runtime.last_tool_message();
    let results: Vec<_> = tool_message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolResult { result } => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results[0].is_error, "the private call was refused");
    assert!(!results[1].is_error, "{}", results[1].content);
    assert!(results[1].content.contains("/fetch/public"));
}

#[tokio::test]
async fn cancelling_after_one_web_call_finished_keeps_that_result() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context =
        ToolContext::new(workspace, PermissionProfile::Assisted).trust_all_definitions_for_tests();
    let runtime = Arc::new(ScriptRuntime::new(vec![
        tool_round(vec![
            (
                "fast".to_string(),
                "web_fetch",
                serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/fast?ms=40")}),
            ),
            (
                "slow".to_string(),
                "web_fetch",
                serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/slow?ms=2000")}),
            ),
        ]),
        text("never reached"),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    )
    .with_execution_controls(3);
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        token.cancel();
    });
    let mut results = Vec::new();
    let result = executor
        .run(
            "cancel after the fast result",
            &mut |event| {
                if let AgentEvent::ToolResult {
                    id,
                    is_error,
                    preview,
                    ..
                } = event
                    && (id.starts_with("fast") || id.starts_with("slow"))
                {
                    results.push((id, is_error, preview));
                }
            },
            &mut NoopSink,
            cancellation,
        )
        .await;
    server.abort();
    assert!(
        matches!(result, Err(AgentError::Cancelled)),
        "the turn is cancelled: {result:?}"
    );
    results.sort_by_key(|(id, _, _)| id.clone());
    let fast = results
        .iter()
        .find(|(id, _, _)| id == "fast")
        .expect("the fast call's result was observed");
    let slow = results
        .iter()
        .find(|(id, _, _)| id == "slow")
        .expect("the slow call's result was observed");
    assert!(!fast.1, "a completed call keeps its success: {}", fast.2);
    assert!(slow.1, "the interrupted call is an error");
    let _ = state;
}

#[tokio::test]
async fn cancelling_the_turn_stops_in_flight_web_calls_promptly() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context =
        ToolContext::new(workspace, PermissionProfile::Assisted).trust_all_definitions_for_tests();
    let runtime = Arc::new(ScriptRuntime::new(vec![
        tool_round(vec![
            (
                "c1".to_string(),
                "web_fetch",
                serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/slow1?ms=2000")}),
            ),
            (
                "c2".to_string(),
                "web_fetch",
                serde_json::json!({"url": format!("http://127.0.0.1:{port}/fetch/slow2?ms=2000")}),
            ),
        ]),
        text("never reached"),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    )
    .with_execution_controls(3);
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        token.cancel();
    });
    let started = Instant::now();
    let result = executor
        .run(
            "cancel the fixture batch",
            &mut |_| {},
            &mut NoopSink,
            cancellation,
        )
        .await;
    let elapsed = started.elapsed();
    server.abort();
    assert!(
        matches!(result, Err(AgentError::Cancelled)),
        "a cancelled turn surfaces Cancelled: {result:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1200),
        "in-flight requests were stopped instead of waiting 2 s: {elapsed:?}"
    );
    // The server may have accepted both connections before the cancel; what
    // matters is that the run did not wait for their handlers.
    let _ = state;
}

fn percentile(values: &mut [u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let index = (((values.len() - 1) as f64) * p).round() as usize;
    values[index]
}

fn max_wire_overlap(records: &[RequestRecord]) -> usize {
    let mut events = Vec::with_capacity(records.len() * 2);
    for record in records {
        let finished = if record.finished_us == 0 {
            record.arrived_us
        } else {
            record.finished_us
        };
        events.push((record.arrived_us, 1_i32));
        events.push((finished, -1_i32));
    }
    events.sort_unstable();
    let mut current = 0_i32;
    let mut peak = 0_i32;
    for (_, delta) in events {
        current += delta;
        peak = peak.max(current);
    }
    peak.max(0) as usize
}

fn measurement_session(
    dir: &std::path::Path,
    session: &str,
    host: &str,
    port: u16,
    width: usize,
) -> (Executor, Arc<ScriptRuntime>) {
    let calls = (0..3)
        .map(|index| {
            (
                format!("{session}-{index}"),
                "web_fetch",
                serde_json::json!({
                    "url": format!("http://{host}:{port}/fetch/{session}-{index}?ms=150")
                }),
            )
        })
        .collect();
    let runtime = Arc::new(ScriptRuntime::new(vec![tool_round(calls), text("done")]));
    let context = ToolContext::new(Workspace::new(dir).unwrap(), PermissionProfile::Assisted)
        .trust_all_definitions_for_tests()
        .with_session_scope(session);
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(registry()),
        context,
        ModelRef::new("mock", "m"),
        5,
    )
    .with_execution_controls(width);
    (executor, runtime)
}

fn measurement_search_session(
    dir: &std::path::Path,
    session: &str,
    port: u16,
    width: usize,
) -> (Executor, Arc<ScriptRuntime>) {
    let mut registry = registry();
    let provider = leveler_tools::search::DeepSeekSearchProvider::new(
        "fixture-key",
        Some(&format!("http://127.0.0.1:{port}/v1/tools")),
        5,
        Duration::from_secs(5),
    )
    .unwrap();
    registry.register(Arc::new(
        leveler_tools::tools::WebSearchTool::with_provider(Arc::new(provider)),
    ));
    let calls = (0..2)
        .map(|index| {
            (
                format!("{session}-{index}"),
                "web_search",
                serde_json::json!({"query": format!("q-{session}-{index}"), "count": 1}),
            )
        })
        .collect();
    let runtime = Arc::new(ScriptRuntime::new(vec![tool_round(calls), text("done")]));
    let context = ToolContext::new(Workspace::new(dir).unwrap(), PermissionProfile::Assisted)
        .trust_all_definitions_for_tests()
        .with_session_scope(session);
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(registry),
        context,
        ModelRef::new("mock", "m"),
        5,
    )
    .with_execution_controls(width);
    (executor, runtime)
}

/// Measurement harness, not a gate. Run explicitly:
/// `cargo test -p leveler-agent --test web_parallel_test network_budget_measurement -- --ignored --nocapture`
#[ignore = "measurement harness; run explicitly"]
#[tokio::test]
async fn network_budget_measurement() {
    let _serial = serialize_network_budget().await;
    let (port, state, server) = spawn_server().await;
    let dir = tempfile::tempdir().unwrap();

    let mut single = serde_json::Map::new();
    for width in [1_usize, 2, 3] {
        let mut walls = Vec::new();
        for _ in 0..30 {
            let calls = (0..3)
                .map(|index| {
                    (
                        format!("single-{index}"),
                        "web_fetch",
                        serde_json::json!({
                            "url": format!("http://127.0.0.1:{port}/fetch/single-{index}?ms=150")
                        }),
                    )
                })
                .collect();
            let run = run_batch(width, calls, None).await;
            for (_, is_error, content) in &run.tool_results {
                assert!(!*is_error, "{content}");
            }
            walls.push(run.elapsed.as_micros() as u64);
        }
        single.insert(
            format!("width_{width}"),
            serde_json::json!({
                "n": walls.len(),
                "p50_us": percentile(&mut walls, 0.50),
                "p95_us": percentile(&mut walls, 0.95),
            }),
        );
    }

    let mut multi = serde_json::Map::new();
    for sessions in [2_usize, 5, 10] {
        leveler_tools::clear_budget_wait_samples();
        let before = state.lock().unwrap().requests.len();
        let mut round_walls = Vec::new();
        let mut session_walls = Vec::new();
        for round in 0..10 {
            let started = Instant::now();
            let mut tasks = Vec::new();
            for index in 0..sessions {
                let session = format!("m{sessions}-r{round}-s{index}");
                // Two host identities for the same fixture so the per-host
                // bound is not the only ceiling: the process budget must bind.
                let host = if index % 2 == 0 {
                    "127.0.0.1"
                } else {
                    "localhost"
                };
                let (executor, runtime) = measurement_session(dir.path(), &session, host, port, 3);
                tasks.push(tokio::spawn(async move {
                    let mut sink = NoopSink;
                    let started = Instant::now();
                    let outcome = executor
                        .run("measure", &mut |_| {}, &mut sink, CancellationToken::new())
                        .await;
                    let results = runtime
                        .last_tool_message()
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::ToolResult { result } => Some(result.is_error),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    (
                        outcome.is_ok(),
                        started.elapsed().as_micros() as u64,
                        results,
                    )
                }));
            }
            for task in tasks {
                let (ok, wall, results) = task.await.unwrap();
                assert!(ok);
                assert_eq!(results.len(), 3);
                for is_error in results {
                    assert!(!is_error);
                }
                session_walls.push(wall);
            }
            round_walls.push(started.elapsed().as_micros() as u64);
        }
        let records = state.lock().unwrap().requests[before..].to_vec();
        let samples = leveler_tools::take_budget_wait_samples();
        let mut layers = serde_json::Map::new();
        for (name, layer) in [
            ("global", leveler_tools::LAYER_GLOBAL),
            ("session", leveler_tools::LAYER_SESSION),
            ("provider", leveler_tools::LAYER_PROVIDER),
            ("provider_rate", leveler_tools::LAYER_PROVIDER_RATE),
            ("host", leveler_tools::LAYER_HOST),
        ] {
            let mut waits: Vec<u64> = samples
                .iter()
                .filter(|(sample_layer, _)| *sample_layer == layer)
                .map(|(_, wait)| *wait)
                .collect();
            layers.insert(
                name.to_string(),
                serde_json::json!({
                    "n": waits.len(),
                    "p50_us": percentile(&mut waits, 0.50),
                    "p95_us": percentile(&mut waits, 0.95),
                    "max_us": waits.last().copied().unwrap_or(0),
                }),
            );
        }
        multi.insert(
            format!("sessions_{sessions}"),
            serde_json::json!({
                "rounds": round_walls.len(),
                "round_p50_us": percentile(&mut round_walls, 0.50),
                "round_p95_us": percentile(&mut round_walls, 0.95),
                "session_p50_us": percentile(&mut session_walls, 0.50),
                "session_p95_us": percentile(&mut session_walls, 0.95),
                "session_max_us": session_walls.iter().copied().max().unwrap_or(0),
                "wire_requests": records.len(),
                "wire_max_in_flight": max_wire_overlap(&records),
                "layers": layers,
            }),
        );
    }
    // Provider budget: five sessions, two searches each, one provider. The
    // provider concurrency bound is two, so this queues at that layer rather
    // than the global or per-host layers.
    let (search_port, search_state, search_server) = spawn_server_with_search_delay(150).await;
    leveler_tools::clear_budget_wait_samples();
    let search_before = search_state.lock().unwrap().requests.len();
    let mut search_rounds = Vec::new();
    for round in 0..10 {
        let started = Instant::now();
        let mut tasks = Vec::new();
        for index in 0..5 {
            let session = format!("p-r{round}-s{index}");
            let (executor, _runtime) =
                measurement_search_session(dir.path(), &session, search_port, 3);
            tasks.push(tokio::spawn(async move {
                let mut sink = NoopSink;
                let outcome = executor
                    .run(
                        "measure searches",
                        &mut |_| {},
                        &mut sink,
                        CancellationToken::new(),
                    )
                    .await;
                outcome.is_ok()
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap());
        }
        search_rounds.push(started.elapsed().as_micros() as u64);
    }
    let search_records = search_state.lock().unwrap().requests[search_before..].to_vec();
    let search_server_peak = max_wire_overlap(&search_records);
    search_server.abort();
    let search_samples = leveler_tools::take_budget_wait_samples();
    let mut search_layers = serde_json::Map::new();
    for (name, layer) in [
        ("global", leveler_tools::LAYER_GLOBAL),
        ("session", leveler_tools::LAYER_SESSION),
        ("provider", leveler_tools::LAYER_PROVIDER),
        ("provider_rate", leveler_tools::LAYER_PROVIDER_RATE),
        ("host", leveler_tools::LAYER_HOST),
    ] {
        let mut waits: Vec<u64> = search_samples
            .iter()
            .filter(|(sample_layer, _)| *sample_layer == layer)
            .map(|(_, wait)| *wait)
            .collect();
        search_layers.insert(
            name.to_string(),
            serde_json::json!({
                "n": waits.len(),
                "p50_us": percentile(&mut waits, 0.50),
                "p95_us": percentile(&mut waits, 0.95),
                "max_us": waits.last().copied().unwrap_or(0),
            }),
        );
    }
    let provider_contention = serde_json::json!({
        "sessions": 5,
        "searches_per_session": 2,
        "rounds": search_rounds.len(),
        "round_p50_us": percentile(&mut search_rounds, 0.50),
        "round_p95_us": percentile(&mut search_rounds, 0.95),
        "wire_requests": search_records.len(),
        "wire_max_in_flight": search_server_peak,
        "layers": search_layers,
    });

    server.abort();
    println!(
        "{}",
        serde_json::json!({
            "single_session": single,
            "multi_session": multi,
            "provider_contention": provider_contention,
            "process_budget": {
                "global_in_flight": 8,
                "session_in_flight": 4,
                "host_in_flight": 4,
                "provider_in_flight": 2,
                "provider_starts_per_second": 20,
            },
        })
    );
}
