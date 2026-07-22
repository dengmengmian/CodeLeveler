//! Background process tasks (TL-4/5): spawn, get, wait, kill with log caps.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::command::{ProcessRequest, sandbox_command};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::command::{
    SandboxPaths, apply_sandbox_environment, prepare_sandbox_paths, should_read_host_caches,
};
use crate::snapshot::SnapshotId;
use crate::windows_sandbox::{FilesystemIntent, assert_intent_spawn_allowed};

/// Pre-spawn workspace snapshot used for wait-end mutation accounting (PR-3b).
#[derive(Debug, Clone)]
pub struct MutationBaseline {
    pub snapshot: SnapshotId,
    pub workspace_root: PathBuf,
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

/// Identity needed to signal a process after the registry no longer holds `Child`
/// (the wait reaper `take()`s it).
#[derive(Debug, Clone, Copy)]
struct ProcessIdentity {
    /// Unix process group id (set when spawned with `process_group(0)`).
    #[cfg(unix)]
    pgid: i32,
    /// Windows / non-Unix process id for taskkill-style tree kill.
    #[cfg(not(unix))]
    pid: u32,
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
                signal_process_tree(*identity);
            }
        }
    }
}

struct TaskInner {
    id: String,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    status: BackgroundTaskStatus,
    exit_code: Option<i32>,
    log: String,
    started: Instant,
    finished: Option<Instant>,
    child: Option<Child>,
    identity: Option<ProcessIdentity>,
    done: Arc<Notify>,
    process_done: bool,
    log_pumps_remaining: u8,
    /// Taken once by wait-end accounting so restore/diff runs at most once.
    mutation_baseline: Option<MutationBaseline>,
    /// Keeps the private temp tree alive until the child and log pumps finish.
    /// [`finalize_if_drained`] drops it immediately at that point.
    sandbox_scratch: Option<tempfile::TempDir>,
}

