//! Real-model acceptance for the verification contract.
//!
//! This drives the real provider, the real executor, the real tool surface,
//! the real SQLite stores and the real terminal projection. It proves the
//! property the scripted suites cannot: a live model declares a structured
//! requirement, completes it, and the terminal's verification report names
//! only executions the runtime actually ran. A completion with no declared
//! requirement is deliberately not verified here: command success alone is not
//! a delivered requirement.
//!
//! Ignored by default because it spends real tokens:
//!
//! ```text
//! DEEPSEEK_API_KEY=... cargo test -p leveler-app \
//!   --test verification_dogfood_real -- --ignored --nocapture
//! ```

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_engine::EngineEvent;
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;

const TIMEOUT: Duration = Duration::from_secs(600);

fn write_config(home: &Path) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        r#"
default_model = "deepseek/deepseek-flash"

[providers.deepseek]
protocol = "openai_chat"
base_url = "https://taotoken.net/api/v1"
api_key_env = "DEEPSEEK_API_KEY"

[models."deepseek-flash"]
provider = "deepseek"
model_id = "deepseek-flash"
protocol = "openai_chat"
context_window = 1048576
max_output_tokens = 65536
reasoning = true
reasoning_style = "thinking_flag"
supported_efforts = ["low", "high", "max"]
reasoning_effort = "low"
thinking_supports_forced_tool_choice = false
passback_reasoning_content = true
"#,
    )
    .unwrap();
}

#[derive(Default, Debug)]
struct TurnFacts {
    tools: Vec<String>,
    text: String,
    outcome: Option<String>,
    failure: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
}

async fn run_turn(
    client: &Arc<InProcessRuntimeClient>,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    command: ClientCommand,
) -> TurnFacts {
    let mut facts = TurnFacts::default();
    client
        .send(command)
        .await
        .expect("the runtime must accept the command");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline
        && facts.outcome.is_none()
        && facts.failure.is_none()
    {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Ok(RuntimeEvent::ToolCallStarted { name, .. })) => facts.tools.push(name),
            Ok(Ok(RuntimeEvent::AssistantTextDelta { delta, .. })) => facts.text.push_str(&delta),
            Ok(Ok(RuntimeEvent::TokenUsage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
                ..
            })) => {
                facts.input_tokens += u64::from(input_tokens);
                facts.output_tokens += u64::from(output_tokens);
                facts.cached_input_tokens += u64::from(cached_input_tokens);
            }
            Ok(Ok(RuntimeEvent::TurnCompleted)) => facts.outcome = Some("completed".into()),
            Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { reason, .. })) => {
                facts.outcome = Some(format!("completed_with_warnings: {reason}"));
            }
            Ok(Ok(RuntimeEvent::TurnAnswered)) => facts.outcome = Some("answered".into()),
            Ok(Ok(RuntimeEvent::TurnIncomplete { reason })) => {
                facts.outcome = Some(format!("incomplete: {reason}"));
            }
            Ok(Ok(RuntimeEvent::TurnFailed { error, .. })) => facts.failure = Some(error),
            Ok(Ok(RuntimeEvent::TurnTruncated { error })) => facts.failure = Some(error),
            Ok(Ok(RuntimeEvent::TurnCancelled | RuntimeEvent::TaskCancelled)) => {
                facts.outcome = Some("cancelled".into());
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                facts.failure = Some(error.to_string());
                break;
            }
            Err(_) => break,
        }
    }
    println!(
        "  tools={:?} outcome={:?} failure={:?}\n  answer: {}",
        facts.tools,
        facts.outcome,
        facts.failure,
        facts.text.trim()
    );
    facts
}

