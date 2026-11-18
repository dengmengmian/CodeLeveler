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

fn tool_call_frame_at(id: &str, index: usize, name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": index,
            "id": id,
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]
    })
    .to_string()
}

fn usage_frame(input: u32, output: u32, cached: u32) -> String {
    serde_json::json!({
        "choices": [],
        "usage": {
            "prompt_tokens": input,
            "completion_tokens": output,
            "prompt_tokens_details": {"cached_tokens": cached}
        }
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
                "report": "审查完成：发现一个越权读取问题——账单读取缺少 owner 过滤，另一个租户可能读到账单。",
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

fn report_response(report: &str) -> MockResponse {
    sse(&[
        tool_call_frame(
            "call_verdict",
            "report_review",
            serde_json::json!({
                "report": report,
                "conclusion": "issues_found",
                "coverage": [{
                    "dimension": "security",
                    "status": "partial",
                    "scope": "账单读取"
                }]
            }),
        ),
        finish_frame("tool_calls"),
    ])
}

fn adjudicate_response(
    finding_id: &str,
    disposition: &str,
    extra: serde_json::Value,
) -> MockResponse {
    let mut arguments = serde_json::json!({
        "finding_id": finding_id,
        "disposition": disposition,
    });
    if let (Some(object), Some(extra)) = (arguments.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            object.insert(key.clone(), value.clone());
        }
    }
    sse(&[
        tool_call_frame("call_adjudicate", "adjudicate_finding", arguments),
        finish_frame("tool_calls"),
    ])
}

fn spawn_investigation_response() -> MockResponse {
    sse(&[
        tool_call_frame(
            "call_spawn",
            "spawn_agent",
            serde_json::json!({
                "title": "账单读取权限调查",
                "task": "独立调查账单读取路径的 owner/tenant 边界。确认读取入口、查询条件和返回字段，报告有证据支持的事实。",
                "run_in_background": false
            }),
        ),
        finish_frame("tool_calls"),
    ])
}

fn investigation_response() -> MockResponse {
    sse(&[
        tool_call_frame(
            "call_investigation",
            "report_investigation",
            serde_json::json!({
                "objective": "核对账单读取的 owner/tenant 边界",
                "observed_facts": [{
                    "statement": "load() 只按 document id 查询，没有 owner 或 tenant 条件。",
                    "evidence_refs": ["src/billing.rs:1"]
                }],
                "business_constraints": [
                    "候选不变量：账单只能被其 owner 读取。"
                ],
                "examined_scope": ["src/billing.rs"],
                "unresolved_questions": ["是否存在上层强制 owner 校验？"],
                "limitations": ["只读静态检查，没有真实数据库。"],
                "evidence_refs": ["src/billing.rs:1"]
            }),
        ),
        finish_frame("tool_calls"),
    ])
}

fn investigator_finding_response() -> MockResponse {
    sse(&[
        tool_call_frame(
            "call_investigator_finding",
            "report_finding",
            serde_json::json!({
                "kind": "correctness",
                "summary": "load() 缺少 owner 过滤，可能跨租户读取账单",
                "title": "账单读取缺少 owner 过滤",
                "dimension": "security",
                "severity": "p1",
                "confidence": "medium",
                "file": "src/billing.rs",
                "line": 1,
                "evidence": "src/billing.rs:1 只按 document id 查询",
                "impact": "其他租户可能读到账单",
                "recommendation": "查询加入 owner/tenant 条件"
            }),
        ),
        finish_frame("tool_calls"),
    ])
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

    // The investigation trace belongs to the REVIEW row, not to the session
    // it judged. It names the tools the reviewer actually called.
    let activity: serde_json::Value =
        serde_json::from_str(&stored.activity_json).expect("activity JSON");
    let rows = activity.as_array().expect("an activity array");
    assert!(!rows.is_empty(), "the review must leave a tool trace");
    assert!(
        rows.iter()
            .all(|row| row["review_id"] == finished.id.as_str()),
        "every activity row belongs to this review: {activity}"
    );
    assert!(
        stored.activity_json.contains("report_finding"),
        "the trace records the finding tool: {}",
        stored.activity_json
    );

    // The real model request declares the role's read-only investigation
    // surface: code intelligence and revision-scoped git, and none of the
    // write/command/spawn/self-widening controls.
    let bodies = server.request_bodies().await;
    let first: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    let names: Vec<&str> = first["tools"]
        .as_array()
        .expect("a tool-calling request declares tools")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    for expected in [
        "find_symbol",
        "read_symbol",
        "find_references",
        "diagnostics",
        "git_history",
        "report_finding",
        "report_review",
        "adjudicate_finding",
        "session_history",
        "memory_search",
        "spawn_agent",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    // The review's spawn surface is investigation-only: no role, agent,
    // write scope or background-mode choice leaks the ordinary spawn tool.
    let spawn = first["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["function"]["name"] == "spawn_agent")
        .expect("restricted spawn definition");
    let properties = spawn["function"]["parameters"]["properties"]
        .as_object()
        .expect("spawn schema properties");
    for forbidden in ["role", "profile", "agent", "files"] {
        assert!(
            !properties.contains_key(forbidden),
            "review spawn must not offer `{forbidden}`: {properties:?}"
        );
    }
    for forbidden in [
        "write_file",
        "apply_patch",
        "run_command",
        "shell_command",
        "capability",
        "claim_write_scope",
        "kill_task",
        "wait_task",
    ] {
        assert!(
            !names.contains(&forbidden),
            "{forbidden} must not reach a review seat: {names:?}"
        );
    }

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

/// Memory reaches the reviewer through the real wire: the initial recall is
/// in the first request, and a `memory_search` call returns the topic excerpt
/// to the next request. The read is bounded and never writes the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reviewer_reads_memory_through_the_bounded_tool() {
    let (tmp, server, _app, client, session) = harness(vec![
        sse(&[
            tool_call_frame(
                "call_memory",
                "memory_search",
                serde_json::json!({"query": "订单结算"}),
            ),
            finish_frame("tool_calls"),
        ]),
        final_response("审查完成：历史业务规则已核对。"),
    ])
    .await;

    let storage =
        leveler_memory::MemoryStorage::for_workspace(tmp.path(), &tmp.path().join("memory"));
    storage.ensure_initialized().unwrap();
    let topic = storage.workspace_dir().join("topics/order-settlement.md");
    std::fs::create_dir_all(topic.parent().unwrap()).unwrap();
    std::fs::write(
        &topic,
        "## 订单结算规则\n\n终止订单不得进入结算；结算前必须核对 terminatedAt。\n",
    )
    .unwrap();
    leveler_memory::regenerate_scope_manifest(
        storage.workspace_dir(),
        leveler_memory::MemoryScope::Workspace,
        leveler_memory::ManifestBudget::default(),
    )
    .unwrap();
    let before = std::fs::read_to_string(&topic).unwrap();

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session.clone(),
            focus: "审查订单结算的业务规则".into(),
            scope: UiReviewScope::Workspace,
            model: None,
        })
        .await
        .unwrap();
    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);

    let bodies = server.request_bodies().await;
    assert!(bodies.len() >= 2, "expected a tool round and a final round");
    assert!(
        bodies[0].contains("memory_search"),
        "the review seat must be offered the read-only memory query: {}",
        bodies[0]
    );
    assert!(
        bodies[0].contains("相关 Memory"),
        "the initial recall belongs in the first request: {}",
        bodies[0]
    );
    assert!(
        bodies[1].contains("终止订单不得进入结算"),
        "the tool result must reach the model: {}",
        bodies[1]
    );
    assert_eq!(
        std::fs::read_to_string(&topic).unwrap(),
        before,
        "a review must not modify memory"
    );
}

/// A session-level memory pause is the user's setting. The review must not
/// bypass it: no index, no recall block and no query tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_memory_setting_removes_review_recall() {
    let (tmp, server, app, client, session) = harness(vec![final_response("审查完成。")]).await;
    let storage =
        leveler_memory::MemoryStorage::for_workspace(tmp.path(), &tmp.path().join("memory"));
    storage.ensure_initialized().unwrap();
    let topic = storage.workspace_dir().join("topics/order-settlement.md");
    std::fs::create_dir_all(topic.parent().unwrap()).unwrap();
    std::fs::write(&topic, "## 订单结算规则\n\n终止订单不得进入结算。\n").unwrap();
    leveler_memory::regenerate_scope_manifest(
        storage.workspace_dir(),
        leveler_memory::MemoryScope::Workspace,
        leveler_memory::ManifestBudget::default(),
    )
    .unwrap();
    app.memory()
        .set_session_enabled(&session, false)
        .expect("memory is configured active");

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session.clone(),
            focus: "审查订单结算的业务规则".into(),
            scope: UiReviewScope::Workspace,
            model: None,
        })
        .await
        .unwrap();
    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);

    let bodies = server.request_bodies().await;
    assert!(!bodies.is_empty());
    for forbidden in ["memory_search", "相关 Memory", "终止订单不得进入结算"] {
        assert!(
            !bodies[0].contains(forbidden),
            "paused memory must not reach the reviewer (`{forbidden}`): {}",
            bodies[0]
        );
    }
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
    for forbidden in [
        "git_history",
        "memory_search",
        "report_review",
        "adjudicate_finding",
    ] {
        assert!(
            !engine
                .factory
                .registry
                .definitions()
                .iter()
                .any(|tool| tool.name == forbidden),
            "{forbidden} belongs to the manual review surface only"
        );
    }
}

