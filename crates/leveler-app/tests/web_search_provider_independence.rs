//! Deterministic proof that the search backend is independent of the
//! conversation model, driven through the production runtime:
//! `InProcessRuntimeClient` → Agent loop → ToolHost admission → Tool Registry
//! → `web_search` → SearchProvider → Host HTTP gate → local server.
//!
//! The model here is a scripted OpenAI-compatible mock. That is deliberate and
//! is NOT live model acceptance: it proves the search path receives no model
//! identity, while the real-API test (separately gated) covers a real vendor.
//! The search backend is a real HTTP server on this machine, so the request,
//! auth header, response mapping, tool result and permission path are all real.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ApprovalDecision, ClientCommand, InteractiveRuntimeClient, RuntimeEvent,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

const TIMEOUT: Duration = Duration::from_secs(60);
const SEARCH_KEY: &str = "sk-deepseek-search-test-key";

/// `LEVELER_HOME` and the search credential variable are process-global, so
/// tests that assemble an Application must not overlap.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().await
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

fn text(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

fn tool_call(name: &str, arguments: serde_json::Value) -> MockResponse {
    let call = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": "call_1",
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()},
            }]},
        }]
    })
    .to_string();
    let finish =
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
            .to_string();
    sse(vec![call, finish])
}

fn capability_enable(id: &str) -> MockResponse {
    tool_call(
        "capability",
        serde_json::json!({"action": "enable", "id": id}),
    )
}

/// One search answer with a URL and a field that must never reach the model.
fn search_body() -> String {
    serde_json::json!({
        "results": [{
            "title": "Independent result",
            "url": "https://example.test/model-independent",
            "snippet": "A result that came back to whichever model asked.",
            "summary": "provider-internal summary that must not reach the model",
            "date": "2026-10-18T00:00:00+00:00",
            "rerank_score": 2.9,
            "authority": 4,
            "site_icon": "https://example.test/favicon.ico"
        }]
    })
    .to_string()
}

/// Write one global config that names five model identities, all served by the
/// same scripted mock, and one explicit search backend.
fn write_global_config(
    home: &std::path::Path,
    model_base_url: &str,
    search_base_url: &str,
    provider: &str,
) {
    std::fs::create_dir_all(home).unwrap();
    let models = [
        "deepseek-flash",
        "gpt-5",
        "claude-sonnet",
        "grok-4",
        "kimi-k2",
    ]
    .iter()
    .map(|id| {
        format!(
            r#"
[models."{id}"]
provider = "mock"
model_id = "{id}"
protocol = "openai_chat"
context_window = 131072
max_output_tokens = 1024
"#
        )
    })
    .collect::<String>();
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"
default_model = "mock/deepseek-flash"

[providers.mock]
protocol = "openai_chat"
base_url = "{model_base_url}"

[web.search]
provider = "{provider}"
api_key_env = "LEVELER_TEST_SEARCH_KEY"
base_url = "{search_base_url}"
max_results = 5
timeout_ms = 5000
{models}
"#
        ),
    )
    .unwrap();
}

fn repo_config(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.join("configs/models")).unwrap();
}

/// Assemble one application over a private global home and repo.
fn assemble_app(
    model_base_url: &str,
    search_base_url: &str,
) -> (tempfile::TempDir, tempfile::TempDir, Arc<Application>) {
    assemble_app_with_provider(model_base_url, search_base_url, "deepseek")
}

fn assemble_app_with_provider(
    model_base_url: &str,
    search_base_url: &str,
    provider: &str,
) -> (tempfile::TempDir, tempfile::TempDir, Arc<Application>) {
    let global_home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    repo_config(repo.path());
    // Sequential within this test file: environment mutation is process-global
    // and every caller holds ENV_LOCK.
    unsafe {
        std::env::set_var("LEVELER_HOME", global_home.path());
        std::env::set_var("LEVELER_TEST_SEARCH_KEY", SEARCH_KEY);
    }
    write_global_config(
        global_home.path(),
        model_base_url,
        search_base_url,
        provider,
    );
    let app = Arc::new(
        Application::assemble(Layout::from_parts(
            repo.path().to_path_buf(),
            repo.path().join("configs"),
            repo.path().join("state"),
        ))
        .unwrap(),
    );
    (global_home, repo, app)
}

