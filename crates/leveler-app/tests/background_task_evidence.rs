//! A background command's REAL result must become a durable engine fact.
//!
//! The reported defect: a command that runs longer than 15 seconds is handed to
//! the background, its call reports only a START, and when it finally succeeds
//! nothing records that result. `update_goal(complete)` then refuses a citation
//! of a test that really ran and really passed, and the same is true after a
//! resume. These tests drive the real registry, the real session event log and
//! the real runtime bridge — no mocked lifecycle.
//!
//! Covered here:
//! - a natural zero exit is persisted with the STARTING CALL's id and the argv
//!   the registry actually ran,
//! - a natural failure is persisted as a failure (never as a pass),
//! - a stopped task is not persisted at all (a cancel is not a result),
//! - the fact does not depend on the model calling `wait_task`,
//! - a task started outside the runtime (no recorded call) writes nothing,
//! - a reconnecting snapshot separates running, completed, failed, cancelled,
//!   lost, unknown and id-conflict states instead of inventing an outcome.
#![cfg(unix)]

use std::sync::Arc;

use leveler_app::runtime_boot::RuntimeBootLease;
use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    InteractiveRuntimeClient, PermissionProfile as WirePermissionProfile, RuntimeEvent,
    UiBackgroundTaskRecord, UiBackgroundTaskState, UiSessionSnapshot,
};
use leveler_execution::PermissionProfile;
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::EventStore as _;

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

/// A model endpoint that accepts a connection and never answers. The lifecycle
/// under test is independent of the model; it only needs a session that exists.
async fn hold_open_model_endpoint() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut stream = stream;
                let (_reader, writer) = stream.split();
                // Hold the connection open until the test process ends.
                let _ = writer;
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

fn write_config(root: &std::path::Path, base_url: &str) {
    isolate_global_config();
    std::fs::create_dir_all(root.join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.join("configs/models")).unwrap();
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("configs/models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities:
  streaming: true
  tool_calling: true
  parallel_tool_calls: false
  structured_output: true
  reasoning: false
  vision: false
limits:
  context_window: 131072
  reliable_context: 65536
  max_output_tokens: 1024
  max_tool_schema_bytes: 8192
  max_parallel_tool_calls: 1
compatibility:
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#,
    )
    .unwrap();
}

struct Fixture {
    app: Arc<Application>,
    runtime: Arc<InProcessRuntimeClient>,
    session: leveler_core::SessionId,
    repo: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let base_url = hold_open_model_endpoint().await;
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), &base_url);
    // The repository is a child of the state root: an Execution Host refuses
    // a state dir inside its own workspace write authority, and these tests
    // start the real host to observe owner loss while its process still runs.
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let layout = Layout::from_parts(
        repo.clone(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let runtime = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        ModelRef::new("mock", "m"),
        PermissionProfile::Assisted,
        false,
    ));
    let session = runtime
        .create_session(CreateSessionRequest {
            request_id: None,
            collaboration: leveler_local_transport::CollaborationMode::Goal,
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "background evidence".into(),
            model: None,
            mode: WirePermissionProfile::Assisted,
        })
        .await
        .expect("create session")
        .session
        .id;
    Fixture {
        app,
        runtime,
        session,
        repo,
        _tmp: tmp,
    }
}

impl Fixture {
    /// Record the tool call that starts a background command, exactly as the
    /// engine does. The call's own `ToolCallFinished` is not needed: a start is
    /// what the model saw.
    async fn record_starting_call(&self, call_id: &str, program: &str, args: &[&str]) {
        let db = self.app.open_database().await.expect("db");
        let event = leveler_engine::EngineEvent::ToolCallStarted {
            call_id: call_id.into(),
            name: "run_command".into(),
            arguments: serde_json::json!({ "program": program, "args": args }).to_string(),
            parallel: false,
            risk: None,
            agent_id: None,
            model_step: Some(1),
        };
        let (event_type, payload) = event.to_row().unwrap();
        db.append(
            &self.session,
            None,
            &event_type,
            &payload,
            leveler_core::now(),
        )
        .await
        .expect("append start event");
    }

    async fn spawn(&self, call_id: Option<&str>, script: &str) -> String {
        self.app
            .background_tasks()
            .spawn_for_writer(
                leveler_execution::ProcessRequest::new(
                    "sh",
                    vec!["-c".into(), script.to_string()],
                    self.repo.clone(),
                ),
                None,
                Some(self.session.as_str()),
                "parent",
                leveler_execution::BackgroundTaskLifetime::Goal,
                call_id.map(str::to_string),
            )
            .await
            .expect("spawn")
    }

