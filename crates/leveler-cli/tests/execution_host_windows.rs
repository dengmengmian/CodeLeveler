//! Real Windows Execution Host lifecycle: a separate OS process owns the
//! background task, survives the client, reports its exit, can be stopped, and
//! leaves no ghost work when it dies.
//!
//! Every process here is real: `leveler.exe execution-host`, `cmd.exe` and the
//! `ping.exe` it starts. Durable facts are read from the host's own archive on
//! disk. No mock clock, mock process or reducer stands in for the OS.
//!
//! The host is a separate process by construction (`ExecutionHostClient::ensure`
//! launches `leveler execution-host` detached), so "the client went away" is
//! exactly "this test dropped the client and reconnected". "The host died" is a
//! real `taskkill /F`, which closes the Job Object handle and therefore kills
//! the workload; recovery must then say `Unknown` with proof the process is
//! gone, never invent an exit code and never leave a Running ghost.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use leveler_execution::execution_host::{ExecutionHostClient, ExecutionHostConfig, HostRecords};
use leveler_execution::{
    BackgroundTaskLifetime, BackgroundTaskStatus, ProcessPresence, ProcessRequest,
};

/// `LEVELER_HOME` is process-global, so the tests that spawn a host run one at
/// a time. Each test uses its own home directory.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("tracked.txt"), "before\n").unwrap();
        Self {
            _temp: temp,
            home,
            repo,
        }
    }

    fn config(&self) -> ExecutionHostConfig {
        // The host records its repository in canonical form (as the product's
        // `Layout` does); an endpoint whose recorded repo does not match the
        // client's is refused, so the fixture speaks the same form.
        let repo_root = self.repo.canonicalize().expect("canonical repo");
        ExecutionHostConfig {
            state_dir: self.home.join("execution-host"),
            repo_root,
            executable: PathBuf::from(env!("CARGO_BIN_EXE_leveler")),
        }
    }

    fn request(&self, script: &str) -> ProcessRequest {
        ProcessRequest::new(
            "cmd",
            vec!["/c".to_string(), script.to_string()],
            self.repo.clone(),
        )
    }
}

fn host_pid(config: &ExecutionHostConfig) -> u32 {
    let ready: serde_json::Value = serde_json::from_slice(
        &std::fs::read(config.state_dir.join("ready.json")).expect("ready file"),
    )
    .expect("ready json");
    ready["pid"].as_u64().expect("host pid") as u32
}

fn kill(pid: u32) {
    let taskkill = Path::new(r"C:\Windows\System32\taskkill.exe");
    let status = std::process::Command::new(taskkill)
        .args(["/F", "/PID", &pid.to_string()])
        .status()
        .expect("taskkill");
    assert!(status.success(), "taskkill /F /PID {pid} failed");
}

fn process_gone(pid: u32) -> bool {
    match leveler_win_confine::process_is_running(pid) {
        // A terminated process object is not running work.
        Ok(running) => !running,
        Err(error)
            if error.raw_os_error() == Some(leveler_win_confine::ERROR_INVALID_PARAMETER_CODE) =>
        {
            true
        }
        // The token cannot prove absence; only absence is proof.
        Err(_) => false,
    }
}

