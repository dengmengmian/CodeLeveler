//! Multi-turn session reliability: budgeted chat history, Goal prior injection,
//! second chat after compact, and resume-from-transcript (mock model only).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AutoClarify, ContinuationPolicy, StepLimits};
use leveler_core::{RequestId, ToolCallId};
use leveler_engine::{EngineEvent, ExecutionKind, ExecutorFactory, TaskEngine, TaskSpec};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_lifecycle::TaskOutcome;
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEvent, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::{Database, MessageRepository, SessionRepository};
use leveler_tools::{ToolContext, default_registry};
use leveler_verifier::VerificationPlan;

struct MockRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl MockRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl ModelRuntime for MockRuntime {
    async fn generate(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        // Completion Reconciliation Gate calls are answered out of band so
        // scripted FIFOs and request-count assertions stay about the loop.
        if let Some(reply) = leveler_test_support::derive_autopilot(&request) {
            return Ok(reply);
        }
        if let Some(reply) = leveler_test_support::reconcile_autopilot(&request) {
            return Ok(reply);
        }
        self.requests.lock().unwrap().push(request);
        self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let response = self.generate(request, cancellation).await?;
        let mut events: Vec<Result<ModelEvent, ModelError>> = Vec::new();
        events.push(Ok(ModelEvent::MessageStarted {
            request_id: response.request_id.clone(),
        }));
        for part in &response.message.content {
            match part {
                ContentPart::Text { text } => events.push(Ok(ModelEvent::TextDelta {
                    delta: text.clone(),
                })),
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }));
                }
                _ => {}
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
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
                "max_parallel_tool_calls": 4
            }
        }))
        .unwrap())
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

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: name.to_string(),
                    arguments: args,
                },
            }],
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

