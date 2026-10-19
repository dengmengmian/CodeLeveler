//! User shell execution (`!command`): session-scoped direct host execution.
//!
//! USER EXPLICIT COMMAND → application use case → shared host execution
//! boundary (`CommandRunner::run_streaming`) → canonical EngineEvent facts →
//! the same exhaustive projection every other client fact uses. No model, no
//! agent loop, no tool registry — and none of it ever enters the model
//! conversation.
//!
//! The user typing the command IS the authorization for that exact
//! invocation (no approval dialog), but host safety is not waived: the same
//! permission-profile write confinement, network sandbox, env scrubbing, and
//! process-tree termination as agent shell execution apply.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use leveler_client_protocol::UiUserShell;
use leveler_core::{SessionId, UserShellId};
use leveler_engine::EngineEvent;
use leveler_execution::{OutputStream, ProcessError, ProcessOutput};

/// Bounded live/completed output tail per execution. Large enough for a real
/// test run; the explicit truncation marker handles the rest, and 16 retained
/// executions keep the session store bounded.
pub(crate) const OUTPUT_TAIL_CAP: usize = 1024 * 1024;
/// Completed executions kept per session for reconnect/history.
const HISTORY_CAP: usize = 16;
// Complete captures are retained for the life of their session. They are
// referenced by the transcript (a finished record names its capture), so
// trimming the oldest ones would delete a file the session can still open.
// `remove_session_output` reclaims them when the session itself is deleted.

