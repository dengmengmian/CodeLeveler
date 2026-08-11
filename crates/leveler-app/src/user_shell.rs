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
use std::sync::Mutex;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use leveler_client_protocol::UiUserShell;
use leveler_core::{SessionId, UserShellId};
use leveler_engine::EngineEvent;
use leveler_execution::{OutputStream, ProcessError};

/// Bounded live/completed output tail per execution. Big enough for a useful
/// Details view, small enough to ride snapshots and stay in memory.
pub(crate) const OUTPUT_TAIL_CAP: usize = 64 * 1024;
/// Completed executions kept per session for reconnect/history.
const HISTORY_CAP: usize = 16;

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
}

#[derive(Debug)]
struct ActiveShell {
    id: UserShellId,
    command: String,
    cwd: String,
    started: Instant,
    cancel: CancellationToken,
    tail: OutputTail,
}

#[derive(Debug, Default)]
struct SessionShells {
    active: Option<ActiveShell>,
    /// Completed executions, oldest first.
    history: VecDeque<UiUserShell>,
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
    ) {
        let mut inner = self.inner.lock().unwrap();
        let shells = inner.entry(session_id.clone()).or_default();
        shells.active = Some(ActiveShell {
            id,
            command,
            cwd,
            started: Instant::now(),
            cancel,
            tail: OutputTail::default(),
        });
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

    /// Move the active execution to history with its terminal facts. Returns
    /// the total runtime. A stale id (already finished) is a no-op `None`.
    pub fn finish(
        &self,
        session_id: &SessionId,
        id: &UserShellId,
        exit_code: Option<i32>,
        status: &str,
    ) -> Option<u64> {
        let mut inner = self.inner.lock().unwrap();
        let shells = inner.get_mut(session_id)?;
        if shells.active.as_ref().is_none_or(|a| &a.id != id) {
            return None;
        }
        let active = shells.active.take().unwrap();
        let duration_ms = active.started.elapsed().as_millis() as u64;
        let (output_tail, output_truncated) = active.tail.snapshot();
        shells.history.push_back(UiUserShell {
            id: active.id,
            command: active.command,
            cwd: active.cwd,
            status: status.to_string(),
            elapsed_secs: duration_ms / 1000,
            exit_code,
            output_tail,
            output_truncated,
        });
        while shells.history.len() > HISTORY_CAP {
            shells.history.pop_front();
        }
        Some(duration_ms)
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
            });
        }
        out
    }
}

/// Terminal status wording on the wire (`UserShellFinished.status`).
pub(crate) fn terminal_status(
    result: &Result<leveler_execution::ProcessOutput, ProcessError>,
) -> &'static str {
    match result {
        Ok(output) if output.exit_code == Some(0) => "success",
        Ok(_) => "failed",
        Err(ProcessError::Cancelled) => "cancelled",
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
    fn tail_is_bounded_and_flags_truncation() {
        let mut tail = OutputTail::default();
        tail.push(&"x".repeat(OUTPUT_TAIL_CAP + 10));
        let (text, truncated) = tail.snapshot();
        assert_eq!(text.len(), OUTPUT_TAIL_CAP);
        assert!(truncated);
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
        );
        store.finish(&session, &old, Some(0), "success");
        store.begin(
            &session,
            new.clone(),
            "sleep 2".into(),
            "/repo".into(),
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
        );
        store.append_output(&session, &a, "one\n");
        store.finish(&session, &a, Some(0), "success");
        let b = UserShellId::new("b");
        store.begin(
            &session,
            b.clone(),
            "sleep 9".into(),
            "/repo".into(),
            CancellationToken::new(),
        );
        let snap = store.snapshot(&session);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].status, "success");
        assert_eq!(snap[0].output_tail, "one\n");
        assert_eq!(snap[1].status, "running");
        assert_eq!(snap[1].command, "sleep 9");
    }
}
