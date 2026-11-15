//! Manual review end-to-end: `/review` admits an independent read-only
//! execution, persists its findings and report, and leaves the source
//! session's task lifecycle untouched.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, CommandId, InteractiveRuntimeClient, RuntimeEvent, UiReviewScope, UiReviewStatus,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{ReviewRepository, TurnRepository};
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

fn text_frame(text: &str) -> String {
    serde_json::json!({"choices": [{"delta": {"content": text}}]}).to_string()
}

fn finish_frame(reason: &str) -> String {
    serde_json::json!({"choices": [{"delta": {}, "finish_reason": reason}]}).to_string()
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
                "evidence": "src/billing.rs:42 selects by document id only",
                "impact": "另一个租户可以读到账单",
                "recommendation": "查询条件加入 owner/tenant",
                "reproduced": false
            }),
        ),
        finish_frame("tool_calls"),
    ])
}

fn review_verdict_response() -> MockResponse {
    sse(&[
        tool_call_frame(
            "call_verdict",
            "report_review",
            serde_json::json!({
                "conclusion": "issues_found",
                "coverage": [{
                    "dimension": "security",
                    "status": "partial",
                    "scope": "账单读取",
                    "evidence_refs": ["src/billing.rs:42"],
                    "limitations": "没有真实数据库，只能静态确认"
                }],
                "limitations": [{
                    "kind": "environment",
                    "detail": "没有真实租户数据库"
                }]
            }),
        ),
        finish_frame("tool_calls"),
    ])
}

fn final_response(text: &str) -> MockResponse {
    sse(&[text_frame(text), finish_frame("stop")])
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

async fn harness(
    responses: Vec<MockResponse>,
) -> (
    tempfile::TempDir,
    MockServer,
    Arc<Application>,
    Arc<InProcessRuntimeClient>,
    leveler_core::SessionId,
) {
    isolate_global_config();
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
    // The workspace the review reads.
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
    std::fs::write(
        tmp.path().join("src/billing.rs"),
        "pub fn load(document_id: &str) { let _ = document_id; }\n\
         pub fn receipt(document_id: &str) { let _ = document_id; }\n",
    )
    .unwrap();

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
    let session = app.create_session(&model, "审查账单读取").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::FullAccess,
        false,
    ));
    (tmp, server, app, client, session)
}