    /// Every durable background-result fact in this session's log.
    async fn result_events(&self) -> Vec<leveler_engine::EngineEvent> {
        let db = self.app.open_database().await.expect("db");
        db.load_by_types(&self.session, &["background_task_finished"])
            .await
            .expect("load")
            .into_iter()
            .filter_map(|row| leveler_engine::EngineEvent::from_payload(&row.payload).ok())
            .collect()
    }

    async fn wait_for_events(&self, events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
                .await
                .expect("lifecycle event")
                .expect("stream open");
            if matches!(event, RuntimeEvent::BackgroundTaskExited { .. }) {
                break;
            }
        }
    }
}

/// The long-running success: the result is durable WITHOUT any `wait_task`.
#[tokio::test]
async fn a_finished_background_command_becomes_durable_evidence() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    f.record_starting_call("call-bg-ok", "cargo", &["test"])
        .await;

    let id = f
        .spawn(Some("call-bg-ok"), "printf 'running tests\\n'; exit 0")
        .await;
    f.wait_for_events(&mut events).await;

    let recorded = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let rows = f.result_events().await;
            if !rows.is_empty() {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the durable result is written");

    let (_, payload) = recorded[0].to_row().unwrap();
    assert!(
        payload.contains("[stdout] running tests"),
        "missing durable output: {payload}"
    );

    match &recorded[0] {
        leveler_engine::EngineEvent::BackgroundTaskFinished {
            call_id,
            task_id,
            program,
            args,
            is_error,
            exit_code,
            ..
        } => {
            assert_eq!(call_id, "call-bg-ok", "attributed to the starting call");
            assert_eq!(task_id, &id);
            assert_eq!(program, "sh");
            assert_eq!(args[1], "printf 'running tests\\n'; exit 0");
            assert!(!is_error, "a zero exit is a pass");
            assert_eq!(*exit_code, Some(0));
        }
        other => panic!("unexpected event {other:?}"),
    }
}

/// A terminal fact is never persisted without its start. A foreground call
/// handed to the background can exit before the live start row reaches the
/// log; the terminal writer must restore the start first, with the same call,
/// argv and lifetime, so a replay sees a complete lifecycle.
#[tokio::test]
async fn a_terminal_fact_ensures_its_start_is_durable_first() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    f.record_starting_call("call-bg-pair", "sh", &["-c", "exit 0"])
        .await;
    let id = f.spawn(Some("call-bg-pair"), "exit 0").await;
    f.wait_for_events(&mut events).await;

    let rows = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let db = f.app.open_database().await.expect("db");
            let rows = db
                .load_by_types(
                    &f.session,
                    &["background_task_started", "background_task_finished"],
                )
                .await
                .expect("load");
            if rows
                .iter()
                .any(|row| row.event_type == "background_task_finished")
            {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the durable result is written");

    let starts: Vec<_> = rows
        .iter()
        .filter(|row| row.event_type == "background_task_started")
        .collect();
    assert_eq!(starts.len(), 1, "exactly one start fact: {rows:?}");
    let finished = rows
        .iter()
        .find(|row| row.event_type == "background_task_finished")
        .expect("terminal fact");
    assert!(
        starts[0].sequence < finished.sequence,
        "the start must be durable before the result"
    );
    match leveler_engine::EngineEvent::from_payload(&starts[0].payload).unwrap() {
        leveler_engine::EngineEvent::BackgroundTaskStarted {
            call_id,
            task_id,
            program,
            args,
            lifetime,
        } => {
            assert_eq!(call_id, "call-bg-pair", "attributed to the starting call");
            assert_eq!(task_id, id);
            assert_eq!(program, "sh");
            assert_eq!(args, vec!["-c".to_string(), "exit 0".to_string()]);
            assert_eq!(lifetime, "goal", "the start keeps the spawn lifetime");
        }
        other => panic!("unexpected event {other:?}"),
    }
}

/// A real failure stays a failure: it can never be cited as a pass.
#[tokio::test]
async fn a_failed_background_command_is_recorded_as_a_failure() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    f.record_starting_call("call-bg-fail", "cargo", &["test"])
        .await;
    f.spawn(Some("call-bg-fail"), "exit 3").await;
    f.wait_for_events(&mut events).await;

    let recorded = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let rows = f.result_events().await;
            if !rows.is_empty() {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the durable result is written");
    match &recorded[0] {
        leveler_engine::EngineEvent::BackgroundTaskFinished {
            is_error,
            exit_code,
            ..
        } => {
            assert!(is_error, "exit 3 is a failure");
            assert_eq!(*exit_code, Some(3));
        }
        other => panic!("unexpected event {other:?}"),
    }
}

