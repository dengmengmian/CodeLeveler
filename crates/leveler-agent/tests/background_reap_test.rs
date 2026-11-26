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
    /// When set, every response after the first waits for the session's
    /// background tasks to reach a terminal state before it is produced.
    ///
    /// A real model takes time to write its status text, which gives the
    /// registry driver time to observe a process exit before the next request
    /// admission. A zero-latency scripted model does not, so a test that needs
    /// an auto-delivered result would otherwise race the reaper. The wait is a
    /// real condition wait on the registry, not a fixed sleep.
    wait_for_background_terminal: Option<Arc<BackgroundTaskRegistry>>,
    generated: Mutex<usize>,
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
        Self::with_background_wait(responses, None)
    }

    fn with_background_wait(
        responses: Vec<ModelResponse>,
        registry: Option<Arc<BackgroundTaskRegistry>>,
    ) -> Self {
        let responses = with_delivery(responses);
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            error_when_exhausted: Mutex::new(ModelError::new(
                leveler_model::ModelErrorKind::Other,
                "no more responses",
            )),
            cancel_before_closeout: Mutex::new(None),
            wait_for_background_terminal: registry,
            generated: Mutex::new(0),
        }
    }

    /// Wait until no task in this session is still active, using the
    /// registry's own completion notification.
    async fn settle_background_tasks(registry: &BackgroundTaskRegistry) -> Result<(), ModelError> {
        loop {
            let active = registry.active_snapshots_for_scope(SESSION_SCOPE).await;
            let Some(task) = active.into_iter().next() else {
                return Ok(());
            };
            registry
                .wait(
                    &task.id,
                    Some(Duration::from_secs(30)),
                    &CancellationToken::new(),
                )
                .await
                .map_err(|error| {
                    ModelError::new(
                        leveler_model::ModelErrorKind::Other,
                        format!("background task {} did not settle: {error}", task.id),
                    )
                })?;
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
        let index = {
            let mut generated = self.generated.lock().unwrap();
            let index = *generated;
            *generated += 1;
            index
        };
        if index > 0
            && let Some(registry) = self.wait_for_background_terminal.as_ref()
        {
            Self::settle_background_tasks(registry).await?;
        }
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

/// One assistant message with several calls, processed in order. A terminal
/// lifecycle read and a completion in the SAME batch must still account the
/// task's write before the completion is judged.
#[cfg(unix)]
fn tool_calls(calls: &[(&str, &str, serde_json::Value)]) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: calls
                .iter()
                .map(|(id, name, args)| ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new(*id),
                        name: name.to_string(),
                        arguments: args.clone(),
                    },
                })
                .collect(),
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
    /// The runtime home this harness resolves. A background spawn creates its
    /// private sandbox scratch here, and the owner refuses a home inside the
    /// writable workspace, so it must be a sibling of `dir`, not a child.
    _home: tempfile::TempDir,
    registry: Arc<BackgroundTaskRegistry>,
    runtime: Arc<MockRuntime>,
}

async fn harness(responses: Vec<ModelResponse>) -> Harness {
    harness_with(responses, false).await
}

/// The same harness, with a scripted model whose later responses wait for the
/// session's background tasks to finish. A test about auto-delivery needs the
/// exit to be observable by the next request admission; the zero-latency mock
/// otherwise races the registry driver.
async fn harness_waiting_for_background(responses: Vec<ModelResponse>) -> Harness {
    harness_with(responses, true).await
}

