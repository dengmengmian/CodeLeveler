//! A scheduled run must settle on its own execution, never on a look-alike.
//!
//! The regression: the dispatcher used to identify its execution by
//! (command text + "not present before delivery"). Another client running the
//! same text in the same session is indistinguishable from that evidence, so
//! the run could adopt someone else's exit status. These tests pin the
//! dispatcher's correlation to the delivery identity it already owns.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use leveler_client_protocol::{
    ClientCommand, ClientError, CommandEnvelope, InteractiveRuntimeClient, PermissionProfile,
    RuntimeEvent, SessionId, UiSessionSnapshot, UiUserShell,
};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
use leveler_schedule::{
    Dispatcher, NewScheduleDefinition, RunState, RuntimeConnector, ScheduleStore, ScheduleTrigger,
    UnattendedRequest,
};
use tokio::sync::broadcast;

const IDENTICAL: &str = "printf 'same-text\\n'";

/// A runtime whose observed session already contains the same text, run by
/// somebody else, while the dispatcher's own command never produces an
/// execution. This is the executor-level shape of two clients racing on one
/// session: one execution exists, and it does not belong to the schedule.
struct ForeignExecutionRuntime {
    session: SessionId,
    snapshots: AtomicUsize,
    delivered: Mutex<Vec<CommandEnvelope>>,
    events: broadcast::Sender<RuntimeEvent>,
}

impl ForeignExecutionRuntime {
    fn new() -> Self {
        let (events, _) = broadcast::channel(8);
        Self {
            session: SessionId::new("sess-identity"),
            snapshots: AtomicUsize::new(0),
            delivered: Mutex::new(Vec::new()),
            events,
        }
    }

    fn snapshot_with(&self, shells: Vec<UiUserShell>) -> UiSessionSnapshot {
        UiSessionSnapshot {
            history: None,
            id: self.session.clone(),
            repository: None,
            goal: "Scheduled action: identity".into(),
            task_status: None,
            task_terminal: None,
            model: None,
            mode: PermissionProfile::Assisted,
            branch: None,
            status: "idle".into(),
            finalization_stage: None,
            messages: Vec::new(),
            pending_interactions: Vec::new(),
            available_models: Vec::new(),
            vision: false,
            last_sequence: None,
            latest_turn_ordinal: None,
            active_tools: Vec::new(),
            interrupted_tool_calls: Vec::new(),
            active_background_tasks: Vec::new(),
            background_tasks: Vec::new(),
            plan: None,
            diff: None,
            checkpoints: Vec::new(),
            recaps: Vec::new(),
            user_shells: shells,
            completion_report: None,
            thinking: None,
            work_profile: None,
            collaboration: None,
            execution_task: None,
            children: Vec::new(),
            foreign_origin: None,
        }
    }
}

#[async_trait]
impl leveler_client_protocol::InteractiveRuntimeClient for ForeignExecutionRuntime {
    async fn send(&self, _command: ClientCommand) -> Result<(), ClientError> {
        Ok(())
    }

    async fn deliver(&self, envelope: CommandEnvelope) -> Result<(), ClientError> {
        self.delivered.lock().unwrap().push(envelope);
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        if session_id != &self.session {
            return Err(ClientError::SessionNotFound(session_id.clone()));
        }
        let seen = self.snapshots.fetch_add(1, Ordering::SeqCst);
        // The first read is the dispatcher's pre-delivery state (empty). From
        // then on, a foreign execution with the identical text is visible and
        // its status is terminal.
        if seen == 0 {
            return Ok(self.snapshot_with(Vec::new()));
        }
        Ok(self.snapshot_with(vec![UiUserShell {
            id: leveler_core::UserShellId::new("ush-another-client"),
            // The racing client delivered through its own envelope; its
            // execution carries that client's command id, not the schedule's.
            origin_command_id: Some(leveler_core::CommandId::new("another-client-command")),
            command: IDENTICAL.into(),
            cwd: "/tmp".into(),
            status: "success".into(),
            elapsed_secs: 1,
            exit_code: Some(0),
            output_tail: "same-text\n".into(),
            output_truncated: false,
            full_output: None,
            task_id: None,
        }]))
    }
}

