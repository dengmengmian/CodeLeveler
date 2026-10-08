//! Owned-child policy used by the Web project manager. The manager retains
//! the child monitor, kill signal, registry and project status.
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use leveler_local_transport::{LocalSocketRuntimeClient, TransportError};
use leveler_project::Layout;
use tokio::process::{Child, Command};

use crate::client::{HandoffUi, reconcile_runtime_generation};
use crate::lifecycle_state::RuntimeLifecycleState;

#[derive(Debug, thiserror::Error)]
pub enum OwnedRuntimeError {
    #[error("invalid daemon readiness JSON: {0}")]
    InvalidReadyJson(serde_json::Error),
    #[error("daemon readiness JSON lacks socket")]
    MissingSocket,
    #[error("daemon exited during startup: {0}")]
    ChildExited(ExitStatus),
    #[error("timed out waiting for daemon readiness")]
    Timeout,
}

/// The shell supplies the executable and its readiness-file name. The host
/// owns the startup sequence; Web keeps ownership of any child it launched.
pub struct OwnedRuntimeLaunch<'a> {
    pub executable: &'a Path,
    pub ready_path: &'a Path,
}

pub struct OwnedRuntime {
    pub client: LocalSocketRuntimeClient,
    pub child: Option<Child>,
}

#[derive(Debug, thiserror::Error)]
pub enum EnsureOwnedError {
    #[error("no running daemon at {} and no executable to start one", socket.display())]
    NoLauncher { socket: PathBuf },
    #[error("failed to spawn daemon: {0}")]
    Spawn(std::io::Error),
    #[error("daemon did not become ready: {0}")]
    Ready(OwnedRuntimeError),
    #[error("daemon became ready but could not be connected: {0}")]
    Connect(TransportError),
    /// A runtime IS serving the endpoint, but not this generation — and it
    /// could not be reconciled with it. This is the variant that stops a Web
    /// server from silently attaching to a runtime it does not match.
    #[error("{state_name}: {reason}")]
    Generation {
        state: RuntimeLifecycleState,
        state_name: &'static str,
        reason: String,
    },
}

/// The lifecycle state a failure to obtain an owned runtime represents.
impl EnsureOwnedError {
    /// The shared vocabulary name for this failure. A launch failure (no
    /// executable, spawn error, readiness timeout) is `Unresponsive`: something
    /// was supposed to answer and did not.
    pub fn lifecycle_state(&self) -> RuntimeLifecycleState {
        match self {
            Self::Generation { state, .. } => *state,
            Self::NoLauncher { .. } | Self::Spawn(_) | Self::Ready(_) | Self::Connect(_) => {
                RuntimeLifecycleState::Unresponsive
            }
        }
    }
}

/// Web's attach-or-start path, generation-aware.
///
/// The compatibility decision is NOT made here. This calls
/// [`reconcile_runtime_generation`], the same function the terminal and the
/// Desktop bridge use, so the Web cannot reach a different verdict about the
/// same runtime than they do. What is Web-specific is only what happens when the
/// answer is "start one": this path keeps the child handle, because an owned
/// child has to be reaped, monitored and asked before it is stopped.
///
/// `layout` is the generation this project expects (its build, its
/// configuration sources, its endpoint). `repo` is the workspace the spawned
/// daemon should serve, kept explicit because a workspace-free layout has none.
/// `socket` is the endpoint to use, kept separate so a caller may point at an
/// equivalent path.
pub async fn ensure_owned_runtime(
    repo: &Path,
    layout: &Layout,
    socket: &Path,
    launcher: Option<OwnedRuntimeLaunch<'_>>,
    ui: Arc<dyn HandoffUi>,
) -> Result<OwnedRuntime, EnsureOwnedError> {
    match reconcile_runtime_generation(socket, layout, ui.as_ref()).await {
        Ok(Some(client)) => {
            return Ok(OwnedRuntime {
                client,
                child: None,
            });
        }
        Ok(None) => {}
        Err(error) => {
            let state = error
                .downcast_ref::<crate::EnsureError>()
                .map(RuntimeLifecycleState::from_ensure_error)
                .unwrap_or(RuntimeLifecycleState::Unresponsive);
            return Err(EnsureOwnedError::Generation {
                state,
                state_name: state.as_str(),
                reason: error.to_string(),
            });
        }
    }
    let Some(launcher) = launcher else {
        return Err(EnsureOwnedError::NoLauncher {
            socket: socket.to_path_buf(),
        });
    };
    let repo = layout.require_workspace().unwrap_or(repo);
    let _ = std::fs::remove_file(launcher.ready_path);
    let mut child = spawn_owned_runtime(launcher.executable, repo, launcher.ready_path)
        .map_err(EnsureOwnedError::Spawn)?;
    let ready_socket = match wait_for_owned_runtime(launcher.ready_path, &mut child).await {
        Ok(socket) => socket,
        Err(error) => {
            let _ = child.start_kill();
            return Err(EnsureOwnedError::Ready(error));
        }
    };
    let _ = std::fs::remove_file(launcher.ready_path);
    let client = connect_owned_runtime(&ready_socket)
        .await
        .map_err(|error| {
            let _ = child.start_kill();
            EnsureOwnedError::Connect(error)
        })?;
    Ok(OwnedRuntime {
        client,
        child: Some(child),
    })
}

