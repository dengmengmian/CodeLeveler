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
/// Bound on the readiness handshake of a daemon this process just spawned.
///
/// The 50ms probe default is a DISCOVERY budget: it exists so a stale socket
/// cannot delay startup. Readiness is the opposite question — a starting daemon
/// may legitimately need longer to answer while it loads — but it still has to
/// be bounded, or `DAEMON_ENSURE_TIMEOUT` can be bypassed by a socket that
/// accepts the connection and never replies to the subscribe handshake.
#[cfg(any(unix, windows))]
const READY_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(any(unix, windows))]
const DAEMON_ENSURE_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(any(unix, windows))]
const HANDOVER_STATUS_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(any(unix, windows))]
const HANDOVER_CANCEL_GRACE: Duration = Duration::from_secs(5);
/// How many times a retirement decision may be refused with
/// `GenerationChanged` before this client stops re-deciding. A runtime being
/// replaced under us is a real fact; a loop re-asking forever is not.
#[cfg(any(unix, windows))]
const MAX_GENERATION_CHANGES: u32 = 3;
/// How long a handover wait may run before the client reports that it is still
/// pending. Observability only: it changes no ownership and cancels nothing.
#[cfg(any(unix, windows))]
const HANDOVER_STATUS_REFRESH: Duration = Duration::from_secs(30);
/// Default budget for a drain nobody can be asked about.
///
/// This bounds the ONE case a shell with no handover prompt cannot resolve: a
/// runtime that accepts the connection and never answers. A runtime that DOES
/// answer `Busy` is reported immediately (see
/// [`HandoffEvent::UpgradeDeferred`]), so the budget is never the reason a
/// person waits — and it is deliberately short, because "we cannot tell" is not
/// something to sit on for minutes. `0` still means "wait forever", an explicit
/// operator choice.
#[cfg(any(unix, windows))]
const DEFAULT_DRAIN_DECISION_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(any(unix, windows))]
const DRAIN_DECISION_TIMEOUT_ENV: &str = "LEVELER_HANDOVER_DRAIN_TIMEOUT_SECS";

/// How long a shell that cannot answer may wait for an old runtime to drain.
///
/// A shell WITH a handover prompt keeps waiting for as long as the old runtime
/// needs: protecting running work is the point, and the operator can interrupt
/// or force at any time. A shell WITHOUT one (the Desktop bridge, a piped or CI
/// TUI, a TUI started by another program) cannot make that decision, so it must
/// not pretend to — it reports the blocked handover instead of blocking with no
/// terminal state.
///
/// `None` means "wait forever", the historical behaviour, kept as an explicit
/// opt-in (`0`) rather than the default.
#[cfg(any(unix, windows))]
fn drain_decision_timeout() -> Option<Duration> {
    parse_drain_decision_timeout(std::env::var(DRAIN_DECISION_TIMEOUT_ENV).ok().as_deref())
}

/// Pure half of [`drain_decision_timeout`], so the parsing (the only part with
/// a decision in it) is testable without touching process environment.
#[cfg(any(unix, windows))]
fn parse_drain_decision_timeout(raw: Option<&str>) -> Option<Duration> {
    match raw {
        None => Some(DEFAULT_DRAIN_DECISION_TIMEOUT),
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(Duration::from_secs(secs)),
            Err(error) => {
                tracing::warn!(%error, value = %raw, "ignoring unparseable handover drain timeout");
                Some(DEFAULT_DRAIN_DECISION_TIMEOUT)
            }
        },
    }
}

/// Bounded connect, shared by discovery and readiness.
///
/// The two error kinds that mean "no runtime is listening here" fold into
/// `None`; every other transport error is real and is returned. A timeout is
/// also `None`: from the caller's side, an endpoint that will not complete the
/// handshake is not usable, and the caller's own deadline owns the verdict.
#[cfg(any(unix, windows))]
async fn connect_within(
    path: &Path,
    timeout: Duration,
) -> Result<Option<LocalSocketRuntimeClient>, TransportError> {
    match tokio::time::timeout(timeout, LocalSocketRuntimeClient::connect(path)).await {
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
                timeout_ms = timeout.as_millis(),
                "timed out connecting to local runtime"
            );
            Ok(None)
        }
    }
}

