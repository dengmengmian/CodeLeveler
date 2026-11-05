//! R006 R6-P4 accident regression: session-owned background tasks must be
//! reaped at the engine's terminal settlement on EVERY route — including the
//! chat-routed continuation path that leaked `./server` + `npm run start` in
//! production. Daemon-scoped tasks and interrupted turns must NOT be reaped.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::coding::{CodingRuntime, ExecutorFactory, TaskSpec};
use leveler_agent::{AutoClarify, ContinuationPolicy, StepLimits};
use leveler_core::{RequestId, ToolCallId};
use leveler_engine::{ExecutionKind, TaskEngine};
use leveler_execution::{
    AutoApprove, BackgroundTaskRegistry, BackgroundTaskStatus, PermissionProfile, ProcessRequest,
    Workspace,
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEvent, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_storage::Database;
use leveler_tools::ToolContext;

const SESSION_SCOPE: &str = "sess-under-test";

struct MockRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    error_when_exhausted: Mutex<ModelError>,
    cancel_before_closeout: Mutex<Option<(Database, leveler_core::SessionId)>>,
}

/// Append the answer a real model must write after it resolves its goal.
///
/// The runtime takes one delivery round after `update_goal` when the resolving
/// round carried no text, so a script that ends on that call needs one more
/// response. The fixture states the contract rather than relying on the turn
/// ending on a tool call.
fn with_delivery(mut responses: Vec<ModelResponse>) -> Vec<ModelResponse> {
    let resolves_without_text = responses.last().is_some_and(|response| {
        response.message.text_content().trim().is_empty()
            && response.message.content.iter().any(
                |part| matches!(part, ContentPart::ToolCall { call } if call.name == "update_goal"),
            )
    });
    if resolves_without_text {
        responses.push(text("Done: the goal is resolved and reported."));
    }
    responses
}

impl MockRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        let responses = with_delivery(responses);
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            error_when_exhausted: Mutex::new(ModelError::new(
                leveler_model::ModelErrorKind::Other,
                "no more responses",
            )),
            cancel_before_closeout: Mutex::new(None),
        }
    }
}

#[async_trait]
impl ModelRuntime for MockRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| self.error_when_exhausted.lock().unwrap().clone())?;
        if response.message.content.iter().any(
            |part| matches!(part, ContentPart::ToolCall { call } if call.name == "update_goal"),
        ) {
            let pending = self.cancel_before_closeout.lock().unwrap().take();
            if let Some((db, session)) = pending {
                let task = leveler_storage::TaskStore::task_for_session(&db, &session)
                    .await
                    .unwrap()
                    .unwrap();
                let owner = leveler_storage::OwnershipStore::current(&db, &task)
                    .await
                    .unwrap()
                    .unwrap();
                let token = leveler_core::OwnershipToken {
                    task_id: task,
                    runtime_id: owner.runtime.unwrap(),
                    boot_id: owner.boot.unwrap(),
                    owner_epoch: owner.epoch,
                };
                assert!(
                    leveler_storage::TerminalRepository::new(&db)
                        .request_task_cancel(&token, &session)
                        .await
                        .unwrap()
                );
            }
        }
        Ok(response)
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
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: name.to_string(),
                    arguments: args,
                },
            }],
            route: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

/// A model turn that backgrounds a long-lived process. The requirement is "a
/// process that stays alive", not `sleep` — Windows has no such program.
fn spawn_sleep_server() -> ModelResponse {
    let (program, args) = leveler_test_support::sleep_command(300);
    tool_call(
        "bg1",
        "run_command",
        serde_json::json!({
            "program": program,
            "args": args,
            "background": true
        }),
    )
}

/// A long-lived process the user explicitly asked the project runtime to keep
/// after the current goal reaches its terminal state.
fn spawn_runtime_sleep_server() -> ModelResponse {
    let (program, args) = leveler_test_support::sleep_command(300);
    tool_call(
        "bg-runtime",
        "run_command",
        serde_json::json!({
            "program": program,
            "args": args,
            "background": true,
            "background_lifetime": "runtime"
        }),
    )
}

/// The same long-lived process as a direct [`ProcessRequest`].
fn sleep_request(cwd: &std::path::Path) -> ProcessRequest {
    let (program, args) = leveler_test_support::sleep_command(300);
    ProcessRequest::new(program, args, cwd.to_path_buf())
}