async fn harness_with(responses: Vec<ModelResponse>, wait_for_background: bool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    // Tests never initialize the global env snapshot, so the registry default
    // (empty env → no resolvable LevelerHome) would fail sandbox scratch prep.
    // The home must be this test's own temp directory, outside the workspace:
    // a background spawn creates its private sandbox scratch under LevelerHome,
    // and inheriting the developer's `$HOME` makes the test depend on whether
    // that directory is writable at all (a sandboxed runner denies it).
    let home = tempfile::tempdir().unwrap();
    let mut environment_vars: Vec<(std::ffi::OsString, std::ffi::OsString)> =
        std::env::vars_os().collect();
    environment_vars.retain(|(key, _)| key != "LEVELER_HOME");
    environment_vars.push((
        std::ffi::OsString::from("LEVELER_HOME"),
        home.path().as_os_str().to_os_string(),
    ));
    let environment = Arc::new(leveler_core::EnvSnapshot::new(
        environment_vars,
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    ));
    let registry = Arc::new(BackgroundTaskRegistry::with_environment(
        environment.clone(),
    ));
    let tool_context =
        ToolContext::with_environment(workspace, PermissionProfile::Assisted, environment.clone())
            .trust_all_definitions_for_tests()
            .with_session_scope(SESSION_SCOPE);
    // ONE registry: the tools that start and manage background tasks, and the
    // engine that reaps them at terminal settlement, must be looking at the
    // same one — that is the whole subject of this file.
    let capabilities = leveler_tools::Capabilities::in_process(environment)
        .with_background_tasks(registry.clone());
    let tool_registry =
        leveler_tools::model_surface(leveler_tools::CapabilityPacks::ALL, &capabilities);
    let runtime = Arc::new(if wait_for_background {
        MockRuntime::with_background_wait(responses, Some(registry.clone()))
    } else {
        MockRuntime::new(responses)
    });
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
        turn_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    Harness {
        engine,
        db,
        dir,
        _home: home,
        registry,
        runtime,
    }
}

/// A background command's terminal result is a verification fact even when
/// the model never polls it. Auto-delivery must register the same execution
/// the active `wait_task` path records, so the first citation of the command
/// can close the goal instead of being refused as a command that never ran.
///
/// The audit session ran `go test ./... -count=1` in the background, let its
/// exit-0 notice arrive on its own, then cited it in `update_goal`; the gate
/// said the command "never ran in this task" because only the `wait_task`
/// path fed the ledger.
#[tokio::test]
async fn auto_delivered_background_result_is_citable_without_a_rerun() {
    // The fixture must terminate successfully under the sandbox on every host
    // and on every runner: `echo` is what the runtime's own confined-background
    // canary uses. A zero-second sleep is not that guarantee — on Windows it is
    // `ping`, whose exit under the runner's confined background path is not
    // part of any contract, while this gate only accepts a run that exited 0.
    let (program, args) = leveler_test_support::echo_command("background-check");
    let citation = format!("{} {} → 退出码 0", program, args.join(" "));
    // The mock waits for the real task exit before its next response, so the
    // auto-delivery admission — not a sleep in the test — decides when the
    // result becomes citable.
    let h = harness_waiting_for_background(vec![
        tool_call(
            "bg-auto",
            "run_command",
            serde_json::json!({
                "program": program,
                "args": args,
                "background": true
            }),
        ),
        text("后台检查已启动，等待它结束。"),
        text("仍在等待后台检查结束。"),
        tool_call(
            "resolve",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "后台检查已退出，退出码 0",
                "verification": { "status": "passed", "detail": citation },
                "deliver_answer": true
            }),
        ),
    ])
    .await;
    let s = spec(&h, "运行检查并报告结果");
    let session = h.engine.create_task(&s).await.unwrap();

    let mut intercepted = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| {
                if let leveler_engine::EngineEvent::GoalIntercepted { kind, detail } = event {
                    intercepted.push(format!("{kind}: {detail}"));
                }
            },
            CancellationToken::new(),
        )
        .await
        .expect("the goal should resolve on the auto-delivered result");

    let verification = report.verification.as_ref().unwrap_or_else(|| {
        panic!("a passed citation must produce a verification report; intercepts: {intercepted:?}")
    });
    assert!(
        intercepted.is_empty(),
        "the auto-delivered result must satisfy the gate, not be refused: {intercepted:?}"
    );
    assert_eq!(
        verification.status,
        leveler_lifecycle::VerificationStatus::Unverified,
        "this harness declares no owed requirement, so a matched citation is downgraded to \
         Unverified by the requirement-coverage rule: {verification:?}"
    );
    assert!(
        report
            .completion_warnings
            .iter()
            .any(|warning| warning.contains("no owed requirement")),
        "the downgrade must be the no-requirement rule, not a missing check: {:?}",
        report.completion_warnings
    );
    assert!(
        !report
            .completion_warnings
            .iter()
            .any(|warning| warning.contains("without executable verification")),
        "a matched citation is executable verification: {:?}",
        report.completion_warnings
    );
}