/// A path component that cannot escape `shell-output`.
///
/// Identifiers are UUIDs (or `ush-<uuid>`), so valid values pass unchanged.
/// A malformed id from a client is hex-encoded instead of being joined as a
/// path segment; the cleanup and capture paths are destructive/authoritative
/// and must not honour `..` or a separator.
fn safe_component(value: &str) -> String {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        return value.to_string();
    }
    let mut encoded = String::from("invalid-");
    for byte in value.bytes().take(32) {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

fn session_output_directory(state_dir: &Path, session_id: &SessionId) -> PathBuf {
    state_dir
        .join("shell-output")
        .join(safe_component(session_id.as_str()))
}

/// The directory that holds one execution's exact stdout/stderr streams.
pub(crate) fn output_directory(
    state_dir: &Path,
    session_id: &SessionId,
    id: &UserShellId,
) -> PathBuf {
    session_output_directory(state_dir, session_id).join(safe_component(id.as_str()))
}

/// The directory of a COMPLETE capture, or `None` when any stream's sidecar
/// is missing or reports an unfinished copy. The caller must never advertise
/// bytes the capture does not hold.
pub(crate) fn complete_output_directory(directory: &Path) -> Option<String> {
    for stream in ["stdout", "stderr", "combined"] {
        let meta = std::fs::read_to_string(directory.join(format!("{stream}.meta.json"))).ok()?;
        if !meta.contains("\"complete\":true") {
            return None;
        }
    }
    Some(directory.display().to_string())
}

/// The last `cap` bytes of a COMPLETE combined capture.
///
/// The background registry's retained log tags every chunk with
/// `[stdout] `/`[stderr] `; that is a registry presentation, not the user
/// shell's output. Replacing a finished record from the exact capture keeps
/// the raw bytes (and their arrival order) without leaking those tags.
pub(crate) fn complete_capture_tail(directory: &Path, cap: usize) -> Option<(String, bool)> {
    complete_output_directory(directory)?;
    let path = directory.join("combined.log");
    let mut file = std::fs::File::open(&path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(cap as u64);
    {
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut bytes = Vec::new();
    {
        use std::io::Read;
        file.read_to_end(&mut bytes).ok()?;
    }
    // The cut can land inside a multi-byte character. Drop the partial one
    // instead of rendering a replacement glyph at the head of the record.
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{FFFD}').unwrap_or(&text).to_string();
    Some((text, start > 0))
}

/// Prepare one execution's capture directory, pruning older ones first.
pub(crate) fn prepare_output_directory(
    state_dir: &Path,
    session_id: &SessionId,
    id: &UserShellId,
) -> PathBuf {
    let session_dir = session_output_directory(state_dir, session_id);
    if let Err(error) = ensure_private_directory(&session_dir) {
        tracing::warn!(
            %error,
            path = %session_dir.display(),
            "user shell capture directory could not be made private"
        );
    }
    output_directory(state_dir, session_id, id)
}

/// Create a directory only this user can enter. Captured output may contain
/// credentials, so the process umask is not the boundary.
fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Remove a deleted session's captures. The database is the authority for
/// whether a session exists; this only reclaims its derived files after the
/// delete committed.
pub(crate) fn remove_session_output(state_dir: &Path, session_id: &SessionId) {
    let path = session_output_directory(state_dir, session_id);
    if path.is_dir()
        && let Err(error) = std::fs::remove_dir_all(&path)
    {
        tracing::warn!(
            %error,
            path = %path.display(),
            "deleted session output could not be removed"
        );
    }
}

/// Bounded tail buffer with an honest truncation flag.
#[derive(Debug, Default)]
struct OutputTail {
    text: VecDeque<u8>,
    truncated: bool,
}

impl OutputTail {
    fn push(&mut self, chunk: &str) {
        for &b in chunk.as_bytes() {
            if self.text.len() == OUTPUT_TAIL_CAP {
                self.text.pop_front();
                self.truncated = true;
            }
            self.text.push_back(b);
        }
    }

    fn snapshot(&self) -> (String, bool) {
        let bytes: Vec<u8> = self.text.iter().copied().collect();
        (String::from_utf8_lossy(&bytes).into_owned(), self.truncated)
    }

    /// Replace the best-effort live bytes with the execution's authoritative
    /// capture. The live channel is bounded and drops rather than block the
    /// child, so the streamed tail can be missing its end; the runner's own
    /// capture is the real head+tail.
    fn adopt(&mut self, text: &str) {
        self.text.clear();
        self.truncated = false;
        self.push(text);
    }
}

#[derive(Debug)]
struct ActiveShell {
    id: UserShellId,
    command: String,
    cwd: String,
    started: Instant,
    cancel: CancellationToken,
    /// Cancelled to ask the streaming driver to hand the same process over.
    handoff: CancellationToken,
    /// The one in-flight move-to-background request, if any.
    background_request: Option<oneshot::Sender<Result<String, String>>>,
    tail: OutputTail,
}

#[derive(Debug)]
struct BackgroundShell {
    shell: UiUserShell,
    started: Instant,
    tail: OutputTail,
    /// The capture directory started in the foreground and continued by the
    /// adopted reader tasks. Exposed only once its sidecars report a complete
    /// copy.
    capture_dir: PathBuf,
}

#[derive(Debug, Default)]
struct SessionShells {
    active: Option<ActiveShell>,
    /// Executions moved to the background registry. Their task id is the link
    /// to that registry; the execution id and process are unchanged.
    background: Vec<BackgroundShell>,
    /// Completed executions, oldest first.
    history: VecDeque<UiUserShell>,
}

/// Outcome of asking the active execution to move to the background.
#[derive(Debug)]
pub(crate) enum BackgroundRequest {
    NotFound,
    /// The execution already runs in the background; this is the same task.
    AlreadyBackground(String),
    /// A request is already in flight. Idempotent: no second task is made.
    Pending,
    /// The worker owns this response. It answers exactly once.
    Requested(oneshot::Receiver<Result<String, String>>),
}

/// What the worker needs to hand the active execution over: the output it has
/// already shown, and the reply slot for the client command.
pub(crate) struct BackgroundedActive {
    reply: Option<oneshot::Sender<Result<String, String>>>,
}

impl BackgroundedActive {
    pub fn reply(self, result: Result<String, String>) {
        if let Some(tx) = self.reply {
            let _ = tx.send(result);
        }
    }
}

/// The one owner of user-shell execution state: the active execution per
/// session (with its cancel token) plus a bounded completed history. Shared
/// via `Arc` between the runtime client and the detached shell workers.
#[derive(Default)]
pub(crate) struct UserShellStore {
    inner: Mutex<HashMap<SessionId, SessionShells>>,
}

impl UserShellStore {
    pub fn begin(
        &self,
        session_id: &SessionId,
        id: UserShellId,
        command: String,
        cwd: String,
        cancel: CancellationToken,
        handoff: CancellationToken,
    ) {
        let mut inner = self.inner.lock().unwrap();
        let shells = inner.entry(session_id.clone()).or_default();
        shells.active = Some(ActiveShell {
            id,
            command,
            cwd,
            started: Instant::now(),
            cancel,
            handoff,
            background_request: None,
            tail: OutputTail::default(),
        });
    }

    /// Ask the active execution to move to the background. The worker observes
    /// the handoff token and transfers the same process. A repeated request
    /// returns `Pending`; it never queues a second transfer.
    pub fn request_background(
        &self,
        session_id: &SessionId,
        id: &UserShellId,
    ) -> BackgroundRequest {
        let mut inner = self.inner.lock().unwrap();
        let Some(shells) = inner.get_mut(session_id) else {
            return BackgroundRequest::NotFound;
        };
        if let Some(active) = shells.active.as_mut().filter(|a| &a.id == id) {
            if active.background_request.is_some() {
                return BackgroundRequest::Pending;
            }
            let (tx, rx) = oneshot::channel();
            active.background_request = Some(tx);
            active.handoff.cancel();
            return BackgroundRequest::Requested(rx);
        }
        if let Some(shell) = shells
            .background
            .iter()
            .find(|b| &b.shell.id == id)
            .and_then(|b| b.shell.task_id.clone())
        {
            return BackgroundRequest::AlreadyBackground(shell);
        }
        BackgroundRequest::NotFound
    }

    /// Answer a pending transfer request exactly once. A late or duplicate
    /// answer is a no-op.
    pub fn complete_background_request(
        &self,
        session_id: &SessionId,
        id: &UserShellId,
        result: Result<String, String>,
    ) {
        let sender = self
            .inner
            .lock()
            .unwrap()
            .get_mut(session_id)
            .and_then(|s| s.active.as_mut())
            .filter(|a| &a.id == id)
            .and_then(|a| a.background_request.take());
        if let Some(tx) = sender {
            let _ = tx.send(result);
        }
    }

    /// When the active foreground execution started. The adopted task keeps
    /// the whole execution's duration rather than starting a new clock.
    pub fn active_started(&self, session_id: &SessionId, id: &UserShellId) -> Option<Instant> {
        self.inner
            .lock()
            .unwrap()
            .get(session_id)?
            .active
            .as_ref()
            .filter(|a| &a.id == id)
            .map(|a| a.started)
    }

    /// The output the foreground already showed, for seeding the adopted
    /// task's log. Not a second copy of the execution.
    pub fn foreground_output(
        &self,
        session_id: &SessionId,
        id: &UserShellId,
    ) -> Option<(String, bool)> {
        let inner = self.inner.lock().unwrap();
        let active = inner
            .get(session_id)?
            .active
            .as_ref()
            .filter(|a| &a.id == id)?;
        Some(active.tail.snapshot())
    }

    /// Commit the ownership move: the active execution becomes a background
    /// record linked to `task_id`. The reply slot is returned so the worker
    /// can answer only after the registry accepted the process.
    pub fn background(
        &self,
        session_id: &SessionId,
        id: &UserShellId,
        task_id: String,
        capture_dir: PathBuf,
    ) -> Option<BackgroundedActive> {
        let mut inner = self.inner.lock().unwrap();
        let shells = inner.get_mut(session_id)?;
        if shells.active.as_ref().is_none_or(|a| &a.id != id) {
            return None;
        }
        let active = shells.active.take()?;
        let (output, output_truncated) = active.tail.snapshot();
        let elapsed_secs = active.started.elapsed().as_secs();
        shells.background.push(BackgroundShell {
            shell: UiUserShell {
                id: active.id,
                command: active.command,
                cwd: active.cwd,
                status: "background".to_string(),
                elapsed_secs,
                exit_code: None,
                output_tail: output.clone(),
                output_truncated,
                full_output: None,
                task_id: Some(task_id),
            },
            started: active.started,
            tail: active.tail,
            capture_dir,
        });
        Some(BackgroundedActive {
            reply: active.background_request,
        })
    }

    /// Append a live chunk to a background execution, found by execution id.
    pub fn append_background_output(&self, session_id: &SessionId, id: &UserShellId, chunk: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(shell) = inner
            .get_mut(session_id)
            .and_then(|s| s.background.iter_mut().find(|b| &b.shell.id == id))
        {
            shell.tail.push(chunk);
        }
    }

    /// Resolve the execution linked to a background registry task.
    pub fn execution_for_task(&self, session_id: &SessionId, task_id: &str) -> Option<UserShellId> {
        self.inner
            .lock()
            .unwrap()
            .get(session_id)?
            .background
            .iter()
            .find(|b| b.shell.task_id.as_deref() == Some(task_id))
            .map(|b| b.shell.id.clone())
    }

    /// Move a background execution to history with the registry's terminal
    /// facts. Returns the updated shell for the live event.
    pub fn finish_background(
        &self,
        session_id: &SessionId,
        task_id: &str,
        exit_code: Option<i32>,
        status: &str,
        duration_ms: u64,
        authoritative: Option<&str>,
    ) -> Option<UiUserShell> {
        let mut inner = self.inner.lock().unwrap();
        let shells = inner.get_mut(session_id)?;
        let index = shells
            .background
            .iter()
            .position(|b| b.shell.task_id.as_deref() == Some(task_id))?;
        let background = shells.background.remove(index);
        let mut tail = background.tail;
        if let Some(text) = authoritative {
            tail.adopt(text);
        }
        let (output_tail, output_truncated) = tail.snapshot();
        let shell = UiUserShell {
            id: background.shell.id,
            command: background.shell.command,
            cwd: background.shell.cwd,
            status: status.to_string(),
            elapsed_secs: duration_ms / 1000,
            exit_code,
            output_tail,
            output_truncated,
            full_output: complete_output_directory(&background.capture_dir),
            task_id: None,
        };
        shells.history.push_back(shell.clone());
        while shells.history.len() > HISTORY_CAP {
            shells.history.pop_front();
        }
        Some(shell)
    }

    pub fn append_output(&self, session_id: &SessionId, id: &UserShellId, chunk: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(active) = inner
            .get_mut(session_id)
            .and_then(|s| s.active.as_mut())
            .filter(|a| &a.id == id)
        {
            active.tail.push(chunk);
        }
    }

    pub fn mark_output_truncated(&self, session_id: &SessionId, id: &UserShellId) {
        if let Some(active) = self
            .inner
            .lock()
            .unwrap()
            .get_mut(session_id)
            .and_then(|s| s.active.as_mut())
            .filter(|a| &a.id == id)
        {
            active.tail.truncated = true;
        }
    }

    /// Move the active execution to history with its terminal facts. Returns
    /// the total runtime. A stale id (already finished) is a no-op `None`.
    ///
    /// `authoritative` replaces the streamed tail when the bounded live channel
    /// dropped bytes; `None` keeps the interleaved live tail.
    pub fn finish(
        &self,
        session_id: &SessionId,
        id: &UserShellId,
        exit_code: Option<i32>,
        status: &str,
        authoritative: Option<&str>,
        full_output: Option<String>,
    ) -> Option<u64> {
        let mut inner = self.inner.lock().unwrap();
        let shells = inner.get_mut(session_id)?;
        if let Some(active) = shells.active.take().filter(|a| &a.id == id) {
            let duration_ms = active.started.elapsed().as_millis() as u64;
            let mut tail = active.tail;
            if let Some(text) = authoritative {
                tail.adopt(text);
            }
            let (output_tail, output_truncated) = tail.snapshot();
            shells.history.push_back(UiUserShell {
                id: active.id,
                command: active.command,
                cwd: active.cwd,
                status: status.to_string(),
                elapsed_secs: duration_ms / 1000,
                exit_code,
                output_tail,
                output_truncated,
                full_output,
                task_id: None,
            });
            while shells.history.len() > HISTORY_CAP {
                shells.history.pop_front();
            }
            return Some(duration_ms);
        }
        // A background execution reaches history through the background
        // registry's terminal event, not through the foreground worker.
        None
    }

    /// The cancel token for exactly this execution — a stale id never cancels
    /// a newer one.
    pub fn cancel_token(
        &self,
        session_id: &SessionId,
        id: &UserShellId,
    ) -> Option<CancellationToken> {
        let inner = self.inner.lock().unwrap();
        inner
            .get(session_id)?
            .active
            .as_ref()
            .filter(|a| &a.id == id)
            .map(|a| a.cancel.clone())
    }

    /// Reconnect view: completed history (oldest first) then the active
    /// execution last, with live elapsed.
    pub fn snapshot(&self, session_id: &SessionId) -> Vec<UiUserShell> {
        let inner = self.inner.lock().unwrap();
        let Some(shells) = inner.get(session_id) else {
            return Vec::new();
        };
        let mut out: Vec<UiUserShell> = shells.history.iter().cloned().collect();
        for background in &shells.background {
            let (output_tail, output_truncated) = background.tail.snapshot();
            out.push(UiUserShell {
                id: background.shell.id.clone(),
                command: background.shell.command.clone(),
                cwd: background.shell.cwd.clone(),
                status: "background".to_string(),
                elapsed_secs: background.started.elapsed().as_secs(),
                exit_code: None,
                output_tail,
                output_truncated,
                full_output: None,
                task_id: background.shell.task_id.clone(),
            });
        }
        if let Some(active) = &shells.active {
            let (output_tail, output_truncated) = active.tail.snapshot();
            out.push(UiUserShell {
                id: active.id.clone(),
                command: active.command.clone(),
                cwd: active.cwd.clone(),
                status: "running".to_string(),
                elapsed_secs: active.started.elapsed().as_secs(),
                exit_code: None,
                output_tail,
                output_truncated,
                full_output: None,
                task_id: None,
            });
        }
        out
    }
}

/// Rebuild user-shell records from the durable event log for a snapshot after
/// a runtime restart. Live in-memory records win; this only fills executions
/// the current runtime no longer holds.
///
/// A started execution without a terminal fact is `unknown`, never `running`:
/// the process belonged to the dead runtime generation, so claiming it still
/// runs would be a lie.
pub(crate) async fn restore_from_events(
    db: &leveler_storage::Database,
    session_id: &SessionId,
) -> Result<Vec<UiUserShell>, String> {
    use leveler_storage::EventStore;
    let rows = db
        .load_by_types(
            session_id,
            &[
                "user_shell_started",
                "user_shell_backgrounded",
                "user_shell_finished",
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    let mut ordered: Vec<UiUserShell> = Vec::new();
    let mut index: HashMap<UserShellId, usize> = HashMap::new();
    for row in rows {
        let event = leveler_engine::EngineEvent::from_payload(&row.payload)
            .map_err(|error| error.to_string())?;
        match event {
            leveler_engine::EngineEvent::UserShellStarted {
                execution_id,
                command,
                cwd,
            } => {
                index.insert(execution_id.clone(), ordered.len());
                ordered.push(UiUserShell {
                    id: execution_id,
                    command,
                    cwd,
                    // Internal placeholder until a terminal fact is applied;
                    // the final pass turns a remaining one into `unknown`.
                    status: "running".to_string(),
                    elapsed_secs: 0,
                    exit_code: None,
                    output_tail: String::new(),
                    output_truncated: false,
                    full_output: None,
                    task_id: None,
                });
            }
            leveler_engine::EngineEvent::UserShellBackgrounded {
                execution_id,
                task_id,
                output_tail,
                output_truncated,
            } => {
                if let Some(shell) = index
                    .get(&execution_id)
                    .and_then(|index| ordered.get_mut(*index))
                {
                    // A terminal fact already written for this execution wins
                    // even if the transfer event reached the log later: the
                    // process is over, and no later record may claim it runs.
                    if !matches!(shell.status.as_str(), "success" | "failed" | "cancelled") {
                        shell.status = "background".to_string();
                        shell.task_id = Some(task_id);
                        if !output_tail.is_empty() {
                            shell.output_tail = output_tail;
                            shell.output_truncated = output_truncated;
                        }
                    }
                }
            }
            leveler_engine::EngineEvent::UserShellFinished {
                execution_id,
                exit_code,
                duration_ms,
                status,
                output_tail,
                output_truncated,
                full_output,
            } => {
                if let Some(shell) = index
                    .get(&execution_id)
                    .and_then(|index| ordered.get_mut(*index))
                {
                    shell.status = status;
                    shell.exit_code = exit_code;
                    shell.elapsed_secs = duration_ms / 1000;
                    shell.output_tail = output_tail;
                    shell.output_truncated = output_truncated;
                    shell.full_output = full_output;
                    shell.task_id = None;
                }
            }
            _ => {}
        }
    }
    // A background/started record with no terminal fact lost its owner with the
    // old runtime generation. It is unknown, not a live background task.
    for shell in &mut ordered {
        if matches!(shell.status.as_str(), "running" | "background") {
            shell.status = "unknown".to_string();
            shell.task_id = None;
        }
    }
    Ok(ordered)
}

/// Terminal status wording on the wire (`UserShellFinished.status`).
pub(crate) fn terminal_status(
    result: &Result<leveler_execution::ProcessOutput, ProcessError>,
) -> &'static str {
    match result {
        Ok(output) if output.exit_code == Some(0) => "success",
        Ok(_) => "failed",
        Err(ProcessError::Cancelled) => "cancelled",
        // Stopped, but nothing proves the tree is gone: say only that.
        Err(ProcessError::CancelUnconfirmed) => "unknown",
        Err(_) => "failed",
    }
}

/// Wire tag for an output chunk's stream.
pub(crate) fn stream_tag(stream: OutputStream) -> &'static str {
    match stream {
        OutputStream::Stdout => "stdout",
        OutputStream::Stderr => "stderr",
    }
}

/// The execution layer's own bounded capture, used for the completed record
/// when the live channel dropped bytes. Stdout and stderr are concatenated
/// because the interleaving between them is exactly what the dropped chunks
/// carried; the capture's own `…[N bytes dropped]…` marker still reports any
/// retained truncation.
pub(crate) fn authoritative_tail(output: &ProcessOutput) -> String {
    let mut text = String::with_capacity(output.stdout.len() + output.stderr.len());
    text.push_str(&output.stdout);
    text.push_str(&output.stderr);
    text
}

/// Canonical started fact.
pub(crate) fn started_event(id: &UserShellId, command: &str, cwd: &str) -> EngineEvent {
    EngineEvent::UserShellStarted {
        execution_id: id.clone(),
        command: command.to_string(),
        cwd: cwd.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unconfirmed_stop_is_unknown_never_cancelled_or_failed() {
        assert_eq!(terminal_status(&Err(ProcessError::Cancelled)), "cancelled");
        assert_eq!(
            terminal_status(&Err(ProcessError::CancelUnconfirmed)),
            "unknown"
        );
    }

    #[test]
    fn tail_is_bounded_and_flags_truncation() {
        let mut tail = OutputTail::default();
        tail.push(&"x".repeat(OUTPUT_TAIL_CAP + 10));
        let (text, truncated) = tail.snapshot();
        assert_eq!(text.len(), OUTPUT_TAIL_CAP);
        assert!(truncated);
    }

    /// The bounded live channel drops rather than block the child, so the
    /// streamed tail can be missing its end. When that happened, the completed
    /// record must be the runner's own capture instead.
    #[test]
    fn a_dropped_live_channel_leaves_the_completed_tail_to_the_authoritative_capture() {
        let store = UserShellStore::default();
        let session = SessionId::new("s1");
        let id = UserShellId::new("ush-dropped");
        store.begin(
            &session,
            id.clone(),
            "seq 1 2000".into(),
            "/repo".into(),
            CancellationToken::new(),
            CancellationToken::new(),
        );
        // Only the head of the stream made it through before chunks were lost.
        store.append_output(&session, &id, "1\n2\n");
        store.finish(
            &session,
            &id,
            Some(0),
            "success",
            Some("1\n\n\u{2026}[15000 bytes dropped]\u{2026}\n1999\n2000\n"),
            None,
        );
        let shell = store.snapshot(&session);
        assert_eq!(
            shell[0].output_tail,
            "1\n\n\u{2026}[15000 bytes dropped]\u{2026}\n1999\n2000\n"
        );
        assert!(
            shell[0].output_tail.contains("2000"),
            "the end survives: {}",
            shell[0].output_tail
        );
    }

    /// With nothing dropped, the interleaved live tail is what the record shows.
    #[test]
    fn an_intact_live_tail_is_kept_for_the_completed_record() {
        let store = UserShellStore::default();
        let session = SessionId::new("s1");
        let id = UserShellId::new("ush-intact");
        store.begin(
            &session,
            id.clone(),
            "echo one".into(),
            "/repo".into(),
            CancellationToken::new(),
            CancellationToken::new(),
        );
        store.append_output(&session, &id, "out\nerr\n");
        store.finish(&session, &id, Some(0), "success", None, None);
        let shell = store.snapshot(&session);
        assert_eq!(shell[0].output_tail, "out\nerr\n");
    }

    #[test]
    fn stale_cancel_id_never_reaches_a_newer_execution() {
        let store = UserShellStore::default();
        let session = SessionId::new("s1");
        let old = UserShellId::new("ush-old");
        let new = UserShellId::new("ush-new");
        store.begin(
            &session,
            old.clone(),
            "sleep 1".into(),
            "/repo".into(),
            CancellationToken::new(),
            CancellationToken::new(),
        );
        store.finish(&session, &old, Some(0), "success", None, None);
        store.begin(
            &session,
            new.clone(),
            "sleep 2".into(),
            "/repo".into(),
            CancellationToken::new(),
            CancellationToken::new(),
        );
        assert!(store.cancel_token(&session, &old).is_none());
        assert!(store.cancel_token(&session, &new).is_some());
    }

    #[test]
    fn snapshot_lists_history_then_active() {
        let store = UserShellStore::default();
        let session = SessionId::new("s1");
        let a = UserShellId::new("a");
        store.begin(
            &session,
            a.clone(),
            "echo one".into(),
            "/repo".into(),
            CancellationToken::new(),
            CancellationToken::new(),
        );
        store.append_output(&session, &a, "one\n");
        store.finish(&session, &a, Some(0), "success", None, None);
        let b = UserShellId::new("b");
        store.begin(
            &session,
            b.clone(),
            "sleep 9".into(),
            "/repo".into(),
            CancellationToken::new(),
            CancellationToken::new(),
        );
        let snap = store.snapshot(&session);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].status, "success");
        assert_eq!(snap[0].output_tail, "one\n");
        assert_eq!(snap[1].status, "running");
        assert_eq!(snap[1].command, "sleep 9");
    }

    #[test]
    fn a_capture_is_advertised_only_when_every_copy_reports_completion() {
        let dir = tempfile::tempdir().unwrap();
        for stream in ["stdout", "stderr", "combined"] {
            std::fs::write(
                dir.path().join(format!("{stream}.meta.json")),
                format!(r#"{{"stream":"{stream}","bytes":10,"complete":true}}"#),
            )
            .unwrap();
        }
        assert_eq!(
            complete_output_directory(dir.path()).as_deref(),
            Some(dir.path().to_string_lossy().as_ref())
        );
        std::fs::remove_file(dir.path().join("combined.meta.json")).unwrap();
        assert!(
            complete_output_directory(dir.path()).is_none(),
            "a missing combined copy is never advertised as complete"
        );
        std::fs::write(
            dir.path().join("stderr.meta.json"),
            r#"{"stream":"stderr","bytes":0,"complete":false,"error":"disk full"}"#,
        )
        .unwrap();
        assert!(
            complete_output_directory(dir.path()).is_none(),
            "an unfinished capture is never advertised as complete"
        );
    }

    #[test]
    fn a_new_execution_never_removes_captures_the_session_still_references() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new("s1");
        let session_dir = root.path().join("shell-output").join(session.as_str());
        for index in 0..40 {
            let id = UserShellId::new(format!("ush-{index}"));
            let directory = output_directory(root.path(), &session, &id);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("combined.log"), b"x").unwrap();
        }
        let current = UserShellId::new("current");
        prepare_output_directory(root.path(), &session, &current);
        let entries = std::fs::read_dir(&session_dir).unwrap().count();
        assert_eq!(
            entries, 40,
            "a finished record still names its capture, so starting another command must not delete it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_prepared_capture_directory_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new("s1");
        let id = UserShellId::new("ush-private");
        let directory = prepare_output_directory(root.path(), &session, &id);
        let session_dir = root.path().join("shell-output").join(session.as_str());
        let mode = std::fs::metadata(&session_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "session captures are owner-only");
        assert!(
            !directory.exists(),
            "the execution crate creates the leaf with its own mode"
        );
    }

    #[test]
    fn a_malformed_session_id_cannot_escape_the_capture_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"x").unwrap();
        let hostile = SessionId::new("../../outside");
        remove_session_output(root.path(), &hostile);
        assert!(
            outside.join("keep").exists(),
            "cleanup never follows a session id out of shell-output"
        );
        let path = session_output_directory(root.path(), &hostile);
        assert!(
            path.starts_with(root.path().join("shell-output")),
            "a malformed id is encoded, not joined: {}",
            path.display()
        );
    }

    #[test]
    fn deleting_a_session_removes_only_its_own_captures() {
        let root = tempfile::tempdir().unwrap();
        let deleted = SessionId::new("s-deleted");
        let kept = SessionId::new("s-kept");
        for session in [&deleted, &kept] {
            let id = UserShellId::new("ush-1");
            let directory = output_directory(root.path(), session, &id);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("combined.log"), b"x").unwrap();
        }
        remove_session_output(root.path(), &deleted);
        assert!(
            !root
                .path()
                .join("shell-output")
                .join(deleted.as_str())
                .exists(),
            "the deleted session's captures are reclaimed"
        );
        assert!(
            root.path()
                .join("shell-output")
                .join(kept.as_str())
                .exists(),
            "another session's captures are untouched"
        );
    }
}