struct Harness {
    engine: CodingRuntime,
    #[allow(dead_code)]
    db: Database,
    dir: tempfile::TempDir,
    registry: Arc<BackgroundTaskRegistry>,
    runtime: Arc<MockRuntime>,
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
        ToolContext::with_environment(workspace, PermissionProfile::Assisted, environment.clone())
            .with_session_scope(SESSION_SCOPE);
    // ONE registry: the tools that start and manage background tasks, and the
    // engine that reaps them at terminal settlement, must be looking at the
    // same one — that is the whole subject of this file.
    let capabilities = leveler_tools::Capabilities::in_process(environment)
        .with_background_tasks(registry.clone());
    let tool_registry =
        leveler_tools::model_surface(leveler_tools::CapabilityPacks::ALL, &capabilities);
    let runtime = Arc::new(MockRuntime::new(responses));
    let db = Database::connect_in_memory().await.unwrap();
    let engine = CodingRuntime {
        engine: TaskEngine {
            stores: leveler_storage::EngineStores::from_database(&db),
            runtime_id: leveler_core::RuntimeId::new("rt-test"),
            boot: leveler_engine::EngineBoot {
                id: leveler_core::BootId::generate(),
                liveness: std::sync::Arc::new(leveler_test_support::TestBoots::new()),
            },
        },
        factory: ExecutorFactory {
            tool_discovery: None,
            resource_grants: std::sync::Arc::new(
                leveler_storage::MemoryResourceGrantStore::default(),
            ),
            runtime: runtime.clone(),
            registry: Arc::new(tool_registry),
            tool_context,
            model: ModelRef::new("mock", "m"),
            commit_co_author: false,
            overrides: None,

            background_tasks: registry.clone(),
            permission_rules: leveler_execution::PermissionRuleSet::default(),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(std::path::PathBuf::from(".")),
            steering: None,
            allow_delegation: true,
            allow_host_input: true,
            capabilities: None,
            independent_review: leveler_agent::coding::IndependentReviewPolicy::Off,
        },
        approver: Arc::new(AutoApprove),
        clarifier: Arc::new(AutoClarify),
        task_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    Harness {
        engine,
        db,
        dir,
        registry,
        runtime,
    }
}

fn spec(h: &Harness, goal: &str) -> TaskSpec {
    TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: goal.into(),
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::bounded(6),
            limits: StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(h.dir.path().to_path_buf()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
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

/// The TUI's default Chat session owns no goal row. A `goal`-lifetime task it
/// starts must still stop at the turn's terminal: that lifetime's contract is
/// the creating terminal, not the presence of a goal. Reading the reap decision
/// through `goal_session` alone left this path running until it exited by
/// itself (the live PTY probe caught it as "not stopped at turn end").
#[tokio::test]
async fn a_goal_less_chat_session_reaps_goal_lifetime_tasks() {
    let h = harness(vec![spawn_sleep_server(), text("server started; done")]).await;
    let s = spec(&h, "start the dev server and report");
    let session = h
        .engine
        .engine
        .create_task(&leveler_engine::NewSession {
            workspace: Some(h.dir.path().display().to_string()),
            goal: String::new(),
            model: Some("mock/m".into()),
            mode: "assisted".into(),
            sandbox: false,
            kind: leveler_engine::ExecutionKind::Direct,
            axes: Some(leveler_engine::NewSessionAxes {
                collaboration: "chat".into(),
            }),
            model_pinned: true,
        })
        .await
        .unwrap();

    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "起服务".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("chat turn should settle");

    assert_terminal_within(&h.registry, "bg-1", "goal-less chat path").await;
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

/// A durable cancellation wins even when the model reports completion and
/// neither the executor token nor the in-memory task-cancel flag was signalled.
#[tokio::test]
async fn durable_cancel_wins_completed_goal_and_cleans_exact_background_targets() {
    let h = harness(vec![
        spawn_sleep_server(),
        tool_call(
            "closeout",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "done"}),
        ),
    ])
    .await;
    let s = spec(&h, "start a service and finish");
    let session = h.engine.create_task(&s).await.unwrap();
    *h.runtime.cancel_before_closeout.lock().unwrap() = Some((h.db.clone(), session.clone()));
    let result = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await;
    assert!(
        matches!(result, Err(leveler_engine::EngineError::Cancelled)),
        "{result:?}"
    );
    assert!(
        !h.engine.task_cancel.load(Ordering::SeqCst),
        "only durable intent arbitrated the race"
    );
    assert_terminal_within(&h.registry, "bg-1", "durable cancellation race").await;
    let task = leveler_storage::TaskStore::task_for_session(&h.db, &session)
        .await
        .unwrap()
        .unwrap();
    let goals = leveler_storage::GoalStore::for_task(&h.db, &task)
        .await
        .unwrap();
    assert!(!goals.is_empty());
    assert!(
        goals
            .iter()
            .all(|goal| goal.state == leveler_storage::GoalState::Settled)
    );
    let events = leveler_storage::EventRepository::new(&h.db)
        .load(&session)
        .await
        .unwrap();
    let terminals: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == "task_finished")
        .collect();
    assert_eq!(terminals.len(), 1);
    assert!(matches!(
        leveler_engine::EngineEvent::from_payload(&terminals[0].payload).unwrap(),
        leveler_engine::EngineEvent::TaskFinished {
            outcome: leveler_lifecycle::TaskOutcome::Cancelled,
            ..
        }
    ));
    assert!(
        leveler_storage::OwnershipStore::current(&h.db, &task)
            .await
            .unwrap()
            .unwrap()
            .runtime
            .is_none()
    );
}

/// A user can explicitly ask a dev server to outlive the goal that launched
/// it. It remains session-addressable for list/logs/stop, but terminal cleanup
/// must not reap it; runtime shutdown still owns the final cleanup.
#[tokio::test]
async fn runtime_lifetime_background_task_survives_goal_terminal() {
    let h = harness(vec![
        spawn_runtime_sleep_server(),
        tool_call(
            "g-runtime",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "server verified"}),
        ),
    ])
    .await;
    let s = spec(&h, "start the dev server and leave it running");
    let session = h.engine.create_task(&s).await.unwrap();

    h.engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .expect("direct run should settle");

    let snapshot = h.registry.get("bg-1").await.expect("task stays registered");
    assert_eq!(
        snapshot.status,
        BackgroundTaskStatus::Running,
        "an explicitly runtime-lived task must survive goal terminal cleanup"
    );
    assert_eq!(
        h.registry.kill_scope(SESSION_SCOPE).await,
        1,
        "the retained task remains session-addressable for explicit stop"
    );
    assert_terminal_within(&h.registry, "bg-1", "runtime-lived task cleanup").await;
}

