//! Background process tasks (TL-4/5): spawn, get, wait, kill with log caps.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::{Mutex, Notify, broadcast};
use tokio_util::sync::CancellationToken;

use crate::command::{CommandRunner, ManagedProcess, ProcessIdentity, ProcessRequest};
use crate::snapshot::SnapshotId;

/// Pre-spawn workspace snapshot and write authority, used to settle a
/// background task when its process exits.
///
/// The allowlist travels with the baseline rather than being read at
/// settlement time because the authority a task runs under is the one it was
/// spawned with. A background process outlives the round that started it, so
/// there is no later scope to consult.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MutationBaseline {
    pub snapshot: SnapshotId,
    pub workspace_root: PathBuf,
    /// Namespace admitted at spawn, retained through process-group settlement.
    pub write_scope: crate::WriteScope,
}

/// What the runtime found when the task's process exited.
///
/// Produced exactly once per task, by the reaper, without any tool call. A
/// waiter reads it; it never computes it (`docs/ARCHITECTURE.md` §18.3 H).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackgroundSettlement {
    /// Workspace-relative paths changed in this task's admitted namespace.
    pub modified: Vec<String>,
    /// An execution invariant failure, when one was mechanically observed.
    pub violation: Option<String>,
    /// Why the change set is unknown, when the workspace could not be diffed.
    pub note: Option<String>,
    /// A full-workspace recovery snapshot, absent for scoped accounting trees.
    pub snapshot: Option<SnapshotId>,
}

const MAX_CONCURRENT: usize = 4;
const MAX_LOG_BYTES: usize = 256 * 1024;
/// Completed task records kept for later `get`/`wait` calls. Pruning happens
/// when a new task is spawned, so a waiter cannot lose the task that just woke
/// it. At most `MAX_CONCURRENT` newly terminal records can temporarily sit
/// above this bound before the next spawn.
const MAX_RETAINED_TERMINAL_TASKS: usize = 64;

/// How often the post-reap watcher probes whether the task's process group is
/// gone. Low frequency on purpose: an owned dev server can run for hours, and
/// this is a single `signal(0)` probe, not a process scan.
const GROUP_WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// How long the reaper lets the output pumps flush after the process group is
/// confirmed gone, before it finishes the task anyway. Bounded on purpose: an
/// fd inherited by a process outside the group must not extend the task.
const LOG_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BackgroundTaskStatus {
    Running,
    /// Kill requested; process has been signaled but has not reaped yet.
    Killing,
    Exited,
    Killed,
}

/// How long the runtime retains ownership of a background process.
///
/// Every lifetime retains its creating session. Goal/Runtime are legacy local
/// ownership; Session/Persistent run in the independent execution substrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BackgroundTaskLifetime {
    /// Default: stop the process when the creating goal reaches a terminal state.
    Goal,
    /// Keep the process until it exits, is explicitly stopped, or the runtime
    /// shuts down. Intended for user-requested dev servers and watchers.
    Runtime,
    /// Hosted work recoverable across runtime replacement; session deletion stops it.
    Session,
    /// Explicitly requested service, owned by the independent Execution Host.
    Persistent,
}

impl BackgroundTaskStatus {
    fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Killing)
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Exited | Self::Killed)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackgroundTaskSnapshot {
    pub id: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub status: BackgroundTaskStatus,
    pub exit_code: Option<i32>,
    pub log: String,
    pub duration_ms: u64,
    /// The process-group leader's OS pid, captured at spawn and retained for
    /// the task's whole record. A mechanical fact for a detail view; it carries
    /// no authority — stopping a task still goes through this registry, never a
    /// raw signal. `None` only for a task that never reached a process.
    #[serde(default)]
    pub pid: Option<u32>,
    /// The session that owns this task, when known. Session-owned tasks can be
    /// stopped by their owner through the runtime; daemon-scoped tasks have no
    /// owner and are not addressable by a client session.
    pub owner_scope: Option<String>,
}

/// One atomic observation of status and a bounded slice of retained output.
/// `snapshot.log` is the delta, not a second copy of the full task log.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackgroundTaskObservation {
    pub snapshot: BackgroundTaskSnapshot,
    /// Terminal truth is retained even after its mutation report is delivered.
    pub settlement: Option<BackgroundSettlement>,
    pub dropped_bytes: u64,
    pub next_cursor: u64,
    pub log_remaining: bool,
}

/// Registry-owned lifecycle facts for UI/runtime projections.
#[derive(Debug, Clone)]
pub enum BackgroundTaskEvent {
    Started {
        owner_scope: Option<String>,
        task: BackgroundTaskSnapshot,
    },
    /// A stream-tagged chunk of the task's stdout/stderr, after sanitization
    /// and the registry's own log cap. Live output for a detail view; the
    /// final snapshot on [`Self::Exited`] stays authoritative.
    Output {
        owner_scope: Option<String>,
        task_id: String,
        chunk: String,
    },
    Exited {
        owner_scope: Option<String>,
        task: BackgroundTaskSnapshot,
    },
}

#[derive(Debug, Clone, Copy)]
enum BackgroundOutputStream {
    Stdout,
    Stderr,
}