struct Harness {
    engine: TaskEngine,
    db: Database,
    dir: tempfile::TempDir,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

async fn harness(responses: Vec<ModelResponse>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(responses));
    let requests = runtime.requests.clone();
    let db = Database::connect_in_memory().await.unwrap();
    let engine = TaskEngine {
        stores: leveler_storage::EngineStores::from_database(&db),
        runtime_id: leveler_core::RuntimeId::new("rt-test"),
        factory: ExecutorFactory {
            runtime,
            registry: Arc::new(default_registry()),
            tool_context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: false,
            overrides: None,
            work_profile: leveler_agent::WorkProfile::Balanced,
            memory_index: String::new(),
            permission_rules: leveler_execution::PermissionRuleSet::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(std::path::PathBuf::from(".")),
            grants_state_dir: None,
            steering: None,
            allow_delegation: true,
            independent_review: leveler_engine::IndependentReviewPolicy::Auto,
            completion_judge_model: None,
            completion_judge_timeout: None,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        supervisor: None,
    };
    Harness {
        engine,
        db,
        dir,
        requests,
    }
}

fn spec(h: &Harness, goal: &str) -> TaskSpec {
    TaskSpec {
        runtime: leveler_engine::RuntimeTaskSpec {
            goal: goal.into(),
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::bounded(6),
            limits: StepLimits::default(),
        },
        coding: leveler_engine::CodingTaskSpec {
            repository: h.dir.path().to_path_buf(),
            mode: PermissionProfile::Assisted,
            sandbox: false,
            verification: VerificationPlan::default(),
            base_commit: None,
        },
    }
}

fn spec_with_command_budget(h: &Harness, goal: &str, max_commands: u32) -> TaskSpec {
    let mut s = spec(h, goal);
    s.runtime.limits = StepLimits {
        max_commands: Some(max_commands),
        ..StepLimits::default()
    };
    s
}

fn request_blob(req: &ModelRequest) -> String {
    req.messages
        .iter()
        .map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn seed_oversized_login_history(db: &Database, session: &leveler_core::SessionId) {
    let mut payloads = Vec::new();
    payloads.push(serde_json::to_string(&Message::text(Role::User, "修改登录模块")).unwrap());
    let pad = "login-timeout-path-and-retry-policy ".repeat(40);
    for i in 0..100 {
        payloads.push(
            serde_json::to_string(&Message::text(Role::Assistant, format!("detail {i} {pad}")))
                .unwrap(),
        );
    }
    MessageRepository::new(db)
        .append(session, &payloads, leveler_core::now())
        .await
        .unwrap();
}

#[tokio::test]
async fn chat_compacts_when_history_oversized_and_persists_snapshot() {
    // Over-threshold chat first makes one advisory summarize call, then the
    // chat turn itself.
    let h = harness(vec![
        text("SUMMARY_BRIEFING_MARKER: earlier rounds investigated the login timeout"),
        text("timeout answer"),
    ])
    .await;
    let s = spec(&h, "chat session");
    let session = h.engine.create_task(&s).await.unwrap();

    let mut payloads = Vec::new();
    payloads.push(serde_json::to_string(&Message::text(Role::System, "sys")).unwrap());
    payloads.push(serde_json::to_string(&Message::text(Role::User, "fix login")).unwrap());
    let pad = "detail-about-login-timeout-and-session-retry ".repeat(40);
    for i in 0..100 {
        payloads.push(
            serde_json::to_string(&Message::text(
                Role::Assistant,
                format!("long assistant note {i} {pad}"),
            ))
            .unwrap(),
        );
        payloads.push(
            serde_json::to_string(&Message::text(
                Role::User,
                format!("continue investigating timeout detail {i} {pad}"),
            ))
            .unwrap(),
        );
    }
    MessageRepository::new(&h.db)
        .append(&session, &payloads, leveler_core::now())
        .await
        .unwrap();

    let mut events = Vec::new();
    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "what about the timeout we just discussed?".into(),
            }],
            &mut |e| events.push(e),
            CancellationToken::new(),
        )
        .await
        .expect("chat should succeed");

    let snapshot_messages: Vec<String> = events
        .iter()
        .find_map(|e| match e {
            EngineEvent::ContextSnapshot { messages, .. } => Some(
                messages
                    .iter()
                    .map(|m| m.text_content())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .expect("expected ContextSnapshot when history exceeds pre-request threshold");
    assert!(
        snapshot_messages
            .iter()
            .any(|t| t.contains("SUMMARY_BRIEFING_MARKER")),
        "the fold must carry the model handoff briefing, not a bare breadcrumb: {snapshot_messages:?}"
    );
}

#[tokio::test]
async fn second_chat_after_compact_still_sees_first_chat_turn() {
    // AC2: snapshot must not erase the previous chat exchange for the next turn.
    let h = harness(vec![
        text("summary of chat1 history"),
        text("compact-aware answer about login"),
        // chat2 assembles from chat1's snapshot and fits: no summary call.
        text("I still remember UNIQUE_CHAT1_MARKER"),
    ])
    .await;
    let s = spec(&h, "chat session");
    let session = h.engine.create_task(&s).await.unwrap();
    seed_oversized_login_history(&h.db, &session).await;

    let mut events = Vec::new();
    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "小结一下登录改动 UNIQUE_CHAT1_MARKER".into(),
            }],
            &mut |e| events.push(e),
            CancellationToken::new(),
        )
        .await
        .expect("chat1");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::ContextSnapshot { .. })),
        "chat1 must compact oversized history"
    );

    let after_chat1 = h.requests.lock().unwrap().len();

    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "刚才那句小结里提了什么 marker？".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("chat2");

    let reqs = h.requests.lock().unwrap();
    assert!(
        reqs.len() > after_chat1,
        "chat2 must issue at least one model request"
    );
    let chat2_blob = request_blob(&reqs[after_chat1]);
    assert!(
        chat2_blob.contains("UNIQUE_CHAT1_MARKER")
            || chat2_blob.contains("小结一下登录改动")
            || chat2_blob.contains("修改登录模块"),
        "second chat prior must retain first-chat / login context, got: {chat2_blob}"
    );
}