/// An ordinary turn pays no review context and no extra review request.
/// The review brief, prefetch block and memory recall are absent from the
/// real model request, and the turn is exactly one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_turn_carries_no_review_context() {
    let (_tmp, server, _app, client, session) = harness(vec![final_response("普通回复")]).await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session,
            content: "say hello".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("the ordinary turn must settle")
            .expect("event stream");
        if matches!(
            event,
            RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnCompletedWithWarnings { .. }
                | RuntimeEvent::TurnAnswered
                | RuntimeEvent::TurnTruncated { .. }
                | RuntimeEvent::TurnIncomplete { .. }
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnCancelled
        ) {
            break;
        }
    }
    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), 1, "an ordinary turn is one request");
    for forbidden in [
        "上下文来源与权威等级",
        "代码关系预取",
        "相关 Memory",
        "memory_search",
        "session_history",
        "git_history",
        "report_review",
        "adjudicate_finding",
    ] {
        assert!(
            !bodies[0].contains(forbidden),
            "ordinary turns must not carry review context `{forbidden}`: {}",
            bodies[0]
        );
    }
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

/// A commit-scoped review must read the committed tree, not the dirty
/// worktree, and must refuse the live-worktree tools structurally. The test
/// scripts a `git_history` read followed by a `read_file` call the host must
/// deny, so both halves are observable in the real wire transcript.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commit_review_reads_that_tree_and_refuses_live_worktree_tools() {
    let responses = vec![
        sse(&[
            tool_call_frame(
                "call_history",
                "git_history",
                serde_json::json!({
                    "action": "file",
                    "rev": "HEAD",
                    "path": "src/billing.rs"
                }),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(&[
            tool_call_frame(
                "call_live_read",
                "read_file",
                serde_json::json!({"path": "src/billing.rs"}),
            ),
            finish_frame("tool_calls"),
        ]),
        final_response("审查完成：该提交中不存在 receipt。"),
    ];
    let (tmp, server, app, client, session) = harness(responses).await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session,
            focus: "检查该提交的账单读取实现".into(),
            scope: UiReviewScope::Commit { rev: "HEAD".into() },
            model: None,
        })
        .await
        .unwrap();

    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);
    assert!(!finished.modified_source);

    // The object identity is the RESOLVED commit SHA, not the user's
    // spelling ("HEAD"): a later `HEAD` move must not change what was
    // reviewed.
    let app_db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&app_db)
        .get(&finished.id)
        .await
        .unwrap()
        .expect("the review row is durable");
    let object: serde_json::Value =
        serde_json::from_str(stored.object_json.as_deref().expect("object identity")).unwrap();
    let revision = object["revision"].as_str().expect("revision");
    assert_eq!(revision.len(), 40, "{revision}");
    assert!(
        revision.chars().all(|c| c.is_ascii_hexdigit()),
        "revision must be the resolved SHA: {revision}"
    );

    let bodies = server.request_bodies().await;
    let first: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    let names: Vec<&str> = first["tools"]
        .as_array()
        .expect("a tool-calling request declares tools")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert!(
        names.contains(&"git_history"),
        "the revision-scoped tool must be advertised: {names:?}"
    );
    for live_tool in ["read_file", "grep", "list_files", "git_status", "git_diff"] {
        assert!(
            !names.contains(&live_tool),
            "{live_tool} must not be offered for a fixed commit: {names:?}"
        );
    }

    let all: String = bodies
        .iter()
        .map(|body| body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all.contains("pub fn load") && !all.contains("pub fn receipt"),
        "the git_history result must be the commit's file, not the worktree"
    );
    assert!(
        all.contains("固定版本") && all.contains("不能作为该对象的证据"),
        "the live read must be refused with the revision reason: {all}"
    );
    assert!(
        std::fs::read_to_string(tmp.path().join("src/billing.rs"))
            .unwrap()
            .contains("pub fn receipt"),
        "the review must not alter the dirty worktree"
    );
}