/// Direct attach with the transport's original timing and error semantics.
pub async fn connect_existing_runtime(
    path: &Path,
) -> Result<LocalSocketRuntimeClient, TransportError> {
    LocalSocketRuntimeClient::connect(path).await
}

pub fn spawn_owned_runtime(exe: &Path, repo: &Path, ready_path: &Path) -> std::io::Result<Child> {
    Command::new(exe)
        .arg("--repo")
        .arg(repo)
        .arg("serve")
        .arg("--ready-json")
        .arg(ready_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Deliberately NOT `kill_on_drop`. The child handle this manager holds
        // is a handle to a PROCESS, not to the work the runtime owns: dropping
        // it used to SIGKILL the daemon, so closing the Web UI destroyed a
        // turn that happened to be running. A daemon is a long-lived owner
        // with its own idle eviction, exactly like one started by the terminal,
        // so an exiting shell releases its handle and leaves the runtime alone.
        // Stopping a runtime goes through the runtime's own decision
        // (`ProjectManager::retire_owned`), never through this drop.
        .spawn()
}

/// Preserve Web's twenty-second wait and early child-exit diagnosis.
pub async fn wait_for_owned_runtime(
    ready_path: &Path,
    child: &mut Child,
) -> Result<PathBuf, OwnedRuntimeError> {
    const READY_TIMEOUT: Duration = Duration::from_secs(20);
    const POLL: Duration = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    loop {
        if let Ok(bytes) = std::fs::read(ready_path) {
            let ready: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(OwnedRuntimeError::InvalidReadyJson)?;
            let socket = ready
                .get("socket")
                .and_then(|value| value.as_str())
                .ok_or(OwnedRuntimeError::MissingSocket)?;
            return Ok(PathBuf::from(socket));
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(OwnedRuntimeError::ChildExited(status));
        }
        if waited >= READY_TIMEOUT {
            return Err(OwnedRuntimeError::Timeout);
        }
        tokio::time::sleep(POLL).await;
        waited += POLL;
    }
}

/// Preserve Web's five connection attempts with a sleep after each failure.
pub async fn connect_owned_runtime(
    socket: &Path,
) -> Result<LocalSocketRuntimeClient, TransportError> {
    let mut last = None;
    for _ in 0..5 {
        match LocalSocketRuntimeClient::connect(socket).await {
            Ok(client) => return Ok(client),
            Err(error) => last = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(last.expect("at least one attempt ran"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sleeping_child() -> Child {
        Command::new("sh")
            .arg("-c")
            .arg("sleep 2")
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn no_runtime_and_no_launcher_reports_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("missing.sock");
        let layout = leveler_project::Layout::ephemeral(dir.path().to_path_buf(), None, dir.path());
        let result = ensure_owned_runtime(
            dir.path(),
            &layout,
            &socket,
            None,
            crate::NonInteractiveHandoffUi::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(EnsureOwnedError::NoLauncher { socket: missing }) if missing == socket
        ));
    }

    #[tokio::test]
    async fn ready_file_reports_socket_and_rejects_invalid_contract() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ready.json");
        let mut child = sleeping_child();
        std::fs::write(&path, br#"{"socket":"/tmp/codeleveler.sock"}"#).unwrap();
        assert_eq!(
            wait_for_owned_runtime(&path, &mut child).await.unwrap(),
            PathBuf::from("/tmp/codeleveler.sock")
        );
        std::fs::write(&path, b"{").unwrap();
        assert!(matches!(
            wait_for_owned_runtime(&path, &mut child).await,
            Err(OwnedRuntimeError::InvalidReadyJson(_))
        ));
        std::fs::write(&path, b"{}").unwrap();
        assert!(matches!(
            wait_for_owned_runtime(&path, &mut child).await,
            Err(OwnedRuntimeError::MissingSocket)
        ));
        let _ = child.start_kill();
    }

    #[tokio::test]
    async fn child_exit_fails_before_ready_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("sh").arg("-c").arg("exit 7").spawn().unwrap();
        let result = wait_for_owned_runtime(&dir.path().join("missing.json"), &mut child).await;
        assert!(matches!(
            result,
            Err(OwnedRuntimeError::ChildExited(status)) if status.code() == Some(7)
        ));
    }
}