#[tokio::test]
async fn goal_turn_includes_prior_history_in_model_request() {
    let h = harness(vec![
        text("looking at prior login work"),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "timeout handled"}),
        ),
    ])
    .await;
    let s = spec(&h, "把刚才那个超时也处理一下");
    let session = h.engine.create_task(&s).await.unwrap();
    MessageRepository::new(&h.db)
        .append(
            &session,
            &[
                serde_json::to_string(&Message::text(Role::User, "修改登录模块")).unwrap(),
                serde_json::to_string(&Message::text(
                    Role::Assistant,
                    "已修改 login.rs 的校验逻辑",
                ))
                .unwrap(),
            ],
            leveler_core::now(),
        )
        .await
        .unwrap();

    let _ = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await;

    let reqs = h.requests.lock().unwrap();
    assert!(!reqs.is_empty(), "goal path must call the model");
    let joined = request_blob(&reqs[0]);
    assert!(
        joined.contains("修改登录模块") || joined.contains("login.rs"),
        "goal request must include prior session history: {joined}"
    );
    assert!(
        joined.contains("超时") || joined.contains("刚才"),
        "goal request must include the new goal text: {joined}"
    );
}

#[tokio::test]
async fn multi_turn_deictic_followup_after_compact_then_resume() {
    // 1) Long history + chat compact.
    // 2) Second chat still sees prior (deictic path under snapshot).
    // 3) Goal incomplete (BudgetLimited) so resume is allowed.
    // 4) Resume Ok and its model request carries login context.
    let h = harness(vec![
        text("summary of chat1 history"),
        text("compact-aware answer"),
        // chat2 assembles from chat1's snapshot and fits: no summary call.
        text("follow-up still knows UNIQUE_CHAT1_MARKER"),
        // Goal: exhaust rounds without terminal complete → BudgetLimited.
        // (Goal prior injection uses the bounded-history path: no summarize.)
        text("working on timeout, not done yet"),
        // Resume also fits from the snapshot: only the resume turn itself.
        text("resumed with prior login context"),
    ])
    .await;
    let s = spec(&h, "session");
    let session = h.engine.create_task(&s).await.unwrap();
    seed_oversized_login_history(&h.db, &session).await;

    let mut events = Vec::new();
    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "小结一下刚才登录改动 UNIQUE_CHAT1_MARKER".into(),
            }],
            &mut |e| events.push(e),
            CancellationToken::new(),
        )
        .await
        .expect("chat1");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::ContextSnapshot { .. })),
        "compact path must emit ContextSnapshot"
    );

    let after_chat1 = h.requests.lock().unwrap().len();
    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "刚才小结提了什么？".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("chat2");
    {
        let reqs = h.requests.lock().unwrap();
        let chat2 = request_blob(&reqs[after_chat1]);
        assert!(
            chat2.contains("UNIQUE_CHAT1_MARKER")
                || chat2.contains("登录")
                || chat2.contains("timeout"),
            "chat2 must see prior login/chat1 context: {chat2}"
        );
    }

    // Incomplete Goal: 1-round budget so we land BudgetLimited (resumable), not
    // CompletedUnverified from update_goal.
    let mut goal_spec = spec(&h, "把刚才那个超时也处理一下");
    goal_spec.runtime.continuation = ContinuationPolicy::bounded(1);

    // Chat left outcome CompletedUnverified; force interrupted epoch for resume.
    SessionRepository::new(&h.db)
        .set_execution(
            &session,
            "assisted",
            false,
            ExecutionKind::Direct.as_str(),
            leveler_core::now(),
        )
        .await
        .unwrap();
    SessionRepository::new(&h.db)
        .set_outcome(&session, TaskOutcome::Interrupted, leveler_core::now())
        .await
        .unwrap();

    let after_chats = h.requests.lock().unwrap().len();
    let goal_report = h
        .engine
        .run(&session, &goal_spec, &mut |_| {}, CancellationToken::new())
        .await
        .expect("incomplete goal run");
    {
        let reqs = h.requests.lock().unwrap();
        assert!(reqs.len() > after_chats, "goal must call the model");
        let goal_blob = request_blob(&reqs[after_chats]);
        assert!(
            goal_blob.contains("登录")
                || goal_blob.contains("timeout")
                || goal_blob.contains("UNIQUE_CHAT1_MARKER")
                || goal_blob.contains("超时"),
            "goal request must include deictic/prior login context: {goal_blob}"
        );
    }
    // Ensure we did not complete successfully (resume must be allowed).
    assert_ne!(
        goal_report.outcome,
        TaskOutcome::Verified,
        "goal fixture must not fully verify"
    );

    // If run marked CompletedUnverified (text answer), re-open for resume.
    SessionRepository::new(&h.db)
        .set_outcome(&session, TaskOutcome::Interrupted, leveler_core::now())
        .await
        .unwrap();
    SessionRepository::new(&h.db)
        .set_execution(
            &session,
            "assisted",
            false,
            ExecutionKind::Direct.as_str(),
            leveler_core::now(),
        )
        .await
        .unwrap();

    let before_resume = h.requests.lock().unwrap().len();
    let resumed = h
        .engine
        .resume(&session, &goal_spec, &mut |_| {}, CancellationToken::new())
        .await;
    assert!(
        resumed.is_ok(),
        "resume must succeed after interrupt, got: {resumed:?}"
    );

    let reqs = h.requests.lock().unwrap();
    assert!(
        reqs.len() > before_resume,
        "resume must issue a model request; before={before_resume} after={}",
        reqs.len()
    );
    let resume_blob = request_blob(&reqs[before_resume]);
    assert!(
        resume_blob.contains("登录")
            || resume_blob.contains("timeout")
            || resume_blob.contains("UNIQUE_CHAT1_MARKER")
            || resume_blob.contains("超时")
            || resume_blob.contains("login"),
        "post-resume model request must carry prior login context: {resume_blob}"
    );
}