/// A branch review's object is `merge-base(base, HEAD)..HEAD`. On Windows the
/// scope and the evidence both come from the local object database: no Git
/// process is started, and the reviewed branch content is still exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_branch_review_reads_the_merge_base_range() {
    let responses = vec![
        sse(&[
            tool_call_frame(
                "call_branch_history",
                "git_history",
                serde_json::json!({
                    "action": "file",
                    "rev": "HEAD",
                    "path": "src/billing.rs"
                }),
            ),
            finish_frame("tool_calls"),
        ]),
        final_response("审查完成。"),
    ];
    let (tmp, server, _app, client, session) = harness(responses).await;
    let base = {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    for args in [
        vec!["checkout", "-q", "-b", "feature"],
        vec!["add", "."],
        vec!["commit", "-qm", "feature: receipt"],
    ] {
        let output = std::process::Command::new("git")
            .args(&args)
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session,
            focus: "检查该分支的账单读取实现".into(),
            scope: UiReviewScope::Branch { base },
            model: None,
        })
        .await
        .unwrap();
    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);

    let bodies = server.request_bodies().await;
    assert!(!bodies.is_empty());
    let first = &bodies[0];
    assert!(
        first.contains("pub fn receipt"),
        "the branch range must reach the reviewer: {first}"
    );
    assert!(
        first.contains("本次分支审查由本地只读对象库解析"),
        "the review must state that its object access started no process: {first}"
    );
}

