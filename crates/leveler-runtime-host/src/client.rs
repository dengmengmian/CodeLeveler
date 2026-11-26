use std::path::{Path, PathBuf};
#[cfg(any(unix, windows))]
use std::process::Stdio;
#[cfg(any(unix, windows))]
use std::sync::Arc;
#[cfg(any(unix, windows))]
use std::time::Duration;

#[cfg(any(unix, windows))]
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, RestartReason, RetireRequest,
};
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
/// Settle time before the deciding probe after two handshake closes: long
/// enough for a dead listener's queued connection to drain, short enough that
/// a genuine pre-token runtime is still identified promptly.
#[cfg(any(unix, windows))]
const PEER_CLOSE_SETTLE: Duration = Duration::from_millis(25);
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

/// A legacy `runtime_info` is one read on an endpoint that is already
/// accepting. The bound keeps startup prompt; an unanswered observation means
/// "cannot prove busy", which lands on the conservative refusal.
#[cfg(unix)]
const LEGACY_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(2);

/// A cooperative retirement request is sent only after two observations agree
/// that the runtime owes no work and has no attached local UI. This is the
/// settle between them: a UI that is reconnecting attaches within it, and a
/// freshly admitted turn shows up in the runtime's own health.
#[cfg(unix)]
const LEGACY_RETIREMENT_SETTLE: Duration = Duration::from_millis(500);
#[cfg(unix)]
const LEGACY_RETIREMENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a programmatic shell waits for a quiescent legacy runtime that
/// accepted the request to release its endpoint. A shell with an operator
/// waits as long as the drain needs; this bound only keeps a bridge or CI
/// caller from spinning on a state it cannot see.
#[cfg(unix)]
const LEGACY_RETIREMENT_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

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
    match probe_endpoint(path, timeout).await? {
        EndpointProbe::Ready(client) => Ok(Some(client)),
        EndpointProbe::Absent | EndpointProbe::NoAnswer { .. } => Ok(None),
        #[cfg(unix)]
        EndpointProbe::NotReady => Ok(None),
    }
}

/// Test-only seam for the credential-publication race.
///
/// The window being modelled is BETWEEN a failed credential read and the
/// liveness connect that follows it. A test installs a one-shot action that
/// publishes the credential and binds the listener during that window; no
/// production build contains this module or the call site it serves.
#[cfg(all(test, unix))]
mod credential_race_hook {
    use std::path::Path;
    use std::sync::Mutex;

    type Action = Box<dyn Fn(&Path) + Send + Sync + 'static>;

    static ACTION: Mutex<Option<Action>> = Mutex::new(None);

    pub(super) fn install(action: Action) {
        *ACTION.lock().unwrap() = Some(action);
    }

    pub(super) fn clear() {
        *ACTION.lock().unwrap() = None;
    }

    pub(super) fn fire(path: &Path) {
        let action = ACTION.lock().unwrap();
        if let Some(action) = action.as_ref() {
            action(path);
        }
    }
}

/// What a bounded connect learned about an endpoint.
///
/// `Absent`, `NotReady` and `NoAnswer` are deliberately distinct: a refused
/// connection is proof that nothing owns the endpoint; a listener whose
/// credential was published while the probe was running is a runtime that is
/// still starting; and a connection that is accepted and then never completes
/// the handshake proves somebody is listening but did not answer. Callers that
/// report a failure must not collapse these facts into one another.
#[cfg(any(unix, windows))]
pub(super) enum EndpointProbe {
    Ready(LocalSocketRuntimeClient),
    Absent,
    /// A listener is live and its credential was published while this probe
    /// was running. The runtime is starting, not absent and not legacy; the
    /// caller's bounded wait owns the next observation. The credential file
    /// only exists on Unix local sockets, so only that platform can observe
    /// this state.
    #[cfg(unix)]
    NotReady,
    NoAnswer {
        timeout: Duration,
    },
}

#[cfg(any(unix, windows))]
async fn probe_endpoint(path: &Path, timeout: Duration) -> Result<EndpointProbe, TransportError> {
    probe_endpoint_as(
        path,
        timeout,
        leveler_local_transport::ClientKind::LocalInteractive,
    )
    .await
}