/// The runtime proof for the terminal visibility invariant: the terminal
/// observer fires while the old process is still live, then post-terminal
/// settlement kills only the ids detached beforehand. A process admitted by
/// the observer under the same session scope is a newer epoch and survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_precedes_cleanup_and_the_ticket_spares_new_work() {
    let h = harness(vec![text("done")]).await;
    let old = h
        .registry
        .spawn_owned(sleep_request(h.dir.path()), None, Some(SESSION_SCOPE))
        .await
        .expect("old session process");
    let s = spec(&h, "answer and close");
    let session = h.engine.create_task(&s).await.unwrap();
    let terminal_saw_old = Arc::new(AtomicBool::new(false));
    let new_id = Arc::new(Mutex::new(None));
    let observer_registry = h.registry.clone();
    let observer_path = h.dir.path().to_path_buf();
    let observer_old = old.clone();
    let observer_saw_old = terminal_saw_old.clone();
    let observer_new_id = new_id.clone();

    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text { text: "hi".into() }],
            &mut |event| {
                if matches!(event, leveler_engine::EngineEvent::TaskFinished { .. }) {
                    observer_saw_old.store(
                        observer_registry
                            .try_active_ids_for_scope(SESSION_SCOPE)
                            .is_some_and(|ids| ids.contains(&observer_old)),
                        Ordering::SeqCst,
                    );
                    let id = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(observer_registry.spawn_owned(
                            sleep_request(&observer_path),
                            None,
                            Some(SESSION_SCOPE),
                        ))
                    })
                    .expect("newly admitted process");
                    *observer_new_id.lock().unwrap() = Some(id);
                }
            },
            CancellationToken::new(),
        )
        .await
        .expect("chat turn should settle");

    assert!(
        terminal_saw_old.load(Ordering::SeqCst),
        "TaskFinished must be projected before cleanup signals the old process"
    );
    assert_terminal_within(&h.registry, &old, "detached old process").await;
    let new_id = new_id
        .lock()
        .unwrap()
        .clone()
        .expect("observer spawned work");
    let new_snapshot = h
        .registry
        .get(&new_id)
        .await
        .expect("new process registered");
    assert_eq!(
        new_snapshot.status,
        BackgroundTaskStatus::Running,
        "the immutable cleanup ticket must not kill newly admitted work"
    );
    assert_eq!(
        h.registry.kill_scope(SESSION_SCOPE).await,
        1,
        "the new process remains independently owned by the session scope"
    );
    assert_terminal_within(&h.registry, &new_id, "new independently reaped process").await;
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
            sleep_request(h.dir.path()),
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
        .spawn_owned(sleep_request(h.dir.path()), None, Some(SESSION_SCOPE))
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
        max_model_steps: Some(1),
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