/// A fixed commit review may read that commit and its ancestors, never a
/// later commit on the same branch. Without this, a reviewer could read the
/// fix from a future commit and call it evidence about the reviewed object.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commit_review_cannot_read_a_later_commit_as_evidence() {
    let responses = vec![
        sse(&[
            tool_call_frame(
                "call_later",
                "git_history",
                serde_json::json!({
                    "action": "file",
                    "rev": "HEAD",
                    "path": "src/billing.rs"
                }),
            ),
            finish_frame("tool_calls"),
        ]),
        final_response("审查完成。"),
    ];
    let (tmp, server, _app, client, session) = harness(responses).await;
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    git(&["add", "."]);
    git(&["commit", "-qm", "later"]);
    let base = git(&["rev-parse", "HEAD~1"]);

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session,
            focus: "检查该提交的账单读取实现".into(),
            scope: UiReviewScope::Commit { rev: base },
            model: None,
        })
        .await
        .unwrap();
    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);

    let bodies = server.request_bodies().await;
    let second = &bodies[1];
    // Windows reads the same object database through the local reader, so
    // the scope rule is identical on every host: the later commit is refused
    // by ancestry, not by an unavailable network-deny backend.
    assert!(
        second.contains("outside the reviewed object's ancestry"),
        "a later commit must be refused: {second}"
    );
    assert!(
        !second.contains("pub fn receipt"),
        "the later commit's content must not become evidence: {second}"
    );
}

/// A Lead Reviewer may launch one independent read-only investigation and
/// must receive its structured facts and candidate finding as evidence, while
/// the reviewed workspace stays unchanged and the final report stays single.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reviewer_autonomously_investigates_and_synthesizes_structured_evidence() {
    let responses = vec![
        spawn_investigation_response(),
        investigation_response(),
        investigator_finding_response(),
        final_response("调查完成：读取路径缺少 owner 条件。"),
        review_verdict_response(),
        final_response("审查完成：发现一个跨租户读取风险。"),
    ];
    let (tmp, server, app, client, session) = harness(responses).await;
    let before = std::fs::read_to_string(tmp.path().join("src/billing.rs")).unwrap();
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
    assert_eq!(finished.investigations.len(), 1, "{finished:?}");
    let investigation = &finished.investigations[0];
    assert_eq!(investigation.role, "investigator");
    assert_eq!(
        investigation.state,
        leveler_client_protocol::UiReviewInvestigationState::Completed
    );
    assert_eq!(investigation.title, "账单读取权限调查");
    assert_eq!(
        investigation.findings, 1,
        "the adopted finding is attributed"
    );
    assert_eq!(
        finished.counts.total(),
        0,
        "an investigator's finding is a candidate, not a formal defect"
    );
    assert_eq!(
        finished.counts.pending_candidates, 1,
        "the undecided candidate is reported separately"
    );

    // The workspace the review read is unchanged.
    let after = std::fs::read_to_string(tmp.path().join("src/billing.rs")).unwrap();
    assert_eq!(before, after);

    // The durable row carries the investigation list, so the report survives
    // a reconnect instead of reconstructing it from the live stream.
    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&finished.id)
        .await
        .unwrap()
        .expect("review row");
    assert!(
        stored.investigations_json.contains("investigator")
            && stored.investigations_json.contains("账单读取权限调查"),
        "{}",
        stored.investigations_json
    );
    assert!(
        stored.findings_json.contains("\"source_child\":\"")
            && stored.findings_json.contains("\"role\":\"investigator\""),
        "a candidate finding keeps its source child: {}",
        stored.findings_json
    );

    let bodies = server.request_bodies().await;
    assert!(
        bodies.len() >= 6,
        "the child and the parent each need their own requests: {bodies:#?}"
    );

    // The reviewer's opening surface carries the restricted spawn definition,
    // not the ordinary one.
    let first: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    let spawn = first["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["function"]["name"] == "spawn_agent")
        .expect("restricted spawn definition");
    let properties = spawn["function"]["parameters"]["properties"]
        .as_object()
        .expect("spawn schema properties");
    for forbidden in ["role", "profile", "agent", "files"] {
        assert!(
            !properties.contains_key(forbidden),
            "the review spawn schema must not offer `{forbidden}`: {properties:?}"
        );
    }

    // The child runs on its own transcript: investigator role, read-only
    // tools, structured evidence declaration, and no way to delegate again.
    let child_body = bodies
        .iter()
        .find(|body| body.contains("INVESTIGATOR sub-agent"))
        .expect("the investigation child's own request");
    let child: serde_json::Value = serde_json::from_str(child_body).unwrap();
    let child_tools: Vec<&str> = child["tools"]
        .as_array()
        .expect("child tools")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert!(
        child_tools.contains(&"report_investigation"),
        "{child_tools:?}"
    );
    assert!(child_tools.contains(&"report_finding"), "{child_tools:?}");
    for forbidden in [
        "spawn_agent",
        "apply_patch",
        "write_file",
        "run_command",
        "shell_command",
        "report_review",
    ] {
        assert!(
            !child_tools.contains(&forbidden),
            "{forbidden} must not reach an investigation: {child_tools:?}"
        );
    }

    // The parent's next request contains the child's structured evidence, and
    // the adopted finding is attributed rather than presented as the parent's.
    let settlement = bodies
        .iter()
        .find(|body| body.contains("## Independent investigation"))
        .expect("the investigation settlement reaches the Lead Reviewer");
    assert!(
        settlement.contains("load() 只按 document id 查询"),
        "observed fact missing: {settlement}"
    );
    assert!(
        settlement.contains("候选不变量：账单只能被其 owner 读取。"),
        "business constraint missing: {settlement}"
    );
    assert!(
        settlement.contains("是否存在上层强制 owner 校验？"),
        "unresolved question missing: {settlement}"
    );
    assert!(
        settlement.contains("Candidate findings recorded"),
        "the investigator's finding must arrive as a candidate: {settlement}"
    );
    assert!(
        settlement.contains("adjudicate_finding"),
        "the Lead must be told how to decide it: {settlement}"
    );
    assert!(
        settlement.contains("investigator"),
        "the source child role must be carried: {settlement}"
    );
}