/// One background execution is ONE mutation fact. Reading its terminal result
/// again through `wait_task` and `get_task` must not advance the workspace
/// version a second time, or evidence that really ran on the current tree
/// would expire for no reason.
#[cfg(unix)]
#[tokio::test]
async fn repeated_background_reads_do_not_account_the_same_change_twice() {
    let h = harness_waiting_for_background(vec![
        tool_call(
            "check-before",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        tool_call(
            "bg-write",
            "run_command",
            serde_json::json!({
                "program": "sh",
                "args": ["-c", "printf 'pub fn changed() {}\\n' > src/lib.rs"],
                "background": true
            }),
        ),
        text("后台修改已执行，等待结果送达。"),
        // The pre-change pass is refused once (this also proves the
        // auto-delivered settlement really advanced the version).
        tool_call(
            "stale-complete",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "引用先前的测试通过结果",
                "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                "deliver_answer": false
            }),
        ),
        // A real rerun on the current tree is the new evidence.
        tool_call(
            "check-after",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        // Each of these observes the SAME terminal execution, AFTER the fresh
        // evidence exists. The id is the harness's first local task in a
        // fresh registry.
        tool_call(
            "wait-same",
            "wait_task",
            serde_json::json!({ "task_id": "bg-1" }),
        ),
        tool_call(
            "get-same-1",
            "get_task",
            serde_json::json!({ "task_id": "bg-1" }),
        ),
        tool_call(
            "get-same-2",
            "get_task",
            serde_json::json!({ "task_id": "bg-1" }),
        ),
        tool_call(
            "fresh-complete",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "重新运行测试后完成",
                "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                "deliver_answer": true
            }),
        ),
        text("同一后台执行只记一次变更，重新验证后完成。"),
    ])
    .await;
    std::fs::write(h.dir.path().join("run-tests.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    leveler_test_support::git::init_repo(h.dir.path());
    leveler_test_support::git::run(h.dir.path(), &["add", "-A"]);
    leveler_test_support::git::run(h.dir.path(), &["commit", "-qm", "seed"]);

    let s = TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "后台修改一次，读取多次，最后用当前工作区的测试证据完成".into(),
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::bounded(12),
            limits: StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(h.dir.path().to_path_buf()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    };
    let session = h.engine.create_task(&s).await.unwrap();

    let mut intercepted = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| {
                if let leveler_engine::EngineEvent::GoalIntercepted { kind, detail } = event {
                    intercepted.push(format!("{kind}: {detail}"));
                }
            },
            CancellationToken::new(),
        )
        .await
        .expect("the goal should resolve after the current-version rerun");

    let refusals: Vec<&String> = intercepted
        .iter()
        .filter(|line| line.starts_with("verification_refused:"))
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "only the pre-change pass may be refused; repeated reads must not expire the \
         current-version rerun: {intercepted:?}"
    );
    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Completed,
        "the fresh rerun must close the goal: {report:?}"
    );
    let verification = report
        .verification
        .as_ref()
        .unwrap_or_else(|| panic!("the rerun must produce evidence: {intercepted:?}"));
    assert!(
        verification
            .notes
            .iter()
            .any(|note| note.contains("no owed requirement")),
        "the accepted citation is downgraded only by the no-requirement rule: {verification:?}"
    );
}