/// A provider stream failure ends a window, not the still-resumable goal that
/// owns its service. The next window must find the same live process.
#[tokio::test]
async fn a_provider_timeout_keeps_the_open_goals_service_alive() {
    let h = harness(vec![spawn_sleep_server()]).await;
    *h.runtime.error_when_exhausted.lock().unwrap() = ModelError::new(
        leveler_model::ModelErrorKind::Timeout,
        "error decoding response body",
    )
    .with_delivery_state(leveler_model::DeliveryState::StreamInterrupted {
        progress: leveler_model::StreamProgress {
            text: true,
            ..Default::default()
        },
    });
    let s = spec(&h, "start a service, then keep working");
    let session = h.engine.create_task(&s).await.unwrap();
    let result = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await;
    let task = leveler_storage::TaskStore::task_for_session(&h.db, &session)
        .await
        .unwrap()
        .unwrap();
    let goals = leveler_storage::GoalStore::for_task(&h.db, &task)
        .await
        .unwrap();
    let snapshot = h.registry.get("bg-1").await.unwrap();
    #[cfg(unix)]
    let process_alive = std::process::Command::new("kill")
        .args(["-0", &snapshot.pid.unwrap().to_string()])
        .status()
        .unwrap()
        .success();
    // Clean up even on the red run before asserting the captured facts.
    h.registry.kill_all().await;
    assert_terminal_within(&h.registry, "bg-1", "test cleanup").await;

    assert!(
        matches!(result, Err(leveler_engine::EngineError::Execution {
            model: Some(ref error), ..
        }) if error.kind == leveler_model::ModelErrorKind::Timeout),
        "the window must retain the actual provider timeout: {result:?}"
    );
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Running);
    assert_eq!(
        snapshot.status,
        BackgroundTaskStatus::Running,
        "a provider failure must not destroy the open goal's environment"
    );
    #[cfg(unix)]
    assert!(process_alive, "the service process must actually survive");
}

/// A provider rejection that cannot resume still settles the goal and reaps
/// its service; retaining processes is not a blanket exception for errors.
#[tokio::test]
async fn an_unrecoverable_provider_failure_reaps_the_settled_goals_service() {
    let h = harness(vec![spawn_sleep_server()]).await;
    *h.runtime.error_when_exhausted.lock().unwrap() = ModelError::new(
        leveler_model::ModelErrorKind::Auth,
        "provider authentication rejected",
    );
    let s = spec(&h, "start a service, then keep working");
    let session = h.engine.create_task(&s).await.unwrap();
    let result = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await;
    let task = leveler_storage::TaskStore::task_for_session(&h.db, &session)
        .await
        .unwrap()
        .unwrap();
    let goals = leveler_storage::GoalStore::for_task(&h.db, &task)
        .await
        .unwrap();
    assert_terminal_within(&h.registry, "bg-1", "unrecoverable provider failure").await;
    assert!(result.is_err(), "provider rejection must remain an error");
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].state, leveler_storage::GoalState::Settled);
}

