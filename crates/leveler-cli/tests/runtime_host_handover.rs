//! The generation-handover contract, at the one seam the reported defect was in:
//! a client that discovers an older runtime which is still working.
//!
//! The product rule these tests pin:
//!
//! - an operator who can be asked keeps waiting for as long as the old runtime
//!   needs, because protecting running work is the point of the wait;
//! - an operator who cannot be asked is REPORTED as blocked instead of blocking
//!   with no terminal state, and a runtime that answers `Busy` is reported
//!   immediately rather than after a multi-minute budget;
//! - a runtime that answers `Busy` is never coerced: it keeps admitting work;
//! - a runtime that positively cannot retire atomically is replaced only after
//!   its process identity is PROVEN, and a runtime whose identity cannot be
//!   proven is refused with nothing signalled.
//!
//! Why a dedicated target: the budget is read from the process environment, and
//! a test that mutates it must not race tests in another binary that expect the
//! default. This file owns its own process, so it can set it freely.
#![cfg(unix)]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use leveler_client_protocol::{
    BuildIdentity, ClientCommand, ClientError, InteractiveRuntimeClient, RestartReason,
    RetireDecision, RetireRequest, RuntimeEvent, RuntimeHealth, RuntimeId, RuntimeInfo, SessionId,
    UiSessionSnapshot,
};
use leveler_local_transport::{
    CreateSessionRequest, LocalRuntimeService, LocalSocketRuntimeClient, LocalSocketServer,
    SessionBootstrap,
};
use leveler_project::Layout;
use leveler_runtime_host::{
    DetachedRuntimeLaunch, DrainOutcome, HandoffAction, HandoffEvent, HandoffUi,
    observe_retiring_runtime,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const BUDGET_SECS: u64 = 1;

struct Env {
    _tmp: tempfile::TempDir,
    home: std::path::PathBuf,
    repo: std::path::PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    Env {
        _tmp: tmp,
        home,
        repo,
    }
}

fn layout(env: &Env) -> Layout {
    let config = env.home.join("configs");
    std::fs::create_dir_all(config.join("providers")).unwrap();
    std::fs::create_dir_all(config.join("models")).unwrap();
    Layout::ephemeral(env.repo.clone(), Some(config), &env.home)
}

fn old_build() -> BuildIdentity {
    let mut build = BuildIdentity::current();
    build.fingerprint.push_str("-previous-build");
    build
}

/// What the previous runtime reports right now.
struct FakeState {
    pid: u32,
    health: RuntimeHealth,
    /// The runtime's authoritative answer to "may I retire now?". A runtime old
    /// enough to have no atomic retirement answers `Unsupported`; a current one
    /// answers `Busy` while work is owed. The two must never be conflated: the
    /// first may be replaced after verification, the second must be left alone.
    retire: RetireDecision,
}

/// A previous generation that accepted the retirement request and stopped
/// admitting work, but still owes a main turn and a runtime-owned background
/// task. One shared state, so a test can watch the client react to a
/// replacement taking the endpoint without restarting the fake.
struct FakeOldRuntime {
    build: BuildIdentity,
    state: Arc<Mutex<FakeState>>,
    observed: Arc<Mutex<Vec<String>>>,
    events: broadcast::Sender<RuntimeEvent>,
}

impl FakeOldRuntime {
    fn busy() -> Arc<Self> {
        let (events, _) = broadcast::channel(4);
        Arc::new(Self {
            build: old_build(),
            state: Arc::new(Mutex::new(FakeState {
                pid: std::process::id(),
                health: RuntimeHealth {
                    accepting_work: false,
                    active_turns: 1,
                    active_background_tasks: 1,
                    quiescent: false,
                    shutting_down: true,
                    retiring_reason: Some(RestartReason::BuildMismatch),
                    ..RuntimeHealth::default()
                },
                retire: RetireDecision::Busy {
                    active_turns: 1,
                    active_background_tasks: 1,
                },
            })),
            observed: Arc::new(Mutex::new(Vec::new())),
            events,
        })
    }

    /// A runtime that predates the atomic retirement request. Its pid is this
    /// test process's own, which is exactly the case that must be REFUSED when
    /// this process is the CLIENT — and exactly the right target when this
    /// process is the re-exec'ed fake daemon.
    fn legacy_without_a_provable_identity() -> Arc<Self> {
        Self::legacy()
    }

    /// The plain legacy fake: answers `Unsupported` for the pid it actually is.
    fn legacy() -> Arc<Self> {
        let (events, _) = broadcast::channel(4);
        Arc::new(Self {
            build: old_build(),
            state: Arc::new(Mutex::new(FakeState {
                pid: std::process::id(),
                health: RuntimeHealth {
                    accepting_work: true,
                    quiescent: true,
                    ..RuntimeHealth::default()
                },
                retire: RetireDecision::Unsupported,
            })),
            observed: Arc::new(Mutex::new(Vec::new())),
            events,
        })
    }

    fn pid(&self) -> u32 {
        self.state.lock().unwrap().pid
    }

    /// A replacement generation took the endpoint.
    fn replaced_by(&self, pid: u32) {
        self.state.lock().unwrap().pid = pid;
    }

    fn commands(&self) -> Vec<String> {
        self.observed.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl InteractiveRuntimeClient for FakeOldRuntime {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        self.observed.lock().unwrap().push(format!("{command:?}"));
        match command {
            ClientCommand::ShutdownWhenIdle { reason } => {
                assert_eq!(reason, RestartReason::BuildMismatch);
                Ok(())
            }
            other => Err(ClientError::Runtime(format!(
                "the handover must never coerce the previous runtime: {other:?}"
            ))),
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self, _session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        Err(ClientError::Runtime(
            "the old runtime has no sessions".into(),
        ))
    }
}

#[async_trait::async_trait]
impl LocalRuntimeService for FakeOldRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Err(ClientError::Runtime(
            "the old runtime cannot create sessions".into(),
        ))
    }

    async fn runtime_info(&self) -> Result<RuntimeInfo, ClientError> {
        let state = self.state.lock().unwrap();
        Ok(RuntimeInfo {
            runtime_id: RuntimeId::new("previous-runtime"),
            version: self.build.version.clone(),
            build: self.build.clone(),
            config_fingerprint: None,
            pid: state.pid,
            health: state.health.clone(),
        })
    }

    async fn try_retire_if_idle(
        &self,
        _request: RetireRequest,
    ) -> Result<RetireDecision, ClientError> {
        Ok(self.state.lock().unwrap().retire)
    }
}

/// A shell with no handover prompt: the Desktop bridge, a piped or CI TUI, or
/// any client launched by another program. It answers the question honestly
/// instead of pretending it can ask someone.
struct UnanswerableShell;

impl HandoffUi for UnanswerableShell {
    fn emit(&self, _event: HandoffEvent) {}

    fn input(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<HandoffAction>> {
        None
    }
}

/// A shell with a human at it. It records what it was told, so the test can
/// prove a long wait is observable rather than silent.
struct AnswerableShell {
    seen: Arc<Mutex<Vec<String>>>,
}

impl HandoffUi for AnswerableShell {
    fn emit(&self, event: HandoffEvent) {
        self.seen.lock().unwrap().push(format!("{event:?}"));
    }

    fn input(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<HandoffAction>> {
        None
    }

    fn interactive(&self) -> bool {
        true
    }
}

/// Start the fake previous runtime on this layout's endpoint.
async fn serve(
    old: &Arc<FakeOldRuntime>,
    layout: &Layout,
) -> (CancellationToken, tokio::task::JoinHandle<()>) {
    let service: Arc<dyn LocalRuntimeService> = old.clone();
    let server = LocalSocketServer::bind(layout.socket_path(), service)
        .await
        .expect("the previous runtime owns the endpoint");
    let shutdown = CancellationToken::new();
    let serve_shutdown = shutdown.clone();
    let served = tokio::spawn(async move {
        let _ = server.serve(serve_shutdown).await;
    });
    (shutdown, served)
}

/// A shell that cannot be asked learns the upgrade is deferred IMMEDIATELY —
/// not after a multi-minute budget — and nothing is cancelled.
#[tokio::test]
async fn an_unanswerable_shell_is_reported_promptly_instead_of_waiting() {
    // The budget is deliberately longer than the assertion below: the point is
    // that the answer does not come from the budget at all.
    // SAFETY: this test target owns its process, and the budget is read once per
    // handover, so no other test in this binary can observe the mutation.
    unsafe {
        std::env::set_var(
            "LEVELER_HANDOVER_DRAIN_TIMEOUT_SECS",
            (BUDGET_SECS + 30).to_string(),
        )
    };

    let env = env();
    let layout = layout(&env);
    let old = FakeOldRuntime::busy();
    let (_shutdown, served) = serve(&old, &layout).await;

    let started = std::time::Instant::now();
    let result = leveler_runtime_host::ensure_default_runtime(
        &layout,
        // Never reached: the handover is reported blocked before any spawn.
        &DetachedRuntimeLaunch {
            executable: env.home.join("must-not-be-launched"),
            ready_prefix: "must-not-launch".to_string(),
        },
        Arc::new(UnanswerableShell),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("an unanswerable shell must not wait forever on a busy runtime"),
        Err(error) => error,
    };

    let blocked = error
        .downcast_ref::<leveler_runtime_host::EnsureError>()
        .expect("the failure names the handover contract");
    assert!(
        matches!(
            blocked,
            leveler_runtime_host::EnsureError::RetireBlocked {
                active_turns: 1,
                active_background_tasks: 1,
                ..
            }
        ),
        "the report must carry what is holding the handover: {blocked}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a deferral must be reported promptly, took {:?}",
        started.elapsed()
    );

    // Nothing was coerced: the previous runtime is still serving, it is still
    // admitting work, and it was never commanded to stop.
    let commands = old.commands();
    assert!(
        commands.is_empty(),
        "a busy runtime must never be commanded: {commands:?}"
    );
    let still_there = LocalSocketRuntimeClient::connect(&layout.socket_path())
        .await
        .expect("the previous runtime is still serving");
    assert_eq!(
        LocalRuntimeService::runtime_info(&still_there)
            .await
            .unwrap()
            .pid,
        old.pid(),
        "no replacement may take the endpoint while the old owner is blocked"
    );

    drop(still_there);
    served.abort();
}

/// A runtime that positively cannot retire atomically, and whose process
/// identity cannot be proven, is REFUSED — never killed on the strength of its
/// own claim.
///
/// The fake reports this test process's own pid, which is the adversarial case
/// the ownership check exists for: a runtime that answers a pid the client
/// cannot safely signal must produce a refusal, not a signal.
#[tokio::test]
async fn a_legacy_runtime_without_a_provable_identity_is_refused_without_a_signal() {
    // SAFETY: this test target owns its process.
    unsafe { std::env::set_var("LEVELER_HANDOVER_DRAIN_TIMEOUT_SECS", "1") };

    let env = env();
    let layout = layout(&env);
    let old = FakeOldRuntime::legacy_without_a_provable_identity();
    let (_shutdown, served) = serve(&old, &layout).await;

    let result = leveler_runtime_host::ensure_default_runtime(
        &layout,
        &DetachedRuntimeLaunch {
            executable: env.home.join("must-not-be-launched"),
            ready_prefix: "must-not-launch".to_string(),
        },
        Arc::new(UnanswerableShell),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("a legacy runtime must not be adopted without a replacement"),
        Err(error) => error,
    };
    let refused = error
        .downcast_ref::<leveler_runtime_host::EnsureError>()
        .expect("the failure names the migration contract");
    assert!(
        matches!(
            refused,
            leveler_runtime_host::EnsureError::LegacyMigrationRefused { .. }
        ),
        "an unprovable identity must be a refusal, got: {refused}"
    );
    // Refused, not terminated: this process is still here, and so is the
    // runtime on the endpoint.
    assert!(LocalSocketRuntimeClient::connect(&layout.socket_path())
        .await
        .is_ok());
    assert!(
        old.commands().is_empty(),
        "a refusal must not command the previous runtime"
    );
    served.abort();
}

/// A shell with a human at it keeps waiting past the budget, says so, and
/// completes the handover as soon as a replacement owns the endpoint.
#[tokio::test]
async fn an_answerable_shell_keeps_waiting_and_reports_that_it_is_waiting() {
    // SAFETY: see the test above.
    unsafe {
        std::env::set_var(
            "LEVELER_HANDOVER_DRAIN_TIMEOUT_SECS",
            BUDGET_SECS.to_string(),
        )
    };

    let env = env();
    let layout = layout(&env);
    let old = FakeOldRuntime::busy();
    let (_shutdown, served) = serve(&old, &layout).await;

    let client = LocalSocketRuntimeClient::connect(&layout.socket_path())
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let ui: Arc<dyn HandoffUi> = Arc::new(AnswerableShell { seen: seen.clone() });
    let socket = layout.socket_path();
    let observed_pid = old.pid();
    let observing = tokio::spawn(async move {
        observe_retiring_runtime(
            &client,
            &socket,
            Some(observed_pid),
            Duration::from_millis(20),
            &*ui,
        )
        .await
    });

    tokio::time::sleep(Duration::from_secs(BUDGET_SECS * 3)).await;
    assert!(
        !observing.is_finished(),
        "a shell that can be asked must keep protecting the running work"
    );
    let reported = seen.lock().unwrap().clone();
    assert!(
        reported.iter().any(|event| event.contains("StillWaiting")),
        "a long handover must be visible, not silent: {reported:?}"
    );

    // Another client won the race and a replacement took the endpoint: the wait
    // ends on that fact, not on the budget.
    old.replaced_by(observed_pid.wrapping_add(1));
    let outcome = tokio::time::timeout(Duration::from_secs(10), observing)
        .await
        .expect("the wait ends once a replacement owns the endpoint")
        .unwrap();
    assert!(
        matches!(outcome, DrainOutcome::Drained),
        "a replacement on the endpoint completes the handover"
    );
    served.abort();
}

/// A stale socket, with no daemon behind it, is not a generation handover at
/// all: discovery must read it as "no runtime", not as a previous generation.
#[tokio::test]
async fn a_stale_socket_reads_as_no_runtime() {
    let env = env();
    let layout = layout(&env);
    let socket = layout.socket_path();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    // Bound then dropped: the file stays behind with no process to answer it.
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    drop(listener);
    assert!(Path::new(&socket).exists(), "the stale socket file exists");
    assert!(
        leveler_runtime_host::probe_default_runtime(&socket)
            .await
            .unwrap()
            .is_none(),
        "a stale socket must read as no runtime"
    );
}

// ── PR1: every client-side IPC step is bounded, and a timeout is not a death ──

/// A service that accepts the connection and then never answers `runtime_info`.
/// Everything else about it is a normal local-runtime service, so the transport,
/// the socket and the daemon-side dispatch are all real.
struct MuteRuntime;

#[async_trait::async_trait]
impl InteractiveRuntimeClient for MuteRuntime {
    async fn send(&self, _command: ClientCommand) -> Result<(), ClientError> {
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        broadcast::channel(4).1
    }

    async fn snapshot(&self, _session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        Err(ClientError::Runtime("mute runtime".into()))
    }
}

#[async_trait::async_trait]
impl LocalRuntimeService for MuteRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Err(ClientError::Runtime("mute runtime".into()))
    }

    async fn runtime_info(&self) -> Result<RuntimeInfo, ClientError> {
        // Long enough that the 1s client deadline is the only thing that can
        // end the wait, short enough that a leaked test task cannot pin the
        // suite: the test aborts the server anyway.
        tokio::time::sleep(Duration::from_secs(30)).await;
        unreachable!("the request must time out long before this returns")
    }
}

/// A runtime older than build identity reports none, and none is not a match:
/// reusing it silently is exactly the failure this refusal exists for.
struct AnonymousRuntime;

#[async_trait::async_trait]
impl InteractiveRuntimeClient for AnonymousRuntime {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        Err(ClientError::Runtime(format!(
            "an unidentifiable runtime must not be commanded: {command:?}"
        )))
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        broadcast::channel(4).1
    }

    async fn snapshot(&self, _session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        Err(ClientError::Runtime("anonymous runtime".into()))
    }
}