/// A failed background command that already wrote the source is still a real
/// workspace mutation: its failure is not a reason to keep a stale pass
/// current.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_background_write_expires_the_verification_it_followed() {
    let h = harness_waiting_for_background(vec![
        tool_call(
            "check-before",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        tool_call(
            "bg-failing-write",
            "run_command",
            serde_json::json!({
                "program": "sh",
                "args": [
                    "-c",
                    "printf 'pub fn changed() {}\\n' > src/lib.rs; exit 3"
                ],
                "background": true
            }),
        ),
        text("后台命令失败，等待结果送达。"),
        tool_call(
            "stale-complete",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "引用先前的测试通过结果",
                "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                "deliver_answer": false
            }),
        ),
        tool_call(
            "check-after",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        tool_call(
            "fresh-complete",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "重新运行测试后完成",
                "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                "deliver_answer": true
            }),
        ),
        text("后台命令失败但确实修改了源码，重新验证后完成。"),
    ])
    .await;
    std::fs::write(h.dir.path().join("run-tests.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    leveler_test_support::git::init_repo(h.dir.path());
    leveler_test_support::git::run(h.dir.path(), &["add", "-A"]);
    leveler_test_support::git::run(h.dir.path(), &["commit", "-qm", "seed"]);

    let s = TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "后台失败也要按真实文件修改失效旧证据".into(),
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::bounded(10),
            limits: StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(h.dir.path().to_path_buf()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    };
    let session = h.engine.create_task(&s).await.unwrap();

    let mut intercepted = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| {
                if let leveler_engine::EngineEvent::GoalIntercepted { kind, detail } = event {
                    intercepted.push(format!("{kind}: {detail}"));
                }
            },
            CancellationToken::new(),
        )
        .await
        .expect("the goal should resolve after the current-version rerun");

    assert!(
        std::fs::read_to_string(h.dir.path().join("src/lib.rs"))
            .unwrap()
            .contains("changed"),
        "the failed background command must still have written the source"
    );
    let refusals: Vec<&String> = intercepted
        .iter()
        .filter(|line| line.starts_with("verification_refused:"))
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "the pre-change pass must be refused exactly once: {intercepted:?}"
    );
    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Completed,
        "the rerun must permit completion: {report:?}"
    );
}

/// A write-capable task that has ALREADY changed the tree but has not exited
/// yet has no settlement. A completion must observe the tree against the
/// task's own launch baseline rather than accept an older pass as current.
/// The observation is the real diff, not an assumption that every running
/// task wrote something: the same task with no change leaves evidence valid.
#[cfg(unix)]
#[tokio::test]
async fn a_running_background_write_expires_the_verification_it_followed() {
    let h = harness(vec![
        tool_call(
            "check-before",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        tool_call(
            "bg-write-running",
            "run_command",
            serde_json::json!({
                "program": "sh",
                "args": [
                    "-c",
                    "printf 'pub fn changed() {}\\n' > src/lib.rs; echo READY; exec sleep 60"
                ],
                "background": true
            }),
        ),
        // A readiness wait returns while the task keeps running: this is a
        // deterministic barrier on the task's own output, not a sleep.
        tool_call(
            "wait-ready",
            "wait_task",
            serde_json::json!({
                "task_id": "bg-1",
                "until": "match",
                "pattern": "READY",
                "timeout_seconds": 10
            }),
        ),
        tool_call(
            "stale-complete",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "引用先前的测试通过结果",
                "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                "deliver_answer": false
            }),
        ),
        tool_call(
            "check-after",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        tool_call(
            "fresh-complete",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "重新运行测试后完成",
                "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                "deliver_answer": true
            }),
        ),
        text("后台任务仍在运行但已经改动源码，重新验证后完成。"),
    ])
    .await;
    std::fs::write(h.dir.path().join("run-tests.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    leveler_test_support::git::init_repo(h.dir.path());
    leveler_test_support::git::run(h.dir.path(), &["add", "-A"]);
    leveler_test_support::git::run(h.dir.path(), &["commit", "-qm", "seed"]);

    let s = TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "运行中的后台任务已经修改源码时，旧测试证据不能直接完成".into(),
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::bounded(10),
            limits: StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(h.dir.path().to_path_buf()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    };
    let session = h.engine.create_task(&s).await.unwrap();

    let mut intercepted = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| {
                if let leveler_engine::EngineEvent::GoalIntercepted { kind, detail } = event {
                    intercepted.push(format!("{kind}: {detail}"));
                }
            },
            CancellationToken::new(),
        )
        .await
        .expect("the goal should resolve after the current-version rerun");

    assert!(
        std::fs::read_to_string(h.dir.path().join("src/lib.rs"))
            .unwrap()
            .contains("changed"),
        "the running background process must already have written the source"
    );
    let refusals: Vec<&String> = intercepted
        .iter()
        .filter(|line| line.starts_with("verification_refused:"))
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "the pre-change pass must be refused exactly once: {intercepted:?}"
    );
    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Completed,
        "the goal may complete while the task still runs, once the current tree is verified: \
         {report:?}"
    );

    // The goal terminal reaps its Goal-lifetime task; the test's own registry
    // owns it, so make sure nothing is left for the next test.
    h.registry.kill_scope(SESSION_SCOPE).await;
}