/// Resume uses the original goal and background process after a transport
/// failure; observing and explicitly stopping it completes that same goal.
#[tokio::test]
async fn resume_after_provider_timeout_observes_and_stops_the_original_service() {
    let h = harness(vec![spawn_sleep_server()]).await;
    *h.runtime.error_when_exhausted.lock().unwrap() = ModelError::new(
        leveler_model::ModelErrorKind::Timeout,
        "response stream timed out",
    )
    .with_delivery_state(leveler_model::DeliveryState::StreamInterrupted {
        progress: leveler_model::StreamProgress {
            text: true,
            ..Default::default()
        },
    });
    let s = spec(&h, "start a service, observe it, then stop it and report");
    let session = h.engine.create_task(&s).await.unwrap();
    let first = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await;
    let task = leveler_storage::TaskStore::task_for_session(&h.db, &session)
        .await
        .unwrap()
        .unwrap();
    let before_goals = leveler_storage::GoalStore::for_task(&h.db, &task)
        .await
        .unwrap();
    let before = h.registry.get("bg-1").await.unwrap();
    let pid = before.pid.unwrap();
    #[cfg(unix)]
    let alive_before = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    eprintln!(
        "resume evidence before: goal={} state={:?} task=bg-1 pid={pid} status={:?}",
        before_goals[0].id, before_goals[0].state, before.status
    );

    h.runtime
        .responses
        .lock()
        .unwrap()
        .extend(with_delivery(vec![
            tool_call(
                "observe-existing",
                "get_task",
                serde_json::json!({"task_id": "bg-1"}),
            ),
            tool_call(
                "stop-existing",
                "kill_task",
                serde_json::json!({"task_id": "bg-1"}),
            ),
            tool_call(
                "finish-existing",
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "Observed the original service and stopped it."
                }),
            ),
        ]));
    let mut observed_running = false;
    let resumed = h
        .engine
        .resume_with_instruction(
            &session,
            &s,
            Message::user_input("继续"),
            &mut |event| {
                if let leveler_engine::EngineEvent::ToolCallFinished {
                    call_id,
                    is_error,
                    preview,
                    ..
                } = event
                    && call_id == "observe-existing"
                {
                    observed_running = !is_error && preview.contains("status: running");
                }
            },
            CancellationToken::new(),
        )
        .await;
    assert_terminal_within(&h.registry, "bg-1", "resumed explicit stop").await;
    let after = h.registry.get("bg-1").await.unwrap();
    #[cfg(unix)]
    let alive_after = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    let after_goals = leveler_storage::GoalStore::for_task(&h.db, &task)
        .await
        .unwrap();
    h.registry.kill_all().await;
    eprintln!(
        "resume evidence after: goal={} state={:?} task=bg-1 pid={:?} status={:?} tool_observed_running={observed_running}",
        after_goals[0].id, after_goals[0].state, after.pid, after.status
    );
    #[cfg(unix)]
    eprintln!(
        "resume OS evidence: pid={pid} alive_before={alive_before} alive_after={alive_after}"
    );

    assert!(
        first.is_err(),
        "first window must fail at the provider boundary"
    );
    assert_eq!(before_goals.len(), 1);
    assert_eq!(before_goals[0].state, leveler_storage::GoalState::Running);
    assert_eq!(before.status, BackgroundTaskStatus::Running);
    assert!(
        observed_running,
        "resumed model must observe the same live task through get_task"
    );
    assert_eq!(
        resumed.unwrap().stop_reason,
        leveler_agent::StopReason::Completed
    );
    assert_eq!(after_goals.len(), 1, "resume must not duplicate the goal");
    assert_eq!(after_goals[0].id, before_goals[0].id);
    assert_eq!(after_goals[0].state, leveler_storage::GoalState::Settled);
    assert_eq!(
        after.pid,
        Some(pid),
        "the stopped task must be the original process"
    );
    #[cfg(unix)]
    assert!(
        alive_before && !alive_after,
        "original PID must survive the fault and disappear after explicit stop"
    );
}

