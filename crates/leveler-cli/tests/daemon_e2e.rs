//! Runtime P2A process-level E2E: the real `leveler` binary, real daemons,
//! real sockets, isolated `LEVELER_HOME`/`LEVELER_CONFIG_DIR` per test.
//!
//! - Scenario A: RuntimeId survives a clean daemon stop/restart.
//! - Scenario D: two daemons racing the same repository elect exactly one,
//!   and clients discover exactly one identity.
//! - Scenario E: SIGKILL during a running task; a restarted daemon reaps the
//!   orphan turn (no zombie `running` rows), the transcript survives without
//!   duplication, and the session remains openable.
//!
//! Terminal-rendering-level TUI automation is intentionally out of scope
//! (NOT VERIFIED IN THIS ENVIRONMENT); these tests prove the runtime facts
//! the TUI is a client of.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use leveler_client_protocol::{
    BuildIdentity, ClientCommand, ClientError, InteractiveRuntimeClient, RestartReason,
    RuntimeEvent, RuntimeHealth, RuntimeId, RuntimeInfo, SessionId, UiSessionSnapshot,
};
use leveler_local_transport::{
    CreateSessionRequest, LocalRuntimeService, LocalSocketRuntimeClient, LocalSocketServer,
    SessionBootstrap,
};
use leveler_project::Layout;
use leveler_runtime_host::{
    DaemonReviver, DetachedRuntimeLaunch, HandoffAction, HandoffEvent, HandoffUi,
    OwnedRuntimeLaunch, ensure_default_runtime, ensure_owned_runtime, probe_default_runtime,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
// Test-owned daemons run in their own process group and are reclaimed on drop,
// so a panic or a fired deadline cannot leave an orphan `leveler serve` behind.
use leveler_test_support::ManagedChild;

struct TestEnv {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    config_dir: PathBuf,
    repo: PathBuf,
}

/// A fully isolated environment: its own home (state, sockets), config
/// bundle (mock provider), and repository.
fn test_env(base_url: &str) -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    let config_dir = tmp.path().join("configs");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
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
    TestEnv {
        _tmp: tmp,
        home,
        config_dir,
        repo,
    }
}