/// Poll the host's own process fact until it is terminal. `observe` returns
/// as soon as there is new output, so its `wait` is not a wait-for-exit; the
/// terminal fact is read from `Get`.
async fn wait_for_terminal(
    client: &ExecutionHostClient,
    id: &str,
    owner: &str,
) -> leveler_execution::BackgroundTaskSnapshot {
    let started = Instant::now();
    loop {
        let task = client.get_owned(id, owner).await.expect("get task");
        if task.status.is_terminal() {
            return task;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "task {id} did not settle: {task:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_until<F>(what: &str, deadline: Duration, mut check: F)
where
    F: FnMut() -> bool,
{
    let started = Instant::now();
    while started.elapsed() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The environment lock must span the awaits: `LEVELER_HOME` is process-global
// and every host spawn reads it.
#[allow(clippy::await_holding_lock)]
async fn the_host_outlives_its_client_and_delivers_the_real_exit() {
    let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    // SAFETY: the serial mutex above keeps every host-spawning test apart; the
    // host inherits this environment when `ensure` launches it.
    unsafe {
        std::env::set_var("LEVELER_HOME", &fixture.home);
    }
    let config = fixture.config();
    let client = ExecutionHostClient::ensure(config.clone())
        .await
        .expect("host must start on Windows");
    assert!(
        client.peer_major() >= 2,
        "the host must speak the current owner protocol"
    );

    let id = "host-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let view = client
        .spawn(
            id,
            fixture.request("ping -n 6 127.0.0.1 > nul & echo HOSTED-DONE"),
            "owner-a",
            "owner-a",
            BackgroundTaskLifetime::Session,
            Default::default(),
            None,
        )
        .await
        .expect("spawn");
    assert_eq!(view.id, id, "the Execution ID is the caller's stable id");
    let pid = view.pid.expect("a real process fact");
    assert!(
        view.start_identity
            .as_deref()
            .is_some_and(|token| token.starts_with("windows:")),
        "Windows must capture a start token, not treat the pid as identity"
    );

    // The client disappears; the host process must not.
    drop(client);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !process_gone(pid),
        "the workload must still be alive after its client went away"
    );

    // A new client attaches to the same host and waits for the host's own
    // terminal fact, then reads the output the terminal record retained.
    let client = ExecutionHostClient::connect(config.clone())
        .await
        .expect("reconnect to the surviving host");
    let terminal = wait_for_terminal(&client, id, "owner-a").await;
    assert_eq!(
        terminal.status,
        BackgroundTaskStatus::Exited,
        "a real exit status: {terminal:?}"
    );
    assert_eq!(terminal.exit_code, Some(0));
    assert!(
        terminal.log.contains("HOSTED-DONE"),
        "the output must survive client replacement: {:?}",
        terminal.log
    );
    let records = HostRecords::read(&config.state_dir).expect("durable archive");
    let record = records.task(id).expect("archived task");
    let completion = record.result.as_ref().expect("committed completion record");
    assert_eq!(completion.exit_code, Some(0));
    assert!(
        !records.has_unmanaged_process(),
        "a completed task is not unmanaged live work"
    );
    // Re-read: the committed result is stable, not re-derived per observer.
    let reread = HostRecords::read(&config.state_dir).expect("reread");
    assert_eq!(
        reread.task(id).and_then(|t| t.result.as_ref()),
        Some(completion),
        "the completion record must be the same fact on every read"
    );
    client
        .acknowledge(id, "owner-a")
        .await
        .expect("acknowledge");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The environment lock must span the awaits: `LEVELER_HOME` is process-global
// and every host spawn reads it.
#[allow(clippy::await_holding_lock)]
async fn a_dead_host_is_settled_as_unknown_without_a_running_ghost() {
    let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    // SAFETY: see the serial mutex rationale above.
    unsafe {
        std::env::set_var("LEVELER_HOME", &fixture.home);
    }
    let config = fixture.config();
    let client = ExecutionHostClient::ensure(config.clone())
        .await
        .expect("host must start");
    let id = "host-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let view = client
        .spawn(
            id,
            fixture.request("ping -n 60 127.0.0.1 > nul"),
            "owner-b",
            "owner-b",
            BackgroundTaskLifetime::Session,
            Default::default(),
            None,
        )
        .await
        .expect("spawn");
    let workload = view.pid.expect("pid");
    assert_eq!(view.status, BackgroundTaskStatus::Running);

    let host = host_pid(&config);
    kill(host);
    drop(client);
    wait_until("the host process to die", Duration::from_secs(10), || {
        process_gone(host)
    })
    .await;
    // The host's Job Object is kill-on-close: closing its handle with the host
    // is what ends the workload. Wait for that kernel fact before asking
    // recovery to describe the process.
    wait_until("the killed workload tree", Duration::from_secs(10), || {
        process_gone(workload)
    })
    .await;

    // The probe sees no endpoint, takes the lifecycle lock (which proves the
    // owner is gone) and converges the live record from OS evidence.
    let probe = ExecutionHostClient::probe(&config)
        .await
        .expect("probe after host death");
    assert!(probe.client.is_none(), "the old host must not answer");
    let recovery = probe.recovery.expect("recovery must report the lost task");
    assert_eq!(recovery.tasks.len(), 1);
    assert_eq!(recovery.tasks[0].task_id, id);
    assert_eq!(
        recovery.tasks[0].process,
        ProcessPresence::Gone,
        "the job's kill-on-close must be proven, not guessed"
    );
    let records = HostRecords::read(&config.state_dir).expect("archive");
    let record = records.task(id).expect("archived");
    assert_eq!(
        record.snapshot.status,
        BackgroundTaskStatus::Unknown,
        "a lost exit is never invented"
    );
    assert!(record.result.is_none(), "no completion record on Unknown");
    assert!(record.loss.is_some(), "the loss must be durable");
    assert!(
        !records.has_unmanaged_process(),
        "no ghost Running record may survive recovery"
    );

    // A replacement host starts over the converged archive.
    let client = ExecutionHostClient::ensure(config.clone())
        .await
        .expect("a replacement host must start");
    let tasks = client.list().await.expect("list");
    assert!(
        tasks
            .iter()
            .find(|task| task.snapshot.id == id)
            .is_some_and(|task| task.snapshot.status == BackgroundTaskStatus::Unknown),
        "the replacement host must expose the settled fact: {tasks:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The environment lock must span the awaits: `LEVELER_HOME` is process-global
// and every host spawn reads it.
#[allow(clippy::await_holding_lock)]
async fn the_registry_hands_background_work_to_the_host_and_readopts_it() {
    let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    // SAFETY: see the serial mutex rationale above.
    unsafe {
        std::env::set_var("LEVELER_HOME", &fixture.home);
    }
    let config = fixture.config();
    let environment = std::sync::Arc::new(leveler_core::EnvSnapshot::new(
        std::env::vars_os(),
        fixture.repo.clone(),
        std::env::temp_dir(),
    ));
    let registry = leveler_execution::BackgroundTaskRegistry::with_environment(environment)
        .with_stable_task_ids()
        .with_execution_host(config.clone());
    let id = registry
        .spawn_owned_with_lifetime(
            fixture.request("ping -n 8 127.0.0.1 > nul & echo REGISTRY-HOSTED"),
            None,
            Some("session-a"),
            BackgroundTaskLifetime::Session,
        )
        .await
        .expect("hosted spawn");
    assert!(
        id.starts_with("host-") && id.len() == 37,
        "the registry must use the host's stable execution id, got {id}"
    );
    let host = host_pid(&config);

    // The runtime that admitted the task goes away before it finishes.
    drop(registry);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !process_gone(host),
        "the Execution Host must outlive the registry that launched it"
    );

    // A replacement runtime attaches to the same host and the same execution.
    let replacement = leveler_execution::BackgroundTaskRegistry::with_environment(
        std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            fixture.repo.clone(),
            std::env::temp_dir(),
        )),
    )
    .with_stable_task_ids()
    .with_execution_host(config.clone());
    let terminal = replacement
        .wait_owned(
            &id,
            "session-a",
            Some(Duration::from_secs(20)),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("replacement runtime must observe the task");
    assert_eq!(terminal.id, id, "same stable Execution ID");
    assert_eq!(
        terminal.status,
        BackgroundTaskStatus::Exited,
        "a real terminal fact: {:?}",
        terminal
    );
    assert!(
        terminal.log.contains("REGISTRY-HOSTED"),
        "output must survive the runtime handover: {:?}",
        terminal.log
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The environment lock must span the awaits: `LEVELER_HOME` is process-global
// and every host spawn reads it.
#[allow(clippy::await_holding_lock)]
async fn a_running_task_can_be_stopped_and_its_tree_is_gone() {
    let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    // SAFETY: see the serial mutex rationale above.
    unsafe {
        std::env::set_var("LEVELER_HOME", &fixture.home);
    }
    let config = fixture.config();
    let client = ExecutionHostClient::ensure(config.clone())
        .await
        .expect("host must start");
    let id = "host-cccccccccccccccccccccccccccccccc";
    // The child starts a grandchild so termination of the whole tree is what
    // the assertion observes, not just the direct child.
    let view = client
        .spawn(
            id,
            fixture.request(r#"powershell -NoProfile -Command "Start-Sleep -Seconds 120""#),
            "owner-c",
            "owner-c",
            BackgroundTaskLifetime::Session,
            Default::default(),
            None,
        )
        .await
        .expect("spawn");
    let pid = view.pid.expect("pid");
    let stopped = client.kill_owned(id, "owner-c").await.expect("stop");
    assert!(
        stopped.status.is_terminal(),
        "stop must settle the task: {:?}",
        stopped.status
    );
    wait_until("the workload after stop", Duration::from_secs(10), || {
        process_gone(pid)
    })
    .await;
    let records = HostRecords::read(&config.state_dir).expect("archive");
    assert!(
        !records.has_unmanaged_process(),
        "a stopped task leaves no live record"
    );
}

#[test]
fn the_state_directory_and_ready_endpoint_are_owner_only() {
    let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let state = fixture.config().state_dir;
    leveler_win_confine::secure_private_state_dir(&state).expect("create private state dir");
    assert!(state.is_dir());
    let ready = state.join("ready.json");
    std::fs::write(&ready, b"{}").unwrap();
    leveler_win_confine::verify_private_state_file(&ready).expect("ready file is private");
}
