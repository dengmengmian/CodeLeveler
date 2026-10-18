//! Cross-provider acceptance: the conversation model is taotoken's DeepSeek,
//! while `web_search` uses the official DeepSeek Search API.
//!
//! This is the exact split the architecture promises:
//! Model Provider ≠ Search Provider. Ignored by default because it spends real
//! model tokens and one real search request:
//!
//! ```text
//! TAOTOKEN_API_KEY=... DEEPSEEK_API_KEY=... cargo test -p leveler-app \
//!   --test taotoken_model_official_search_real -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ApprovalDecision, ClientCommand, InteractiveRuntimeClient, RuntimeEvent,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;

const TIMEOUT: Duration = Duration::from_secs(240);

fn write_config(home: &std::path::Path) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        r#"
default_model = "taotoken/deepseek-flash"

[providers.taotoken]
protocol = "openai_chat"
base_url = "https://taotoken.net/api/v1"
api_key_env = "TAOTOKEN_API_KEY"

[models."deepseek-flash"]
provider = "taotoken"
model_id = "deepseek-flash"
protocol = "openai_chat"
context_window = 131072
max_output_tokens = 4096
reasoning = true
reasoning_style = "thinking_flag"
supported_efforts = ["low", "high", "max"]
reasoning_effort = "low"
thinking_supports_forced_tool_choice = false
passback_reasoning_content = true

[web.search]
provider = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"
base_url = "https://api.deepseek.com/v1/tools"
max_results = 3
timeout_ms = 30000
"#,
    )
    .unwrap();
}

#[tokio::test]
#[ignore = "real API: needs TAOTOKEN_API_KEY and DEEPSEEK_API_KEY"]
async fn taotoken_model_uses_official_deepseek_search() {
    assert!(
        std::env::var("TAOTOKEN_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "TAOTOKEN_API_KEY must carry the gateway key"
    );
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the official DeepSeek key"
    );

    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(repo.path().join("configs/models")).unwrap();
    unsafe {
        std::env::set_var("LEVELER_HOME", home.path());
    }
    write_config(home.path());
    let app = Arc::new(
        Application::assemble(Layout::from_parts(
            repo.path().to_path_buf(),
            repo.path().join("configs"),
            repo.path().join("state"),
        ))
        .unwrap(),
    );

    let model = ModelRef::new("taotoken", "deepseek-flash");
    let session = app
        .create_session_with_collaboration(
            &model,
            "cross-provider search",
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
            content: "Use the web_search tool to search for \"DeepSeek V4 release\". \
                      Then answer with the title and URL of the first result. \
                      Do not answer from memory."
                .to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let mut text = String::new();
    let mut tools_started: Vec<String> = Vec::new();
    let mut search_completed = false;
    let mut search_preview = String::new();
    let mut terminal = false;
    let mut failure = None;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline && !terminal {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(RuntimeEvent::ApprovalRequested { request })) => {
                let _ = client
                    .send(ClientCommand::ApprovalDecision {
                        request_id: request.id,
                        decision: ApprovalDecision::Deny,
                    })
                    .await;
            }
            Ok(Ok(RuntimeEvent::ToolCallStarted { name, .. })) => tools_started.push(name),
            Ok(Ok(RuntimeEvent::ToolCallCompleted { ok, preview, .. })) => {
                if tools_started
                    .last()
                    .is_some_and(|name| name == "web_search")
                {
                    search_completed = ok;
                    search_preview = preview;
                }
            }
            Ok(Ok(RuntimeEvent::AssistantTextDelta { delta, .. })) => text.push_str(&delta),
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

    println!("tool calls: {tools_started:?}");
    println!("search preview:\n{search_preview}");
    println!("assistant answer:\n{text}");
    assert!(terminal, "turn never settled; failure: {failure:?}");
    assert!(failure.is_none(), "{failure:?}");
    assert!(
        tools_started.iter().any(|name| name == "web_search"),
        "the gateway model never called web_search; tools={tools_started:?}"
    );
    assert!(
        search_completed,
        "web_search did not complete successfully; preview={search_preview}"
    );
    assert!(
        search_preview.contains("http"),
        "the official search returned no URL; preview={search_preview}"
    );
    assert!(
        text.contains("http"),
        "the model's answer does not cite a URL: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("deepseek"),
        "the answer does not reflect the real search: {text}"
    );
}