/// Probe the default per-repository endpoint without blocking TUI startup on
/// a stale socket. Web's direct attach intentionally uses a different API.
#[cfg(any(unix, windows))]
pub async fn probe_default_runtime(
    path: &Path,
) -> Result<Option<LocalSocketRuntimeClient>, TransportError> {
    connect_within(path, DEFAULT_DAEMON_CONNECT_TIMEOUT).await
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

/// The configuration generation a runtime serving `layout` must have loaded.
///
/// Re-exported so anything that has to *stand in* for such a runtime — a test
/// double, a fake daemon, a harness — states the same fact the client compares
/// against, instead of writing a second implementation of the digest and then
/// drifting from it.
pub fn expected_config_fingerprint(layout: &Layout) -> std::io::Result<String> {
    leveler_app::runtime_config_fingerprint(layout)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffAction {
    Interrupt,
    Force,
}

/// What a handover observed. The typed form IS the cross-shell contract: a
/// shell that cannot ask a human (the Desktop bridge, the Web server) forwards
/// this value, and a reader branches on it — never on a `Debug` rendering, which
/// is prose and drifts silently.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
    /// The drain outlived its status budget and is still running. Emitted once,
    /// so a shell can say how long the wait has lasted and what is holding it —
    /// a long handover must never look like a hung process.
    StillWaiting {
        waited_secs: u64,
        active_turns: u32,
        active_background_tasks: u32,
        /// Whether a force-retire would be accepted right now. False while a
        /// runtime-owned background task is alive, because a handover must not
        /// destroy a user-launched local process.
        can_force: bool,
    },
    /// The runtime was ASKED to retire and answered that it still owes work.
    ///
    /// Distinct from [`Self::Status`] on purpose: this is the fact that the
    /// upgrade is deferred and the previous runtime is unchanged — still
    /// admitting work, still serving every other client — which is the whole
    /// reason no fire-and-forget shutdown was sent.
    UpgradeDeferred {
        active_turns: u32,
        active_background_tasks: u32,
    },
    /// The runtime was ASKED to retire and did not answer at all.
    ///
    /// [`Self::UpgradeDeferred`] is a decision from the runtime; this is the
    /// absence of one. Nothing was signalled and nothing was changed: a client
    /// that cannot tell "busy" from "mute" must not act on either.
    MigrationFailed {
        reason: String,
    },
    /// A runtime too old to retire atomically was verified and is being
    /// replaced.
    ///
    /// Reported as its OWN event rather than a task outcome: this runtime's
    /// in-flight work is interrupted, and a reader must never see that as a
    /// normal `TaskFinished`.
    MigrationStarted {
        pid: u32,
        version: String,
    },
    /// The signal that was sent to that runtime, and whether it was the forced
    /// escalation.
    MigrationTerminating {
        pid: u32,
        force: bool,
    },
    /// The old runtime released the endpoint; the replacement may start.
    MigrationTerminated {
        pid: u32,
    },
}

/// The shell supplies its own locale, text, stderr and terminal input.
/// Runtime host sees only typed events and actions.
pub trait HandoffUi: Send + Sync {
    fn emit(&self, event: HandoffEvent);
    fn input(&self) -> Option<UnboundedReceiver<HandoffAction>>;
    /// Whether a human at this shell can answer a handover prompt at all.
    ///
    /// Distinct from [`Self::input`] on purpose: `input` may acquire the
    /// terminal (the TUI spawns a stdin reader), so it must stay lazy, while
    /// this answers the "can this shell decide?" question without starting
    /// anything. A shell that returns `false` gets the bounded
    /// [`EnsureError::RetireBlocked`] outcome instead of an unbounded wait.
    fn interactive(&self) -> bool {
        false
    }
    fn render_ensure_error(&self, error: &EnsureError) -> String {
        error.to_string()
    }
}

/// The handover UI of a shell with no operator at it and no prompt: the Web
/// aggregation server, the Desktop bridge, a scripted or CI client.
///
/// It makes the ONE choice such a shell can make honestly — never wait for a
/// human, never force — and records what it was told, so a programmatic caller
/// can report the reason instead of inventing one. The recorded strings are
/// Event `Debug` renderings, deliberately NOT a cross-process contract: callers
/// branch on the typed [`EnsureError`] they receive, which is the contract.
#[cfg(any(unix, windows))]
#[derive(Debug, Default)]
pub struct NonInteractiveHandoffUi {
    events: std::sync::Mutex<Vec<String>>,
}

#[cfg(any(unix, windows))]
impl NonInteractiveHandoffUi {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// What this shell observed, most recent last. Diagnostics only.
    pub fn observed(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

#[cfg(any(unix, windows))]
impl HandoffUi for NonInteractiveHandoffUi {
    fn emit(&self, event: HandoffEvent) {
        tracing::info!(event = ?event, "runtime handover event");
        self.events.lock().unwrap().push(format!("{event:?}"));
    }

    fn input(&self) -> Option<UnboundedReceiver<HandoffAction>> {
        None
    }

    fn interactive(&self) -> bool {
        false
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

/// What the startup loop can PROVE about the daemon it launched when it gives
/// up. Three distinct facts, deliberately not one "failed" label: a daemon that
/// is still starting is not a daemon that died, and a daemon that published
/// readiness but will not answer is neither.
#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupObservation {
    /// Our child is alive and has not published readiness yet.
    Starting,
    /// Readiness was published but the endpoint did not answer the handshake.
    /// The process is NOT proven dead, so nothing may be killed or replaced on
    /// the strength of this observation alone.
    Unresponsive,
    /// Our child exited without publishing readiness.
    ChildExited,
}

#[derive(Debug, thiserror::Error)]
pub enum EnsureError {
    #[error("runtime generation is unknown")]
    UnknownGeneration,
    /// The previous runtime is still working and this build will not take over
    /// behind the operator's back. Distinct from a startup failure: nothing
    /// failed, and nothing was cancelled — the old runtime still owns its work
    /// and is still serving. Raised only for a shell that cannot be asked; a
    /// shell with a handover prompt keeps waiting instead.
    #[error(
        "the previous runtime is still working after {waited_secs}s; \
         this build will not interrupt it to take over \
         (turns: {active_turns}, background tasks: {active_background_tasks})"
    )]
    RetireBlocked {
        waited_secs: u64,
        active_turns: u32,
        active_background_tasks: u32,
    },
    /// The previous runtime is a generation this build cannot share an endpoint
    /// with, and it could not be replaced: either it refused the forced
    /// migration (its process identity could not be proven) or it never
    /// answered the atomic retirement question. Nothing was signalled, nothing
    /// was cancelled, and the request must not be retried blindly — the
    /// operator has to decide.
    #[error(
        "the previous runtime at {} could not be replaced: {reason}",
        socket.display()
    )]
    LegacyMigrationRefused { socket: PathBuf, reason: String },
    #[error(
        "runtime did not become ready within {seconds}s ({observation:?}); log: {}",
        log_path.display()
    )]
    ReadyTimeout {
        seconds: u64,
        log_path: PathBuf,
        observation: StartupObservation,
    },
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

