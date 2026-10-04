use std::path::{Path, PathBuf};
#[cfg(any(unix, windows))]
use std::process::Stdio;
#[cfg(any(unix, windows))]
use std::sync::Arc;
#[cfg(any(unix, windows))]
use std::time::Duration;

#[cfg(any(unix, windows))]
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RestartReason};
use leveler_client_protocol::{RuntimeHealth, RuntimeInfo, SessionId};
#[cfg(any(unix, windows))]
use leveler_local_transport::{LocalRuntimeService, RuntimeReviver};
use leveler_local_transport::{LocalSocketRuntimeClient, TransportError};
#[cfg(any(unix, windows))]
use leveler_project::Layout;
use tokio::sync::mpsc::UnboundedReceiver;

#[cfg(any(unix, windows))]
const DEFAULT_DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_millis(50);
#[cfg(any(unix, windows))]
const DAEMON_ENSURE_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(any(unix, windows))]
const HANDOVER_STATUS_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(any(unix, windows))]
const HANDOVER_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Probe the default per-repository endpoint without blocking TUI startup on
/// a stale socket. Web's direct attach intentionally uses a different API.
#[cfg(any(unix, windows))]
pub async fn probe_default_runtime(
    path: &Path,
) -> Result<Option<LocalSocketRuntimeClient>, TransportError> {
    match tokio::time::timeout(
        DEFAULT_DAEMON_CONNECT_TIMEOUT,
        LocalSocketRuntimeClient::connect(path),
    )
    .await
    {
        Ok(Ok(client)) => Ok(Some(client)),
        Ok(Err(TransportError::Io(error)))
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            tracing::debug!(%error, socket = %path.display(), "skipping unavailable local runtime");
            Ok(None)
        }
        Ok(Err(error)) => Err(error),
        Err(_) => {
            tracing::debug!(
                socket = %path.display(),
                timeout_ms = DEFAULT_DAEMON_CONNECT_TIMEOUT.as_millis(),
                "timed out probing local runtime"
            );
            Ok(None)
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub async fn probe_default_runtime(
    _path: &Path,
) -> Result<Option<LocalSocketRuntimeClient>, TransportError> {
    Ok(None)
}

#[derive(Debug)]
pub enum RuntimeConsistency {
    Current,
    Outdated {
        runtime: leveler_core::BuildIdentity,
        expected: leveler_core::BuildIdentity,
    },
    ConfigChanged,
    Unknown,
}

pub fn classify_runtime_generation(
    reported: Option<&RuntimeInfo>,
    expected_build: &leveler_core::BuildIdentity,
    expected_config_fingerprint: &str,
) -> RuntimeConsistency {
    match classify_runtime(reported.map(|info| &info.build), expected_build) {
        RuntimeConsistency::Current => match reported.and_then(|info| {
            info.config_fingerprint
                .as_deref()
                .map(|fingerprint| fingerprint == expected_config_fingerprint)
        }) {
            Some(true) => RuntimeConsistency::Current,
            Some(false) => RuntimeConsistency::ConfigChanged,
            None => RuntimeConsistency::Unknown,
        },
        other => other,
    }
}

pub fn classify_runtime(
    reported: Option<&leveler_core::BuildIdentity>,
    expected: &leveler_core::BuildIdentity,
) -> RuntimeConsistency {
    let Some(runtime) = reported else {
        return RuntimeConsistency::Unknown;
    };
    if !runtime.is_known() || !expected.is_known() {
        return RuntimeConsistency::Unknown;
    }
    if expected.matches(runtime) {
        RuntimeConsistency::Current
    } else {
        RuntimeConsistency::Outdated {
            runtime: runtime.clone(),
            expected: expected.clone(),
        }
    }
}

pub fn verify_replacement(
    reported: Option<&leveler_core::BuildIdentity>,
    expected: &leveler_core::BuildIdentity,
) -> anyhow::Result<()> {
    if reported.is_some_and(|reported| reported.is_known() && reported == expected) {
        return Ok(());
    }
    match classify_runtime(reported, expected) {
        RuntimeConsistency::Current => Ok(()),
        RuntimeConsistency::Outdated { runtime, expected } => anyhow::bail!(
            "the local runtime started as {} but this CodeLeveler is {}; \
             the runtime was not replaced correctly",
            runtime.short(),
            expected.short()
        ),
        RuntimeConsistency::ConfigChanged => {
            anyhow::bail!("the replacement runtime loaded a different configuration generation")
        }
        RuntimeConsistency::Unknown => {
            anyhow::bail!("the local runtime started but did not report a usable build identity")
        }
    }
}

#[cfg(any(unix, windows))]
async fn runtime_is_current(
    client: &LocalSocketRuntimeClient,
    layout: &Layout,
) -> anyhow::Result<RuntimeConsistency> {
    let expected = leveler_core::BuildIdentity::current();
    let reported = LocalRuntimeService::runtime_info(client).await.ok();
    let expected_config = leveler_app::runtime_config_fingerprint(layout)?;
    Ok(classify_runtime_generation(
        reported.as_ref(),
        &expected,
        &expected_config,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffAction {
    Interrupt,
    Force,
}

#[derive(Debug, Clone)]
pub enum HandoffEvent {
    Status(RuntimeHealth),
    HandshakeUnavailable,
    ForceAvailable,
    StateUnavailable,
    NoStalledTurns,
    InterruptRequested,
    InterruptDeliveryFailed,
    ForceBlocked,
    ForceRequested,
    ForceFailed(String),
}

/// The shell supplies its own locale, text, stderr and terminal input.
/// Runtime host sees only typed events and actions.
pub trait HandoffUi: Send + Sync {
    fn emit(&self, event: HandoffEvent);
    fn input(&self) -> Option<UnboundedReceiver<HandoffAction>>;
    fn render_ensure_error(&self, error: &EnsureError) -> String {
        error.to_string()
    }
}

/// The shell chooses its executable and readiness-file namespace. The host
/// supplies the serve argv and the detached process lifetime policy.
#[cfg(any(unix, windows))]
#[derive(Debug, Clone)]
pub struct DetachedRuntimeLaunch {
    pub executable: PathBuf,
    pub ready_prefix: String,
}

#[derive(Debug, thiserror::Error)]
pub enum EnsureError {
    #[error("runtime generation is unknown")]
    UnknownGeneration,
    #[error("runtime did not become ready within {seconds}s; log: {}", log_path.display())]
    ReadyTimeout { seconds: u64, log_path: PathBuf },
    /// `tail` is the daemon's own last lines. The message carries it because a
    /// caller that only sees this error has no other way to read why the
    /// process it launched refused to start — collecting the reason and then
    /// printing only the path was losing the only evidence there is.
    #[error(
        "runtime exited during startup; log: {}\n{tail}",
        log_path.display()
    )]
    StartupFailed { log_path: PathBuf, tail: String },
}

pub fn handoff_key(health: &RuntimeHealth) -> String {
    let turns = health
        .turn_blockers
        .iter()
        .map(|blocker| {
            format!(
                "{}:{}",
                blocker.session_id.as_str(),
                blocker.suspected_stalled_after(leveler_client_protocol::STALE_TURN_WARN_AFTER)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{}|{}|{}|{}|{}",
        health.active_turns,
        health.active_background_tasks,
        health.quiescent(),
        health
            .blockers
            .iter()
            .map(|blocker| blocker.task_id.as_str())
            .collect::<Vec<_>>()
            .join(","),
        turns,
    )
}

pub fn stalled_turn_sessions(health: &RuntimeHealth) -> Vec<SessionId> {
    health
        .turn_blockers
        .iter()
        .filter(|blocker| {
            blocker.suspected_stalled_after(leveler_client_protocol::STALE_TURN_WARN_AFTER)
        })
        .map(|blocker| blocker.session_id.clone())
        .collect()
}

pub fn force_handover_allowed(health: &RuntimeHealth) -> bool {
    health.active_background_tasks == 0
}

#[cfg(any(unix, windows))]
async fn request_turn_interrupts(
    client: &LocalSocketRuntimeClient,
    sessions: &[SessionId],
) -> usize {
    let mut sent = 0;
    for session_id in sessions {
        if client
            .send(ClientCommand::CancelCurrentTurn {
                session_id: session_id.clone(),
            })
            .await
            .is_ok()
        {
            sent += 1;
        }
    }
    sent
}

#[cfg(any(unix, windows))]
async fn apply_handover_action(
    client: &LocalSocketRuntimeClient,
    action: HandoffAction,
    health: Option<&RuntimeHealth>,
    ui: &dyn HandoffUi,
    grace: Duration,
    grace_deadline: &mut Option<tokio::time::Instant>,
) {
    let Some(health) = health else {
        ui.emit(HandoffEvent::StateUnavailable);
        return;
    };
    match action {
        HandoffAction::Interrupt => {
            let stalled = stalled_turn_sessions(health);
            if stalled.is_empty() {
                ui.emit(HandoffEvent::NoStalledTurns);
                return;
            }
            if request_turn_interrupts(client, &stalled).await > 0 {
                *grace_deadline = Some(tokio::time::Instant::now() + grace);
                ui.emit(HandoffEvent::InterruptRequested);
            } else {
                ui.emit(HandoffEvent::InterruptDeliveryFailed);
            }
        }
        HandoffAction::Force => {
            if !force_handover_allowed(health) {
                ui.emit(HandoffEvent::ForceBlocked);
                return;
            }
            match client
                .send(ClientCommand::ForceRetire {
                    reason: RestartReason::BuildMismatch,
                })
                .await
            {
                Ok(()) => ui.emit(HandoffEvent::ForceRequested),
                Err(error) => ui.emit(HandoffEvent::ForceFailed(error.to_string())),
            }
        }
    }
}

/// Wait without a deadline for the old owner to drain. A replacement PID on
/// the same socket completes handoff even if another client won the race.
#[cfg(any(unix, windows))]
pub async fn observe_retiring_runtime(
    client: &LocalSocketRuntimeClient,
    socket_path: &Path,
    observed_pid: Option<u32>,
    interval: Duration,
    ui: &dyn HandoffUi,
) {
    observe_retiring_runtime_with_grace(
        client,
        socket_path,
        observed_pid,
        interval,
        HANDOVER_CANCEL_GRACE,
        ui,
    )
    .await;
}

#[cfg(any(unix, windows))]
async fn observe_retiring_runtime_with_grace(
    client: &LocalSocketRuntimeClient,
    socket_path: &Path,
    observed_pid: Option<u32>,
    interval: Duration,
    grace: Duration,
    ui: &dyn HandoffUi,
) {
    let mut last = String::new();
    let mut input: Option<UnboundedReceiver<HandoffAction>> = None;
    let mut grace_deadline: Option<tokio::time::Instant> = None;
    let mut force_hint_shown = false;
    loop {
        let (key, health) = match LocalRuntimeService::runtime_info(client).await {
            Ok(info) => {
                if observed_pid.is_some_and(|pid| info.pid != pid) {
                    return;
                }
                (handoff_key(&info.health), Some(info.health))
            }
            Err(_) => {
                if LocalSocketRuntimeClient::connect(socket_path)
                    .await
                    .is_err()
                {
                    return;
                }
                ("handshake-unavailable".to_string(), None)
            }
        };
        if key != last {
            match health.as_ref() {
                Some(health) => ui.emit(HandoffEvent::Status(health.clone())),
                None => ui.emit(HandoffEvent::HandshakeUnavailable),
            }
            last = key;
        }
        if let Some(deadline) = grace_deadline
            && tokio::time::Instant::now() >= deadline
        {
            let still_stalled = health
                .as_ref()
                .is_some_and(|health| !stalled_turn_sessions(health).is_empty());
            if !still_stalled {
                grace_deadline = None;
            } else if !force_hint_shown {
                ui.emit(HandoffEvent::ForceAvailable);
                force_hint_shown = true;
            }
        }
        match input.as_mut() {
            Some(rx) => {
                tokio::select! {
                    biased;
                    message = rx.recv() => match message {
                        Some(action) => {
                            apply_handover_action(
                                client, action, health.as_ref(), ui, grace, &mut grace_deadline,
                            ).await;
                        }
                        None => input = None,
                    },
                    _ = tokio::time::sleep(interval) => {}
                }
            }
            None => {
                if health
                    .as_ref()
                    .is_some_and(|health| !stalled_turn_sessions(health).is_empty())
                {
                    input = ui.input();
                }
                tokio::time::sleep(interval).await;
            }
        }
    }
}

#[cfg(any(unix, windows))]
async fn retire_runtime(
    client: &LocalSocketRuntimeClient,
    socket_path: &Path,
    reason: RestartReason,
    ui: &dyn HandoffUi,
) -> anyhow::Result<()> {
    let observed_pid = LocalRuntimeService::runtime_info(client)
        .await
        .ok()
        .map(|info| info.pid);
    client
        .send(ClientCommand::ShutdownWhenIdle { reason })
        .await
        .map_err(|error| anyhow::anyhow!("could not ask the local runtime to retire: {error}"))?;
    observe_retiring_runtime(
        client,
        socket_path,
        observed_pid,
        HANDOVER_STATUS_INTERVAL,
        ui,
    )
    .await;
    Ok(())
}

/// Resolve an indexed session through its durable source before connecting.
/// The index grants no execution authority; the selected owner still applies
/// ownership and capability policy when the caller submits work.
#[cfg(any(unix, windows))]
pub async fn connect_global_task_runtime(
    home: &leveler_core::LevelerHome,
    source_id: &str,
    session_id: &SessionId,
    config_dir: Option<PathBuf>,
    launch: &DetachedRuntimeLaunch,
    ui: Arc<dyn HandoffUi>,
) -> anyhow::Result<LocalSocketRuntimeClient> {
    let (database, workspace) =
        leveler_app::global_task_index::resolve_global_task_source(home, source_id, session_id)
            .await?;
    let layout = match workspace {
        Some(workspace) => {
            let root = PathBuf::from(workspace);
            if !root.is_dir() {
                anyhow::bail!(
                    "the task's primary workspace is unavailable: {}",
                    root.display()
                );
            }
            Layout::ephemeral(root, config_dir, home.root())
        }
        None => Layout::no_workspace(home.clone(), config_dir),
    };
    if layout.database_path() != database {
        anyhow::bail!("the indexed source identity does not match its workspace owner");
    }
    ensure_default_runtime(&layout, launch, ui).await
}

#[cfg(any(unix, windows))]
pub async fn ensure_default_runtime(
    layout: &Layout,
    launch: &DetachedRuntimeLaunch,
    ui: Arc<dyn HandoffUi>,
) -> anyhow::Result<LocalSocketRuntimeClient> {
    let socket_path = layout.socket_path();
    if let Some(client) = probe_default_runtime(&socket_path).await? {
        match runtime_is_current(&client, layout).await? {
            RuntimeConsistency::Current => return Ok(client),
            RuntimeConsistency::Unknown => {
                return Err(EnsureError::UnknownGeneration.into());
            }
            RuntimeConsistency::Outdated { runtime, expected } => {
                tracing::info!(
                    runtime = %runtime.short(),
                    expected = %expected.short(),
                    "local runtime is a different build; asking it to retire"
                );
                retire_runtime(
                    &client,
                    &socket_path,
                    RestartReason::BuildMismatch,
                    ui.as_ref(),
                )
                .await?;
            }
            RuntimeConsistency::ConfigChanged => {
                tracing::info!(
                    "local runtime loaded a different configuration generation; asking it to retire"
                );
                retire_runtime(
                    &client,
                    &socket_path,
                    RestartReason::ConfigChanged,
                    ui.as_ref(),
                )
                .await?;
            }
        }
    }

    std::fs::create_dir_all(&layout.state_dir)?;
    let log_path = layout.state_dir.join("daemon.log");
    let log = std::fs::File::create(&log_path)?;
    let ready_path = std::env::temp_dir().join(format!(
        "{}-{}-{}.json",
        launch.ready_prefix,
        std::process::id(),
        leveler_core::new_uuid_string()
    ));
    let _ = std::fs::remove_file(&ready_path);
    let mut command = tokio::process::Command::new(&launch.executable);
    command
        .env("LEVELER_HOME", layout.home().root())
        .arg("--config-dir")
        .arg(&layout.config_dir);
    if let Some(workspace) = layout.primary_workspace() {
        command.arg("--repo").arg(workspace).arg("serve");
    } else {
        command.arg("serve").arg("--no-workspace");
    }
    command
        .arg("--ready-json")
        .arg(&ready_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    configure_detached_runtime(&mut command);
    let mut child = command.spawn()?;

    let deadline = tokio::time::Instant::now() + DAEMON_ENSURE_TIMEOUT;
    let client = loop {
        let child_done = child.try_wait()?.is_some();
        if ready_path.is_file()
            && let Ok(client) = LocalSocketRuntimeClient::connect(&socket_path).await
        {
            break client;
        }
        if child_done && let Some(client) = probe_default_runtime(&socket_path).await? {
            break client;
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.start_kill();
            return Err(EnsureError::ReadyTimeout {
                seconds: DAEMON_ENSURE_TIMEOUT.as_secs(),
                log_path,
            }
            .into());
        }
        if child_done {
            let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
            let tail = tail.lines().rev().take(8).collect::<Vec<_>>();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            return Err(EnsureError::StartupFailed {
                log_path,
                tail: tail.join("\n"),
            }
            .into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let _ = std::fs::remove_file(&ready_path);
    match LocalRuntimeService::runtime_info(&client).await {
        Ok(info) => tracing::info!(
            runtime_id = %info.runtime_id,
            pid = info.pid,
            "connected to local runtime"
        ),
        Err(error) => tracing::debug!(%error, "local runtime did not report an identity"),
    }
    let reported = LocalRuntimeService::runtime_info(&client).await.ok();
    verify_replacement(
        reported.as_ref().map(|info| &info.build),
        &leveler_core::BuildIdentity::current(),
    )?;
    let expected_config = leveler_app::runtime_config_fingerprint(layout)?;
    if reported
        .as_ref()
        .and_then(|info| info.config_fingerprint.as_deref())
        != Some(expected_config.as_str())
    {
        anyhow::bail!("the replacement runtime did not load the current configuration generation");
    }
    Ok(client)
}

#[cfg(any(unix, windows))]
pub struct DaemonReviver {
    layout: Layout,
    launch: DetachedRuntimeLaunch,
    ui: Arc<dyn HandoffUi>,
}

#[cfg(any(unix, windows))]
impl DaemonReviver {
    pub fn new(layout: Layout, launch: DetachedRuntimeLaunch, ui: Arc<dyn HandoffUi>) -> Self {
        Self { layout, launch, ui }
    }
}

#[cfg(any(unix, windows))]
#[async_trait::async_trait]
impl RuntimeReviver for DaemonReviver {
    async fn revive(&self) -> Result<(), String> {
        ensure_default_runtime(&self.layout, &self.launch, self.ui.clone())
            .await
            .map(|_probe_client| ())
            .map_err(|error| {
                error
                    .downcast_ref::<EnsureError>()
                    .map(|host| self.ui.render_ensure_error(host))
                    .unwrap_or_else(|| error.to_string())
            })
    }
}

#[cfg(all(test, unix))]
mod probe_tests {
    use super::*;

    #[tokio::test]
    async fn default_probe_bounds_transport_handshake_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stall.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let mut connections = Vec::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                connections.push(stream);
            }
        });
        let result =
            tokio::time::timeout(Duration::from_millis(150), probe_default_runtime(&path)).await;
        server.abort();
        assert!(
            matches!(result, Ok(Ok(None))),
            "a listening endpoint with a stalled handshake must not block startup"
        );
    }
}

/// Platform process isolation only; discovery and lifetime remain shared.
#[cfg(any(unix, windows))]
fn configure_detached_runtime(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: do not share the caller's
    // console lifetime or console control events. Handles remain explicit.
    command.creation_flags(0x0000_0008 | 0x0000_0200);
}
