//! Background process tasks (TL-4/5): spawn, get, wait, kill with log caps.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::command::{CommandRunner, ManagedProcess, ProcessIdentity, ProcessRequest};
use crate::snapshot::SnapshotId;
use crate::windows_sandbox::assert_background_intent_spawn_allowed;

/// Pre-spawn workspace snapshot and write authority, used to settle a
/// background task when its process exits.
///
/// The allowlist travels with the baseline rather than being read at
/// settlement time because the authority a task runs under is the one it was
/// spawned with. A background process outlives the round that started it, so
/// there is no later scope to consult.
#[derive(Debug, Clone)]
pub struct MutationBaseline {
    pub snapshot: SnapshotId,
    pub workspace_root: PathBuf,
    /// Paths this task was allowed to modify. `None` means unconstrained: the
    /// task's changes are still accounted, but nothing is restored.
    pub write_allowlist: Option<Vec<String>>,
}

/// What the runtime found when the task's process exited.
///
/// Produced exactly once per task, by the reaper, without any tool call. A
/// waiter reads it; it never computes it (`docs/ARCHITECTURE.md` §18.3 H).
#[derive(Debug, Clone)]
pub struct BackgroundSettlement {
    /// Workspace-relative paths the task changed, after any restore.
    pub modified: Vec<String>,
    /// The write-scope violation this task committed, and what was done about
    /// it. `None` when the task stayed inside its authority.
    pub violation: Option<String>,
    /// Why the change set is unknown, when the workspace could not be diffed.
    pub note: Option<String>,
    /// The snapshot the task was measured against.
    pub snapshot: SnapshotId,
}

const MAX_CONCURRENT: usize = 4;
const MAX_LOG_BYTES: usize = 256 * 1024;
/// Completed task records kept for later `get`/`wait` calls. Pruning happens
/// when a new task is spawned, so a waiter cannot lose the task that just woke
/// it. At most `MAX_CONCURRENT` newly terminal records can temporarily sit
/// above this bound before the next spawn.
const MAX_RETAINED_TERMINAL_TASKS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTaskStatus {
    Running,
    /// Kill requested; process has been signaled but has not reaped yet.
    Killing,
    Exited,
    Killed,
}

impl BackgroundTaskStatus {
    fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Killing)
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Exited | Self::Killed)
    }
}

#[derive(Debug, Clone)]
pub struct BackgroundTaskSnapshot {
    pub id: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub status: BackgroundTaskStatus,
    pub exit_code: Option<i32>,
    pub log: String,
    pub duration_ms: u64,
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
    /// no owner; session-owned tasks are reaped when their session's goal
    /// reaches a terminal state or the daemon shuts down (R004 F7).
    owner_scope: Option<String>,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    status: BackgroundTaskStatus,
    exit_code: Option<i32>,
    log: String,
    started: Instant,
    finished: Option<Instant>,
    child: Option<ManagedProcess>,
    identity: Option<ProcessIdentity>,
    done: Arc<Notify>,
    process_done: bool,
    log_pumps_remaining: u8,
    /// Consumed by the reaper when the process exits, so the diff and any
    /// restore run exactly once and without waiting for a tool call.
    mutation_baseline: Option<MutationBaseline>,
    /// What the reaper found. Read by a waiter; never produced by one.
    settlement: Option<BackgroundSettlement>,
    /// Keeps the private scratch (and, on macOS/Linux, its OS lease) alive
    /// until the child and log pumps finish — so a backgrounded command holds
    /// its lease for its whole life. [`finalize_if_drained`] drops it at that
    /// point. On other platforms there is no lease, just the temp tree.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    sandbox_scratch: Option<crate::command::SandboxScratch>,
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    sandbox_scratch: Option<tempfile::TempDir>,
}