struct Observed {
    approvals: usize,
    tool_completed: bool,
    context_accounted: bool,
    terminal: bool,
    failure: Option<String>,
    debug: Vec<String>,
}

async fn run_turn(
    app: Arc<Application>,
    model: ModelRef,
    mode: PermissionProfile,
    message: &str,
) -> Observed {
    let session_id = app
        .create_session_with_collaboration(
            &model,
            "web-search",
            mode,
            leveler_agent::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    run_on_session(app, model, mode, session_id, message).await
}

async fn run_on_session(
    app: Arc<Application>,
    model: ModelRef,
    mode: PermissionProfile,
    session_id: leveler_core::SessionId,
    message: &str,
) -> Observed {
    let client = Arc::new(InProcessRuntimeClient::new(app.clone(), model, mode, false));
    let mut events = client.subscribe_session(&session_id);
    client
        .send(ClientCommand::SubmitMessage {
            session_id,
            content: message.to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let mut observed = Observed {
        approvals: 0,
        tool_completed: false,
        context_accounted: false,
        terminal: false,
        failure: None,
        debug: Vec::new(),
    };
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline && !observed.terminal {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(RuntimeEvent::ApprovalRequested { request })) => {
                observed.approvals += 1;
                observed
                    .debug
                    .push(format!("ApprovalRequested {request:?}"));
                let _ = client
                    .send(ClientCommand::ApprovalDecision {
                        request_id: request.id,
                        decision: ApprovalDecision::Deny,
                    })
                    .await;
            }
            Ok(Ok(RuntimeEvent::ToolCallCompleted { .. })) => {
                observed.tool_completed = true;
                observed.debug.push("ToolCallCompleted".into());
            }
            Ok(Ok(RuntimeEvent::ContextUsage { .. })) => {
                observed.context_accounted = true;
            }
            Ok(Ok(event @ RuntimeEvent::TurnCompleted))
            | Ok(Ok(event @ RuntimeEvent::TurnCompletedWithWarnings { .. }))
            | Ok(Ok(event @ RuntimeEvent::TurnAnswered))
            | Ok(Ok(event @ RuntimeEvent::TurnCancelled))
            | Ok(Ok(event @ RuntimeEvent::TaskCancelled))
            | Ok(Ok(event @ RuntimeEvent::TurnIncomplete { .. }))
            | Ok(Ok(event @ RuntimeEvent::TurnTruncated { .. })) => {
                observed.debug.push(format!("{event:?}"));
                observed.terminal = true;
            }
            Ok(Ok(RuntimeEvent::TurnFailed { error, .. })) => {
                observed.debug.push(format!("TurnFailed {error}"));
                observed.failure = Some(error);
                observed.terminal = true;
            }
            Ok(Ok(event)) => observed.debug.push(format!("{event:?}")),
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    assert!(
        observed.terminal,
        "turn never settled; failure: {:?}",
        observed.failure
    );
    observed
}

/// All five model identities go through the same search backend. Each turn's
/// tool result is delivered back to the model that asked, and every search
/// request is byte-identical: no model name, provider or protocol reaches it.
#[tokio::test]
async fn the_search_backend_is_identical_across_five_model_identities() {
    let _env = env_lock().await;
    // Fifteen scripted responses: every turn enables the optional Web pack,
    // calls web_search, then answers.
    let mut script = Vec::new();
    for _ in 0..5 {
        script.push(capability_enable("web"));
        script.push(tool_call(
            "web_search",
            serde_json::json!({"query": "model independent search", "count": 2}),
        ));
        script.push(text("done"));
    }
    let model_server = MockServer::start(script).await;
    let search_server = MockServer::start_one(MockResponse::json_ok(&search_body())).await;

    let (_home, _repo, app) = assemble_app(
        &model_server.base_url(),
        &format!("{}/v1/tools", search_server.base_url()),
    );

    for model_id in [
        "deepseek-flash",
        "gpt-5",
        "claude-sonnet",
        "grok-4",
        "kimi-k2",
    ] {
        let observed = run_turn(
            app.clone(),
            ModelRef::new("mock", model_id),
            PermissionProfile::FullAccess,
            "search the web",
        )
        .await;
        assert_eq!(observed.approvals, 0, "{model_id}: Full must not ask");
        assert!(
            observed.tool_completed,
            "{model_id}: tool did not complete; model requests={}; search requests={}; events={:?}",
            model_server.request_count(),
            search_server.request_count(),
            observed.debug
        );
        assert!(
            observed.context_accounted,
            "{model_id}: context accounting did not run"
        );
        assert_eq!(observed.failure, None, "{model_id}");

        // The tool result reached the model that asked: the next model request
        // carries the URL and the snippet, but not provider-internal fields.
        let bodies = model_server.request_bodies().await;
        let latest = bodies.last().expect("model request");
        assert!(
            latest.contains("https://example.test/model-independent"),
            "{model_id}: the search source never reached the model"
        );
        assert!(
            latest.contains("A result that came back to whichever model asked."),
            "{model_id}: the snippet never reached the model"
        );
        assert!(
            !latest.contains("provider-internal summary"),
            "{model_id}: the provider summary leaked"
        );
        assert!(
            !latest.contains(SEARCH_KEY),
            "{model_id}: the search key leaked into the model transcript"
        );
    }

    let heads = search_server.request_heads().await;
    let bodies = search_server.request_bodies().await;
    assert_eq!(heads.len(), 5, "one direct search per turn");
    assert_eq!(
        model_server.request_count(),
        15,
        "three normal model rounds per turn (capability, tool call, answer); \
         the search itself added none"
    );
    assert_eq!(bodies.len(), 5);
    for (index, body) in bodies.iter().enumerate() {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            serde_json::json!({"query": "model independent search", "count": 2, "need_content": false}),
            "search request {index} changed with the model"
        );
        assert!(
            heads[index].starts_with("POST /v1/tools/search HTTP/1.1"),
            "{}",
            heads[index]
        );
        assert!(
            heads[index]
                .to_ascii_lowercase()
                .contains(&format!("authorization: bearer {SEARCH_KEY}").to_ascii_lowercase()),
            "{}",
            heads[index]
        );
    }
}

/// Auto (替我审批) runs an ordinary internet-scoped search without a second
/// approval. Restricted (请求批准) still asks when the destination is outside
/// its current scope; the denied approval never dispatches a request.
#[tokio::test]
async fn auto_runs_without_a_second_gate_and_restricted_respects_scope() {
    let _env = env_lock().await;
    let mut script = Vec::new();
    for _ in 0..2 {
        script.push(capability_enable("web"));
        script.push(tool_call(
            "web_search",
            serde_json::json!({"query": "permission check"}),
        ));
        script.push(text("done"));
    }
    let model_server = MockServer::start(script).await;
    let search_server = MockServer::start_one(MockResponse::json_ok(&search_body())).await;

    // ── Auto: loopback search endpoint is inside the Internet scope ─────────
    let (_auto_home, _auto_repo, auto_app) = assemble_app(
        &model_server.base_url(),
        &format!("{}/v1/tools", search_server.base_url()),
    );
    let auto = run_turn(
        auto_app,
        ModelRef::new("mock", "deepseek-flash"),
        PermissionProfile::Assisted,
        "search the web",
    )
    .await;
    assert_eq!(auto.approvals, 0, "Auto must not add a second network gate");
    assert!(auto.tool_completed, "failure: {:?}", auto.failure);
    assert_eq!(search_server.request_count(), 1);

    // ── Restricted: a public destination is outside the Loopback scope ──────
    // TEST-NET-1 is a literal address, so admission resolves no DNS; the
    // Loopback scope refuses it before any socket is opened.
    let (_restricted_home, _restricted_repo, restricted_app) =
        assemble_app(&model_server.base_url(), "http://192.0.2.1/v1/tools");
    let model_requests_before = model_server.request_bodies().await.len();
    let restricted = run_turn(
        restricted_app,
        ModelRef::new("mock", "deepseek-flash"),
        PermissionProfile::RequestApproval,
        "search the web",
    )
    .await;
    assert_eq!(
        restricted.approvals, 1,
        "Restricted must ask before leaving its scope"
    );
    // A denied call still produces a tool-call completion event carrying the
    // refusal; the authoritative fact is that no request reached the backend.
    assert_eq!(
        search_server.request_count(),
        1,
        "the denied request must never reach the search backend"
    );
    let bodies = model_server.request_bodies().await;
    assert!(
        !bodies[model_requests_before..]
            .iter()
            .any(|body| body.contains("https://example.test/model-independent")),
        "a denied search must return no source to the model"
    );
}

/// Resume: a second turn in the same session keeps the same configured
/// provider and still delivers the result to the model. Capability disclosure
/// is durable per goal, and the search backend is application-scoped.
#[tokio::test]
async fn a_resumed_session_keeps_the_configured_search_backend() {
    let _env = env_lock().await;
    let mut script = Vec::new();
    for _ in 0..2 {
        script.push(capability_enable("web"));
        script.push(tool_call(
            "web_search",
            serde_json::json!({"query": "resume search"}),
        ));
        script.push(text("done"));
    }
    let model_server = MockServer::start(script).await;
    let search_server = MockServer::start_one(MockResponse::json_ok(&search_body())).await;
    let (_home, repo, app) = assemble_app(
        &model_server.base_url(),
        &format!("{}/v1/tools", search_server.base_url()),
    );

    let model = ModelRef::new("mock", "deepseek-flash");
    let session = app
        .create_session_with_collaboration(
            &model,
            "resume",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Chat,
        )
        .await
        .unwrap();

    let first = run_on_session(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        session.clone(),
        "first turn",
    )
    .await;
    assert!(first.tool_completed, "first turn: {:?}", first.failure);

    // Reassemble the application over the same state directory: the session is
    // reopened from persistence, not continued in memory.
    drop(app);
    let resumed = Arc::new(
        Application::assemble(Layout::from_parts(
            repo.path().to_path_buf(),
            repo.path().join("configs"),
            repo.path().join("state"),
        ))
        .unwrap(),
    );
    let second = run_on_session(
        resumed,
        model,
        PermissionProfile::FullAccess,
        session,
        "second turn",
    )
    .await;
    assert!(second.tool_completed, "resumed turn: {:?}", second.failure);

    assert_eq!(search_server.request_count(), 2, "one search per turn");
    let bodies = model_server.request_bodies().await;
    let delivered = bodies
        .iter()
        .filter(|body| body.contains("https://example.test/model-independent"))
        .count();
    assert!(
        delivered >= 2,
        "each turn's model must receive the search source"
    );
    assert!(
        bodies.iter().all(|body| !body.contains(SEARCH_KEY)),
        "the search key must never enter the model transcript"
    );
}

/// Goal mode: the model can search mid-goal, receives the result, and the
/// result is in the request that resolves the goal.
#[tokio::test]
async fn goal_mode_delivers_the_search_result_before_completion() {
    let _env = env_lock().await;
    let model_server = MockServer::start(vec![
        capability_enable("web"),
        tool_call("web_search", serde_json::json!({"query": "goal search"})),
        tool_call(
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "searched"}),
        ),
        text("done"),
    ])
    .await;
    let search_server = MockServer::start_one(MockResponse::json_ok(&search_body())).await;
    let (_home, _repo, app) = assemble_app(
        &model_server.base_url(),
        &format!("{}/v1/tools", search_server.base_url()),
    );

    let model = ModelRef::new("mock", "deepseek-flash");
    let session = app
        .create_session_with_collaboration(
            &model,
            "goal search",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Goal,
        )
        .await
        .unwrap();
    let observed = run_on_session(
        app,
        model,
        PermissionProfile::FullAccess,
        session,
        "search and finish",
    )
    .await;

    assert!(
        observed.tool_completed,
        "search did not run: {:?}",
        observed.failure
    );
    assert_eq!(search_server.request_count(), 1);
    let bodies = model_server.request_bodies().await;
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("https://example.test/model-independent")),
        "the search result never reached the model before the goal resolved"
    );
    assert!(bodies.iter().all(|body| !body.contains(SEARCH_KEY)));
}