/// The durable fact distinguishes an explicit stop from a failed exit.
#[tokio::test]
async fn a_stopped_background_command_records_a_cancelled_fact() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    f.record_starting_call("call-bg-stop", "cargo", &["test"])
        .await;
    let id = f.spawn(Some("call-bg-stop"), "sleep 30").await;
    // Wait for the start to be observable, then stop it.
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .expect("start event")
            .expect("stream open");
        if matches!(event, RuntimeEvent::BackgroundTaskStarted { .. }) {
            break;
        }
    }
    f.app.background_tasks().kill(&id).await.expect("kill");
    f.wait_for_events(&mut events).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(event) = f.result_events().await.first() {
                assert!(
                    matches!(
                        event,
                        leveler_engine::EngineEvent::BackgroundTaskFinished {
                            status: Some(leveler_execution::BackgroundTaskStatus::Killed),
                            is_error: true,
                            ..
                        }
                    ),
                    "{event:?}"
                );
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the durable cancellation fact is written");
}

/// A task with no recorded starting call (an internal service, a user shell)
/// has no call to attribute a result to, so nothing is written.
#[tokio::test]
async fn a_task_without_a_starting_call_records_no_result() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    f.spawn(None, "exit 0").await;
    f.wait_for_events(&mut events).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(f.result_events().await.is_empty(), "no call, no evidence");
}

/// A competing fact writer must neither duplicate the receipt nor claim a turn.
#[tokio::test]
async fn background_fact_append_is_fenced_and_idempotent() {
    let f = fixture().await;
    let db = f.app.open_database().await.unwrap();
    let engine = f.app.task_engine(&db).unwrap();
    let owner = engine.acquire_ownership(&f.session).await.unwrap();
    let foreign_boot = leveler_core::BootId::new("different-boot");
    let (_, payload) = leveler_engine::EngineEvent::BackgroundTaskFinished {
        call_id: "call-fenced".into(),
        task_id: "task-fenced".into(),
        goal_id: None,
        program: "sh".into(),
        args: Vec::new(),
        is_error: false,
        exit_code: Some(0),
        status: Some(leveler_execution::BackgroundTaskStatus::Exited),
        output: "from the owning boot".into(),
        output_truncated: false,
    }
    .to_row()
    .unwrap();
    let append = |boot| {
        db.append_background_task_finished(
            &f.session,
            &owner.runtime_id,
            boot,
            "task-fenced",
            &payload,
            leveler_core::now(),
        )
    };

    append(&foreign_boot).await.unwrap();
    assert!(f.result_events().await.is_empty());
    tokio::try_join!(append(&owner.boot_id), append(&owner.boot_id)).unwrap();
    assert_eq!(f.result_events().await.len(), 1);
    engine.release_ownership(&owner).await.unwrap();
    append(&owner.boot_id).await.unwrap();
    assert_eq!(f.result_events().await.len(), 1);
    let next = engine.acquire_ownership(&f.session).await.unwrap();
    assert_eq!(Some(next.owner_epoch), owner.owner_epoch.next());
    engine.release_ownership(&next).await.unwrap();
}