async fn next_review_finished(
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    server: &MockServer,
) -> leveler_client_protocol::UiReviewSummary {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(error)) => panic!("event stream: {error}"),
            Err(_) => {
                let heads = server.request_heads().await;
                let bodies = server.request_bodies().await;
                panic!(
                    "review must settle; requests={heads:#?} bodies={:#?}",
                    bodies.last()
                );
            }
        };
        if let RuntimeEvent::ReviewFinished { review } = event {
            return review;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manual_review_runs_persists_and_does_not_open_a_turn() {
    let (_tmp, server, app, client, session) = harness(vec![
        finding_response(),
        review_verdict_response(),
        final_response("审查完成：发现一个越权读取问题。"),
    ])
    .await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session.clone(),
            focus: "检查账单读取的授权边界".into(),
            scope: UiReviewScope::Workspace,
            model: None,
        })
        .await
        .unwrap();

    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);
    assert_eq!(finished.scope, UiReviewScope::Workspace);
    assert_eq!(finished.counts.p1, 1);
    assert_eq!(finished.counts.total(), 1);
    assert_eq!(
        finished.conclusion,
        Some(leveler_client_protocol::UiReviewConclusion::IssuesFound)
    );
    assert!(!finished.modified_source);

    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&finished.id)
        .await
        .unwrap()
        .expect("the review row is durable");
    assert_eq!(stored.status, "completed");
    assert!(stored.report.contains("越权读取"));
    assert!(stored.findings_json.contains("owner"));
    let verdict_json = stored
        .verdict_json
        .as_deref()
        .expect("the structured verdict is durable");
    assert!(verdict_json.contains("issues_found"), "{verdict_json}");
    assert!(verdict_json.contains("security"), "{verdict_json}");
    assert!(
        verdict_json.contains("没有真实租户数据库"),
        "{verdict_json}"
    );

    // A review is not a task: it must not open or settle a turn.
    assert!(
        TurnRepository::new(&db)
            .list(&session)
            .await
            .unwrap()
            .is_empty(),
        "a review must never create a turn"
    );

    // The report is served from the durable row, not from the live stream.
    let query = CommandId::new("report-query");
    client
        .send(ClientCommand::QueryReviewReport {
            session_id: session,
            review_id: finished.id.clone(),
            query_id: Some(query.clone()),
        })
        .await
        .unwrap();
    let report = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("report must answer")
            .expect("event stream");
        if let RuntimeEvent::ReviewReportLoaded {
            query_id, report, ..
        } = event
            && query_id.as_ref() == Some(&query)
        {
            break report;
        }
    };
    assert_eq!(report.summary.id, finished.id);
    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert_eq!(
        finding.severity,
        Some(leveler_client_protocol::UiReviewSeverity::P1)
    );
    assert_eq!(
        finding.dimension,
        Some(leveler_client_protocol::UiReviewDimension::Security)
    );
    assert_eq!(finding.file.as_deref(), Some("src/billing.rs"));
    assert_eq!(finding.line, Some(42));
    let verdict = &report.verdict;
    assert_eq!(
        verdict.conclusion,
        Some(leveler_client_protocol::UiReviewConclusion::IssuesFound)
    );
    assert_eq!(verdict.coverage.len(), 1);
    assert_eq!(
        verdict.coverage[0].status,
        leveler_client_protocol::UiReviewCoverageStatus::Partial
    );
    assert!(
        verdict.limitations.iter().any(|limitation| {
            limitation.source == leveler_client_protocol::UiReviewLimitationSource::Reviewer
                && limitation.detail.contains("没有真实租户数据库")
        }),
        "{:?}",
        verdict.limitations
    );
    assert!(
        verdict.limitations.iter().any(|limitation| {
            limitation.source == leveler_client_protocol::UiReviewLimitationSource::Runtime
                && limitation.kind
                    == leveler_client_protocol::UiReviewLimitationKind::ToolCapability
        }),
        "the missing command tool is a runtime fact, not a model claim: {:?}",
        verdict.limitations
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_review_settles_it_as_cancelled_without_a_turn() {
    let (_tmp, _server, app, client, session) = harness(vec![MockResponse::NeverResponds]).await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session.clone(),
            focus: "检查当前变更".into(),
            scope: UiReviewScope::Workspace,
            model: None,
        })
        .await
        .unwrap();

    // Wait for admission, then cancel by the durable id the runtime reported.
    let review_id = loop {
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("review must start")
            .expect("event stream");
        if let RuntimeEvent::ReviewStarted { review } = event {
            break review.id;
        }
    };
    client
        .send(ClientCommand::CancelReview {
            session_id: session.clone(),
            review_id: review_id.clone(),
        })
        .await
        .unwrap();
    // A concurrent history read during the cancellation window must not
    // mistake a live, cancelling review for one whose runtime died.
    client
        .send(ClientCommand::ListReviews {
            session_id: session.clone(),
            query_id: Some(CommandId::new("during-cancel")),
        })
        .await
        .unwrap();

    let mut terminal_statuses = Vec::new();
    let finished = loop {
        let event = tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .expect("cancelled review must settle")
            .expect("event stream");
        if let RuntimeEvent::ReviewFinished { review } = event
            && review.id == review_id
        {
            terminal_statuses.push(review.status);
            if review.status == UiReviewStatus::Cancelled {
                break review;
            }
        }
    };
    assert_eq!(finished.status, UiReviewStatus::Cancelled);
    assert!(
        !terminal_statuses.contains(&UiReviewStatus::Incomplete),
        "a cancelling review must never be reported as a dead runtime: {terminal_statuses:?}"
    );

    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&review_id)
        .await
        .unwrap()
        .expect("cancelled review stays durable");
    assert_eq!(stored.status, "cancelled");
    assert!(
        TurnRepository::new(&db)
            .list(&session)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The reviewer can read the reviewed session's own record on demand, and the
/// tool is bound to that id: an extra `session_id` argument cannot redirect it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reviewer_reads_the_reviewed_session_through_the_bound_tool() {
    let (_tmp, server, app, client, session) = harness(vec![
        sse(&[
            tool_call_frame(
                "call_history",
                "session_history",
                serde_json::json!({
                    "action": "summary",
                    // The tool has no such input; a model cannot redirect it.
                    "session_id": "another-session"
                }),
            ),
            finish_frame("tool_calls"),
        ]),
        final_response("审查完成：会话记录已读取。"),
    ])
    .await;

    // Seed the record the reviewer is allowed to read.
    let db = app.open_database().await.unwrap();
    let turn = leveler_storage::TurnRepository::new(&db)
        .start(&session, "user", None, leveler_core::now())
        .await
        .unwrap();
    let turn_id = leveler_core::TurnId::new(turn.id);
    leveler_storage::EventRepository::new(&db)
        .append(
            &session,
            Some(&turn_id),
            "delivery_committed",
            r#"{"payload":{"text":"退款页签已经实现","modified_files":["src/pay.rs"]}}"#,
            leveler_core::now(),
        )
        .await
        .unwrap();
    leveler_storage::MessageRepository::new(&db)
        .append(
            &session,
            &[serde_json::to_string(&leveler_model::Message::user_input(
                "把退款页签加到应收详情",
            ))
            .unwrap()],
            leveler_core::now(),
        )
        .await
        .unwrap();
    let before_turns = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    let before_messages = leveler_storage::MessageRepository::new(&db)
        .load(&session)
        .await
        .unwrap();

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session.clone(),
            focus: "检查这次交付是否与用户请求一致".into(),
            scope: UiReviewScope::Workspace,
            model: None,
        })
        .await
        .unwrap();
    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);

    // The model's next request carries the tool result it actually saw.
    let bodies = server.request_bodies().await;
    assert!(bodies.len() >= 2, "expected a tool round and a final round");
    let second = &bodies[1];
    assert!(
        second.contains("退款页签已经实现"),
        "the reviewer must see the bound session's delivery: {second}"
    );
    assert!(
        second.contains(&session.to_string()),
        "the summary must name the reviewed session: {second}"
    );

    // Reading is read-only: the session's narrative rows are unchanged.
    assert_eq!(
        leveler_storage::TurnRepository::new(&db)
            .list(&session)
            .await
            .unwrap(),
        before_turns
    );
    assert_eq!(
        leveler_storage::MessageRepository::new(&db)
            .load(&session)
            .await
            .unwrap(),
        before_messages
    );
}

