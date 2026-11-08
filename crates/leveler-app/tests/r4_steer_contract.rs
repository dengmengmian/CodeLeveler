//! Phase 2C-R4 Gate A: durable mid-turn input and assist delivery through the
//! production client/runtime protocol.
//!
//! The provider is a scripted OpenAI-wire server, but everything that decides
//! whether an appended instruction is accepted, journaled, ordered, delivered
//! or cancelled is the production runtime. Request bodies are recorded by the
//! mock, so "the model actually saw it" is an assertion on the wire, not on the
//! transcript.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, MessageId, RuntimeEvent, UiInputState,
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

fn text(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

fn tool_call(name: &str, arguments: serde_json::Value) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": "call-tool",
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]})
        .to_string(),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string(),
    ])
}

async fn client_with(
    responses: Vec<MockResponse>,
) -> (
    tempfile::TempDir,
    MockServer,
    Arc<InProcessRuntimeClient>,
    leveler_core::SessionId,
) {
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
    let session = app.create_session(&model, "goal").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(app, model, profile(), false));
    (tmp, server, client, session)
}

fn profile() -> PermissionProfile {
    PermissionProfile::Assisted
}

/// Wait for the first terminal turn event. Returns whether one was seen.
async fn await_turn_terminal(rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(event)) if is_turn_terminal(&event) => return true,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return false,
        }
    }
}

fn is_turn_terminal(event: &RuntimeEvent) -> bool {
    matches!(
        event,
        RuntimeEvent::TurnCompleted
            | RuntimeEvent::TurnCompletedWithWarnings { .. }
            | RuntimeEvent::TurnAnswered
            | RuntimeEvent::TurnPaused { .. }
            | RuntimeEvent::TurnFailed { .. }
            | RuntimeEvent::TurnCancelled
            | RuntimeEvent::TurnTruncated { .. }
            | RuntimeEvent::TurnIncomplete { .. }
    )
}

/// The assist event must reach a client that asked for it, after a real turn
/// completed. This is the runtime half of the 「※ 回顾」 chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_away_summary_is_delivered_to_the_client_that_requested_it() {
    let (_tmp, _server, client, session) = client_with(vec![
        text("the answer"),
        MockResponse::json_ok(
            r#"{"id":"assist","choices":[{"message":{"role":"assistant","content":"R4-RECAP-one sentence"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#,
        ),
        MockResponse::json_ok(
            r#"{"id":"assist","choices":[{"message":{"role":"assistant","content":"suggestion"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#,
        ),
    ])
    .await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "hello".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    assert!(await_turn_terminal(&mut rx).await, "the turn settles");

    client
        .send(ClientCommand::RequestAwaySummary {
            session_id: session.clone(),
        })
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::AwaySummary { text })) => {
                assert_eq!(text, "R4-RECAP-one sentence");
                return;
            }
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            other => panic!("no away summary: {other:?}"),
        }
    }
}

/// S2/S5 (durable side): three appends sent while a tool call runs arrive in
/// the next model request in submission order, each exactly once, and the tool
/// itself is neither interrupted nor repeated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_appends_reach_the_next_model_request_in_order_once_each() {
    let (_tmp, server, client, session) = client_with(vec![
        tool_call(
            "run_command",
            serde_json::json!({"program": "sh", "args": ["-c", "sleep 2"]}),
        ),
        text("noted all three"),
    ])
    .await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "start".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let mut ids = Vec::new();
    for text in ["ALPHA-first", "BETA-second", "GAMMA-third"] {
        let id = MessageId::new(leveler_core::new_uuid_string());
        ids.push(id.clone());
        client
            .send(ClientCommand::SteerCurrentTurn {
                session_id: session.clone(),
                content: text.into(),
                message_id: Some(id),
            })
            .await
            .unwrap();
    }
    assert!(await_turn_terminal(&mut rx).await, "the turn settles");
    assert_eq!(ids.len(), 3);

    let bodies = server.request_bodies().await;
    let agent: Vec<String> = bodies
        .into_iter()
        .filter(|body| body.contains("\"tools\""))
        .collect();
    let last = agent.last().expect("a second round ran").clone();
    let alpha = last
        .find("ALPHA-first")
        .expect("first append in the request");
    let beta = last
        .find("BETA-second")
        .expect("second append in the request");
    let gamma = last
        .find("GAMMA-third")
        .expect("third append in the request");
    assert!(alpha < beta && beta < gamma, "order preserved: {last}");
    assert_eq!(last.matches("ALPHA-first").count(), 1);
    assert_eq!(last.matches("BETA-second").count(), 1);
    assert_eq!(last.matches("GAMMA-third").count(), 1);
}