/// Process-backed background task registry shared via [`Arc`] on tool context.
#[derive(Clone)]
pub struct BackgroundTaskRegistry {
    inner: Arc<Mutex<RegistryState>>,
    /// The one spawn path, shared with foreground execution (PR 4).
    runner: CommandRunner,
    /// Dropped when the last registry handle is dropped (session end).
    kill_on_drop: Arc<KillOnDrop>,
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

impl BackgroundTaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_environment(environment: Arc<leveler_core::EnvSnapshot>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryState::default())),
            kill_on_drop: Arc::new(KillOnDrop::default()),
            runner: CommandRunner::with_environment(environment),
        }
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
        let mut st = self.inner.lock().await;
        prune_terminal_tasks(&mut st);
        let running = st.tasks.values().filter(|t| t.status.is_active()).count();
        if running >= MAX_CONCURRENT {
            return Err(format!(
                "background task limit reached ({MAX_CONCURRENT} concurrent)"
            ));
        }
        st.next += 1;
        let id = format!("bg-{}", st.next);

        let intent = request.filesystem_intent();
        // PR 0: this registry has no confining runner on Windows — the spawn
        // below is a plain one — so a restricted intent must be refused here,
        // never run unconfined.
        if let Err(err) = assert_background_intent_spawn_allowed(&intent, request.deny_network) {
            return Err(err.to_string());
        }

        let mut process = self
            .runner
            .spawn(&request)
            .await
            .map_err(|e| format!("spawn background {}: {e}", request.program))?;
        let identity = process.identity();
        let stdout = process.take_stdout();
        let stderr = process.take_stderr();
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let sandbox_scratch = process.take_sandbox_scratch();
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let sandbox_scratch: Option<tempfile::TempDir> = None;
        let log_pumps_remaining = u8::from(stdout.is_some()) + u8::from(stderr.is_some());
        let done = Arc::new(Notify::new());
        let reg = self.inner.clone();
        // Weak: must not keep KillOnDrop alive past registry handle drop.
        let kill_on_drop = Arc::downgrade(&self.kill_on_drop);
        let tid = id.clone();

        self.kill_on_drop.insert(id.clone(), identity);

        st.tasks.insert(
            id.clone(),
            TaskInner {
                id: id.clone(),
                owner_scope: owner_scope.map(str::to_string),
                program: request.program.clone(),
                args: request.args.clone(),
                cwd: request.cwd.clone(),
                status: BackgroundTaskStatus::Running,
                exit_code: None,
                log: String::new(),
                started: Instant::now(),
                finished: None,
                child: Some(process),
                identity: Some(identity),
                done: done.clone(),
                process_done: false,
                log_pumps_remaining,
                mutation_baseline,
                settlement: None,
                sandbox_scratch,
            },
        );
        drop(st);

        spawn_log_pump(reg.clone(), tid.clone(), stdout);
        spawn_log_pump(reg.clone(), tid.clone(), stderr);

        tokio::spawn(async move {
            let code = {
                let mut st = reg.lock().await;
                let Some(task) = st.tasks.get_mut(&tid) else {
                    return;
                };
                let Some(mut child) = task.child.take() else {
                    return;
                };
                drop(st);
                match child.wait().await {
                    Ok(s) => s.code(),
                    Err(_) => None,
                }
            };
            // Settle before publishing the terminal state, so a waiter woken
            // by `finalize_if_drained` never observes a task that is finished
            // but not yet accounted for.
            let baseline = {
                let mut st = reg.lock().await;
                st.tasks
                    .get_mut(&tid)
                    .and_then(|t| t.mutation_baseline.take())
            };
            let settlement = match baseline {
                Some(baseline) => Some(settle(&baseline).await),
                None => None,
            };
            let mut st = reg.lock().await;
            if let Some(task) = st.tasks.get_mut(&tid) {
                task.process_done = true;
                task.exit_code = code;
                task.identity = None;
                task.settlement = settlement;
                finalize_if_drained(task);
            }
            if let Some(kod) = kill_on_drop.upgrade() {
                kod.remove(&tid);
            }
        });

        Ok(id)
    }

    pub async fn get(&self, id: &str) -> Option<BackgroundTaskSnapshot> {
        let st = self.inner.lock().await;
        st.tasks.get(id).map(snapshot)
    }

    /// Take the settlement exactly once, so a task's file changes are reported
    /// to the agent loop a single time however often it is waited on.
    pub async fn take_settlement(&self, id: &str) -> Option<BackgroundSettlement> {
        let mut st = self.inner.lock().await;
        st.tasks.get_mut(id).and_then(|t| t.settlement.take())
    }

    pub async fn wait(
        &self,
        id: &str,
        timeout: Option<Duration>,
        cancellation: &CancellationToken,
    ) -> Result<BackgroundTaskSnapshot, String> {
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
        let ids: Vec<String> = {
            let st = self.inner.lock().await;
            st.tasks
                .values()
                .filter(|t| t.owner_scope.as_deref() == Some(scope) && !t.status.is_terminal())
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

    pub async fn kill(&self, id: &str) -> Result<BackgroundTaskSnapshot, String> {
        let identity = {
            let mut st = self.inner.lock().await;
            let task = st
                .tasks
                .get_mut(id)
                .ok_or_else(|| format!("unknown task `{id}`"))?;
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
                return Err(format!(
                    "task `{id}` has no process identity to signal (Child already taken)"
                ));
            }
            // Running → Killing; prefer pid/pgid so kill works after reaper take().
            task.status = BackgroundTaskStatus::Killing;
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

fn spawn_log_pump<R>(reg: Arc<Mutex<RegistryState>>, tid: String, stream: Option<R>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(stream) = stream else {
        return;
    };
    tokio::spawn(async move {
        let mut reader = BufReader::new(stream);
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => append_log(&reg, &tid, &buf[..n]).await,
                Err(_) => break,
            }
        }
        let mut st = reg.lock().await;
        if let Some(task) = st.tasks.get_mut(&tid) {
            task.log_pumps_remaining = task.log_pumps_remaining.saturating_sub(1);
            finalize_if_drained(task);
        }
    });
}

/// Diff the workspace against the task's baseline and, when the task wrote
/// outside the authority it was spawned with, restore it.
///
/// Restore runs ONLY under an explicit allowlist. A default background task —
/// a dev server, a watcher — is accounted and never rolled back: it is
/// supposed to write, and reverting a running server's output is worse than
/// reporting it (K17).
async fn settle(baseline: &MutationBaseline) -> BackgroundSettlement {
    let root = &baseline.workspace_root;
    let id = &baseline.snapshot;

    let mut modified = Vec::new();
    let mut note = None;
    match crate::snapshot::WorkspaceSnapshot::changed_since(root, id).await {
        Ok(changed) => modified = changed,
        Err(error) => {
            note = Some(format!(
                "could not diff the workspace after this background task ({error}); \
                 its file changes were not tracked"
            ));
        }
    }

    let mut violation = None;
    if let Some(allowlist) = baseline.write_allowlist.as_deref() {
        let outside: Vec<&str> = modified
            .iter()
            .map(String::as_str)
            .filter(|path| !allowlist.iter().any(|allowed| path_allows(allowed, path)))
            .collect();
        if !outside.is_empty() {
            let detail = format!(
                "background task modified files outside allowed paths: {}",
                outside.join(", ")
            );
            match crate::snapshot::WorkspaceSnapshot::restore(root, id).await {
                Ok(()) => {
                    modified.clear();
                    violation = Some(format!("{detail}; workspace restored"));
                }
                Err(error) => {
                    violation = Some(format!(
                        "{detail}; automatic workspace restore failed: {error}"
                    ));
                }
            }
        }
    }

    BackgroundSettlement {
        modified,
        violation,
        note,
        snapshot: id.clone(),
    }
}

fn path_allows(allowed: &str, modified: &str) -> bool {
    let allowed = allowed.trim_end_matches('/');
    modified == allowed || modified.starts_with(&format!("{allowed}/"))
}

fn finalize_if_drained(task: &mut TaskInner) {
    if !task.process_done || task.log_pumps_remaining != 0 {
        return;
    }
    task.status = match task.status {
        BackgroundTaskStatus::Killing => BackgroundTaskStatus::Killed,
        BackgroundTaskStatus::Running => BackgroundTaskStatus::Exited,
        terminal => terminal,
    };
    task.finished = Some(Instant::now());
    // The process and both output pumps are done, so no child can use TMPDIR.
    // Release potentially large temp files independently of history retention.
    task.sandbox_scratch.take();
    task.done.notify_waiters();
}

async fn append_log(reg: &Arc<Mutex<RegistryState>>, id: &str, bytes: &[u8]) {
    let mut st = reg.lock().await;
    let Some(task) = st.tasks.get_mut(id) else {
        return;
    };
    let chunk = String::from_utf8_lossy(bytes);
    task.log.push_str(&chunk);
    truncate_log(&mut task.log);
}

fn truncate_log(log: &mut String) {
    if log.len() > MAX_LOG_BYTES {
        // The marker embeds the dropped byte count, which depends on the
        // boundary-adjusted cut point — compute it first, then truncate.
        let dropped = leveler_core::ceil_char_boundary(log, log.len() - MAX_LOG_BYTES);
        let marker = format!("…[truncated {dropped} bytes]…");
        *log = leveler_core::truncate_tail_bytes(log, MAX_LOG_BYTES, &marker);
    }
}

fn snapshot(task: &TaskInner) -> BackgroundTaskSnapshot {
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
        log: task.log.clone(),
        duration_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::ProcessRequest;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use crate::command::prepare_sandbox_paths;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(super) fn unix_host_registry() -> BackgroundTaskRegistry {
        BackgroundTaskRegistry::with_environment(Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )))
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
                    program: "true".into(),
                    args: Vec::new(),
                    cwd: PathBuf::new(),
                    status: BackgroundTaskStatus::Exited,
                    exit_code: Some(0),
                    log: String::new(),
                    started: finished,
                    finished: Some(finished),
                    child: None,
                    identity: None,
                    done: Arc::new(Notify::new()),
                    process_done: true,
                    log_pumps_remaining: 0,
                    mutation_baseline: None,
                    settlement: None,
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
                    program: "sleep".into(),
                    args: Vec::new(),
                    cwd: PathBuf::new(),
                    status,
                    exit_code: None,
                    log: String::new(),
                    started: now,
                    finished: None,
                    child: None,
                    identity: None,
                    done: Arc::new(Notify::new()),
                    process_done: false,
                    log_pumps_remaining: 0,
                    mutation_baseline: None,
                    settlement: None,
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
            program: "true".into(),
            args: Vec::new(),
            cwd: PathBuf::new(),
            status: BackgroundTaskStatus::Running,
            exit_code: Some(0),
            log: String::new(),
            started: Instant::now(),
            finished: None,
            child: None,
            identity: None,
            done: Arc::new(Notify::new()),
            process_done: true,
            log_pumps_remaining: 0,
            mutation_baseline: None,
            settlement: None,
            sandbox_scratch: {
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                {
                    Some(crate::command::SandboxScratch::unleased(scratch))
                }
                #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                {
                    Some(scratch)
                }
            },
        };

        finalize_if_drained(&mut task);

        assert_eq!(task.status, BackgroundTaskStatus::Exited);
        assert!(task.sandbox_scratch.is_none());
        assert!(!scratch_path.exists());
    }

    /// The baseline is consumed by the reaper, so a task is diffed and
    /// restored at most once however many times it is inspected afterwards.
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
            write_allowlist: None,
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

    /// The runtime settles a background task when its process exits, not when
    /// the model happens to call `wait_task`.
    ///
    /// A background process outlives the round that started it. If the write
    /// allowlist were only enforced at wait-end, a model that never waits —
    /// or a turn that ends first — would leave an authority violation standing
    /// on disk. Settlement is a runtime guarantee, so it does not wait for a
    /// tool call.
    #[tokio::test]
    async fn a_background_task_is_settled_on_exit_without_anyone_waiting() {
        use crate::snapshot::WorkspaceSnapshot;

        let dir = leveler_test_support::git::scratch_repo();
        std::fs::create_dir_all(dir.path().join("allowed")).expect("mkdir");
        std::fs::write(dir.path().join("allowed/keep"), "keep\n").expect("seed");
        std::fs::write(dir.path().join("protected"), "original\n").expect("seed");
        leveler_test_support::git::run(dir.path(), &["add", "-A"]);
        leveler_test_support::git::run(dir.path(), &["commit", "-qm", "i"]);
        let snapshot = WorkspaceSnapshot::capture(dir.path())
            .await
            .expect("capture")
            .expect("git repo");

        let reg = BackgroundTaskRegistry::new();
        let request = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "echo tampered > protected".into()],
            dir.path().to_path_buf(),
        );
        let id = reg
            .spawn(
                request,
                Some(MutationBaseline {
                    snapshot,
                    workspace_root: dir.path().to_path_buf(),
                    write_allowlist: Some(vec!["allowed".to_string()]),
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
        assert!(
            settlement
                .violation
                .as_deref()
                .is_some_and(|v| v.contains("protected")),
            "the violation must name the path: {settlement:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("protected")).expect("read"),
            "original\n",
            "the runtime must have restored the file the task was not allowed to touch"
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
        // does not refuse a normal confined command). OS confinement canary is
        // `background_confined_blocks_write_outside_workspace` below.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let reg = unix_host_registry();
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let reg = BackgroundTaskRegistry::new();
        let ws = tempfile::tempdir().expect("ws");
        let mut req =
            ProcessRequest::new("echo", vec!["sandboxed-bg".into()], ws.path().to_path_buf());
        req.deny_network = true;
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

        let reg = unix_host_registry();

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
        let reg = super::tests::unix_host_registry();
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
        let reg = super::tests::unix_host_registry();
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
}