#[cfg(any(unix, windows))]
pub(super) async fn probe_endpoint_as(
    path: &Path,
    timeout: Duration,
    kind: leveler_local_transport::ClientKind,
) -> Result<EndpointProbe, TransportError> {
    let connect = || LocalSocketRuntimeClient::connect_as(path, kind);
    match tokio::time::timeout(timeout, connect()).await {
        Ok(Ok(client)) => Ok(EndpointProbe::Ready(client)),
        Ok(Err(TransportError::Io(error))) if error.kind() == std::io::ErrorKind::NotFound => {
            // A tokenless endpoint can mean three different things. If a
            // listener accepts AND the credential is still missing, it is an
            // older runtime that predates the trusted-local credential:
            // report the migration fact instead of treating it as absent and
            // racing a second bind. If the credential appeared between that
            // read and this connect, it is a runtime that is starting right
            // now — its bind sequence installs the credential first, so a
            // visible listener can always be authenticated. If nothing
            // accepts, this is an ordinary stale socket.
            #[cfg(unix)]
            {
                #[cfg(test)]
                credential_race_hook::fire(path);
                match tokio::time::timeout(timeout, tokio::net::UnixStream::connect(path)).await {
                    Ok(Ok(_)) => {
                        if leveler_local_transport::access_token_is_published(path)? {
                            return Ok(EndpointProbe::NotReady);
                        }
                        return Err(TransportError::PredatesAccessToken(path.to_path_buf()));
                    }
                    Ok(Err(error)) if endpoint_unavailable(&error) => {}
                    Ok(Err(error)) => return Err(error.into()),
                    Err(_) => return Ok(EndpointProbe::NoAnswer { timeout }),
                }
            }
            Ok(EndpointProbe::Absent)
        }
        Ok(Err(TransportError::Io(error))) if endpoint_unavailable(&error) => {
            tracing::debug!(%error, socket = %path.display(), "skipping unavailable local runtime");
            Ok(EndpointProbe::Absent)
        }
        Ok(Err(TransportError::PeerClosed)) => {
            // A peer that closes the handshake may be a live pre-token runtime
            // that cannot decode it, or a current runtime that died while this
            // call was in flight. One close cannot tell them apart: an
            // accepted-then-killed listener produces the same EOF, and a
            // connection already queued in its backlog can close the same way.
            // A live pre-token runtime keeps accepting and closing; a dead
            // listener drains its backlog once. So require a third close after
            // a settle wait before reading the sequence as a capability
            // statement.
            let mut closes = 1u8;
            loop {
                if closes >= 2 {
                    tokio::time::sleep(PEER_CLOSE_SETTLE).await;
                }
                match tokio::time::timeout(timeout, connect()).await {
                    Ok(Ok(client)) => return Ok(EndpointProbe::Ready(client)),
                    Ok(Err(TransportError::PeerClosed)) => {
                        closes += 1;
                        if closes >= 3 {
                            return Err(TransportError::PredatesAccessToken(path.to_path_buf()));
                        }
                    }
                    Ok(Err(TransportError::Io(error))) if endpoint_unavailable(&error) => {
                        // The listener is gone: the earlier closes were a
                        // runtime dying, not a runtime refusing the handshake.
                        return Ok(EndpointProbe::Absent);
                    }
                    Ok(Err(TransportError::NoAnswer { timeout_ms, .. })) => {
                        return Ok(EndpointProbe::NoAnswer {
                            timeout: Duration::from_millis(timeout_ms),
                        });
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(_) => return Ok(EndpointProbe::NoAnswer { timeout }),
                }
            }
        }
        Ok(Err(TransportError::NoAnswer { timeout_ms, .. })) => Ok(EndpointProbe::NoAnswer {
            timeout: Duration::from_millis(timeout_ms),
        }),
        Ok(Err(error)) => Err(error),
        Err(_) => {
            tracing::debug!(
                socket = %path.display(),
                timeout_ms = timeout.as_millis(),
                "local runtime accepted the connection but did not answer"
            );
            Ok(EndpointProbe::NoAnswer { timeout })
        }
    }
}

/// Whether a transport error means no runtime owns this endpoint. Linux can
/// report a closed listener as ECONNRESET during the subscribe handshake, the
/// same fact as ECONNREFUSED; macOS can report EPIPE when the closed listener
/// leaves no peer to receive that handshake, and Windows named pipes report
/// EOF when the server accepted and then closed without a reply. None says the
/// endpoint is recoverable.
#[cfg(any(unix, windows))]
fn endpoint_unavailable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
    )
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
        RuntimeConsistency::Outdated { runtime, expected } => Err(EnsureError::BuildMismatch {
            expected,
            observed: runtime,
        }
        .into()),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffAction {
    Interrupt,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyHandoffBlock {
    AccessTokenUnavailable,
    AtomicRetirementUnavailable,
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
    /// The shell is waiting for the previous owner to exit on its own.
    ///
    /// The counts are what that owner reported about itself, `None` when it
    /// could not be observed. They are facts, never a guess: a shell must not
    /// promise a wait that the reported state contradicts.
    LegacyExitRequired {
        block: LegacyHandoffBlock,
        version: Option<String>,
        pid: Option<u32>,
        active_turns: Option<u32>,
        active_background_tasks: Option<u32>,
        local_waiters: Option<usize>,
    },
    /// A verified, idle runtime too old to authenticate a handover accepted a
    /// cooperative retirement request.
    ///
    /// The old runtime stops admitting work and exits after the work it owns
    /// settles. Reported as its own event so the shell can say what actually
    /// happened instead of implying the user must stop the process themselves.
    LegacyRetirementRequested {
        version: Option<String>,
        pid: Option<u32>,
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
    /// An initial terminal launch can keep observing a legacy endpoint until
    /// the operator exits the old instance or cancels this launch.
    fn waits_for_legacy_exit(&self) -> bool {
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
    /// A runtime holds the endpoint but has not published readiness yet.
    /// Usually our child; after an election loss it is the winner.
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
    /// and is still serving. Interactive shells may wait on modern retirement;
    /// legacy work without atomic retirement is always deferred.
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
    /// with, and it could not be replaced: it either lacks atomic retirement
    /// or did not answer that question. Nothing was signalled, nothing
    /// was cancelled, and the request must not be retried blindly — the
    /// operator has to decide.
    #[error(
        "the previous runtime at {} could not be replaced: {reason}",
        socket.display()
    )]
    LegacyMigrationRefused { socket: PathBuf, reason: String },
    /// A pre-token runtime ACCEPTED cooperative retirement and is still
    /// releasing its endpoint.
    ///
    /// Distinct from [`Self::LegacyMigrationRefused`]: the old runtime did not
    /// refuse, it is draining its own work after closing admission. Nothing is
    /// wrong and nothing was cancelled; a later launch (or a shell that can
    /// wait) attaches once the endpoint is released. Typed so a programmatic
    /// caller can retry without parsing a sentence.
    #[error(
        "the previous runtime at {} accepted retirement and is still releasing its endpoint",
        socket.display()
    )]
    LegacyRetirementPending { socket: PathBuf },
    /// The runtime that answered is a different artifact from this client.
    ///
    /// Typed, not prose: the revive loop must be able to recognise a stable
    /// build conflict without parsing a sentence. Both identities are carried
    /// because the diagnosis needs the side that refused as well as the side
    /// that was expected.
    #[error(
        "the local runtime started as {} but this CodeLeveler is {}; \
         the runtime was not replaced correctly",
        observed.diagnostic(),
        expected.diagnostic()
    )]
    BuildMismatch {
        expected: leveler_core::BuildIdentity,
        observed: leveler_core::BuildIdentity,
    },
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
    /// The connected peer cannot confirm atomic retirement. A startup shell
    /// can keep observing after dropping its interactive subscription.
    LegacyExitRequired(Box<RuntimeInfo>),
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
    requested: Option<RetireRequest>,
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
    // `Some(request)` until this runtime has actually committed to retiring.
    // While it is still `Some`, the runtime has NOT closed admission, so other
    // clients keep working; the upgrade is deferred, not forced.
    let mut pending = requested.as_ref();
    let mut deferral_reported: Option<String> = None;
    let mut generation_changes = 0u32;
    loop {
        let info = LocalRuntimeService::runtime_info(client).await.ok();
        if info.is_none()
            && matches!(
                probe_endpoint_as(
                    socket_path,
                    DEFAULT_DAEMON_CONNECT_TIMEOUT,
                    leveler_local_transport::ClientKind::Remote,
                )
                .await,
                Ok(EndpointProbe::Absent)
            )
        {
            return DrainOutcome::Drained;
        }
        if let Some(info) = info.as_ref()
            && (observed_pid.is_some_and(|pid| info.pid != pid)
                || requested
                    .as_ref()
                    .is_some_and(|request| info.build != request.expected_build))
        {
            return DrainOutcome::Drained;
        }
        let health = info.as_ref().map(|info| info.health.clone());
        // ---- Ask for the decision, when it has not been made yet -----------
        if let (Some(request), Some(info)) = (pending, info.as_ref()) {
            let reason = request.reason;
            match client.try_retire_if_idle(request.clone()).await {
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
                    // A legacy health snapshot cannot close admission: even
                    // apparent idleness can race a newly admitted turn. Keep
                    // the old owner untouched; only it may settle its work and
                    // exit normally before a replacement starts.
                    if ui.waits_for_legacy_exit() {
                        return DrainOutcome::LegacyExitRequired(Box::new(info.clone()));
                    }
                    if let Some(health) = health.as_ref()
                        && (health.active_turns > 0 || health.active_background_tasks > 0)
                    {
                        ui.emit(HandoffEvent::UpgradeDeferred {
                            active_turns: health.active_turns,
                            active_background_tasks: health.active_background_tasks,
                        });
                        return DrainOutcome::Deferred {
                            active_turns: health.active_turns,
                            active_background_tasks: health.active_background_tasks,
                        };
                    }
                    let reason = "the previous runtime does not support atomic retirement; \
                                  let its owner exit normally before upgrading"
                        .to_string();
                    ui.emit(HandoffEvent::MigrationFailed {
                        reason: reason.clone(),
                    });
                    return DrainOutcome::MigrationBlocked { reason };
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

/// Settle a retirement end to end: obtain the runtime's authoritative decision
/// (unless a caller already obtained it), then wait for the endpoint to be
/// released. One loop, so the decision, its reporting and the wait can never
/// disagree about what this client is waiting for.
#[cfg(any(unix, windows))]
async fn retire_runtime(
    client: LocalSocketRuntimeClient,
    socket_path: &Path,
    observed: &RuntimeInfo,
    reason: RestartReason,
    ui: &dyn HandoffUi,
) -> anyhow::Result<()> {
    // Each IPC call reconnects. Only the generation that was classified as
    // outdated may be retired, even if another launch takes over now.
    let request = RetireRequest {
        reason,
        expected_build: observed.build.clone(),
        expected_pid: observed.pid,
    };
    let outcome = observe_retiring_runtime_with_grace(
        &client,
        socket_path,
        Some(observed.pid),
        HANDOVER_STATUS_INTERVAL,
        HANDOVER_CANCEL_GRACE,
        ui,
        Some(request),
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
        DrainOutcome::LegacyExitRequired(info) => {
            // The startup probe has a local subscription. Keeping it while
            // we wait would prevent an idle legacy runtime from self-evicting.
            drop(client);
            let observed = LegacyObservation {
                info: (*info).clone(),
                local_waiters: None,
            };
            wait_for_legacy_exit(
                socket_path,
                LegacyHandoffBlock::AtomicRetirementUnavailable,
                Some(&observed),
                ui,
            )
            .await;
            Ok(())
        }
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
/// - `Ok(Some(client))` — this generation is serving the endpoint; attach to it.
/// - `Ok(None)` — nothing of this generation owns the endpoint any more. Either
///   nothing was listening, or a different generation handed over atomically.
///   The caller may start one.
/// - `Err(EnsureError::UnknownGeneration)` — a runtime is answering but cannot
///   state which generation it is. Never silently adopted.
/// - `Err(EnsureError::RetireBlocked)` — the previous runtime owes work and this
///   shell cannot wait, or that generation lacks atomic retirement. It is UNCHANGED and still
///   serving; the caller reports it and must not start a competitor.
/// - `Err(EnsureError::LegacyMigrationRefused)` — the previous runtime cannot be
///   replaced because atomic retirement is unavailable or unconfirmed.
///   Its owner must exit normally first. Nothing was signalled.
#[cfg(any(unix, windows))]
pub async fn reconcile_runtime_generation(
    socket_path: &Path,
    layout: &Layout,
    ui: &dyn HandoffUi,
) -> anyhow::Result<Option<LocalSocketRuntimeClient>> {
    for _ in 0..=MAX_GENERATION_CHANGES {
        let probe = match probe_endpoint(socket_path, DEFAULT_DAEMON_CONNECT_TIMEOUT).await {
            Ok(EndpointProbe::NoAnswer { .. }) => {
                probe_endpoint(socket_path, READY_CONNECT_TIMEOUT).await
            }
            #[cfg(unix)]
            Ok(EndpointProbe::NotReady) => probe_endpoint(socket_path, READY_CONNECT_TIMEOUT).await,
            other => other,
        };
        let client = match probe {
            Ok(EndpointProbe::Ready(client)) => client,
            Ok(EndpointProbe::Absent) => return Ok(None),
            Ok(EndpointProbe::NoAnswer { timeout }) => {
                return Err(TransportError::NoAnswer {
                    operation: "the startup handshake",
                    timeout_ms: timeout.as_millis() as u64,
                }
                .into());
            }
            #[cfg(unix)]
            Ok(EndpointProbe::NotReady) => return Err(EnsureError::UnknownGeneration.into()),
            Err(TransportError::PredatesAccessToken(observed)) => {
                // A pre-token runtime cannot prove a generation or answer the
                // atomic retirement question, but every generation that answers
                // `runtime_info` also honors `shutdown_when_idle` over the same
                // unauthenticated protocol. That request is COOPERATIVE: the
                // old runtime stops admitting work and exits only after the
                // work it already owns drains. Send it only to an instance this
                // client can name by process facts, and only while it reports
                // no work and no attached local UI — the command closes
                // admission immediately, so a busy or watched runtime must keep
                // serving its own clients.
                let observation = observe_legacy(&observed).await;
                #[cfg(unix)]
                let requested = legacy_retirement_is_safe(layout, &observed, observation.as_ref())
                    .await
                    && request_legacy_retirement(&observed, observation.as_ref(), ui).await;
                #[cfg(not(unix))]
                let requested = false;
                if !ui.waits_for_legacy_exit() {
                    // Cooperative retirement of a pre-token runtime is a
                    // Unix-only compatibility path. The Windows endpoint has
                    // no pre-token generation to retire; keep the branch out
                    // of the Windows build instead of referencing Unix-only
                    // helpers that do not exist there.
                    #[cfg(unix)]
                    if requested {
                        if wait_for_endpoint_release_bounded(
                            &observed,
                            LEGACY_RETIREMENT_RELEASE_TIMEOUT,
                        )
                        .await
                        {
                            continue;
                        }
                        // The request WAS accepted; the runtime is draining,
                        // not refusing. A programmatic caller must retry, not
                        // start a competitor or claim the old build refused.
                        ui.emit(HandoffEvent::MigrationFailed {
                            reason: "the previous runtime accepted cooperative retirement and is \
                                     still releasing its endpoint; retry once it exits"
                                .to_string(),
                        });
                        return Err(EnsureError::LegacyRetirementPending {
                            socket: socket_path.to_path_buf(),
                        }
                        .into());
                    }
                    // No request was acknowledged. That does not prove the
                    // runtime refused: the frame may have been processed while
                    // its reply was lost, or another shell may have asked.
                    // Only the runtime's own current state can tell an
                    // accepted retirement from a refusal, so read it again.
                    if endpoint_released(
                        &observed,
                        LegacyHandoffBlock::AccessTokenUnavailable,
                        None,
                    )
                    .await
                    {
                        continue;
                    }
                    let after = observe_legacy(&observed).await;
                    if after
                        .as_ref()
                        .is_some_and(|observed| observed.info.health.shutting_down)
                    {
                        // The bounded release wait uses the Unix-only legacy
                        // handoff helpers; Windows has no pre-token generation
                        // to retire and reports the pending transition instead.
                        #[cfg(unix)]
                        if wait_for_endpoint_release_bounded(
                            &observed,
                            LEGACY_RETIREMENT_RELEASE_TIMEOUT,
                        )
                        .await
                        {
                            continue;
                        }
                        ui.emit(HandoffEvent::MigrationFailed {
                            reason: "the previous runtime is already retiring and is still \
                                     releasing its endpoint"
                                .to_string(),
                        });
                        return Err(EnsureError::LegacyRetirementPending {
                            socket: socket_path.to_path_buf(),
                        }
                        .into());
                    }
                    return Err(legacy_runtime_refusal(
                        socket_path,
                        after.as_ref().or(observation.as_ref()),
                        ui,
                    ));
                }
                if requested {
                    wait_for_legacy_handoff(layout, &observed, observation.as_ref(), ui, true)
                        .await;
                } else {
                    wait_for_legacy_handoff(layout, &observed, observation.as_ref(), ui, false)
                        .await;
                }
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let info = LocalRuntimeService::runtime_info(&client)
            .await
            .map_err(|error| {
                tracing::debug!(%error, "could not observe serving runtime generation");
                EnsureError::UnknownGeneration
            })?;
        let expected_config = leveler_app::runtime_config_fingerprint(layout)?;
        let reason = match classify_runtime_generation(
            Some(&info),
            &leveler_core::BuildIdentity::current(),
            &expected_config,
        ) {
            RuntimeConsistency::Current => return Ok(Some(client)),
            RuntimeConsistency::Unknown => return Err(EnsureError::UnknownGeneration.into()),
            RuntimeConsistency::Outdated { runtime, expected } => {
                tracing::info!(
                    runtime = %runtime.diagnostic(),
                    expected = %expected.diagnostic(),
                    "local runtime is a different build; asking it to retire"
                );
                RestartReason::BuildMismatch
            }
            RuntimeConsistency::ConfigChanged => {
                // Configuration normally reloads without replacing the process.
                tracing::info!(
                    "local runtime loaded an older configuration generation; asking it to reload"
                );
                if reload_runtime_config(&client).await.is_ok() {
                    return Ok(Some(client));
                }
                tracing::info!("runtime cannot reload configuration in place; asking it to retire");
                RestartReason::ConfigChanged
            }
        };
        retire_runtime(client, socket_path, &info, reason, ui).await?;
    }
    Err(EnsureError::UnknownGeneration.into())
}

/// What a pre-token endpoint said about itself, read without adopting it.
///
/// `info` is the runtime's own authoritative health report; `local_waiters` is
/// the transport's attached-UI count. Both are read-only requests that every
/// generation answered before the access token existed.
#[cfg(any(unix, windows))]
struct LegacyObservation {
    info: RuntimeInfo,
    local_waiters: Option<usize>,
}

#[cfg(any(unix, windows))]
async fn observe_legacy(_socket_path: &Path) -> Option<LegacyObservation> {
    #[cfg(unix)]
    {
        let (info, local_waiters) = tokio::join!(
            leveler_local_transport::legacy_runtime_info(_socket_path, LEGACY_OBSERVATION_TIMEOUT),
            leveler_local_transport::legacy_local_waiter_count(
                _socket_path,
                LEGACY_OBSERVATION_TIMEOUT
            ),
        );
        info.map(|info| LegacyObservation {
            info,
            local_waiters,
        })
    }
    #[cfg(windows)]
    None
}

/// Whether the runtime itself reports "no work owed right now".
#[cfg(unix)]
fn legacy_reports_no_work(info: &RuntimeInfo) -> bool {
    // A legacy reply that omits `turn_capacity` is not a populated health
    // object; its zeroed counts prove nothing and must never authorize a
    // retirement request.
    info.health.turn_capacity.is_some()
        && info.health.active_turns == 0
        && info.health.active_background_tasks == 0
        && info.health.quiescent()
        && !info.health.shutting_down
}

/// The kernel-visible identity of the runtime a gate verified.
///
/// A pid alone is a reusable number and a path alone can be rebound. The start
/// marker and the socket object are what make "the same instance" a fact the
/// settle can re-check instead of a claim the runtime made about itself.
#[cfg(unix)]
#[derive(Clone, Debug)]
struct LegacyIdentity {
    witness: crate::legacy::ProcessWitness,
    socket: crate::legacy::SocketObject,
}

/// The pure identity half of the cooperative-retirement gate: the runtime
/// reports no work, and the pid it names is a live process of this user whose
/// command line identifies it as a runtime for this workspace. The returned
/// witness and socket object are revalidated by the settle.
#[cfg(unix)]
fn legacy_verified_idle(
    socket_path: &Path,
    workspace: Option<&Path>,
    info: &RuntimeInfo,
) -> Option<LegacyIdentity> {
    if !legacy_reports_no_work(info) {
        return None;
    }
    if info.pid == 0
        || info.pid == std::process::id()
        || info.runtime_id.as_str().is_empty()
        || !info.build.is_known()
        || info.build == leveler_core::BuildIdentity::current()
    {
        return None;
    }
    let socket = crate::legacy::socket_object(socket_path)?;
    if !crate::legacy::process_alive(info.pid)
        || !crate::legacy::process_owned_by_current_user(info.pid)
    {
        return None;
    }
    let witness = crate::legacy::process_witness(info.pid)?;
    let command = crate::legacy::process_command(info.pid)?;
    if !crate::legacy::serves_workspace(&command, workspace) {
        return None;
    }
    Some(LegacyIdentity { witness, socket })
}

/// Whether the second observation still describes the instance the identity
/// gate verified. Kept pure so the settle's facts are testable without a real
/// PID reuse or socket rebind: the witness and the socket object are what
/// distinguish "the same process" from "the same number".
#[cfg(unix)]
fn legacy_settle_agrees(
    first: &RuntimeInfo,
    identity: &LegacyIdentity,
    second: &LegacyObservation,
    second_witness: Option<&crate::legacy::ProcessWitness>,
    second_socket: Option<&crate::legacy::SocketObject>,
) -> bool {
    second.local_waiters == Some(0)
        && second.info.pid == first.pid
        && second.info.runtime_id == first.runtime_id
        && legacy_reports_no_work(&second.info)
        && second_witness == Some(&identity.witness)
        && second_socket == Some(&identity.socket)
}

/// The identity gate plus the settle every caller must use: two observations,
/// `LEGACY_RETIREMENT_SETTLE` apart, must agree that the runtime is idle,
/// unattached, and still the SAME instance. A UI that attaches between the
/// probe and the request is not retired underneath, a runtime replaced under
/// the caller is re-examined instead of being asked on stale facts, and a pid
/// reused inside the window cannot pass for the process that was verified.
#[cfg(unix)]
async fn legacy_retirement_settles(
    socket_path: &Path,
    workspace: Option<&Path>,
    first: &RuntimeInfo,
) -> bool {
    let Some(identity) = legacy_verified_idle(socket_path, workspace, first) else {
        return false;
    };
    tokio::time::sleep(LEGACY_RETIREMENT_SETTLE).await;
    let Some(second) = observe_legacy(socket_path).await else {
        return false;
    };
    let witness = crate::legacy::process_witness(second.info.pid);
    let socket = crate::legacy::socket_object(socket_path);
    legacy_settle_agrees(first, &identity, &second, witness.as_ref(), socket.as_ref())
}

/// Observe `socket_path` and return the runtime only when it passes the SAME
/// gate the startup handshake uses, so `runtime stop` cannot be more willing
/// to close admission than a launch.
#[cfg(unix)]
pub(crate) async fn legacy_cooperative_retirement_target(
    socket_path: &Path,
    workspace: Option<&Path>,
) -> Option<RuntimeInfo> {
    let observation = observe_legacy(socket_path).await?;
    if observation.local_waiters != Some(0) {
        return None;
    }
    legacy_retirement_settles(socket_path, workspace, &observation.info)
        .await
        .then_some(observation.info)
}

/// The startup-path gate: the caller already has one observation (used for the
/// reported event); the settle re-observes the same instance.
#[cfg(unix)]
async fn legacy_retirement_is_safe(
    layout: &Layout,
    socket_path: &Path,
    observation: Option<&LegacyObservation>,
) -> bool {
    let Some(first) = observation else {
        return false;
    };
    first.local_waiters == Some(0)
        && legacy_retirement_settles(socket_path, layout.primary_workspace(), &first.info).await
}

/// Send the cooperative retirement request. A refusal is not a failure of the
/// launch: the caller keeps waiting and reports what the runtime did.
#[cfg(unix)]
async fn request_legacy_retirement(
    socket_path: &Path,
    observation: Option<&LegacyObservation>,
    ui: &dyn HandoffUi,
) -> bool {
    let Some(observed) = observation else {
        return false;
    };
    match leveler_local_transport::legacy_request_shutdown_when_idle(
        socket_path,
        RestartReason::BuildMismatch,
        LEGACY_RETIREMENT_REQUEST_TIMEOUT,
        observed.info.pid,
    )
    .await
    {
        Ok(()) => {
            tracing::info!(
                pid = observed.info.pid,
                version = %observed.info.version,
                "the idle legacy runtime accepted cooperative retirement"
            );
            ui.emit(HandoffEvent::LegacyRetirementRequested {
                version: (!observed.info.version.is_empty()).then(|| observed.info.version.clone()),
                pid: (observed.info.pid != 0).then_some(observed.info.pid),
            });
            true
        }
        Err(error) => {
            tracing::info!(
                %error,
                pid = observed.info.pid,
                "the legacy runtime did not accept cooperative retirement"
            );
            false
        }
    }
}

/// Wait until a legacy endpoint is released, without asking it to change state.
///
/// A current-generation runtime answering on the endpoint means the previous
/// owner released it (another shell finished the same handover). That is a
/// completed release, not "still draining": the caller must re-probe and
/// attach instead of reporting a pending retirement.
#[cfg(unix)]
async fn wait_for_endpoint_release_bounded(socket_path: &Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if endpoint_released(
            socket_path,
            LegacyHandoffBlock::AccessTokenUnavailable,
            None,
        )
        .await
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait for a pre-token runtime to hand over.
///
/// This wait is not passive forever: a runtime that was busy or watched when
/// the launch began can become a safe cooperative-retirement target later (its
/// last turn ends, the old UI attached to it is closed). Each refresh
/// re-observes the runtime and sends the same verified request, so the user
/// does not have to wait out the old build's own idle timer — the exact delay
/// this handover exists to remove. A runtime that never becomes safe is never
/// asked, and its state is reported instead.
///
/// A DIFFERENT pre-token instance appearing on the endpoint (the old owner
/// exited and an old client restarted it) is a new target: it is evaluated
/// again instead of inheriting the previous request.
#[cfg(unix)]
async fn wait_for_legacy_handoff(
    layout: &Layout,
    socket_path: &Path,
    previous: Option<&LegacyObservation>,
    ui: &dyn HandoffUi,
    requested: bool,
) {
    let mut requested = if requested {
        previous.map(|observed| (observed.info.pid, observed.info.runtime_id.clone()))
    } else {
        None
    };
    if requested.is_none() {
        emit_legacy_exit_required(
            socket_path,
            LegacyHandoffBlock::AccessTokenUnavailable,
            previous,
            ui,
        );
    }
    loop {
        tokio::time::sleep(HANDOVER_STATUS_INTERVAL).await;
        let current = observe_legacy(socket_path).await;
        if let Some((pid, runtime_id)) = requested.as_ref()
            && current.as_ref().is_some_and(|observed| {
                observed.info.pid != *pid || observed.info.runtime_id != *runtime_id
            })
        {
            // The instance this client asked is gone; a new one answers.
            requested = None;
        }
        if requested.is_none()
            && let Some(observed) = current.as_ref()
            && observed.local_waiters == Some(0)
            && legacy_retirement_settles(socket_path, layout.primary_workspace(), &observed.info)
                .await
            && request_legacy_retirement(socket_path, Some(observed), ui).await
        {
            requested = Some((observed.info.pid, observed.info.runtime_id.clone()));
        }
        if endpoint_released(
            socket_path,
            LegacyHandoffBlock::AccessTokenUnavailable,
            previous,
        )
        .await
        {
            return;
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_legacy_handoff(
    _layout: &Layout,
    socket_path: &Path,
    previous: Option<&LegacyObservation>,
    ui: &dyn HandoffUi,
    _requested: bool,
) {
    emit_legacy_exit_required(
        socket_path,
        LegacyHandoffBlock::AccessTokenUnavailable,
        previous,
        ui,
    );
    wait_for_endpoint_release(
        socket_path,
        LegacyHandoffBlock::AccessTokenUnavailable,
        previous,
    )
    .await;
}

/// One observation step: has the endpoint been released (or replaced)?
#[cfg(any(unix, windows))]
async fn endpoint_released(
    socket_path: &Path,
    block: LegacyHandoffBlock,
    previous: Option<&LegacyObservation>,
) -> bool {
    match probe_endpoint_as(
        socket_path,
        DEFAULT_DAEMON_CONNECT_TIMEOUT,
        leveler_local_transport::ClientKind::Remote,
    )
    .await
    {
        Ok(EndpointProbe::Absent) => true,
        Ok(EndpointProbe::Ready(client)) => {
            if block == LegacyHandoffBlock::AccessTokenUnavailable {
                return true;
            }
            if let Some(previous) = previous
                && let Ok(Ok(observed)) = tokio::time::timeout(
                    READY_CONNECT_TIMEOUT,
                    LocalRuntimeService::runtime_info(&client),
                )
                .await
                && (observed.pid != previous.info.pid
                    || observed.runtime_id != previous.info.runtime_id
                    || observed.build != previous.info.build)
            {
                return true;
            }
            false
        }
        Ok(_) | Err(TransportError::PredatesAccessToken(_)) => false,
        Err(error) => {
            tracing::debug!(%error, "could not observe legacy runtime exit");
            false
        }
    }
}

#[cfg(any(unix, windows))]
fn emit_legacy_exit_required(
    socket_path: &Path,
    block: LegacyHandoffBlock,
    previous: Option<&LegacyObservation>,
    ui: &dyn HandoffUi,
) {
    tracing::info!(
        ?block,
        socket = %socket_path.display(),
        pid = previous.map(|observed| observed.info.pid),
        build = previous.map(|observed| observed.info.build.diagnostic()),
        "waiting for legacy runtime exit"
    );
    let info = previous.map(|observed| &observed.info);
    ui.emit(HandoffEvent::LegacyExitRequired {
        block,
        version: info.and_then(|info| (!info.version.is_empty()).then(|| info.version.clone())),
        pid: info.and_then(|info| (info.pid != 0).then_some(info.pid)),
        active_turns: info.and_then(|info| {
            (info.health.turn_capacity.is_some()).then_some(info.health.active_turns)
        }),
        active_background_tasks: info.and_then(|info| {
            (info.health.turn_capacity.is_some()).then_some(info.health.active_background_tasks)
        }),
        local_waiters: previous.and_then(|observed| observed.local_waiters),
    });
}

/// A passive wait never asks a legacy peer to change state. When the endpoint
/// is released or its owner changes, the caller will use normal discovery and
/// ownership election. Failed observations do not prove release.
#[cfg(any(unix, windows))]
async fn wait_for_legacy_exit(
    socket_path: &Path,
    block: LegacyHandoffBlock,
    previous: Option<&LegacyObservation>,
    ui: &dyn HandoffUi,
) {
    tracing::info!(
        ?block,
        socket = %socket_path.display(),
        pid = previous.map(|observed| observed.info.pid),
        build = previous.map(|observed| observed.info.build.diagnostic()),
        "waiting for legacy runtime exit"
    );
    let info = previous.map(|observed| &observed.info);
    ui.emit(HandoffEvent::LegacyExitRequired {
        block,
        version: info.and_then(|info| (!info.version.is_empty()).then(|| info.version.clone())),
        pid: info.and_then(|info| (info.pid != 0).then_some(info.pid)),
        active_turns: info.and_then(|info| {
            (info.health.turn_capacity.is_some()).then_some(info.health.active_turns)
        }),
        active_background_tasks: info.and_then(|info| {
            (info.health.turn_capacity.is_some()).then_some(info.health.active_background_tasks)
        }),
        local_waiters: previous.and_then(|observed| observed.local_waiters),
    });
    wait_for_endpoint_release(socket_path, block, previous).await;
}

/// Observe the endpoint until its owner releases it (or is replaced).
///
/// Failed observations do not prove release: only an absent endpoint, or a
/// different generation answering on it, ends the wait.
#[cfg(any(unix, windows))]
async fn wait_for_endpoint_release(
    socket_path: &Path,
    block: LegacyHandoffBlock,
    previous: Option<&LegacyObservation>,
) {
    loop {
        tokio::time::sleep(HANDOVER_STATUS_INTERVAL).await;
        if endpoint_released(socket_path, block, previous).await {
            return;
        }
    }
}

/// Unattended callers report a legacy block without waiting on the operator.
#[cfg(any(unix, windows))]
fn legacy_runtime_refusal(
    socket_path: &Path,
    observation: Option<&LegacyObservation>,
    ui: &dyn HandoffUi,
) -> anyhow::Error {
    if let Some(observed) = observation {
        let info = &observed.info;
        let active_turns = info.health.active_turns;
        let active_background_tasks = info.health.active_background_tasks;
        if active_turns > 0 || active_background_tasks > 0 {
            ui.emit(HandoffEvent::UpgradeDeferred {
                active_turns,
                active_background_tasks,
            });
            return EnsureError::RetireBlocked {
                waited_secs: 0,
                active_turns,
                active_background_tasks,
            }
            .into();
        }
    }
    // Name the fact that actually blocks this launch. An attached local UI is
    // not "work"; reporting it as work would tell a programmatic caller to
    // wait for a turn that does not exist.
    let attached = observation
        .and_then(|observed| observed.local_waiters)
        .filter(|waiters| *waiters > 0)
        .map(|waiters| format!("; {waiters} local UI(s) are attached to it"))
        .unwrap_or_default();
    let reason = format!(
        "the previous runtime at {} predates the access token and does not support atomic \
         retirement{attached}; let it exit normally, then restart this build",
        socket_path.display()
    );
    ui.emit(HandoffEvent::MigrationFailed {
        reason: reason.clone(),
    });
    EnsureError::LegacyMigrationRefused {
        socket: socket_path.to_path_buf(),
        reason,
    }
    .into()
}

/// Ask a running runtime to re-read its configuration sources and apply them.
///
/// The command is acknowledged only after the reload handler ran, so a
/// successful answer means the runtime already published (or refused) the new
/// generation. A failure here is a transport/protocol failure — the command is
/// unknown to an older daemon, or the connection died — and the caller decides
/// what that means.
#[cfg(any(unix, windows))]
async fn reload_runtime_config(client: &LocalSocketRuntimeClient) -> anyhow::Result<()> {
    client
        .send(leveler_client_protocol::ClientCommand::ReloadConfig { query_id: None })
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))
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
            // The child lost the socket election, or it crashed. A loser exits
            // before the winner is listening. Keep probing inside this ensure
            // deadline instead of treating that exit as a permanent failure.
            // Refusal fails fast, so the budget below is only ever spent on an
            // endpoint that accepted the connection and stayed silent — and
            // that silence is an unanswered call, not a startup crash.
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let probe = remaining.min(READY_CONNECT_TIMEOUT);
            if probe.is_zero() {
                observation = StartupObservation::ChildExited;
            } else {
                match probe_endpoint(&socket_path, probe).await? {
                    EndpointProbe::Ready(client) => break client,
                    EndpointProbe::Absent => observation = StartupObservation::ChildExited,
                    // A listener whose credential was published mid-probe is
                    // another runtime still starting (typically the winner of
                    // this election). The ensure deadline owns the verdict;
                    // an immediate hard failure would turn a healthy start
                    // into a transport error.
                    #[cfg(unix)]
                    EndpointProbe::NotReady => observation = StartupObservation::Starting,
                    EndpointProbe::NoAnswer { timeout } => {
                        return Err(TransportError::NoAnswer {
                            operation: "the startup handshake",
                            timeout_ms: timeout.as_millis() as u64,
                        }
                        .into());
                    }
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            // Deliberately NOT killed. A daemon that is still starting must not
            // be destroyed by the client that launched it: the reported
            // failure is this client's liveness bound, not a verdict on the
            // process. It stays detached (its own process group, no
            // kill-on-drop) and self-reclaims through the daemon's own idle
            // eviction if nothing ever attaches to it, so a later probe can
            // still adopt it once it does become ready.
            if observation == StartupObservation::ChildExited {
                let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
                let tail = tail.lines().rev().take(8).collect::<Vec<_>>();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                return Err(EnsureError::StartupFailed {
                    log_path,
                    tail: tail.join("\n"),
                }
                .into());
            }
            return Err(EnsureError::ReadyTimeout {
                seconds: DAEMON_ENSURE_TIMEOUT.as_secs(),
                log_path,
                observation,
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
    /// The build observed while a revival failed. Kept only long enough for the
    /// transport to report it: the attach stays refused, but a known-different
    /// generation is a fact the user needs in order to restart.
    observed_build: std::sync::Mutex<Option<leveler_client_protocol::BuildIdentity>>,
    /// A revival that cannot make progress by trying again.
    ///
    /// `ensure_default_runtime` retires a different-build runtime it finds and
    /// spawns a replacement from `launch.executable`. When this client is a
    /// stale process, that replacement is the NEW binary on disk, so
    /// verification refuses it — and the next attempt retires that runtime and
    /// spawns yet another, forever. The 2026-10-09 logs are 137 such attempts
    /// in 6.7 minutes. The record pins the exact conflict (client build,
    /// observed build + pid, launch artifact) so an unchanged conflict is
    /// reported without touching the runtime; a change to any of those facts
    /// clears it and revival is attempted again.
    conflict: std::sync::Mutex<Option<BuildConflict>>,
    /// One revival at a time. The request path and the event-stream reconnect
    /// path revive the same runtime, and `ensure_default_runtime` only probes
    /// the endpoint once at its start: two concurrent revivals each spawn a
    /// daemon, and the loser exits while racing the winner's readiness — and
    /// whatever bookkeeping its caller keeps about the process it launched.
    /// Serialized, the second caller adopts the first one's daemon instead.
    gate: tokio::sync::Mutex<()>,
}

/// Identity of the artifact a revival re-executes. `len` plus `modified` is a
/// portable change detector; the endpoint's own build+pid is the primary
/// signal and this only answers "was the launch file replaced?".
#[cfg(any(unix, windows))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct LaunchStamp {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

#[cfg(any(unix, windows))]
impl LaunchStamp {
    fn of(path: &Path) -> Self {
        let metadata = std::fs::metadata(path).ok();
        Self {
            path: path.to_path_buf(),
            len: metadata.as_ref().map(std::fs::Metadata::len).unwrap_or(0),
            modified: metadata.as_ref().and_then(|m| m.modified().ok()),
        }
    }
}

/// A stable build conflict, kept as one record so repeated attempts log and
/// report the same correlation id instead of looking like fresh failures.
#[cfg(any(unix, windows))]
#[derive(Debug, Clone)]
struct BuildConflict {
    expected: leveler_core::BuildIdentity,
    observed: leveler_core::BuildIdentity,
    /// Diagnostic only: the instance that answered when the conflict was
    /// recorded. A later restart serving the SAME build does not clear it.
    observed_pid: Option<u32>,
    launch: LaunchStamp,
    correlation: String,
    attempts: u32,
    first_seen: std::time::Instant,
}

/// What a probe of the endpoint proved.
///
/// `Absent` and `Unknown` are deliberately different facts: an endpoint with no
/// listener can be retried, while a probe that timed out proves nothing and
/// must not become a reason to replace a runtime.
#[cfg(any(unix, windows))]
#[derive(Debug, Clone)]
enum RuntimeObservation {
    /// The endpoint answered with this process instance and build.
    Serving {
        pid: u32,
        build: leveler_client_protocol::BuildIdentity,
    },
    /// There is no listener at the endpoint.
    Absent,
    /// The probe failed: whether a runtime exists was not established.
    Unknown,
}

/// Whether a recorded conflict still describes reality. Only a fact that could
/// resolve the conflict is a reason to try again: the client build (not
/// possible without a restart, but stated), the launch artifact, an absent
/// runtime, or a runtime that now serves a different build.
///
/// A pid change alone does NOT clear it. A restarted instance that still serves
/// the same unconvertible build is the same conflict — retrying would retire it
/// and respawn from the same artifact, which is exactly the loop this record
/// exists to stop (observed live: a new pid every ~10s). A probe that cannot
/// verify the runtime is also NOT a change: replacing on an unproven absence is
/// the unsafe direction.
#[cfg(any(unix, windows))]
fn build_conflict_still_applies(
    recorded: &BuildConflict,
    expected: &leveler_core::BuildIdentity,
    launch: &LaunchStamp,
    observed: &RuntimeObservation,
) -> bool {
    if recorded.expected != *expected || recorded.launch != *launch {
        return false;
    }
    match observed {
        RuntimeObservation::Serving { build, .. } => *build == recorded.observed,
        RuntimeObservation::Absent => false,
        RuntimeObservation::Unknown => true,
    }
}

#[cfg(any(unix, windows))]
impl DaemonReviver {
    pub fn new(layout: Layout, launch: DetachedRuntimeLaunch, ui: Arc<dyn HandoffUi>) -> Self {
        Self {
            layout,
            launch,
            ui,
            observed_build: std::sync::Mutex::new(None),
            conflict: std::sync::Mutex::new(None),
            gate: tokio::sync::Mutex::new(()),
        }
    }
}

#[cfg(any(unix, windows))]
#[async_trait::async_trait]
impl RuntimeReviver for DaemonReviver {
    async fn revive(&self) -> Result<(), String> {
        let _gate = self.gate.lock().await;
        let recorded = self.conflict.lock().unwrap().clone();
        if let Some(recorded) = recorded {
            let expected = leveler_core::BuildIdentity::current();
            let launch = LaunchStamp::of(&self.launch.executable);
            let observed = observe_runtime_identity(&self.layout).await;
            if build_conflict_still_applies(&recorded, &expected, &launch, &observed) {
                let mut updated = recorded;
                updated.attempts = updated.attempts.saturating_add(1);
                tracing::warn!(
                    correlation = %updated.correlation,
                    attempt = updated.attempts,
                    elapsed_secs = updated.first_seen.elapsed().as_secs(),
                    expected = %updated.expected.diagnostic(),
                    observed = %updated.observed.diagnostic(),
                    observed_pid = ?updated.observed_pid,
                    observation = ?observed,
                    launch = %updated.launch.path.display(),
                    "runtime build conflict is unchanged; replacement is not re-attempted"
                );
                *self.observed_build.lock().unwrap() = Some(updated.observed.clone());
                *self.conflict.lock().unwrap() = Some(updated.clone());
                let error = EnsureError::BuildMismatch {
                    expected: updated.expected,
                    observed: updated.observed,
                };
                return Err(self.ui.render_ensure_error(&error));
            }
            tracing::info!(
                correlation = %recorded.correlation,
                "runtime build conflict no longer applies; revival may be attempted again"
            );
            *self.conflict.lock().unwrap() = None;
        }

        let result = ensure_default_runtime(&self.layout, &self.launch, self.ui.clone()).await;
        match &result {
            Ok(_) => {
                *self.observed_build.lock().unwrap() = None;
                *self.conflict.lock().unwrap() = None;
            }
            Err(error) => {
                // A mismatch is the only failure that a retry cannot resolve:
                // every attempt retires the runtime that just answered and
                // spawns another from the same artifact. Pin it instead.
                if let Some(EnsureError::BuildMismatch { expected, observed }) =
                    error.downcast_ref::<EnsureError>()
                {
                    let observed_pid = match observe_runtime_identity(&self.layout).await {
                        RuntimeObservation::Serving { pid, build } if build == *observed => {
                            Some(pid)
                        }
                        _ => None,
                    };
                    *self.observed_build.lock().unwrap() = Some(observed.clone());
                    let conflict = BuildConflict {
                        expected: expected.clone(),
                        observed: observed.clone(),
                        observed_pid,
                        launch: LaunchStamp::of(&self.launch.executable),
                        correlation: leveler_core::new_uuid_string(),
                        attempts: 1,
                        first_seen: std::time::Instant::now(),
                    };
                    tracing::warn!(
                        correlation = %conflict.correlation,
                        expected = %expected.diagnostic(),
                        observed = %observed.diagnostic(),
                        observed_pid = ?conflict.observed_pid,
                        launch = %self.launch.executable.display(),
                        "runtime build conflict detected; pausing replacement until the observed runtime or launch artifact changes"
                    );
                    *self.conflict.lock().unwrap() = Some(conflict);
                } else {
                    // A transient failure keeps no conflict: the runtime may
                    // still be starting, busy with work worth preserving, or
                    // gone by the next attempt.
                    *self.observed_build.lock().unwrap() =
                        observe_runtime_build(&self.layout).await;
                }
            }
        }
        result.map(|_probe_client| ()).map_err(|error| {
            error
                .downcast_ref::<EnsureError>()
                .map(|host| self.ui.render_ensure_error(host))
                .unwrap_or_else(|| error.to_string())
        })
    }

    fn observed_build(&self) -> Option<leveler_client_protocol::BuildIdentity> {
        self.observed_build.lock().unwrap().clone()
    }
}

/// Ask the endpoint for the build it is serving, without attaching or sending
/// a command. `Unknown` means nothing was proven.
#[cfg(any(unix, windows))]
async fn observe_runtime_build(layout: &Layout) -> Option<leveler_client_protocol::BuildIdentity> {
    match observe_runtime_identity(layout).await {
        RuntimeObservation::Serving { build, .. } => Some(build),
        RuntimeObservation::Absent | RuntimeObservation::Unknown => None,
    }
}

/// The process serving the endpoint and the build it reports.
///
/// A failed probe is NOT an absent runtime: a timeout or a handshake error
/// leaves the runtime's existence unproven, and the conflict policy must not
/// replace anything on that basis.
#[cfg(any(unix, windows))]
async fn observe_runtime_identity(layout: &Layout) -> RuntimeObservation {
    let socket = layout.socket_path();
    let client = match tokio::time::timeout(
        DEFAULT_DAEMON_CONNECT_TIMEOUT,
        LocalSocketRuntimeClient::connect(&socket),
    )
    .await
    {
        Ok(Ok(client)) => client,
        Ok(Err(TransportError::Io(error))) if endpoint_unavailable(&error) => {
            return RuntimeObservation::Absent;
        }
        Ok(Err(_)) | Err(_) => return RuntimeObservation::Unknown,
    };
    match LocalRuntimeService::runtime_info(&client).await {
        Ok(info) => RuntimeObservation::Serving {
            pid: info.pid,
            build: info.build,
        },
        Err(_) => RuntimeObservation::Unknown,
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
        std::fs::write(path.with_extension("token"), "a".repeat(64)).unwrap();
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

    // The platform-independent endpoint classification test lives in
    // `endpoint_classification_tests`, so it also runs on Windows.

    /// A runtime publishes its access token BEFORE it binds the socket, so a
    /// listener that is visible can always be authenticated. The probe still
    /// has a two-step observation (read the credential, then connect), and a
    /// credential published between those two steps used to be reported as a
    /// runtime that predates the token — a hard startup failure.
    ///
    /// The hook fires exactly between the failed read and the liveness
    /// connect, so the interleaving is deterministic rather than timed.
    #[tokio::test]
    async fn a_credential_published_during_the_probe_is_not_a_legacy_runtime() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("starting.sock");
        let listener_slot: Arc<std::sync::Mutex<Option<UnixListener>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot = listener_slot.clone();
        let expected = path.clone();
        credential_race_hook::install(Box::new(move |probed: &Path| {
            if probed != expected {
                return;
            }
            // The starting runtime installs the credential, then publishes the
            // endpoint — the order the fixed bind sequence guarantees.
            std::fs::write(expected.with_extension("token"), "b".repeat(64)).unwrap();
            *slot.lock().unwrap() = Some(UnixListener::bind(&expected).unwrap());
        }));

        let outcome = probe_endpoint(&path, Duration::from_millis(200)).await;
        credential_race_hook::clear();
        drop(listener_slot.lock().unwrap().take());

        match outcome {
            Ok(EndpointProbe::NotReady) => {}
            Ok(EndpointProbe::Absent) => {
                panic!("a live listener whose credential was published must not be reported absent")
            }
            Ok(EndpointProbe::Ready(_)) => {
                panic!("an endpoint still starting must not be adopted as ready")
            }
            Ok(EndpointProbe::NoAnswer { .. }) => {
                panic!("an endpoint still starting must not be reported as unanswered")
            }
            Err(TransportError::Unavailable(message)) => panic!(
                "a credential published during the probe was misread as a runtime that \
                 predates the token: {message}"
            ),
            Err(TransportError::PredatesAccessToken(path)) => panic!(
                "a credential published during the probe was misread as a runtime that \
                 predates the token: {}",
                path.display()
            ),
            Err(other) => panic!("unexpected probe failure: {other}"),
        }
    }

    /// A live listener with no credential beside it is a runtime that predates
    /// the access token, not an absent endpoint. Treating it as absent made the
    /// client spawn a competitor, whose bind then failed with "already
    /// listening" — a startup failure that hid the only actionable fact.
    #[tokio::test]
    async fn a_live_listener_without_a_credential_predates_the_token() {
        use tokio::io::AsyncReadExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let serving = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut frame = [0u8; 256];
                let _ = stream.read(&mut frame).await;
            }
        });
        let error = probe_endpoint(&path, Duration::from_millis(500))
            .await
            .err();
        serving.abort();
        assert!(
            matches!(&error, Some(TransportError::PredatesAccessToken(reported)) if reported == &path),
            "a live listener without a credential must be a typed legacy fact, got {error:?}"
        );
    }

    /// A `.token` left behind by a stopped runtime is not proof that the live
    /// listener speaks the current handshake. A pre-token runtime reads the
    /// envelope, does not understand `handshake`, and closes without answering;
    /// that EOF must not be folded into "nobody is listening" as long as the
    /// listener keeps accepting connections.
    #[tokio::test]
    async fn a_peer_that_closes_on_the_handshake_is_not_an_absent_endpoint() {
        use tokio::io::AsyncReadExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::write(path.with_extension("token"), "c".repeat(64)).unwrap();
        let serving = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut frame = [0u8; 256];
                let _ = stream.read(&mut frame).await;
            }
        });
        let error = probe_endpoint(&path, Duration::from_millis(500))
            .await
            .err();
        serving.abort();
        assert!(
            matches!(&error, Some(TransportError::PredatesAccessToken(reported)) if reported == &path),
            "a peer that closes on the handshake must be a typed legacy fact, got {error:?}"
        );
    }

    /// A current runtime killed while a probe was in flight accepts the
    /// connection and then disappears without answering. That EOF is not a
    /// capability statement: the endpoint is absent, and startup must be able
    /// to spawn its replacement instead of refusing with an upgrade message
    /// about a runtime that no longer exists.
    #[tokio::test]
    async fn a_peer_that_died_after_accepting_the_handshake_is_an_absent_endpoint() {
        use tokio::io::AsyncReadExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crashed.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::write(path.with_extension("token"), "d".repeat(64)).unwrap();
        let serving = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut frame = [0u8; 256];
            let _ = stream.read(&mut frame).await;
            // The runtime died with the call in flight: the endpoint stops
            // accepting before the in-flight connection ends without a reply.
            drop(listener);
            drop(stream);
        });
        let outcome = probe_endpoint(&path, Duration::from_millis(500)).await;
        serving.await.unwrap();
        match outcome {
            Ok(EndpointProbe::Absent) => {}
            Ok(EndpointProbe::Ready(_)) => {
                panic!("a runtime that died after accepting must not be adopted")
            }
            Ok(EndpointProbe::NoAnswer { .. }) => {
                panic!("a runtime that died after accepting is absent, not unanswered")
            }
            Ok(EndpointProbe::NotReady) => {
                panic!("a runtime that died after accepting is absent, not starting")
            }
            Err(error) => panic!(
                "a runtime that died after accepting must not be reported as a live \
                 pre-token runtime: {error}"
            ),
        }
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

/// Whether a recorded build conflict must suppress another replacement
/// attempt. Each fact is a separate re-trigger condition: a changed launch
/// artifact may be the build this client expects, an absent runtime can be
/// started again, a runtime that now serves a different build may be the one
/// this client expects, and a changed client build means this is a new
/// generation. None of them is a permanent abandonment: all of them clear the
/// record and let the next attempt observe reality again. An unverifiable
/// runtime and a same-build restart are not changes.
#[cfg(all(test, unix))]
mod build_conflict_tests {
    use super::*;

    fn build(revision: &str, fingerprint: &str) -> leveler_core::BuildIdentity {
        leveler_core::BuildIdentity {
            version: "1.0.23".into(),
            revision: revision.into(),
            dirty: true,
            fingerprint: fingerprint.into(),
        }
    }

    fn conflict(dir: &Path) -> BuildConflict {
        let launch = dir.join("leveler");
        std::fs::write(&launch, b"build-one").unwrap();
        BuildConflict {
            expected: build("samecommit", "samecommit-00000000000000aa"),
            observed: build("samecommit", "samecommit-00000000000000bb"),
            observed_pid: Some(4242),
            launch: LaunchStamp::of(&launch),
            correlation: "corr-1".into(),
            attempts: 1,
            first_seen: std::time::Instant::now(),
        }
    }

    fn serving(pid: u32, build: &leveler_core::BuildIdentity) -> RuntimeObservation {
        RuntimeObservation::Serving {
            pid,
            build: build.clone(),
        }
    }

    #[test]
    fn an_unchanged_conflict_suppresses_the_next_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = conflict(dir.path());
        let launch = LaunchStamp::of(&recorded.launch.path);
        assert!(build_conflict_still_applies(
            &recorded,
            &recorded.expected,
            &launch,
            &serving(4242, &recorded.observed),
        ));
    }

    /// The 2026-10-11 shape: same revision, same short string, different
    /// artifact. A same-pid runtime serving the other fingerprint is a
    /// different conflict and must be re-evaluated, not merged.
    #[test]
    fn a_same_short_different_fingerprint_runtime_clears_the_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = conflict(dir.path());
        let launch = LaunchStamp::of(&recorded.launch.path);
        let other = build("samecommit", "samecommit-00000000000000cc");
        assert!(!build_conflict_still_applies(
            &recorded,
            &recorded.expected,
            &launch,
            &serving(4242, &other),
        ));
    }

    /// A pid change is not progress when the replacement still serves the same
    /// unconvertible build. Observed live: keying on the pid let the reviver
    /// retire and respawn the same wrong artifact every ~10s while the pin
    /// reported a fresh conflict each time.
    #[test]
    fn a_restarted_runtime_serving_the_same_build_keeps_the_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = conflict(dir.path());
        let launch = LaunchStamp::of(&recorded.launch.path);
        assert!(build_conflict_still_applies(
            &recorded,
            &recorded.expected,
            &launch,
            &serving(9001, &recorded.observed),
        ));
    }

    /// A probe that cannot establish what is running must not be read as an
    /// absent runtime: replacing on an unproven observation is unsafe.
    #[test]
    fn an_unverifiable_runtime_keeps_the_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = conflict(dir.path());
        let launch = LaunchStamp::of(&recorded.launch.path);
        assert!(build_conflict_still_applies(
            &recorded,
            &recorded.expected,
            &launch,
            &RuntimeObservation::Unknown,
        ));
    }

    #[test]
    fn an_absent_runtime_clears_the_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = conflict(dir.path());
        let launch = LaunchStamp::of(&recorded.launch.path);
        assert!(!build_conflict_still_applies(
            &recorded,
            &recorded.expected,
            &launch,
            &RuntimeObservation::Absent,
        ));
    }

    #[test]
    fn a_replaced_launch_artifact_clears_the_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = conflict(dir.path());
        std::fs::write(&recorded.launch.path, b"a different build").unwrap();
        let launch = LaunchStamp::of(&recorded.launch.path);
        assert_ne!(launch, recorded.launch);
        assert!(!build_conflict_still_applies(
            &recorded,
            &recorded.expected,
            &launch,
            &serving(4242, &recorded.observed),
        ));
    }

    #[test]
    fn a_changed_client_build_clears_the_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = conflict(dir.path());
        let launch = LaunchStamp::of(&recorded.launch.path);
        let restarted_client = build("samecommit", "samecommit-00000000000000dd");
        assert!(!build_conflict_still_applies(
            &recorded,
            &restarted_client,
            &launch,
            &serving(4242, &recorded.observed),
        ));
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

#[cfg(all(test, unix))]
mod generation_attach_tests {
    use super::*;
    use leveler_client_protocol::{
        ClientError, RetireDecision, RetireRequest, RuntimeEvent, UiSessionSnapshot,
    };
    use leveler_local_transport::{CreateSessionRequest, LocalSocketServer, SessionBootstrap};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::{Notify, broadcast};

    struct Runtime {
        info: RuntimeInfo,
        decision: RetireDecision,
        presence: leveler_local_transport::LocalWaiters,
        replace_after_first_observation: AtomicBool,
        info_reads: AtomicUsize,
        retire_calls: AtomicUsize,
        retire_before_confirmation: AtomicBool,
        commands: AtomicUsize,
        events: broadcast::Sender<RuntimeEvent>,
        retired: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl InteractiveRuntimeClient for Runtime {
        async fn send(&self, _command: ClientCommand) -> Result<(), ClientError> {
            self.commands.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
            self.events.subscribe()
        }
        async fn snapshot(&self, _session: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
            Err(ClientError::Runtime("unused snapshot".into()))
        }
    }

    #[async_trait::async_trait]
    impl LocalRuntimeService for Runtime {
        async fn create_session(
            &self,
            _request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            Err(ClientError::Runtime("unused creation".into()))
        }
        async fn runtime_info(&self) -> Result<RuntimeInfo, ClientError> {
            let mut info = self.info.clone();
            if self.replace_after_first_observation.load(Ordering::SeqCst)
                && self.info_reads.fetch_add(1, Ordering::SeqCst) > 0
            {
                info.build = leveler_core::BuildIdentity::current();
                info.pid += 1;
            }
            if self.retire_before_confirmation.load(Ordering::SeqCst)
                && self.retire_calls.load(Ordering::SeqCst) > 0
            {
                // Retirement closes admission before process shutdown starts.
                info.health.retiring_reason = Some(RestartReason::BuildMismatch);
            }
            Ok(info)
        }
        async fn try_retire_if_idle(
            &self,
            request: RetireRequest,
        ) -> Result<RetireDecision, ClientError> {
            let info = self.runtime_info().await.unwrap();
            assert_eq!(request.expected_build, info.build);
            assert_eq!(request.expected_pid, info.pid);
            self.retire_calls.fetch_add(1, Ordering::SeqCst);
            if self.decision.admits_replacement() {
                self.retired.notify_one();
            }
            Ok(self.decision)
        }
    }

    #[derive(Default)]
    struct Ui(std::sync::Mutex<Vec<HandoffEvent>>);
    impl HandoffUi for Ui {
        fn emit(&self, event: HandoffEvent) {
            self.0.lock().unwrap().push(event);
        }
        fn input(&self) -> Option<UnboundedReceiver<HandoffAction>> {
            None
        }
        fn interactive(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct WaitingUi {
        events: std::sync::Mutex<Vec<HandoffEvent>>,
        reported: Notify,
    }

    impl HandoffUi for WaitingUi {
        fn emit(&self, event: HandoffEvent) {
            self.events.lock().unwrap().push(event);
            self.reported.notify_one();
        }

        fn input(&self) -> Option<UnboundedReceiver<HandoffAction>> {
            panic!("a passive legacy wait must not open handover input")
        }

        fn interactive(&self) -> bool {
            true
        }

        fn waits_for_legacy_exit(&self) -> bool {
            true
        }
    }

    fn layout(dir: &Path) -> Layout {
        let repo = dir.join("repo");
        let config = dir.join("configs");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        Layout::from_parts(repo, config, dir.join("state"))
    }

    async fn runtime(
        layout: &Layout,
        build: leveler_core::BuildIdentity,
        decision: RetireDecision,
    ) -> (Arc<Runtime>, tokio::task::JoinHandle<()>) {
        let runtime = Arc::new(Runtime {
            info: RuntimeInfo {
                runtime_id: leveler_core::RuntimeId::new("generation-test"),
                version: build.version.clone(),
                build,
                config_fingerprint: Some(expected_config_fingerprint(layout).unwrap()),
                pid: std::process::id(),
                idle_eviction: None,
                health: RuntimeHealth {
                    background_tasks_unknown: false,
                    accepting_work: true,
                    active_background_tasks: if matches!(
                        decision,
                        RetireDecision::Busy { .. } | RetireDecision::Unsupported
                    ) {
                        1
                    } else {
                        0
                    },
                    ..Default::default()
                },
            },
            decision,
            presence: leveler_local_transport::LocalWaiters::new(),
            replace_after_first_observation: AtomicBool::new(false),
            info_reads: AtomicUsize::new(0),
            retire_calls: AtomicUsize::new(0),
            retire_before_confirmation: AtomicBool::new(false),
            commands: AtomicUsize::new(0),
            events: broadcast::channel(8).0,
            retired: Arc::new(Notify::new()),
        });
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let server = LocalSocketServer::bind_with_waiters(
            &socket,
            runtime.clone(),
            runtime.presence.clone(),
        )
        .await
        .unwrap();
        let retired = runtime.retired.clone();
        let serving = tokio::spawn(async move {
            tokio::select! {
                _ = server.serve(Default::default()) => {},
                _ = retired.notified() => {},
            }
        });
        (runtime, serving)
    }

    fn previous_build() -> leveler_core::BuildIdentity {
        leveler_core::BuildIdentity {
            version: "previous".into(),
            revision: "previous".into(),
            fingerprint: "previous".into(),
            dirty: false,
        }
    }

    #[tokio::test]
    async fn a_second_unattended_client_does_not_reuse_a_busy_previous_build() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let (runtime, serving) = runtime(
            &layout,
            previous_build(),
            RetireDecision::Busy {
                active_turns: 0,
                active_background_tasks: 1,
            },
        )
        .await;
        let first = LocalSocketRuntimeClient::connect(&layout.socket_path())
            .await
            .unwrap();
        let ui = NonInteractiveHandoffUi::new();
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            reconcile_runtime_generation(&layout.socket_path(), &layout, ui.as_ref()),
        )
        .await
        .expect("a busy previous build must answer promptly, not hold the client")
        .err()
        .expect("a different build is never attached while it is busy");
        assert!(
            matches!(
                error.downcast_ref::<EnsureError>(),
                Some(EnsureError::RetireBlocked { .. })
            ),
            "{error}"
        );
        assert_eq!(runtime.retire_calls.load(Ordering::SeqCst), 1);
        assert!(runtime.info.health.accepting_work);
        // The serving generation keeps working for the client that owns it.
        first.send(ClientCommand::RequestSessionList).await.unwrap();
        assert_eq!(runtime.commands.load(Ordering::SeqCst), 1);
        serving.abort();
    }

    #[tokio::test]
    async fn a_current_busy_generation_is_reused_without_requesting_retirement() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let (runtime, serving) = runtime(
            &layout,
            leveler_core::BuildIdentity::current(),
            RetireDecision::Busy {
                active_turns: 0,
                active_background_tasks: 1,
            },
        )
        .await;
        let client = reconcile_runtime_generation(&layout.socket_path(), &layout, &Ui::default())
            .await
            .unwrap();
        assert!(client.is_some());
        assert_eq!(runtime.retire_calls.load(Ordering::SeqCst), 0);
        serving.abort();
    }

    #[tokio::test]
    async fn an_idle_previous_generation_still_retires_before_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let (runtime, serving) = runtime(&layout, previous_build(), RetireDecision::Accepted).await;
        let client = tokio::time::timeout(
            Duration::from_secs(8),
            reconcile_runtime_generation(&layout.socket_path(), &layout, &Ui::default()),
        )
        .await
        .expect("accepted retirement releases its endpoint")
        .unwrap();
        assert!(
            client.is_none(),
            "an accepted retirement must not be attached to"
        );
        assert_eq!(runtime.retire_calls.load(Ordering::SeqCst), 1);
        serving.await.unwrap();
    }

    #[tokio::test]
    async fn a_busy_previous_build_with_different_configuration_is_not_adopted() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let (runtime, serving) = runtime(
            &layout,
            previous_build(),
            RetireDecision::Busy {
                active_turns: 0,
                active_background_tasks: 1,
            },
        )
        .await;
        std::fs::create_dir_all(layout.providers_dir()).unwrap();
        std::fs::write(layout.providers_dir().join("changed.yaml"), "id: changed\n").unwrap();
        let ui = NonInteractiveHandoffUi::new();
        let error = reconcile_runtime_generation(&layout.socket_path(), &layout, ui.as_ref())
            .await
            .err()
            .expect("configuration mismatch must not be hidden by reuse");
        assert!(
            matches!(
                error.downcast_ref::<EnsureError>(),
                Some(EnsureError::RetireBlocked { .. })
            ),
            "{error}"
        );
        assert_eq!(runtime.retire_calls.load(Ordering::SeqCst), 1);
        serving.abort();
    }

    #[tokio::test]
    async fn a_same_build_election_winner_is_not_retired_after_observing_the_old_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let (runtime, serving) = runtime(&layout, previous_build(), RetireDecision::Accepted).await;
        runtime
            .replace_after_first_observation
            .store(true, Ordering::SeqCst);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            reconcile_runtime_generation(&layout.socket_path(), &layout, &Ui::default()),
        )
        .await
        .expect("the already-ready replacement should be adopted promptly")
        .unwrap();
        serving.abort();

        assert_eq!(runtime.retire_calls.load(Ordering::SeqCst), 0);
        assert!(result.is_some(), "adopt the winner of the other launch");
    }

    #[tokio::test]
    async fn a_runtime_without_known_identity_is_not_adopted() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let (runtime, serving) = runtime(
            &layout,
            leveler_core::BuildIdentity::default(),
            RetireDecision::Busy {
                active_turns: 0,
                active_background_tasks: 1,
            },
        )
        .await;
        let error = reconcile_runtime_generation(&layout.socket_path(), &layout, &Ui::default())
            .await
            .err()
            .expect("unknown generation cannot be reused");
        assert!(
            matches!(
                error.downcast_ref::<EnsureError>(),
                Some(EnsureError::UnknownGeneration)
            ),
            "{error}"
        );
        assert_eq!(runtime.retire_calls.load(Ordering::SeqCst), 0);
        serving.abort();
    }

    #[tokio::test]
    async fn missing_atomic_retirement_is_not_a_busy_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let (runtime, serving) =
            runtime(&layout, previous_build(), RetireDecision::Unsupported).await;
        let error = reconcile_runtime_generation(&layout.socket_path(), &layout, &Ui::default())
            .await
            .err()
            .expect("unsupported must not authorize reuse");
        assert!(
            matches!(
                error.downcast_ref::<EnsureError>(),
                Some(EnsureError::RetireBlocked { .. })
            ),
            "{error}"
        );
        assert_eq!(runtime.retire_calls.load(Ordering::SeqCst), 1);
        serving.abort();
    }

    #[tokio::test]
    async fn unsupported_authenticated_runtime_is_observed_until_another_owner_takes_over() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let (old_runtime, old_serving) =
            runtime(&layout, previous_build(), RetireDecision::Unsupported).await;
        let ui = Arc::new(WaitingUi::default());
        let reconciling = tokio::spawn({
            let ui = ui.clone();
            let layout = layout.clone();
            async move {
                reconcile_runtime_generation(&layout.socket_path(), &layout, ui.as_ref()).await
            }
        });

        tokio::time::timeout(Duration::from_secs(3), ui.reported.notified())
            .await
            .expect("the shell reports why it needs the previous runtime to exit");
        assert!(matches!(
            ui.events.lock().unwrap().as_slice(),
            [HandoffEvent::LegacyExitRequired {
                block: LegacyHandoffBlock::AtomicRetirementUnavailable,
                version: Some(version),
                pid: Some(pid),
                ..
            }] if version == "previous" && *pid == std::process::id()
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while old_runtime.presence.count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a passive waiter must not prevent the previous runtime from self-evicting");
        old_serving.abort();
        assert!(old_serving.await.unwrap_err().is_cancelled());
        let (new_runtime, new_serving) = runtime(
            &layout,
            leveler_core::BuildIdentity::current(),
            RetireDecision::Accepted,
        )
        .await;
        let result = tokio::time::timeout(Duration::from_secs(10), reconciling)
            .await
            .expect("the same launch recognizes the replacement")
            .unwrap();
        assert!(matches!(result, Ok(Some(_))), "{:?}", result.err());
        assert_eq!(old_runtime.retire_calls.load(Ordering::SeqCst), 1);
        assert_eq!(old_runtime.commands.load(Ordering::SeqCst), 0);
        assert_eq!(new_runtime.retire_calls.load(Ordering::SeqCst), 0);
        new_serving.abort();
    }

    #[tokio::test]
    async fn a_live_listener_that_does_not_answer_is_not_available_for_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::write(socket.with_extension("token"), "a".repeat(64)).unwrap();

        let result = reconcile_runtime_generation(&socket, &layout, &Ui::default()).await;
        assert!(matches!(
            result
                .err()
                .and_then(|error| error.downcast::<TransportError>().ok()),
            Some(TransportError::NoAnswer {
                operation: "the startup handshake",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn an_incompatible_wire_major_is_rejected_before_generation_reuse() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::write(socket.with_extension("token"), "a".repeat(64)).unwrap();
        let serving = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let len = stream.read_u32().await.unwrap();
            let mut request = vec![0; len as usize];
            stream.read_exact(&mut request).await.unwrap();
            let mut response =
                leveler_client_protocol::ProtocolEnvelope::wrap(serde_json::json!({"type": "ack"}));
            response.protocol.major += 1;
            let bytes = serde_json::to_vec(&response).unwrap();
            stream.write_u32(bytes.len() as u32).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
        });
        let error = reconcile_runtime_generation(&socket, &layout, &Ui::default())
            .await
            .err()
            .expect("major mismatch is never a reusable peer");
        assert!(
            matches!(
                error.downcast_ref::<TransportError>(),
                Some(TransportError::Protocol(_))
            ),
            "{error}"
        );
        serving.await.unwrap();
    }

    #[tokio::test]
    async fn a_runtime_that_predates_the_token_is_a_typed_migration_refusal() {
        use tokio::io::AsyncReadExt;
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        // A `.token` left behind by a stopped candidate is not proof that the
        // listener speaks the current handshake.
        std::fs::write(socket.with_extension("token"), "d".repeat(64)).unwrap();
        let serving = tokio::spawn(async move {
            // A live pre-token runtime keeps listening after it closes the
            // handshake it cannot decode. A one-shot listener would be a
            // runtime that died, which is a different fact.
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut frame = [0u8; 256];
                let _ = stream.read(&mut frame).await;
            }
        });
        let ui = Ui::default();
        let error = reconcile_runtime_generation(&socket, &layout, &ui)
            .await
            .err()
            .expect("a runtime from before the access token cannot be attached to");
        serving.abort();
        match error.downcast_ref::<EnsureError>() {
            Some(EnsureError::LegacyMigrationRefused {
                socket: reported,
                reason,
            }) => {
                assert_eq!(reported, &socket);
                assert!(
                    reason.contains("does not support atomic retirement"),
                    "{reason}"
                );
                assert!(reason.contains("predates the access token"), "{reason}");
            }
            other => panic!("expected a typed migration refusal, got {other:?} ({error})"),
        }
        let events = ui.0.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, HandoffEvent::MigrationFailed { .. })),
            "the shell must be told the migration failed: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_waiting_shell_rechecks_after_the_tokenless_owner_releases_its_socket() {
        use tokio::io::AsyncReadExt;

        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let serving = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                if let Ok(length) = stream.read_u32().await {
                    let mut frame = vec![0; length as usize];
                    stream.read_exact(&mut frame).await.unwrap();
                    let request: serde_json::Value = serde_json::from_slice(&frame).unwrap();
                    assert!(
                        matches!(
                            request["body"]["type"].as_str(),
                            Some("runtime_info" | "local_waiters")
                        ),
                        "unexpected observation: {request}"
                    );
                }
            }
        });
        let ui = Arc::new(WaitingUi::default());
        let reconciling = tokio::spawn({
            let ui = ui.clone();
            let layout = layout.clone();
            async move {
                reconcile_runtime_generation(&layout.socket_path(), &layout, ui.as_ref()).await
            }
        });

        tokio::time::timeout(Duration::from_secs(3), ui.reported.notified())
            .await
            .expect("the shell is told why it must wait");
        serving.abort();
        assert!(serving.await.unwrap_err().is_cancelled());
        assert!(socket.exists(), "the stale socket file is still in place");
        let result = tokio::time::timeout(Duration::from_secs(10), reconciling)
            .await
            .expect("the same launch rechecks the endpoint after its owner exits")
            .unwrap();
        assert!(matches!(result, Ok(None)), "{:?}", result.err());
        assert!(matches!(
            ui.events.lock().unwrap().as_slice(),
            [HandoffEvent::LegacyExitRequired {
                block: LegacyHandoffBlock::AccessTokenUnavailable,
                version: None,
                pid: None,
                active_turns: None,
                active_background_tasks: None,
                local_waiters: None,
            }]
        ));
    }

    /// A busy runtime from before the access token is still BUSY. Telling its
    /// owner to exit would interrupt real work, so the client reads the one
    /// request every generation answered (`runtime_info`, unauthenticated,
    /// read-only) and reports the same deferral a modern busy runtime gets.
    #[tokio::test]
    async fn a_busy_runtime_that_predates_the_token_is_deferred_not_told_to_exit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let serving = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                // The probe's liveness connect writes nothing and is dropped.
                let Ok(length) = stream.read_u32().await else {
                    continue;
                };
                let mut request = vec![0; length as usize];
                stream.read_exact(&mut request).await.unwrap();
                let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
                let body = if request["body"]["type"] == "local_waiters" {
                    serde_json::json!({"type": "local_waiters", "body": 0})
                } else {
                    serde_json::json!({
                        "type": "runtime_info",
                        "body": {
                            "runtime_id": "legacy-busy",
                            "version": "1.0.11",
                            "build": {
                                "version": "1.0.11",
                                "revision": "0000000000000000000000000000000000000000",
                                "fingerprint": "legacy",
                                "dirty": false
                            },
                            "config_fingerprint": null,
                            "pid": std::process::id(),
                            "health": {
                                "accepting_work": true,
                                "active_turns": 1,
                                "active_background_tasks": 0,
                                "quiescent": false,
                                "shutting_down": false
                            }
                        }
                    })
                };
                let bytes = serde_json::to_vec(&serde_json::json!({
                    "protocol": {"major": 1, "minor": 0},
                    "body": body
                }))
                .unwrap();
                stream.write_u32(bytes.len() as u32).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
            }
        });
        let ui = Ui::default();
        let error = reconcile_runtime_generation(&socket, &layout, &ui)
            .await
            .err()
            .expect("a busy legacy runtime is still not adopted");
        let _ = tokio::time::timeout(Duration::from_secs(5), serving).await;
        match error.downcast_ref::<EnsureError>() {
            Some(EnsureError::RetireBlocked {
                waited_secs: 0,
                active_turns: 1,
                ..
            }) => {}
            other => panic!("expected a prompt deferral, got {other:?} ({error})"),
        }
        let events = ui.0.lock().unwrap();
        assert!(
            events.iter().any(|event| matches!(
                event,
                HandoffEvent::UpgradeDeferred {
                    active_turns: 1,
                    ..
                }
            )),
            "the shell must see the deferral, not a migration failure: {events:?}"
        );
    }

    /// A separate process that speaks the pre-token protocol on a Unix socket.
    ///
    /// The cooperative request is bound to the kernel's peer credential, so a
    /// listener in this test process would not exercise the production path:
    /// the verified pid would name one process while the connection belonged
    /// to another. This child both listens and reports its own pid, exactly as
    /// a real runtime does, and its command line names a `serve` runtime for
    /// the workspace (the trailing positional arguments).
    #[cfg(unix)]
    struct FakeLegacyProcess {
        child: std::process::Child,
        command_log: std::path::PathBuf,
        #[allow(dead_code)]
        ready: std::path::PathBuf,
    }

    #[cfg(unix)]
    const FAKE_LEGACY_RUNTIME: &str = r#"