impl BackgroundOutputStream {
    fn label(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// Tracks live process identities for kill-on-drop of the registry handle.
///
/// Strong refs live only on [`BackgroundTaskRegistry`] (and its `Clone`s).
/// Reapers hold a [`std::sync::Weak`] and upgrade only to `remove` on exit —
/// so session/registry drop drops this while wait reapers still hold the task
/// map, and `Drop` reaps remaining Running processes (design §2.4).
#[derive(Default)]
struct KillOnDrop {
    live: std::sync::Mutex<HashMap<String, ProcessIdentity>>,
}

impl KillOnDrop {
    fn insert(&self, id: String, identity: ProcessIdentity) {
        if let Ok(mut map) = self.live.lock() {
            map.insert(id, identity);
        }
    }

    fn remove(&self, id: &str) {
        if let Ok(mut map) = self.live.lock() {
            map.remove(id);
        }
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Ok(map) = self.live.get_mut() {
            for identity in map.values() {
                identity.kill_tree();
            }
        }
    }
}

struct TaskInner {
    id: String,
    /// Which session started this task, when known. Daemon-scoped tasks have
    /// no owner. Session-owned task cleanup follows `lifetime`; runtime shutdown
    /// reaps every remaining task regardless of lifetime (R004 F7).
    owner_scope: Option<String>,
    writer_scope: String,
    write_scope: crate::WriteScope,
    lifetime: BackgroundTaskLifetime,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    status: BackgroundTaskStatus,
    exit_code: Option<i32>,
    /// The process-group leader's OS pid, recorded at spawn and kept after the
    /// identity is dropped so a finished task's detail can still name it.
    pid: Option<u32>,
    log: String,
    /// Absolute byte position in the sanitized output stream. The retained
    /// log may be truncated, so a plain index into `log` is not a cursor.
    log_end: u64,
    log_prefix_len: usize,
    default_log_cursor: u64,
    started: Instant,
    finished: Option<Instant>,
    child: Option<ManagedProcess>,
    identity: Option<ProcessIdentity>,
    done: Arc<Notify>,
    changed: Arc<Notify>,
    process_done: bool,
    log_pumps_remaining: u8,
    /// The process group this task owns has been confirmed gone, so any log
    /// pump still open is an fd held outside our tree and must not keep the
    /// task Running. Set by the post-reap group watcher.
    group_reaped: bool,
    /// Consumed by the reaper when the process exits, so the diff and any
    /// accounting runs exactly once and without waiting for a tool call.
    mutation_baseline: Option<MutationBaseline>,
    /// What the reaper found. Read by a waiter; never produced by one.
    settlement: Option<BackgroundSettlement>,
    settlement_reported: bool,
    /// Keeps the private scratch, its OS lease and (on Windows) the write-root
    /// labels alive until the child and log pumps finish — so a backgrounded
    /// command holds its whole confinement for its whole life.
    /// [`finalize_if_drained`] drops it at that point.
    sandbox_scratch: Option<crate::command::SandboxScratch>,
}

/// Process-backed background task registry shared via [`Arc`] on tool context.
#[derive(Clone)]
pub struct BackgroundTaskRegistry {
    instance_identity: Arc<str>,
    inner: Arc<Mutex<RegistryState>>,
    /// Spawn reservations live outside the async mutex so dropping a
    /// cancelled spawn future can release its slot synchronously.
    pending_spawns: Arc<AtomicUsize>,
    #[cfg(test)]
    spawn_registration_hook: Arc<std::sync::Mutex<Option<Arc<SpawnRegistrationHook>>>>,
    /// The one spawn path, shared with foreground execution (PR 4).
    runner: CommandRunner,
    lifecycle_events: broadcast::Sender<BackgroundTaskEvent>,
    /// Dropped when the last registry handle is dropped (session end).
    kill_on_drop: Arc<KillOnDrop>,
    execution_host: Option<crate::execution_host::ExecutionHostConfig>,
    retain_process_facts: bool,
    hosted_monitors: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl Default for BackgroundTaskRegistry {
    fn default() -> Self {
        Self::with_environment(Arc::new(leveler_core::environment().clone()))
    }
}

#[derive(Default)]
struct RegistryState {
    tasks: HashMap<String, TaskInner>,
    next: u64,
}

#[cfg(test)]
#[derive(Default)]
struct SpawnRegistrationHook {
    reached: Notify,
    release: Notify,
}

/// Cancellation-safe ownership of one reserved background slot and, once the
/// OS spawn succeeds, its not-yet-registered process.
///
/// There is deliberately no async work in `Drop`: a future cancelled while it
/// waits to re-enter the registry releases the capacity slot and kills the
/// process tree immediately, so no child can escape the registry boundary.
struct SpawnReservation {
    pending_spawns: Arc<AtomicUsize>,
    process: Option<ManagedProcess>,
    committed: bool,
}

impl SpawnReservation {
    fn new(pending_spawns: Arc<AtomicUsize>) -> Self {
        Self {
            pending_spawns,
            process: None,
            committed: false,
        }
    }

    fn attach(&mut self, process: ManagedProcess) {
        self.process = Some(process);
    }

    fn process_mut(&mut self) -> &mut ManagedProcess {
        self.process
            .as_mut()
            .expect("spawned process must remain owned by its reservation")
    }

    fn take_process(&mut self) -> ManagedProcess {
        self.process
            .take()
            .expect("spawned process must be transferred into the registry")
    }

    fn commit(mut self) {
        self.committed = true;
        self.pending_spawns.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(mut process) = self.process.take() {
            process.identity().kill_tree();
            process.start_kill();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = process.wait().await;
                });
            }
        }
        self.pending_spawns.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Immutable set of process identities detached before a terminal is
/// published and safe to settle afterwards.
pub struct BackgroundCleanupTicket {
    registry: BackgroundTaskRegistry,
    ids: Vec<String>,
}

impl BackgroundCleanupTicket {
    /// Signal every process captured by this ticket. Processes admitted later
    /// under the same scope are not part of the ticket and remain untouched.
    pub async fn settle(self) -> usize {
        let mut reaped = 0;
        for id in self.ids {
            if self.registry.kill(&id).await.is_ok() {
                reaped += 1;
            }
        }
        reaped
    }

    /// Whether the ticket captured no active process.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

impl BackgroundTaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_environment(environment: Arc<leveler_core::EnvSnapshot>) -> Self {
        let (lifecycle_events, _) = broadcast::channel(256);
        Self {
            instance_identity: Arc::from(leveler_core::new_uuid_string()),
            inner: Arc::new(Mutex::new(RegistryState::default())),
            pending_spawns: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            spawn_registration_hook: Arc::new(std::sync::Mutex::new(None)),
            kill_on_drop: Arc::new(KillOnDrop::default()),
            execution_host: None,
            retain_process_facts: false,
            hosted_monitors: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            runner: CommandRunner::with_environment(environment),
            lifecycle_events,
        }
    }

    pub(crate) fn retain_process_facts(mut self) -> Self {
        self.retain_process_facts = true;
        self
    }
    pub(crate) async fn forget_terminal_fact(&self, id: &str) -> Result<(), String> {
        let mut st = self.inner.lock().await;
        if st.tasks.get(id).is_some_and(|t| !t.status.is_terminal()) {
            return Err("cannot forget a live process fact".into());
        }
        st.tasks.remove(id);
        Ok(())
    }

    /// Configure independent ownership for explicitly hosted lifetimes.
    pub fn with_execution_host(
        mut self,
        config: crate::execution_host::ExecutionHostConfig,
    ) -> Self {
        self.execution_host = Some(config);
        self
    }

    fn journal(&self) -> Result<crate::host_settlement::SettlementJournal, String> {
        let config = self
            .execution_host
            .as_ref()
            .ok_or("persistent execution is not configured")?;
        crate::host_settlement::SettlementJournal::open(&config.semantic_dir())
    }

    async fn host_client(&self) -> Result<crate::execution_host::ExecutionHostClient, String> {
        let config = self
            .execution_host
            .clone()
            .ok_or("persistent execution is not configured")?;
        crate::execution_host::ExecutionHostClient::connect(config).await
    }

    /// Reconcile process facts without launching a new host. Unknown owner
    /// state remains an error and retains every outstanding write reservation.
    pub async fn reconcile_hosted(&self) -> Result<(), String> {
        let Some(config) = &self.execution_host else {
            return Ok(());
        };
        let Some(client) = crate::execution_host::ExecutionHostClient::probe(config).await? else {
            if !self.journal()?.pending()?.is_empty() {
                return Err(
                    "Execution Host is absent with outstanding semantic reservations".into(),
                );
            }
            return Ok(());
        };
        let journal = self.journal()?;
        for task in client.list().await? {
            if task.snapshot.status.is_terminal() {
                journal.reconcile(&task.snapshot.id).await?;
                client
                    .acknowledge(
                        &task.snapshot.id,
                        task.snapshot
                            .owner_scope
                            .as_deref()
                            .ok_or("missing hosted owner")?,
                    )
                    .await?;
            } else {
                self.monitor_hosted(task.snapshot)?;
            }
        }
        Ok(())
    }

    pub async fn try_all_snapshots(&self) -> Result<Vec<BackgroundTaskSnapshot>, String> {
        let mut snapshots = {
            let st = self.inner.lock().await;
            st.tasks.values().map(snapshot).collect::<Vec<_>>()
        };
        if let Some(config) = &self.execution_host {
            if let Some(client) = crate::execution_host::ExecutionHostClient::probe(config).await? {
                for task in client.list().await? {
                    if task.snapshot.status.is_terminal() {
                        self.journal()?.reconcile(&task.snapshot.id).await?;
                        client
                            .acknowledge(
                                &task.snapshot.id,
                                task.snapshot
                                    .owner_scope
                                    .as_deref()
                                    .ok_or("missing hosted owner")?,
                            )
                            .await?;
                    }
                    snapshots.push(task.snapshot);
                }
            } else if !self.journal()?.pending()?.is_empty() {
                return Err(
                    "Execution Host is absent; outstanding service states are unknown".into(),
                );
            }
        }
        Ok(snapshots)
    }

    pub async fn try_active_snapshots_for_scope(
        &self,
        scope: &str,
    ) -> Result<Vec<BackgroundTaskSnapshot>, String> {
        Ok(self
            .try_active_snapshots()
            .await?
            .into_iter()
            .filter(|t| t.owner_scope.as_deref() == Some(scope))
            .collect())
    }

    pub async fn try_active_snapshots(&self) -> Result<Vec<BackgroundTaskSnapshot>, String> {
        Ok(self
            .try_all_snapshots()
            .await?
            .into_iter()
            .filter(|t| t.status.is_active())
            .collect())
    }

    /// Only locally owned, nonrecoverable tasks prevent runtime replacement.
    pub async fn try_update_blockers(&self) -> Result<Vec<BackgroundTaskSnapshot>, String> {
        // Validate remote owner availability too: incompatibility is never idle success.
        self.try_active_snapshots().await?;
        Ok(self.active_snapshots().await)
    }

    pub async fn kill_session(&self, owner: &str) -> Result<(), String> {
        let local_ids = self.active_ids_for_scope(owner).await;
        for id in local_ids {
            self.kill_owned(&id, owner).await?;
        }
        if let Some(config) = &self.execution_host
            && let Some(client) = crate::execution_host::ExecutionHostClient::probe(config).await?
        {
            let ids = client
                .list()
                .await?
                .into_iter()
                .filter(|t| {
                    t.lifetime == BackgroundTaskLifetime::Session
                        && t.snapshot.owner_scope.as_deref() == Some(owner)
                        && t.snapshot.status.is_active()
                })
                .map(|t| t.snapshot.id)
                .collect::<Vec<_>>();
            for id in ids {
                self.kill_owned(&id, owner).await?;
                let result = self
                    .wait_owned(
                        &id,
                        owner,
                        Some(Duration::from_secs(5)),
                        &CancellationToken::new(),
                    )
                    .await?;
                if !result.status.is_terminal() {
                    return Err(format!("session task `{id}` has not stopped"));
                }
            }
        }
        Ok(())
    }

    async fn spawn_hosted(
        &self,
        request: ProcessRequest,
        baseline: Option<MutationBaseline>,
        owner: Option<&str>,
        writer: &str,
        lifetime: BackgroundTaskLifetime,
    ) -> Result<String, String> {
        let owner = owner.ok_or("hosted service requires a creating session")?;
        let config = self
            .execution_host
            .clone()
            .ok_or("persistent execution is not configured")?;
        let client = crate::execution_host::ExecutionHostClient::ensure(config).await?;
        let id = format!("host-{}", crate::execution_host::fresh_id()?);
        self.journal()?
            .prepare_with_scope(&id, owner, writer, baseline, request.write_scope.clone())
            .await?;
        // Transport failure is ambiguous: retain the prepared write reservation.
        let task = match client.spawn(&id, request, owner, writer, lifetime).await {
            Ok(task) => task,
            Err(crate::execution_host::SpawnError::Rejected(error)) => {
                self.journal()?.abandon_prepared(&id)?;
                return Err(error);
            }
            Err(crate::execution_host::SpawnError::OutcomeUnknown(error)) => return Err(error),
        };
        let _ = self.lifecycle_events.send(BackgroundTaskEvent::Started {
            owner_scope: Some(owner.into()),
            task,
        });
        self.monitor_hosted(self.host_client().await?.get_owned(&id, owner).await?)?;
        Ok(id)
    }

    fn monitor_hosted(&self, task: BackgroundTaskSnapshot) -> Result<(), String> {
        let config = self
            .execution_host
            .clone()
            .ok_or("execution host is not configured")?;
        let owner = task.owner_scope.ok_or("hosted task has no session owner")?;
        let id = task.id;
        let monitors = self.hosted_monitors.clone();
        if !monitors
            .lock()
            .map_err(|_| "hosted monitor lock poisoned")?
            .insert(id.clone())
        {
            return Ok(());
        }
        let events = self.lifecycle_events.clone();
        // Do not retain the local registry's KillOnDrop through a remote monitor.
        tokio::spawn(async move {
            let mut cursor = Some(0);
            let mut reported_error: Option<String> = None;
            loop {
                let result: Result<bool, String> = async {
                    let client =
                        crate::execution_host::ExecutionHostClient::connect(config.clone()).await?;
                    let journal =
                        crate::host_settlement::SettlementJournal::open(&config.semantic_dir())?;
                    let observation = client
                        .observe_owned(&id, &owner, cursor, 64 * 1024, Duration::from_secs(1))
                        .await?;
                    let mut terminal_snapshot = None;
                    if observation.snapshot.status.is_terminal() {
                        // Any failure leaves the terminal unpublished and its
                        // namespace reserved. Retry, including concurrent commit.
                        journal.reconcile(&id).await?;
                        // Exit events carry the complete retained log, not the
                        // last observation page which could omit its tail.
                        terminal_snapshot = Some(client.get_owned(&id, &owner).await?);
                        client.acknowledge(&id, &owner).await?;
                    }
                    cursor = Some(observation.next_cursor);
                    if observation.dropped_bytes > 0 {
                        let _ = events.send(BackgroundTaskEvent::Output {
                            owner_scope: Some(owner.clone()),
                            task_id: id.clone(),
                            chunk: format!(
                                "[log history truncated: {} bytes unavailable]\n",
                                observation.dropped_bytes
                            ),
                        });
                    }
                    if !observation.snapshot.log.is_empty() {
                        let _ = events.send(BackgroundTaskEvent::Output {
                            owner_scope: Some(owner.clone()),
                            task_id: id.clone(),
                            chunk: observation.snapshot.log.clone(),
                        });
                    }
                    if observation.snapshot.status.is_terminal() {
                        let _ = events.send(BackgroundTaskEvent::Exited {
                            owner_scope: Some(owner.clone()),
                            task: terminal_snapshot.ok_or("missing terminal snapshot")?,
                        });
                        return Ok(true);
                    }
                    Ok(false)
                }
                .await;
                match result {
                    Ok(true) => break,
                    Ok(false) => reported_error = None,
                    Err(error) => {
                        if reported_error.as_ref() != Some(&error) {
                            tracing::error!("hosted task observation pending: {error}");
                            reported_error = Some(error);
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
            if let Ok(mut active) = monitors.lock() {
                active.remove(&id);
            }
        });
        Ok(())
    }

    pub async fn spawn(
        &self,
        request: ProcessRequest,
        mutation_baseline: Option<MutationBaseline>,
    ) -> Result<String, String> {
        self.spawn_owned(request, mutation_baseline, None).await
    }

    /// Spawn with an owner scope (typically the session id). Owned tasks can
    /// be reaped as a group via [`Self::kill_scope`]; unowned tasks stay
    /// daemon-scoped (R004 F7).
    pub async fn spawn_owned(
        &self,
        request: ProcessRequest,
        mutation_baseline: Option<MutationBaseline>,
        owner_scope: Option<&str>,
    ) -> Result<String, String> {
        self.spawn_owned_with_lifetime(
            request,
            mutation_baseline,
            owner_scope,
            BackgroundTaskLifetime::Goal,
        )
        .await
    }

    /// Spawn an owned task with an explicit cleanup boundary. Runtime-lived
    /// tasks retain their session owner so list/logs/stop keep working after
    /// the goal that created them has completed.
    pub async fn spawn_owned_with_lifetime(
        &self,
        request: ProcessRequest,
        mutation_baseline: Option<MutationBaseline>,
        owner_scope: Option<&str>,
        lifetime: BackgroundTaskLifetime,
    ) -> Result<String, String> {
        self.spawn_for_writer(request, mutation_baseline, owner_scope, "parent", lifetime)
            .await
    }

    pub async fn spawn_for_writer(
        &self,
        request: ProcessRequest,
        mutation_baseline: Option<MutationBaseline>,
        owner_scope: Option<&str>,
        writer_scope: &str,
        lifetime: BackgroundTaskLifetime,
    ) -> Result<String, String> {
        if matches!(
            lifetime,
            BackgroundTaskLifetime::Persistent | BackgroundTaskLifetime::Session
        ) {
            return self
                .spawn_hosted(
                    request,
                    mutation_baseline,
                    owner_scope,
                    writer_scope,
                    lifetime,
                )
                .await;
        }
        let (id, mut reservation) = {
            let mut st = self.inner.lock().await;
            if !self.retain_process_facts {
                prune_terminal_tasks(&mut st);
            }
            let running = st.tasks.values().filter(|t| t.status.is_active()).count();
            if running + self.pending_spawns.load(Ordering::Acquire) >= MAX_CONCURRENT {
                return Err(format!(
                    "background task limit reached ({MAX_CONCURRENT} concurrent)"
                ));
            }
            self.pending_spawns.fetch_add(1, Ordering::AcqRel);
            st.next += 1;
            (
                format!("bg-{}", st.next),
                SpawnReservation::new(self.pending_spawns.clone()),
            )
        };

        // `CommandRunner::spawn` is the one confining spawn on every host, so a
        // background command is confined exactly like a foreground one and
        // fails closed in exactly the same place. This registry adds no policy
        // of its own.
        let process = self
            .runner
            .spawn(&request)
            .await
            .map_err(|error| format!("spawn background {}: {error}", request.program))?;
        reservation.attach(process);
        #[cfg(test)]
        if let Some(hook) = self
            .spawn_registration_hook
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        {
            hook.reached.notify_one();
            hook.release.notified().await;
        }
        let identity = reservation.process_mut().identity();
        let pid = process_identity_pid(&identity);
        let stdout = reservation.process_mut().take_stdout();
        let stderr = reservation.process_mut().take_stderr();
        let sandbox_scratch = reservation.process_mut().take_sandbox_scratch();
        let log_pumps_remaining = u8::from(stdout.is_some()) + u8::from(stderr.is_some());
        let done = Arc::new(Notify::new());
        let reg = self.inner.clone();
        // Weak: must not keep KillOnDrop alive past registry handle drop.
        let kill_on_drop = Arc::downgrade(&self.kill_on_drop);
        let tid = id.clone();

        let mut st = self.inner.lock().await;
        self.kill_on_drop.insert(id.clone(), identity);
        let process = reservation.take_process();
        st.tasks.insert(
            id.clone(),
            TaskInner {
                id: id.clone(),
                owner_scope: owner_scope.map(str::to_string),
                writer_scope: writer_scope.to_string(),
                write_scope: request.write_scope.clone(),
                lifetime,
                program: request.program.clone(),
                args: request.args.clone(),
                cwd: request.cwd.clone(),
                status: BackgroundTaskStatus::Running,
                exit_code: None,
                pid: Some(pid),
                log: String::new(),
                log_end: 0,
                log_prefix_len: 0,
                default_log_cursor: 0,
                started: Instant::now(),
                finished: None,
                child: Some(process),
                identity: Some(identity),
                done: done.clone(),
                changed: Arc::new(Notify::new()),
                process_done: false,
                log_pumps_remaining,
                group_reaped: false,
                mutation_baseline,
                settlement: None,
                settlement_reported: false,
                sandbox_scratch,
            },
        );
        if let Some(task) = st.tasks.get(&id) {
            let _ = self.lifecycle_events.send(BackgroundTaskEvent::Started {
                owner_scope: task.owner_scope.clone(),
                task: snapshot(task),
            });
        }
        reservation.commit();
        drop(st);

        let log_cancel = CancellationToken::new();
        spawn_log_pump(
            reg.clone(),
            tid.clone(),
            stdout,
            BackgroundOutputStream::Stdout,
            self.lifecycle_events.clone(),
            log_cancel.clone(),
        );
        spawn_log_pump(
            reg.clone(),
            tid.clone(),
            stderr,
            BackgroundOutputStream::Stderr,
            self.lifecycle_events.clone(),
            log_cancel.clone(),
        );

        let lifecycle_events = self.lifecycle_events.clone();
        tokio::spawn(async move {
            // Capture the owned process-group identity BEFORE waiting. The
            // direct child's exit is not the task's exit: descendants that
            // share its process group are still this task's workload, and
            // dropping the identity here is what used to make them
            // unstoppable (and the task immortal).
            let (code, identity) = {
                let mut st = reg.lock().await;
                let Some(task) = st.tasks.get_mut(&tid) else {
                    return;
                };
                let Some(mut child) = task.child.take() else {
                    return;
                };
                let identity = task.identity;
                drop(st);
                let code = match child.wait().await {
                    Ok(s) => s.code(),
                    Err(_) => None,
                };
                (code, identity)
            };
            {
                let mut st = reg.lock().await;
                let Some(task) = st.tasks.get_mut(&tid) else {
                    return;
                };
                task.process_done = true;
                task.exit_code = code;
            }
            // The process group, not its launcher, owns the write lifetime.
            // Keep the baseline and prohibit terminal publication until every
            // member is gone and its final mutations have been settled.
            if let Some(identity) = identity {
                while !identity.group_gone().await {
                    tokio::time::sleep(GROUP_WATCH_INTERVAL).await;
                }
            }
            let baseline = {
                let mut st = reg.lock().await;
                st.tasks.get_mut(&tid).and_then(|task| {
                    // Group identity expires when the group is gone, before
                    // potentially slow git accounting can reuse that pgid.
                    task.identity = None;
                    task.mutation_baseline.take()
                })
            };
            let settlement = match baseline {
                Some(baseline) => Some(settle(&baseline).await),
                None => None,
            };
            // Pipes may have been inherited outside the owned group. Allow a
            // bounded drain; no log pump can publish terminal before this block.
            let deadline = Instant::now() + LOG_DRAIN_GRACE;
            loop {
                let drained = {
                    let st = reg.lock().await;
                    st.tasks
                        .get(&tid)
                        .is_none_or(|task| task.log_pumps_remaining == 0)
                };
                if drained || Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            log_cancel.cancel();
            {
                let mut st = reg.lock().await;
                if let Some(task) = st.tasks.get_mut(&tid) {
                    task.settlement = settlement;
                    task.group_reaped = true;
                    if finalize_if_drained(task) {
                        let _ = lifecycle_events.send(BackgroundTaskEvent::Exited {
                            owner_scope: task.owner_scope.clone(),
                            task: snapshot(task),
                        });
                    }
                }
            }
            if let Some(kod) = kill_on_drop.upgrade() {
                kod.remove(&tid);
            }
        });

        Ok(id)
    }

    /// One atomic process/log fact. Persisting separately read cursors and logs
    /// can corrupt archive offsets when an output pump truncates between reads.
    pub(crate) async fn process_fact(
        &self,
        id: &str,
    ) -> Option<(BackgroundTaskSnapshot, Option<u32>, u64, usize)> {
        let state = self.inner.lock().await;
        state.tasks.get(id).map(|task| {
            let pid = task.identity.map(|identity| {
                #[cfg(unix)]
                {
                    identity.pgid() as u32
                }
                #[cfg(not(unix))]
                {
                    identity.pid()
                }
            });
            (snapshot(task), pid, task.log_end, task.log_prefix_len)
        })
    }

    pub async fn get(&self, id: &str) -> Option<BackgroundTaskSnapshot> {
        let st = self.inner.lock().await;
        st.tasks.get(id).map(snapshot)
    }

    /// Inspect ownership without reconciling terminal facts, acknowledging them,
    /// advancing a log cursor, or consuming a settlement report.
    pub async fn peek_task_owner(&self, id: &str) -> Result<Option<String>, String> {
        if id.starts_with("host-") {
            return Ok(self
                .host_client()
                .await?
                .get_unrestricted(id)
                .await?
                .owner_scope);
        }
        let state = self.inner.lock().await;
        state
            .tasks
            .get(id)
            .map(|task| task.owner_scope.clone())
            .ok_or_else(|| format!("unknown task `{id}`"))
    }

    /// Resolve local task creation and ownership proof without consuming output.
    /// Hosted tasks require host incarnation proof and currently earn no grant.
    pub async fn resource_identity(
        &self,
        id: &str,
    ) -> Result<Option<leveler_core::ResourceIdentity>, String> {
        if id.starts_with("host-") {
            return Ok(None);
        }
        let state = self.inner.lock().await;
        let task = state
            .tasks
            .get(id)
            .ok_or_else(|| format!("unknown task `{id}`"))?;
        let Some(pid) = task.pid else {
            return Ok(None);
        };
        Ok(Some(leveler_core::ResourceIdentity::BackgroundTask {
            runtime: self.instance_identity.to_string(),
            task_id: task.id.clone(),
            owner: task
                .owner_scope
                .clone()
                .unwrap_or_else(|| format!("runtime:{}", self.instance_identity)),
            process_identity: format!("{}:{}:{pid}", self.instance_identity, task.id),
        }))
    }

    pub async fn get_owned(&self, id: &str, owner: &str) -> Result<BackgroundTaskSnapshot, String> {
        self.get_authorized(id, owner, false).await
    }

    pub async fn get_authorized(
        &self,
        id: &str,
        owner: &str,
        unrestricted: bool,
    ) -> Result<BackgroundTaskSnapshot, String> {
        if id.starts_with("host-") {
            let client = self.host_client().await?;
            let task = if unrestricted {
                client.get_unrestricted(id).await?
            } else {
                client.get_owned(id, owner).await?
            };
            if task.status.is_terminal() {
                self.journal()?.reconcile(id).await?;
                if unrestricted {
                    client.acknowledge_unrestricted(id).await?;
                } else {
                    client.acknowledge(id, owner).await?;
                };
            }
            return Ok(task);
        }
        let st = self.inner.lock().await;
        let task = st
            .tasks
            .get(id)
            .ok_or_else(|| format!("unknown task `{id}`"))?;
        if !unrestricted {
            check_owner(task, owner)?;
        }
        Ok(snapshot(task))
    }

    pub async fn wait_owned(
        &self,
        id: &str,
        owner: &str,
        timeout: Option<Duration>,
        cancellation: &CancellationToken,
    ) -> Result<BackgroundTaskSnapshot, String> {
        self.wait_authorized(id, owner, false, timeout, cancellation)
            .await
    }

    pub async fn wait_authorized(
        &self,
        id: &str,
        owner: &str,
        unrestricted: bool,
        timeout: Option<Duration>,
        cancellation: &CancellationToken,
    ) -> Result<BackgroundTaskSnapshot, String> {
        if id.starts_with("host-") {
            let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
            loop {
                let task = self.get_authorized(id, owner, unrestricted).await?;
                if task.status.is_terminal()
                    || deadline.is_some_and(|d| tokio::time::Instant::now() >= d)
                {
                    return Ok(task);
                }
                tokio::select! { _ = cancellation.cancelled() => return Err("wait cancelled".into()), _ = tokio::time::sleep(Duration::from_millis(50)) => {} }
            }
        }
        // Task IDs are never reused and owner_scope is immutable. The wait
        // subscribes only after this locked authorization; observation checks
        // ownership again before delivering bytes or advancing a cursor.
        self.get_authorized(id, owner, unrestricted).await?;
        self.wait(id, timeout, cancellation).await
    }

    pub async fn take_settlement_owned(
        &self,
        id: &str,
        owner: &str,
    ) -> Result<Option<BackgroundSettlement>, String> {
        self.take_settlement_authorized(id, owner, false).await
    }

    pub async fn take_settlement_authorized(
        &self,
        id: &str,
        owner: &str,
        unrestricted: bool,
    ) -> Result<Option<BackgroundSettlement>, String> {
        if id.starts_with("host-") {
            let task = self.get_authorized(id, owner, unrestricted).await?;
            if !task.status.is_terminal() {
                return Ok(None);
            }
            return Ok(self.journal()?.read(id)?.settlement);
        }
        let mut st = self.inner.lock().await;
        let task = st
            .tasks
            .get_mut(id)
            .ok_or_else(|| format!("unknown task `{id}`"))?;
        if !unrestricted {
            check_owner(task, owner)?;
        }
        if task.settlement_reported {
            return Ok(None);
        }
        let settlement = task.settlement.clone();
        if settlement.is_some() {
            task.settlement_reported = true;
        }
        Ok(settlement)
    }

    /// End a writer's processes before its path ownership can be released.
    /// Failure keeps the writer's ownership live; signalling alone is not proof.
    pub async fn settle_writer(&self, owner: &str, writer: &str) -> Result<(), String> {
        let ids: Vec<String> = {
            let st = self.inner.lock().await;
            st.tasks
                .values()
                .filter(|task| {
                    task.owner_scope.as_deref() == Some(owner)
                        && task.writer_scope == writer
                        && task.status.is_active()
                })
                .map(|task| task.id.clone())
                .collect()
        };
        // Hosted services retain their durable namespace independently of a
        // writer token. write_conflicts/foreign_write_paths consult the journal
        // even after this writer retires; cleanup must not change their lifetime.
        for id in &ids {
            self.kill_owned(id, owner).await?;
        }
        for id in &ids {
            let state = self
                .wait_owned(
                    id,
                    owner,
                    Some(Duration::from_secs(5)),
                    &CancellationToken::new(),
                )
                .await?;
            if !state.status.is_terminal() {
                return Err(format!("background task `{id}` has not finished settling"));
            }
        }
        Ok(())
    }

    /// Live workloads retain their namespace until settlement. The ownership
    /// authority checks this before granting overlapping paths to a new writer.
    pub async fn write_conflicts(&self, owner: &str, paths: &[String]) -> Vec<String> {
        let st = self.inner.lock().await;
        let mut conflicts = st
            .tasks
            .values()
            .filter(|task| {
                task.owner_scope.as_deref() == Some(owner)
                    && task.status.is_active()
                    && paths
                        .iter()
                        .any(|path| scope_overlaps(&task.write_scope, &task.cwd, path))
            })
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        drop(st);
        if let Some(config) = &self.execution_host {
            match self.journal().and_then(|j| j.pending()) {
                Ok(tasks) => conflicts.extend(
                    tasks
                        .into_iter()
                        .filter(|t| {
                            t.owner == owner
                                && paths
                                    .iter()
                                    .any(|p| scope_overlaps(&t.write_scope, &config.repo_root, p))
                        })
                        .map(|t| t.task_id),
                ),
                Err(e) => {
                    tracing::error!("could not establish hosted write ownership: {e}");
                    conflicts.push(format!("hosted write ownership unavailable: {e}"));
                }
            }
        }
        conflicts
    }

    /// The actual namespaces still leased by other writers, including writers
    /// from an earlier turn whose cleanup did not complete. The current host's
    /// ownership map may be new; active process capabilities remain authoritative.
    pub async fn foreign_write_paths(&self, owner: &str, writer: &str) -> Vec<String> {
        let state = self.inner.lock().await;
        let mut paths = Vec::new();
        for task in state.tasks.values().filter(|task| {
            task.owner_scope.as_deref() == Some(owner)
                && task.writer_scope != writer
                && task.status.is_active()
        }) {
            match &task.write_scope {
                crate::WriteScope::None => {}
                crate::WriteScope::ScopedWorkspace { root, allowed, .. } => {
                    for path in allowed {
                        let relative = path.strip_prefix(root).unwrap_or(std::path::Path::new("."));
                        paths.push(if relative.as_os_str().is_empty() {
                            ".".into()
                        } else {
                            relative.to_string_lossy().into_owned()
                        });
                    }
                }
                _ => paths.push(".".into()),
            }
        }
        drop(state);
        if self.execution_host.is_some() {
            match self.journal().and_then(|j| j.pending()) {
                Ok(tasks) => {
                    for task in tasks
                        .into_iter()
                        .filter(|t| t.owner == owner && t.writer != writer)
                    {
                        match task.write_scope {
                            crate::WriteScope::None => {}
                            crate::WriteScope::ScopedWorkspace { root, allowed, .. } => {
                                for path in allowed {
                                    paths.push(
                                        path.strip_prefix(&root)
                                            .ok()
                                            .filter(|p| !p.as_os_str().is_empty())
                                            .map(|p| p.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| ".".into()),
                                    );
                                }
                            }
                            _ => paths.push(".".into()),
                        }
                    }
                }
                Err(error) => {
                    tracing::error!("could not establish hosted write ownership: {error}");
                    paths.push(".".into());
                }
            }
        }
        paths.sort();
        paths.dedup();
        paths
    }

    /// Subscribe to authoritative process lifecycle transitions.
    pub fn subscribe(&self) -> broadcast::Receiver<BackgroundTaskEvent> {
        self.lifecycle_events.subscribe()
    }

    /// Deliver the mutation report once without consuming the terminal facts.
    /// Independent observers must still see a rejected mutation as failure.
    pub async fn take_settlement(&self, id: &str) -> Option<BackgroundSettlement> {
        let mut st = self.inner.lock().await;
        let task = st.tasks.get_mut(id)?;
        if task.settlement_reported {
            return None;
        }
        let settlement = task.settlement.clone()?;
        task.settlement_reported = true;
        Some(settlement)
    }

    pub async fn wait(
        &self,
        id: &str,
        timeout: Option<Duration>,
        cancellation: &CancellationToken,
    ) -> Result<BackgroundTaskSnapshot, String> {
        if id.starts_with("host-") {
            let record = self.journal()?.read(id)?;
            return Box::pin(self.wait_owned(id, &record.owner, timeout, cancellation)).await;
        }
        let notify = {
            let st = self.inner.lock().await;
            let task = st
                .tasks
                .get(id)
                .ok_or_else(|| format!("unknown task `{id}`"))?;
            if task.status.is_terminal() {
                return Ok(snapshot(task));
            }
            task.done.clone()
        };
        // Register before the final state check. `notify_waiters` does not
        // retain a permit, so registering afterwards can lose completion.
        let wait_fut = notify.notified();
        tokio::pin!(wait_fut);
        wait_fut.as_mut().enable();
        {
            let st = self.inner.lock().await;
            if let Some(task) = st.tasks.get(id)
                && task.status.is_terminal()
            {
                return Ok(snapshot(task));
            }
        }
        if let Some(t) = timeout {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err("wait cancelled".into()),
                _ = &mut wait_fut => {}
                _ = tokio::time::sleep(t) => {
                    // Interval elapsed: return current snapshot. Do not kill
                    // the process. A still-running status is truthful, not an
                    // error — the waiter gets control back.
                    return self
                        .get(id)
                        .await
                        .ok_or_else(|| format!("task `{id}` disappeared"));
                }
            }
        } else {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err("wait cancelled".into()),
                _ = &mut wait_fut => {}
            }
        }
        self.get(id)
            .await
            .ok_or_else(|| format!("task `{id}` disappeared"))
    }

    /// Wait for unread output, a non-running status, or the bounded interval.
    /// Registration, cursor validation and delivery share the task lock, so
    /// output cannot land in a gap between checking and subscribing. Explicit
    /// cursors never consume the convenience reader's position. Completion-only
    /// consumers continue to use `wait`; log arrival must not settle a process.
    pub async fn observe(
        &self,
        id: &str,
        cursor: Option<u64>,
        max_bytes: usize,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<BackgroundTaskObservation, String> {
        self.observe_scoped(id, None, cursor, max_bytes, timeout, cancellation)
            .await
    }

    pub async fn observe_owned(
        &self,
        id: &str,
        owner: &str,
        cursor: Option<u64>,
        max_bytes: usize,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<BackgroundTaskObservation, String> {
        self.observe_scoped(id, Some(owner), cursor, max_bytes, timeout, cancellation)
            .await
    }

    pub async fn observe_authorized(
        &self,
        id: &str,
        owner: &str,
        unrestricted: bool,
        cursor: Option<u64>,
        max_bytes: usize,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<BackgroundTaskObservation, String> {
        self.observe_scoped(
            id,
            if unrestricted { None } else { Some(owner) },
            cursor,
            max_bytes,
            timeout,
            cancellation,
        )
        .await
    }

    async fn observe_scoped(
        &self,
        id: &str,
        owner: Option<&str>,
        cursor: Option<u64>,
        max_bytes: usize,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<BackgroundTaskObservation, String> {
        if id.starts_with("host-") {
            let client = self.host_client().await?;
            let mut result = tokio::select! {
                _ = cancellation.cancelled() => return Err("wait cancelled".into()),
                result = async { match owner { Some(owner) => client.observe_owned(id, owner, cursor, max_bytes, timeout).await, None => client.observe_unrestricted(id, cursor, max_bytes, timeout).await } } => result?,
            };
            if result.snapshot.status.is_terminal() {
                result.settlement = Some(self.journal()?.reconcile(id).await?);
                match owner {
                    Some(owner) => client.acknowledge(id, owner).await?,
                    None => client.acknowledge_unrestricted(id).await?,
                };
            }
            return Ok(result);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cancellation.is_cancelled() {
                return Err("wait cancelled".into());
            }
            let mut st = self.inner.lock().await;
            let task = st
                .tasks
                .get_mut(id)
                .ok_or_else(|| format!("unknown task `{id}`"))?;
            if let Some(owner) = owner {
                check_owner(task, owner)?;
            }
            let mut position = cursor.unwrap_or(task.default_log_cursor);
            if position > task.log_end {
                return Err(format!(
                    "log cursor {position} is beyond task `{id}` output"
                ));
            }
            if position < task.log_end
                || task.status != BackgroundTaskStatus::Running
                || tokio::time::Instant::now() >= deadline
            {
                let (log, dropped_bytes) = take_log_delta(
                    &task.log,
                    task.log_end,
                    task.log_prefix_len,
                    &mut position,
                    max_bytes,
                )?;
                if cursor.is_none() {
                    task.default_log_cursor = position;
                }
                return Ok(BackgroundTaskObservation {
                    snapshot: snapshot_with_log(task, log),
                    settlement: task.settlement.clone(),
                    dropped_bytes,
                    next_cursor: position,
                    log_remaining: position < task.log_end,
                });
            }
            let changed = task.changed.clone();
            let notified = changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            drop(st);
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err("wait cancelled".into()),
                _ = &mut notified => {},
                _ = tokio::time::sleep_until(deadline) => {},
            }
        }
    }

    /// Kill every non-terminal task regardless of owner. Used on runtime
    /// shutdown paths where Drop-based reaping may never run (R004 F7).
    /// How many background tasks are still alive.
    ///
    /// A runtime is not idle just because no turn is running: the wait_task
    /// incident was a `cargo check` that outlived its turn by half an hour,
    /// and replacing the process under it would have thrown that work away.
    pub async fn alive_count(&self) -> usize {
        self.inner
            .lock()
            .await
            .tasks
            .values()
            .filter(|t| t.status.is_active())
            .count()
    }

    /// Every active task across all scopes, oldest first.
    ///
    /// This is the read-only blocker list a retiring runtime reports: it is
    /// the SAME `is_active` predicate `alive_count` counts, so a client can
    /// never be shown a task the drain does not also wait on. It carries no
    /// authority — stopping a task still goes through the session-scoped
    /// runtime command.
    pub async fn active_snapshots(&self) -> Vec<BackgroundTaskSnapshot> {
        let st = self.inner.lock().await;
        let mut active: Vec<&TaskInner> =
            st.tasks.values().filter(|t| t.status.is_active()).collect();
        active.sort_by_key(|t| t.started);
        active.into_iter().map(snapshot).collect()
    }

    pub async fn kill_all(&self) -> usize {
        let ids: Vec<String> = {
            let st = self.inner.lock().await;
            st.tasks
                .values()
                .filter(|t| !t.status.is_terminal())
                .map(|t| t.id.clone())
                .collect()
        };
        let mut n = 0;
        for id in ids {
            if self.kill(&id).await.is_ok() {
                n += 1;
            }
        }
        n
    }

    /// Kill every non-terminal task owned by `scope` (best-effort; errors on
    /// individual tasks are ignored). Returns how many tasks were signalled.
    pub async fn kill_scope(&self, scope: &str) -> usize {
        let ids = self.active_ids_for_scope(scope).await;
        let mut n = 0;
        for id in ids {
            if self.kill(&id).await.is_ok() {
                n += 1;
            }
        }
        n
    }

    /// Snapshot the active process identities owned by one scope. Terminal
    /// settlement captures this list before publishing completion, then may
    /// reap exactly these ids afterwards without touching work admitted by a
    /// later turn in the same session.
    pub async fn active_ids_for_scope(&self, scope: &str) -> Vec<String> {
        {
            let st = self.inner.lock().await;
            st.tasks
                .values()
                .filter(|t| t.owner_scope.as_deref() == Some(scope) && !t.status.is_terminal())
                .map(|t| t.id.clone())
                .collect()
        }
    }

    /// Snapshot the tasks that are genuinely still live for one session.
    ///
    /// Reconnect projections use this rather than replaying task-start events:
    /// the registry owns process lifecycle truth, while terminal records are
    /// retained only for later `get`/`wait` calls.
    pub async fn active_snapshots_for_scope(&self, scope: &str) -> Vec<BackgroundTaskSnapshot> {
        let st = self.inner.lock().await;
        let mut snapshots: Vec<_> = st
            .tasks
            .values()
            .filter(|task| task.owner_scope.as_deref() == Some(scope) && task.status.is_active())
            .map(snapshot)
            .collect();
        snapshots.sort_by(|a, b| a.id.cmp(&b.id));
        snapshots
    }

    /// Detach an immutable cleanup ticket for one scope. The registry never
    /// holds its lock across process I/O, so this synchronization is bounded
    /// to an in-memory snapshot; process signalling happens only when the
    /// caller settles the ticket later.
    pub async fn detach_cleanup(&self, scope: &str) -> BackgroundCleanupTicket {
        let ids = {
            let st = self.inner.lock().await;
            st.tasks
                .values()
                .filter(|task| {
                    task.owner_scope.as_deref() == Some(scope)
                        && task.lifetime == BackgroundTaskLifetime::Goal
                        && !task.status.is_terminal()
                })
                .map(|task| task.id.clone())
                .collect()
        };
        BackgroundCleanupTicket {
            registry: self.clone(),
            ids,
        }
    }

    /// Best-effort non-blocking scope inspection for diagnostics and tests.
    pub fn try_active_ids_for_scope(&self, scope: &str) -> Option<Vec<String>> {
        let Ok(st) = self.inner.try_lock() else {
            return None;
        };
        Some(
            st.tasks
                .values()
                .filter(|t| t.owner_scope.as_deref() == Some(scope) && !t.status.is_terminal())
                .map(|t| t.id.clone())
                .collect(),
        )
    }

    pub async fn kill(&self, id: &str) -> Result<BackgroundTaskSnapshot, String> {
        self.kill_scoped(id, None).await
    }

    pub async fn kill_owned(
        &self,
        id: &str,
        owner: &str,
    ) -> Result<BackgroundTaskSnapshot, String> {
        self.kill_scoped(id, Some(owner)).await
    }

    pub async fn kill_authorized(
        &self,
        id: &str,
        owner: &str,
        unrestricted: bool,
    ) -> Result<BackgroundTaskSnapshot, String> {
        self.kill_scoped(id, if unrestricted { None } else { Some(owner) })
            .await
    }

    async fn kill_scoped(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> Result<BackgroundTaskSnapshot, String> {
        if id.starts_with("host-") {
            let client = self.host_client().await?;
            let task = match owner {
                Some(owner) => client.kill_owned(id, owner).await?,
                None => client.kill_unrestricted(id).await?,
            };
            if task.status.is_terminal() {
                self.journal()?.reconcile(id).await?;
                match owner {
                    Some(owner) => client.acknowledge(id, owner).await?,
                    None => client.acknowledge_unrestricted(id).await?,
                };
            }
            return Ok(task);
        }
        let identity = {
            let mut st = self.inner.lock().await;
            let task = st
                .tasks
                .get_mut(id)
                .ok_or_else(|| format!("unknown task `{id}`"))?;
            if let Some(owner) = owner {
                check_owner(task, owner)?;
            }
            if task.status.is_terminal() {
                return Ok(snapshot(task));
            }
            if task.status == BackgroundTaskStatus::Killing {
                return Ok(snapshot(task));
            }
            let identity = task.identity;
            let has_child = task.child.is_some();
            // Without pid/pgid and without Child we cannot signal at all.
            if identity.is_none() && !has_child {
                if task.process_done {
                    return Ok(snapshot(task));
                }
                return Err(format!(
                    "task `{id}` has no process identity to signal (Child already taken)"
                ));
            }
            // Running → Killing; prefer pid/pgid so kill works after reaper take().
            task.status = BackgroundTaskStatus::Killing;
            task.changed.notify_waiters();
            if let Some(child) = task.child.as_mut() {
                child.start_kill();
            }
            identity
        };

        if let Some(identity) = identity {
            // SIGTERM then SIGKILL so stubborn children still die after
            // the reaper has taken `Child`.
            identity.terminate_tree().await;
        }

        let st = self.inner.lock().await;
        st.tasks
            .get(id)
            .map(snapshot)
            .ok_or_else(|| format!("task `{id}` disappeared"))
    }

    /// Test-only: whether the wait reaper has already `take()`n `Child`.
    #[cfg(all(test, unix))]
    async fn child_taken_for_test(&self, id: &str) -> Option<bool> {
        let st = self.inner.lock().await;
        st.tasks.get(id).map(|t| t.child.is_none())
    }
}

fn scope_overlaps(scope: &crate::WriteScope, cwd: &std::path::Path, path: &str) -> bool {
    let root = scope.root().unwrap_or(cwd);
    let path = root.join(path);
    let overlaps =
        |allowed: &std::path::Path| path.starts_with(allowed) || allowed.starts_with(&path);
    match scope {
        crate::WriteScope::None => false,
        crate::WriteScope::Unrestricted => true,
        crate::WriteScope::Workspace { root } | crate::WriteScope::WorkspaceWithGit { root } => {
            overlaps(root)
        }
        crate::WriteScope::ScopedWorkspace {
            allowed, excluded, ..
        } => allowed.iter().any(|a| overlaps(a)) && !excluded.iter().any(|e| path.starts_with(e)),
    }
}

fn check_owner(task: &TaskInner, owner: &str) -> Result<(), String> {
    if task.owner_scope.as_deref() != Some(owner) {
        return Err("background task is not owned by this session".into());
    }
    Ok(())
}

/// Evict the oldest completed records while preserving every running/killing
/// task. Called only before a spawn: completion waiters therefore get a stable
/// chance to observe their terminal snapshot.
fn prune_terminal_tasks(state: &mut RegistryState) {
    let mut terminal: Vec<(Instant, String)> = state
        .tasks
        .iter()
        .filter(|(_, task)| task.status.is_terminal())
        .map(|(id, task)| (task.finished.unwrap_or(task.started), id.clone()))
        .collect();
    let remove_count = terminal.len().saturating_sub(MAX_RETAINED_TERMINAL_TASKS);
    if remove_count == 0 {
        return;
    }
    terminal.sort_unstable();
    for (_, id) in terminal.into_iter().take(remove_count) {
        state.tasks.remove(&id);
    }
}

fn spawn_log_pump<R>(
    reg: Arc<Mutex<RegistryState>>,
    tid: String,
    stream: Option<R>,
    output_stream: BackgroundOutputStream,
    lifecycle_events: broadcast::Sender<BackgroundTaskEvent>,
    cancellation: CancellationToken,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(stream) = stream else {
        return;
    };
    tokio::spawn(async move {
        let mut reader = BufReader::new(stream);
        let mut buf = [0u8; 4096];
        loop {
            let read = tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                read = reader.read(&mut buf) => read,
            };
            match read {
                Ok(0) => break,
                Ok(n) => {
                    if let Some((owner_scope, chunk)) =
                        append_log(&reg, &tid, output_stream, &buf[..n]).await
                    {
                        let _ = lifecycle_events.send(BackgroundTaskEvent::Output {
                            owner_scope,
                            task_id: tid.clone(),
                            chunk,
                        });
                    }
                }
                Err(_) => break,
            }
        }
        let mut st = reg.lock().await;
        if let Some(task) = st.tasks.get_mut(&tid) {
            task.log_pumps_remaining = task.log_pumps_remaining.saturating_sub(1);
            if finalize_if_drained(task) {
                let _ = lifecycle_events.send(BackgroundTaskEvent::Exited {
                    owner_scope: task.owner_scope.clone(),
                    task: snapshot(task),
                });
            }
        }
    });
}

/// Account only the namespace enforced at spawn, after its entire process
/// group has exited. Settlement never rewrites the shared workspace.
async fn settle(baseline: &MutationBaseline) -> BackgroundSettlement {
    let root = &baseline.workspace_root;
    let id = &baseline.snapshot;

    let mut modified = Vec::new();
    let mut note = None;
    match crate::snapshot::WorkspaceSnapshot::changed_since_for_scope(
        root,
        id,
        &baseline.write_scope,
    )
    .await
    {
        Ok(changed) => modified = changed,
        Err(error) => {
            note = Some(format!(
                "could not diff the workspace after this background task ({error}); \
                 its file changes were not tracked"
            ));
        }
    }

    BackgroundSettlement {
        modified,
        violation: None,
        note,
        snapshot: (!matches!(
            baseline.write_scope,
            crate::WriteScope::ScopedWorkspace { .. }
        ))
        .then(|| id.clone()),
    }
}

fn finalize_if_drained(task: &mut TaskInner) -> bool {
    // The task owns its process group: the direct child is not the task. A
    // workload is over only when the child has been reaped AND the group it
    // owned is confirmed gone. The output pumps are a delivery channel, not
    // liveness — a pipe still held outside the group must not keep the task
    // Running, and an EOF before the group ends must not finish it either.
    if !task.process_done || !task.group_reaped {
        return false;
    }
    if task.status.is_terminal() {
        return false;
    }
    task.status = match task.status {
        BackgroundTaskStatus::Killing => BackgroundTaskStatus::Killed,
        BackgroundTaskStatus::Running => BackgroundTaskStatus::Exited,
        terminal => terminal,
    };
    task.finished = Some(Instant::now());
    // Ownership ends with the terminal state: a later signal must never reach
    // a process group id the kernel may have recycled.
    task.identity = None;
    // The process and any still-open output pumps are done, so no child can
    // use TMPDIR. Release potentially large temp files independently of
    // history retention.
    task.sandbox_scratch.take();
    task.done.notify_waiters();
    task.changed.notify_waiters();
    true
}

/// Append one read to the task log and return the sanitized chunk with the
/// task's owner scope, so the caller can publish live output. `None` when the
/// task is gone or the read carried nothing printable.
async fn append_log(
    reg: &Arc<Mutex<RegistryState>>,
    id: &str,
    stream: BackgroundOutputStream,
    bytes: &[u8],
) -> Option<(Option<String>, String)> {
    let mut st = reg.lock().await;
    let task = st.tasks.get_mut(id)?;
    if task.status.is_terminal() {
        return None;
    }
    let raw = String::from_utf8_lossy(bytes);
    // Sanitize BEFORE the registry buffer: the log cap and every projection are
    // then measured on the same clean text.
    let sanitized = leveler_core::sanitize_terminal_output(&raw);
    if sanitized.is_empty() {
        return None;
    }
    let chunk = format!("[{}] {sanitized}", stream.label());
    task.log.push_str(&chunk);
    task.log_end = task.log_end.saturating_add(chunk.len() as u64);
    if let Some(prefix_len) = truncate_log(&mut task.log) {
        task.log_prefix_len = prefix_len;
    }
    task.changed.notify_waiters();
    Some((task.owner_scope.clone(), chunk))
}

fn truncate_log(log: &mut String) -> Option<usize> {
    if log.len() > MAX_LOG_BYTES {
        // The marker embeds the dropped byte count, which depends on the
        // boundary-adjusted cut point — compute it first, then truncate.
        let dropped = leveler_core::ceil_char_boundary(log, log.len() - MAX_LOG_BYTES);
        let marker = format!("…[truncated {dropped} bytes]…");
        *log = leveler_core::truncate_tail_bytes(log, MAX_LOG_BYTES, &marker);
        return Some(marker.len());
    }
    None
}

pub(crate) fn take_log_delta(
    log: &str,
    end: u64,
    prefix_len: usize,
    cursor: &mut u64,
    max_bytes: usize,
) -> Result<(String, u64), String> {
    let retained = &log[prefix_len..];
    let retained_start = end.saturating_sub(retained.len() as u64);
    let gap = retained_start.saturating_sub(*cursor);
    let start = (*cursor).max(retained_start);
    let offset = (start - retained_start) as usize;
    if !retained.is_char_boundary(offset) {
        return Err(format!("log cursor {} splits a UTF-8 character", *cursor));
    }
    if max_bytes < 4 && offset < retained.len() {
        return Err("log output budget is too small".into());
    }
    let limit = leveler_core::floor_char_boundary(
        retained,
        offset.saturating_add(max_bytes).min(retained.len()),
    );
    let delta = retained[offset..limit].to_string();
    *cursor = start + delta.len() as u64;
    Ok((delta, gap))
}

fn snapshot(task: &TaskInner) -> BackgroundTaskSnapshot {
    snapshot_with_log(task, task.log.clone())
}

fn snapshot_with_log(task: &TaskInner, log: String) -> BackgroundTaskSnapshot {
    let duration_ms = task
        .finished
        .unwrap_or_else(Instant::now)
        .duration_since(task.started)
        .as_millis() as u64;
    BackgroundTaskSnapshot {
        id: task.id.clone(),
        program: task.program.clone(),
        args: task.args.clone(),
        cwd: task.cwd.clone(),
        status: task.status,
        exit_code: task.exit_code,
        log,
        duration_ms,
        pid: task.pid,
        owner_scope: task.owner_scope.clone(),
    }
}

/// The pid a detail view shows for a task: the process-group leader on unix
/// (every spawn uses `process_group(0)`, so the leader is the child) and the
/// direct pid on Windows.
fn process_identity_pid(identity: &ProcessIdentity) -> u32 {
    #[cfg(unix)]
    {
        identity.pgid() as u32
    }
    #[cfg(not(unix))]
    {
        identity.pid()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::ProcessRequest;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use crate::command::prepare_sandbox_paths;

    /// A registry whose runner sees the real host environment. The default
    /// `BackgroundTaskRegistry::new()` reads the installed process snapshot,
    /// which a unit test never installs — so a confined command would resolve
    /// no home, no PATH and no temp directory.
    pub(super) fn host_registry() -> BackgroundTaskRegistry {
        BackgroundTaskRegistry::with_environment(Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn abort_during_registration_releases_capacity_and_kills_the_child() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        let reg = host_registry();
        let hook = Arc::new(SpawnRegistrationHook::default());
        *reg.spawn_registration_hook.lock().expect("hook lock") = Some(hook.clone());

        let spawn = tokio::spawn({
            let reg = reg.clone();
            let request = ProcessRequest::new(
                "sh",
                vec![
                    "-c".into(),
                    format!("echo $$ > '{}'; exec sleep 30", pid_file.display()),
                ],
                dir.path().to_path_buf(),
            );
            async move { reg.spawn(request, None).await }
        });

        hook.reached.notified().await;
        let pid = wait_for_pid_file(&pid_file, Duration::from_secs(5))
            .await
            .expect("spawned child pid");
        spawn.abort();
        assert!(
            spawn
                .await
                .expect_err("spawn future must be cancelled")
                .is_cancelled()
        );
        assert_eq!(
            reg.pending_spawns.load(Ordering::Acquire),
            0,
            "a dropped reservation must release its capacity slot"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while process_alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !process_alive(pid),
            "an aborted, not-yet-registered child must not escape the registry"
        );
        assert!(reg.active_ids_for_scope("unused").await.is_empty());

        *reg.spawn_registration_hook.lock().expect("hook lock") = None;
        let id = reg
            .spawn(
                ProcessRequest::new("true", Vec::new(), dir.path().to_path_buf()),
                None,
            )
            .await
            .expect("released capacity can be reused");
        let finished = reg
            .wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .expect("replacement task finishes");
        assert_eq!(finished.exit_code, Some(0));
    }

    #[tokio::test]
    async fn wait_interval_returns_running_snapshot_without_killing() {
        let dir = tempfile::tempdir().expect("tempdir");
        #[cfg(unix)]
        let pid_file = dir.path().join("pid");
        #[cfg(unix)]
        let (program, args) = (
            "sh",
            vec![
                "-c".into(),
                format!("echo $$ > '{}'; exec sleep 30", pid_file.display()),
            ],
        );
        #[cfg(windows)]
        let (program, args) = ("ping", vec!["-n".into(), "30".into(), "127.0.0.1".into()]);
        let reg = BackgroundTaskRegistry::new();
        let req = ProcessRequest::new(program, args, dir.path().to_path_buf());
        let id = reg.spawn(req, None).await.expect("spawn");
        let snap = reg
            .wait(
                &id,
                Some(Duration::from_millis(200)),
                &CancellationToken::new(),
            )
            .await
            .expect("wait interval should return a snapshot, not an error");
        assert_eq!(
            snap.status,
            BackgroundTaskStatus::Running,
            "interval expiry is not terminal: {:?}",
            snap.status
        );
        assert!(snap.exit_code.is_none());
        let still = reg.get(&id).await.expect("task retained");
        assert_eq!(still.status, BackgroundTaskStatus::Running);
        #[cfg(unix)]
        {
            let pid = wait_for_pid_file(&pid_file, Duration::from_secs(5))
                .await
                .expect("pid file");
            assert!(
                process_alive(pid),
                "wait interval must not kill background pid {pid}"
            );
        }
        let _ = reg.kill(&id).await;
    }

    #[tokio::test]
    async fn wait_cancel_returns_without_killing() {
        let reg = BackgroundTaskRegistry::new();
        let req = ProcessRequest::new("sleep", vec!["30".into()], std::env::temp_dir());
        let id = reg.spawn(req, None).await.expect("spawn");
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let wait = tokio::spawn({
            let reg = reg.clone();
            let id = id.clone();
            async move {
                reg.wait(&id, Some(Duration::from_secs(10)), &cancel_clone)
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let err = wait
            .await
            .expect("join")
            .expect_err("cancelled wait is an error");
        assert!(
            err.contains("cancelled"),
            "expected wait cancelled, got {err}"
        );
        let snap = reg.get(&id).await.expect("still listed");
        assert!(
            snap.status.is_active(),
            "cancel waits; it does not kill: {:?}",
            snap.status
        );
        let _ = reg.kill(&id).await;
    }

    #[tokio::test]
    async fn kill_wakes_a_pending_wait() {
        let reg = BackgroundTaskRegistry::new();
        let req = ProcessRequest::new("sleep", vec!["30".into()], std::env::temp_dir());
        let id = reg.spawn(req, None).await.expect("spawn");
        let wait = tokio::spawn({
            let reg = reg.clone();
            let id = id.clone();
            async move {
                reg.wait(
                    &id,
                    Some(Duration::from_secs(10)),
                    &CancellationToken::new(),
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = reg.kill(&id).await;
        let snap = tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("wait must wake after kill")
            .expect("join")
            .expect("wait after kill");
        assert_eq!(snap.status, BackgroundTaskStatus::Killed);
    }

    #[tokio::test]
    async fn spawn_wait_echo() {
        let reg = BackgroundTaskRegistry::new();
        #[cfg(unix)]
        let echo = "/bin/echo";
        #[cfg(windows)]
        let echo = "cmd";
        #[cfg(unix)]
        let args = vec!["hello-bg".into()];
        #[cfg(windows)]
        let args = vec!["/C".into(), "echo hello-bg".into()];
        let req = ProcessRequest::new(echo, args, std::env::temp_dir());
        let id = reg.spawn(req, None).await.expect("spawn");
        let snap = reg
            .wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .expect("wait");
        assert_eq!(snap.status, BackgroundTaskStatus::Exited);
        assert_eq!(snap.exit_code, Some(0));
        assert!(
            snap.log.contains("hello-bg"),
            "log should capture stdout: {}",
            snap.log
        );
    }

    #[test]
    fn log_truncation_preserves_utf8_boundaries() {
        let mut log = "🙂".repeat(MAX_LOG_BYTES / 4 + 2);
        truncate_log(&mut log);
        assert!(log.is_char_boundary(log.len()));
        assert!(log.contains('🙂'));
    }

    #[test]
    fn wait_delta_reports_gap_after_log_truncation() {
        let mut cursor = 0;
        let marker = "…[truncated]…";
        let retained = "🙂new output\n";
        let log = format!("{marker}{retained}");
        let end = 100 + retained.len() as u64;
        let (first, gap) = take_log_delta(&log, end, marker.len(), &mut cursor, 1024).unwrap();
        assert_eq!(gap, 100);
        assert_eq!(first, retained);
        assert_eq!(cursor, end);
        let (second, gap) = take_log_delta(&log, end, marker.len(), &mut cursor, 1024).unwrap();
        assert_eq!(gap, 0);
        assert!(second.is_empty());
    }

    #[test]
    fn wait_delta_keeps_undelivered_bytes_for_next_call() {
        let mut cursor = 0;
        let log = "🙂abcdef";
        let end = log.len() as u64;
        let (first, gap) = take_log_delta(log, end, 0, &mut cursor, 5).unwrap();
        assert_eq!(first, "🙂a");
        assert_eq!(gap, 0);
        assert_eq!(cursor, 5);
        let (second, gap) = take_log_delta(log, end, 0, &mut cursor, 5).unwrap();
        assert_eq!(second, "bcdef");
        assert_eq!(gap, 0);
        assert_eq!(cursor, end);
    }

    #[test]
    fn wait_delta_rejects_cursor_inside_utf8_character() {
        let mut cursor = 1;
        let error = take_log_delta("🙂", 4, 0, &mut cursor, 1024).unwrap_err();
        assert!(error.contains("UTF-8"));
        assert_eq!(cursor, 1);
    }

    #[test]
    fn terminal_history_is_bounded_without_evicting_active_tasks() {
        let now = Instant::now();
        let mut state = RegistryState::default();
        for index in 0..(MAX_RETAINED_TERMINAL_TASKS + 3) {
            let id = format!("terminal-{index:03}");
            // Build ascending instants by ADDING to `now` (higher index = more
            // recent), never subtracting a large Duration from Instant::now():
            // on Windows Instant is QPC-from-boot, so `now - 1000s` underflows
            // the monotonic epoch on a low-uptime runner and panics. prune only
            // needs relative ordering, which addition preserves identically.
            let finished = now + Duration::from_secs(index as u64);
            state.tasks.insert(
                id.clone(),
                TaskInner {
                    id,
                    owner_scope: None,
                    writer_scope: "parent".into(),
                    write_scope: crate::WriteScope::Unrestricted,
                    lifetime: BackgroundTaskLifetime::Goal,
                    program: "true".into(),
                    args: Vec::new(),
                    cwd: PathBuf::new(),
                    status: BackgroundTaskStatus::Exited,
                    exit_code: Some(0),
                    pid: None,
                    log: String::new(),
                    log_end: 0,
                    log_prefix_len: 0,
                    default_log_cursor: 0,
                    started: finished,
                    finished: Some(finished),
                    child: None,
                    identity: None,
                    done: Arc::new(Notify::new()),
                    changed: Arc::new(Notify::new()),
                    process_done: true,
                    log_pumps_remaining: 0,
                    group_reaped: false,
                    mutation_baseline: None,
                    settlement: None,
                    settlement_reported: false,
                    sandbox_scratch: None,
                },
            );
        }
        for (id, status) in [
            ("still-running", BackgroundTaskStatus::Running),
            ("being-killed", BackgroundTaskStatus::Killing),
        ] {
            state.tasks.insert(
                id.into(),
                TaskInner {
                    id: id.into(),
                    owner_scope: None,
                    writer_scope: "parent".into(),
                    write_scope: crate::WriteScope::Unrestricted,
                    lifetime: BackgroundTaskLifetime::Goal,
                    program: "sleep".into(),
                    args: Vec::new(),
                    cwd: PathBuf::new(),
                    status,
                    exit_code: None,
                    pid: None,
                    log: String::new(),
                    log_end: 0,
                    log_prefix_len: 0,
                    default_log_cursor: 0,
                    started: now,
                    finished: None,
                    child: None,
                    identity: None,
                    done: Arc::new(Notify::new()),
                    changed: Arc::new(Notify::new()),
                    process_done: false,
                    log_pumps_remaining: 0,
                    group_reaped: false,
                    mutation_baseline: None,
                    settlement: None,
                    settlement_reported: false,
                    sandbox_scratch: None,
                },
            );
        }

        prune_terminal_tasks(&mut state);

        assert_eq!(
            state
                .tasks
                .values()
                .filter(|task| task.status.is_terminal())
                .count(),
            MAX_RETAINED_TERMINAL_TASKS
        );
        assert!(!state.tasks.contains_key("terminal-000"));
        assert!(state.tasks.contains_key("terminal-066"));
        assert!(state.tasks.contains_key("still-running"));
        assert!(state.tasks.contains_key("being-killed"));
    }

    #[test]
    fn finalization_releases_private_scratch_immediately() {
        let scratch = tempfile::tempdir().expect("scratch");
        let scratch_path = scratch.path().to_path_buf();
        let mut task = TaskInner {
            id: "done".into(),
            owner_scope: None,
            writer_scope: "parent".into(),
            write_scope: crate::WriteScope::Unrestricted,
            lifetime: BackgroundTaskLifetime::Goal,
            program: "true".into(),
            args: Vec::new(),
            cwd: PathBuf::new(),
            status: BackgroundTaskStatus::Running,
            exit_code: Some(0),
            pid: None,
            log: String::new(),
            log_end: 0,
            log_prefix_len: 0,
            default_log_cursor: 0,
            started: Instant::now(),
            finished: None,
            child: None,
            identity: None,
            done: Arc::new(Notify::new()),
            changed: Arc::new(Notify::new()),
            process_done: true,
            log_pumps_remaining: 0,
            group_reaped: true,
            mutation_baseline: None,
            settlement: None,
            settlement_reported: false,
            sandbox_scratch: Some(crate::command::SandboxScratch::unleased(scratch)),
        };

        finalize_if_drained(&mut task);

        assert_eq!(task.status, BackgroundTaskStatus::Exited);
        assert!(task.sandbox_scratch.is_none());
        assert!(!scratch_path.exists());
    }

    #[tokio::test]
    async fn killing_a_reaped_group_waits_for_settlement_without_signalling() {
        let task = TaskInner {
            id: "done".into(),
            owner_scope: Some("owner".into()),
            writer_scope: "parent".into(),
            write_scope: crate::WriteScope::Unrestricted,
            lifetime: BackgroundTaskLifetime::Goal,
            program: "true".into(),
            args: Vec::new(),
            cwd: PathBuf::new(),
            status: BackgroundTaskStatus::Running,
            exit_code: Some(0),
            pid: None,
            log: String::new(),
            log_end: 0,
            log_prefix_len: 0,
            default_log_cursor: 0,
            started: Instant::now(),
            finished: None,
            child: None,
            identity: None,
            done: Arc::new(Notify::new()),
            changed: Arc::new(Notify::new()),
            process_done: true,
            log_pumps_remaining: 0,
            group_reaped: false,
            mutation_baseline: None,
            settlement: None,
            settlement_reported: false,
            sandbox_scratch: None,
        };

        let registry = BackgroundTaskRegistry::new();
        registry
            .inner
            .lock()
            .await
            .tasks
            .insert("done".into(), task);
        let state = registry
            .kill_owned("done", "owner")
            .await
            .expect("already reaped is a valid settling state");
        assert_eq!(state.status, BackgroundTaskStatus::Running);
        let state = registry
            .wait_owned(
                "done",
                "owner",
                Some(Duration::ZERO),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !state.status.is_terminal(),
            "only settlement can publish terminal"
        );
        {
            let mut state = registry.inner.lock().await;
            let task = state.tasks.get_mut("done").unwrap();
            task.group_reaped = true;
            assert!(finalize_if_drained(task));
        }
        assert!(
            registry
                .get_owned("done", "owner")
                .await
                .unwrap()
                .status
                .is_terminal()
        );
    }

    #[tokio::test]
    async fn writer_settlement_keeps_other_writers_alive_and_releases_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let reg = host_registry();
        let mut request = ProcessRequest::new("sleep", vec!["30".into()], dir.path().to_path_buf());
        request.write_scope = crate::WriteScope::Workspace {
            root: dir.path().to_path_buf(),
        };
        let child = reg
            .spawn_for_writer(
                request.clone(),
                None,
                Some("session"),
                "child",
                BackgroundTaskLifetime::Goal,
            )
            .await
            .unwrap();
        let peer = reg
            .spawn_for_writer(
                request,
                None,
                Some("session"),
                "peer",
                BackgroundTaskLifetime::Goal,
            )
            .await
            .unwrap();
        assert_eq!(
            reg.write_conflicts("session", &["src".into()]).await.len(),
            2
        );
        assert_eq!(
            reg.foreign_write_paths("session", "parent").await,
            vec!["."]
        );
        assert!(
            reg.foreign_write_paths("other-session", "parent")
                .await
                .is_empty()
        );
        reg.settle_writer("session", "child").await.unwrap();
        assert!(reg.get(&child).await.unwrap().status.is_terminal());
        assert_eq!(
            reg.get(&peer).await.unwrap().status,
            BackgroundTaskStatus::Running
        );
        assert_eq!(
            reg.write_conflicts("session", &["src".into()]).await,
            vec![peer.clone()]
        );
        reg.settle_writer("session", "peer").await.unwrap();
        assert!(
            reg.foreign_write_paths("session", "parent")
                .await
                .is_empty()
        );
        assert!(
            reg.write_conflicts("session", &["src".into()])
                .await
                .is_empty()
        );
    }

    /// The baseline is consumed by the reaper, so a task is diffed and
    /// accounted at most once however many times it is inspected afterwards.
    #[tokio::test]
    async fn a_task_is_settled_at_most_once() {
        use crate::snapshot::WorkspaceSnapshot;

        // git repo so we can capture a real SnapshotId
        let dir = leveler_test_support::git::scratch_repo();
        std::fs::write(dir.path().join("a"), "x\n").expect("seed file");
        leveler_test_support::git::run(dir.path(), &["add", "-A"]);
        leveler_test_support::git::run(dir.path(), &["commit", "-qm", "i"]);
        let snap = WorkspaceSnapshot::capture(dir.path())
            .await
            .expect("capture")
            .expect("git repo");
        let baseline = MutationBaseline {
            snapshot: snap,
            workspace_root: dir.path().to_path_buf(),
            write_scope: crate::WriteScope::Unrestricted,
        };

        let reg = BackgroundTaskRegistry::new();
        let req = ProcessRequest::new("echo", vec!["ok".into()], dir.path().to_path_buf());
        let id = reg.spawn(req, Some(baseline)).await.expect("spawn");
        let _ = reg
            .wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await;
        let first = reg.take_settlement(&id).await.expect("settled once");
        assert!(first.modified.is_empty(), "{:?}", first.modified);
        assert!(first.violation.is_none(), "{:?}", first.violation);
        assert!(
            reg.take_settlement(&id).await.is_none(),
            "a settlement is reported exactly once"
        );
    }

    /// Terminal state implies completed accounting even without wait_task.
    #[tokio::test]
    async fn a_background_task_is_settled_on_exit_without_anyone_waiting() {
        use crate::snapshot::WorkspaceSnapshot;

        let dir = leveler_test_support::git::scratch_repo();
        std::fs::create_dir_all(dir.path().join("allowed")).expect("mkdir");
        std::fs::write(dir.path().join("allowed/keep"), "keep\n").expect("seed");
        std::fs::write(dir.path().join("protected"), "original\n").expect("seed");
        leveler_test_support::git::run(dir.path(), &["add", "-A"]);
        leveler_test_support::git::run(dir.path(), &["commit", "-qm", "i"]);
        let scope = crate::WriteScope::ScopedWorkspace {
            root: dir.path().to_path_buf(),
            allowed: vec![dir.path().join("allowed")],
            excluded: Vec::new(),
        };
        let snapshot = WorkspaceSnapshot::capture_for_scope(dir.path(), &scope)
            .await
            .expect("capture")
            .expect("git repo");

        let reg = host_registry();
        let mut request = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "echo tampered > protected".into()],
            dir.path().to_path_buf(),
        );
        request.write_scope = scope.clone();
        let id = reg
            .spawn(
                request,
                Some(MutationBaseline {
                    snapshot,
                    workspace_root: dir.path().to_path_buf(),
                    write_scope: scope,
                }),
            )
            .await
            .expect("spawn");

        // Nobody calls wait_task: this only observes the terminal state. The
        // reaper stores the settlement in the same lock acquisition that marks
        // the process done, so a terminal status already implies a settlement.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !reg
            .get(&id)
            .await
            .is_some_and(|snap| snap.status.is_terminal())
        {
            assert!(
                Instant::now() < deadline,
                "task never reached a terminal state"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let settlement = reg.take_settlement(&id).await.expect("settlement recorded");
        assert!(settlement.modified.is_empty(), "{settlement:?}");
        assert_ne!(reg.get(&id).await.unwrap().exit_code, Some(0));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("protected")).expect("read"),
            "original\n",
            "the sandbox must prevent the unauthorized write"
        );
        assert!(
            reg.take_settlement(&id).await.is_none(),
            "a settlement is reported once"
        );
    }

    /// R004 F7 / T7: session-owned tasks are reaped as a group; tasks owned
    /// by another scope (or daemon-scoped) are untouched by kill_scope.
    #[tokio::test]
    async fn kill_scope_reaps_only_the_owning_session() {
        let reg = BackgroundTaskRegistry::new();
        let mk = || ProcessRequest::new("sleep", vec!["30".into()], std::env::temp_dir());
        let owned = reg
            .spawn_owned(mk(), None, Some("session-a"))
            .await
            .expect("spawn owned");
        let other = reg
            .spawn_owned(mk(), None, Some("session-b"))
            .await
            .expect("spawn other");
        let daemon = reg.spawn(mk(), None).await.expect("spawn daemon-scoped");

        let n = reg.kill_scope("session-a").await;
        assert_eq!(n, 1, "exactly the session-a task is reaped");
        let snap = reg
            .wait(
                &owned,
                Some(Duration::from_secs(5)),
                &CancellationToken::new(),
            )
            .await
            .expect("wait owned");
        assert_eq!(snap.status, BackgroundTaskStatus::Killed);
        // The other two are still running (then cleaned up).
        for id in [&other, &daemon] {
            let snap = reg.get(id).await.expect("get");
            assert!(
                snap.status.is_active(),
                "{id} must survive: {:?}",
                snap.status
            );
            let _ = reg.kill(id).await;
        }
    }

    #[tokio::test]
    async fn active_snapshots_for_scope_excludes_terminal_and_other_owners() {
        let reg = BackgroundTaskRegistry::new();
        let mk = || ProcessRequest::new("sleep", vec!["30".into()], std::env::temp_dir());
        let active = reg
            .spawn_owned(mk(), None, Some("session-a"))
            .await
            .expect("spawn active");
        let terminal = reg
            .spawn_owned(mk(), None, Some("session-a"))
            .await
            .expect("spawn terminal");
        let other = reg
            .spawn_owned(mk(), None, Some("session-b"))
            .await
            .expect("spawn other");
        reg.kill(&terminal).await.expect("kill terminal");
        reg.wait(
            &terminal,
            Some(Duration::from_secs(5)),
            &CancellationToken::new(),
        )
        .await
        .expect("terminal settles");

        let snapshots = reg.active_snapshots_for_scope("session-a").await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].id, active);
        assert!(snapshots[0].status.is_active());

        for id in [&active, &other] {
            let _ = reg.kill(id).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lagged_lifecycle_stream_can_reconcile_from_active_snapshots() {
        let reg = BackgroundTaskRegistry::new();
        let mut events = reg.subscribe();
        for _ in 0..130 {
            let id = reg
                .spawn_owned(
                    ProcessRequest::new("true", vec![], std::env::temp_dir()),
                    None,
                    Some("session-a"),
                )
                .await
                .expect("spawn");
            reg.wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
                .await
                .expect("settles");
        }

        assert!(matches!(
            events.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
        assert!(reg.active_snapshots_for_scope("session-a").await.is_empty());
    }

    /// R004 F7 / T7: kill_all reaps everything for daemon shutdown paths.
    #[tokio::test]
    async fn kill_all_reaps_every_scope() {
        let reg = BackgroundTaskRegistry::new();
        let mk = || ProcessRequest::new("sleep", vec!["30".into()], std::env::temp_dir());
        let a = reg.spawn_owned(mk(), None, Some("s1")).await.expect("a");
        let b = reg.spawn(mk(), None).await.expect("b");
        let n = reg.kill_all().await;
        assert_eq!(n, 2);
        for id in [&a, &b] {
            let snap = reg
                .wait(id, Some(Duration::from_secs(5)), &CancellationToken::new())
                .await
                .expect("wait");
            assert_eq!(snap.status, BackgroundTaskStatus::Killed);
        }
    }

    #[tokio::test]
    async fn kill_running_sleep() {
        let reg = BackgroundTaskRegistry::new();
        let req = ProcessRequest::new("sleep", vec!["30".into()], std::env::temp_dir());
        let id = reg.spawn(req, None).await.expect("spawn");
        let snap = reg.kill(&id).await.expect("kill");
        assert!(
            matches!(
                snap.status,
                BackgroundTaskStatus::Killing | BackgroundTaskStatus::Killed
            ),
            "kill should leave Killing or Killed, got {:?}",
            snap.status
        );
        let final_snap = reg
            .wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .expect("wait after kill");
        assert_eq!(final_snap.status, BackgroundTaskStatus::Killed);
    }

    /// Core PR-3a guarantee: after the wait reaper `take()`s `Child`, kill must
    /// still terminate the process via recorded pid/pgid.
    /// Unix-only: pid/reaper semantics; Windows kills via Job Objects
    /// (windows_job_ tests) and MSYS shell pids are not Win32 pids.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_after_reaper_takes_child_terminates_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        let script = format!("echo $$ > '{}'; exec sleep 60", pid_file.display());
        let reg = BackgroundTaskRegistry::new();
        let req = ProcessRequest::new("sh", vec!["-c".into(), script], dir.path().to_path_buf());
        let id = reg.spawn(req, None).await.expect("spawn");

        let pid = wait_for_pid_file(&pid_file, Duration::from_secs(5))
            .await
            .expect("pid file");
        // Strict precondition: reaper has taken Child (not just a fixed sleep).
        wait_until_child_taken(&reg, &id, Duration::from_secs(5))
            .await
            .expect("reaper should take Child");

        assert!(
            process_alive(pid),
            "precondition: sleep child pid {pid} should be alive"
        );

        let snap = reg.kill(&id).await.expect("kill");
        assert!(
            matches!(
                snap.status,
                BackgroundTaskStatus::Killing | BackgroundTaskStatus::Killed
            ),
            "unexpected status {:?}",
            snap.status
        );

        let final_snap = reg
            .wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .expect("wait after kill");
        assert_eq!(final_snap.status, BackgroundTaskStatus::Killed);

        for _ in 0..50 {
            if !process_alive(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !process_alive(pid),
            "kill must terminate sleep child pid {pid} even after Child was taken"
        );
    }

    /// Session/registry drop must reap Running processes without an explicit kill
    /// (KillOnDrop must not be kept alive by reaper tasks).
    /// Unix-only for the same pid-namespace reason as above.
    #[cfg(unix)]
    #[tokio::test]
    async fn registry_drop_reaps_running_sleep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        let script = format!("echo $$ > '{}'; exec sleep 60", pid_file.display());
        let reg = BackgroundTaskRegistry::new();
        let req = ProcessRequest::new("sh", vec!["-c".into(), script], dir.path().to_path_buf());
        let _id = reg.spawn(req, None).await.expect("spawn");
        let pid = wait_for_pid_file(&pid_file, Duration::from_secs(5))
            .await
            .expect("pid file");
        assert!(
            process_alive(pid),
            "precondition: sleep child pid {pid} should be alive"
        );

        drop(reg);

        for _ in 0..50 {
            if !process_alive(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !process_alive(pid),
            "dropping the registry handle must reap sleep pid {pid}"
        );
    }

    /// Complement to `registry_drop_reaps_running_sleep`: a per-turn engine holds
    /// a *clone* of the process-lived registry, so dropping that clone at turn end
    /// must NOT reap background processes — only the last handle (process exit)
    /// does. This is the invariant the app-level registry hoist relies on so
    /// `background=true` servers survive across turns.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_registry_clone_keeps_tasks_alive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        let script = format!("echo $$ > '{}'; exec sleep 60", pid_file.display());
        let reg = BackgroundTaskRegistry::new();
        let req = ProcessRequest::new("sh", vec!["-c".into(), script], dir.path().to_path_buf());
        let id = reg.spawn(req, None).await.expect("spawn");
        let pid = wait_for_pid_file(&pid_file, Duration::from_secs(5))
            .await
            .expect("pid file");

        // Simulate a turn ending: the engine's registry clone goes out of scope.
        let clone = reg.clone();
        drop(clone);

        // The process must stay alive and the task must remain queryable on the
        // surviving handle across a window that comfortably exceeds reap timing.
        for _ in 0..15 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(
                process_alive(pid),
                "a clone drop must not reap background pid {pid}"
            );
        }
        assert!(
            reg.get(&id).await.is_some(),
            "surviving handle must still know the task id after a clone drop"
        );

        drop(reg); // last handle: reaping is allowed now.
    }

    #[tokio::test]
    async fn spawn_honors_sandbox_fields_on_process_request() {
        // Smoke: confined ProcessRequest spawns and produces stdout (wrap path
        // does not refuse a normal confined command). OS confinement canaries
        // are `background_confined_blocks_write_outside_workspace` below and
        // `windows_confine::windows_canaries`.
        let reg = host_registry();
        let ws = tempfile::tempdir().expect("ws");
        let (program, args) = leveler_test_support::echo_command("sandboxed-bg");
        let mut req = ProcessRequest::new(program, args, ws.path().to_path_buf());
        // Ask for a network deny only where the host can actually enforce one.
        // Windows cannot outside AppContainer, and a request for one there is
        // refused rather than run open — asserted in `windows_sandbox`.
        req.deny_network = crate::windows_sandbox::probe_sandbox_capabilities().network_deny;
        req.write_scope = crate::WriteScope::Workspace {
            root: ws.path().to_path_buf(),
        };
        let id = reg.spawn(req, None).await.expect("spawn confined");
        let snap = reg
            .wait(
                &id,
                Some(Duration::from_secs(10)),
                &CancellationToken::new(),
            )
            .await
            .expect("wait");
        assert_eq!(snap.status, BackgroundTaskStatus::Exited);
        assert_eq!(snap.exit_code, Some(0));
        assert!(
            snap.log.contains("sandboxed-bg"),
            "sandboxed background echo should produce log: {}",
            snap.log
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retained_log_identifies_stdout_and_stderr() {
        let reg = BackgroundTaskRegistry::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let req = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "printf out; printf err >&2".into()],
            dir.path().to_path_buf(),
        );
        let id = reg.spawn(req, None).await.expect("spawn");
        let snap = reg
            .wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .expect("wait");

        assert!(snap.log.contains("[stdout] out"), "log: {}", snap.log);
        assert!(snap.log.contains("[stderr] err"), "log: {}", snap.log);
    }

    /// Four concurrently spawned tasks must keep their own stdout/stderr and
    /// their own pid: the live `Output` stream is tagged by task id, so a
    /// viewer (and a `/btw` reader) can never attribute one task's line to
    /// another. Also covers the mechanical fact the detail pane shows (pid).
    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_tasks_keep_output_and_pid_separate() {
        let reg = host_registry();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut rx = reg.subscribe();
        let mut ids = Vec::new();
        for i in 0..4 {
            let script = format!("printf 'out-{i}-{i}-{i}\\n'; printf 'err-{i}-{i}-{i}\\n' >&2");
            let req =
                ProcessRequest::new("sh", vec!["-c".into(), script], dir.path().to_path_buf());
            let id = reg.spawn(req, None).await.expect("spawn");
            let snap = reg.get(&id).await.expect("live snapshot");
            assert!(
                snap.pid.is_some_and(|pid| pid > 0),
                "task {id} must expose its pid"
            );
            ids.push(id);
        }

        // Drain the lifecycle stream until every task has exited terminally.
        let mut chunks: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut exited = std::collections::HashSet::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while exited.len() < ids.len() && Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(BackgroundTaskEvent::Output { task_id, chunk, .. })) => {
                    chunks.entry(task_id).or_default().push_str(&chunk);
                }
                Ok(Ok(BackgroundTaskEvent::Exited { task, .. })) => {
                    exited.insert(task.id);
                }
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => break,
            }
        }

        for (i, id) in ids.iter().enumerate() {
            let seen = chunks.get(id).cloned().unwrap_or_default();
            assert!(
                seen.contains(&format!("out-{i}-{i}-{i}")),
                "task {id} must see its own stdout, got {seen:?}"
            );
            assert!(
                seen.contains(&format!("err-{i}-{i}-{i}")),
                "task {id} must see its own stderr, got {seen:?}"
            );
            for other in 0..4 {
                if other != i {
                    let foreign = format!("out-{other}-{other}-{other}");
                    assert!(
                        !seen.contains(&foreign),
                        "task {id} leaked another task's line {foreign}: {seen:?}"
                    );
                }
            }
        }
    }

    /// A chatty process must not grow the registry's retained log without
    /// bound: the cap holds, UTF-8 stays valid, and the truncation is explicit.
    #[cfg(unix)]
    #[tokio::test]
    async fn high_volume_output_stays_bounded() {
        let reg = host_registry();
        let dir = tempfile::tempdir().expect("tempdir");
        // ~12k lines, far past the 256 KiB cap.
        let script = "i=0; while [ $i -lt 12000 ]; do printf 'line-%05d-xxxxxxxxxxxxxxxxxxxxxxxx\\n' $i; i=$((i+1)); done";
        let req = ProcessRequest::new(
            "sh",
            vec!["-c".into(), script.to_string()],
            dir.path().to_path_buf(),
        );
        let id = reg.spawn(req, None).await.expect("spawn");
        let snap = reg
            .wait(
                &id,
                Some(Duration::from_secs(30)),
                &CancellationToken::new(),
            )
            .await
            .expect("wait");
        assert!(
            snap.log.len() <= MAX_LOG_BYTES + 64,
            "retained log must stay bounded, was {} bytes",
            snap.log.len()
        );
        assert!(
            snap.log.contains("truncated"),
            "early output must be explicitly marked as dropped"
        );
        assert!(snap.log.is_char_boundary(snap.log.len()));
        // The newest output is what a live reader needs, so the tail survives.
        assert!(
            snap.log.contains("line-11999"),
            "the retained window must keep the newest lines"
        );
    }

    /// Real OS confinement canary for background spawn (mirrors foreground
    /// seatbelt/bwrap write-outside tests).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn background_confined_blocks_write_outside_workspace() {
        #[cfg(target_os = "linux")]
        {
            if std::process::Command::new("bwrap")
                .arg("--version")
                .output()
                .is_err()
            {
                eprintln!("skipping: bubblewrap is not installed");
                return;
            }
        }

        let home = std::path::PathBuf::from(std::env::var("HOME").expect("HOME set"));
        let base = home.join(format!(".leveler-bg-sbtest-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ws = ws.canonicalize().unwrap();

        let reg = host_registry();

        // Write inside workspace: allowed.
        let mut inside = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "echo hi > ok.txt".into()],
            ws.clone(),
        );
        inside.write_scope = crate::WriteScope::Workspace { root: ws.clone() };
        let id = reg.spawn(inside, None).await.expect("spawn inside");
        let snap = reg
            .wait(
                &id,
                Some(Duration::from_secs(10)),
                &CancellationToken::new(),
            )
            .await
            .expect("wait inside");
        assert_eq!(
            snap.exit_code,
            Some(0),
            "write inside workspace should succeed: {snap:?}"
        );
        assert!(ws.join("ok.txt").exists());

        // Write to a sibling under $HOME (outside writable roots): blocked.
        let escape = base.join("escape.txt");
        let _ = std::fs::remove_file(&escape);
        let mut outside = ProcessRequest::new(
            "sh",
            vec!["-c".into(), format!("echo x > {}", escape.display())],
            ws.clone(),
        );
        outside.write_scope = crate::WriteScope::Workspace { root: ws.clone() };
        let id = reg.spawn(outside, None).await.expect("spawn outside");
        let snap = reg
            .wait(
                &id,
                Some(Duration::from_secs(10)),
                &CancellationToken::new(),
            )
            .await
            .expect("wait outside");
        assert!(
            snap.exit_code != Some(0),
            "write outside workspace must be blocked: {snap:?}"
        );
        assert!(
            !escape.exists(),
            "escape file must not exist after confined background write"
        );

        // Cache contents are writable, but the trusted leaf itself must not be
        // unlinked and replaced by a background child.
        let prepared = prepare_sandbox_paths(reg.runner.environment(), &ws, false).unwrap();
        let registry_root = prepared.tool_cache_path().join("cargo/registry");
        drop(prepared);
        let sentinel_dir = base.join("cache-escape");
        std::fs::create_dir_all(&sentinel_dir).unwrap();
        std::fs::write(sentinel_dir.join("sentinel"), "unchanged").unwrap();
        let script = "target=$(readlink \"$CARGO_HOME/registry\")\nrm -rf \"$target\" || exit 91\nln -s \"$1\" \"$target\"";
        let mut poison = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                script.into(),
                "sh".into(),
                sentinel_dir.display().to_string(),
            ],
            ws.clone(),
        );
        poison.write_scope = crate::WriteScope::Workspace { root: ws.clone() };
        let id = reg.spawn(poison, None).await.expect("spawn cache poison");
        let snap = reg
            .wait(
                &id,
                Some(Duration::from_secs(10)),
                &CancellationToken::new(),
            )
            .await
            .expect("wait cache poison");
        assert_ne!(snap.exit_code, Some(0), "cache leaf replacement: {snap:?}");
        assert!(registry_root.is_dir());
        assert!(
            !std::fs::symlink_metadata(&registry_root)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(sentinel_dir.join("sentinel")).unwrap(),
            "unchanged"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// CASE A + CASE C: a task whose direct child exits while a descendant
    /// keeps the log pipes open is still the owner of that descendant's
    /// process group. It must stay non-terminal, stay stoppable after the
    /// `Child` handle is gone, and a kill must terminate the whole group.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_descendant_outliving_the_direct_child_keeps_the_task_stoppable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let child_pid_file = dir.path().join("child.pid");
        let desc_pid_file = dir.path().join("desc.pid");
        let reg = host_registry();
        // `sh` exits at once; the backgrounded `sleep` inherits the child's
        // stdout/stderr (so the pumps never see EOF) and its process group.
        let req = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                format!(
                    "echo $$ > '{}'; sleep 30 & echo $! > '{}'",
                    child_pid_file.display(),
                    desc_pid_file.display()
                ),
            ],
            dir.path().to_path_buf(),
        );
        let id = reg.spawn(req, None).await.expect("spawn");
        let child = wait_for_pid_file(&child_pid_file, Duration::from_secs(5))
            .await
            .expect("direct child pid");
        let descendant = wait_for_pid_file(&desc_pid_file, Duration::from_secs(5))
            .await
            .expect("descendant pid");

        wait_until_child_taken(&reg, &id, Duration::from_secs(5))
            .await
            .expect("reaper must hold the Child handle");
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_alive(child) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!process_alive(child), "the direct child must have exited");
        assert!(
            process_alive(descendant),
            "the descendant must still be running"
        );

        let snap = reg.get(&id).await.expect("retained");
        assert!(
            snap.status.is_active(),
            "the task still owns the live descendant: {:?}",
            snap.status
        );
        assert_eq!(reg.alive_count().await, 1);

        // This is the defect: with the identity dropped at child reap, kill
        // failed with "no process identity to signal".
        reg.kill(&id)
            .await
            .expect("kill must reach the retained process group");

        let deadline = Instant::now() + Duration::from_secs(5);
        while reg.get(&id).await.is_some_and(|s| s.status.is_active()) && Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let final_snap = reg.get(&id).await.expect("retained");
        assert_eq!(final_snap.status, BackgroundTaskStatus::Killed);
        assert!(
            !process_alive(descendant),
            "kill must terminate the descendant too"
        );
        assert_eq!(
            reg.alive_count().await,
            0,
            "no ghost Running task may remain"
        );
    }

    /// CASE B + CASE E (group gone first): a descendant that ends on its own
    /// terminates the task without a kill — even when it redirected its own
    /// output, so no pipe EOF ever signalled its end.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_descendant_that_exits_naturally_finishes_the_task() {
        let dir = tempfile::tempdir().expect("tempdir");
        let desc_pid_file = dir.path().join("desc.pid");
        let reg = host_registry();
        // The descendant redirects its own stdio, so the task's pipes EOF as
        // soon as `sh` exits; only the process group still proves it lives.
        let req = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                format!(
                    "sleep 1 >/dev/null 2>&1 & echo $! > '{}'",
                    desc_pid_file.display()
                ),
            ],
            dir.path().to_path_buf(),
        );
        let id = reg.spawn(req, None).await.expect("spawn");
        let descendant = wait_for_pid_file(&desc_pid_file, Duration::from_secs(5))
            .await
            .expect("descendant pid");

        // The direct child is reaped and the pipes EOF well before `sleep 1`
        // ends; EOF must not be read as task liveness.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mid = reg.get(&id).await.expect("retained");
        assert!(
            mid.status.is_active(),
            "pipe EOF alone must not finish a task whose group is alive: {:?}",
            mid.status
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        while reg.get(&id).await.is_some_and(|s| s.status.is_active()) && Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let final_snap = reg.get(&id).await.expect("retained");
        assert_eq!(final_snap.status, BackgroundTaskStatus::Exited);
        assert!(
            !process_alive(descendant),
            "the descendant exited on its own"
        );
        assert_eq!(reg.alive_count().await, 0);
    }

    /// CASE D: an ordinary short command still finishes promptly and keeps its
    /// captured output — the group watcher and bounded final drain add no
    /// visible stall.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_short_command_finishes_promptly_with_its_output() {
        let reg = host_registry();
        let req = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "echo done-marker".into()],
            std::env::temp_dir(),
        );
        let id = reg.spawn(req, None).await.expect("spawn");
        let started = Instant::now();
        let snap = reg
            .wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .expect("wait");
        assert_eq!(snap.status, BackgroundTaskStatus::Exited);
        assert!(snap.log.contains("done-marker"), "log: {}", snap.log);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "a short task must not stall on the group/drain step: {:?}",
            started.elapsed()
        );
        assert_eq!(reg.alive_count().await, 0);
    }

