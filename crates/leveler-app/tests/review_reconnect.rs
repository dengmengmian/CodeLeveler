//! What a reconnecting client can learn from the runtime after a review
//! settled while its transport was down.
//!
//! The live terminal event is gone with the broken stream. The durable facts
//! are not: the review row, the settlement notice, and the history the
//! runtime attaches to a snapshot while a turn is live. These tests pin that
//! contract, because the TUI's reconnect projection depends on it.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiReviewScope,
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

fn sse(frames: &[String]) -> MockResponse {
    MockResponse::sse(&frames.iter().map(String::as_str).collect::<Vec<_>>())
}

fn tool_call_frame(id: &str, name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": id,
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]
    })
    .to_string()
}

fn finish_frame(reason: &str) -> String {
    serde_json::json!({"choices": [{"delta": {}, "finish_reason": reason}]}).to_string()
}

fn text_frame(text: &str) -> String {
    serde_json::json!({"choices": [{"delta": {"content": text}}]}).to_string()
}

fn finding_response() -> MockResponse {
    sse(&[
        tool_call_frame(
            "call_finding",
            "report_finding",
            serde_json::json!({
                "kind": "correctness",
                "summary": "owner filter is missing",
                "title": "读取账单时没有按 owner 过滤",
                "dimension": "security",
                "severity": "p1",
                "confidence": "high",
                "file": "src/billing.rs",
                "line": 42,
                "evidence": "src/billing.rs:42",
                "impact": "越权读取",
                "recommendation": "加入 owner",
                "reproduced": false
            }),
        ),
        finish_frame("tool_calls"),
    ])
}

fn verdict_frame() -> String {
    tool_call_frame(
        "call_verdict",
        "report_review",
        serde_json::json!({
            "report": "审查完成：发现一个越权读取问题。",
            "formal_findings": ["f-1"],
            "conclusion": "issues_found"
        }),
    )
}

fn git(root: &std::path::Path, args: &[&str]) {
    std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
}

/// A workspace the review can read, with the mock provider configured.
async fn fixture(
    responses: Vec<MockResponse>,
) -> (tempfile::TempDir, MockServer, Arc<Application>) {
    isolate_global_config();
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q"]);
    git(tmp.path(), &["config", "user.email", "t@t"]);
    git(tmp.path(), &["config", "user.name", "t"]);
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/billing.rs"),
        "pub fn load(document_id: &str) { let _ = document_id; }\n",
    )
    .unwrap();
    git(tmp.path(), &["add", "."]);
    git(tmp.path(), &["commit", "-qm", "base"]);
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
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 1024, max_tool_schema_bytes: 65536, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
    )
    .unwrap();
    let app = Arc::new(
        Application::assemble(Layout::from_parts(
            tmp.path().to_path_buf(),
            tmp.path().join("configs"),
            tmp.path().join("state"),
        ))
        .unwrap()
        .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    (tmp, server, app)
}

/// The exact query a reconnect asks while the main turn is still streaming:
/// its snapshot must carry the review's started and settled facts, with the
/// formal report and at their own positions. That is what lets the client
/// rebuild the result the broken stream lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_turn_snapshot_carries_a_review_settled_during_the_turn() {
    let verdict_gate = Arc::new(tokio::sync::Notify::new());
    let turn_gate = Arc::new(tokio::sync::Notify::new());
    let (_tmp, server, app) = fixture(vec![
        finding_response(),
        // The reviewer's verdict is held until the main turn streams.
        MockResponse::GatedSse {
            first: format!("data: {}\n\n", verdict_frame()),
            tail: format!("data: {}\n\ndata: [DONE]\n\n", finish_frame("tool_calls")),
            release: verdict_gate.clone(),
        },
        // The main turn streams one chunk and holds, like a live answer.
        MockResponse::GatedSse {
            first: format!("data: {}\n\n", text_frame("部分回答")),
            tail: format!("data: {}\n\ndata: [DONE]\n\n", finish_frame("stop")),
            release: turn_gate.clone(),
        },
        // The reviewer's own closing message after its verdict tool call.
        sse(&[
            text_frame("审查完成：发现一个越权读取问题。"),
            finish_frame("stop"),
        ]),
    ])
    .await;
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "reconnect").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::FullAccess,
        false,
    ));
    let mut events = client.subscribe_session(&session);

    // 1. The review starts and blocks on its verdict.
    client
        .send(ClientCommand::RunReview {
            session_id: session.clone(),
            focus: "检查账单读取的授权边界".into(),
            scope: UiReviewScope::Workspace,
            model: None,
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && server.request_heads().await.len() < 2 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 2. A normal turn starts and streams while the review is still open.
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "先解释一下仓库".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(30), events.recv()).await {
            Ok(Ok(RuntimeEvent::AssistantTextDelta { delta, .. })) if delta.contains("部分") => {
                break;
            }
            Ok(Ok(_)) => {}
            other => panic!("turn never streamed: {other:?}"),
        }
    }

    // 3. The review settles mid-turn. Its event is delivered here because the
    //    stream is healthy; a broken stream is the case the snapshot covers.
    verdict_gate.notify_one();
    let finished = loop {
        match tokio::time::timeout(Duration::from_secs(30), events.recv()).await {
            Ok(Ok(RuntimeEvent::ReviewFinished { review, report })) => break (review, report),
            Ok(Ok(_)) => {}
            other => panic!("review never settled: {other:?}"),
        }
    };
    assert!(
        finished.1.is_some(),
        "the formal report is part of the terminal fact"
    );

    // 4. This is what a reconnecting client reads.
    let snapshot = client.snapshot(&session).await.unwrap();
    assert_eq!(snapshot.status, "running");
    let history = snapshot
        .history
        .expect("a live turn's snapshot carries the durable history");
    let started = history
        .entries
        .iter()
        .position(|entry| {
            matches!(&entry.event, RuntimeEvent::ReviewStarted { review } if review.id == finished.0.id)
        })
        .expect("the execution card's fact");
    let settled = history
        .entries
        .iter()
        .position(|entry| {
            matches!(&entry.event,
                RuntimeEvent::ReviewFinished { review, report }
                    if review.id == finished.0.id && report.is_some())
        })
        .expect("the delivered result's fact");
    assert!(
        started < settled,
        "the result follows the card it belongs to"
    );
    assert!(
        history.entries.iter().any(|entry| matches!(
            &entry.event,
            RuntimeEvent::UserMessageAdded { message } if message.text == "先解释一下仓库"
        )),
        "the turn that was live at the break is in the same history"
    );
    turn_gate.notify_one();
}

/// Without a live turn the snapshot carries no history; the client asks for it
/// with its own `QuerySessionHistory`. This is why an idle reconnect must
/// request the durable log before it can project review results.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_snapshot_does_not_carry_history() {
    let (_tmp, _server, app) = fixture(Vec::new()).await;
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "idle").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::FullAccess,
        false,
    ));
    let snapshot = client.snapshot(&session).await.unwrap();
    assert!(
        snapshot.status == "created" || snapshot.status == "idle",
        "a session with no turn: {}",
        snapshot.status
    );
    assert!(
        snapshot.history.is_none(),
        "the client's own QuerySessionHistory is the idle replay path"
    );
}