/// Process-backed background task registry shared via [`Arc`] on tool context.
#[derive(Clone)]
pub struct BackgroundTaskRegistry {
    inner: Arc<Mutex<RegistryState>>,
    /// Dropped when the last registry handle is dropped (session end).
    kill_on_drop: Arc<KillOnDrop>,
    environment: Arc<leveler_core::EnvSnapshot>,
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
            environment,
        }
    }

    pub async fn spawn(
        &self,
        request: ProcessRequest,
        mutation_baseline: Option<MutationBaseline>,
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

        let intent = request.filesystem_intent.clone().unwrap_or_else(|| {
            FilesystemIntent::from_legacy(
                request.write_root.as_deref(),
                /* full_access */ request.write_root.is_none(),
            )
        });
        if let Err(err) = assert_intent_spawn_allowed(&intent, request.deny_network) {
            return Err(err.to_string());
        }

        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let sandbox_paths = request
            .write_root
            .as_ref()
            .map(|workspace| {
                prepare_sandbox_paths(
                    &self.environment,
                    workspace,
                    should_read_host_caches(&request),
                )
            })
            .transpose()
            .map_err(|err| format!("create private sandbox scratch directory: {err}"))?;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let sandbox_paths: Option<tempfile::TempDir> = None;

        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let sandbox_scratch_root = sandbox_paths.as_ref().map(SandboxPaths::scratch_path);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let sandbox_scratch_root: Option<&std::path::Path> = None;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let sandbox_cache_write_roots = sandbox_paths
            .as_ref()
            .map(SandboxPaths::cache_write_roots)
            .unwrap_or(&[]);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let sandbox_cache_write_roots: &[std::path::PathBuf] = &[];

        let (program, args) = sandbox_command(
            &request.program,
            &request.args,
            request.deny_network,
            request.write_root.as_deref(),
            &request.extra_read_roots,
            sandbox_scratch_root,
            sandbox_cache_write_roots,
        );

        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .current_dir(&request.cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null());
        // Own process group so kill can signal the whole tree via pgid even
        // after the wait reaper has taken `Child`.
        #[cfg(unix)]
        {
            cmd.process_group(0);
            crate::command::set_parent_death_signal(&mut cmd);
        }
        cmd.env_clear();
        for (name, value) in self.environment.vars_os() {
            let name_text = name.to_string_lossy();
            let denied = crate::is_credential_env_name(&name_text)
                || request.deny_env.iter().any(|v| v == &name_text);
            if !denied || request.allow_env.iter().any(|v| v == &name_text) {
                cmd.env(name, value);
            }
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(paths) = sandbox_paths.as_ref() {
            apply_sandbox_environment(&mut cmd, paths);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn background {}: {e}", request.program))?;

        let pid = child
            .id()
            .ok_or_else(|| format!("spawn background {}: child has no pid", request.program))?;
        let identity = ProcessIdentity {
            #[cfg(unix)]
            pgid: pid as i32,
            #[cfg(not(unix))]
            pid,
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
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
                program: request.program.clone(),
                args: request.args.clone(),
                cwd: request.cwd.clone(),
                status: BackgroundTaskStatus::Running,
                exit_code: None,
                log: String::new(),
                started: Instant::now(),
                finished: None,
                child: Some(child),
                identity: Some(identity),
                done: done.clone(),
                process_done: false,
                log_pumps_remaining,
                mutation_baseline,
                sandbox_scratch: {
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    {
                        sandbox_paths.map(SandboxPaths::into_scratch)
                    }
                    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                    {
                        sandbox_paths
                    }
                },
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
            let mut st = reg.lock().await;
            if let Some(task) = st.tasks.get_mut(&tid) {
                task.process_done = true;
                task.exit_code = code;
                task.identity = None;
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

    /// Take the pre-spawn mutation baseline exactly once (wait-end accounting).
    ///
    /// Subsequent calls return `None` so diff/restore cannot double-apply.
    pub async fn take_mutation_baseline(&self, id: &str) -> Option<MutationBaseline> {
        let mut st = self.inner.lock().await;
        st.tasks
            .get_mut(id)
            .and_then(|t| t.mutation_baseline.take())
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
                _ = tokio::time::sleep(t) => return Err("wait timed out".into()),
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
                let _ = child.start_kill();
            }
            identity
        };

        if let Some(identity) = identity {
            // SIGTERM then SIGKILL so stubborn children still die after
            // the reaper has taken `Child`.
            signal_process_tree_graceful(identity).await;
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

/// Immediate tree kill (drop path / hard kill).
///
/// Unix: `killpg(SIGKILL)` on the recorded process group (spawn used
/// `process_group(0)`). Windows: `taskkill /T /F` by pid — best-effort tree
/// kill; not equivalent to foreground Job Object / `process-wrap` (follow-up
/// if Windows background parity is required).
fn signal_process_tree(identity: ProcessIdentity) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let group = Pid::from_raw(identity.pgid);
        let _ = killpg(group, Signal::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &identity.pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = identity;
    }
}

/// Graceful then hard kill (async kill path).
async fn signal_process_tree_graceful(identity: ProcessIdentity) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let group = Pid::from_raw(identity.pgid);
        let _ = killpg(group, Signal::SIGTERM);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = killpg(group, Signal::SIGKILL);
    }
    #[cfg(windows)]
    {
        // taskkill /T /F terminates the tree immediately (see signal_process_tree).
        signal_process_tree(identity);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = identity;
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
        let overflow = log.len() - MAX_LOG_BYTES;
        let mut keep_from = overflow;
        while !log.is_char_boundary(keep_from) {
            keep_from += 1;
        }
        *log = format!("…[truncated {keep_from} bytes]…{}", &log[keep_from..]);
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
    use crate::windows_sandbox::FilesystemIntent;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn unix_host_registry() -> BackgroundTaskRegistry {
        BackgroundTaskRegistry::with_environment(Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )))
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
            sandbox_scratch: Some(scratch),
        };

        finalize_if_drained(&mut task);

        assert_eq!(task.status, BackgroundTaskStatus::Exited);
        assert!(task.sandbox_scratch.is_none());
        assert!(!scratch_path.exists());
    }

    #[tokio::test]
    async fn mutation_baseline_is_taken_once() {
        use crate::snapshot::WorkspaceSnapshot;

        let dir = tempfile::tempdir().expect("tempdir");
        // git repo so we can capture a real SnapshotId
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("git init -q && git config user.email t@t && git config user.name t && echo x > a && git add -A && git commit -qm i")
            .current_dir(dir.path())
            .output()
            .await
            .expect("git init");
        assert!(out.status.success(), "git init failed");
        let snap = WorkspaceSnapshot::capture(dir.path())
            .await
            .expect("capture")
            .expect("git repo");
        let baseline = MutationBaseline {
            snapshot: snap,
            workspace_root: dir.path().to_path_buf(),
        };

        let reg = BackgroundTaskRegistry::new();
        let req = ProcessRequest::new("echo", vec!["ok".into()], dir.path().to_path_buf());
        let id = reg.spawn(req, Some(baseline)).await.expect("spawn");
        let first = reg
            .take_mutation_baseline(&id)
            .await
            .expect("baseline present once");
        assert_eq!(first.workspace_root, dir.path());
        assert!(
            reg.take_mutation_baseline(&id).await.is_none(),
            "baseline must be consumed on first take"
        );
        let _ = reg
            .wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await;
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
        req.write_root = Some(ws.path().to_path_buf());
        req.filesystem_intent = Some(FilesystemIntent::WorkspaceWrite {
            write_root: ws.path().to_path_buf(),
            extra_read_roots: vec![],
        });
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
        inside.write_root = Some(ws.clone());
        inside.filesystem_intent = Some(FilesystemIntent::WorkspaceWrite {
            write_root: ws.clone(),
            extra_read_roots: vec![],
        });
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
        outside.write_root = Some(ws.clone());
        outside.filesystem_intent = Some(FilesystemIntent::WorkspaceWrite {
            write_root: ws.clone(),
            extra_read_roots: vec![],
        });
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
        let prepared = prepare_sandbox_paths(&reg.environment, &ws, false).unwrap();
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
        poison.write_root = Some(ws.clone());
        poison.filesystem_intent = Some(FilesystemIntent::WorkspaceWrite {
            write_root: ws.clone(),
            extra_read_roots: vec![],
        });
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