#[async_trait::async_trait]
impl LocalRuntimeService for AnonymousRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Err(ClientError::Runtime("anonymous runtime".into()))
    }

    async fn runtime_info(&self) -> Result<RuntimeInfo, ClientError> {
        Ok(RuntimeInfo {
            runtime_id: RuntimeId::new("anonymous-runtime"),
            version: String::new(),
            // A runtime that predates build identity reports the default, which
            // `is_known()` rejects.
            build: BuildIdentity::default(),
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

/// An unidentifiable runtime is refused, not silently reused and not replaced.
#[tokio::test]
async fn a_runtime_without_a_usable_identity_is_refused() {
    // SAFETY: this test target owns its process.
    unsafe { std::env::set_var("LEVELER_HANDOVER_DRAIN_TIMEOUT_SECS", "1") };

    let env = env();
    let layout = layout(&env);
    let socket = layout.socket_path();
    let service: Arc<dyn LocalRuntimeService> = Arc::new(AnonymousRuntime);
    let server = LocalSocketServer::bind(&socket, service).await.unwrap();
    let shutdown = CancellationToken::new();
    let serve_shutdown = shutdown.clone();
    let served = tokio::spawn(async move {
        let _ = server.serve(serve_shutdown).await;
    });

    let result = leveler_runtime_host::ensure_default_runtime(
        &layout,
        &DetachedRuntimeLaunch {
            executable: env.home.join("must-not-be-launched"),
            ready_prefix: "must-not-launch".to_string(),
        },
        Arc::new(UnanswerableShell),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("an unidentifiable runtime must not be reused"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error.downcast_ref::<leveler_runtime_host::EnsureError>(),
            Some(leveler_runtime_host::EnsureError::UnknownGeneration)
        ),
        "the refusal must name the unknown generation, got: {error}"
    );
    // Refused, not replaced: the runtime is still there and still serving.
    assert!(LocalSocketRuntimeClient::connect(&socket).await.is_ok());
    shutdown.cancel();
    served.abort();
}

/// A reviver that records whether it was asked to act. Revival is how a client
/// starts a SECOND daemon, so "was it called" is the whole safety question.
struct SpyReviver {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl leveler_local_transport::RuntimeReviver for SpyReviver {
    async fn revive(&self) -> Result<(), String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// An unresponsive runtime is reported as an unresponsive runtime: bounded, not
/// revived (which would spawn a competitor), and not treated as a death.
#[tokio::test]
async fn an_unanswered_request_is_bounded_and_never_revives() {
    // SAFETY: this test target owns its process.
    unsafe {
        std::env::set_var("LEVELER_IPC_REQUEST_TIMEOUT_SECS", "1");
        std::env::set_var("LEVELER_HANDOVER_DRAIN_TIMEOUT_SECS", "1");
    }

    let env = env();
    let layout = layout(&env);
    let socket = layout.socket_path();
    let service: Arc<dyn LocalRuntimeService> = Arc::new(MuteRuntime);
    let server = LocalSocketServer::bind(&socket, service).await.unwrap();
    let shutdown = CancellationToken::new();
    let serve_shutdown = shutdown.clone();
    let served = tokio::spawn(async move {
        let _ = server.serve(serve_shutdown).await;
    });

    let client = LocalSocketRuntimeClient::connect(&socket)
        .await
        .expect("the handshake itself answers");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    client.set_reviver(Arc::new(SpyReviver {
        calls: calls.clone(),
    }));

    let started = std::time::Instant::now();
    let error = LocalRuntimeService::runtime_info(&client)
        .await
        .expect_err("a request with no answer must not succeed");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(20),
        "the request must be bounded, took {elapsed:?}"
    );
    let text = error.to_string();
    assert!(
        text.contains("did not answer") || text.contains("outcome unknown"),
        "the failure must say the runtime went unanswered: {text}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a timeout must never start a second daemon"
    );
    // Still there, still owned by the same process: nothing was killed and no
    // socket was deleted on the strength of a timeout.
    let second = LocalSocketRuntimeClient::connect(&socket)
        .await
        .expect("the endpoint must still be listening");
    drop(second);
    drop(client);
    shutdown.cancel();
    // Abort rather than await: the in-flight `runtime_info` is deliberately
    // still pending, and waiting for it would make a bounded contract test run
    // for the length of the mute sleep.
    served.abort();
}

/// A daemon that is still starting must not be killed by the client that
/// launched it: the readiness bound is the client's liveness budget, not a
/// verdict on the process.
#[tokio::test]
async fn a_daemon_that_is_still_starting_is_not_killed() {
    // SAFETY: this test target owns its process.
    unsafe { std::env::set_var("LEVELER_HANDOVER_DRAIN_TIMEOUT_SECS", "1") };

    use std::os::unix::fs::PermissionsExt;

    let env = env();
    let layout = layout(&env);
    let pid_file = env.home.join("slow-daemon.pid");
    let script = layout.home().root().join("slow-daemon.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > {}\nsleep 120\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

    let started = std::time::Instant::now();
    let result = leveler_runtime_host::ensure_default_runtime(
        &layout,
        &DetachedRuntimeLaunch {
            executable: script.clone(),
            ready_prefix: "pr1-starting".to_string(),
        },
        Arc::new(UnanswerableShell),
    )
    .await;
    let elapsed = started.elapsed();
    let error = match result {
        Ok(_) => panic!("a daemon that never publishes readiness must not be reported ready"),
        Err(error) => error,
    };
    let host = error
        .downcast_ref::<leveler_runtime_host::EnsureError>()
        .expect("the failure names the startup contract");
    assert!(
        matches!(
            host,
            leveler_runtime_host::EnsureError::ReadyTimeout {
                observation: leveler_runtime_host::StartupObservation::Starting,
                ..
            }
        ),
        "a live child without readiness must be reported as still starting: {host}"
    );
    assert!(
        elapsed < Duration::from_secs(120),
        "the startup bound must hold, took {elapsed:?}"
    );

    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("the launched daemon recorded its pid")
        .trim()
        .parse()
        .expect("pid is numeric");
    let alive = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .expect("probe the process");
    assert!(
        alive.success(),
        "the still-starting daemon must not be killed by the client that launched it"
    );
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status();
}

// ── Forced migration of a runtime with no atomic retirement ──────────────────
//
// These two tests are one scenario split in two, because the only honest way to
// prove the migration is against a REAL second process: the client under test
// must witness a pid it did not create, verify it, and terminate exactly it.

/// The fake legacy runtime, when this test binary is re-exec'ed with
/// `LEVELER_LEGACY_RUNTIME_SOCKET` set. It speaks the real local-transport
/// protocol over a real Unix socket and answers `Unsupported` to the atomic
/// retirement request, which is what a build older than that request does.
///
/// Returns immediately on a normal test run.
#[test]
fn legacy_runtime_child_helper() {
    let Some(socket) = std::env::var_os("LEVELER_LEGACY_RUNTIME_SOCKET") else {
        return;
    };
    let socket = std::path::PathBuf::from(socket);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let service: Arc<dyn LocalRuntimeService> = FakeOldRuntime::legacy();
        let server = LocalSocketServer::bind(&socket, service)
            .await
            .expect("the fake legacy runtime owns the endpoint");
        // Serves until this process is signalled. Nothing here exits on its own:
        // the point of the test is that the CLIENT is what ends it.
        server.serve(CancellationToken::new()).await.unwrap();
    });
}

/// Wait until the endpoint answers, so the parent never races the child's bind.
async fn await_endpoint(socket: &std::path::Path) -> LocalSocketRuntimeClient {
    for _ in 0..200 {
        if let Ok(client) = LocalSocketRuntimeClient::connect(socket).await {
            return client;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the fake legacy runtime never answered on {}", socket.display());
}

/// A runtime that cannot retire atomically is replaced in a CONTROLLED way: its
/// identity is verified against a REAL second process, it is asked to exit, and
/// it is confirmed gone before anything may take the endpoint.
#[tokio::test]
async fn a_legacy_runtime_is_verified_and_terminated_before_replacement() {
    unsafe { std::env::set_var("LEVELER_HANDOVER_DRAIN_TIMEOUT_SECS", "1") };

    let env = env();
    let layout = layout(&env);
    let socket = layout.socket_path();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("legacy_runtime_child_helper")
        .arg("--nocapture")
        .env("LEVELER_LEGACY_RUNTIME_SOCKET", &socket)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("a real legacy runtime process must start");

    let client = await_endpoint(&socket).await;
    let info = LocalRuntimeService::runtime_info(&client).await.unwrap();
    assert_eq!(
        info.pid,
        child.id(),
        "the fake must report the pid of the process that is actually serving"
    );
    drop(client);

    // The launch is deliberately impossible: what is under test is that the
    // handover COMPLETES (the old owner is verified and gone) and the run only
    // fails at the spawn, never at the handover.
    let result = leveler_runtime_host::ensure_default_runtime(
        &layout,
        &DetachedRuntimeLaunch {
            executable: env.home.join("must-not-exist"),
            ready_prefix: "legacy-migration".to_string(),
        },
        Arc::new(UnanswerableShell),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("a runtime with no atomic retirement must be replaced"),
        Err(error) => error,
    };
    assert!(
        error.downcast_ref::<leveler_runtime_host::EnsureError>().is_none(),
        "the handover must not be the failure; it must fail at the launch: {error}"
    );

    // The proof of the whole procedure: the real process is gone.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().unwrap() {
            Some(_) => break,
            None if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            None => panic!("the verified legacy runtime must be terminated"),
        }
    }
    assert!(
        leveler_runtime_host::probe_default_runtime(&socket)
            .await
            .unwrap()
            .is_none(),
        "the endpoint must be free before a replacement starts"
    );
}