/// The converse of the running-write case: an active task that has changed
/// nothing must not invalidate a current pass. A running task is not itself a
/// workspace mutation.
#[cfg(unix)]
#[tokio::test]
async fn a_running_background_read_does_not_expire_current_evidence() {
    let h = harness(vec![
        tool_call(
            "check-before",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        tool_call(
            "bg-idle-running",
            "run_command",
            serde_json::json!({
                "program": "sh",
                "args": ["-c", "echo READY; exec sleep 60"],
                "background": true
            }),
        ),
        tool_call(
            "wait-ready",
            "wait_task",
            serde_json::json!({
                "task_id": "bg-1",
                "until": "match",
                "pattern": "READY",
                "timeout_seconds": 10
            }),
        ),
        tool_call(
            "fresh-complete",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "后台只读任务未改动工作区，测试证据保持有效",
                "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                "deliver_answer": true
            }),
        ),
        text("后台任务仍在运行但没有修改工作区，测试证据保持有效。"),
    ])
    .await;
    std::fs::write(h.dir.path().join("run-tests.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    leveler_test_support::git::init_repo(h.dir.path());
    leveler_test_support::git::run(h.dir.path(), &["add", "-A"]);
    leveler_test_support::git::run(h.dir.path(), &["commit", "-qm", "seed"]);

    let s = TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "后台只读任务运行期间，当前测试证据仍然有效".into(),
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::bounded(8),
            limits: StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(h.dir.path().to_path_buf()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    };
    let session = h.engine.create_task(&s).await.unwrap();

    let mut intercepted = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| {
                if let leveler_engine::EngineEvent::GoalIntercepted { kind, detail } = event {
                    intercepted.push(format!("{kind}: {detail}"));
                }
            },
            CancellationToken::new(),
        )
        .await
        .expect("the goal should resolve on the current pass");

    let refusals: Vec<&String> = intercepted
        .iter()
        .filter(|line| line.starts_with("verification_refused:"))
        .collect();
    assert!(
        refusals.is_empty(),
        "a running task that changed nothing must not expire current evidence: {intercepted:?}"
    );
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Completed);
    let verification = report.verification.as_ref().expect("the cited pass");
    assert!(
        verification
            .notes
            .iter()
            .all(|note| !note.contains("expired")),
        "no freshness boundary may be invented: {verification:?}"
    );

    h.registry.kill_scope(SESSION_SCOPE).await;
}

