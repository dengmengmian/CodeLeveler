//! End-to-end smoke test of the TUI runtime client against a live model.
//!
//! Drives `InProcessRuntimeClient` exactly as the TUI does — subscribe, submit a
//! message, consume `RuntimeEvent`s — and prints the stream, so the protocol
//! bridge can be verified without a terminal. Auto-denies any approval so it
//! never blocks.
//!
//! Run: `DEEPSEEK_BASE_URL=... DEEPSEEK_API_KEY=... \
//!   cargo run -p leveler-app --example tui_smoke -- deepseek/deepseek-v4-pro`

use std::sync::Arc;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let model_arg = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "deepseek/deepseek-v4-pro".to_string());
    let model = ModelRef::parse(&model_arg).expect("provider/model");

    let layout = Layout::resolve(std::env::current_dir()?, None);
    let app = Arc::new(Application::assemble(layout)?);
    let session_id = app.create_session(&model, "tui smoke").await?;

    let client: Arc<dyn InteractiveRuntimeClient> = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::RequestApproval,
        false,
    ));

    // Confirm the snapshot path works (this is what the TUI calls on startup).
    let snap = client.snapshot(&session_id).await?;
    println!(
        "[snapshot] repo={} model={:?} vision={} models={}",
        snap.repository,
        snap.model,
        snap.vision,
        snap.available_models.len()
    );

    // One or more messages, sent sequentially in the same session, to exercise
    // conversational context carry-over.
    let messages: Vec<String> = {
        let extra: Vec<String> = std::env::args().skip(2).collect();
        if extra.is_empty() {
            vec!["用一句话中文回答：你是谁？".to_string()]
        } else {
            extra
        }
    };

    for content in messages {
        println!("\n>>> submit: {content}");
        run_one_turn(client.as_ref(), &session_id, content).await?;
    }
    Ok(())
}

async fn run_one_turn(
    client: &dyn InteractiveRuntimeClient,
    session_id: &leveler_core::SessionId,
    content: String,
) -> anyhow::Result<()> {
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content,
            attachments: Vec::new(),
        })
        .await?;

    let mut assistant = String::new();
    let mut delta_count = 0usize;
    loop {
        match rx.recv().await {
            Ok(event) => match event {
                RuntimeEvent::UserMessageAdded { message } => {
                    println!("[user] {}", message.text);
                }
                RuntimeEvent::AssistantMessageStarted { .. } => {
                    println!("[assistant] <started>");
                }
                RuntimeEvent::AssistantTextDelta { delta, .. } => {
                    assistant.push_str(&delta);
                    delta_count += 1;
                    print!("{delta}");
                }
                RuntimeEvent::AssistantMessageCompleted { .. } => {
                    println!("\n[assistant] <completed>");
                }
                RuntimeEvent::ToolCallStarted { name, .. } => {
                    println!("[tool] {name} started");
                }
                RuntimeEvent::ToolCallCompleted {
                    ok, duration_ms, ..
                } => {
                    println!("[tool] done ok={ok} {duration_ms}ms");
                }
                RuntimeEvent::ClarificationRequested { request } => {
                    println!(
                        "[clarify] Q: {} opts={:?} — auto-answer '选项一'",
                        request.question, request.options
                    );
                    client
                        .send(ClientCommand::AnswerClarification {
                            request_id: request.id,
                            answer: "选项一".to_string(),
                        })
                        .await?;
                }
                RuntimeEvent::ApprovalRequested { request } => {
                    println!("[approval] {} — auto-deny", request.summary);
                    client
                        .send(ClientCommand::ApprovalDecision {
                            request_id: request.id,
                            decision: leveler_client_protocol::ApprovalDecision::Deny,
                        })
                        .await?;
                }
                RuntimeEvent::TurnCompleted => {
                    println!("[turn] completed");
                    break;
                }
                RuntimeEvent::TurnFailed { error } => {
                    println!("[turn] FAILED: {error}");
                    break;
                }
                RuntimeEvent::TurnCancelled => {
                    println!("[turn] cancelled");
                    break;
                }
                other => println!("[event] {other:?}"),
            },
            Err(e) => {
                println!("[recv error] {e}");
                break;
            }
        }
    }

    println!(
        "\n=== assistant: {} chars in {} deltas ===",
        assistant.chars().count(),
        delta_count
    );
    Ok(())
}