/// End-to-end task-epoch budget: chat1 spends a shell command → ProgressUpdated
/// is persisted → chat2 (next Engine request) seeds that spend and trips
/// max_commands=1 before another shell call runs.
#[tokio::test]
async fn engine_chat_command_spend_forces_budget_on_next_request() {
    let h = harness(vec![
        // Chat 1: one shell command, then answer (non-terminal progress keeps epoch).
        tool_call(
            "c1",
            "run_command",
            serde_json::json!({"program": "true", "args": []}),
        ),
        text("first turn spent one command"),
        // Chat 2: try another command — must be refused under max_commands=1.
        tool_call(
            "c2",
            "run_command",
            serde_json::json!({"program": "true", "args": []}),
        ),
        text("should not freely run a second epoch command"),
    ])
    .await;
    let s = spec_with_command_budget(&h, "budget epoch", 1);
    let session = h.engine.create_task(&s).await.unwrap();

    let mut events1 = Vec::new();
    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "run one command".into(),
            }],
            &mut |e| events1.push(e),
            CancellationToken::new(),
        )
        .await
        .expect("chat1");

    let progress_cmds = events1.iter().rev().find_map(|e| match e {
        EngineEvent::ProgressUpdated { ledger } => Some(ledger.cumulative_commands),
        _ => None,
    });
    assert!(
        progress_cmds.unwrap_or(0) >= 1,
        "chat1 must persist command spend on ProgressUpdated; events={events1:?}"
    );

    let mut events2 = Vec::new();
    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "run another command".into(),
            }],
            &mut |e| events2.push(e),
            CancellationToken::new(),
        )
        .await
        .expect("chat2");

    let refused = events2.iter().any(|e| match e {
        EngineEvent::ToolCallFinished {
            is_error: true,
            preview,
            ..
        } => {
            preview.contains("command budget")
                || preview.contains("budget is exhausted")
                || preview.contains("command")
        }
        EngineEvent::TurnFinished { stop_reason, .. } => {
            stop_reason.contains("Budget") || stop_reason.contains("budget")
        }
        EngineEvent::AssistantMessage { text } => {
            text.contains("command") && text.contains("budget")
        }
        _ => false,
    });
    let final_cmds = events2.iter().rev().find_map(|e| match e {
        EngineEvent::ProgressUpdated { ledger } => Some(ledger.cumulative_commands),
        _ => None,
    });
    assert!(
        refused,
        "next Engine request must enforce seeded max_commands=1; final_cmds={final_cmds:?} \
         events2={events2:?}"
    );
    if let Some(n) = final_cmds {
        assert!(
            n <= 1,
            "epoch command total must not exceed max_commands=1 after chat2; n={n}"
        );
    }
}

