//! A scheduled action end to end through a real local socket runtime.
//!
//! This is the Phase 3 acceptance harness that does not depend on mail
//! authorization: a one-shot schedule delivers the existing user-originated
//! shell command through the runtime, records exactly one run, and never
//! reaches a model. The second dispatch is the deduplication proof.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ModelRef, PermissionProfile};
use leveler_execution::PermissionProfile as ExecutionProfile;
use leveler_local_transport::{
    LocalRuntimeService, LocalSocketRuntimeClient, LocalSocketServer, LocalWaiters,
};
use leveler_project::Layout;
use leveler_schedule::{
    Dispatcher, NewScheduleDefinition, OccurrenceState, RunState, RuntimeConnector, ScheduleStore,
    ScheduleTrigger,
};
use leveler_test_support::MockServer;
use tokio_util::sync::CancellationToken;

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn write_config(config_dir: &Path, base_url: &str) {
    isolate_global_config();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_due_schedule_runs_its_action_once_without_a_model() {
    let provider = MockServer::start(Vec::new()).await;

    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let config_dir = tmp.path().join("configs");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    write_config(&config_dir, &provider.base_url());
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    let before: Vec<_> = std::fs::read_dir(&workspace)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();

    let layout = Layout::from_parts(workspace.clone(), config_dir, state_dir);
    let app = Arc::new(Application::assemble(layout).unwrap());
    let runtime = Arc::new(InProcessRuntimeClient::new(
        app,
        ModelRef::new("mock", "m"),
        ExecutionProfile::Assisted,
        false,
    ));
    let socket = tmp.path().join("daemon.sock");
    let waiters = LocalWaiters::new();
    let server = LocalSocketServer::bind_with_waiters(&socket, runtime, waiters.clone())
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let serve = tokio::spawn(server.serve(shutdown.clone()));

    let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
        .await
        .unwrap();
    let definition = store
        .create_definition(NewScheduleDefinition {
            name: "deterministic action".into(),
            workspace: workspace.clone(),
            command: "printf 'scheduled-action-ok\\n'".into(),
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            mode: PermissionProfile::Assisted,
            model: Some(ModelRef::new("mock", "m")),
            timeout: Duration::from_secs(20),
            misfire: leveler_schedule::MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
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
    .with_poll_interval(Duration::from_millis(50));

    let first = dispatcher.dispatch_due(10).await.unwrap();
    assert_eq!(first.claimed, 1);
    assert_eq!(first.succeeded, 1, "the action did not succeed: {first:?}");

    let runs = store.runs(Some(&definition.id), 10).await.unwrap();
    assert_eq!(runs.len(), 1, "one occurrence must produce exactly one run");
    assert_eq!(runs[0].state, RunState::Succeeded);
    assert_eq!(runs[0].exit_code, Some(0));
    assert!(runs[0].session_id.is_some(), "the run lost its session id");
    assert!(
        runs[0].execution_id.is_some(),
        "the run lost the runtime execution id"
    );
    let bound = store
        .definition(&definition.id)
        .await
        .unwrap()
        .unwrap()
        .session_id
        .expect("the definition must keep its session");
    assert_eq!(runs[0].session_id.as_ref(), Some(&bound));

    let occurrences = store.occurrences(&definition.id, 10).await.unwrap();
    assert_eq!(occurrences.len(), 1);
    assert_eq!(occurrences[0].state, OccurrenceState::Completed);

    assert_eq!(
        provider.request_count(),
        0,
        "a scheduled shell action must never reach a model"
    );
    let after: Vec<_> = std::fs::read_dir(&workspace)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(after, before, "the action changed its workspace");

    let second = dispatcher.dispatch_due(10).await.unwrap();
    assert_eq!(second.claimed, 0, "a settled occurrence was claimed again");
    assert_eq!(store.runs(Some(&definition.id), 10).await.unwrap().len(), 1);

    // The dispatcher must not hold a subscription open: the client it used to
    // observe the run is dropped when the claim ends, so the daemon's own idle
    // eviction still sees a quiet runtime once the user leaves too.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while waiters.count() != 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        waiters.count(),
        0,
        "the dispatcher left a local client attached"
    );

    shutdown.cancel();
    let _ = serve.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recurring_definition_keeps_one_future_slot_after_a_run() {
    let provider = MockServer::start(Vec::new()).await;
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let config_dir = tmp.path().join("configs");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&workspace).unwrap();
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
    let socket = tmp.path().join("daemon.sock");
    let waiters = LocalWaiters::new();
    let server = LocalSocketServer::bind_with_waiters(&socket, runtime, waiters)
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let serve = tokio::spawn(server.serve(shutdown.clone()));

    let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
        .await
        .unwrap();
    let definition = store
        .create_definition(NewScheduleDefinition {
            name: "hourly action".into(),
            workspace: workspace.clone(),
            command: "printf 'hourly-ok\\n'".into(),
            trigger: ScheduleTrigger::Every {
                anchor: leveler_core::now() - chrono::Duration::seconds(10),
                every: Duration::from_secs(3600),
            },
            mode: PermissionProfile::Assisted,
            model: None,
            timeout: Duration::from_secs(20),
            misfire: leveler_schedule::MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
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
    .with_poll_interval(Duration::from_millis(50));

    let first = dispatcher.dispatch_due(10).await.unwrap();
    assert_eq!(first.succeeded, 1, "{first:?}");
    let occurrences = store.occurrences(&definition.id, 10).await.unwrap();
    assert_eq!(occurrences.len(), 2, "missed slots must not be backfilled");
    assert!(
        occurrences
            .iter()
            .any(|occurrence| occurrence.state == OccurrenceState::Pending),
        "a recurring definition must keep its next slot"
    );
    assert_eq!(
        store.runs(Some(&definition.id), 10).await.unwrap().len(),
        1
    );
    assert_eq!(
        provider.request_count(),
        0,
        "a scheduled shell action must never reach a model"
    );

    shutdown.cancel();
    let _ = serve.await;
}
