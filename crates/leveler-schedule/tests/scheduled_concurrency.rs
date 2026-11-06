//! Two due slots of an `allow-concurrent` shell schedule must really execute
//! at the same time.
//!
//! The regression this pins: the store claimed both occurrences (the claim
//! predicate honours the overlap policy), but the dispatcher always resolved
//! the definition's one `continue` session. The runtime admits one turn per
//! session, so the second delivery hit `Busy`, the runtime answered `Ok` after
//! showing a notice, and the dispatcher could only record `unknown` when the
//! execution never appeared. Claim success was not execution.
//!
//! The harness is the real local socket runtime over an isolated workspace.
//! Run A blocks on a gate file the test only creates after run B has finished,
//! so the overlap is proven by construction rather than by clock sampling:
//!
//! ```text
//! start(A) < start(B) < finish(B) < finish(A)
//! ```

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{CommandId, InteractiveRuntimeClient, ModelRef, PermissionProfile};
use leveler_execution::PermissionProfile as ExecutionProfile;
use leveler_local_transport::{
    LocalRuntimeService, LocalSocketRuntimeClient, LocalSocketServer, LocalWaiters,
};
use leveler_project::Layout;
use leveler_schedule::{
    Dispatcher, NewScheduleDefinition, OccurrenceState, OverlapPolicy, RunState, RuntimeConnector,
    ScheduleAction, ScheduleStore, ScheduleTrigger, SessionPolicy, UnattendedRequest,
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

/// One line because a schedule command may not contain control characters.
///
/// The first execution parks on `markers/gate`; the test dispatches the second
/// execution only after `markers/first` exists, so only the first one waits.
/// Each execution writes its own PID and start/finish marks to its own file
/// and prints the file name, which the runtime keeps in the execution's
/// output tail — the join from run id to the marker file.
const SHELL: &str = "mkdir -p markers; f=\"markers/shell-$$.log\"; \
printf 'start pid=%s at=%s\\n' \"$$\" \"$(date -u +%s)\" > \"$f\"; \
printf 'marker=%s\\n' \"$f\"; \
if [ ! -f markers/first ]; then : > markers/first; \
while [ ! -f markers/gate ]; do sleep 0.05; done; fi; \
printf 'finish pid=%s at=%s\\n' \"$$\" \"$(date -u +%s)\" >> \"$f\"";

fn marker_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn marker_pid(path: &Path) -> String {
    marker_lines(path)
        .iter()
        .find_map(|line| line.strip_prefix("start pid="))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_default()
        .to_string()
}

fn marker_has_finish(path: &Path) -> bool {
    marker_lines(path)
        .iter()
        .any(|line| line.starts_with("finish "))
}

fn marker_stamp(path: &Path, prefix: &str) -> f64 {
    marker_lines(path)
        .iter()
        .find_map(|line| {
            let rest = line.strip_prefix(prefix)?;
            rest.split("at=").nth(1)?.parse().ok()
        })
        .unwrap_or(0.0)
}

/// Read shells that were released while still running. `max = 0` claims nothing.
async fn observe_runs(
    dispatcher: &Dispatcher,
    store: &ScheduleStore,
    ids: &[leveler_schedule::RunId],
    budget: Duration,
) {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let mut open = Vec::new();
        for id in ids {
            let run = store.run(id).await.unwrap().expect("run row");
            if !run.state.is_terminal() {
                open.push(format!("{}={:?}", id, run.state));
            }
        }
        if open.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "observation window elapsed: {open:?}"
        );
        dispatcher.dispatch_due(0).await.expect("observation pass");
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

/// Wait for the first marker file whose name matches `shell-*.log`.
async fn wait_for_marker(dir: &Path, timeout: Duration) -> Option<PathBuf> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut found: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("shell-") && name.ends_with(".log"))
                })
                .collect();
            found.sort();
            if let Some(path) = found.into_iter().next() {
                return Some(path);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_marker_count(dir: &Path, count: usize, timeout: Duration) -> Vec<PathBuf> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                name.starts_with("shell-") && name.ends_with(".log")
                            })
                    })
                    .collect()
            })
            .unwrap_or_default();
        found.sort();
        if found.len() >= count || tokio::time::Instant::now() >= deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_session_turns_a_scheduled_delivery_into_a_definite_failure() {
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
    let session = app
        .create_session(&ModelRef::new("mock", "m"), "busy session")
        .await
        .unwrap();
    let runtime = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
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
            name: "busy delivery".into(),
            workspace: workspace.clone(),
            action: ScheduleAction::Shell {
                command: "touch scheduled-must-not-run".into(),
            },
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            session_policy: SessionPolicy::Continue,
            overlap: OverlapPolicy::SkipIfRunning,
            mode: PermissionProfile::Assisted,
            model: None,
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

    // An interactive command owns the session's single turn slot.
    let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    let hold = leveler_test_support::sleep_shell_line(30);
    client
        .send(leveler_client_protocol::ClientCommand::RunUserShell {
            session_id: session.clone(),
            command: hold,
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = client.snapshot(&session).await.unwrap();
        if snapshot.user_shells.iter().any(|shell| {
            shell.origin_command_id.is_none()
                && matches!(shell.status.as_str(), "running" | "background")
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the interactive shell never occupied the session: {:?}",
            snapshot.user_shells
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let connector: Arc<dyn RuntimeConnector> = Arc::new(SocketConnector {
        socket: socket.clone(),
    });
    let dispatcher = Dispatcher::new(store.clone(), connector)
        .with_lease(Duration::from_secs(30))
        .with_poll_interval(Duration::from_millis(50));
    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.failed, 1, "{summary:?}");

    let run = store
        .runs(Some(&definition.id), 10)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("the claimed run");
    // The runtime said no before acknowledging, so this is a definite
    // failure — never a delivered-but-unexplained `unknown`.
    assert_eq!(run.state, RunState::Failed, "{run:?}");
    assert_eq!(run.session_id.as_ref(), Some(&session));
    assert!(
        run.delivered_at.is_none(),
        "a refused delivery must not be recorded as delivered: {run:?}"
    );
    let error = run.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("did not accept the scheduled action") && error.contains("Agent 正在运行"),
        "{run:?}"
    );

    // The scheduled command did not run, and the interactive shell is intact.
    assert!(!workspace.join("scheduled-must-not-run").exists());
    let snapshot = client.snapshot(&session).await.unwrap();
    assert_eq!(snapshot.user_shells.len(), 1, "{:?}", snapshot.user_shells);
    if let Some(shell) = snapshot.user_shells.first() {
        client
            .send(leveler_client_protocol::ClientCommand::CancelUserShell {
                session_id: session.clone(),
                execution_id: shell.id.clone(),
            })
            .await
            .unwrap();
        // Let the worker reach its terminal before the runtime goes away;
        // tearing down a live tokio context panics the worker thread.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = client.snapshot(&session).await.unwrap();
            if snapshot
                .user_shells
                .iter()
                .all(|shell| !matches!(shell.status.as_str(), "running" | "background"))
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the interactive shell did not stop after cancel: {:?}",
                snapshot.user_shells
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    shutdown.cancel();
    let _ = serve.await;
    let _ = provider.request_count();
}

/// Like [`SHELL`], but every execution blocks for a few seconds: the test
/// cancels one run while the other is still working.
const SLEEPING_SHELL: &str = "mkdir -p markers; f=\"markers/shell-$$.log\"; \
printf 'start pid=%s at=%s\\n' \"$$\" \"$(date -u +%s)\" > \"$f\"; \
sleep 5; \
printf 'finish pid=%s at=%s\\n' \"$$\" \"$(date -u +%s)\" >> \"$f\"";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_one_overlapping_run_leaves_the_other_alone() {
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
            name: "cancelled concurrent shell".into(),
            workspace: workspace.clone(),
            action: ScheduleAction::Shell {
                command: SLEEPING_SHELL.into(),
            },
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            session_policy: SessionPolicy::Fresh,
            overlap: OverlapPolicy::AllowConcurrent,
            mode: PermissionProfile::Assisted,
            model: None,
            timeout: Duration::from_secs(30),
            misfire: leveler_schedule::MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            unattended: Some(UnattendedRequest {
                allow_external_side_effects: true,
            }),
        })
        .await
        .unwrap();

    let connector: Arc<dyn RuntimeConnector> = Arc::new(SocketConnector {
        socket: socket.clone(),
    });
    let dispatcher = |store: ScheduleStore| {
        Arc::new(
            Dispatcher::new(store, connector.clone())
                .with_lease(Duration::from_secs(30))
                .with_poll_interval(Duration::from_millis(50)),
        )
    };
    let markers = workspace.join("markers");
    let first_pass = {
        let dispatcher = dispatcher(store.clone());
        tokio::spawn(async move { dispatcher.dispatch_due(10).await })
    };
    let marker_a = wait_for_marker(&markers, Duration::from_secs(10))
        .await
        .expect("run A started");
    let run_a = store
        .runs(Some(&definition.id), 10)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("run A row");

    store.create_run_now(&definition).await.unwrap();
    let second_pass = {
        let dispatcher = dispatcher(store.clone());
        tokio::spawn(async move { dispatcher.dispatch_due(10).await })
    };
    let started = wait_for_marker_count(&markers, 2, Duration::from_secs(10)).await;
    assert_eq!(
        started.len(),
        2,
        "the second run never executed: {started:?}"
    );
    let run_b = store
        .runs(Some(&definition.id), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|run| run.id != run_a.id)
        .expect("run B row");
    // Both passes return once the shells are running. Cancellation and the
    // terminal records are applied by later read-only passes.
    let first_summary = first_pass
        .await
        .expect("run A task panicked")
        .expect("run A pass failed");
    let second_summary = second_pass
        .await
        .expect("run B task panicked")
        .expect("run B pass failed");
    assert!(
        first_summary.handed_off + first_summary.succeeded >= 1,
        "{first_summary:?}"
    );
    assert!(
        second_summary.handed_off + second_summary.succeeded >= 1,
        "{second_summary:?}"
    );
    store.request_cancel(&run_b.id).await.unwrap();
    observe_runs(
        &dispatcher(store.clone()),
        &store,
        &[run_a.id.clone(), run_b.id.clone()],
        Duration::from_secs(20),
    )
    .await;

    let run_a = store.run(&run_a.id).await.unwrap().unwrap();
    let run_b = store.run(&run_b.id).await.unwrap().unwrap();
    assert_eq!(
        run_a.state,
        RunState::Succeeded,
        "{run_a:?} {first_summary:?}"
    );
    assert_eq!(
        run_b.state,
        RunState::Cancelled,
        "{run_b:?} {second_summary:?}"
    );
    assert_ne!(run_a.session_id, run_b.session_id);
    assert!(marker_has_finish(&marker_a), "run A did not finish");
    assert_eq!(
        marker_lines(&marker_a)
            .iter()
            .filter(|line| line.starts_with("start "))
            .count(),
        1
    );

    // The cancellation hit run B's execution, not A's.
    let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    let snapshot_a = client
        .snapshot(run_a.session_id.as_ref().unwrap())
        .await
        .unwrap();
    let snapshot_b = client
        .snapshot(run_b.session_id.as_ref().unwrap())
        .await
        .unwrap();
    let origin_a = CommandId::new(format!("schedule-run-{}", run_a.id));
    let origin_b = CommandId::new(format!("schedule-run-{}", run_b.id));
    assert!(
        snapshot_a
            .user_shells
            .iter()
            .all(|shell| { shell.origin_command_id.as_ref() != Some(&origin_b) })
    );
    assert!(
        snapshot_b
            .user_shells
            .iter()
            .all(|shell| { shell.origin_command_id.as_ref() != Some(&origin_a) })
    );
    let shell_a = snapshot_a
        .user_shells
        .iter()
        .find(|shell| shell.origin_command_id.as_ref() == Some(&origin_a))
        .expect("run A execution");
    let shell_b = snapshot_b
        .user_shells
        .iter()
        .find(|shell| shell.origin_command_id.as_ref() == Some(&origin_b))
        .expect("run B execution");
    assert_eq!(shell_a.status, "success");
    assert_eq!(shell_b.status, "cancelled");

    shutdown.cancel();
    let _ = serve.await;
    let _ = provider.request_count();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_overlapping_shell_runs_settle_independently() {
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
            name: "three concurrent shells".into(),
            workspace: workspace.clone(),
            action: ScheduleAction::Shell {
                command: SHELL.into(),
            },
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            session_policy: SessionPolicy::Fresh,
            overlap: OverlapPolicy::AllowConcurrent,
            mode: PermissionProfile::Assisted,
            model: None,
            timeout: Duration::from_secs(30),
            misfire: leveler_schedule::MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            unattended: Some(UnattendedRequest {
                allow_external_side_effects: true,
            }),
        })
        .await
        .unwrap();

    let connector: Arc<dyn RuntimeConnector> = Arc::new(SocketConnector {
        socket: socket.clone(),
    });
    let dispatcher = |store: ScheduleStore| {
        Arc::new(
            Dispatcher::new(store, connector.clone())
                .with_lease(Duration::from_secs(30))
                .with_poll_interval(Duration::from_millis(50)),
        )
    };
    let dispatcher_a = dispatcher(store.clone());
    let dispatcher_b = dispatcher(store.clone());

    let markers = workspace.join("markers");
    let first_pass = {
        let dispatcher = dispatcher_a.clone();
        tokio::spawn(async move { dispatcher.dispatch_due(10).await })
    };
    assert!(
        wait_for_file(&markers.join("first"), Duration::from_secs(10)).await,
        "run A never started"
    );

    // Two more occurrences of the same definition while A is parked. The
    // occurrence identity is (definition, scheduled_for) at millisecond
    // granularity, so back-to-back run-now slots need distinct instants.
    store.create_run_now(&definition).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    store.create_run_now(&definition).await.unwrap();
    let second_pass = {
        let dispatcher = dispatcher_b.clone();
        tokio::spawn(async move { dispatcher.dispatch_due(10).await })
    };

    let started = wait_for_marker_count(&markers, 3, Duration::from_secs(10)).await;
    assert_eq!(
        started.len(),
        3,
        "only {} of three concurrent runs executed: {started:?}",
        started.len()
    );
    let first_summary = first_pass
        .await
        .expect("run A task panicked")
        .expect("run A pass failed");
    let second_summary = second_pass
        .await
        .expect("runs B/C task panicked")
        .expect("runs B/C pass failed");
    assert!(
        first_summary.claimed >= 1,
        "{first_summary:?} {second_summary:?}"
    );
    std::fs::write(markers.join("gate"), "").unwrap();
    let runs = store.runs(Some(&definition.id), 10).await.unwrap();
    assert_eq!(runs.len(), 3, "{runs:?}");
    observe_runs(
        &dispatcher(store.clone()),
        &store,
        &runs.iter().map(|run| run.id.clone()).collect::<Vec<_>>(),
        Duration::from_secs(20),
    )
    .await;

    let runs = store.runs(Some(&definition.id), 10).await.unwrap();
    assert_eq!(runs.len(), 3, "{runs:?}");
    assert!(
        runs.iter().all(|run| run.state == RunState::Succeeded),
        "{runs:?} ({first_summary:?} {second_summary:?})"
    );
    let sessions: std::collections::HashSet<_> = runs
        .iter()
        .map(|run| run.session_id.clone().expect("every run has a session"))
        .collect();
    assert_eq!(sessions.len(), 3, "runs shared an execution context");

    let pids: std::collections::HashSet<_> = started.iter().map(|path| marker_pid(path)).collect();
    assert_eq!(pids.len(), 3, "runs shared a shell process");
    for marker in &started {
        assert!(marker_has_finish(marker), "{marker:?} did not finish");
        assert_eq!(
            marker_lines(marker)
                .iter()
                .filter(|line| line.starts_with("start "))
                .count(),
            1,
            "{marker:?}"
        );
    }

    // Independent settlement: every run owns exactly one execution in its own
    // session, matched by delivery identity.
    let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    for run in &runs {
        let session = run.session_id.clone().unwrap();
        let origin = CommandId::new(format!("schedule-run-{}", run.id));
        let snapshot = client.snapshot(&session).await.unwrap();
        let shells: Vec<_> = snapshot
            .user_shells
            .iter()
            .filter(|shell| shell.origin_command_id.as_ref() == Some(&origin))
            .collect();
        assert_eq!(shells.len(), 1, "run {} owns one execution", run.id);
        assert_eq!(
            Some(shells[0].id.as_str().to_string()),
            run.execution_id.clone()
        );
        assert_eq!(shells[0].exit_code, run.exit_code);
    }

    let occurrences = store.occurrences(&definition.id, 10).await.unwrap();
    assert_eq!(occurrences.len(), 3);
    assert!(
        occurrences
            .iter()
            .all(|occurrence| occurrence.state == OccurrenceState::Completed),
        "{occurrences:?}"
    );

    shutdown.cancel();
    let _ = serve.await;
    let _ = provider.request_count();
}

