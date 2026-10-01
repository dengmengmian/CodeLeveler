//! Owned-child policy used by the Web project manager. The manager retains
//! the child monitor, kill signal, registry and project status.
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use leveler_local_transport::{LocalSocketRuntimeClient, TransportError};
use tokio::process::{Child, Command};

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
}

/// Web's attach-or-start path. The direct probe and retry timing are kept
/// separate from the TUI's short default-daemon probe. On success the caller
/// receives the child only when this call started it, so it can retain its
/// existing monitor, removal and shutdown policy.
pub async fn ensure_owned_runtime(
    repo: &Path,
    socket: &Path,
    launcher: Option<OwnedRuntimeLaunch<'_>>,
) -> Result<OwnedRuntime, EnsureOwnedError> {
    if let Ok(client) = connect_existing_runtime(socket).await {
        return Ok(OwnedRuntime {
            client,
            child: None,
        });
    }
    let Some(launcher) = launcher else {
        return Err(EnsureOwnedError::NoLauncher {
            socket: socket.to_path_buf(),
        });
    };
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
        .kill_on_drop(true)
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
        let result = ensure_owned_runtime(dir.path(), &socket, None).await;
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