#[tokio::test]
async fn cancelling_an_idle_goal_reaps_its_background_process() {
    let f = fixture().await;
    let db = f.app.open_database().await.unwrap();
    let engine = f.app.task_engine(&db).unwrap();
    let owner = engine.acquire_ownership(&f.session).await.unwrap();
    let goal = engine
        .open_goal(&owner, "finish the running command")
        .await
        .unwrap();
    engine.release_ownership(&owner).await.unwrap();
    let registry = f.app.background_tasks();
    let id = registry
        .spawn_with_origin(
            leveler_execution::ProcessRequest::new(
                "sh",
                vec!["-c".into(), "exec sleep 30".into()],
                f.repo.clone(),
            ),
            None,
            Some(f.session.as_str()),
            "parent",
            leveler_execution::BackgroundTaskLifetime::Goal,
            leveler_execution::BackgroundTaskOrigin {
                call_id: Some("call-idle".into()),
                goal_id: Some(goal),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        registry.get(&id).await.unwrap().status,
        leveler_execution::BackgroundTaskStatus::Running
    );
    assert!(f.app.cancel_task(&f.session).await.unwrap().is_some());
    assert_eq!(
        registry.get(&id).await.unwrap().status,
        leveler_execution::BackgroundTaskStatus::Killed
    );
}

// ---------------------------------------------------------------------------
// Reconnect reconciliation: a snapshot must state what it can prove about a
// task that is no longer in the live registry, and leave the rest open.
// ---------------------------------------------------------------------------

impl Fixture {
    async fn record_background_start(
        &self,
        turn_id: Option<&str>,
        call_id: &str,
        task_id: &str,
        program: &str,
        args: &[&str],
    ) {
        let db = self.app.open_database().await.expect("db");
        let event = leveler_engine::EngineEvent::BackgroundTaskStarted {
            call_id: call_id.into(),
            task_id: task_id.into(),
            program: program.into(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            lifetime: "goal".into(),
        };
        let (event_type, payload) = event.to_row().unwrap();
        let turn = turn_id.map(leveler_core::TurnId::new);
        db.append(
            &self.session,
            turn.as_ref(),
            &event_type,
            &payload,
            leveler_core::now(),
        )
        .await
        .expect("append background start");
    }

    async fn record_background_finish(
        &self,
        call_id: &str,
        task_id: &str,
        status: leveler_execution::BackgroundTaskStatus,
        exit_code: Option<i32>,
    ) {
        let db = self.app.open_database().await.expect("db");
        let event = leveler_engine::EngineEvent::BackgroundTaskFinished {
            call_id: call_id.into(),
            task_id: task_id.into(),
            goal_id: None,
            program: "sh".into(),
            args: vec!["-c".into(), "true".into()],
            is_error: exit_code != Some(0),
            exit_code,
            status: Some(status),
            output: String::new(),
            output_truncated: false,
        };
        let (event_type, payload) = event.to_row().unwrap();
        db.append(
            &self.session,
            None,
            &event_type,
            &payload,
            leveler_core::now(),
        )
        .await
        .expect("append background finish");
    }

    async fn snapshot(&self) -> UiSessionSnapshot {
        self.runtime
            .snapshot(&self.session)
            .await
            .expect("snapshot")
    }

    /// Start a turn owned by a boot that is alive only as long as `lease`.
    async fn turn_under(&self, lease: &RuntimeBootLease) -> leveler_storage::TurnRecord {
        let db = self.app.open_database().await.expect("db");
        let engine = leveler_engine::TaskEngine {
            stores: leveler_storage::EngineStores::from_database(&db),
            runtime_id: self.app.runtime_id().unwrap(),
            boot: leveler_engine::EngineBoot {
                id: lease.id().clone(),
                liveness: Arc::new(leveler_app::runtime_boot::StateDirBootLiveness::new(
                    &self.app.layout.state_dir,
                )),
            },
        };
        let token = engine.acquire_ownership(&self.session).await.unwrap();
        engine
            .stores
            .turns
            .start_owned(&token, &self.session, "chat", None, leveler_core::now())
            .await
            .unwrap()
    }

    /// A turn that completed under a boot which has since ended, with its
    /// ownership released: the state a runtime leaves when it finished the turn
    /// and died while an independently hosted process kept running.
    async fn finished_turn_under(&self, lease: &RuntimeBootLease) -> leveler_storage::TurnRecord {
        let db = self.app.open_database().await.expect("db");
        let engine = leveler_engine::TaskEngine {
            stores: leveler_storage::EngineStores::from_database(&db),
            runtime_id: self.app.runtime_id().unwrap(),
            boot: leveler_engine::EngineBoot {
                id: lease.id().clone(),
                liveness: Arc::new(leveler_app::runtime_boot::StateDirBootLiveness::new(
                    &self.app.layout.state_dir,
                )),
            },
        };
        let token = engine.acquire_ownership(&self.session).await.unwrap();
        let turn = engine
            .stores
            .turns
            .start_owned(&token, &self.session, "chat", None, leveler_core::now())
            .await
            .unwrap();
        let event = leveler_engine::EngineEvent::TurnFinished {
            stop: None,
            turn_id: leveler_core::TurnId::new(turn.id.clone()),
            outcome: leveler_engine::TurnOutcome::Completed,
            stop_reason: "completed".into(),
            model_steps: 1,
            modified_files: Vec::new(),
        };
        let (event_type, payload) = event.to_row().unwrap();
        engine
            .stores
            .terminal
            .finish_turn_owned(
                &token,
                &self.session,
                &leveler_core::TurnId::new(turn.id.clone()),
                &event_type,
                &payload,
                leveler_engine::TurnOutcome::Completed,
                leveler_core::now(),
            )
            .await
            .unwrap();
        engine.release_ownership(&token).await.unwrap();
        turn
    }

    /// Archive a terminal process fact exactly as the Execution Host persists
    /// it. No live host is needed: the scope query only reads `tasks.json`.
    fn write_hosted_terminal(
        &self,
        task_id: &str,
        call_id: &str,
        status: leveler_execution::BackgroundTaskStatus,
        exit_code: Option<i32>,
    ) {
        let state_dir = self.app.layout.state_dir.join("execution-host");
        // The host's own boundary refuses a group/world-readable state dir;
        // a test that fabricates its archive must meet the same contract.
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&state_dir)
            .unwrap();
        let record = leveler_execution::execution_host::HostedTask {
            snapshot: leveler_execution::BackgroundTaskSnapshot {
                id: task_id.into(),
                program: "sh".into(),
                args: vec!["-c".into(), "printf 'hosted done\\n'".into()],
                cwd: self.repo.clone(),
                status,
                exit_code,
                log: "hosted done\n".into(),
                duration_ms: 42,
                pid: None,
                owner_scope: Some(self.session.as_str().to_owned()),
                goal_id: None,
                origin_call_id: Some(call_id.into()),
                log_truncated: false,
                ready: None,
                silent_ms: 0,
                lifetime: leveler_execution::BackgroundTaskLifetime::Goal,
                expired_exit_waits: 0,
                output_bytes: 12,
                start_identity: None,
                signal: None,
            },
            lifetime: leveler_execution::BackgroundTaskLifetime::Goal,
            writer_scope: self.session.as_str().to_owned(),
            write_scope: leveler_execution::WriteScope::WorkspaceWithGit {
                root: self.repo.clone(),
            },
            process_id: None,
            started_at_ms: 1,
            finished_at_ms: Some(2),
            log_end: 12,
            log_prefix_len: 0,
            owner_boot: None,
            owner_loss: None,
            loss: None,
            result: None,
        };
        std::fs::write(
            state_dir.join("tasks.json"),
            serde_json::to_vec(&[record]).unwrap(),
        )
        .unwrap();
    }
}

fn record_of<'a>(snapshot: &'a UiSessionSnapshot, task_id: &str) -> &'a UiBackgroundTaskRecord {
    snapshot
        .background_tasks
        .iter()
        .find(|record| record.task_id == task_id)
        .unwrap_or_else(|| panic!("no record for {task_id}: {:?}", snapshot.background_tasks))
}