/// Cancelling a review while an investigation is mid-request must cancel the
/// child, settle the parent as cancelled, and leave no live delegation behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_review_cancels_its_active_investigation() {
    let responses = vec![spawn_investigation_response(), MockResponse::NeverResponds];
    let (_tmp, server, app, client, session) = harness(responses).await;
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
    let review_id = loop {
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("review start")
            .expect("event stream");
        if let RuntimeEvent::ReviewStarted { review } = event {
            break review.id;
        }
    };
    // Wait until the child's own request reached the provider. A fixed sleep
    // would prove nothing about which request is in flight.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let bodies = server.request_bodies().await;
        if bodies
            .iter()
            .any(|body| body.contains("INVESTIGATOR sub-agent"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the investigation never started: {bodies:#?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let before_cancel = server.request_bodies().await.len();
    client
        .send(ClientCommand::CancelReview {
            session_id: session.clone(),
            review_id: review_id.clone(),
        })
        .await
        .unwrap();
    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.id, review_id);
    assert_eq!(finished.status, UiReviewStatus::Cancelled);
    assert_eq!(finished.investigations.len(), 1);
    // The child has no live result and is reported as cancelled, never as
    // still running or completed.
    assert_eq!(
        finished.investigations[0].state,
        leveler_client_protocol::UiReviewInvestigationState::Cancelled
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        server.request_bodies().await.len(),
        before_cancel,
        "a cancelled review must not keep issuing child requests"
    );
    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&finished.id)
        .await
        .unwrap()
        .expect("review row");
    assert_eq!(stored.status, "cancelled");
}

/// A review seat cannot widen its authority by supplying a capability field
/// the restricted schema does not advertise. Host admission refuses it and no
/// child is started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_spawn_cannot_request_a_writer() {
    let responses = vec![
        sse(&[
            tool_call_frame(
                "call_writer",
                "spawn_agent",
                serde_json::json!({
                    "title": "试图委派写入",
                    "task": "修改源码",
                    "role": "worker",
                    "files": ["src/billing.rs"]
                }),
            ),
            finish_frame("tool_calls"),
        ]),
        review_verdict_response(),
        final_response("审查完成。"),
    ];
    let (_tmp, server, _app, client, session) = harness(responses).await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session,
            focus: "检查账单读取的授权边界".into(),
            scope: UiReviewScope::Workspace,
            model: None,
        })
        .await
        .unwrap();
    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);
    assert!(
        finished.investigations.is_empty(),
        "a refused spawn must not be reported as a running investigation: {finished:?}"
    );
    let bodies = server.request_bodies().await;
    let refusal = bodies
        .iter()
        .find(|body| body.contains("cannot request role"))
        .expect("the refusal reaches the model");
    assert!(!refusal.contains("INVESTIGATOR sub-agent"));
}