/// A stopped task that already wrote the workspace is still a real mutation.
/// The cancel is not a reason to ignore what the process changed, and the
/// later auto-delivery of the same terminal fact must not count it again.
#[cfg(unix)]
#[tokio::test]
async fn a_stopped_background_write_expires_the_verification_it_followed() {
    let h = harness(vec![
        tool_call(
            "check-before",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        tool_call(
            "bg-write-stoppable",
            "run_command",
            serde_json::json!({
                "program": "sh",
                "args": [
                    "-c",
                    "printf 'pub fn changed() {}\\n' > src/lib.rs; echo READY; exec sleep 60"
                ],
                "background": true
            }),
        ),
        tool_call(
            "wait-ready",
            "wait_task",
            serde_json::json!({
                "task_id": "bg-1",
                "until": "match",
                "pattern": "READY",
                "timeout_seconds": 10
            }),
        ),
        tool_call(
            "stop-task",
            "kill_task",
            serde_json::json!({ "task_id": "bg-1" }),
        ),
        tool_call(
            "wait-stopped",
            "wait_task",
            serde_json::json!({ "task_id": "bg-1", "timeout_seconds": 10 }),
        ),
        tool_call(
            "stale-complete",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "引用先前的测试通过结果",
                "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                "deliver_answer": false
            }),
        ),
        tool_call(
            "check-after",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        tool_call(
            "fresh-complete",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "重新运行测试后完成",
                "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                "deliver_answer": true
            }),
        ),
        text("被停止的后台任务已经修改源码，旧证据失效，重新验证后完成。"),
    ])
    .await;
    std::fs::write(h.dir.path().join("run-tests.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    leveler_test_support::git::init_repo(h.dir.path());
    leveler_test_support::git::run(h.dir.path(), &["add", "-A"]);
    leveler_test_support::git::run(h.dir.path(), &["commit", "-qm", "seed"]);

    let s = TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "被停止的后台任务已经修改源码时，旧测试证据不能完成".into(),
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::bounded(12),
            limits: StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(h.dir.path().to_path_buf()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    };
    let session = h.engine.create_task(&s).await.unwrap();

    let mut intercepted = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| {
                if let leveler_engine::EngineEvent::GoalIntercepted { kind, detail } = event {
                    intercepted.push(format!("{kind}: {detail}"));
                }
            },
            CancellationToken::new(),
        )
        .await
        .expect("the goal should resolve after the current-version rerun");

    assert!(
        std::fs::read_to_string(h.dir.path().join("src/lib.rs"))
            .unwrap()
            .contains("changed"),
        "the stopped process must already have written the source"
    );
    let refusals: Vec<&String> = intercepted
        .iter()
        .filter(|line| line.starts_with("verification_refused:"))
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "the pre-change pass must be refused exactly once: {intercepted:?}"
    );
    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Completed,
        "the rerun must permit completion: {report:?}"
    );
}