/// An ordinary engine has no session reader. The tool exists only on the
/// review surface, so a normal turn pays no schema and gains no extra tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_engine_does_not_advertise_session_history() {
    let (_tmp, _server, app, _client, session) = harness(Vec::new()).await;
    let model = ModelRef::new("mock", "m");
    let engine = app
        .engine_for_session(
            &model,
            PermissionProfile::FullAccess,
            false,
            Arc::new(leveler_execution::AutoApprove),
            Arc::new(leveler_agent::AutoClarify),
            false,
            Some(session.as_str()),
        )
        .await
        .unwrap();
    assert!(
        !engine
            .factory
            .registry
            .definitions()
            .iter()
            .any(|tool| tool.name == "session_history"),
        "the reader belongs to the manual review surface only"
    );
}

/// No `report_review` is not "no issues": the row keeps an undeclared
/// conclusion and only the runtime-observed limits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undeclared_verdict_is_not_turned_into_a_clean_conclusion() {
    let (_tmp, server, app, client, session) = harness(vec![
        finding_response(),
        final_response("审查结束：仅记录了问题，未做结构化声明。"),
    ])
    .await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session.clone(),
            focus: "检查账单读取".into(),
            scope: UiReviewScope::Workspace,
            model: None,
        })
        .await
        .unwrap();
    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);
    assert_eq!(finished.conclusion, None);

    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&finished.id)
        .await
        .unwrap()
        .unwrap();
    let verdict: leveler_client_protocol::UiReviewVerdict =
        serde_json::from_str(stored.verdict_json.as_deref().unwrap()).unwrap();
    assert_eq!(verdict.conclusion, None);
    assert!(verdict.coverage.is_empty());
    assert!(
        verdict.limitations.iter().any(|limitation| {
            limitation.source == leveler_client_protocol::UiReviewLimitationSource::Runtime
                && limitation.kind
                    == leveler_client_protocol::UiReviewLimitationKind::ToolCapability
        }),
        "{:?}",
        verdict.limitations
    );
}