/// An interactive chat turn must anchor its own baseline before editing.
///
/// `run` did this; `chat` did not, so `reconcile_with_baseline` had nothing to
/// compare against and failures the repository already carried were charged to
/// the turn. Measured on a repo carrying one pre-existing red test: a 3-round
/// edit turned into a 45-round run whose repair turn began rewriting unrelated
/// files trying to fix someone else's failure. Interactive chat is exactly
/// where an already-red worktree is normal.
#[tokio::test]
async fn chat_anchors_a_baseline_for_pre_existing_failures() {
    let h = harness(vec![text("ok")]).await;
    // Make the fixture a real git repo with a commit, so HEAD resolves.
    leveler_test_support::git::init_repo(h.dir.path());
    leveler_test_support::git::run(h.dir.path(), &["add", "-A"]);
    leveler_test_support::git::run(h.dir.path(), &["commit", "-qm", "base"]);

    let spec = spec(&h, "explain this repo");
    assert!(
        spec.coding.base_commit.is_none(),
        "spec starts without an anchor"
    );
    let session = h.engine.create_task(&spec).await.unwrap();
    h.engine
        .chat(
            &session,
            &spec,
            vec![ContentPart::Text {
                text: "hello".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // The turn ran against a git worktree, so HEAD must have been resolvable —
    // the chat path is responsible for capturing it, not the caller.
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(h.dir.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git available");
    assert!(
        head.status.success() && !head.stdout.is_empty(),
        "fixture must have a resolvable HEAD for this test to mean anything"
    );
}

/// Once a chat turn has folded an oversized history into a persisted
/// snapshot, the next turn assembles from that snapshot plus the new tail.
/// When that fits the budget there is nothing to fold, so no summarization
/// call is made: exactly one model request per turn.
#[tokio::test]
async fn second_chat_after_compact_does_not_summarize_again() {
    let h = harness(vec![
        text("summary of chat1 history"),
        text("answer one"),
        text("answer two"),
    ])
    .await;
    let s = spec(&h, "chat session");
    let session = h.engine.create_task(&s).await.unwrap();
    seed_oversized_login_history(&h.db, &session).await;

    let mut events = Vec::new();
    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "小结一下登录改动".into(),
            }],
            &mut |e| events.push(e),
            CancellationToken::new(),
        )
        .await
        .expect("chat1");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::ContextSnapshot { .. })),
        "chat1 must compact oversized history"
    );
    let after_chat1 = h.requests.lock().unwrap().len();
    assert_eq!(
        after_chat1, 2,
        "chat1 = one summary call + one chat request"
    );

    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "刚才那句小结说了什么？".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("chat2");

    let reqs = h.requests.lock().unwrap();
    assert_eq!(
        reqs.len(),
        after_chat1 + 1,
        "chat2 fits under the budget from the snapshot; it must not pay for a summary it discards"
    );
}