/// The command labels the runtime actually dispatched, from the authoritative
/// `ToolCallStarted` events. The report may name only these.
fn executed_commands(events: &[EngineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::ToolCallStarted {
                name, arguments, ..
            } => {
                let arguments: serde_json::Value = serde_json::from_str(arguments).ok()?;
                match name.as_str() {
                    "run_command" => {
                        let program = arguments.get("program")?.as_str()?.trim();
                        if program.is_empty() {
                            return None;
                        }
                        let args: Vec<&str> = arguments
                            .get("args")
                            .and_then(|value| value.as_array())
                            .map(|args| args.iter().filter_map(|arg| arg.as_str()).collect())
                            .unwrap_or_default();
                        Some(if args.is_empty() {
                            program.to_string()
                        } else {
                            format!("{program} {}", args.join(" "))
                        })
                    }
                    "shell_command" => arguments
                        .get("cmd")
                        .and_then(|value| value.as_str())
                        .map(str::trim)
                        .filter(|command| !command.is_empty())
                        .map(str::to_string),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn a_real_goal_completion_carries_only_executions_that_happened() {
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("LEVELER_HOME", home.path());
    }
    write_config(home.path());
    std::fs::create_dir_all(repo.path().join("configs")).unwrap();
    std::fs::create_dir_all(repo.path().join("state")).unwrap();
    leveler_test_support::git::init_repo(repo.path());
    std::fs::write(repo.path().join("seed.txt"), "seed\n").unwrap();
    leveler_test_support::git::run(repo.path(), &["add", "-A"]);
    leveler_test_support::git::run(repo.path(), &["commit", "-qm", "seed"]);

    let app = Arc::new(
        Application::assemble(Layout::from_parts(
            repo.path().to_path_buf(),
            repo.path().join("configs"),
            repo.path().join("state"),
        ))
        .unwrap(),
    );
    let model = ModelRef::new("deepseek", "deepseek-flash");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = app
        .create_session_with_collaboration(
            &model,
            "verify hello.txt",
            PermissionProfile::FullAccess,
            leveler_agent::CollaborationMode::Goal,
        )
        .await
        .unwrap();
    let mut events = client.subscribe_session(&session);

    let turn = run_turn(
        &client,
        &mut events,
        ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "在仓库根目录创建 hello.txt，内容为一行 hello；然后运行 `cat hello.txt` \
                      确认内容。运行 `cat hello.txt` 确认内容 这句话是用户原文：请用 \
                      declare_scope 声明一个 command 需求，target 为 `cat hello.txt`，\
                      source_quote 必须逐字复制上面这句话。然后用 get_goal_scope 读出该需求的 \
                      id，最后调用 update_goal(status=\"complete\")，在 requirements 里把该 id \
                      记为 done，并在 verification.detail 里用 `call_id:<id>` 引用刚才那次 cat 运行。"
                .to_string(),
            attachments: vec![],
        },
    )
    .await;

    assert!(turn.failure.is_none(), "the goal turn failed: {turn:?}");
    assert!(
        turn.tools.iter().any(|tool| tool == "write_file"),
        "the model must actually write the file: {:?}",
        turn.tools
    );
    assert!(
        turn.tools
            .iter()
            .any(|tool| tool == "run_command" || tool == "shell_command"),
        "the model must actually run the check: {:?}",
        turn.tools
    );
    let outcome = turn.outcome.clone().unwrap_or_default();
    assert!(
        outcome.starts_with("completed"),
        "the goal did not complete: {turn:?}"
    );

    // The persisted terminal is the authority. Read it back through the event
    // log, not through the transient UI event.
    let db = app.open_database().await.unwrap();
    let log = leveler_engine::EventLog::new(&db, session.clone());
    let recorded = log.replay().await.expect("the session must replay");
    let (terminal_outcome, verification) = recorded
        .iter()
        .rev()
        .find_map(|event| match event {
            EngineEvent::TaskFinished {
                outcome,
                verification,
                ..
            } => Some((*outcome, verification.clone())),
            _ => None,
        })
        .expect("a task terminal");
    println!("verification report: {verification:?}");
    assert_eq!(
        terminal_outcome,
        leveler_lifecycle::TaskOutcome::Completed,
        "the persisted terminal must agree with the visible outcome"
    );
    let report = verification.expect("a completed goal carries its verification report");
    assert_eq!(
        report.status,
        leveler_lifecycle::VerificationStatus::Verified,
        "a completion that cited the recorded check must be verified: {report:?}"
    );

    let executed = executed_commands(&recorded);
    assert!(!executed.is_empty(), "the log records no executed command");
    for record in &report.records {
        if let Some(command) = record.command.as_deref() {
            assert!(
                executed.iter().any(|ran| ran == command),
                "the report names a command the runtime never ran: {record:?} / {executed:?}"
            );
            assert!(
                record.call_id.is_some(),
                "a named execution must carry its call id: {record:?}"
            );
        }
    }

    println!(
        "verification acceptance: outcome={terminal_outcome:?} status={:?} records={} executed={:?} \
         input_tokens={} cached_input_tokens={} output_tokens={}",
        report.status,
        report.records.len(),
        executed,
        turn.input_tokens,
        turn.cached_input_tokens,
        turn.output_tokens
    );
}