/// Two investigations launched in one assistant message run concurrently and
/// both results return to the same Lead Reviewer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_review_investigations_run_concurrently_and_both_return() {
    let responses = vec![
        sse(&[
            tool_call_frame_at(
                "call_spawn_a",
                0,
                "spawn_agent",
                serde_json::json!({
                    "title": "订单状态调查",
                    "task": "独立调查订单状态写入路径，报告事实。",
                    "run_in_background": false
                }),
            ),
            tool_call_frame_at(
                "call_spawn_b",
                1,
                "spawn_agent",
                serde_json::json!({
                    "title": "结算入口调查",
                    "task": "独立调查结算入口的权限与调用者，报告事实。",
                    "run_in_background": false
                }),
            ),
            finish_frame("tool_calls"),
        ]),
        investigation_response(),
        investigation_response(),
        investigator_finding_response(),
        investigator_finding_response(),
        final_response("调查 A 完成。"),
        final_response("调查 B 完成。"),
        review_verdict_response(),
        final_response("审查完成。"),
    ];
    let (_tmp, server, _app, client, session) = harness(responses).await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session,
            focus: "检查订单结算的授权边界".into(),
            scope: UiReviewScope::Workspace,
            model: None,
        })
        .await
        .unwrap();
    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);
    assert_eq!(finished.investigations.len(), 2, "{finished:?}");
    for investigation in &finished.investigations {
        assert_eq!(
            investigation.state,
            leveler_client_protocol::UiReviewInvestigationState::Completed,
            "{investigation:?}"
        );
    }
    assert_eq!(
        finished.counts.total(),
        0,
        "both children's findings are undecided candidates"
    );
    assert_eq!(finished.counts.pending_candidates, 2);
}

/// Facts are a result. An investigation that finds no defect but established
/// real facts must not be reported to the Lead Reviewer as "nothing to
/// report".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_investigation_with_facts_and_no_finding_is_still_a_result() {
    let responses = vec![
        spawn_investigation_response(),
        investigation_response(),
        final_response("调查完成，没有形成候选缺陷。"),
        review_verdict_response(),
        final_response("审查完成：调查提供了事实，但没有确认缺陷。"),
    ];
    let (_tmp, server, _app, client, session) = harness(responses).await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session,
            focus: "检查账单读取的授权边界".into(),
            scope: UiReviewScope::Workspace,
            model: None,
        })
        .await
        .unwrap();
    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);
    assert_eq!(finished.investigations.len(), 1);
    assert_eq!(finished.investigations[0].findings, 0);
    assert_eq!(
        finished.investigations[0].state,
        leveler_client_protocol::UiReviewInvestigationState::Completed
    );
    let bodies = server.request_bodies().await;
    let settlement = bodies
        .iter()
        .find(|body| body.contains("## Independent investigation"))
        .expect("the facts reach the Lead Reviewer");
    assert!(
        !settlement.contains("had nothing to report"),
        "a fact-only investigation must not be described as empty: {settlement}"
    );
}

/// A review's usage is the sum of every request it caused, including the
/// independent investigators' requests. No second ledger, no re-charging.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_usage_includes_the_investigations_requests() {
    let responses = vec![
        sse(&[
            tool_call_frame(
                "call_spawn",
                "spawn_agent",
                serde_json::json!({
                    "title": "账单读取权限调查",
                    "task": "独立调查账单读取路径的 owner/tenant 边界。",
                    "run_in_background": false
                }),
            ),
            usage_frame(300, 30, 150),
            finish_frame("tool_calls"),
        ]),
        sse(&[
            tool_call_frame(
                "call_investigation",
                "report_investigation",
                serde_json::json!({
                    "objective": "核对账单读取的 owner/tenant 边界",
                    "observed_facts": [{"statement": "load() 只按 document id 查询。"}],
                    "examined_scope": ["src/billing.rs"]
                }),
            ),
            usage_frame(100, 10, 40),
            finish_frame("tool_calls"),
        ]),
        sse(&[
            text_frame("调查完成。"),
            usage_frame(50, 5, 20),
            finish_frame("stop"),
        ]),
        sse(&[
            tool_call_frame(
                "call_verdict",
                "report_review",
                serde_json::json!({
                    "conclusion": "inconclusive",
                    "coverage": [{"dimension": "security", "status": "partial"}]
                }),
            ),
            usage_frame(200, 20, 100),
            finish_frame("tool_calls"),
        ]),
        sse(&[
            text_frame("审查完成。"),
            usage_frame(60, 6, 30),
            finish_frame("stop"),
        ]),
    ];
    let (_tmp, server, _app, client, session) = harness(responses).await;
    let mut rx = client.subscribe();
    client
        .send(ClientCommand::RunReview {
            session_id: session,
            focus: "检查账单读取的授权边界".into(),
            scope: UiReviewScope::Workspace,
            model: None,
        })
        .await
        .unwrap();
    let finished = next_review_finished(&mut rx, &server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);
    assert_eq!(finished.usage.input_tokens, 710);
    assert_eq!(finished.usage.output_tokens, 71);
    assert_eq!(finished.usage.cached_input_tokens, 340);
}

/// An investigator's finding is a CANDIDATE. Until the Lead Reviewer decides,
/// it is reported separately and never adds to the formal defect count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_investigator_candidate_is_never_a_formal_finding_until_the_lead_decides() {
    let responses = vec![
        spawn_investigation_response(),
        investigator_finding_response(),
        final_response("调查完成。"),
        report_response("正式报告：调查者提出一条候选，Lead 尚未裁决。"),
        final_response("补充说明：候选仍未裁决。"),
    ];
    let (_tmp, server, app, client, session) = harness(responses).await;
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
    assert_eq!(finished.counts.total(), 0, "{finished:?}");
    assert_eq!(finished.counts.pending_candidates, 1, "{finished:?}");

    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&finished.id)
        .await
        .unwrap()
        .expect("review row");
    let findings: Vec<serde_json::Value> = serde_json::from_str(&stored.findings_json).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["disposition"], "candidate");
    assert_eq!(findings[0]["role"], "investigator");
    assert_eq!(findings[0]["source_finding_id"], "f-1");
}