/// A terminal wait and a completion can arrive in ONE assistant batch. The
/// wait's settlement must advance the version before the completion is judged,
/// even though no later request admission could deliver it.
#[cfg(unix)]
#[tokio::test]
async fn a_same_batch_terminal_wait_accounts_before_the_completion() {
    let h = harness(vec![
        tool_call(
            "check-before",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        tool_call(
            "bg-write-batch",
            "run_command",
            serde_json::json!({
                "program": "sh",
                "args": [
                    "-c",
                    "printf 'pub fn changed() {}\\n' > src/lib.rs; echo READY; exec sleep 60"
                ],
                "background": true
            }),
        ),
        tool_call(
            "wait-ready",
            "wait_task",
            serde_json::json!({
                "task_id": "bg-1",
                "until": "match",
                "pattern": "READY",
                "timeout_seconds": 10
            }),
        ),
        tool_calls(&[
            (
                "stop-in-batch",
                "kill_task",
                serde_json::json!({ "task_id": "bg-1" }),
            ),
            (
                "wait-stopped-in-batch",
                "wait_task",
                serde_json::json!({ "task_id": "bg-1", "timeout_seconds": 10 }),
            ),
            (
                "stale-complete",
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "引用先前的测试通过结果",
                    "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                    "deliver_answer": false
                }),
            ),
        ]),
        tool_call(
            "check-after",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        tool_call(
            "fresh-complete",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "重新运行测试后完成",
                "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                "deliver_answer": true
            }),
        ),
        text("同一批次内先结算后台写入，再判定完成。"),
    ])
    .await;
    std::fs::write(h.dir.path().join("run-tests.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    leveler_test_support::git::init_repo(h.dir.path());
    leveler_test_support::git::run(h.dir.path(), &["add", "-A"]);
    leveler_test_support::git::run(h.dir.path(), &["commit", "-qm", "seed"]);

    let s = TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "同一批次内停止后台任务并完成时，旧测试证据必须失效".into(),
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::bounded(10),
            limits: StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(h.dir.path().to_path_buf()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    };
    let session = h.engine.create_task(&s).await.unwrap();

    let mut intercepted = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| {
                if let leveler_engine::EngineEvent::GoalIntercepted { kind, detail } = event {
                    intercepted.push(format!("{kind}: {detail}"));
                }
            },
            CancellationToken::new(),
        )
        .await
        .expect("the goal should resolve after the current-version rerun");

    let refusals: Vec<&String> = intercepted
        .iter()
        .filter(|line| line.starts_with("verification_refused:"))
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "the same-batch completion must see the terminal settlement: {intercepted:?}"
    );
    assert_eq!(report.stop_reason, leveler_agent::StopReason::Completed);
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

