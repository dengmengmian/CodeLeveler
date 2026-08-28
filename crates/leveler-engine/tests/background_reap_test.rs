//! R006 R6-P4 accident regression: session-owned background tasks must be
//! reaped at the engine's terminal settlement on EVERY route — including the
//! chat-routed continuation path that leaked `./server` + `npm run start` in
//! production. Daemon-scoped tasks and interrupted turns must NOT be reaped.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AutoClarify, ContinuationPolicy, StepLimits};
use leveler_core::{RequestId, ToolCallId};
use leveler_engine::{ExecutionKind, ExecutorFactory, TaskEngine, TaskSpec};
use leveler_execution::{
    AutoApprove, BackgroundTaskRegistry, BackgroundTaskStatus, PermissionProfile, ProcessRequest,
    Workspace,
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEvent, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::Database;
use leveler_tools::{ToolContext, default_registry};
use leveler_verifier::VerificationPlan;

const SESSION_SCOPE: &str = "sess-under-test";

struct MockRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
}

impl MockRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
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

fn spawn_sleep_server() -> ModelResponse {
    tool_call(
        "bg1",
        "run_command",
        serde_json::json!({
            "program": "sleep",
            "args": ["300"],
            "background": true
        }),
    )
}

struct Harness {
    engine: TaskEngine,
    #[allow(dead_code)]
    db: Database,
    dir: tempfile::TempDir,
    registry: Arc<BackgroundTaskRegistry>,
}

async fn harness(responses: Vec<ModelResponse>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    // Tests never initialize the global env snapshot, so the registry default
    // (empty env → no resolvable LevelerHome) would fail sandbox scratch prep.
    let environment = Arc::new(leveler_core::EnvSnapshot::new(
        std::env::vars_os(),
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    ));
    let registry = Arc::new(BackgroundTaskRegistry::with_environment(
        environment.clone(),
    ));
    let tool_context =
        ToolContext::with_environment(workspace, PermissionProfile::Assisted, environment)
            .with_background_tasks(registry.clone())
            .with_session_scope(SESSION_SCOPE);
    let runtime = Arc::new(MockRuntime::new(responses));
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
        registry,
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

/// Wait for the reaped task to reach a terminal status. `kill_scope` signals
/// SIGTERM→SIGKILL; the wait reaper records the terminal fact shortly after.
async fn assert_terminal_within(registry: &BackgroundTaskRegistry, id: &str, what: &str) {
    let snap = registry
        .wait(id, Some(Duration::from_secs(10)), &CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{what}: task `{id}` should exist and terminate: {e}"));
    assert!(
        matches!(
            snap.status,
            BackgroundTaskStatus::Exited | BackgroundTaskStatus::Killed
        ),
        "{what}: task `{id}` should be terminal after settlement, got {:?}",
        snap.status
    );
}

/// The R006 accident, exactly: a goal continued through ordinary messages
/// (chat-routed) spawns a background dev server; when the turn reaches its
/// terminal fact, the session-owned task must be reaped. Before the fix the
/// reap hung off `spawn_direct_goal_turn` only, so this path leaked the
/// server (R006 P4 FAIL: `./server` + `npm run start` survived the goal).
#[tokio::test]
async fn chat_routed_terminal_reaps_session_owned_background_tasks() {
    let h = harness(vec![spawn_sleep_server(), text("server started; done")]).await;
    let s = spec(&h, "start the dev server and report");
    let session = h.engine.create_task(&s).await.unwrap();

    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "继续：起动 dev server 验证".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("chat turn should settle");

    assert_terminal_within(&h.registry, "bg-1", "chat path").await;
    assert_eq!(
        h.registry.kill_scope(SESSION_SCOPE).await,
        0,
        "no session-owned task may survive the chat-routed terminal settlement"
    );
}

/// Control: the direct/goal route (the one the R004 fix DID cover) must still
/// reap now that the reap lives at the engine's terminal settlement.
#[tokio::test]
async fn direct_run_terminal_reaps_session_owned_background_tasks() {
    let h = harness(vec![
        spawn_sleep_server(),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "server verified"}),
        ),
    ])
    .await;
    let s = spec(&h, "start the dev server and finish");
    let session = h.engine.create_task(&s).await.unwrap();

    h.engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .expect("direct run should settle");

    assert_terminal_within(&h.registry, "bg-1", "direct path").await;
    assert_eq!(h.registry.kill_scope(SESSION_SCOPE).await, 0);
}