/// The Lead's explicit adoption is the only path from a candidate to a formal
/// finding, and it may adjust severity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lead_can_adopt_a_candidate_as_a_formal_finding() {
    let responses = vec![
        spawn_investigation_response(),
        investigator_finding_response(),
        final_response("调查完成。"),
        adjudicate_response(
            "f-1",
            "adopted",
            serde_json::json!({
                "reason": "已核对调用链，没有上层 owner 校验",
                "evidence_refs": ["src/billing.rs:1"],
                "severity": "p0",
                "confidence": "high"
            }),
        ),
        report_response("正式报告：采纳调查者的越权读取候选，严重程度上调为 P0。"),
        final_response("审查完成。"),
    ];
    let (_tmp, server, app, client, session) = harness(responses).await;
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
    assert_eq!(finished.counts.total(), 1, "{finished:?}");
    assert_eq!(
        finished.counts.p0, 1,
        "the Lead's severity override applies"
    );
    assert_eq!(finished.counts.pending_candidates, 0);

    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&finished.id)
        .await
        .unwrap()
        .expect("review row");
    let findings: Vec<serde_json::Value> = serde_json::from_str(&stored.findings_json).unwrap();
    assert_eq!(findings[0]["disposition"], "adopted");
    assert_eq!(findings[0]["severity"], "p0");
    assert_eq!(findings[0]["disposition_evidence"][0], "src/billing.rs:1");
}

/// A candidate the Lead explicitly rejects stays in the record with its
/// evidence, but never counts as a defect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lead_can_reject_a_candidate_and_it_does_not_count() {
    let responses = vec![
        spawn_investigation_response(),
        investigator_finding_response(),
        final_response("调查完成。"),
        adjudicate_response(
            "f-1",
            "rejected",
            serde_json::json!({
                "reason": "上层已经按 owner 过滤，调查者的入口不成立",
                "evidence_refs": ["src/billing.rs:1"]
            }),
        ),
        report_response("正式报告：调查者的越权读取主张经复核不成立。"),
        final_response("审查完成。"),
    ];
    let (_tmp, server, app, client, session) = harness(responses).await;
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
    assert_eq!(finished.counts.total(), 0, "{finished:?}");
    assert_eq!(finished.counts.pending_candidates, 0);

    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&finished.id)
        .await
        .unwrap()
        .expect("review row");
    let findings: Vec<serde_json::Value> = serde_json::from_str(&stored.findings_json).unwrap();
    assert_eq!(findings[0]["disposition"], "rejected");
    assert_eq!(
        findings[0]["disposition_reason"],
        "上层已经按 owner 过滤，调查者的入口不成立"
    );
}

/// Two investigators reporting the same root cause are one formal finding, not
/// two. The merged record keeps its source and names the survivor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lead_can_merge_a_duplicate_root_cause() {
    let responses = vec![
        spawn_investigation_response(),
        investigator_finding_response(),
        final_response("第一路调查完成。"),
        spawn_investigation_response(),
        investigator_finding_response(),
        final_response("第二路调查完成。"),
        adjudicate_response("f-1", "adopted", serde_json::json!({})),
        adjudicate_response(
            "f-2",
            "duplicate",
            serde_json::json!({"duplicate_of": "f-1"}),
        ),
        report_response("正式报告：两路调查指向同一根因，合并为一条。"),
        final_response("审查完成。"),
    ];
    let (_tmp, server, app, client, session) = harness(responses).await;
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
    assert_eq!(finished.counts.total(), 1, "one root cause, one defect");
    assert_eq!(finished.counts.pending_candidates, 0);

    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&finished.id)
        .await
        .unwrap()
        .expect("review row");
    let findings: Vec<serde_json::Value> = serde_json::from_str(&stored.findings_json).unwrap();
    assert_eq!(findings.len(), 2, "both candidates stay traceable");
    assert_eq!(findings[0]["disposition"], "adopted");
    assert_eq!(findings[1]["disposition"], "duplicate");
    assert_eq!(findings[1]["duplicate_of"], "f-1");
}

/// The submitted formal report is the report of record. A later chat message is
/// kept beside it and never overwrites it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_chat_message_does_not_overwrite_the_submitted_report() {
    let responses = vec![
        report_response("完整正式报告：范围、发现、覆盖与限制。"),
        final_response("补充说明：定稿后又想到一句话。"),
    ];
    let (_tmp, _server, app, client, session) = harness(responses).await;
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
    let finished = next_review_finished(&mut rx, &_server).await;
    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&finished.id)
        .await
        .unwrap()
        .expect("review row");
    assert_eq!(stored.report, "完整正式报告：范围、发现、覆盖与限制。");
    assert_eq!(stored.report_source, "submitted");
    assert_eq!(stored.final_message, "补充说明：定稿后又想到一句话。");
    assert_eq!(stored.report_revision, 1);
}