/// How a handover wait ended.
#[cfg(any(unix, windows))]
#[derive(Debug, Clone)]
pub enum DrainOutcome {
    /// The old owner released the endpoint (or a replacement took it). The
    /// caller may now start the new generation.
    Drained,
    /// The old owner answered that it still owes work and this shell cannot be
    /// asked whether to wait. Nothing was cancelled and nothing was killed: the
    /// old runtime still owns its work and is still serving, and it is still
    /// admitting new work.
    Deferred {
        active_turns: u32,
        active_background_tasks: u32,
    },
    /// The old owner is still working and this shell cannot be asked whether to
    /// wait, interrupt or force. Nothing was cancelled and nothing was killed:
    /// the old runtime still owns its work and is still serving.
    Undecided {
        waited: Duration,
        active_turns: u32,
        active_background_tasks: u32,
    },
    /// The handover could not be completed and MUST NOT be retried blindly: the
    /// previous runtime either refused a forced migration or never answered the
    /// atomic question. Nothing was signalled and nothing was changed.
    MigrationBlocked { reason: String },
}

/// Wait for the old owner to drain. A replacement PID on the same socket
/// completes handoff even if another client won the race.
///
/// The wait protects running work, so an operator who can be asked keeps
/// waiting indefinitely. An operator who cannot be asked gets
/// [`DrainOutcome::Undecided`] at the drain budget instead of a process that
/// never returns.
#[cfg(any(unix, windows))]
pub async fn observe_retiring_runtime(
    client: &LocalSocketRuntimeClient,
    socket_path: &Path,
    observed_pid: Option<u32>,
    interval: Duration,
    ui: &dyn HandoffUi,
) -> DrainOutcome {
    observe_retiring_runtime_with_grace(
        client,
        socket_path,
        observed_pid,
        interval,
        HANDOVER_CANCEL_GRACE,
        ui,
        None,
        None,
    )
    .await
}