/// S3/S7: a retried submission with the SAME input id is absorbed by the
/// journal: one durable settlement, one transcript row, and the model sees the
/// instruction exactly once in the conversation it is part of (a later round
/// of the same turn carries the same row forward, which is not a redelivery).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retried_input_id_is_delivered_once() {
    let (_tmp, server, client, session) = client_with(vec![
        tool_call(
            "run_command",
            serde_json::json!({"program": "sh", "args": ["-c", "sleep 2"]}),
        ),
        text("noted"),
    ])
    .await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "start".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let id = MessageId::new("retry-same-id");
    let mut delivered: Vec<MessageId> = Vec::new();
    for _ in 0..3 {
        client
            .send(ClientCommand::SteerCurrentTurn {
                session_id: session.clone(),
                content: "RETRY-ONCE".into(),
                message_id: Some(id.clone()),
            })
            .await
            .unwrap();
    }
    assert!(await_turn_terminal_and_settlements(&mut rx, &mut delivered).await);
    assert_eq!(
        delivered.iter().filter(|seen| *seen == &id).count(),
        1,
        "one input id settles as delivered exactly once: {delivered:?}"
    );

    let bodies = server.request_bodies().await;
    let agent: Vec<serde_json::Value> = bodies
        .iter()
        .filter(|body| body.contains("\"tools\""))
        .filter_map(|body| serde_json::from_str(body).ok())
        .collect();
    let last = agent.last().expect("a second round ran");
    assert_eq!(
        user_messages_with(last, "RETRY-ONCE"),
        1,
        "the conversation carries the instruction once: {last}"
    );
}

/// Count user messages whose text is exactly `needle` in a model request.
fn user_messages_with(request: &serde_json::Value, needle: &str) -> usize {
    request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message.get("role").and_then(|r| r.as_str()) == Some("user"))
                .filter(|message| {
                    message
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|content| content.contains(needle))
                })
                .count()
        })
        .unwrap_or(0)
}

/// Collect delivered settlements until the turn settles, reporting the
/// submit-to-settlement latency of each append.
async fn await_turn_terminal_and_settlements(
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    delivered: &mut Vec<MessageId>,
) -> bool {
    let started = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::InputSettled {
                message_id,
                state: UiInputState::Delivered,
                ..
            })) => {
                eprintln!(
                    "[perf] input {} settled as delivered {} ms after collection began",
                    message_id.as_str(),
                    started.elapsed().as_millis()
                );
                delivered.push(message_id);
            }
            Ok(Ok(event)) if is_turn_terminal(&event) => return true,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return false,
        }
    }
}

/// S4: the same text submitted twice with DIFFERENT ids is two legitimate
/// instructions, not a duplicate to swallow: two settlements, and one user row
/// each in the conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_text_with_distinct_ids_is_delivered_twice() {
    let (_tmp, server, client, session) = client_with(vec![
        tool_call(
            "run_command",
            serde_json::json!({"program": "sh", "args": ["-c", "sleep 2"]}),
        ),
        text("noted both"),
    ])
    .await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "start".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    for _ in 0..2 {
        client
            .send(ClientCommand::SteerCurrentTurn {
                session_id: session.clone(),
                content: "SAME-TEXT".into(),
                message_id: Some(MessageId::new(leveler_core::new_uuid_string())),
            })
            .await
            .unwrap();
    }
    let mut delivered: Vec<MessageId> = Vec::new();
    assert!(await_turn_terminal_and_settlements(&mut rx, &mut delivered).await);
    assert_eq!(
        delivered.len(),
        2,
        "two distinct ids must both settle as delivered"
    );

    let bodies = server.request_bodies().await;
    let agent: Vec<serde_json::Value> = bodies
        .iter()
        .filter(|body| body.contains("\"tools\""))
        .filter_map(|body| serde_json::from_str(body).ok())
        .collect();
    let last = agent.last().expect("a second round ran");
    assert_eq!(
        user_messages_with(last, "SAME-TEXT"),
        2,
        "two identical texts are two rows: {last}"
    );
}