    /// CASE F: a kill racing natural completion settles the task exactly once,
    /// with no panic and no ghost `Running` record.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_racing_natural_exit_is_idempotent() {
        for _ in 0..20 {
            let reg = host_registry();
            let id = reg
                .spawn(
                    ProcessRequest::new("true", Vec::new(), std::env::temp_dir()),
                    None,
                )
                .await
                .expect("spawn");
            // The kill may land before or after the reaper finalizes; both
            // must be safe and produce exactly one terminal.
            let _ = reg.kill(&id).await;
            let deadline = Instant::now() + Duration::from_secs(5);
            while reg.get(&id).await.is_some_and(|s| s.status.is_active())
                && Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let snap = reg.get(&id).await.expect("retained");
            assert!(
                snap.status.is_terminal(),
                "a raced kill must settle once: {:?}",
                snap.status
            );
            assert_eq!(reg.alive_count().await, 0);
        }
    }

    /// The read-only blocker list a retiring runtime reports is the SAME set
    /// `alive_count` counts, and it names the command.
    #[cfg(unix)]
    #[tokio::test]
    async fn active_snapshots_name_every_live_blocker() {
        let reg = host_registry();
        let id = reg
            .spawn(
                ProcessRequest::new("sleep", vec!["30".into()], std::env::temp_dir()),
                None,
            )
            .await
            .expect("spawn");
        let blockers = reg.active_snapshots().await;
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert_eq!(blockers[0].id, id);
        assert_eq!(blockers[0].program, "sleep");
        assert_eq!(reg.alive_count().await, blockers.len());
        let _ = reg.kill(&id).await;
    }

    #[cfg(unix)]
    async fn wait_for_pid_file(path: &std::path::Path, timeout: Duration) -> Option<u32> {
        let start = Instant::now();
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                return Some(pid);
            }
            if start.elapsed() > timeout {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[cfg(unix)]
    async fn wait_until_child_taken(
        reg: &BackgroundTaskRegistry,
        id: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let start = Instant::now();
        loop {
            match reg.child_taken_for_test(id).await {
                Some(true) => return Ok(()),
                Some(false) => {}
                None => return Err(format!("task `{id}` disappeared before Child take")),
            }
            if start.elapsed() > timeout {
                return Err(format!(
                    "timed out waiting for reaper to take Child of `{id}`"
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(unix)]
    fn process_alive(pid: u32) -> bool {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        // signal 0 = existence check
        kill(Pid::from_raw(pid as i32), None).is_ok()
    }
}

/// PR 4: the background registry spawns through the same `CommandRunner`
/// as the foreground, so one policy yields one result in both.
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod unified_runner_tests {
    use super::*;
    use crate::WriteScope;
    use crate::command::ProcessRequest;

    #[cfg(target_os = "linux")]
    fn bwrap_available() -> bool {
        std::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .is_ok()
    }
    #[cfg(target_os = "macos")]
    fn bwrap_available() -> bool {
        true
    }

    fn confined_request(ws: &std::path::Path, script: String) -> ProcessRequest {
        let mut req = ProcessRequest::new("sh", vec!["-c".into(), script], ws.to_path_buf());
        req.write_scope = WriteScope::Workspace {
            root: ws.to_path_buf(),
        };
        req
    }

    /// Same policy, foreground and background: an outside write is blocked
    /// the same way on both paths, and the workspace write succeeds on both.
    #[tokio::test]
    async fn foreground_and_background_enforce_the_same_write_scope() {
        if !bwrap_available() {
            eprintln!("skipping: bubblewrap is not installed");
            return;
        }
        let home = std::path::PathBuf::from(std::env::var("HOME").expect("HOME set"));
        let base = home.join(format!(".leveler-unified-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ws = ws.canonicalize().unwrap();
        let reg = super::tests::host_registry();
        let runner = reg.runner.clone();

        for (label, escape) in [
            ("fg", base.join("fg-escape.txt")),
            ("bg", base.join("bg-escape.txt")),
        ] {
            let _ = std::fs::remove_file(&escape);
            let inside = confined_request(&ws, format!("echo hi > {label}-ok.txt"));
            let outside = confined_request(&ws, format!("echo x > {}", escape.display()));
            let (inside_code, outside_code) = if label == "fg" {
                let a = runner.run(inside, CancellationToken::new()).await.unwrap();
                let b = runner.run(outside, CancellationToken::new()).await.unwrap();
                (a.exit_code, b.exit_code)
            } else {
                let a = reg.spawn(inside, None).await.unwrap();
                let a = reg
                    .wait(&a, Some(Duration::from_secs(10)), &CancellationToken::new())
                    .await
                    .unwrap();
                let b = reg.spawn(outside, None).await.unwrap();
                let b = reg
                    .wait(&b, Some(Duration::from_secs(10)), &CancellationToken::new())
                    .await
                    .unwrap();
                (a.exit_code, b.exit_code)
            };
            assert_eq!(
                inside_code,
                Some(0),
                "{label}: workspace write must succeed"
            );
            assert_ne!(
                outside_code,
                Some(0),
                "{label}: outside write must be blocked"
            );
            assert!(ws.join(format!("{label}-ok.txt")).exists(), "{label}");
            assert!(!escape.exists(), "{label}: escape file must not exist");
        }
        std::fs::remove_dir_all(&base).ok();
    }

    /// Network deny/allow, measured against a real local listener, through
    /// both paths.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn network_policy_is_enforced_the_same_way_fg_and_bg() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let reg = super::tests::host_registry();
        let runner = reg.runner.clone();
        let connect = format!("exec 3<>/dev/tcp/127.0.0.1/{port}");

        for deny in [true, false] {
            let mut fg = ProcessRequest::new(
                "bash",
                vec!["-c".into(), connect.clone()],
                dir.path().to_path_buf(),
            );
            fg.deny_network = deny;
            let bg = fg.clone();
            let fg_out = runner.run(fg, CancellationToken::new()).await.unwrap();
            let id = reg.spawn(bg, None).await.unwrap();
            let bg_out = reg
                .wait(
                    &id,
                    Some(Duration::from_secs(10)),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            if deny {
                assert_ne!(
                    fg_out.exit_code,
                    Some(0),
                    "fg: connect must fail under deny_network"
                );
                assert_ne!(
                    bg_out.exit_code,
                    Some(0),
                    "bg: connect must fail under deny_network"
                );
            } else {
                assert_eq!(
                    fg_out.exit_code,
                    Some(0),
                    "fg: connect must succeed: {fg_out:?}"
                );
                assert_eq!(
                    bg_out.exit_code,
                    Some(0),
                    "bg: connect must succeed: {bg_out:?}"
                );
            }
        }
        drop(listener);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn settlement_waits_for_descendant_writes() {
        let dir = leveler_test_support::git::scratch_repo();
        let snapshot = crate::WorkspaceSnapshot::capture(dir.path())
            .await
            .unwrap()
            .unwrap();
        let registry = super::tests::host_registry();
        let request = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                "(while [ ! -e release ]; do sleep 0.01; done; printf late > late.txt) &".into(),
            ],
            dir.path().to_path_buf(),
        );
        let id = registry
            .spawn(
                request,
                Some(MutationBaseline {
                    snapshot,
                    workspace_root: dir.path().to_path_buf(),
                    write_scope: crate::WriteScope::Unrestricted,
                }),
            )
            .await
            .unwrap();
        let before_release = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let st = registry.inner.lock().await;
                let task = st.tasks.get(&id).unwrap();
                if task.process_done {
                    break task.settlement.is_some();
                }
                drop(st);
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("direct child must exit");
        std::fs::write(dir.path().join("release"), "go").unwrap();
        let _ = registry
            .wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap();
        let settlement = registry.take_settlement(&id).await.unwrap();
        assert!(
            !before_release,
            "settlement must remain pending while the descendant can write"
        );
        assert!(
            settlement.modified.contains(&"late.txt".to_string()),
            "{settlement:?}"
        );
    }
}