import json, os, socket, struct, sys

sock_path, mode, command_log, flag_path, ready_path = sys.argv[1:6]

server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    os.unlink(sock_path)
except OSError:
    pass
server.bind(sock_path)
server.listen(64)
open(ready_path, "w").write("ready")
sent = False

def waiters():
    if mode == "attached_until_flag":
        return 1 if os.path.exists(flag_path) else 0
    return 0

def shutting_down():
    return mode == "retiring" or (mode == "acklost" and sent)

def info():
    sd = shutting_down()
    return {
        "runtime_id": "legacy-fake",
        "version": "1.0.27",
        "build": {
            "version": "1.0.27",
            "revision": "0000000000000000000000000000000000000000",
            "fingerprint": "legacy",
            "dirty": False,
        },
        "config_fingerprint": None,
        "pid": os.getpid(),
        "health": {
            "accepting_work": not sd,
            "active_turns": 0,
            "active_background_tasks": 0,
            "quiescent": not sd,
            "turn_capacity": 4,
            "shutting_down": sd,
        },
    }

def read_exact(conn, n):
    data = b""
    while len(data) < n:
        chunk = conn.recv(n - len(data))
        if not chunk:
            return None
        data += chunk
    return data

def send(conn, body):
    payload = json.dumps({"protocol": {"major": 1, "minor": 0}, "body": body}).encode()
    conn.sendall(struct.pack(">I", len(payload)) + payload)

