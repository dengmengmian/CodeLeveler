use super::*;
use sha2::{Digest, Sha256};
use std::os::windows::io::OwnedHandle;

/// A logical runtime path is an identity, never a named-pipe filesystem path.
/// Hash UTF-16 code units to preserve non-Unicode Windows paths losslessly.
pub(super) fn pipe_name(path: &Path) -> std::io::Result<String> {
    use std::os::windows::ffi::OsStrExt;
    let sid = leveler_win_local_ipc::current_sid()?;
    let mut digest = Sha256::new();
    digest.update(b"CodeLeveler-local-endpoint-v1");
    digest.update((sid.len() as u64).to_le_bytes());
    digest.update(sid.as_bytes());
    for unit in path.as_os_str().encode_wide() {
        digest.update(unit.to_le_bytes());
    }
    Ok(format!(r"\\.\pipe\CodeLeveler-{:x}", digest.finalize()))
}

pub(super) async fn connect_windows(path: &Path) -> Result<NamedPipeClient, TransportError> {
    let name = pipe_name(path)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match ClientOptions::new().open(&name) {
            Ok(client) => {
                leveler_win_local_ipc::verify_server(&client)?;
                return Ok(client);
            }
            // ERROR_PIPE_BUSY is a transient lack of available listener instances.
            Err(error)
                if error.raw_os_error() == Some(231) && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Runtime ownership is the retained first pipe instance, not a second lock.
pub struct LocalSocketServer {
    path: PathBuf,
    name: String,
    listener: NamedPipeServer,
    // This duplicate retains the initial instance after its connection finishes.
    // It does not listen separately, so real clients cannot enter a black hole.
    _ownership: OwnedHandle,
    runtime: Arc<dyn LocalRuntimeService>,
    local_waiters: LocalWaiters,
}
impl LocalSocketServer {
    pub async fn bind(
        path: impl AsRef<Path>,
        runtime: Arc<dyn LocalRuntimeService>,
    ) -> Result<Self, TransportError> {
        Self::bind_with_waiters(path, runtime, LocalWaiters::new()).await
    }
    pub async fn bind_with_waiters(
        path: impl AsRef<Path>,
        runtime: Arc<dyn LocalRuntimeService>,
        local_waiters: LocalWaiters,
    ) -> Result<Self, TransportError> {
        let path = path.as_ref().to_path_buf();
        let name = pipe_name(&path)?;
        let listener = leveler_win_local_ipc::create_server(&name, true).map_err(|error| {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                TransportError::AlreadyRunning(path.display().to_string())
            } else {
                error.into()
            }
        })?;
        let ownership = leveler_win_local_ipc::retain_instance(&listener)?;
        Ok(Self {
            path,
            name,
            listener,
            _ownership: ownership,
            runtime,
            local_waiters,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn local_waiters(&self) -> LocalWaiters {
        self.local_waiters.clone()
    }
    pub async fn serve(mut self, shutdown: CancellationToken) -> Result<(), TransportError> {
        let child_shutdown = shutdown.child_token();
        let mut tasks = tokio::task::JoinSet::new();
        let result = loop {
            tokio::select! {
                _ = shutdown.cancelled() => break Ok(()),
                accepted = self.listener.connect() => {
                    if let Err(error) = accepted { break Err(error.into()); }
                    // Replace before dispatch: the pipe namespace and ownership
                    // remain present across client close/reconnect.
                    let next = match leveler_win_local_ipc::create_server(&self.name, false) {
                        Ok(next) => next,
                        Err(error) => break Err(error.into()),
                    };
                    let stream = std::mem::replace(&mut self.listener, next);
                    if let Err(error) = leveler_win_local_ipc::verify_client(&stream) {
                        tracing::warn!(%error, "rejected local pipe peer");
                        continue;
                    }
                    let runtime = self.runtime.clone();
                    let connection_shutdown = child_shutdown.clone();
                    let waiters = self.local_waiters.clone();
                    tasks.spawn(async move {
                        if let Err(error) = handle_connection(stream, runtime, connection_shutdown, waiters, true).await {
                            tracing::debug!(%error, "local pipe connection ended");
                        }
                    });
                }
            }
        };
        child_shutdown.cancel();
        while tasks.join_next().await.is_some() {}
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_identity_is_deterministic_unicode_safe_and_bounded() {
        let path = PathBuf::from(r"C:\Users\用户\仓库\runtime.sock");
        let endpoint = pipe_name(&path).unwrap();
        assert_eq!(endpoint, pipe_name(&path).unwrap());
        assert_ne!(
            endpoint,
            pipe_name(&path.with_file_name("other.sock")).unwrap()
        );
        assert!(endpoint.starts_with(r"\\.\pipe\CodeLeveler-"));
        assert!(endpoint.is_ascii());
        let long = PathBuf::from(format!(r"C:\用户\{}\runtime.sock", "长路径".repeat(500)));
        assert!(pipe_name(&long).unwrap().len() < 256);
    }
}