/// The first execution parks on the gate; a later execution that finds
/// `markers/fail-next` consumes it and exits non-zero instead. Used to prove
/// one run's failure does not disturb a concurrent run.
const FAILABLE_SHELL: &str = "mkdir -p markers; f=\"markers/shell-$$.log\"; \
printf 'start pid=%s at=%s\\n' \"$$\" \"$(date -u +%s)\" > \"$f\"; \
if [ -f markers/fail-next ]; then rm -f markers/fail-next; \
printf 'finish pid=%s at=%s exit=3\\n' \"$$\" \"$(date -u +%s)\" >> \"$f\"; exit 3; fi; \
if [ ! -f markers/first ]; then : > markers/first; \
while [ ! -f markers/gate ]; do sleep 0.05; done; fi; \
printf 'finish pid=%s at=%s\\n' \"$$\" \"$(date -u +%s)\" >> \"$f\"";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_overlapping_run_failing_does_not_disturb_the_other() {
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
            name: "failing concurrent shell".into(),
            workspace: workspace.clone(),
            action: ScheduleAction::Shell {
                command: FAILABLE_SHELL.into(),
            },
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            session_policy: SessionPolicy::Fresh,
            overlap: OverlapPolicy::AllowConcurrent,
            mode: PermissionProfile::Assisted,
            model: None,
            timeout: Duration::from_secs(30),
            misfire: leveler_schedule::MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            unattended: Some(UnattendedRequest {
                allow_external_side_effects: true,
            }),
        })
        .await
        .unwrap();

    let connector: Arc<dyn RuntimeConnector> = Arc::new(SocketConnector {
        socket: socket.clone(),
    });
    let dispatcher = |store: ScheduleStore| {
        Arc::new(
            Dispatcher::new(store, connector.clone())
                .with_lease(Duration::from_secs(30))
                .with_poll_interval(Duration::from_millis(50)),
        )
    };
    let markers = workspace.join("markers");
    let first_pass = {
        let dispatcher = dispatcher(store.clone());
        tokio::spawn(async move { dispatcher.dispatch_due(10).await })
    };
    assert!(
        wait_for_file(&markers.join("first"), Duration::from_secs(10)).await,
        "run A never started"
    );
    let run_a = store
        .runs(Some(&definition.id), 10)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("run A row");
    let marker_a = wait_for_marker(&markers, Duration::from_secs(5))
        .await
        .expect("run A wrote its start marker");

    // The next run consumes the failure flag and exits 3 while A is parked.
    std::fs::write(markers.join("fail-next"), "").unwrap();
    store.create_run_now(&definition).await.unwrap();
    let second_pass = {
        let dispatcher = dispatcher(store.clone());
        tokio::spawn(async move { dispatcher.dispatch_due(10).await })
    };
    let second_summary = second_pass
        .await
        .expect("run B task panicked")
        .expect("run B pass failed");
    let run_b = store
        .runs(Some(&definition.id), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|run| run.id != run_a.id)
        .expect("run B row");
    observe_runs(
        &dispatcher(store.clone()),
        &store,
        &[run_b.id.clone()],
        Duration::from_secs(15),
    )
    .await;
    let run_b = store.run(&run_b.id).await.unwrap().unwrap();
    assert_eq!(
        run_b.state,
        RunState::Failed,
        "{run_b:?} {second_summary:?}"
    );
    assert_eq!(run_b.exit_code, Some(3));
    assert!(
        !marker_has_finish(&marker_a),
        "run A finished before the gate; overlap was not observed"
    );

    std::fs::write(markers.join("gate"), "").unwrap();
    let first_summary = first_pass
        .await
        .expect("run A task panicked")
        .expect("run A pass failed");
    observe_runs(
        &dispatcher(store.clone()),
        &store,
        &[run_a.id.clone()],
        Duration::from_secs(20),
    )
    .await;
    let run_a = store.run(&run_a.id).await.unwrap().unwrap();
    assert_eq!(
        run_a.state,
        RunState::Succeeded,
        "{run_a:?} {first_summary:?}"
    );
    assert_ne!(run_a.session_id, run_b.session_id);

    shutdown.cancel();
    let _ = serve.await;
    let _ = provider.request_count();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allow_concurrent_shell_runs_execute_in_independent_sessions() {
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
            name: "concurrent shell".into(),
            workspace: workspace.clone(),
            action: ScheduleAction::Shell {
                command: SHELL.into(),
            },
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            // The runtime cannot admit two turns in one session; a concurrent
            // schedule must get one execution context per run.
            session_policy: SessionPolicy::Fresh,
            overlap: OverlapPolicy::AllowConcurrent,
            mode: PermissionProfile::Assisted,
            model: None,
            // Long enough to cover the B-marker observation window below, so
            // a gated run is never settled by its own timeout.
            timeout: Duration::from_secs(20),
            misfire: leveler_schedule::MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            unattended: Some(UnattendedRequest {
                allow_external_side_effects: true,
            }),
        })
        .await
        .unwrap();

    let connector: Arc<dyn RuntimeConnector> = Arc::new(SocketConnector {
        socket: socket.clone(),
    });
    let dispatcher_a = Arc::new(
        Dispatcher::new(store.clone(), connector.clone())
            .with_lease(Duration::from_secs(30))
            .with_poll_interval(Duration::from_millis(50)),
    );
    let dispatcher_b = Arc::new(
        Dispatcher::new(store.clone(), connector.clone())
            .with_lease(Duration::from_secs(30))
            .with_poll_interval(Duration::from_millis(50)),
    );

    let markers = workspace.join("markers");

    // Run A claims the once slot and starts executing.
    let first_pass = {
        let dispatcher = dispatcher_a.clone();
        tokio::spawn(async move { dispatcher.dispatch_due(10).await })
    };
    assert!(
        wait_for_file(&markers.join("first"), Duration::from_secs(10)).await,
        "run A never started: the dispatcher did not deliver the shell command"
    );
    // A is the only run so far, so this is unambiguously its marker.
    let marker_a = wait_for_marker(&markers, Duration::from_secs(5))
        .await
        .expect("run A wrote its start marker");
    let runs_after_a = store.runs(Some(&definition.id), 10).await.unwrap();
    let run_a = runs_after_a
        .iter()
        .find(|run| run.state == RunState::Running || run.state == RunState::Dispatching)
        .or_else(|| runs_after_a.first())
        .expect("run A must exist after its command started")
        .clone();

    // Run B is a second occurrence of the same definition, claimed while A is
    // still parked on the gate.
    store.create_run_now(&definition).await.unwrap();
    let second_pass = {
        let dispatcher = dispatcher_b.clone();
        tokio::spawn(async move { dispatcher.dispatch_due(10).await })
    };

    // B either writes its own marker (the fixed contract) or never runs.
    let markers_when_b_should_have_started =
        wait_for_marker_count(&markers, 2, Duration::from_secs(5)).await;
    let marker_b = markers_when_b_should_have_started
        .iter()
        .find(|path| path.as_path() != marker_a.as_path())
        .cloned();
    if let Some(b) = &marker_b {
        // Wait for B to finish while A is still gated; the gate is written
        // only after this observation.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !marker_has_finish(b) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let a_finished_before_gate = marker_has_finish(&marker_a);
    std::fs::write(markers.join("gate"), "").unwrap();

    let first_summary = first_pass
        .await
        .expect("run A task panicked")
        .expect("run A pass failed");
    let second_summary = second_pass
        .await
        .expect("run B task panicked")
        .expect("run B pass failed");
    let pending_ids = store
        .runs(Some(&definition.id), 10)
        .await
        .unwrap()
        .into_iter()
        .map(|run| run.id)
        .collect::<Vec<_>>();
    observe_runs(&dispatcher_a, &store, &pending_ids, Duration::from_secs(20)).await;

    let runs = store.runs(Some(&definition.id), 10).await.unwrap();
    let run_a = store.run(&run_a.id).await.unwrap().expect("run A row");
    let run_b = runs
        .iter()
        .find(|run| run.id != run_a.id)
        .expect("run B must have its own run row")
        .clone();
    let run_b = store.run(&run_b.id).await.unwrap().expect("run B row");

    assert_eq!(
        run_a.state,
        RunState::Succeeded,
        "run A did not succeed: {run_a:?} (pass {first_summary:?})"
    );
    assert_eq!(
        run_b.state,
        RunState::Succeeded,
        "run B did not execute: {run_b:?} (pass {second_summary:?})"
    );
    assert_eq!(run_a.exit_code, Some(0));
    assert_eq!(run_b.exit_code, Some(0));

    let session_a = run_a.session_id.clone().expect("run A session");
    let session_b = run_b.session_id.clone().expect("run B session");
    assert_ne!(
        session_a, session_b,
        "two active runs shared one runtime session"
    );
    assert!(
        store
            .definition(&definition.id)
            .await
            .unwrap()
            .unwrap()
            .session_id
            .is_none(),
        "a fresh-per-run definition must not bind a durable session"
    );

    let marker_b = marker_b.unwrap_or_else(|| {
        panic!(
            "run B never wrote a marker; expected a second shell process. markers: {:?}",
            markers_when_b_should_have_started
        )
    });
    assert!(
        !a_finished_before_gate,
        "run A finished before the gate; overlap was not observed"
    );
    assert!(marker_has_finish(&marker_a), "run A has no finish line");
    assert!(marker_has_finish(&marker_b), "run B has no finish line");
    assert_ne!(
        marker_pid(&marker_a),
        marker_pid(&marker_b),
        "both runs used the same shell process"
    );
    assert_eq!(
        marker_lines(&marker_a)
            .iter()
            .filter(|line| line.starts_with("start "))
            .count(),
        1
    );
    assert_eq!(
        marker_lines(&marker_b)
            .iter()
            .filter(|line| line.starts_with("start "))
            .count(),
        1
    );

    // Each run's delivery identity owns exactly one execution in exactly one
    // session, and that execution names the marker file this run wrote.
    let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    let shells_for = |snapshot: &leveler_client_protocol::UiSessionSnapshot,
                      run_id: &leveler_schedule::RunId| {
        let origin = CommandId::new(format!("schedule-run-{run_id}"));
        snapshot
            .user_shells
            .iter()
            .filter(|shell| shell.origin_command_id.as_ref() == Some(&origin))
            .cloned()
            .collect::<Vec<_>>()
    };
    let snapshot_a = client.snapshot(&session_a).await.unwrap();
    let snapshot_b = client.snapshot(&session_b).await.unwrap();
    let shells_a = shells_for(&snapshot_a, &run_a.id);
    let shells_b = shells_for(&snapshot_b, &run_b.id);
    assert_eq!(shells_a.len(), 1, "run A owns exactly one execution");
    assert_eq!(shells_b.len(), 1, "run B owns exactly one execution");
    assert_eq!(
        Some(shells_a[0].id.as_str().to_string()),
        run_a.execution_id.clone()
    );
    assert_eq!(
        Some(shells_b[0].id.as_str().to_string()),
        run_b.execution_id.clone()
    );
    assert_eq!(shells_a[0].status, "success");
    assert_eq!(shells_b[0].status, "success");
    assert!(
        shells_a[0]
            .output_tail
            .contains(marker_a.file_name().unwrap().to_str().unwrap()),
        "run A's execution does not name its marker: {:?}",
        shells_a[0].output_tail
    );
    assert!(
        shells_b[0]
            .output_tail
            .contains(marker_b.file_name().unwrap().to_str().unwrap()),
        "run B's execution does not name its marker: {:?}",
        shells_b[0].output_tail
    );

    let occurrences = store.occurrences(&definition.id, 10).await.unwrap();
    assert_eq!(occurrences.len(), 2);
    assert!(
        occurrences
            .iter()
            .all(|occurrence| occurrence.state == OccurrenceState::Completed),
        "{occurrences:?}"
    );

    shutdown.cancel();
    let _ = serve.await;
    let _ = provider.request_count();
}