/// The auto-delivered notice and an active `wait_task` read are two
/// observations of ONE background execution. The workspace change that
/// execution really made must advance the mutation version in both routes —
/// not only when the model polls. Otherwise a task that writes the source
/// after a passing check can leave the stale pass looking current.
#[cfg(unix)]
#[tokio::test]
async fn an_auto_delivered_background_write_expires_the_verification_it_followed() {
    let h = harness_waiting_for_background(vec![
        // 1. A real foreground check passes on the current tree.
        tool_call(
            "check-before",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        // 2. A real background command changes the source after that pass.
        tool_call(
            "bg-write",
            "run_command",
            serde_json::json!({
                "program": "sh",
                "args": ["-c", "printf 'pub fn changed() {}\\n' > src/lib.rs"],
                "background": true
            }),
        ),
        // The mock waits for the real task to exit before producing this, so
        // the next admission — not a sleep in the test — delivers the result.
        text("后台修改已执行，等待结果送达。"),
        // 3. Citing the pre-change pass must not close the goal.
        tool_call(
            "stale-complete",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "引用先前的测试通过结果",
                "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                "deliver_answer": false
            }),
        ),
        // 4. Re-running on the current tree is the honest repair.
        tool_call(
            "check-after",
            "run_command",
            serde_json::json!({ "program": "sh", "args": ["run-tests.sh"] }),
        ),
        tool_call(
            "fresh-complete",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "重新运行测试后完成",
                "verification": { "status": "passed", "detail": "sh run-tests.sh" },
                "deliver_answer": true
            }),
        ),
        text("后台写入已计入工作区版本，重新验证后完成。"),
    ])
    .await;
    std::fs::write(h.dir.path().join("run-tests.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    // The gateway is a git workspace, which is what gives the background task
    // a mutation baseline and lets its settlement report the files it changed.
    leveler_test_support::git::init_repo(h.dir.path());
    leveler_test_support::git::run(h.dir.path(), &["add", "-A"]);
    leveler_test_support::git::run(h.dir.path(), &["commit", "-qm", "seed"]);

    let s = TaskSpec {
        runtime: leveler_agent::coding::RuntimeTaskSpec {
            goal: "先验证，再让后台任务修改源码，最终用当前工作区的有效测试证据完成".into(),
            kind: ExecutionKind::Direct,
            continuation: ContinuationPolicy::bounded(10),
            limits: StepLimits::default(),
        },
        coding: leveler_agent::coding::CodingTaskSpec {
            repository: Some(h.dir.path().to_path_buf()),
            mode: PermissionProfile::Assisted,
            sandbox: false,
        },
    };
    let session = h.engine.create_task(&s).await.unwrap();

    let mut intercepted = Vec::new();
    let report = h
        .engine
        .run(
            &session,
            &s,
            &mut |event| {
                if let leveler_engine::EngineEvent::GoalIntercepted { kind, detail } = event {
                    intercepted.push(format!("{kind}: {detail}"));
                }
            },
            CancellationToken::new(),
        )
        .await
        .expect("the goal should resolve after the current-version rerun");

    assert!(
        std::fs::read_to_string(h.dir.path().join("src/lib.rs"))
            .unwrap()
            .contains("changed"),
        "the background process must really have written the source"
    );
    let refusals: Vec<&String> = intercepted
        .iter()
        .filter(|line| line.starts_with("verification_refused:"))
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "the pre-change pass must be refused exactly once: {intercepted:?}"
    );
    assert!(
        refusals[0].contains("sh run-tests.sh")
            && (refusals[0].contains("verification expired")
                || refusals[0].contains("before the most recent file change")),
        "the refusal must name the stale command and its freshness: {}",
        refusals[0]
    );
    let verification = report
        .verification
        .as_ref()
        .unwrap_or_else(|| panic!("the rerun must produce evidence: {intercepted:?}"));
    // This harness declares no owed requirement, so the requirement-coverage
    // rule downgrades a matched citation (the same rule the existing
    // auto-delivery test pins). The freshness contract is the refusal count:
    // only the pre-change pass was refused, and the rerun was accepted.
    assert_eq!(
        verification.status,
        leveler_lifecycle::VerificationStatus::Unverified,
        "the accepted rerun is downgraded only by the no-requirement rule: {verification:?}"
    );
    assert!(
        verification
            .notes
            .iter()
            .any(|note| note.contains("no owed requirement")),
        "the downgrade must be the no-requirement rule, not a missing check: {verification:?}"
    );
    assert_eq!(
        report.stop_reason,
        leveler_agent::StopReason::Completed,
        "the goal must complete on the current-version rerun: {report:?}"
    );
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
            permission_contract: 2,
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
/// of a pass: a completion that cites the started command is refused for the
/// real missing fact — no result yet — and the refusal names the task to wait
/// on. The model's honest `blocked` declaration is still the terminal.
/// `execution_status` is the mechanical line between "started" and "ran to its
/// own answer", and no command name is classified either way.
#[tokio::test]
async fn a_background_test_start_is_not_evidence_of_a_pass() {
    // A long-lived process the fixture itself owns. A shell spelling
    // (`ping`/`sleep`) made this test depend on process startup succeeding
    // while the host was loaded; the helper is the test binary, so the only
    // failure left is the product decision under test.
    let suite_program = std::env::current_exe()
        .expect("the test binary path")
        .to_string_lossy()
        .into_owned();
    let suite_args = vec![
        "--exact".to_string(),
        "background_sleeper_helper".to_string(),
        "--ignored".to_string(),
        "--test-threads=1".to_string(),
    ];
    let suite_label = format!("{suite_program} {}", suite_args.join(" "));
    let h = harness(vec![
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
            "g1",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "the suite is running",
                "verification": {"status": "passed", "detail": suite_label}
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
            kind == "verification_refused"
                && detail.contains("only STARTED")
                && detail.contains("wait_task")
        }),
        "the gate must say the command only STARTED, not accept the start: {refusals:?}"
    );
}

/// The long-lived process the background-start test starts. It is ignored in
/// ordinary runs and selected with `--exact` by the test binary itself, so no
/// external shell, network path or package manager can fail under load.
#[test]
#[ignore = "helper process for a_background_test_start_is_not_evidence_of_a_pass"]
fn background_sleeper_helper() {
    std::thread::sleep(Duration::from_secs(300));
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