/// A live task with no durable start (a log written before starts were
/// recorded) is still running work; a reconnect must not drop it.
#[tokio::test]
async fn a_reconnect_reports_a_live_task_as_running() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    let id = f.spawn(Some("call-live"), "exec sleep 30").await;
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .expect("start event")
            .expect("stream open");
        if matches!(event, RuntimeEvent::BackgroundTaskStarted { .. }) {
            break;
        }
    }

    let snapshot = f.snapshot().await;
    let record = record_of(&snapshot, &id);
    assert_eq!(record.state, UiBackgroundTaskState::Running);
    assert_eq!(record.program, "sh");
    assert_eq!(record.call_id, None, "no start row was written");
    assert!(
        snapshot
            .active_background_tasks
            .iter()
            .any(|task| task.task_id == id),
        "the live list is unchanged"
    );
    f.app.background_tasks().kill(&id).await.expect("kill");
}

/// The durable terminal is the only evidence of completion; the TUI being
/// reconnected to a quiet session is not.
#[tokio::test]
async fn a_completed_task_is_reported_from_its_paired_terminal_fact() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    f.record_background_start(None, "call-done", "bg-done", "sh", &["-c", "true"])
        .await;
    let id = f
        .spawn(Some("call-done"), "printf 'finished\\n'; exit 0")
        .await;
    f.wait_for_events(&mut events).await;
    while f.result_events().await.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let snapshot = f.snapshot().await;
    let record = record_of(&snapshot, &id);
    assert_eq!(record.state, UiBackgroundTaskState::Completed);
    assert_eq!(record.call_id.as_deref(), Some("call-done"));
    assert_eq!(record.exit_code, Some(0));
    assert!(record.output.contains("finished"), "{record:?}");
    assert_eq!(
        snapshot
            .background_tasks
            .iter()
            .filter(|record| record.task_id == id)
            .count(),
        1,
        "start and terminal are one record"
    );
}