fn spawn_serve(env: &TestEnv, ready: &Path) -> ManagedChild {
    let mut command = Command::new(env!("CARGO_BIN_EXE_leveler"));
    command
        .arg("--repo")
        .arg(&env.repo)
        .arg("serve")
        .arg("--ready-json")
        .arg(ready)
        .env("LEVELER_HOME", &env.home)
        .env("LEVELER_CONFIG_DIR", &env.config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ManagedChild::spawn(&mut command).expect("spawn leveler serve")
}

fn spawn_tcp_serve(env: &TestEnv, ready: &Path, token: &str) -> ManagedChild {
    let mut command = Command::new(env!("CARGO_BIN_EXE_leveler"));
    command
        .arg("--repo")
        .arg(&env.repo)
        .arg("serve")
        .arg("--tcp")
        .arg("127.0.0.1:0")
        .arg("--ready-json")
        .arg(ready)
        .env("LEVELER_HOME", &env.home)
        .env("LEVELER_CONFIG_DIR", &env.config_dir)
        .env("LEVELER_DAEMON_TOKEN", token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ManagedChild::spawn(&mut command).expect("spawn TCP leveler serve")
}

#[cfg(feature = "test-crash-barrier")]
fn spawn_serve_with_after_turn_started_barrier(
    env: &TestEnv,
    ready: &Path,
    barrier: &Path,
) -> ManagedChild {
    spawn_serve_with_barrier(
        env,
        ready,
        "LEVELER_TEST_AFTER_TURN_STARTED_BARRIER",
        barrier,
    )
}

#[cfg(feature = "test-crash-barrier")]
fn spawn_serve_with_barrier(
    env: &TestEnv,
    ready: &Path,
    barrier_var: &str,
    barrier: &Path,
) -> ManagedChild {
    let mut command = Command::new(env!("CARGO_BIN_EXE_leveler"));
    command
        .arg("--repo")
        .arg(&env.repo)
        .arg("serve")
        .arg("--ready-json")
        .arg(ready)
        .env("LEVELER_HOME", &env.home)
        .env("LEVELER_CONFIG_DIR", &env.config_dir)
        .env(barrier_var, barrier)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ManagedChild::spawn(&mut command).expect("spawn leveler serve with crash barrier")
}

/// Wait for the daemon's ready file; panics with the child's status on
/// premature exit so a startup failure is diagnosable.
fn wait_ready(ready: &Path, child: &mut ManagedChild, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        if ready.is_file()
            && let Ok(raw) = std::fs::read_to_string(ready)
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            return value;
        }
        if let Some(status) = child.try_wait().expect("child status") {
            panic!("daemon exited before readiness: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "daemon never became ready within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stop_daemon(child: &mut ManagedChild) {
    // SIGINT = the daemon's documented Ctrl+C shutdown path.
    unsafe {
        libc_kill(child.id() as i32, 2);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if child.try_wait().expect("child status").is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("daemon did not stop on SIGINT");
}

/// Minimal libc-free kill(2) via the `kill` binary would race PID reuse in
/// theory; direct syscall through std is not exposed, so shell out is the
/// pragmatic choice for a test.
unsafe fn libc_kill(pid: i32, signal: i32) {
    let _ = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status();
}

/// The single socket the environment's daemon listens on.
fn find_socket(env: &TestEnv) -> PathBuf {
    let sock_dir = env.home.join("run/sockets");
    let mut sockets: Vec<PathBuf> = std::fs::read_dir(&sock_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "sock"))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(sockets.len(), 1, "expected exactly one daemon socket");
    sockets.pop().unwrap()
}

/// The single per-repo state dir under the isolated home.
fn find_state_dir(env: &TestEnv) -> PathBuf {
    let projects = env.home.join("state/projects");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&projects)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(dirs.len(), 1, "expected exactly one project state dir");
    dirs.pop().unwrap()
}

struct SilentHandoffUi;

impl HandoffUi for SilentHandoffUi {
    fn emit(&self, _event: HandoffEvent) {}

    fn input(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<HandoffAction>> {
        None
    }
}

/// Only the old runtime's two handoff obligations are simulated. The Host,
/// Unix transport and replacement daemon all run their production paths.
struct OldBuildRuntime {
    build: BuildIdentity,
    commands: Arc<Mutex<Vec<RestartReason>>>,
    events: broadcast::Sender<RuntimeEvent>,
}

#[async_trait::async_trait]
impl InteractiveRuntimeClient for OldBuildRuntime {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        match command {
            ClientCommand::ShutdownWhenIdle { reason } => {
                self.commands.lock().unwrap().push(reason);
                Ok(())
            }
            other => Err(ClientError::Runtime(format!(
                "unexpected command to old runtime: {other:?}"
            ))),
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self, _session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        Err(ClientError::Runtime("old runtime has no sessions".into()))
    }
}

#[async_trait::async_trait]
impl LocalRuntimeService for OldBuildRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Err(ClientError::Runtime(
            "old runtime cannot create sessions".into(),
        ))
    }

    async fn runtime_info(&self) -> Result<RuntimeInfo, ClientError> {
        Ok(RuntimeInfo {
            runtime_id: RuntimeId::new("old-build-runtime"),
            version: self.build.version.clone(),
            build: self.build.clone(),
            config_fingerprint: None,
            pid: std::process::id(),
            health: RuntimeHealth {
                accepting_work: true,
                quiescent: true,
                ..RuntimeHealth::default()
            },
        })
    }
}

struct RetireOldBuildUi {
    commands: Arc<Mutex<Vec<RestartReason>>>,
    shutdown: CancellationToken,
}

impl HandoffUi for RetireOldBuildUi {
    fn emit(&self, event: HandoffEvent) {
        // A Status event proves that ShutdownWhenIdle was ACKed and the Host
        // observed the old runtime's drain state. Only then release its socket.
        if matches!(event, HandoffEvent::Status(_)) && !self.commands.lock().unwrap().is_empty() {
            self.shutdown.cancel();
        }
    }

    fn input(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<HandoffAction>> {
        None
    }
}

/// The Host starts a detached daemon, so the test records its PID before
/// `exec` and reclaims it even when a later assertion fails.
struct HostDaemonGuard {
    pid_file: PathBuf,
}

impl HostDaemonGuard {
    fn pid(&self) -> u32 {
        std::fs::read_to_string(&self.pid_file)
            .expect("Host daemon wrapper recorded a PID")
            .trim()
            .parse()
            .expect("Host daemon PID is numeric")
    }
}

impl Drop for HostDaemonGuard {
    fn drop(&mut self) {
        if let Ok(raw) = std::fs::read_to_string(&self.pid_file)
            && let Ok(pid) = raw.trim().parse::<u32>()
        {
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(pid.to_string())
                .output();
        }
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn detached_host_launch(env: &TestEnv) -> (DetachedRuntimeLaunch, HostDaemonGuard) {
    use std::os::unix::fs::PermissionsExt;

    let executable = env.home.join("leveler-test-wrapper.sh");
    let pid_file = env.home.join("host-daemon.pid");
    let script = format!(
        "#!/bin/sh\nprintf '%s' \"$$\" > {}\nexport LEVELER_HOME={}\nexport LEVELER_CONFIG_DIR={}\nexec {} \"$@\"\n",
        shell_quote(&pid_file),
        shell_quote(&env.home),
        shell_quote(&env.config_dir),
        shell_quote(Path::new(env!("CARGO_BIN_EXE_leveler"))),
    );
    std::fs::write(&executable, script).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    (
        DetachedRuntimeLaunch {
            executable,
            ready_prefix: "leveler-host-contract".to_string(),
        },
        HostDaemonGuard { pid_file },
    )
}

/// Scenario A: a daemon restart keeps the same RuntimeId; the id equals the
/// persisted state-dir identity.
#[test]
fn runtime_id_survives_a_daemon_restart() {
    let env = test_env("http://127.0.0.1:9");
    let ready1 = env.home.join("ready1.json");
    let mut daemon = spawn_serve(&env, &ready1);
    let first = wait_ready(&ready1, &mut daemon, Duration::from_secs(30));
    let first_id = first["runtime_id"]
        .as_str()
        .expect("runtime_id")
        .to_string();
    assert!(!first_id.is_empty());
    stop_daemon(&mut daemon);

    let ready2 = env.home.join("ready2.json");
    let mut daemon = spawn_serve(&env, &ready2);
    let second = wait_ready(&ready2, &mut daemon, Duration::from_secs(30));
    let second_id = second["runtime_id"]
        .as_str()
        .expect("runtime_id")
        .to_string();
    stop_daemon(&mut daemon);

    assert_eq!(
        first_id, second_id,
        "a restart must keep the runtime identity"
    );
    let persisted = std::fs::read_to_string(find_state_dir(&env).join("runtime-id")).unwrap();
    assert_eq!(persisted.trim(), first_id);
}

/// A supervised TCP daemon reports the original ready fields, accepts the
/// supplied bearer token, and keeps that secret out of process arguments.
#[test]
fn tcp_daemon_ready_contract_keeps_token_out_of_argv() {
    leveler_test_support::bounded_test(
        "tcp_daemon_ready_contract_keeps_token_out_of_argv",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        tcp_daemon_ready_contract_keeps_token_out_of_argv_body,
    );
}

/// The public Host API starts a real daemon when absent, reuses the existing
/// process, then revives the same client after that process dies.
#[test]
fn host_starts_adopts_and_revives_a_real_daemon() {
    leveler_test_support::bounded_test(
        "host_starts_adopts_and_revives_a_real_daemon",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        host_starts_adopts_and_revives_a_real_daemon_body,
    );
}

/// Web's owned-child policy returns a child only for the process it starts;
/// another project manager can attach to that same daemon without owning it.
#[test]
fn owned_host_starts_then_adopts_a_real_daemon() {
    leveler_test_support::bounded_test(
        "owned_host_starts_then_adopts_a_real_daemon",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        owned_host_starts_then_adopts_a_real_daemon_body,
    );
}

async fn owned_host_starts_then_adopts_a_real_daemon_body() {
    let env = test_env("http://127.0.0.1:9");
    let layout = Layout::ephemeral(env.repo.clone(), Some(env.config_dir.clone()), &env.home);
    let socket = layout.socket_path();
    let (launch, guard) = detached_host_launch(&env);
    let ready_path = env.home.join("owned-ready.json");
    assert!(!socket.exists(), "the first call starts without a daemon");

    let started = ensure_owned_runtime(
        &env.repo,
        &socket,
        Some(OwnedRuntimeLaunch {
            executable: &launch.executable,
            ready_path: &ready_path,
        }),
    )
    .await
    .expect("owned Host starts and connects to a real daemon");
    let mut child = started
        .child
        .expect("the caller owns the newly started child");
    let first = LocalRuntimeService::runtime_info(&started.client)
        .await
        .expect("ready socket serves the started runtime");
    assert_eq!(child.id(), Some(first.pid));
    assert_eq!(guard.pid(), first.pid);
    assert_eq!(find_socket(&env), socket);
    let ready_client = LocalSocketRuntimeClient::connect(&socket)
        .await
        .expect("ready socket accepts another client");
    assert_eq!(
        LocalRuntimeService::runtime_info(&ready_client)
            .await
            .unwrap()
            .runtime_id,
        first.runtime_id
    );

    // An unusable launcher proves that adoption does not start another child.
    let missing_executable = env.home.join("nonexistent-leveler");
    let adopted = ensure_owned_runtime(
        &env.repo,
        &socket,
        Some(OwnedRuntimeLaunch {
            executable: &missing_executable,
            ready_path: &env.home.join("must-not-be-written.json"),
        }),
    )
    .await
    .expect("owned Host adopts the existing daemon");
    assert!(
        adopted.child.is_none(),
        "adopting must not transfer child ownership"
    );
    let second = LocalRuntimeService::runtime_info(&adopted.client)
        .await
        .expect("adopted client reaches the same runtime");
    assert_eq!(second.pid, first.pid);
    assert_eq!(second.runtime_id, first.runtime_id);

    drop(adopted);
    drop(ready_client);
    drop(started.client);
    child.start_kill().expect("stop owned daemon");
    child.wait().await.expect("reap owned daemon");
    std::fs::remove_file(&guard.pid_file).unwrap();
}

/// A different reported build must receive ShutdownWhenIdle before a new
/// daemon can take the same repository socket and answer with the current build.
#[test]
fn host_retires_an_old_build_before_starting_a_replacement() {
    leveler_test_support::bounded_test(
        "host_retires_an_old_build_before_starting_a_replacement",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        host_retires_an_old_build_before_starting_a_replacement_body,
    );
}

async fn host_retires_an_old_build_before_starting_a_replacement_body() {
    let env = test_env("http://127.0.0.1:9");
    let layout = Layout::ephemeral(env.repo.clone(), Some(env.config_dir.clone()), &env.home);
    let (launch, guard) = detached_host_launch(&env);
    let mut old_build = BuildIdentity::current();
    old_build.fingerprint.push_str("-previous-build");
    let commands = Arc::new(Mutex::new(Vec::new()));
    let (events, _) = broadcast::channel(4);
    let old_runtime = Arc::new(OldBuildRuntime {
        build: old_build.clone(),
        commands: commands.clone(),
        events,
    });
    let shutdown = CancellationToken::new();
    let _shutdown_on_drop = shutdown.clone().drop_guard();
    let server = LocalSocketServer::bind(layout.socket_path(), old_runtime)
        .await
        .expect("old build owns the repository socket");
    let old_server = tokio::spawn(server.serve(shutdown.clone()));
    let ui: Arc<dyn HandoffUi> = Arc::new(RetireOldBuildUi {
        commands: commands.clone(),
        shutdown,
    });

    let client = tokio::time::timeout(
        Duration::from_secs(20),
        ensure_default_runtime(&layout, &launch, ui),
    )
    .await
    .expect("Host handoff completes")
    .expect("Host connects to replacement daemon");
    let old_server_result = old_server.await.expect("old server task joins");
    old_server_result.expect("old server retired cleanly");
    assert_eq!(
        *commands.lock().unwrap(),
        vec![RestartReason::BuildMismatch],
        "Host must retire the old build for the correct reason"
    );
    let replacement = LocalRuntimeService::runtime_info(&client).await.unwrap();
    assert_eq!(
        replacement.pid,
        guard.pid(),
        "replacement is a real child process"
    );
    assert_ne!(replacement.pid, std::process::id());
    assert!(
        replacement.build.matches(&BuildIdentity::current()),
        "replacement must report the current build"
    );
    assert_ne!(replacement.build, old_build);
}

async fn host_starts_adopts_and_revives_a_real_daemon_body() {
    let env = test_env("http://127.0.0.1:9");
    let layout = Layout::ephemeral(env.repo.clone(), Some(env.config_dir.clone()), &env.home);
    let (launch, guard) = detached_host_launch(&env);
    let ui: Arc<dyn HandoffUi> = Arc::new(SilentHandoffUi);
    assert!(
        probe_default_runtime(&layout.socket_path())
            .await
            .unwrap()
            .is_none(),
        "the first ensure must begin with no runtime"
    );

    let client = ensure_default_runtime(&layout, &launch, ui.clone())
        .await
        .expect("Host starts a daemon and connects");
    let first = LocalRuntimeService::runtime_info(&client).await.unwrap();
    assert_eq!(
        first.pid,
        guard.pid(),
        "Host must connect to its spawned process"
    );
    assert_eq!(find_socket(&env), layout.socket_path());

    // An invalid executable makes accidental replacement observable: adopt
    // must return the already-running daemon without trying to spawn.
    let adopt_only = DetachedRuntimeLaunch {
        executable: env.home.join("nonexistent-leveler"),
        ready_prefix: "must-not-launch".to_string(),
    };
    let adopted = ensure_default_runtime(&layout, &adopt_only, ui.clone())
        .await
        .expect("compatible runtime is adopted");
    let adopted_info = LocalRuntimeService::runtime_info(&adopted).await.unwrap();
    assert_eq!(adopted_info.pid, first.pid);
    assert_eq!(adopted_info.runtime_id, first.runtime_id);
    drop(adopted);

    let killed = Command::new("kill")
        .arg("-KILL")
        .arg(first.pid.to_string())
        .status()
        .expect("kill original daemon");
    assert!(killed.success(), "original daemon must be killed");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if tokio::net::UnixStream::connect(layout.socket_path())
                .await
                .is_err()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("killed daemon stops answering");

    client.set_reviver(Arc::new(DaemonReviver::new(layout.clone(), launch, ui)));
    let revived = LocalRuntimeService::runtime_info(&client)
        .await
        .expect("a safe read revives and retries through the same client");
    assert_ne!(
        revived.pid, first.pid,
        "revival must start a new OS process"
    );
    assert_eq!(
        revived.pid,
        guard.pid(),
        "Host wrapper records the revived process"
    );
    assert_eq!(
        revived.runtime_id, first.runtime_id,
        "runtime identity survives revival"
    );
}

async fn tcp_daemon_ready_contract_keeps_token_out_of_argv_body() {
    let env = test_env("http://127.0.0.1:9");
    let ready_path = env.home.join("tcp-ready.json");
    let token = "runtime-host-contract-token-supplied-through-environment";
    let mut daemon = spawn_tcp_serve(&env, &ready_path, token);
    let ready = wait_ready(&ready_path, &mut daemon, Duration::from_secs(30));

    assert_eq!(ready["pid"].as_u64(), Some(u64::from(daemon.id())));
    assert_eq!(
        ready["socket"].as_str(),
        Some(find_socket(&env).to_str().unwrap())
    );
    let addr: std::net::SocketAddr = ready["addr"]
        .as_str()
        .expect("TCP ready address")
        .parse()
        .expect("ready address is a socket address");
    assert!(addr.ip().is_loopback(), "TCP daemon must bind loopback");
    assert_ne!(addr.port(), 0, "ready address must expose the bound port");
    assert_eq!(ready["token"].as_str(), Some(token));
    let runtime_id = ready["runtime_id"].as_str().expect("runtime_id");
    assert!(!runtime_id.is_empty());

    // Inspect the running OS process. Checking only our Command builder would
    // not prove that the launched daemon kept the secret off its real argv.
    let ps = Command::new("ps")
        .args(["-p", &daemon.id().to_string(), "-o", "command="])
        .output()
        .expect("inspect daemon argv with ps");
    assert!(
        ps.status.success(),
        "ps failed: {}",
        String::from_utf8_lossy(&ps.stderr)
    );
    let argv = String::from_utf8(ps.stdout).expect("ps output is UTF-8");
    assert!(
        argv.contains("serve"),
        "ps must identify the daemon process: {argv}"
    );
    assert!(
        !argv.contains(token),
        "bearer token leaked into daemon argv"
    );

    let tcp_client = LocalSocketRuntimeClient::connect_tcp(addr, token)
        .await
        .expect("ready token connects to TCP daemon");
    let tcp_info = LocalRuntimeService::runtime_info(&tcp_client)
        .await
        .expect("TCP daemon serves runtime info");
    assert_eq!(tcp_info.runtime_id.as_str(), runtime_id);

    // The per-repo Unix socket remains live for an existing local client.
    let unix_client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .expect("local socket remains connectable");
    let unix_info = LocalRuntimeService::runtime_info(&unix_client)
        .await
        .expect("Unix daemon serves runtime info");
    assert_eq!(unix_info.runtime_id.as_str(), runtime_id);

    drop(tcp_client);
    drop(unix_client);
    stop_daemon(&mut daemon);
}

/// Scenario D: two daemons racing one repository — exactly one survives, and
/// the socket answers with exactly that identity.
#[test]
fn concurrent_daemon_starts_elect_exactly_one_runtime() {
    leveler_test_support::bounded_test(
        "concurrent_daemon_starts_elect_exactly_one_runtime",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        concurrent_daemon_starts_elect_exactly_one_runtime_body,
    );
}

async fn concurrent_daemon_starts_elect_exactly_one_runtime_body() {
    let env = test_env("http://127.0.0.1:9");
    let ready_a = env.home.join("ready-a.json");
    let ready_b = env.home.join("ready-b.json");
    let mut a = spawn_serve(&env, &ready_a);
    let mut b = spawn_serve(&env, &ready_b);

    // One contender must exit (the election loser); the other must be ready.
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut winner, winner_ready, loser_ready) = loop {
        let a_exit = a.try_wait().expect("a status");
        let b_exit = b.try_wait().expect("b status");
        match (a_exit, b_exit) {
            (Some(status), None) => {
                assert!(
                    !status.success(),
                    "the losing daemon must exit with an error, got {status}"
                );
                break (b, ready_b.clone(), ready_a.clone());
            }
            (None, Some(status)) => {
                assert!(!status.success());
                break (a, ready_a.clone(), ready_b.clone());
            }
            (Some(_), Some(_)) => panic!("both daemons exited; nobody won the election"),
            (None, None) => {
                assert!(
                    Instant::now() < deadline,
                    "election never settled: both daemons still alive"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };

    assert!(
        !loser_ready.exists(),
        "the losing process must not publish readiness as a second owner"
    );

    let ready = wait_ready(&winner_ready, &mut winner, Duration::from_secs(30));
    let winner_id = ready["runtime_id"]
        .as_str()
        .expect("runtime_id")
        .to_string();

    // Discovery: a client connecting to the repo's socket reaches exactly
    // the winner's identity, which is also the persisted one.
    let socket = find_socket(&env);
    let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    let info = LocalRuntimeService::runtime_info(&client).await.unwrap();
    assert_eq!(info.runtime_id.as_str(), winner_id);
    let persisted = std::fs::read_to_string(find_state_dir(&env).join("runtime-id")).unwrap();
    assert_eq!(persisted.trim(), winner_id);

    drop(client);
    stop_daemon(&mut winner);
}

/// A model endpoint that accepts and holds connections open forever, so a
/// turn is genuinely running when the daemon is killed.
async fn hold_open_model_endpoint() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                // Hold until the peer goes away.
                let _stream = stream;
                std::future::pending::<()>().await;
            });
        }
    });
    (format!("http://{addr}"), handle)
}

/// Scenario E: SIGKILL mid-task, restart, recover. The restarted daemon reaps
/// the orphan `running` turn, the transcript survives exactly once, and the
/// session snapshot is still served. Side-effect replay conservatism itself
/// is locked by `leveler-engine/tests/crash_recovery_test.rs`; this proves
/// the process-level path into those semantics.
#[test]
fn sigkill_during_a_task_recovers_on_restart_without_duplication() {
    leveler_test_support::bounded_test(
        "sigkill_during_a_task_recovers_on_restart_without_duplication",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        sigkill_during_a_task_recovers_on_restart_without_duplication_body,
    );
}

async fn sigkill_during_a_task_recovers_on_restart_without_duplication_body() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let env = test_env(&base_url);
    let ready1 = env.home.join("ready1.json");
    let mut daemon = spawn_serve(&env, &ready1);
    let ready = wait_ready(&ready1, &mut daemon, Duration::from_secs(30));
    let runtime_id = ready["runtime_id"].as_str().unwrap().to_string();

    let socket = find_socket(&env);
    let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();
    let session = client
        .create_session(CreateSessionRequest {
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "crash me".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "MARKER_BEFORE_CRASH".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();

    // `send` ACKs only after the running turn and its write-ahead initiating
    // input are durable. A one-shot read immediately after ACK locks that
    // contract; no polling is allowed to hide an early ACK.
    let db_path = find_state_dir(&env).join("sessions.db");
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    let running = turns
        .iter()
        .find(|turn| turn.status == "running")
        .expect("ACK must follow the durable running turn");
    assert!(
        running
            .payload
            .as_deref()
            .is_some_and(|payload| payload.contains("MARKER_BEFORE_CRASH")),
        "the running row must carry replayable initiating input: {running:?}"
    );
    drop(client);
    drop(db);

    // SIGKILL: no shutdown path runs — this is the crash case.
    daemon.kill().expect("SIGKILL daemon");
    let _ = daemon.wait();

    // Restart: startup reap must clear the orphan running turn.
    let ready2 = env.home.join("ready2.json");
    let mut daemon = spawn_serve(&env, &ready2);
    let ready = wait_ready(&ready2, &mut daemon, Duration::from_secs(30));
    assert_eq!(
        ready["runtime_id"].as_str().unwrap(),
        runtime_id,
        "a crash must not change the runtime identity"
    );

    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    assert!(!turns.is_empty(), "the crashed turn must be persisted");
    assert!(
        turns.iter().all(|t| t.status != "running"),
        "restart must reap orphan running turns: {turns:?}"
    );
    let messages = leveler_storage::MessageRepository::new(&db)
        .load(&session)
        .await
        .unwrap();
    let marker_count = messages
        .iter()
        .filter(|p| p.contains("MARKER_BEFORE_CRASH"))
        .count();
    assert_eq!(
        marker_count, 1,
        "recovery must not duplicate the transcript"
    );

    // The session is still served after restart.
    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let snapshot = client.snapshot(&session).await.unwrap();
    assert_eq!(snapshot.id, session);
    assert!(
        snapshot
            .messages
            .iter()
            .any(|m| m.text.contains("MARKER_BEFORE_CRASH")),
        "the reopened session must carry the pre-crash transcript"
    );

    drop(client);
    stop_daemon(&mut daemon);
}

/// Running turns and task owners, read straight from the shared database.
async fn running_turns(db: &leveler_storage::Database) -> Vec<leveler_storage::TurnRecord> {
    leveler_storage::TurnStore::list_running(db, None)
        .await
        .unwrap()
}

async fn task_owner(
    db: &leveler_storage::Database,
    session: &leveler_core::SessionId,
) -> leveler_storage::TaskOwner {
    let task = leveler_storage::TaskStore::task_for_session(db, session)
        .await
        .unwrap()
        .expect("a session that ran has a task");
    leveler_storage::OwnershipStore::current(db, &task)
        .await
        .unwrap()
        .unwrap()
}

/// Real processes, one repository, one RuntimeId. A daemon runs a turn; a
/// `leveler run` process starts beside it and must not interrupt or fence it.
/// Then the daemon is SIGKILLed: the next daemon reaps the dead daemon's turn
/// — and leaves the still-live `leveler run` turn alone.
#[test]
fn live_processes_keep_their_turns_and_only_a_killed_ones_turn_is_reaped() {
    leveler_test_support::bounded_test(
        "live_processes_keep_their_turns_and_only_a_killed_ones_turn_is_reaped",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        live_processes_keep_their_turns_and_only_a_killed_ones_turn_is_reaped_body,
    );
}

async fn live_processes_keep_their_turns_and_only_a_killed_ones_turn_is_reaped_body() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let env = test_env(&base_url);
    let ready1 = env.home.join("ready1.json");
    let mut daemon = spawn_serve(&env, &ready1);
    wait_ready(&ready1, &mut daemon, Duration::from_secs(30));
    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let daemon_session = client
        .create_session(CreateSessionRequest {
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "daemon work".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    client
        .send(ClientCommand::SubmitMessage {
            session_id: daemon_session.clone(),
            content: "daemon keeps working".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    drop(client);

    let db_path = find_state_dir(&env).join("sessions.db");
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let daemon_turn = running_turns(&db)
        .await
        .pop()
        .expect("the daemon's turn runs");
    let daemon_owner = task_owner(&db, &daemon_session).await;
    assert!(daemon_turn.owner_boot_id.is_some());
    assert_eq!(
        daemon_owner.boot.as_ref().map(|b| b.as_str()),
        daemon_turn.owner_boot_id.as_deref()
    );

    // A second host on the same repository: `leveler run` creates a session
    // (with its startup recovery) and starts its own turn.
    let mut sibling_command = Command::new(env!("CARGO_BIN_EXE_leveler"));
    sibling_command
        .arg("--repo")
        .arg(&env.repo)
        .arg("run")
        .arg("sibling work")
        .env("LEVELER_HOME", &env.home)
        .env("LEVELER_CONFIG_DIR", &env.config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut sibling = ManagedChild::spawn(&mut sibling_command).expect("spawn leveler run");
    let deadline = Instant::now() + Duration::from_secs(30);
    let sibling_turn = loop {
        if let Some(turn) = running_turns(&db)
            .await
            .into_iter()
            .find(|turn| turn.session_id != daemon_session.as_str())
        {
            break turn;
        }
        assert!(
            sibling.try_wait().unwrap().is_none(),
            "leveler run exited before starting its turn"
        );
        assert!(
            Instant::now() < deadline,
            "leveler run never started a turn"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let sibling_session = leveler_core::SessionId::new(sibling_turn.session_id.clone());
    let sibling_owner = task_owner(&db, &sibling_session).await;
    assert_ne!(sibling_turn.owner_boot_id, daemon_turn.owner_boot_id);

    // LIVE_OWNER_FALSE_REAP = 0: the daemon's turn survived the sibling's start.
    let still = running_turns(&db).await;
    assert!(
        still.iter().any(|turn| turn.id == daemon_turn.id),
        "a starting sibling interrupted the live daemon's turn: {still:?}"
    );
    assert_eq!(task_owner(&db, &daemon_session).await, daemon_owner);

    // The crash.
    daemon.kill().expect("SIGKILL daemon");
    let _ = daemon.wait();

    let ready2 = env.home.join("ready2.json");
    let mut daemon = spawn_serve(&env, &ready2);
    wait_ready(&ready2, &mut daemon, Duration::from_secs(30));

    // DEAD_OWNER_MISSED_REAP = 0, and still no false reap of the live sibling.
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&daemon_session)
        .await
        .unwrap();
    let reaped = turns.iter().find(|turn| turn.id == daemon_turn.id).unwrap();
    assert_eq!(
        reaped.status, "interrupted",
        "the killed daemon's turn is reaped"
    );
    assert_eq!(reaped.owner_boot_id, daemon_turn.owner_boot_id);
    // Recovery is finite: the restarted daemon stays up and leaves the
    // session it settled unowned, one generation on.
    let recovered = task_owner(&db, &daemon_session).await;
    assert_eq!((recovered.runtime, recovered.boot), (None, None));
    assert_eq!(recovered.epoch, daemon_owner.epoch.next().unwrap());
    let running = running_turns(&db).await;
    assert!(
        running.iter().any(|turn| turn.id == sibling_turn.id),
        "the restarted daemon interrupted the live `leveler run` turn: {running:?}"
    );
    assert_eq!(task_owner(&db, &sibling_session).await, sibling_owner);

    let _ = sibling.kill();
    let _ = sibling.wait();
    stop_daemon(&mut daemon);
}

/// Real processes, one repository: a daemon runs a user shell while this test
/// process is another boot on the same state, probing liveness through the
/// real boot leases. While the shell runs the session is the daemon's; once it
/// exits the daemon stays up and the session passes to the other boot.
#[test]
fn a_live_daemons_user_shell_holds_the_session_only_while_it_runs() {
    leveler_test_support::bounded_test(
        "a_live_daemons_user_shell_holds_the_session_only_while_it_runs",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        a_live_daemons_user_shell_holds_the_session_only_while_it_runs_body,
    );
}

async fn a_live_daemons_user_shell_holds_the_session_only_while_it_runs_body() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let env = test_env(&base_url);
    let ready = env.home.join("ready-shell.json");
    let mut daemon = spawn_serve(&env, &ready);
    wait_ready(&ready, &mut daemon, Duration::from_secs(30));
    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let session = client
        .create_session(CreateSessionRequest {
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "shell work".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    client
        .send(ClientCommand::RunUserShell {
            session_id: session.clone(),
            command: "sleep 5".to_string(),
        })
        .await
        .unwrap();

    let state_dir = find_state_dir(&env);
    let db = leveler_storage::Database::connect(&state_dir.join("sessions.db"))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let held = loop {
        let owner = task_owner(&db, &session).await;
        if owner.boot.is_some() {
            break owner;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon's shell never took the session"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let sibling = leveler_engine::TaskEngine {
        stores: leveler_storage::EngineStores::from_database(&db),
        runtime_id: held.runtime.clone().unwrap(),
        boot: leveler_engine::EngineBoot {
            id: leveler_core::BootId::generate(),
            liveness: std::sync::Arc::new(leveler_app::runtime_boot::StateDirBootLiveness::new(
                &state_dir,
            )),
        },
    };

    let refused = sibling.acquire_ownership(&session).await;
    assert!(
        matches!(
            refused,
            Err(leveler_engine::EngineError::OwnedByLiveBoot { .. })
        ),
        "{refused:?}"
    );
    assert_eq!(task_owner(&db, &session).await, held);

    let deadline = Instant::now() + Duration::from_secs(30);
    while task_owner(&db, &session).await.boot.is_some() {
        assert!(
            Instant::now() < deadline,
            "the finished shell kept the session"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(daemon.try_wait().unwrap().is_none(), "the daemon stays up");
    let next = sibling
        .acquire_ownership(&session)
        .await
        .expect("another boot takes the session while the daemon stays up");
    assert_eq!(next.owner_epoch, held.epoch.next().unwrap());

    drop(client);
    stop_daemon(&mut daemon);
}

/// Deterministic C2/C5/C8 boundary: ACK has been returned and the running turn
/// carries its canonical input, while the transcript append is held behind an
/// exact test barrier. SIGKILL there must reconstruct one user message; a
/// second restart must remain a no-op.
#[cfg(feature = "test-crash-barrier")]
#[test]
fn sigkill_after_durable_ack_before_transcript_append_recovers_once() {
    leveler_test_support::bounded_test(
        "sigkill_after_durable_ack_before_transcript_append_recovers_once",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        sigkill_after_durable_ack_before_transcript_append_recovers_once_body,
    );
}

#[cfg(feature = "test-crash-barrier")]
async fn sigkill_after_durable_ack_before_transcript_append_recovers_once_body() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let env = test_env(&base_url);
    let ready1 = env.home.join("ready-barrier-1.json");
    let barrier = env.home.join(format!(
        ".test-crash-barrier-{}",
        leveler_core::new_uuid_string()
    ));
    let mut daemon = spawn_serve_with_after_turn_started_barrier(&env, &ready1, &barrier);
    wait_ready(&ready1, &mut daemon, Duration::from_secs(30));

    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let session = client
        .create_session(CreateSessionRequest {
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "deterministic crash".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "DETERMINISTIC_CRASH_MARKER".to_string(),
            attachments: vec![],
        })
        .await
        .expect("ACK follows durable turn input");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !barrier.is_file() {
        assert!(
            Instant::now() < deadline,
            "daemon never reached the exact post-TurnStarted barrier"
        );
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "daemon exited before the crash barrier"
        );
        std::thread::yield_now();
    }

    let db_path = find_state_dir(&env).join("sessions.db");
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    let running = turns
        .iter()
        .find(|turn| turn.status == "running")
        .expect("barrier requires a durable running turn");
    assert!(
        running
            .payload
            .as_deref()
            .is_some_and(|payload| payload.contains("DETERMINISTIC_CRASH_MARKER")),
        "running turn must carry canonical input"
    );
    let before = leveler_storage::MessageRepository::new(&db)
        .load(&session)
        .await
        .unwrap();
    assert!(
        before
            .iter()
            .all(|payload| !payload.contains("DETERMINISTIC_CRASH_MARKER")),
        "barrier must stop before the transcript projection"
    );
    drop(client);
    drop(db);

    daemon.kill().expect("SIGKILL at exact crash barrier");
    let _ = daemon.wait();

    let ready2 = env.home.join("ready-barrier-2.json");
    let mut daemon = spawn_serve(&env, &ready2);
    wait_ready(&ready2, &mut daemon, Duration::from_secs(30));
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let messages = leveler_storage::MessageRepository::new(&db)
        .load(&session)
        .await
        .unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|payload| payload.contains("DETERMINISTIC_CRASH_MARKER"))
            .count(),
        1,
        "restart must project the accepted input exactly once"
    );
    assert!(
        leveler_storage::TurnRepository::new(&db)
            .list(&session)
            .await
            .unwrap()
            .iter()
            .all(|turn| turn.status != "running"),
        "restart recovery is complete only after the turn is terminal"
    );
    drop(db);
    stop_daemon(&mut daemon);

    let ready3 = env.home.join("ready-barrier-3.json");
    let mut daemon = spawn_serve(&env, &ready3);
    wait_ready(&ready3, &mut daemon, Duration::from_secs(30));
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let messages = leveler_storage::MessageRepository::new(&db)
        .load(&session)
        .await
        .unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|payload| payload.contains("DETERMINISTIC_CRASH_MARKER"))
            .count(),
        1,
        "repeated restart must not duplicate recovery"
    );
    stop_daemon(&mut daemon);
}

/// The dispatching crash window, with a real process and a real SIGKILL: the
/// daemon admits a submission and dispatches it, then dies before recording
/// that. Its boot lease dies with it. A restarted daemon must answer the same
/// command id — over the socket — as unresolvable, every time, and never run it
/// a second time.
#[cfg(feature = "test-crash-barrier")]
#[test]
fn sigkill_before_the_receipt_settles_is_unresolvable_after_restart() {
    leveler_test_support::bounded_test(
        "sigkill_before_the_receipt_settles_is_unresolvable_after_restart",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        sigkill_before_the_receipt_settles_is_unresolvable_after_restart_body,
    );
}

#[cfg(feature = "test-crash-barrier")]
async fn sigkill_before_the_receipt_settles_is_unresolvable_after_restart_body() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let env = test_env(&base_url);
    let ready1 = env.home.join("ready-receipt-1.json");
    let barrier = env.home.join(format!(
        ".test-crash-barrier-{}",
        leveler_core::new_uuid_string()
    ));
    let mut daemon = spawn_serve_with_barrier(
        &env,
        &ready1,
        "LEVELER_TEST_BEFORE_RECEIPT_SETTLED_BARRIER",
        &barrier,
    );
    wait_ready(&ready1, &mut daemon, Duration::from_secs(30));

    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let session = client
        .create_session(CreateSessionRequest {
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "receipt crash".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    let envelope = leveler_client_protocol::CommandEnvelope {
        command_id: leveler_client_protocol::CommandId::new("cmd-sigkill-receipt"),
        session_id: session.clone(),
        expected_version: None,
        issued_at: "2026-09-15T00:00:00Z".to_string(),
        command: ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "RECEIPT_CRASH_MARKER".to_string(),
            attachments: vec![],
        },
    };
    let first_client = std::sync::Arc::new(client);
    let in_flight = {
        let first_client = first_client.clone();
        let envelope = envelope.clone();
        tokio::spawn(async move { first_client.deliver(envelope).await })
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    while !barrier.is_file() {
        assert!(
            Instant::now() < deadline,
            "daemon never reached the barrier before settling the receipt"
        );
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "daemon exited before the barrier"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    daemon
        .kill()
        .expect("SIGKILL with the receipt still dispatching");
    let _ = daemon.wait();
    in_flight.abort();
    drop(first_client);

    let ready2 = env.home.join("ready-receipt-2.json");
    let mut daemon = spawn_serve(&env, &ready2);
    wait_ready(&ready2, &mut daemon, Duration::from_secs(30));
    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    for attempt in 0..2 {
        let error = client
            .deliver(envelope.clone())
            .await
            .expect_err("the crashed command must not be answered delivered");
        assert!(
            matches!(error, leveler_client_protocol::ClientError::Unresolvable(_)),
            "attempt {attempt}: {error:?}"
        );
    }

    let db_path = find_state_dir(&env).join("sessions.db");
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    assert_eq!(
        turns.len(),
        1,
        "the command ran once, before the crash: {turns:?}"
    );
    drop(db);
    stop_daemon(&mut daemon);
}

/// Gate Scenario A: a running daemon reports identity + admission health
/// over the socket, and the numbers reflect reality (no active work yet).
#[test]
fn health_reports_identity_and_admission() {
    leveler_test_support::bounded_test(
        "health_reports_identity_and_admission",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        health_reports_identity_and_admission_body,
    );
}

async fn health_reports_identity_and_admission_body() {
    let env = test_env("http://127.0.0.1:9");
    let ready = env.home.join("ready.json");
    let mut daemon = spawn_serve(&env, &ready);
    let ready_doc = wait_ready(&ready, &mut daemon, Duration::from_secs(30));

    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let info = LocalRuntimeService::runtime_info(&client).await.unwrap();
    assert_eq!(
        info.runtime_id.as_str(),
        ready_doc["runtime_id"].as_str().unwrap()
    );
    assert!(info.health.accepting_work, "an idle daemon accepts work");
    assert_eq!(info.health.active_turns, 0);
    assert_eq!(info.health.active_background_tasks, 0);
    assert!(
        info.health.quiescent(),
        "an idle daemon reports the same quiescence the drain waits on"
    );
    assert!(info.health.retiring_reason.is_none());
    assert!(info.health.turn_capacity.unwrap_or(0) > 0, "real capacity");
    assert!(!info.health.shutting_down);

    drop(client);
    stop_daemon(&mut daemon);
}

/// Gate Scenarios C+D: the daemon dies (SIGKILL) under a connected client
/// mid-task; after a restart the SAME client object reaches the SAME
/// RuntimeId, the session snapshot is served again, the orphan turn was
/// recovered, and the ownership epoch advanced (old tokens powerless).
#[test]
fn connected_client_recovers_after_daemon_sigkill() {
    leveler_test_support::bounded_test(
        "connected_client_recovers_after_daemon_sigkill",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        connected_client_recovers_after_daemon_sigkill_body,
    );
}

async fn connected_client_recovers_after_daemon_sigkill_body() {
    let (base_url, _model) = hold_open_model_endpoint().await;
    let env = test_env(&base_url);
    let ready1 = env.home.join("ready1.json");
    let mut daemon = spawn_serve(&env, &ready1);
    let first = wait_ready(&ready1, &mut daemon, Duration::from_secs(30));
    let runtime_id = first["runtime_id"].as_str().unwrap().to_string();

    let client = LocalSocketRuntimeClient::connect(&find_socket(&env))
        .await
        .unwrap();
    let session = client
        .create_session(CreateSessionRequest {
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
            goal: "survive the crash".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
        })
        .await
        .unwrap()
        .session
        .id;
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "MARKER".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();
    // Wait for a durable running turn, then note the pre-crash epoch.
    let db_path = find_state_dir(&env).join("sessions.db");
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let turns = leveler_storage::TurnRepository::new(&db)
            .list(&session)
            .await
            .unwrap();
        if turns.iter().any(|t| t.status == "running") {
            break;
        }
        assert!(Instant::now() < deadline, "turn never became durable");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let task = leveler_storage::TaskStore::task_for_session(&db, &session)
        .await
        .unwrap()
        .unwrap();
    let epoch_before = leveler_storage::OwnershipStore::current(&db, &task)
        .await
        .unwrap()
        .unwrap()
        .epoch;
    drop(db);

    daemon.kill().expect("SIGKILL");
    let _ = daemon.wait();

    // The supervisor semantics: a (revived) daemon comes back for the same
    // state dir. Here the test plays the reviver's role directly.
    let ready2 = env.home.join("ready2.json");
    let mut daemon = spawn_serve(&env, &ready2);
    let second = wait_ready(&ready2, &mut daemon, Duration::from_secs(30));
    assert_eq!(second["runtime_id"].as_str().unwrap(), runtime_id);

    // SAME client object: per-request connections + the subscription
    // reconnect loop reach the restarted daemon without a rebuild.
    let deadline = Instant::now() + Duration::from_secs(10);
    let snapshot = loop {
        match client.snapshot(&session).await {
            Ok(snapshot) => break snapshot,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(error) => panic!("session unreachable after restart: {error}"),
        }
    };
    assert_eq!(snapshot.id, session);
    let info = LocalRuntimeService::runtime_info(&client).await.unwrap();
    assert_eq!(info.runtime_id.as_str(), runtime_id);
    assert!(info.health.accepting_work);

    // Ownership: the restart reap reacquired — the epoch advanced, so every
    // pre-crash token is powerless; the orphan turn is no longer running.
    let db = leveler_storage::Database::connect(&db_path).await.unwrap();
    let epoch_after = leveler_storage::OwnershipStore::current(&db, &task)
        .await
        .unwrap()
        .unwrap()
        .epoch;
    assert!(
        epoch_after > epoch_before,
        "restart recovery must advance the owner epoch ({epoch_before} -> {epoch_after})"
    );
    let turns = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    assert!(turns.iter().all(|t| t.status != "running"));

    drop(client);
    stop_daemon(&mut daemon);
}

#[test]
fn no_workspace_host_durably_admits_and_restores_after_process_restart() {
    leveler_test_support::bounded_test(
        "no_workspace_host_durably_admits_and_restores_after_process_restart",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        no_workspace_host_durably_admits_and_restores_after_process_restart_body,
    );
}

async fn no_workspace_host_durably_admits_and_restores_after_process_restart_body() {
    use leveler_client_protocol::{ApprovalPolicy, CommandEnvelope, CommandId, PermissionProfile};
    use leveler_core::LevelerHome;
    use leveler_local_transport::CreateWorkspaceSelection;
    let env = test_env("http://127.0.0.1:9");
    let home = LevelerHome::from_root(env.home.clone());
    let layout = Layout::no_workspace(home.clone(), Some(env.config_dir.clone()));
    let (launch, guard) = detached_host_launch(&env);
    let client = ensure_default_runtime(&layout, &launch, Arc::new(SilentHandoffUi))
        .await
        .unwrap();
    let first_info = LocalRuntimeService::runtime_info(&client).await.unwrap();
    let bootstrap = client
        .create_session(CreateSessionRequest {
            workspace: CreateWorkspaceSelection::None,
            goal: "no workspace persisted task".into(),
            model: None,
            mode: PermissionProfile::Assisted,
            approval_policy: ApprovalPolicy::Interactive,
        })
        .await
        .unwrap();
    assert!(bootstrap.session.repository.is_none());
    let session = bootstrap.session.id;
    let mut events = client.subscribe_session(&session);
    let envelope = CommandEnvelope {
        command_id: CommandId::new("no-workspace-host-input"),
        session_id: session.clone(),
        expected_version: None,
        issued_at: leveler_core::now().to_rfc3339(),
        command: ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "persistent input without a directory".into(),
            attachments: vec![],
        },
    };
    // Intentionally lose the response: send a real Deliver frame then close
    // without reading any ACK. Admission is proved independently below.
    use tokio::io::AsyncWriteExt;
    let mut lost_ack_socket = tokio::net::UnixStream::connect(layout.socket_path())
        .await
        .unwrap();
    let request = serde_json::json!({
        "type": "deliver",
        "body": leveler_client_protocol::ProtocolEnvelope::wrap(envelope.clone()),
    });
    let frame =
        serde_json::to_vec(&leveler_client_protocol::ProtocolEnvelope::wrap(request)).unwrap();
    lost_ack_socket.write_u32(frame.len() as u32).await.unwrap();
    lost_ack_socket.write_all(&frame).await.unwrap();
    lost_ack_socket.flush().await.unwrap();
    drop(lost_ack_socket);
    // The fixture provider is unavailable. Wait for the truthful failure before
    // killing the process; the failure does not erase accepted user input.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if matches!(
                events.recv().await.unwrap(),
                RuntimeEvent::TurnFailed { .. }
            ) {
                break;
            }
        }
    })
    .await
    .expect("unavailable provider produces a terminal failure");
    let db = leveler_storage::Database::connect_read_only(&layout.database_path())
        .await
        .unwrap();
    let admitted = leveler_storage::TurnRepository::new(&db)
        .list(&session)
        .await
        .unwrap();
    assert_eq!(
        admitted.len(),
        1,
        "ACK loss must still leave exactly one durable initiating turn"
    );
    assert!(
        admitted[0]
            .payload
            .as_deref()
            .unwrap()
            .contains("persistent input without a directory")
    );
    drop(db);
    unsafe {
        libc_kill(guard.pid() as i32, 9);
    }
    drop(events);
    drop(client);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match probe_default_runtime(&layout.socket_path()).await {
                Ok(None) => break,
                Ok(Some(_)) => tokio::task::yield_now().await,
                Err(leveler_local_transport::TransportError::Io(error))
                    if matches!(
                        error.kind(),
                        // A SIGKILLed daemon's socket can surface as any of these
                        // depending on how far the kernel had progressed; all of
                        // them mean "it is not answering", which is what this
                        // probe is waiting to observe.
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    tokio::task::yield_now().await
                }
                Err(error) => panic!("probe after owned SIGKILL: {error}"),
            }
        }
    })
    .await
    .unwrap();
    let restarted = ensure_default_runtime(&layout, &launch, Arc::new(SilentHandoffUi))
        .await
        .unwrap();
    let second_info = LocalRuntimeService::runtime_info(&restarted).await.unwrap();
    assert_ne!(first_info.pid, second_info.pid);
    assert_eq!(first_info.runtime_id, second_info.runtime_id);
    let restored = restarted.snapshot(&session).await.unwrap();
    assert!(
        restored.repository.is_none(),
        "restart must not inherit cwd or HOME"
    );
    assert!(
        restored
            .messages
            .iter()
            .any(|message| message.text == "persistent input without a directory")
    );
    assert_eq!(
        restored.task_status,
        Some(leveler_client_protocol::UiTaskStatus::Failed)
    );
    assert!(restored.pending_interactions.is_empty());
    // The original command identity queries/reuses its settled receipt. It
    // must not dispatch a second initiating turn after the lost response.
    restarted.deliver(envelope).await.unwrap();
    let db = leveler_storage::Database::connect_read_only(&layout.database_path())
        .await
        .unwrap();
    assert_eq!(
        leveler_storage::TurnRepository::new(&db)
            .list(&session)
            .await
            .unwrap()
            .len(),
        1
    );
    let after_duplicate = restarted.snapshot(&session).await.unwrap();
    assert_eq!(
        after_duplicate
            .messages
            .iter()
            .filter(|message| message.text == "persistent input without a directory")
            .count(),
        1
    );
    let index = leveler_app::global_task_index::query_global_tasks(&home, false).await;
    assert!(index.source_errors.is_empty(), "{:?}", index.source_errors);
    let task = index.tasks.iter().find(|task| task.id == session).unwrap();
    assert!(task.primary_workspace.is_none());
    assert_eq!(task.status, restored.task_status.unwrap());
}