/// One line. Timestamps are fractional seconds so two starts in the same
/// wall-clock second are still ordered.
const TIMED_SLEEP: &str = "mkdir -p markers; f=\"markers/shell-$$.log\"; \
ts=$(perl -MTime::HiRes=time -e 'printf \"%.3f\", time'); \
printf 'start pid=%s at=%s\\n' \"$$\" \"$ts\" > \"$f\"; \
sleep 5; \
ts=$(perl -MTime::HiRes=time -e 'printf \"%.3f\", time'); \
printf 'finish pid=%s at=%s\\n' \"$$\" \"$ts\" >> \"$f\"";

struct Live {
    tmp: tempfile::TempDir,
    workspace: PathBuf,
    store: ScheduleStore,
    connector: Arc<dyn RuntimeConnector>,
    shutdown: CancellationToken,
    serve: tokio::task::JoinHandle<Result<(), leveler_local_transport::TransportError>>,
    provider: MockServer,
}

impl Live {
    async fn start() -> Self {
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
        let server = LocalSocketServer::bind_with_waiters(&socket, runtime, LocalWaiters::new())
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let serve = tokio::spawn(server.serve(shutdown.clone()));
        let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
            .await
            .unwrap();
        let connector: Arc<dyn RuntimeConnector> = Arc::new(SocketConnector { socket });
        Self {
            tmp,
            workspace,
            store,
            connector,
            shutdown,
            serve,
            provider,
        }
    }