#[tokio::test]
async fn a_failed_task_is_never_reported_as_completed() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    let id = f
        .spawn(Some("call-fail"), "printf 'boom\\n' >&2; exit 3")
        .await;
    f.wait_for_events(&mut events).await;
    while f.result_events().await.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let snapshot = f.snapshot().await;
    let record = record_of(&snapshot, &id);
    assert_eq!(record.state, UiBackgroundTaskState::Failed);
    assert_eq!(record.exit_code, Some(3));
}

#[tokio::test]
async fn a_stopped_task_stays_cancelled_across_a_snapshot() {
    let f = fixture().await;
    let mut events = f.runtime.subscribe_session(&f.session);
    let id = f.spawn(Some("call-stop"), "exec sleep 30").await;
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .expect("start event")
            .expect("stream open");
        if matches!(event, RuntimeEvent::BackgroundTaskStarted { .. }) {
            break;
        }
    }
    f.app.background_tasks().kill(&id).await.expect("kill");
    f.wait_for_events(&mut events).await;
    while f.result_events().await.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let snapshot = f.snapshot().await;
    let record = record_of(&snapshot, &id);
    assert_eq!(record.state, UiBackgroundTaskState::Cancelled);
}

/// The boot that hosted the task is gone and no terminal was written: the
/// process may have died with it. Neither success nor failure is claimed.
#[tokio::test]
async fn a_task_whose_owning_boot_ended_is_lost() {
    let f = fixture().await;
    let crashed = RuntimeBootLease::acquire(&f.app.layout.state_dir).unwrap();
    let turn = f.turn_under(&crashed).await;
    f.record_background_start(
        Some(turn.id.as_str()),
        "call-lost",
        "bg-lost",
        "sh",
        &["-c", "sleep 30"],
    )
    .await;
    drop(crashed);

    let snapshot = f.snapshot().await;
    let record = record_of(&snapshot, "bg-lost");
    assert_eq!(record.state, UiBackgroundTaskState::Lost);
    assert_eq!(record.call_id.as_deref(), Some("call-lost"));
    assert_eq!(record.exit_code, None);
    assert!(record.output.is_empty());
}

/// A task the Execution Host outlived the runtime for: it finished while no
/// runtime generation was attached. The archived process fact proves the
/// completion, so a reconnect reports it — not `Lost` because the turn's boot
/// is gone. Repeated snapshots keep the same reading.
#[tokio::test]
async fn a_hosted_terminal_fact_proves_completion_after_the_runtime_died() {
    let f = fixture().await;
    let crashed = RuntimeBootLease::acquire(&f.app.layout.state_dir).unwrap();
    let turn = f.finished_turn_under(&crashed).await;
    f.record_background_start(
        Some(turn.id.as_str()),
        "call-host",
        "host-finished",
        "sh",
        &["-c", "printf 'hosted done\\n'"],
    )
    .await;
    drop(crashed);
    f.write_hosted_terminal(
        "host-finished",
        "call-host",
        leveler_execution::BackgroundTaskStatus::Exited,
        Some(0),
    );

    for attempt in 0..2 {
        let snapshot = f.snapshot().await;
        let record = record_of(&snapshot, "host-finished");
        assert_eq!(
            record.state,
            UiBackgroundTaskState::Completed,
            "snapshot {attempt} must read the archived exit: {record:?}"
        );
        assert_eq!(record.call_id.as_deref(), Some("call-host"));
        assert_eq!(record.exit_code, Some(0));
        assert!(record.output.contains("hosted done"), "{record:?}");
    }
}

/// The fixture's real Execution Host. A goal task can only be genuinely
/// owner-lost while the host that admitted it is still alive; an archive
/// without a host is recovered as an unobserved terminal record instead, which
/// is a different fact.
async fn start_execution_host(f: &Fixture) -> tokio::task::JoinHandle<Result<(), String>> {
    let config = leveler_app::execution_host_config(&f.app.layout).unwrap();
    let server = tokio::spawn(leveler_execution::execution_host::serve(config.clone()));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if leveler_execution::execution_host::ExecutionHostClient::probe(&config)
                .await
                .is_ok_and(|probe| probe.client.is_some())
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Execution Host became reachable");
    server
}