/// A background test run has not produced a result. Its START is not evidence
/// of a pass: the completion gate must keep the goal open when the only test
/// command in the record is a suite that is still running — even when the model
/// cites that command. `execution_status` is the mechanical line between
/// "started" and "ran to its own answer".
#[tokio::test]
async fn a_background_test_start_is_not_evidence_of_a_pass() {
    let (script_name, script) = if cfg!(windows) {
        ("run-tests.cmd", "ping -n 30 127.0.0.1 >NUL\r\n")
    } else {
        ("run-tests.sh", "sleep 30\n")
    };
    let (suite_program, suite_args) = if cfg!(windows) {
        ("cmd", vec!["/c", script_name])
    } else {
        ("sh", vec![script_name])
    };
    let suite_label = format!("{suite_program} {}", suite_args.join(" "));
    // A command that really completed, so the citation itself is valid and any
    // refusal can only be about the tests.
    let (quick_program, quick_args) = if cfg!(windows) {
        ("cmd", vec!["/c", "exit 0"])
    } else {
        ("sh", vec!["-c", "exit 0"])
    };
    let quick_label = format!("{quick_program} {}", quick_args.join(" "));
    let h = harness(vec![
        tool_call(
            "w1",
            "write_file",
            serde_json::json!({"path": script_name, "content": script}),
        ),
        tool_call(
            "b1",
            "run_command",
            serde_json::json!({
                "program": suite_program,
                "args": suite_args,
                "background": true,
                "reason": "run the suite the user required"
            }),
        ),
        tool_call(
            "q1",
            "run_command",
            serde_json::json!({
                "program": quick_program,
                "args": quick_args,
                "reason": "a completed command the completion may cite"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "the suite is running",
                "verification": {"status": "passed", "detail": quick_label},
                "requirements": [{
                    "requirement": "tests",
                    "status": "done",
                    "evidence": format!("{suite_label} started")
                }]
            }),
        ),
        tool_call(
            "g2",
            "update_goal",
            serde_json::json!({
                "status": "blocked",
                "summary": "the suite's result is not in yet"
            }),
        ),
    ])
    .await;
    let s = spec(&h, "跑通全部测试并确认通过。");
    let session = h.engine.create_task(&s).await.unwrap();
    let mut refusals = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| {
                if let leveler_engine::EngineEvent::GoalIntercepted { kind, detail } = event {
                    refusals.push((kind, detail));
                }
            },
            CancellationToken::new(),
        )
        .await
        .expect("run should settle");

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Blocked,
        "a suite that only started cannot close the goal: {report:?}"
    );
    assert!(
        refusals.iter().any(|(kind, detail)| {
            kind == "requirements_refused" && detail.contains("no test command ran")
        }),
        "the gate must say no test command RAN, not accept the start: {refusals:?}"
    );
}

/// GR-02: a citation of a command that only STARTED is refused for the true
/// reason. The real DevOrder run was told `make up` "never ran in this task"
/// while its process was alive as `bg-1`; the model then re-ran a suite it had
/// already run, because the feedback named a problem it did not have. The
/// refusal must instead name the task whose result is missing.
#[tokio::test]
async fn a_citation_of_a_started_service_names_the_task_whose_result_is_missing() {
    let (program, args) = leveler_test_support::sleep_command(300);
    let label = format!("{program} {}", args.join(" "));
    let h = harness(vec![
        tool_call(
            "b1",
            "run_command",
            serde_json::json!({
                "program": program,
                "args": args,
                "background": true,
                "reason": "start the local service"
            }),
        ),
        tool_call(
            "g1",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "the service is up",
                "verification": {"status": "passed", "detail": label},
            }),
        ),
        tool_call(
            "g2",
            "update_goal",
            serde_json::json!({
                "status": "blocked",
                "summary": "the service has no exit status to cite"
            }),
        ),
    ])
    .await;
    let s = spec(&h, "启动本地服务并确认可用。");
    let session = h.engine.create_task(&s).await.unwrap();
    let mut refusals = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| {
                if let leveler_engine::EngineEvent::GoalIntercepted { kind, detail } = event {
                    refusals.push((kind, detail));
                }
            },
            CancellationToken::new(),
        )
        .await
        .expect("run should settle");

    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Blocked,
        "a started service is not a passing check: {report:?}"
    );
    let detail = refusals
        .iter()
        .find(|(kind, _)| kind == "verification_refused")
        .map(|(_, detail)| detail)
        .unwrap_or_else(|| panic!("the citation must be refused: {refusals:?}"));
    assert!(
        detail.contains("only STARTED"),
        "the refusal must say the command started: {detail}"
    );
    assert!(
        detail.contains("bg-1"),
        "the refusal must name the task to wait on: {detail}"
    );
    assert!(
        !detail.contains("never ran"),
        "the command did run; only its result is missing: {detail}"
    );
}