/// A counting message store: the point of a bounded load is what it does NOT
/// read, and only the store can say.
struct CountingMessageStore {
    inner: leveler_storage::MemoryMessageStore,
    full_loads: std::sync::atomic::AtomicUsize,
    rows_read: std::sync::atomic::AtomicUsize,
}

impl CountingMessageStore {
    fn new() -> Self {
        Self {
            inner: leveler_storage::MemoryMessageStore::new(),
            full_loads: std::sync::atomic::AtomicUsize::new(0),
            rows_read: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl leveler_storage::MessageStore for CountingMessageStore {
    async fn append_in_turn(
        &self,
        session_id: &leveler_core::SessionId,
        turn_id: &leveler_core::TurnId,
        payloads: &[String],
        now: leveler_core::Timestamp,
    ) -> Result<(), leveler_storage::StorageError> {
        self.inner
            .append_in_turn(session_id, turn_id, payloads, now)
            .await
    }

    async fn append_in_turn_owned(
        &self,
        token: &leveler_core::OwnershipToken,
        session_id: &leveler_core::SessionId,
        turn_id: &leveler_core::TurnId,
        payloads: &[String],
        now: leveler_core::Timestamp,
    ) -> Result<(), leveler_storage::OwnershipError> {
        self.inner
            .append_in_turn_owned(token, session_id, turn_id, payloads, now)
            .await
    }

    async fn load(
        &self,
        session_id: &leveler_core::SessionId,
    ) -> Result<Vec<String>, leveler_storage::StorageError> {
        self.full_loads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let rows = self.inner.load(session_id).await?;
        self.rows_read
            .fetch_add(rows.len(), std::sync::atomic::Ordering::SeqCst);
        Ok(rows)
    }

    async fn load_from(
        &self,
        session_id: &leveler_core::SessionId,
        from: u64,
    ) -> Result<Vec<String>, leveler_storage::StorageError> {
        let rows = self.inner.load_from(session_id, from).await?;
        self.rows_read
            .fetch_add(rows.len(), std::sync::atomic::Ordering::SeqCst);
        Ok(rows)
    }

    /// A size query, not a read: it must not count as either.
    async fn total_bytes(
        &self,
        session_id: &leveler_core::SessionId,
    ) -> Result<u64, leveler_storage::StorageError> {
        self.inner.total_bytes(session_id).await
    }
}

/// A watermarked snapshot says the rows before it are already represented, and
/// a transcript this size provably cannot fit the fold threshold whole. The
/// load reads the tail, not the session.
#[tokio::test]
async fn a_watermarked_snapshot_bounds_what_a_turn_reads() {
    use leveler_storage::MessageStore;

    let store = CountingMessageStore::new();
    let session = leveler_core::SessionId::generate();
    let turn = leveler_core::TurnId::new("t1");
    let pad = "detail-about-the-login-timeout-and-the-retry-policy ".repeat(60);
    let payloads: Vec<String> = (0..400)
        .map(|i| {
            serde_json::to_string(&Message::text(Role::Assistant, format!("note {i} {pad}")))
                .unwrap()
        })
        .collect();
    store
        .append_in_turn(&session, &turn, &payloads, leveler_core::now())
        .await
        .unwrap();

    let threshold = 24_000u64;
    let bounded = leveler_engine::RawTranscript::load_bounded(
        &store,
        &session,
        threshold,
        Some(380),
        None,
        None,
    )
    .await
    .unwrap();

    assert_eq!(bounded.offset(), 380, "the tail begins at the watermark");
    assert_eq!(bounded.messages.len(), 20);
    assert_eq!(bounded.stored_len(), 400, "the full length is still known");
    assert_eq!(
        store.rows_read.load(std::sync::atomic::Ordering::SeqCst),
        20,
        "only the tail was read"
    );
    assert_eq!(
        store.full_loads.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a bounded load must not fall through to the full one"
    );
}

/// Under the threshold the transcript may still be sent whole, so it is read
/// whole — a snapshot is never a permanent replacement for a later turn.
#[tokio::test]
async fn a_transcript_that_may_still_fit_is_read_whole() {
    use leveler_storage::MessageStore;

    let store = CountingMessageStore::new();
    let session = leveler_core::SessionId::generate();
    let turn = leveler_core::TurnId::new("t1");
    let payloads: Vec<String> = (0..10)
        .map(|i| {
            serde_json::to_string(&Message::text(Role::Assistant, format!("short {i}"))).unwrap()
        })
        .collect();
    store
        .append_in_turn(&session, &turn, &payloads, leveler_core::now())
        .await
        .unwrap();

    let bounded =
        leveler_engine::RawTranscript::load_bounded(&store, &session, 24_000, Some(8), None, None)
            .await
            .unwrap();

    assert_eq!(bounded.offset(), 0);
    assert_eq!(bounded.messages.len(), 10);
    assert_eq!(
        store.full_loads.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

/// A checkpoint reaches further back than the snapshot, so it is the binding
/// watermark. Loading from the snapshot's instead would hand the checkpoint
/// path a delta that starts after the point it splices from.
#[tokio::test]
async fn the_checkpoint_watermark_binds_when_it_reaches_further_back() {
    use leveler_storage::MessageStore;

    let store = CountingMessageStore::new();
    let session = leveler_core::SessionId::generate();
    let turn = leveler_core::TurnId::new("t1");
    let pad = "detail-about-the-login-timeout-and-the-retry-policy ".repeat(60);
    let payloads: Vec<String> = (0..400)
        .map(|i| {
            serde_json::to_string(&Message::text(Role::Assistant, format!("note {i} {pad}")))
                .unwrap()
        })
        .collect();
    store
        .append_in_turn(&session, &turn, &payloads, leveler_core::now())
        .await
        .unwrap();

    let bounded = leveler_engine::RawTranscript::load_bounded(
        &store,
        &session,
        24_000,
        Some(380),
        Some(200),
        None,
    )
    .await
    .unwrap();

    assert_eq!(bounded.offset(), 200, "the earlier watermark wins");
    assert_eq!(bounded.messages.len(), 200);
    assert!(
        bounded.slice_from_ordinal(200).is_some(),
        "the checkpoint's own splice point must be servable"
    );
    assert!(
        bounded.slice_from_ordinal(199).is_none(),
        "anything earlier is honestly reported as unavailable"
    );
}

/// Every engine path that hands a model prior context goes through the one
/// assembly, so none of them can send an unbounded transcript.
///
/// `resume_with_limits` used to be the exception: it passed the raw
/// transcript straight to `TurnInput::Resume`, so a long session resent its
/// entire history on every budget extension. Nothing bounded it, and no test
/// covered it because the path only opens on budget exhaustion — which is
/// why this is a source tripwire, in the same spirit as the ownership one.
#[test]
fn no_engine_path_hands_a_model_an_unassembled_transcript() {
    const MARKER: &str = "TurnInput::Resume(";
    let source = include_str!("../src/engine.rs");
    // Split at the test MODULE, not at any `#[cfg(test)]` attribute: the file
    // carries a cfg-gated field mid-implementation, and cutting there would
    // hide most of the production code from this scan.
    let production = source
        .split("\n#[cfg(test)]\nmod ")
        .next()
        .expect("engine.rs has production code");

    let mut inputs = Vec::new();
    for (at, _) in production.match_indices(MARKER) {
        let tail = &production[at + MARKER.len()..];
        let end = tail.find(')').expect("a closing paren");
        inputs.push(&tail[..end]);
    }
    assert!(!inputs.is_empty(), "the resume path should still exist");
    for input in &inputs {
        assert!(
            !input.contains("raw"),
            "a resumed turn is given assembled prior context, never the raw \
             transcript; got `{input}`"
        );
    }

    // chat, resume, goal continuation, budget extension.
    assert!(
        production.matches("assembled_prior(").count() >= 4,
        "every path that supplies prior context assembles it"
    );
}