#[async_trait]
impl LocalRuntimeService for ForeignExecutionRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Ok(SessionBootstrap {
            session: self.snapshot_with(Vec::new()),
            context_window: 131_072,
            notice: None,
        })
    }
}

struct StaticConnector {
    runtime: Arc<ForeignExecutionRuntime>,
}

#[async_trait]
impl RuntimeConnector for StaticConnector {
    async fn connect(&self, _workspace: &Path) -> Result<Arc<dyn LocalRuntimeService>, String> {
        Ok(self.runtime.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_never_adopts_another_clients_identical_command() {
    let runtime = Arc::new(ForeignExecutionRuntime::new());
    let tmp = tempfile::tempdir().unwrap();
    let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
        .await
        .expect("schedule store");
    let definition = store
        .create_definition(NewScheduleDefinition {
            name: "identity".into(),
            workspace: PathBuf::from("/tmp"),
            action: leveler_schedule::ScheduleAction::Shell {
                command: IDENTICAL.into(),
            },
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            session_policy: leveler_schedule::SessionPolicy::Continue,
            overlap: leveler_schedule::OverlapPolicy::SkipIfRunning,
            mode: PermissionProfile::Assisted,
            model: None,
            timeout: Duration::from_secs(1),
            misfire: leveler_schedule::MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            unattended: Some(UnattendedRequest {
                allow_external_side_effects: true,
            }),
        })
        .await
        .unwrap();

    let dispatcher = Dispatcher::new(
        store.clone(),
        Arc::new(StaticConnector {
            runtime: Arc::clone(&runtime),
        }),
    )
    .with_lease(Duration::from_secs(30))
    .with_poll_interval(Duration::from_millis(20));

    let summary = dispatcher.dispatch_due(10).await.unwrap();
    let runs = store.runs(Some(&definition.id), 10).await.unwrap();
    assert_eq!(runs.len(), 1);

    assert_ne!(
        runs[0].state,
        RunState::Succeeded,
        "the run adopted an execution that does not belong to it: {summary:?}"
    );
    assert!(
        runs[0].execution_id.is_none(),
        "the run recorded another client's execution id: {:?}",
        runs[0].execution_id
    );
    assert_eq!(
        runs[0].state,
        RunState::Unknown,
        "without its own observable execution the run's effect is unknown"
    );
    assert_eq!(summary.unknown, 1);
}

// ---------------------------------------------------------------------------
// Real-socket concurrency: two clients, one session, identical text.
// ---------------------------------------------------------------------------

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{CommandId, ModelRef};
use leveler_execution::PermissionProfile as ExecutionProfile;
use leveler_local_transport::{
    CollaborationMode, CreateWorkspaceSelection, LocalSocketRuntimeClient, LocalSocketServer,
    LocalWaiters,
};
use leveler_project::Layout;
use leveler_test_support::MockServer;
use tokio_util::sync::CancellationToken;

fn write_config(config_dir: &Path, base_url: &str) {
    std::fs::create_dir_all(config_dir.join("providers")).unwrap();
    std::fs::create_dir_all(config_dir.join("models")).unwrap();
    std::fs::write(
        config_dir.join("providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
    )
    .unwrap();
    std::fs::write(
        config_dir.join("models/m.yaml"),
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

async fn create_runtime(
    tmp: &Path,
    provider: &MockServer,
) -> (
    std::path::PathBuf,
    std::path::PathBuf,
    LocalWaiters,
    CancellationToken,
    tokio::task::JoinHandle<Result<(), leveler_local_transport::TransportError>>,
) {
    let workspace = tmp.join("workspace");
    let config_dir = tmp.join("configs");
    let state_dir = tmp.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    isolate_home();
    write_config(&config_dir, &provider.base_url());
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    let layout = Layout::from_parts(workspace.clone(), config_dir, state_dir);
    let app = Arc::new(Application::assemble(layout).unwrap());
    let runtime = Arc::new(InProcessRuntimeClient::new(
        app,
        ModelRef::new("mock", "m"),
        ExecutionProfile::Assisted,
        false,
    ));
    let socket = tmp.join("daemon.sock");
    let waiters = LocalWaiters::new();
    let server = LocalSocketServer::bind_with_waiters(&socket, runtime, waiters.clone())
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let serve = tokio::spawn(server.serve(shutdown.clone()));
    (workspace, socket, waiters, shutdown, serve)
}

fn isolate_home() {
    use std::sync::OnceLock;
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

async fn new_session(client: &LocalSocketRuntimeClient, workspace: &Path) -> SessionId {
    client
        .create_session(CreateSessionRequest {
            request_id: None,
            workspace: CreateWorkspaceSelection::Workspace {
                path: workspace.display().to_string(),
            },
            goal: "identity acceptance".into(),
            model: Some(ModelRef::new("mock", "m")),
            mode: PermissionProfile::Assisted,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            collaboration: CollaborationMode::Chat,
        })
        .await
        .unwrap()
        .session
        .id
}

async fn deliver_shell(
    client: &LocalSocketRuntimeClient,
    session: &SessionId,
    command_id: &CommandId,
    text: &str,
) -> Result<(), ClientError> {
    client
        .deliver(CommandEnvelope {
            command_id: command_id.clone(),
            session_id: session.clone(),
            expected_version: None,
            issued_at: leveler_core::now().to_rfc3339(),
            command: ClientCommand::RunUserShell {
                session_id: session.clone(),
                command: text.into(),
            },
        })
        .await
}

async fn await_execution(
    client: &LocalSocketRuntimeClient,
    session: &SessionId,
    origin: &CommandId,
) -> leveler_client_protocol::UiUserShell {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let snapshot = client.snapshot(session).await.unwrap();
        if let Some(shell) = snapshot
            .user_shells
            .into_iter()
            .find(|shell| shell.origin_command_id.as_ref() == Some(origin))
            && shell.status != "running"
        {
            return shell;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no execution carrying {origin} reached a terminal state"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_text_from_two_clients_and_a_schedule_keeps_three_identities() {
    let provider = MockServer::start(Vec::new()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (workspace, socket, _waiters, shutdown, serve) =
        create_runtime(tmp.path(), &provider).await;

    let first = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    let second = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    let session = new_session(&first, &workspace).await;

    let first_id = CommandId::new("client-a-command");
    let second_id = CommandId::new("client-b-command");
    deliver_shell(&first, &session, &first_id, IDENTICAL)
        .await
        .unwrap();
    let first_shell = await_execution(&first, &session, &first_id).await;
    assert_eq!(first_shell.status, "success");
    assert_eq!(first_shell.exit_code, Some(0));

    deliver_shell(&second, &session, &second_id, IDENTICAL)
        .await
        .unwrap();
    let second_shell = await_execution(&second, &session, &second_id).await;
    assert_eq!(second_shell.status, "success");
    assert_ne!(
        first_shell.id, second_shell.id,
        "two deliveries must produce two executions"
    );

    // At-least-once delivery: resending the same envelope must not run the
    // shell again, and must not change which execution carries the identity.
    deliver_shell(&first, &session, &second_id, IDENTICAL)
        .await
        .unwrap();
    let snapshot = first.snapshot(&session).await.unwrap();
    let matching: Vec<_> = snapshot
        .user_shells
        .iter()
        .filter(|shell| shell.origin_command_id.as_ref() == Some(&second_id))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "a duplicate delivery re-executed the shell"
    );
    assert_eq!(matching[0].id, second_shell.id);

    // A second, different command in the same session gives the dispatcher
    // another execution to choose from; identity matching must still be exact.
    let other_id = CommandId::new("client-a-other-command");
    deliver_shell(&first, &session, &other_id, "printf 'different\\n'")
        .await
        .unwrap();
    let other_shell = await_execution(&first, &session, &other_id).await;
    assert_eq!(other_shell.status, "success");
    assert_ne!(other_shell.id, first_shell.id);
    assert_ne!(other_shell.id, second_shell.id);

    // A schedule sharing the session and the identical text must settle on
    // its own execution, not on either client's.
    let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
        .await
        .unwrap();
    let definition = store
        .create_definition(NewScheduleDefinition {
            name: "same text".into(),
            workspace: workspace.clone(),
            action: leveler_schedule::ScheduleAction::Shell {
                command: IDENTICAL.into(),
            },
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            session_policy: leveler_schedule::SessionPolicy::Continue,
            overlap: leveler_schedule::OverlapPolicy::SkipIfRunning,
            mode: PermissionProfile::Assisted,
            model: Some(ModelRef::new("mock", "m")),
            timeout: Duration::from_secs(10),
            misfire: leveler_schedule::MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            unattended: Some(UnattendedRequest {
                allow_external_side_effects: true,
            }),
        })
        .await
        .unwrap();
    store.set_session(&definition.id, &session).await.unwrap();
    let dispatcher = Dispatcher::new(
        store.clone(),
        Arc::new(SocketConnector {
            socket: socket.clone(),
        }),
    )
    .with_lease(Duration::from_secs(30))
    .with_poll_interval(Duration::from_millis(25));
    let summary = dispatcher.dispatch_due(10).await.unwrap();
    assert_eq!(summary.claimed, 1, "{summary:?}");
    // A fast shell can still be `running` on the first snapshot. That is a
    // handoff, not success. Later read-only passes settle the same delivery.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let runs = store.runs(Some(&definition.id), 10).await.unwrap();
        if runs.len() == 1 && runs[0].state == RunState::Succeeded {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the scheduled shell did not settle on its own execution: {runs:?}"
        );
        let observed = dispatcher.dispatch_due(0).await.unwrap();
        assert_eq!(
            observed.claimed, 0,
            "observation must not deliver the shell again: {observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let runs = store.runs(Some(&definition.id), 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, RunState::Succeeded);
    let execution = runs[0].execution_id.clone().expect("execution id");
    assert_ne!(execution, first_shell.id.as_str());
    assert_ne!(execution, second_shell.id.as_str());
    assert_ne!(execution, other_shell.id.as_str());

    let snapshot = first.snapshot(&session).await.unwrap();
    let schedule_shell = snapshot
        .user_shells
        .iter()
        .find(|shell| shell.id.as_str() == execution)
        .expect("the run's execution must exist");
    assert_eq!(
        schedule_shell.origin_command_id,
        Some(CommandId::new(format!("schedule-run-{}", runs[0].id)))
    );
    assert_eq!(
        snapshot
            .user_shells
            .iter()
            .filter(|shell| shell.command == IDENTICAL)
            .count(),
        3,
        "each delivery must have exactly one execution"
    );

    let summary = dispatcher.dispatch_due(10).await.unwrap();
    assert_eq!(summary.claimed, 0, "a settled occurrence was claimed again");
    assert_eq!(provider.request_count(), 0);

    shutdown.cancel();
    let _ = serve.await;
}

struct SocketConnector {
    socket: std::path::PathBuf,
}

#[async_trait]
impl RuntimeConnector for SocketConnector {
    async fn connect(&self, _workspace: &Path) -> Result<Arc<dyn LocalRuntimeService>, String> {
        let client = LocalSocketRuntimeClient::connect(&self.socket)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(client))
    }
}