    fn dispatcher(&self) -> Dispatcher {
        Dispatcher::new(self.store.clone(), self.connector.clone())
            .with_lease(Duration::from_secs(30))
            .with_poll_interval(Duration::from_millis(50))
    }

    async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.serve.await;
        let _ = self.provider.request_count();
        drop(self.tmp);
    }
}

async fn every_shell(
    live: &Live,
    name: &str,
    overlap: OverlapPolicy,
    session_policy: SessionPolicy,
) -> leveler_schedule::ScheduleDefinition {
    let anchor = leveler_core::now() - chrono::Duration::milliseconds(200);
    live.store
        .create_definition(NewScheduleDefinition {
            name: name.into(),
            workspace: live.workspace.clone(),
            action: ScheduleAction::Shell {
                command: TIMED_SLEEP.into(),
            },
            trigger: ScheduleTrigger::Every {
                anchor,
                every: Duration::from_secs(1),
            },
            session_policy,
            overlap,
            mode: PermissionProfile::Assisted,
            model: None,
            timeout: Duration::from_secs(30),
            misfire: leveler_schedule::MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(30),
            unattended: Some(UnattendedRequest {
                allow_external_side_effects: true,
            }),
        })
        .await
        .unwrap()
}

async fn wait_until_a_slot_is_due(store: &ScheduleStore, id: &leveler_schedule::ScheduleId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let occurrences = store.occurrences(id, 10).await.unwrap();
        let due = occurrences.iter().any(|occurrence| {
            occurrence.state == OccurrenceState::Pending
                && occurrence.scheduled_for <= leveler_core::now()
        });
        if due {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no natural slot became due: {occurrences:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_finish(path: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !marker_has_finish(path) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "marker never finished: {path:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The next grid slot is claimed by a later pass while the first shell is
/// still sleeping. Nothing calls run-now.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn natural_interval_allow_concurrent_overlaps_without_run_now() {
    let live = Live::start().await;
    let definition = every_shell(
        &live,
        "natural allow",
        OverlapPolicy::AllowConcurrent,
        SessionPolicy::Fresh,
    )
    .await;
    let dispatcher = live.dispatcher();
    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.claimed, 1, "{summary:?}");
    assert_eq!(summary.handed_off, 1, "{summary:?}");
    let markers = live.workspace.join("markers");
    let marker_a = wait_for_marker(&markers, Duration::from_secs(10))
        .await
        .expect("the first natural shell started");
    wait_until_a_slot_is_due(&live.store, &definition.id).await;
    assert!(
        !marker_has_finish(&marker_a),
        "the first shell finished before the next natural slot"
    );
    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(
        summary.claimed, 1,
        "the next grid slot was not claimed while the first shell was running: {summary:?}"
    );
    live.store.set_enabled(&definition.id, false).await.unwrap();
    let started = wait_for_marker_count(&markers, 2, Duration::from_secs(10)).await;
    assert_eq!(
        started.len(),
        2,
        "the second natural shell did not start: {started:?}"
    );
    let marker_b = started
        .iter()
        .find(|path| path.as_path() != marker_a.as_path())
        .expect("second marker");
    assert!(
        !marker_has_finish(&marker_a),
        "the first shell had already finished when the second started"
    );
    let runs = live.store.runs(Some(&definition.id), 10).await.unwrap();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_ne!(runs[0].session_id, runs[1].session_id);
    assert_ne!(runs[0].execution_id, runs[1].execution_id);
    let ids = runs.iter().map(|run| run.id.clone()).collect::<Vec<_>>();
    observe_runs(&dispatcher, &live.store, &ids, Duration::from_secs(20)).await;
    wait_for_finish(&marker_a).await;
    wait_for_finish(marker_b).await;
    let start_a = marker_stamp(&marker_a, "start ");
    let start_b = marker_stamp(marker_b, "start ");
    let finish_a = marker_stamp(&marker_a, "finish ");
    assert!(
        start_a < start_b && start_b < finish_a,
        "expected start(A) < start(B) < finish(A), got {start_a} {start_b} {finish_a}"
    );
    let runs = live.store.runs(Some(&definition.id), 10).await.unwrap();
    assert!(
        runs.iter().all(|run| run.state == RunState::Succeeded),
        "{runs:?}"
    );
    assert_eq!(
        runs.iter().filter(|run| run.exit_code == Some(0)).count(),
        2
    );
    live.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn natural_interval_skip_if_running_records_the_skipped_slot() {
    let live = Live::start().await;
    let definition = every_shell(
        &live,
        "natural skip",
        OverlapPolicy::SkipIfRunning,
        SessionPolicy::Continue,
    )
    .await;
    let dispatcher = live.dispatcher();
    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.handed_off, 1, "{summary:?}");
    let markers = live.workspace.join("markers");
    let marker_a = wait_for_marker(&markers, Duration::from_secs(10))
        .await
        .expect("the first shell started");
    wait_until_a_slot_is_due(&live.store, &definition.id).await;
    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.claimed, 0, "{summary:?}");
    assert!(summary.skipped >= 1, "{summary:?}");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let started = wait_for_marker_count(&markers, 1, Duration::from_secs(2)).await;
    assert_eq!(
        started.len(),
        1,
        "skip-if-running started a second shell: {started:?}"
    );
    let runs = live.store.runs(Some(&definition.id), 10).await.unwrap();
    assert!(
        runs.iter().any(|run| run.state == RunState::Skipped),
        "the due slot was not recorded skipped: {runs:?}"
    );
    live.store.set_enabled(&definition.id, false).await.unwrap();
    let active = runs
        .iter()
        .find(|run| !run.state.is_terminal())
        .expect("the first run is still active")
        .id
        .clone();
    observe_runs(&dispatcher, &live.store, &[active], Duration::from_secs(20)).await;
    wait_for_finish(&marker_a).await;
    let started = wait_for_marker_count(&markers, 1, Duration::from_secs(1)).await;
    assert_eq!(
        started.len(),
        1,
        "a skipped trigger still produced a side effect"
    );
    let runs = live.store.runs(Some(&definition.id), 10).await.unwrap();
    assert_eq!(
        runs.iter()
            .filter(|run| run.state == RunState::Succeeded)
            .count(),
        1,
        "{runs:?}"
    );
    live.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn natural_interval_queue_one_runs_the_single_waiting_slot_after_the_active_run() {
    let live = Live::start().await;
    let definition = every_shell(
        &live,
        "natural queue",
        OverlapPolicy::QueueOne,
        SessionPolicy::Continue,
    )
    .await;
    let dispatcher = live.dispatcher();
    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.handed_off, 1, "{summary:?}");
    let markers = live.workspace.join("markers");
    let marker_a = wait_for_marker(&markers, Duration::from_secs(10))
        .await
        .expect("the first shell started");
    let run_a = live
        .store
        .runs(Some(&definition.id), 10)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("run A");
    wait_until_a_slot_is_due(&live.store, &definition.id).await;
    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.claimed, 0, "{summary:?}");
    let pending = live
        .store
        .occurrences(&definition.id, 10)
        .await
        .unwrap()
        .into_iter()
        .filter(|occurrence| occurrence.state == OccurrenceState::Pending)
        .count();
    assert_eq!(pending, 1, "queue-one kept more than one waiting slot");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        wait_for_marker_count(&markers, 1, Duration::from_secs(1))
            .await
            .len(),
        1,
        "queue-one started a second shell while the first was active"
    );
    wait_for_finish(&marker_a).await;
    // One pass both settles the finished shell and claims the waiting slot.
    // The claim must not be stamped with the clock captured before that settle.
    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert!(
        summary.succeeded >= 1,
        "the active run did not settle in the claiming pass: {summary:?}"
    );
    assert_eq!(
        summary.claimed, 1,
        "the queued slot did not run: {summary:?}"
    );
    live.store.set_enabled(&definition.id, false).await.unwrap();
    let started = wait_for_marker_count(&markers, 2, Duration::from_secs(10)).await;
    assert_eq!(started.len(), 2, "{started:?}");
    let marker_b = started
        .iter()
        .find(|path| path.as_path() != marker_a.as_path())
        .expect("queued shell marker");
    let run_b = live
        .store
        .runs(Some(&definition.id), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|run| run.id != run_a.id && run.state != RunState::Skipped)
        .expect("queued run");
    let settled_a = live.store.run(&run_a.id).await.unwrap().expect("run A row");
    let finished_a = settled_a
        .finished_at
        .expect("run A settled before the queued claim");
    assert!(
        run_b.started_at >= finished_a,
        "the queued run is stamped before the active run settled: started {} finished {}",
        run_b.started_at,
        finished_a
    );
    observe_runs(
        &dispatcher,
        &live.store,
        &[run_b.id.clone()],
        Duration::from_secs(20),
    )
    .await;
    wait_for_finish(marker_b).await;
    let finish_a = marker_stamp(&marker_a, "finish ");
    let start_b = marker_stamp(marker_b, "start ");
    assert!(
        finish_a < start_b,
        "the queued shell started before the active shell finished: {finish_a} {start_b}"
    );
    let runs = live.store.runs(Some(&definition.id), 10).await.unwrap();
    assert_eq!(
        runs.iter()
            .filter(|run| run.state == RunState::Succeeded)
            .count(),
        2,
        "{runs:?}"
    );
    assert_eq!(
        wait_for_marker_count(&markers, 2, Duration::from_secs(1))
            .await
            .len(),
        2,
        "queue-one executed more than the one waiting slot"
    );
    live.stop().await;
}