/// A non-DeepSeek model with Tavily: the search backend is a configuration
/// choice, not a consequence of the conversation model.
#[tokio::test]
async fn a_non_deepseek_model_uses_tavily_when_configured() {
    let _env = env_lock().await;
    let model_server = MockServer::start(vec![
        capability_enable("web"),
        tool_call(
            "web_search",
            serde_json::json!({"query": "tavily anywhere"}),
        ),
        text("done"),
    ])
    .await;
    let search_server = MockServer::start_one(MockResponse::json_ok(
        &serde_json::json!({
            "results": [{
                "title": "Tavily result",
                "url": "https://example.test/tavily",
                "content": "Tavily snippet.",
                "score": 0.9
            }]
        })
        .to_string(),
    ))
    .await;
    let (_home, _repo, app) = assemble_app_with_provider(
        &model_server.base_url(),
        &search_server.base_url(),
        "tavily",
    );

    let observed = run_turn(
        app,
        ModelRef::new("mock", "claude-sonnet"),
        PermissionProfile::FullAccess,
        "search with tavily",
    )
    .await;

    assert!(observed.tool_completed, "{:?}", observed.failure);
    assert_eq!(search_server.request_count(), 1);
    let heads = search_server.request_heads().await;
    assert!(
        heads[0].starts_with("POST /search HTTP/1.1"),
        "{}",
        heads[0]
    );
    let bodies = search_server.request_bodies().await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&bodies[0]).unwrap(),
        serde_json::json!({"query": "tavily anywhere", "search_depth": "basic", "max_results": 5})
    );
    let model_requests = model_server.request_bodies().await;
    let last = model_requests.last().unwrap();
    assert!(last.contains("https://example.test/tavily"));
    assert!(last.contains("Tavily snippet."));
    assert!(!last.contains("0.9"), "the provider score leaked");
    assert!(model_requests.iter().all(|body| !body.contains(SEARCH_KEY)));
}