/// R5/R6: the review is durable. Reopening history projects the SAME review
/// after its turn terminal, and a repeated request cannot create a second
/// durable review for the same turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_review_is_durable_and_replays_after_its_turn() {
    let (_tmp, server, client, session) = client_with(vec![
        text("the answer"),
        MockResponse::json_ok(
            r#"{"id":"assist","choices":[{"message":{"role":"assistant","content":"R4-DURABLE-RECAP"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#,
        ),
        MockResponse::json_ok(
            r#"{"id":"assist","choices":[{"message":{"role":"assistant","content":"R4-DURABLE-RECAP"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#,
        ),
    ])
    .await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "hello".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    assert!(await_turn_terminal(&mut rx).await, "the turn settles");

    for _ in 0..2 {
        client
            .send(ClientCommand::RequestAwaySummary {
                session_id: session.clone(),
            })
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(RuntimeEvent::AwaySummary { .. })) => break,
                Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                    continue;
                }
                other => panic!("no away summary: {other:?}"),
            }
        }
    }

    let history_started = std::time::Instant::now();
    client
        .send(ClientCommand::QuerySessionHistory {
            session_id: session.clone(),
            query_id: None,
        })
        .await
        .unwrap();
    let entries = loop {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Ok(RuntimeEvent::SessionHistoryLoaded { entries, .. })) => break entries,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            other => panic!("no history: {other:?}"),
        }
    };
    eprintln!(
        "[perf] session history replay with {} entries built in {} ms",
        entries.len(),
        history_started.elapsed().as_millis()
    );
    let recaps: Vec<String> = entries
        .iter()
        .filter_map(|entry| match &entry.event {
            RuntimeEvent::AwaySummary { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        recaps,
        vec!["R4-DURABLE-RECAP".to_string()],
        "exactly one durable review per turn: {entries:?}"
    );
    let recap_index = entries
        .iter()
        .position(|entry| matches!(entry.event, RuntimeEvent::AwaySummary { .. }))
        .expect("recap entry");
    let terminal_index = entries
        .iter()
        .rposition(|entry| {
            matches!(
                entry.event,
                RuntimeEvent::TurnCompleted
                    | RuntimeEvent::TurnCompletedWithWarnings { .. }
                    | RuntimeEvent::TurnAnswered
            )
        })
        .expect("turn terminal");
    assert!(
        recap_index > terminal_index,
        "the review reads after the turn it describes"
    );
    let recap_calls = server
        .request_bodies()
        .await
        .iter()
        .filter(|body| body.contains("Write one concise"))
        .count();
    assert_eq!(recap_calls, 2, "each request made one assist call");
}

/// R11: two settled turns keep two reviews, each bound to its own turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_settled_turn_keeps_its_own_review() {
    let (_tmp, _server, client, session) = client_with(vec![
        text("first answer"),
        MockResponse::json_ok(
            r#"{"id":"assist","choices":[{"message":{"role":"assistant","content":"REVIEW-ONE"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        ),
        text("second answer"),
        MockResponse::json_ok(
            r#"{"id":"assist","choices":[{"message":{"role":"assistant","content":"REVIEW-TWO"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        ),
    ])
    .await;
    let mut rx = client.subscribe();
    for prompt in ["first", "second"] {
        client
            .send(ClientCommand::SubmitMessage {
                session_id: session.clone(),
                content: prompt.into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        assert!(await_turn_terminal(&mut rx).await, "turn settles");
        client
            .send(ClientCommand::RequestAwaySummary {
                session_id: session.clone(),
            })
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(RuntimeEvent::AwaySummary { .. })) => break,
                Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                    continue;
                }
                other => panic!("no away summary: {other:?}"),
            }
        }
    }
    client
        .send(ClientCommand::QuerySessionHistory {
            session_id: session.clone(),
            query_id: None,
        })
        .await
        .unwrap();
    let entries = loop {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Ok(RuntimeEvent::SessionHistoryLoaded { entries, .. })) => break entries,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            other => panic!("no history: {other:?}"),
        }
    };
    let recaps: Vec<(usize, String)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| match &entry.event {
            RuntimeEvent::AwaySummary { text } => Some((index, text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        recaps
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        vec!["REVIEW-ONE", "REVIEW-TWO"],
        "one review per turn: {entries:?}"
    );
    let terminals: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            matches!(
                entry.event,
                RuntimeEvent::TurnCompleted
                    | RuntimeEvent::TurnCompletedWithWarnings { .. }
                    | RuntimeEvent::TurnAnswered
            )
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(terminals.len(), 2);
    for ((recap_index, _), terminal) in recaps.iter().zip(terminals.iter()) {
        assert!(recap_index > terminal, "a review reads after its own turn");
    }
}
