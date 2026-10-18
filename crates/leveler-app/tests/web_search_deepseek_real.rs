//! Real DeepSeek Search through the production runtime with a NON-DeepSeek
//! model identity.
//!
//! Ignored by default: it spends real search requests and needs an official
//! `DEEPSEEK_API_KEY`. Run explicitly:
//!
//! ```text
//! DEEPSEEK_API_KEY=... cargo test -p leveler-app \
//!   --test web_search_deepseek_real -- --ignored --nocapture
//! ```
//!
//! The model is a scripted OpenAI-compatible mock; that part is NOT live model
//! acceptance. The search request and its result are real.

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

fn write_global_config(home: &std::path::Path, model_base_url: &str) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"
default_model = "mock/claude-sonnet"

[providers.mock]
protocol = "openai_chat"
base_url = "{model_base_url}"

[models."claude-sonnet"]
provider = "mock"
model_id = "claude-sonnet"
protocol = "openai_chat"
context_window = 131072
max_output_tokens = 1024

[web.search]
provider = "deepseek"
api_key_env = "LEVELER_TEST_SEARCH_KEY"
base_url = "https://api.deepseek.com/v1/tools"
max_results = 5
timeout_ms = 30000
"#
        ),
    )
    .unwrap();
}

#[tokio::test]
#[ignore = "real API: needs an official DEEPSEEK_API_KEY"]
async fn real_deepseek_search_enters_a_non_deepseek_agent_turn() {
    let key = std::env::var("DEEPSEEK_API_KEY")
        .expect("DEEPSEEK_API_KEY must carry an official DeepSeek API key");
    let model_server = MockServer::start(vec![
        capability_enable("web"),
        tool_call(
            "web_search",
            serde_json::json!({"query": "DeepSeek V4 release", "count": 2}),
        ),
        text("done"),
    ])
    .await;

    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(repo.path().join("configs/models")).unwrap();
    // The config names the credential variable; the value comes from the test
    // process environment and is never written to disk.
    unsafe {
        std::env::set_var("LEVELER_HOME", home.path());
        std::env::set_var("LEVELER_TEST_SEARCH_KEY", &key);
    }
    write_global_config(home.path(), &model_server.base_url());
    let app = Arc::new(
        Application::assemble(Layout::from_parts(
            repo.path().to_path_buf(),
            repo.path().join("configs"),
            repo.path().join("state"),
        ))
        .unwrap(),
    );

    let model = ModelRef::new("mock", "claude-sonnet");
    let session = app
        .create_session_with_collaboration(
            &model,
            "real search",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Chat,
        )
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::FullAccess,
        false,
    ));
    let mut events = client.subscribe_session(&session);
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session,
            content: "search the real web".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let mut tool_completed = false;
    let mut terminal = false;
    let mut failure = None;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline && !terminal {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(RuntimeEvent::ApprovalRequested { request })) => {
                // Full must not ask; deny anyway so the turn settles if it did.
                let _ = client
                    .send(ClientCommand::ApprovalDecision {
                        request_id: request.id,
                        decision: ApprovalDecision::Deny,
                    })
                    .await;
            }
            Ok(Ok(RuntimeEvent::ToolCallCompleted { .. })) => tool_completed = true,
            Ok(Ok(RuntimeEvent::TurnCompleted))
            | Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { .. }))
            | Ok(Ok(RuntimeEvent::TurnAnswered))
            | Ok(Ok(RuntimeEvent::TurnCancelled))
            | Ok(Ok(RuntimeEvent::TaskCancelled))
            | Ok(Ok(RuntimeEvent::TurnIncomplete { .. }))
            | Ok(Ok(RuntimeEvent::TurnTruncated { .. })) => terminal = true,
            Ok(Ok(RuntimeEvent::TurnFailed { error, .. })) => {
                failure = Some(error);
                terminal = true;
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    assert!(terminal, "turn never settled; failure: {failure:?}");
    assert!(tool_completed, "{failure:?}");

    let bodies = model_server.request_bodies().await;
    let last = bodies.last().expect("final model request");
    assert!(
        last.contains("[EXTERNAL DATA"),
        "the result was not marked as untrusted external data"
    );
    assert!(
        last.contains("https://"),
        "no real source URL reached the agent turn"
    );
    assert!(
        last.to_ascii_lowercase().contains("deepseek"),
        "the real search result never reached the model"
    );
    assert!(
        bodies.iter().all(|body| !body.contains(&key)),
        "the search key leaked into the model transcript"
    );
}