/// Daemon-scoped tasks (no owner scope — browser runtime, MCP servers) must
/// survive a session's terminal settlement untouched.
#[tokio::test]
async fn daemon_scoped_tasks_survive_session_terminal() {
    let h = harness(vec![text("nothing to do")]).await;
    let s = spec(&h, "answer a question");
    let daemon_task = h
        .registry
        .spawn_owned(
            ProcessRequest::new("sleep", vec!["300".into()], h.dir.path().to_path_buf()),
            None,
            None, // daemon-scoped: no owner
        )
        .await
        .expect("daemon-scoped spawn");
    let session = h.engine.create_task(&s).await.unwrap();

    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text { text: "hi".into() }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("chat turn should settle");

    let snap = h
        .registry
        .get(&daemon_task)
        .await
        .expect("daemon task still registered");
    assert!(
        matches!(
            snap.status,
            BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing
        ),
        "daemon-scoped task must survive the session reap, got {:?}",
        snap.status
    );
    h.registry.kill_all().await;
}

/// Interrupt (user cancel) deliberately skips the reap: the user may want to
/// inspect or resume against the still-running server.
#[tokio::test]
async fn interrupted_turn_keeps_session_owned_tasks_alive() {
    let h = harness(vec![text("unreached")]).await;
    let s = spec(&h, "long task");
    let owned = h
        .registry
        .spawn_owned(
            ProcessRequest::new("sleep", vec!["300".into()], h.dir.path().to_path_buf()),
            None,
            Some(SESSION_SCOPE),
        )
        .await
        .expect("session-owned spawn");
    let session = h.engine.create_task(&s).await.unwrap();

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let result = h
        .engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text { text: "hi".into() }],
            &mut |_| {},
            cancelled,
        )
        .await;
    assert!(result.is_err(), "pre-cancelled turn should not settle Ok");

    let snap = h.registry.get(&owned).await.expect("task still registered");
    assert!(
        matches!(
            snap.status,
            BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing
        ),
        "interrupted turn must NOT reap session-owned tasks, got {:?}",
        snap.status
    );
    h.registry.kill_all().await;
}

/// R007 F3 accident shape: a goal that runs out of WORK-WINDOW budget is not
/// a goal that finished. R007 hit the 100-round ceiling twice; each time the
/// terminal settlement reaped the dev server the agent had started, and the
/// next window spent its rounds rebuilding the same environment instead of
/// doing the task.
///
/// A window boundary leaves the session resumable (`Incomplete` / `Execute`),
/// so a service the goal started must survive it. R6-P4 is unaffected: a
/// genuine goal terminal still reaps, which the sibling tests pin.
#[tokio::test]
async fn a_work_window_boundary_keeps_goal_owned_services_alive() {
    // One round of budget: the turn spawns the server and immediately runs
    // out of window, which is exactly the ceiling shape.
    let h = harness(vec![spawn_sleep_server(), text("still working")]).await;
    let mut s = spec(&h, "start the dev server, then keep working");
    s.runtime.limits = StepLimits {
        max_rounds: Some(1),
        ..StepLimits::default()
    };
    let session = h.engine.create_task(&s).await.unwrap();

    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .expect("run should settle at the window boundary");
    assert!(
        matches!(
            report.stop_reason,
            leveler_agent::StopReason::TurnLimitReached
                | leveler_agent::StopReason::BudgetExhausted
        ),
        "test needs a window-budget terminal, got {:?}",
        report.stop_reason
    );

    // The service must still be running: the goal is resumable, and the next
    // window should find its environment intact.
    let survivors = h.registry.kill_scope(SESSION_SCOPE).await;
    assert_eq!(
        survivors, 1,
        "a goal-owned service must survive a work-window boundary so the next \
         window does not rebuild the environment"
    );
}