#[cfg(any(unix, windows))]
async fn observe_retiring_runtime_with_grace(
    client: &LocalSocketRuntimeClient,
    socket_path: &Path,
    observed_pid: Option<u32>,
    interval: Duration,
    grace: Duration,
    ui: &dyn HandoffUi,
    requested: Option<RestartReason>,
    workspace: Option<&Path>,
) -> DrainOutcome {
    let mut last = String::new();
    let mut input: Option<UnboundedReceiver<HandoffAction>> = None;
    let mut grace_deadline: Option<tokio::time::Instant> = None;
    let mut force_hint_shown = false;
    let started = tokio::time::Instant::now();
    let drain_budget = drain_decision_timeout();
    // Report the pending wait on a fixed cadence, starting one refresh in. The
    // default drain budget is longer than one refresh, so a shell sees
    // "still waiting" several times before it ever sees a deadline.
    let mut next_report = HANDOVER_STATUS_REFRESH;
    // `Some(reason)` until this runtime has actually committed to retiring.
    // While it is still `Some`, the runtime has NOT closed admission, so other
    // clients keep working; the upgrade is deferred, not forced.
    let mut pending = requested;
    let mut deferral_reported: Option<String> = None;
    let mut generation_changes = 0u32;
    loop {
        let info = LocalRuntimeService::runtime_info(client).await.ok();
        if info.is_none()
            // Bounded: an endpoint that accepts and then stalls must not be
            // able to hold the handover wait open by refusing to answer, and a
            // refused endpoint means the retiring runtime is gone.
            && connect_within(socket_path, DEFAULT_DAEMON_CONNECT_TIMEOUT)
                .await
                .ok()
                .flatten()
                .is_none()
        {
            return DrainOutcome::Drained;
        }
        if let Some(info) = info.as_ref()
            && observed_pid.is_some_and(|pid| info.pid != pid)
        {
            return DrainOutcome::Drained;
        }
        let health = info.as_ref().map(|info| info.health.clone());
        // ---- Ask for the decision, when it has not been made yet -----------
        if let (Some(reason), Some(info)) = (pending, info.as_ref()) {
            let request = leveler_client_protocol::RetireRequest {
                reason,
                expected_build: info.build.clone(),
                expected_pid: info.pid,
            };
            match client.try_retire_if_idle(request).await {
                Ok(leveler_client_protocol::RetireDecision::Accepted) => {
                    tracing::info!(?reason, "the previous runtime accepted retirement");
                    pending = None;
                }
                Ok(leveler_client_protocol::RetireDecision::AlreadyRetiring { reason }) => {
                    tracing::info!(?reason, "the previous runtime is already retiring");
                    pending = None;
                }
                Ok(leveler_client_protocol::RetireDecision::Busy {
                    active_turns,
                    active_background_tasks,
                }) => {
                    // The runtime answered NO and changed nothing: it is still
                    // admitting work and still serving. Report the deferral
                    // once per distinct blocker set, then wait for it to clear.
                    let key = format!("{active_turns}:{active_background_tasks}");
                    if deferral_reported.as_deref() != Some(key.as_str()) {
                        ui.emit(HandoffEvent::UpgradeDeferred {
                            active_turns,
                            active_background_tasks,
                        });
                        deferral_reported = Some(key);
                    }
                    // A shell that cannot be asked has no way to resolve a
                    // deferral, so it must not be held here: it reports the
                    // state and returns. Waiting is only meaningful for a
                    // shell that can decide (keep waiting, interrupt, force) —
                    // everything else just looks hung.
                    if !ui.interactive() {
                        return DrainOutcome::Deferred {
                            active_turns,
                            active_background_tasks,
                        };
                    }
                }
                Ok(leveler_client_protocol::RetireDecision::GenerationChanged) => {
                    // Another client already replaced this generation, or the
                    // runtime serving us is not the one we examined. Re-observe
                    // and decide again, a bounded number of times so a runtime
                    // that keeps changing under us ends the wait rather than
                    // spinning.
                    generation_changes += 1;
                    if generation_changes > MAX_GENERATION_CHANGES {
                        return DrainOutcome::Undecided {
                            waited: started.elapsed(),
                            active_turns: health.as_ref().map_or(0, |h| h.active_turns),
                            active_background_tasks: health
                                .as_ref()
                                .map_or(0, |h| h.active_background_tasks),
                        };
                    }
                    tokio::time::sleep(interval).await;
                    continue;
                }
                Ok(leveler_client_protocol::RetireDecision::Unsupported) => {
                    // The endpoint positively stated that it has no atomic
                    // retirement. Historical runtimes may be replaced without
                    // an uninterrupted-upgrade guarantee, but only after the
                    // process serving this socket is PROVEN to be the one this
                    // client examined. Any refusal is terminal and reported:
                    // a guess here is a signal delivered to the wrong process.
                    match force_migrate_legacy_runtime(socket_path, workspace, reason, ui).await {
                        Ok(()) => pending = None,
                        Err(refusal) => {
                            tracing::warn!(%refusal, "refusing to force-migrate the previous runtime");
                            ui.emit(HandoffEvent::MigrationFailed {
                                reason: refusal.to_string(),
                            });
                            return DrainOutcome::MigrationBlocked {
                                reason: refusal.to_string(),
                            };
                        }
                    }
                }
                Err(error) => {
                    // No usable answer at all: not a decision, and not a
                    // capability statement either. Nothing here may be read as
                    // consent, so nothing is signalled and no local state is
                    // changed. This is deliberately NOT the historical
                    // fire-and-forget drain: sending it would stop the runtime
                    // admitting work on the strength of a request it never
                    // answered.
                    tracing::warn!(
                        %error,
                        "the previous runtime did not answer the atomic retirement question"
                    );
                    ui.emit(HandoffEvent::MigrationFailed {
                        reason: error.to_string(),
                    });
                    return DrainOutcome::MigrationBlocked {
                        reason: format!(
                            "the previous runtime did not answer the atomic retirement question: \
                             {error}"
                        ),
                    };
                }
            }
        }
        let key = health
            .as_ref()
            .map(handoff_key)
            .unwrap_or_else(|| "handshake-unavailable".to_string());
        if key != last {
            match health.as_ref() {
                Some(health) => ui.emit(HandoffEvent::Status(health.clone())),
                None => ui.emit(HandoffEvent::HandshakeUnavailable),
            }
            last = key;
        }
        // The budget is a DECISION deadline, not a kill switch: it is reported
        // before it is acted on, and acting on it only ends the client's wait.
        if started.elapsed() >= next_report
            || drain_budget.is_some_and(|budget| started.elapsed() >= budget)
        {
            let (turns, background) = health
                .as_ref()
                .map(|health| (health.active_turns, health.active_background_tasks))
                .unwrap_or((0, 0));
            ui.emit(HandoffEvent::StillWaiting {
                waited_secs: started.elapsed().as_secs(),
                active_turns: turns,
                active_background_tasks: background,
                can_force: health.as_ref().is_some_and(force_handover_allowed),
            });
            next_report = started.elapsed() + HANDOVER_STATUS_REFRESH;
            if let Some(budget) = drain_budget
                && started.elapsed() >= budget
                && !ui.interactive()
            {
                return DrainOutcome::Undecided {
                    waited: started.elapsed(),
                    active_turns: turns,
                    active_background_tasks: background,
                };
            }
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
                // Hand the human an escape hatch as soon as work is visible, and
                // also whenever the decision came back `Busy`: a deferred
                // upgrade is exactly the moment an operator may want to
                // interrupt or force instead of waiting.
                if pending.is_some()
                    || health
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

/// How long a forced migration waits for the graceful signal before escalating,
/// and again before giving up on the forced one.
#[cfg(any(unix, windows))]
const LEGACY_TERMINATION_GRACE: Duration = Duration::from_secs(5);
/// How often the migration re-reads the endpoint while waiting.
#[cfg(any(unix, windows))]
const LEGACY_TERMINATION_POLL: Duration = Duration::from_millis(50);
/// Bound on one endpoint identity read inside the migration loop. Short on
/// purpose: this is a liveness probe of a process that has just been signalled,
/// not a request a person is waiting on.
#[cfg(any(unix, windows))]
const LEGACY_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// What an endpoint says about itself right now.
#[cfg(any(unix, windows))]
async fn observe_endpoint(socket_path: &Path) -> Option<(u32, String)> {
    let client = connect_within(socket_path, LEGACY_PROBE_TIMEOUT)
        .await
        .ok()
        .flatten()?;
    let info = LocalRuntimeService::runtime_info(&client).await.ok()?;
    Some((info.pid, info.runtime_id.as_str().to_string()))
}

/// Replace a runtime that positively cannot retire atomically.
///
/// The procedure is deliberately narrow. Every step either re-proves the same
/// three facts or stops:
///
/// 1. read the endpoint's identity from a FRESH connection (a pid observed a
///    second ago is a pid that may already have been reused);
/// 2. require the identity to be usable and to differ from this build, and the
///    socket to be a socket owned by a runtime, and witness the pid by start
///    time ([`legacy::verify_target`]);
/// 3. send `SIGTERM` only after re-reading BOTH the endpoint identity and the
///    process witness, so the signal cannot land on a process that took the pid
///    in between;
/// 4. escalate to `SIGKILL` only after the graceful signal was ignored AND the
///    witness still matches;
/// 5. return only once the endpoint no longer answers as that runtime.
///
/// A replacement that already owns the endpoint also ends the migration: the
/// old generation is gone either way, and the caller's next step is the same.
///
/// The runtime's process GROUP is never signalled. Background services are held
/// by the independent execution host and are not this migration's to destroy.
#[cfg(any(unix, windows))]
async fn force_migrate_legacy_runtime(
    socket_path: &Path,
    workspace: Option<&Path>,
    reason: RestartReason,
    ui: &dyn HandoffUi,
) -> Result<(), crate::legacy::MigrationRefusal> {
    use crate::legacy::{
        MigrationRefusal, TerminationSignal, process_witness, revalidate, signal_process,
        verify_target,
    };

    let expected = leveler_core::BuildIdentity::current();
    let client = connect_within(socket_path, LEGACY_PROBE_TIMEOUT)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| {
            MigrationRefusal::IdentityUnknown(format!(
                "{} is not answering, so nothing can be proven about it",
                socket_path.display()
            ))
        })?;
    let info = LocalRuntimeService::runtime_info(&client)
        .await
        .map_err(|error| MigrationRefusal::IdentityUnknown(error.to_string()))?;
    let target = verify_target(
        socket_path,
        info.pid,
        info.runtime_id.as_str(),
        &info.build,
        &expected,
        workspace,
    )?;

    tracing::info!(
        pid = target.pid,
        runtime_id = %target.runtime_id,
        version = %target.build.version,
        ownership = ?target.ownership,
        ?reason,
        event = "LegacyRuntimeMigration",
        "replacing a runtime with no atomic retirement"
    );
    ui.emit(HandoffEvent::MigrationStarted {
        pid: target.pid,
        version: target.build.version.clone(),
    });

    let started = tokio::time::Instant::now();
    let mut sent: Option<TerminationSignal> = None;
    loop {
        match observe_endpoint(socket_path).await {
            // Nothing owns the endpoint any more: the old owner is gone.
            None => break,
            // A different runtime already owns the endpoint. The old
            // generation is gone; this migration has nothing left to do.
            Some((pid, runtime_id)) if pid != target.pid || runtime_id != target.runtime_id => {
                tracing::info!(
                    pid,
                    %runtime_id,
                    "a different runtime already owns the endpoint"
                );
                break;
            }
            Some(_) => {}
        }
        // The same runtime is still serving. Re-prove that the pid is still the
        // process that was witnessed BEFORE sending anything at it.
        if let Err(refusal) = revalidate(&target) {
            return Err(refusal);
        }
        let elapsed = started.elapsed();
        match sent {
            None => {
                signal_process(target.pid, TerminationSignal::Term).map_err(|error| {
                    MigrationRefusal::ProcessUnverifiable(format!(
                        "could not signal pid {}: {error}",
                        target.pid
                    ))
                })?;
                sent = Some(TerminationSignal::Term);
                ui.emit(HandoffEvent::MigrationTerminating {
                    pid: target.pid,
                    force: false,
                });
            }
            Some(TerminationSignal::Term) if elapsed >= LEGACY_TERMINATION_GRACE => {
                signal_process(target.pid, TerminationSignal::Kill).map_err(|error| {
                    MigrationRefusal::ProcessUnverifiable(format!(
                        "could not force-signal pid {}: {error}",
                        target.pid
                    ))
                })?;
                sent = Some(TerminationSignal::Kill);
                ui.emit(HandoffEvent::MigrationTerminating {
                    pid: target.pid,
                    force: true,
                });
            }
            Some(TerminationSignal::Kill) if elapsed >= LEGACY_TERMINATION_GRACE * 2 => {
                return Err(MigrationRefusal::DidNotExit {
                    pid: target.pid,
                    waited_secs: elapsed.as_secs(),
                });
            }
            Some(_) => {}
        }
        tokio::time::sleep(LEGACY_TERMINATION_POLL).await;
    }

    // Confirmed: the endpoint does not answer as the old runtime any more, and
    // [`process_witness`] is only consulted to describe the outcome honestly.
    let exited = process_witness(target.pid).is_none_or(|witness| witness != target.witness);
    tracing::info!(
        pid = target.pid,
        runtime_id = %target.runtime_id,
        exited,
        waited_ms = started.elapsed().as_millis() as u64,
        event = "LegacyRuntimeTerminated",
        "the runtime with no atomic retirement released the endpoint"
    );
    ui.emit(HandoffEvent::MigrationTerminated { pid: target.pid });
    Ok(())
}

/// Settle a retirement end to end: obtain the runtime's authoritative decision
/// (unless a caller already obtained it), then wait for the endpoint to be
/// released. One loop, so the decision, its reporting and the wait can never
/// disagree about what this client is waiting for.
#[cfg(any(unix, windows))]
async fn retire_runtime(
    client: &LocalSocketRuntimeClient,
    socket_path: &Path,
    reason: RestartReason,
    ui: &dyn HandoffUi,
    workspace: Option<&Path>,
) -> anyhow::Result<()> {
    let observed_pid = LocalRuntimeService::runtime_info(client)
        .await
        .ok()
        .map(|info| info.pid);
    let outcome = observe_retiring_runtime_with_grace(
        client,
        socket_path,
        observed_pid,
        HANDOVER_STATUS_INTERVAL,
        HANDOVER_CANCEL_GRACE,
        ui,
        Some(reason),
        workspace,
    )
    .await;
    match outcome {
        DrainOutcome::Drained => Ok(()),
        DrainOutcome::Deferred {
            active_turns,
            active_background_tasks,
        } => Err(EnsureError::RetireBlocked {
            waited_secs: 0,
            active_turns,
            active_background_tasks,
        }
        .into()),
        DrainOutcome::Undecided {
            waited,
            active_turns,
            active_background_tasks,
        } => Err(EnsureError::RetireBlocked {
            waited_secs: waited.as_secs(),
            active_turns,
            active_background_tasks,
        }
        .into()),
        DrainOutcome::MigrationBlocked { reason } => Err(EnsureError::LegacyMigrationRefused {
            socket: socket_path.to_path_buf(),
            reason,
        }
        .into()),
    }
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

/// The ONE generation decision every shell makes before it attaches to or
/// starts a runtime.
///
/// A shell's job here is not to decide what a runtime *is* — it is to reuse
/// this answer. Web, Desktop and the terminal all faced the same question
/// ("the endpoint is answering; is it MY generation?") and each used to answer
/// it in its own way, which is how the Web could attach to a runtime built from
/// different sources and then serve new work through it.
///
/// Returned outcomes, all of them from the existing lifecycle vocabulary:
///
/// - `Ok(Some(client))` — a runtime of this generation is serving the endpoint;
///   attach to it.
/// - `Ok(None)` — nothing of this generation owns the endpoint any more. Either
///   nothing was listening, or a different generation was handed over (or
///   force-migrated after its identity was verified). The caller may start one.
/// - `Err(EnsureError::UnknownGeneration)` — a runtime is answering but cannot
///   state which generation it is. Never silently adopted.
/// - `Err(EnsureError::RetireBlocked)` — the previous runtime owes work and this
///   shell cannot ask an operator whether to wait. It is UNCHANGED and still
///   serving; the caller reports it and must not start a competitor.
/// - `Err(EnsureError::LegacyMigrationRefused)` — the previous runtime cannot be
///   replaced, and its process identity could not be proven. Nothing was
///   signalled.
#[cfg(any(unix, windows))]
pub async fn reconcile_runtime_generation(
    socket_path: &Path,
    layout: &Layout,
    ui: &dyn HandoffUi,
) -> anyhow::Result<Option<LocalSocketRuntimeClient>> {
    let Some(client) = probe_default_runtime(socket_path).await? else {
        return Ok(None);
    };
    match runtime_is_current(&client, layout).await? {
        RuntimeConsistency::Current => Ok(Some(client)),
        RuntimeConsistency::Unknown => Err(EnsureError::UnknownGeneration.into()),
        RuntimeConsistency::Outdated { runtime, expected } => {
            tracing::info!(
                runtime = %runtime.short(),
                expected = %expected.short(),
                "local runtime is a different build; asking it to retire"
            );
            retire_runtime(
                &client,
                socket_path,
                RestartReason::BuildMismatch,
                ui,
                layout.primary_workspace(),
            )
            .await?;
            Ok(None)
        }
        RuntimeConsistency::ConfigChanged => {
            tracing::info!(
                "local runtime loaded a different configuration generation; asking it to retire"
            );
            retire_runtime(
                &client,
                socket_path,
                RestartReason::ConfigChanged,
                ui,
                layout.primary_workspace(),
            )
            .await?;
            Ok(None)
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub async fn reconcile_runtime_generation(
    _socket_path: &Path,
    _layout: &Layout,
    _ui: &dyn HandoffUi,
) -> anyhow::Result<Option<LocalSocketRuntimeClient>> {
    Ok(None)
}

#[cfg(any(unix, windows))]
pub async fn ensure_default_runtime(
    layout: &Layout,
    launch: &DetachedRuntimeLaunch,
    ui: Arc<dyn HandoffUi>,
) -> anyhow::Result<LocalSocketRuntimeClient> {
    let socket_path = layout.socket_path();
    if let Some(client) = reconcile_runtime_generation(&socket_path, layout, ui.as_ref()).await? {
        return Ok(client);
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
    let mut observation = StartupObservation::Starting;
    let client = loop {
        let child_done = child.try_wait()?.is_some();
        // Readiness is answered by the endpoint, not by the ready file alone:
        // a losing racer never writes one, and the winner is the daemon this
        // client should adopt. Both connects are bounded, so a socket that
        // accepts and then stalls cannot outlive `DAEMON_ENSURE_TIMEOUT`.
        if ready_path.is_file() {
            observation = StartupObservation::Unresponsive;
            if let Some(client) = connect_within(&socket_path, READY_CONNECT_TIMEOUT).await? {
                break client;
            }
        }
        if child_done {
            if let Some(client) = connect_within(&socket_path, READY_CONNECT_TIMEOUT).await? {
                break client;
            }
            observation = StartupObservation::ChildExited;
        }
        if tokio::time::Instant::now() >= deadline {
            // Deliberately NOT killed. A daemon that is still starting must not
            // be destroyed by the client that launched it: the reported
            // failure is this client's liveness bound, not a verdict on the
            // process. It stays detached (its own process group, no
            // kill-on-drop) and self-reclaims through the daemon's own idle
            // eviction if nothing ever attaches to it, so a later probe can
            // still adopt it once it does become ready.
            return Err(EnsureError::ReadyTimeout {
                seconds: DAEMON_ENSURE_TIMEOUT.as_secs(),
                log_path,
                observation,
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
    /// One revival at a time. The request path and the event-stream reconnect
    /// path revive the same runtime, and `ensure_default_runtime` only probes
    /// the endpoint once at its start: two concurrent revivals each spawn a
    /// daemon, and the loser exits while racing the winner's readiness — and
    /// whatever bookkeeping its caller keeps about the process it launched.
    /// Serialized, the second caller adopts the first one's daemon instead.
    gate: tokio::sync::Mutex<()>,
}

#[cfg(any(unix, windows))]
impl DaemonReviver {
    pub fn new(layout: Layout, launch: DetachedRuntimeLaunch, ui: Arc<dyn HandoffUi>) -> Self {
        Self {
            layout,
            launch,
            ui,
            gate: tokio::sync::Mutex::new(()),
        }
    }
}

#[cfg(any(unix, windows))]
#[async_trait::async_trait]
impl RuntimeReviver for DaemonReviver {
    async fn revive(&self) -> Result<(), String> {
        let _gate = self.gate.lock().await;
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

    /// The drain budget is a policy with exactly one surprising value: `0`
    /// means "wait forever". Everything unparseable must land on the default
    /// rather than on "forever", because silence is not consent.
    #[test]
    fn the_drain_budget_only_means_forever_when_asked_for_it() {
        assert_eq!(
            parse_drain_decision_timeout(None),
            Some(DEFAULT_DRAIN_DECISION_TIMEOUT)
        );
        assert_eq!(parse_drain_decision_timeout(Some("0")), None);
        assert_eq!(parse_drain_decision_timeout(Some(" 0 ")), None);
        assert_eq!(
            parse_drain_decision_timeout(Some("7")),
            Some(Duration::from_secs(7))
        );
        for garbage in ["", "forever", "-1", "1.5", "1s"] {
            assert_eq!(
                parse_drain_decision_timeout(Some(garbage)),
                Some(DEFAULT_DRAIN_DECISION_TIMEOUT),
                "{garbage:?} must not disable the budget"
            );
        }
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