/// Evidence that arrives after the report submission makes the report
/// detectably stale. The execution still completes; the report does not claim
/// to cover evidence it never saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_evidence_marks_the_submitted_report_stale() {
    let responses = vec![
        report_response("初版正式报告：提交时还没有调查结果。"),
        spawn_investigation_response(),
        investigator_finding_response(),
        final_response("调查完成。"),
        final_response("补充说明：调查者晚到的证据没有进入正式报告。"),
    ];
    let (_tmp, _server, app, client, session) = harness(responses).await;
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
    let finished = next_review_finished(&mut rx, &_server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);
    assert!(finished.report_stale, "{finished:?}");
    assert_eq!(finished.report_evidence_revision, 0);
    assert_eq!(finished.evidence_revision, 1);
    assert_eq!(finished.counts.pending_candidates, 1);
    assert_eq!(
        finished.counts.total(),
        0,
        "late evidence is not silently promoted to a defect"
    );

    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&finished.id)
        .await
        .unwrap()
        .expect("review row");
    assert_eq!(stored.report, "初版正式报告：提交时还没有调查结果。");
    assert_eq!(
        stored.final_message,
        "补充说明：调查者晚到的证据没有进入正式报告。"
    );

    // The structured verdict carries the runtime-observed staleness as a
    // limitation; it is not invented from prose.
    let verdict = stored.verdict_json.expect("declared verdict");
    assert!(
        verdict.contains("正式报告提交后仍收到新的调查结果"),
        "{verdict}"
    );
}

/// A revised report submitted after the settlement is bound to the new
/// evidence set and is not stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revised_report_is_bound_to_the_evidence_it_saw() {
    let responses = vec![
        report_response("初版报告。"),
        spawn_investigation_response(),
        investigator_finding_response(),
        final_response("调查完成。"),
        adjudicate_response(
            "f-1",
            "adopted",
            serde_json::json!({"reason": "已复核", "severity": "p1"}),
        ),
        report_response("修订报告：已综合调查者证据并采纳候选。"),
        final_response("审查完成。"),
    ];
    let (_tmp, _server, app, client, session) = harness(responses).await;
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
    let finished = next_review_finished(&mut rx, &_server).await;
    assert_eq!(finished.status, UiReviewStatus::Completed);
    assert!(!finished.report_stale);
    assert_eq!(finished.report_revision, 2);
    // One child settlement, plus the Lead's adjudication (which changed the
    // candidate's decision). The revised report is bound to that latest fact.
    assert_eq!(finished.report_evidence_revision, 2);
    assert_eq!(finished.evidence_revision, 2);
    assert_eq!(finished.counts.total(), 1);
    assert_eq!(finished.counts.pending_candidates, 0);

    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&finished.id)
        .await
        .unwrap()
        .expect("review row");
    assert_eq!(stored.report, "修订报告：已综合调查者证据并采纳候选。");
    assert_eq!(stored.report_revision, 2);
}

/// The user's existing `[limits]` caps apply to a review, and the cap is
/// SHARED by the Lead and every investigation: a child cannot start a fresh
/// budget. The review stops honestly and keeps what it established.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_review_token_cap_is_shared_by_the_lead_and_its_investigations() {
    let responses = vec![
        sse(&[
            tool_call_frame(
                "call_spawn",
                "spawn_agent",
                serde_json::json!({
                    "title": "账单读取权限调查",
                    "task": "独立调查账单读取路径的 owner/tenant 边界。",
                    "run_in_background": false
                }),
            ),
            // This response alone spends past the shared cap. If the child
            // received a fresh budget it would still request a response here.
            usage_frame(200_000, 5_000, 0),
            finish_frame("tool_calls"),
        ]),
        // The child must NOT get to send this request.
        investigation_response(),
        final_response("调查完成。"),
    ];
    let (tmp, server, app, client, session) = harness(responses).await;
    std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
    std::fs::write(
        tmp.path().join(".leveler/config.yaml"),
        "limits:\n  max_model_tokens: 100000\n",
    )
    .unwrap();

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
    assert_eq!(
        finished.status,
        UiReviewStatus::Incomplete,
        "a budget stop is not a completed review: {finished:?}"
    );
    assert!(
        finished
            .stop_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("token budget")),
        "the stop names the shared token budget: {finished:?}"
    );
    assert_eq!(
        server.request_count(),
        1,
        "the Lead's spend exhausted the shared cap; no child got a fresh budget"
    );

    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&finished.id)
        .await
        .unwrap()
        .expect("review row");
    assert!(
        stored.report.is_empty(),
        "a budget stop must not fabricate a formal report"
    );
    assert_eq!(
        stored.report_source, "none",
        "the row says no formal report was submitted"
    );
}