/// LT-08: a goal that SETTLES must reap its goal-scoped services even when the
/// session's terminal state still looks resumable.
///
/// Reproduced by the mock dogfood (`steer-match`): the model started a silent
/// worker, cited a command that only STARTED three times, the gate's recovery
/// bound stopped the turn as `incomplete`, and the goal SETTLED — no window can
/// follow it — while the worker kept running. The cleanup gate read the draft
/// session state ("execute") instead of the goal's own settle decision.
#[tokio::test]
async fn a_settled_goal_reaps_its_services_even_when_the_session_looks_resumable() {
    let (program, args) = leveler_test_support::sleep_command(300);
    let label = format!("{program} {}", args.join(" "));
    // Three identical citations of a start: verification refusals, bounded by
    // the recovery budget, stop the turn without executing anything.
    let cite_the_start = || {
        tool_call(
            "g",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "the service is up",
                "verification": {"status": "passed", "detail": label},
            }),
        )
    };
    let h = harness(vec![
        tool_call(
            "b1",
            "run_command",
            serde_json::json!({
                "program": program,
                "args": args,
                "background": true,
            }),
        ),
        cite_the_start(),
        cite_the_start(),
        cite_the_start(),
    ])
    .await;
    let s = spec(&h, "启动本地服务并确认可用。");
    let session = h.engine.create_task(&s).await.unwrap();
    let report = h
        .engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await;

    assert!(
        matches!(&report, Ok(report) if report.outcome == leveler_engine::TaskOutcome::Failed),
        "the recovery bound must stop the turn: {report:?}"
    );
    let task = leveler_storage::TaskStore::task_for_session(&h.db, &session)
        .await
        .unwrap()
        .unwrap();
    let goals = leveler_storage::GoalStore::for_task(&h.db, &task)
        .await
        .unwrap();
    assert!(
        goals
            .iter()
            .all(|goal| goal.state == leveler_storage::GoalState::Settled),
        "a failure nothing can re-enter must settle the goal: {goals:?}"
    );
    // Settled means no window can follow: the service must not outlive it.
    assert_terminal_within(&h.registry, "bg-1", "settled goal").await;
}

#[tokio::test]
async fn goal_completion_does_not_reap_background_work_it_did_not_start() {
    let h = harness(vec![
        spawn_sleep_server(),
        tool_call(
            "closeout",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "done"}),
        ),
    ])
    .await;
    let unrelated = h
        .registry
        .spawn_owned(sleep_request(h.dir.path()), None, Some(SESSION_SCOPE))
        .await
        .expect("existing session work");
    let s = spec(&h, "start a service and finish");
    let session = h.engine.create_task(&s).await.unwrap();
    h.engine
        .run(&session, &s, &mut |_| {}, CancellationToken::new())
        .await
        .expect("the goal settles");

    assert_terminal_within(&h.registry, "bg-2", "this goal's task").await;
    let surviving = h.registry.get(&unrelated).await.unwrap();
    assert_eq!(surviving.status, BackgroundTaskStatus::Running);
    h.registry.kill(&unrelated).await.unwrap();
}

/// A task's START must be durable and structured, not prose in the tool
/// output. After a crash the registry that held the task is gone; only a
/// recorded start can tell a reconnecting client that the task existed, which
/// call started it, what it ran, and which lifetime it was promised.
#[tokio::test]
async fn a_background_start_is_a_durable_structured_fact() {
    let h = harness(vec![spawn_sleep_server(), text("server started; done")]).await;
    let s = spec(&h, "start the dev server and report");
    let session = h.engine.create_task(&s).await.unwrap();
    h.engine
        .chat(
            &session,
            &s,
            vec![ContentPart::Text {
                text: "start the dev server".into(),
            }],
            &mut |_| {},
            CancellationToken::new(),
        )
        .await
        .expect("the turn completes");

    let events = leveler_storage::EventRepository::new(&h.db)
        .load(&session)
        .await
        .unwrap();
    let starts: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == "background_task_started")
        .collect();
    assert_eq!(
        starts.len(),
        1,
        "exactly one durable start fact per spawned task"
    );
    let payload: serde_json::Value = serde_json::from_str(&starts[0].payload).unwrap();
    let fact = &payload["payload"];
    let (expected_program, expected_args) = leveler_test_support::sleep_command(300);
    assert_eq!(fact["call_id"], "bg1");
    assert_eq!(fact["task_id"], "bg-1");
    assert_eq!(fact["program"], expected_program);
    assert_eq!(fact["args"], serde_json::json!(expected_args));
    assert_eq!(fact["lifetime"], "goal");
}
