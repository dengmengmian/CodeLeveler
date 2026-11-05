//! A deterministic scheduled action runs on a runtime with no model at all.
//!
//! The regression: `serve` resolved a model before binding its socket, so a
//! runtime whose only work was fixed shell actions could not start in an empty
//! environment. A session also required a stored model, which forced a fake
//! reference into rows that never call one. These tests pin the real
//! acceptance: empty config, no provider, no key, still a working runtime — and
//! an agent turn that fails by naming the missing model.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, ClientError, InteractiveRuntimeClient, PermissionProfile,
};
use leveler_execution::PermissionProfile as ExecutionProfile;
use leveler_local_transport::{
    LocalRuntimeService, LocalSocketRuntimeClient, LocalSocketServer, LocalWaiters,
};
use leveler_project::Layout;
use leveler_schedule::{
    Dispatcher, NewScheduleDefinition, RunState, RuntimeConnector, ScheduleStore, ScheduleTrigger,
    UnattendedRequest,
};
use tokio_util::sync::CancellationToken;

const COMMAND: &str = "printf 'model-free\\n'";

struct SocketConnector {
    socket: PathBuf,
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

fn isolate_home() {
    use std::sync::OnceLock;
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

/// A workspace with an EMPTY config directory: no providers, no models, no
/// keys. Nothing here can answer a model request.
async fn model_free_runtime(
    tmp: &Path,
) -> (
    PathBuf,
    PathBuf,
    Arc<Application>,
    CancellationToken,
    tokio::task::JoinHandle<Result<(), leveler_local_transport::TransportError>>,
) {
    let workspace = tmp.join("workspace");
    let config_dir = tmp.join("configs");
    let state_dir = tmp.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    isolate_home();
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    let layout = Layout::from_parts(workspace.clone(), config_dir, state_dir);
    let app = Arc::new(Application::assemble(layout).unwrap());
    assert!(
        app.model_refs().is_empty(),
        "the acceptance environment must configure no model"
    );
    let runtime = Arc::new(InProcessRuntimeClient::new_without_model(
        app.clone(),
        ExecutionProfile::Assisted,
        false,
        false,
    ));
    let socket = tmp.join("daemon.sock");
    let waiters = LocalWaiters::new();
    let server = LocalSocketServer::bind_with_waiters(&socket, runtime, waiters)
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let serve = tokio::spawn(server.serve(shutdown.clone()));
    (workspace, socket, app, shutdown, serve)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scheduled_shell_runs_and_calls_no_model() {
    let tmp = tempfile::tempdir().unwrap();
    let (workspace, socket, app, shutdown, serve) = model_free_runtime(tmp.path()).await;

    let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
        .await
        .unwrap();
    let definition = store
        .create_definition(NewScheduleDefinition {
            name: "model-free".into(),
            workspace: workspace.clone(),
            action: leveler_schedule::ScheduleAction::Shell {
                command: COMMAND.into(),
            },
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            session_policy: leveler_schedule::SessionPolicy::Continue,
            overlap: leveler_schedule::OverlapPolicy::SkipIfRunning,
            mode: PermissionProfile::Assisted,
            // The definition itself carries no model: the run is deterministic.
            model: None,
            timeout: Duration::from_secs(15),
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
        Arc::new(SocketConnector {
            socket: socket.clone(),
        }),
    )
    .with_lease(Duration::from_secs(30))
    .with_poll_interval(Duration::from_millis(25));
    let summary = dispatcher.dispatch_due(10).await.unwrap();
    assert_eq!(summary.succeeded, 1, "{summary:?}");

    let runs = store.runs(Some(&definition.id), 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.state, RunState::Succeeded, "{run:?}");
    assert_eq!(run.exit_code, Some(0), "{run:?}");
    let session = run.session_id.clone().expect("the run owns a session");
    let execution = run.execution_id.clone().expect("the run owns an execution");

    let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    let snapshot = client.snapshot(&session).await.unwrap();
    assert_eq!(
        snapshot.model, None,
        "a deterministic session must not store a placeholder model"
    );
    let shell = snapshot
        .user_shells
        .iter()
        .find(|shell| shell.id.as_str() == execution)
        .expect("the run's execution must be visible");
    assert_eq!(shell.command, COMMAND);
    assert_eq!(shell.status, "success");
    assert_eq!(shell.exit_code, Some(0));
    assert_eq!(shell.output_tail, "model-free\n");
    assert_eq!(
        shell.origin_command_id,
        Some(leveler_core::CommandId::new(format!(
            "schedule-run-{}",
            run.id
        ))),
        "the run settles on the execution carrying its own delivery identity"
    );

    // Zero model requests: the whole point of a deterministic action. The
    // runtime has no provider to call, so a nonzero ledger entry would also
    // mean the call path tried anyway.
    let db = app.open_database().await.unwrap();
    let requests = leveler_storage::ModelRequestRepository::new(&db)
        .load_for_session(&session)
        .await
        .unwrap();
    assert_eq!(requests.len(), 0, "a scheduled shell called a model");

    // Re-dispatching a settled occurrence must not execute it again.
    let summary = dispatcher.dispatch_due(10).await.unwrap();
    assert_eq!(summary.claimed, 0, "a settled occurrence was claimed again");
    let after = client.snapshot(&session).await.unwrap();
    assert_eq!(
        after
            .user_shells
            .iter()
            .filter(|shell| shell.command == COMMAND)
            .count(),
        1,
        "re-dispatch executed the shell again"
    );

    // An agent turn on the same session must fail by naming the missing model,
    // never by inventing one or pretending to succeed.
    let error = client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "hello".into(),
            attachments: Vec::new(),
        })
        .await
        .expect_err("an agent turn has no model to run");
    match &error {
        ClientError::Runtime(message) => assert!(
            message.contains("no model configured"),
            "the error must name the missing configuration: {message}"
        ),
        other => panic!("unexpected error shape: {other:?}"),
    }

    shutdown.cancel();
    let _ = serve.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_free_session_is_created_without_an_invented_model() {
    let tmp = tempfile::tempdir().unwrap();
    let (workspace, socket, app, shutdown, serve) = model_free_runtime(tmp.path()).await;

    let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    let bootstrap = client
        .create_session(leveler_local_transport::CreateSessionRequest {
            request_id: None,
            workspace: leveler_local_transport::CreateWorkspaceSelection::Workspace {
                path: workspace.display().to_string(),
            },
            goal: "deterministic".into(),
            model: None,
            mode: PermissionProfile::Assisted,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            collaboration: leveler_local_transport::CollaborationMode::Chat,
        })
        .await
        .unwrap();
    assert_eq!(bootstrap.session.model, None);
    assert_eq!(
        bootstrap.context_window, 0,
        "no model means no context window; zero is the wire's absent value"
    );

    // The same fact is durable, not just a wire projection.
    let stored = leveler_storage::SessionRepository::new(&app.open_database().await.unwrap())
        .get(&bootstrap.session.id)
        .await
        .unwrap()
        .expect("session row");
    assert_eq!(stored.model, None);

    shutdown.cancel();
    let _ = serve.await;
}