/// Reviewing ANOTHER session binds the reader to that session. The requester's
/// own record is not the reviewed object, and an extra id argument cannot
/// redirect the tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_session_review_reads_the_target_not_the_requester() {
    let (_tmp, server, app, client, requester) = harness(vec![
        sse(&[
            tool_call_frame(
                "call_history",
                "session_history",
                serde_json::json!({"action": "summary"}),
            ),
            finish_frame("tool_calls"),
        ]),
        final_response("审查完成：目标会话已读取。"),
    ])
    .await;
    let model = ModelRef::new("mock", "m");
    let target = app
        .create_session(&model, "目标会话的原始任务")
        .await
        .unwrap();

    let db = app.open_database().await.unwrap();
    for (session, text) in [
        (&requester, "REQUESTER-ONLY-CONTEXT"),
        (&target, "TARGET-ONLY-DELIVERY"),
    ] {
        let turn = leveler_storage::TurnRepository::new(&db)
            .start(session, "user", None, leveler_core::now())
            .await
            .unwrap();
        let turn_id = leveler_core::TurnId::new(turn.id);
        leveler_storage::EventRepository::new(&db)
            .append(
                session,
                Some(&turn_id),
                "delivery_committed",
                &format!(r#"{{"payload":{{"text":"{text}","modified_files":["src/pay.rs"]}}}}"#),
                leveler_core::now(),
            )
            .await
            .unwrap();
        leveler_storage::MessageRepository::new(&db)
            .append(
                session,
                &[serde_json::to_string(&leveler_model::Message::user_input(text)).unwrap()],
                leveler_core::now(),
            )
            .await
            .unwrap();
    }
    let before_target_turns = leveler_storage::TurnRepository::new(&db)
        .list(&target)
        .await
        .unwrap();
    let before_target_messages = leveler_storage::MessageRepository::new(&db)
        .load(&target)
        .await
        .unwrap();

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: requester.clone(),
            focus: "分析目标会话为什么提前停止".into(),
            scope: UiReviewScope::Session {
                session_id: Some(target.clone()),
            },
            model: None,
        })
        .await
        .unwrap();
    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);
    assert_eq!(
        finished.session_id, requester,
        "the card belongs to the requester"
    );

    let bodies = server.request_bodies().await;
    let second = &bodies[1];
    assert!(
        second.contains("TARGET-ONLY-DELIVERY"),
        "the reader must return the target's delivery: {second}"
    );
    assert!(
        !second.contains("REQUESTER-ONLY-CONTEXT"),
        "the requester's own delivery must not be attached to the target review: {second}"
    );

    assert_eq!(
        leveler_storage::TurnRepository::new(&db)
            .list(&target)
            .await
            .unwrap(),
        before_target_turns
    );
    assert_eq!(
        leveler_storage::MessageRepository::new(&db)
            .load(&target)
            .await
            .unwrap(),
        before_target_messages
    );
}

/// A `running` row with no live owner in this process is settled as
/// Incomplete by the next list; it is never left running forever and never
/// invented as Failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_running_row_settles_incomplete_on_list() {
    let (_tmp, _server, app, client, session) = harness(Vec::new()).await;
    let db = app.open_database().await.unwrap();
    let repo = ReviewRepository::new(&db);
    repo.create(&leveler_storage::NewReview {
        id: "review-stale",
        session_id: &session,
        focus: "旧审查",
        scope_json: r#"{"kind":"workspace"}"#,
        model: "mock/m",
        status: "running",
        source_session_id: Some(session.as_str()),
        source_turn_id: None,
        created_at: leveler_core::now(),
    })
    .await
    .unwrap();

    let mut rx = client.subscribe();
    let query = CommandId::new("list-stale");
    client
        .send(ClientCommand::ListReviews {
            session_id: session.clone(),
            query_id: Some(query.clone()),
        })
        .await
        .unwrap();
    let reviews = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("list must answer")
            .expect("event stream");
        if let RuntimeEvent::ReviewHistoryLoaded {
            query_id, reviews, ..
        } = event
            && query_id.as_ref() == Some(&query)
        {
            break reviews;
        }
    };
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].status, UiReviewStatus::Incomplete);
    let stored = repo.get("review-stale").await.unwrap().unwrap();
    assert_eq!(stored.status, "incomplete");
    assert!(stored.stop_detail.is_some());
    assert!(stored.finished_at.is_some());
}