/// A goal task the Execution Host still runs for a generation that is gone is
/// Lost, not Running: no runtime can deliver its result. Its clock stops at
/// the proven loss instead of growing for as long as the snapshot is read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_lost_task_is_lost_even_while_its_process_still_runs() {
    let f = fixture().await;
    let server = start_execution_host(&f).await;
    let config = leveler_app::execution_host_config(&f.app.layout).unwrap();
    let crashed = RuntimeBootLease::acquire(&f.app.layout.state_dir).unwrap();
    let crashed_boot = crashed.id().clone();
    f.app.background_tasks().bind_runtime_boot(
        crashed.id().clone(),
        Arc::new(leveler_app::runtime_boot::StateDirBootLiveness::new(
            &f.app.layout.state_dir,
        )),
    );
    let id = f
        .spawn(Some("call-goal"), "while :; do echo HEARTBEAT; done")
        .await;
    f.record_background_start(
        None,
        "call-goal",
        &id,
        "sh",
        &["-c", "while :; do echo HEARTBEAT; done"],
    )
    .await;
    let client = leveler_execution::execution_host::ExecutionHostClient::connect(config.clone())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !client
            .get_owned(&id, f.session.as_str())
            .await
            .unwrap()
            .log
            .contains("HEARTBEAT")
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the hosted task produced output");

    drop(crashed);
    f.app.reconcile_execution_services().await.unwrap();

    let snapshot = f.snapshot().await;
    assert!(
        !snapshot
            .active_background_tasks
            .iter()
            .any(|task| task.task_id == id),
        "a task whose owner is gone must not be this runtime's live work"
    );
    let record = record_of(&snapshot, &id);
    assert_eq!(record.state, UiBackgroundTaskState::Lost, "{record:?}");
    assert_eq!(record.call_id.as_deref(), Some("call-goal"));
    assert_eq!(record.exit_code, None);
    assert!(record.output.contains("HEARTBEAT"), "{record:?}");

    let archive = leveler_execution::execution_host::HostRecords::read(&config.state_dir).unwrap();
    let archived = archive.task(&id).expect("the host kept the record");
    let loss = archived.owner_loss.as_ref().expect("loss is recorded");
    assert_eq!(loss.owner_boot, crashed_boot.as_str());
    assert_eq!(
        archived.snapshot.status,
        leveler_execution::BackgroundTaskStatus::Running,
        "owner loss never signals the process"
    );
    let expected = loss.observed_at_ms.saturating_sub(archived.started_at_ms);
    assert_eq!(record.duration_ms, Some(expected), "{record:?}");

    // A second recovery in the same generation converges on the same fact:
    // no new timestamp and no changed projection.
    f.app.reconcile_execution_services().await.unwrap();
    let repeated = leveler_execution::execution_host::HostRecords::read(&config.state_dir).unwrap();
    assert_eq!(
        repeated
            .task(&id)
            .expect("the host kept the record")
            .owner_loss
            .as_ref()
            .expect("loss is recorded")
            .observed_at_ms,
        loss.observed_at_ms,
        "repeated recovery must not restamp the loss"
    );
    let repeated_snapshot = f.snapshot().await;
    assert_eq!(
        record_of(&repeated_snapshot, &id).duration_ms,
        Some(expected),
        "a repeated recovery must not move the frozen clock"
    );

    f.app
        .background_tasks()
        .kill_owned(&id, f.session.as_str())
        .await
        .unwrap();
    server.abort();
}

/// A crash inside the spawn call can lose the engine's start row while the
/// host still admitted the process. The archive is the only record left, and
/// it must appear as Lost rather than vanish or claim a live owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_lost_task_with_no_start_fact_is_never_dropped() {
    let f = fixture().await;
    let server = start_execution_host(&f).await;
    let crashed = RuntimeBootLease::acquire(&f.app.layout.state_dir).unwrap();
    f.app.background_tasks().bind_runtime_boot(
        crashed.id().clone(),
        Arc::new(leveler_app::runtime_boot::StateDirBootLiveness::new(
            &f.app.layout.state_dir,
        )),
    );
    let id = f
        .spawn(Some("call-before-log"), "while :; do echo HEARTBEAT; done")
        .await;
    let config = leveler_app::execution_host_config(&f.app.layout).unwrap();
    let client = leveler_execution::execution_host::ExecutionHostClient::connect(config.clone())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !client
            .get_owned(&id, f.session.as_str())
            .await
            .unwrap()
            .log
            .contains("HEARTBEAT")
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the hosted task produced output");

    drop(crashed);
    f.app.reconcile_execution_services().await.unwrap();

    let snapshot = f.snapshot().await;
    let record = record_of(&snapshot, &id);
    assert_eq!(record.state, UiBackgroundTaskState::Lost, "{record:?}");
    assert_eq!(record.call_id.as_deref(), Some("call-before-log"));
    assert!(record.output.contains("HEARTBEAT"), "{record:?}");
    assert!(record.duration_ms.is_some(), "{record:?}");

    f.app
        .background_tasks()
        .kill_owned(&id, f.session.as_str())
        .await
        .unwrap();
    server.abort();
}