/// A failed search is a tool result, not a routing event: the same model
/// answers the error, no other model is contacted, and the credential stays out
/// of the transcript.
#[tokio::test]
async fn a_search_failure_never_switches_the_model() {
    let _env = env_lock().await;
    let model_server = MockServer::start(vec![
        capability_enable("web"),
        tool_call("web_search", serde_json::json!({"query": "will fail"})),
        text("the search failed"),
    ])
    .await;
    let search_server = MockServer::start_one(MockResponse::Status {
        code: 401,
        body: serde_json::json!({"error": {"message": "invalid key"}}).to_string(),
    })
    .await;
    let (_home, _repo, app) = assemble_app(
        &model_server.base_url(),
        &format!("{}/v1/tools", search_server.base_url()),
    );

    let observed = run_turn(
        app,
        ModelRef::new("mock", "deepseek-flash"),
        PermissionProfile::FullAccess,
        "search and fail",
    )
    .await;

    assert!(observed.terminal);
    assert_eq!(search_server.request_count(), 1);
    let bodies = model_server.request_bodies().await;
    assert!(!bodies.is_empty());
    for body in &bodies {
        let request: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            request["model"], "deepseek-flash",
            "a search failure must not change the model"
        );
    }
    let last = bodies.last().unwrap();
    assert!(last.contains("401"), "the model must see the real failure");
    assert!(
        !last.contains(SEARCH_KEY),
        "the search key must not enter the transcript"
    );
}