#[test]
fn global_open_resolves_repo_b_owner_while_repo_a_runtime_is_connected() {
    leveler_test_support::bounded_test(
        "global_open_resolves_repo_b_owner_while_repo_a_runtime_is_connected",
        leveler_test_support::DEFAULT_TEST_TIMEOUT,
        global_open_resolves_repo_b_owner_while_repo_a_runtime_is_connected_body,
    );
}

async fn global_open_resolves_repo_b_owner_while_repo_a_runtime_is_connected_body() {
    use leveler_client_protocol::{ApprovalPolicy, PermissionProfile};
    use leveler_core::LevelerHome;
    use leveler_local_transport::CreateWorkspaceSelection;
    let env = test_env("http://127.0.0.1:9");
    let home = LevelerHome::from_root(env.home.clone());
    let repo_b = env.repo.with_file_name("repo-b");
    std::fs::create_dir_all(&repo_b).unwrap();
    let layout_a = Layout::ephemeral(env.repo.clone(), Some(env.config_dir.clone()), &env.home);
    let layout_b = Layout::ephemeral(repo_b.clone(), Some(env.config_dir.clone()), &env.home);
    let app_b = leveler_app::Application::assemble(layout_b.clone()).unwrap();
    let session_b = app_b
        .create_session(&leveler_model::ModelRef::new("mock", "m"), "B history")
        .await
        .unwrap();
    drop(app_b);
    let (launch_a, mut guard_a) = detached_host_launch(&env);
    let client_a = ensure_default_runtime(&layout_a, &launch_a, Arc::new(SilentHandoffUi))
        .await
        .unwrap();
    let boot_a = client_a
        .create_session(CreateSessionRequest {
            workspace: CreateWorkspaceSelection::RuntimeDefault,
            goal: "A history".into(),
            model: None,
            mode: PermissionProfile::Assisted,
            approval_policy: ApprovalPolicy::Interactive,
        })
        .await
        .unwrap();
    let guard_a_path = env.home.join("host-daemon-a.pid");
    std::fs::rename(&guard_a.pid_file, &guard_a_path).unwrap();
    guard_a.pid_file = guard_a_path;
    let (launch_b, _guard_b) = detached_host_launch(&env);
    let index = leveler_app::global_task_index::query_global_tasks(&home, false).await;
    let task_b = index
        .tasks
        .iter()
        .find(|task| task.id == session_b)
        .unwrap();
    let client_b = leveler_runtime_host::connect_global_task_runtime(
        &home,
        &task_b.source_id,
        &session_b,
        Some(env.config_dir.clone()),
        &launch_b,
        Arc::new(SilentHandoffUi),
    )
    .await
    .unwrap();
    let snapshot_b = client_b.snapshot(&session_b).await.unwrap();
    assert_eq!(
        snapshot_b.repository.as_deref(),
        layout_b.primary_workspace().unwrap().to_str()
    );
    assert_eq!(
        client_a
            .snapshot(&boot_a.session.id)
            .await
            .unwrap()
            .repository
            .as_deref(),
        layout_a.primary_workspace().unwrap().to_str()
    );
    assert_ne!(
        LocalRuntimeService::runtime_info(&client_a)
            .await
            .unwrap()
            .runtime_id,
        LocalRuntimeService::runtime_info(&client_b)
            .await
            .unwrap()
            .runtime_id
    );
}