/// The archive is evidence only for the start it belongs to. A record for the
/// same task id but a different call is a new/old conflict, never a
/// completion for this start.
#[tokio::test]
async fn a_hosted_fact_with_a_different_call_is_a_conflict() {
    let f = fixture().await;
    let crashed = RuntimeBootLease::acquire(&f.app.layout.state_dir).unwrap();
    let turn = f.finished_turn_under(&crashed).await;
    f.record_background_start(
        Some(turn.id.as_str()),
        "call-current",
        "host-reused",
        "sh",
        &["-c", "sleep 30"],
    )
    .await;
    drop(crashed);
    f.write_hosted_terminal(
        "host-reused",
        "call-previous",
        leveler_execution::BackgroundTaskStatus::Exited,
        Some(0),
    );

    let snapshot = f.snapshot().await;
    let record = record_of(&snapshot, "host-reused");
    assert_eq!(
        record.state,
        UiBackgroundTaskState::Conflict,
        "a different call cannot close this start: {record:?}"
    );
    assert_eq!(record.exit_code, None);
}

/// An archived task whose runtime lost the ability to observe its exit is
/// terminal but unproven: it is never reported as completed.
#[tokio::test]
async fn a_hosted_unknown_exit_is_not_a_completion() {
    let f = fixture().await;
    let crashed = RuntimeBootLease::acquire(&f.app.layout.state_dir).unwrap();
    let turn = f.finished_turn_under(&crashed).await;
    f.record_background_start(
        Some(turn.id.as_str()),
        "call-unknown",
        "host-unknown",
        "sh",
        &["-c", "sleep 30"],
    )
    .await;
    drop(crashed);
    f.write_hosted_terminal(
        "host-unknown",
        "call-unknown",
        leveler_execution::BackgroundTaskStatus::Unknown,
        None,
    );

    assert_eq!(
        record_of(&f.snapshot().await, "host-unknown").state,
        UiBackgroundTaskState::Unknown
    );
}

/// A start with no turn attribution cannot prove the hosting boot ended.
#[tokio::test]
async fn a_start_without_attribution_is_unknown_not_lost() {
    let f = fixture().await;
    f.record_background_start(None, "call-orphan", "bg-orphan", "sh", &["-c", "true"])
        .await;

    assert_eq!(
        record_of(&f.snapshot().await, "bg-orphan").state,
        UiBackgroundTaskState::Unknown
    );
}

/// The owning boot is still alive, but this process does not hold the task.
/// Neither "running" nor "lost" follows; the state stays open.
#[tokio::test]
async fn a_task_under_a_live_but_silent_boot_is_unknown() {
    let f = fixture().await;
    let live = RuntimeBootLease::acquire(&f.app.layout.state_dir).unwrap();
    let turn = f.turn_under(&live).await;
    f.record_background_start(
        Some(turn.id.as_str()),
        "call-silent",
        "bg-silent",
        "sh",
        &["-c", "true"],
    )
    .await;

    assert_eq!(
        record_of(&f.snapshot().await, "bg-silent").state,
        UiBackgroundTaskState::Unknown
    );
    drop(live);
}

/// Legacy ids (`bg-1`) can repeat across generations. Two starts under one id
/// are one conflict record, not two tasks and not a guessed outcome.
#[tokio::test]
async fn reused_task_ids_are_a_conflict() {
    let f = fixture().await;
    f.record_background_start(None, "call-old", "bg-1", "sh", &["-c", "true"])
        .await;
    f.record_background_start(None, "call-new", "bg-1", "sh", &["-c", "true"])
        .await;

    let snapshot = f.snapshot().await;
    let records: Vec<_> = snapshot
        .background_tasks
        .iter()
        .filter(|record| record.task_id == "bg-1")
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, UiBackgroundTaskState::Conflict);
    assert_eq!(records[0].call_id, None);
    assert_eq!(records[0].exit_code, None);
}

/// "Exited" without an established exit code is not a pass and not a proven
/// failure; it must not be rewritten as either.
#[tokio::test]
async fn an_exit_without_a_code_is_unknown() {
    let f = fixture().await;
    f.record_background_start(None, "call-nocode", "bg-nocode", "sh", &["-c", "true"])
        .await;
    f.record_background_finish(
        "call-nocode",
        "bg-nocode",
        leveler_execution::BackgroundTaskStatus::Exited,
        None,
    )
    .await;

    assert_eq!(
        record_of(&f.snapshot().await, "bg-nocode").state,
        UiBackgroundTaskState::Unknown
    );
}