while True:
    conn, _ = server.accept()
    header = read_exact(conn, 4)
    if header is None:
        conn.close()
        continue
    frame = read_exact(conn, struct.unpack(">I", header)[0])
    if frame is None:
        conn.close()
        continue
    try:
        body = json.loads(frame).get("body", {})
    except ValueError:
        conn.close()
        continue
    kind = body.get("type")
    if kind == "local_waiters":
        send(conn, {"type": "local_waiters", "body": waiters()})
        conn.close()
    elif kind == "send":
        with open(command_log, "a") as log:
            # Record the whole request body, exactly as the in-process fake did,
            # so a test asserts the wire shape the runtime receives.
            log.write(json.dumps(body) + "\n")
        sent = True
        if mode == "acklost":
            # The command was processed, its reply never reached the sender.
            conn.close()
        else:
            send(conn, {"type": "ack"})
            conn.close()
            if mode in ("idle", "attached_until_flag"):
                try:
                    os.unlink(sock_path)
                except OSError:
                    pass
                break
    else:
        send(conn, {"type": "runtime_info", "body": info()})
        conn.close()
"#;

    #[cfg(unix)]
    impl FakeLegacyProcess {
        // The returned value owns the child and its Drop kills and reaps it on
        // every path, including panics; clippy cannot see through the guard.
        #[allow(clippy::zombie_processes)]
        fn start(root: &Path, socket: &Path, mode: &str, repo: &Path) -> Self {
            let script = root.join(format!("fake-legacy-{mode}.py"));
            std::fs::write(&script, FAKE_LEGACY_RUNTIME).unwrap();
            let command_log = root.join(format!("legacy-commands-{mode}.jsonl"));
            let ready = root.join(format!("legacy-ready-{mode}"));
            let flag = root.join("legacy-attached");
            let _ = std::fs::remove_file(&command_log);
            let _ = std::fs::remove_file(&ready);
            let _ = std::fs::remove_file(&flag);
            if mode == "attached_until_flag" {
                std::fs::write(&flag, b"1").unwrap();
            }
            let mut child = std::process::Command::new("python3")
                .arg(&script)
                .arg(socket)
                .arg(mode)
                .arg(&command_log)
                .arg(&flag)
                .arg(&ready)
                // Positional tokens that make the process command line name a
                // runtime serving this workspace, as a real runtime's does.
                .arg(repo)
                .arg("serve")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("the legacy test harness needs python3");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if ready.exists() && socket.exists() {
                    return Self {
                        child,
                        command_log,
                        ready,
                    };
                }
                if let Some(status) = child.try_wait().unwrap() {
                    panic!("the fake legacy runtime exited before readiness: {status}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = child.kill();
            panic!("the fake legacy runtime did not bind within 5s");
        }

        fn commands(&self) -> Vec<serde_json::Value> {
            std::fs::read_to_string(&self.command_log)
                .map(|text| {
                    text.lines()
                        .filter_map(|line| serde_json::from_str(line).ok())
                        .collect()
                })
                .unwrap_or_default()
        }

        fn wait_for_commands(&self, count: usize) -> Vec<serde_json::Value> {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                let commands = self.commands();
                if commands.len() >= count || std::time::Instant::now() >= deadline {
                    return commands;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn release_attachment(&self, root: &Path) {
            std::fs::remove_file(root.join("legacy-attached")).unwrap();
        }
    }

    /// The settle must reject the same pid number when the process behind it
    /// changed, and the same path when the socket object changed. Those are
    /// the two facts a pid and a runtime id cannot prove on their own.
    #[cfg(unix)]
    #[test]
    fn the_settle_rejects_a_reused_pid_or_a_rebound_socket() {
        use crate::legacy::{ProcessWitness, SocketObject};

        fn info(runtime_id: &str) -> RuntimeInfo {
            RuntimeInfo {
                runtime_id: leveler_core::RuntimeId::new(runtime_id),
                version: "1.0.27".into(),
                build: leveler_core::BuildIdentity {
                    version: "1.0.27".into(),
                    revision: "0".repeat(40),
                    dirty: false,
                    fingerprint: "legacy".into(),
                },
                config_fingerprint: None,
                pid: 42,
                idle_eviction: None,
                health: leveler_client_protocol::RuntimeHealth {
                    background_tasks_unknown: false,
                    accepting_work: true,
                    active_turns: 0,
                    active_background_tasks: 0,
                    quiescent: true,
                    turn_capacity: Some(4),
                    shutting_down: false,
                    ..Default::default()
                },
            }
        }

        let first = info("legacy-idle");
        let identity = LegacyIdentity {
            witness: ProcessWitness {
                pid: 42,
                started: "100".into(),
            },
            socket: SocketObject {
                device: 1,
                inode: 2,
            },
        };
        let second = LegacyObservation {
            info: info("legacy-idle"),
            local_waiters: Some(0),
        };
        assert!(legacy_settle_agrees(
            &first,
            &identity,
            &second,
            Some(&identity.witness),
            Some(&identity.socket),
        ));

        let reused_pid = ProcessWitness {
            pid: 42,
            started: "101".into(),
        };
        assert!(
            !legacy_settle_agrees(
                &first,
                &identity,
                &second,
                Some(&reused_pid),
                Some(&identity.socket),
            ),
            "a different start marker is a different process"
        );
        let rebound = SocketObject {
            device: 1,
            inode: 3,
        };
        assert!(
            !legacy_settle_agrees(
                &first,
                &identity,
                &second,
                Some(&identity.witness),
                Some(&rebound),
            ),
            "a different socket object is a different endpoint"
        );
        let attached = LegacyObservation {
            info: info("legacy-idle"),
            local_waiters: Some(1),
        };
        assert!(!legacy_settle_agrees(
            &first,
            &identity,
            &attached,
            Some(&identity.witness),
            Some(&identity.socket),
        ));
        let replaced = LegacyObservation {
            info: info("different-generation"),
            local_waiters: Some(0),
        };
        assert!(!legacy_settle_agrees(
            &first,
            &identity,
            &replaced,
            Some(&identity.witness),
            Some(&identity.socket),
        ));
    }

    #[cfg(unix)]
    impl Drop for FakeLegacyProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// A process whose command line identifies it as a runtime for `repo`,
    /// without starting a real runtime. Used only to give the identity check a
    /// pid that the kernel agrees belongs to this workspace's `serve` process.
    #[cfg(unix)]
    fn spawn_serving_process(repo: &Path) -> std::process::Child {
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            // A compound command keeps the shell as the process: a bare
            // `sleep` is exec'd in place and would replace this argv.
            .arg("sleep 30; :")
            .arg(repo)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn().unwrap()
    }

    /// A pre-token endpoint: `runtime_info` and `local_waiters` are answered
    /// without a credential, and a `send` is recorded. When `release_on_send`
    /// is set, a `shutdown_when_idle` command is acknowledged and the endpoint
    /// is released — the cooperative behaviour a real old runtime has.
    #[cfg(unix)]
    fn fake_legacy_endpoint(
        socket: &Path,
        pid: u32,
        waiters: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        quiescent: bool,
        shutting_down: bool,
        release_on_send: bool,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    ) {
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        let socket = socket.to_path_buf();
        let (commands, received) = tokio::sync::mpsc::unbounded_channel();
        let serving = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                // A liveness probe connects and writes nothing; it is not a
                // request and must not be answered.
                let Ok(length) = stream.read_u32().await else {
                    continue;
                };
                let mut frame = vec![0; length as usize];
                if stream.read_exact(&mut frame).await.is_err() {
                    continue;
                }
                let request: serde_json::Value = serde_json::from_slice(&frame).unwrap();
                let reply = match request["body"]["type"].as_str() {
                    Some("local_waiters") => serde_json::json!({
                        "type": "local_waiters",
                        "body": waiters.load(Ordering::SeqCst)
                    }),
                    Some("send") => {
                        commands.send(request["body"].clone()).ok();
                        serde_json::json!({"type": "ack"})
                    }
                    _ => serde_json::json!({
                        "type": "runtime_info",
                        "body": {
                            "runtime_id": "legacy-idle",
                            "version": "1.0.27",
                            "build": {
                                "version": "1.0.27",
                                "revision": "0000000000000000000000000000000000000000",
                                "fingerprint": "legacy",
                                "dirty": false
                            },
                            "config_fingerprint": null,
                            "pid": pid,
                            "health": {
                                "accepting_work": true,
                                "active_turns": 0,
                                "active_background_tasks": 0,
                                "quiescent": quiescent,
                                "turn_capacity": 4,
                                "shutting_down": shutting_down
                            }
                        }
                    }),
                };
                let bytes = serde_json::to_vec(&serde_json::json!({
                    "protocol": {"major": 1, "minor": 0},
                    "body": reply
                }))
                .unwrap();
                let _ = stream.write_u32(bytes.len() as u32).await;
                let _ = stream.write_all(&bytes).await;
                let _ = stream.flush().await;
                if release_on_send && request["body"]["type"] == "send" {
                    // A retired runtime releases its endpoint.
                    let _ = std::fs::remove_file(&socket);
                    break;
                }
            }
        });
        (serving, received)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_idle_unattended_legacy_runtime_is_asked_to_retire_cooperatively() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let server = FakeLegacyProcess::start(
            tmp.path(),
            &socket,
            "idle",
            layout.primary_workspace().unwrap(),
        );
        let pid = server.child.id();
        let ui = Arc::new(WaitingUi::default());
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            reconcile_runtime_generation(&socket, &layout, ui.as_ref()),
        )
        .await
        .expect("a cooperative retirement releases its endpoint")
        .unwrap();
        assert!(result.is_none(), "a retiring runtime is never attached to");
        let commands = server.wait_for_commands(1);
        assert_eq!(commands.len(), 1, "one request, not a repeated broadcast");
        let command = &commands[0];
        assert_eq!(command["type"], "send");
        assert_eq!(command["body"]["type"], "shutdown_when_idle");
        assert_eq!(command["body"]["reason"], "build_mismatch");
        let events = ui.events.lock().unwrap();
        assert!(
            events.iter().any(|event| matches!(
                event,
                HandoffEvent::LegacyRetirementRequested {
                    version: Some(version),
                    pid: Some(reported),
                } if version == "1.0.27" && *reported == pid
            )),
            "the shell must be told what was actually requested: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, HandoffEvent::LegacyExitRequired { .. })),
            "a requested retirement is not a passive wait: {events:?}"
        );
    }

    /// An accepted retirement that is still draining is NOT a refusal. A
    /// programmatic shell must retry, not report that the old build refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_accepted_legacy_retirement_still_draining_is_reported_as_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let server = FakeLegacyProcess::start(
            tmp.path(),
            &socket,
            "drain",
            layout.primary_workspace().unwrap(),
        );
        let ui = Ui::default();
        let error = tokio::time::timeout(
            Duration::from_secs(15),
            reconcile_runtime_generation(&socket, &layout, &ui),
        )
        .await
        .expect("the bounded wait ends")
        .err()
        .expect("a draining runtime is not attached to");
        assert!(
            matches!(
                error.downcast_ref::<EnsureError>(),
                Some(EnsureError::LegacyRetirementPending { .. })
            ),
            "{error}"
        );
        let commands = server.wait_for_commands(1);
        assert_eq!(commands.len(), 1, "the request was accepted");
        assert_eq!(commands[0]["body"]["type"], "shutdown_when_idle");
    }

    /// The reply to an accepted request can be lost. The runtime's own state
    /// is the only honest evidence then: reporting that the old build refused
    /// would send the operator to stop a process that is already retiring.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_ack_lost_legacy_retirement_is_pending_not_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let server = FakeLegacyProcess::start(
            tmp.path(),
            &socket,
            "acklost",
            layout.primary_workspace().unwrap(),
        );
        let ui = Ui::default();
        let error = tokio::time::timeout(
            Duration::from_secs(15),
            reconcile_runtime_generation(&socket, &layout, &ui),
        )
        .await
        .expect("the bounded wait ends")
        .err()
        .expect("a draining runtime is not attached to");
        assert!(
            matches!(
                error.downcast_ref::<EnsureError>(),
                Some(EnsureError::LegacyRetirementPending { .. })
            ),
            "an accepted retirement with a lost reply must not be reported as a refusal: {error}"
        );
        let commands = server.wait_for_commands(1);
        assert_eq!(commands.len(), 1, "the command was delivered");
        assert_eq!(commands[0]["body"]["type"], "shutdown_when_idle");
    }

    /// Another shell already asked this runtime to retire. That is an accepted
    /// retirement in progress, not a version that refused: a programmatic
    /// caller must retry, never send the operator to stop it by hand.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_legacy_runtime_already_retiring_is_reported_as_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let mut child = spawn_serving_process(layout.primary_workspace().unwrap());
        let pid = child.id();
        let (serving, mut commands) = fake_legacy_endpoint(
            &socket,
            pid,
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            true,
            true,
            false,
        );
        let ui = Ui::default();
        let error = tokio::time::timeout(
            Duration::from_secs(15),
            reconcile_runtime_generation(&socket, &layout, &ui),
        )
        .await
        .expect("the bounded wait ends")
        .err()
        .expect("a retiring runtime is not attached to");
        assert!(
            matches!(
                error.downcast_ref::<EnsureError>(),
                Some(EnsureError::LegacyRetirementPending { .. })
            ),
            "{error}"
        );
        assert!(
            commands.try_recv().is_err(),
            "a runtime that is already retiring is not asked a second time"
        );
        let _ = child.kill();
        let _ = child.wait();
        serving.abort();
    }

    /// An idle runtime with a local UI attached is NOT retired: the command
    /// closes admission immediately, which would strand the person using it.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_attached_legacy_runtime_is_never_told_to_retire() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let mut child = spawn_serving_process(layout.primary_workspace().unwrap());
        let pid = child.id();
        let (serving, mut commands) = fake_legacy_endpoint(
            &socket,
            pid,
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1)),
            true,
            false,
            false,
        );
        let ui = Arc::new(WaitingUi::default());
        let reconciling = tokio::spawn({
            let ui = ui.clone();
            let layout = layout.clone();
            async move {
                reconcile_runtime_generation(&layout.socket_path(), &layout, ui.as_ref()).await
            }
        });
        tokio::time::timeout(Duration::from_secs(3), ui.reported.notified())
            .await
            .expect("the shell is told why it must wait");
        assert!(
            commands.try_recv().is_err(),
            "an attached runtime must not be asked to stop serving"
        );
        let events = ui.events.lock().unwrap();
        assert!(
            events.iter().any(|event| matches!(
                event,
                HandoffEvent::LegacyExitRequired {
                    local_waiters: Some(1),
                    ..
                }
            )),
            "the wait must carry the attached-UI fact: {events:?}"
        );
        reconciling.abort();
        let _ = child.kill();
        let _ = child.wait();
        serving.abort();
    }

    /// The wait must not outlive the blocker: when the old UI that was holding
    /// the runtime closes, the same launch asks the now-idle runtime to retire
    /// instead of waiting out its 30-minute idle timer.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_legacy_runtime_that_becomes_unattached_is_retired_without_waiting_out_its_timer() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let server = FakeLegacyProcess::start(
            tmp.path(),
            &socket,
            "attached_until_flag",
            layout.primary_workspace().unwrap(),
        );
        let ui = Arc::new(WaitingUi::default());
        let reconciling = tokio::spawn({
            let ui = ui.clone();
            let layout = layout.clone();
            async move {
                reconcile_runtime_generation(&layout.socket_path(), &layout, ui.as_ref()).await
            }
        });
        tokio::time::timeout(Duration::from_secs(3), ui.reported.notified())
            .await
            .expect("the attached state is reported first");
        assert!(server.commands().is_empty());
        // The old UI closes. The next refresh must observe the new state.
        server.release_attachment(tmp.path());
        let result = tokio::time::timeout(Duration::from_secs(20), reconciling)
            .await
            .expect("a now-idle runtime is retired without the old idle timer")
            .unwrap()
            .unwrap();
        assert!(result.is_none(), "a retiring runtime is never attached to");
        let commands = server.wait_for_commands(1);
        assert_eq!(commands.len(), 1, "the now-idle runtime is asked to retire");
        assert_eq!(commands[0]["body"]["type"], "shutdown_when_idle");
        let events = ui.events.lock().unwrap();
        let positions: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                matches!(
                    event,
                    HandoffEvent::LegacyExitRequired { .. }
                        | HandoffEvent::LegacyRetirementRequested { .. }
                )
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(positions.len(), 2, "one wait, then one request: {events:?}");
        assert!(matches!(
            events[positions[0]],
            HandoffEvent::LegacyExitRequired {
                local_waiters: Some(1),
                ..
            }
        ));
        assert!(matches!(
            events[positions[1]],
            HandoffEvent::LegacyRetirementRequested { .. }
        ));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unverifiable_legacy_pid_is_never_asked_to_retire() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let socket = layout.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let (serving, mut commands) = fake_legacy_endpoint(
            &socket,
            std::process::id(),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            true,
            false,
            false,
        );
        let ui = Arc::new(WaitingUi::default());
        let reconciling = tokio::spawn({
            let ui = ui.clone();
            let layout = layout.clone();
            async move {
                reconcile_runtime_generation(&layout.socket_path(), &layout, ui.as_ref()).await
            }
        });
        tokio::time::timeout(Duration::from_secs(3), ui.reported.notified())
            .await
            .expect("the shell is told why it must wait");
        assert!(
            commands.try_recv().is_err(),
            "an unverified pid must never receive a retirement request"
        );
        let events = ui.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, HandoffEvent::LegacyExitRequired { .. })),
            "{events:?}"
        );
        reconciling.abort();
        serving.abort();
    }

    #[tokio::test]
    async fn a_busy_generation_that_started_retiring_before_confirmation_is_not_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout(tmp.path());
        let (runtime, serving) = runtime(
            &layout,
            previous_build(),
            RetireDecision::Busy {
                active_turns: 0,
                active_background_tasks: 1,
            },
        )
        .await;
        runtime
            .retire_before_confirmation
            .store(true, Ordering::SeqCst);
        // An unattended client must be told the upgrade is blocked, not be
        // left waiting on a generation that has already started retiring.
        let ui = NonInteractiveHandoffUi::new();
        let result =
            reconcile_runtime_generation(&layout.socket_path(), &layout, ui.as_ref()).await;
        serving.abort();
        let error = result
            .err()
            .expect("a runtime that closed admission after Busy cannot be reused");
        assert!(
            matches!(
                error.downcast_ref::<EnsureError>(),
                Some(EnsureError::RetireBlocked { .. })
            ),
            "{error}"
        );
        assert_eq!(runtime.retire_calls.load(Ordering::SeqCst), 1);
        let info = runtime.runtime_info().await.unwrap();
        assert!(!info.health.shutting_down);
        assert_eq!(
            info.health.retiring_reason,
            Some(RestartReason::BuildMismatch)
        );
    }
}

/// Facts about an endpoint that are the same on every supported host. Kept
/// outside the Unix-only socket tests so Windows named-pipe EOF is covered too.
#[cfg(all(test, any(unix, windows)))]
mod endpoint_classification_tests {
    use super::*;

    /// A stale endpoint can fail the subscribe handshake with ECONNRESET or
    /// EPIPE: Linux reports the former when the listener behind the socket file
    /// is gone, macOS reports the latter when the closed listener leaves no
    /// peer to receive the handshake, and Windows reports EOF when the pipe
    /// server accepted and closed without a reply. All are the same fact as
    /// ECONNREFUSED — no runtime owns the endpoint — and must not turn startup
    /// into a transport error.
    #[test]
    fn an_unusable_endpoint_is_no_runtime() {
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            assert!(
                endpoint_unavailable(&std::io::Error::from(kind)),
                "{kind:?} means no runtime is listening"
            );
        }
        assert!(!endpoint_unavailable(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }
}
