//! Trusted local transport between CodeLeveler UI clients and the runtime.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use leveler_client_protocol::{
    ClientCommand, ClientError, CommandEnvelope, InteractiveRuntimeClient, ModelRef,
    PermissionProfile, ProtocolEnvelope, ProtocolError, RuntimeEvent, SessionId, UiSessionSnapshot,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Everything the daemon needs to create a new interactive session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub goal: String,
    pub model: Option<ModelRef>,
    pub mode: PermissionProfile,
}

/// Initial client state returned atomically with session creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBootstrap {
    pub session: UiSessionSnapshot,
    pub context_window: u32,
}

/// Runtime operations needed in addition to the stable interactive client
/// contract when a daemon owns session creation.
#[async_trait]
pub trait LocalRuntimeService: InteractiveRuntimeClient {
    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError>;
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("local transport io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("local transport json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("local transport protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("local runtime is already listening at {0}")]
    AlreadyRunning(String),
    #[error("local transport frame exceeds the {max_bytes}-byte limit: {actual_bytes} bytes")]
    FrameTooLarge {
        max_bytes: usize,
        actual_bytes: usize,
    },
    #[error("local transport is unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
enum WireRequest {
    Ping,
    Send(ClientCommand),
    Deliver(ProtocolEnvelope<CommandEnvelope>),
    Snapshot { session_id: SessionId },
    CreateSession(CreateSessionRequest),
    Subscribe { session_id: Option<SessionId> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
enum WireResponse {
    Ack,
    Snapshot(UiSessionSnapshot),
    SessionCreated(SessionBootstrap),
    Event(RuntimeEvent),
    Error(WireError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireError {
    message: String,
    session_id: Option<SessionId>,
}

impl From<ClientError> for WireError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::SessionNotFound(session_id) => Self {
                message: format!("session not found: {session_id}"),
                session_id: Some(session_id),
            },
            ClientError::Runtime(message) => Self {
                message,
                session_id: None,
            },
        }
    }
}

impl WireError {
    fn into_client_error(self) -> ClientError {
        match self.session_id {
            Some(session_id) => ClientError::SessionNotFound(session_id),
            None => ClientError::Runtime(self.message),
        }
    }
}

const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::sync::Mutex;
    use std::time::Duration;

    use serde::de::DeserializeOwned;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::broadcast;

    use super::*;

    async fn write_frame<T: Serialize>(
        writer: &mut (impl AsyncWrite + Unpin),
        value: &ProtocolEnvelope<T>,
    ) -> Result<(), TransportError> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(TransportError::FrameTooLarge {
                max_bytes: MAX_FRAME_BYTES,
                actual_bytes: bytes.len(),
            });
        }
        writer.write_u32(bytes.len() as u32).await?;
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn read_frame<T: DeserializeOwned>(
        reader: &mut (impl AsyncRead + Unpin),
    ) -> Result<ProtocolEnvelope<T>, TransportError> {
        let length = reader.read_u32().await? as usize;
        if length > MAX_FRAME_BYTES {
            return Err(TransportError::FrameTooLarge {
                max_bytes: MAX_FRAME_BYTES,
                actual_bytes: length,
            });
        }
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn send_response(
        stream: &mut UnixStream,
        response: WireResponse,
    ) -> Result<(), TransportError> {
        write_frame(stream, &ProtocolEnvelope::wrap(response)).await
    }

    async fn send_result(
        stream: &mut UnixStream,
        result: Result<WireResponse, ClientError>,
    ) -> Result<(), TransportError> {
        let response = result.unwrap_or_else(|error| WireResponse::Error(error.into()));
        send_response(stream, response).await
    }

    async fn handle_connection(
        mut stream: UnixStream,
        runtime: Arc<dyn LocalRuntimeService>,
        shutdown: CancellationToken,
    ) -> Result<(), TransportError> {
        let request = read_frame::<WireRequest>(&mut stream).await?.into_body()?;
        match request {
            WireRequest::Ping => send_response(&mut stream, WireResponse::Ack).await,
            WireRequest::Send(command) => {
                if matches!(&command, ClientCommand::Quit) {
                    return send_result(
                        &mut stream,
                        Err(ClientError::Runtime(
                            "only the runtime owner may shut down the daemon".to_string(),
                        )),
                    )
                    .await;
                }
                send_result(
                    &mut stream,
                    runtime.send(command).await.map(|_| WireResponse::Ack),
                )
                .await
            }
            WireRequest::Deliver(envelope) => {
                if matches!(&envelope.body.command, ClientCommand::Quit) {
                    return send_result(
                        &mut stream,
                        Err(ClientError::Runtime(
                            "only the runtime owner may shut down the daemon".to_string(),
                        )),
                    )
                    .await;
                }
                send_result(
                    &mut stream,
                    runtime
                        .deliver_protocol(envelope)
                        .await
                        .map(|_| WireResponse::Ack),
                )
                .await
            }
            WireRequest::Snapshot { session_id } => {
                send_result(
                    &mut stream,
                    runtime
                        .snapshot(&session_id)
                        .await
                        .map(WireResponse::Snapshot),
                )
                .await
            }
            WireRequest::CreateSession(request) => {
                send_result(
                    &mut stream,
                    runtime
                        .create_session(request)
                        .await
                        .map(WireResponse::SessionCreated),
                )
                .await
            }
            WireRequest::Subscribe { session_id } => {
                send_response(&mut stream, WireResponse::Ack).await?;
                let mut events = match session_id {
                    Some(session_id) => runtime.subscribe_session(&session_id),
                    None => runtime.subscribe(),
                };
                loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => return Ok(()),
                        event = events.recv() => match event {
                            Ok(event) => send_response(&mut stream, WireResponse::Event(event)).await?,
                            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                tracing::warn!(skipped, "local socket event subscriber lagged");
                                // Force a reconnect. Session-scoped clients
                                // resync from a fresh snapshot after reconnect,
                                // so canonical state is never silently skipped.
                                return Ok(());
                            }
                            Err(broadcast::error::RecvError::Closed) => return Ok(()),
                        }
                    }
                }
            }
        }
    }

    /// A bound local runtime server.
    pub struct LocalSocketServer {
        path: PathBuf,
        socket_device: u64,
        socket_inode: u64,
        listener: UnixListener,
        runtime: Arc<dyn LocalRuntimeService>,
    }

    impl Drop for LocalSocketServer {
        fn drop(&mut self) {
            let metadata = match std::fs::symlink_metadata(&self.path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) => {
                    tracing::warn!(%error, path = %self.path.display(), "failed to inspect local socket");
                    return;
                }
            };
            if !metadata.file_type().is_socket()
                || metadata.dev() != self.socket_device
                || metadata.ino() != self.socket_inode
            {
                tracing::warn!(path = %self.path.display(), "local socket path was replaced; preserving replacement");
                return;
            }
            if let Err(error) = std::fs::remove_file(&self.path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(%error, path = %self.path.display(), "failed to remove local socket");
            }
        }
    }

    impl LocalSocketServer {
        pub async fn bind(
            path: impl AsRef<Path>,
            runtime: Arc<dyn LocalRuntimeService>,
        ) -> Result<Self, TransportError> {
            let path = path.as_ref().to_path_buf();
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            if let Ok(metadata) = tokio::fs::symlink_metadata(&path).await {
                if !metadata.file_type().is_socket() {
                    return Err(TransportError::Unavailable(format!(
                        "refusing to replace non-socket path {}",
                        path.display()
                    )));
                }
                if UnixStream::connect(&path).await.is_ok() {
                    return Err(TransportError::AlreadyRunning(path.display().to_string()));
                }
                tokio::fs::remove_file(&path).await?;
            }
            let listener = UnixListener::bind(&path)?;
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
            let metadata = tokio::fs::symlink_metadata(&path).await?;
            Ok(Self {
                path,
                socket_device: metadata.dev(),
                socket_inode: metadata.ino(),
                listener,
                runtime,
            })
        }

        pub fn path(&self) -> &Path {
            &self.path
        }

        pub async fn serve(self, shutdown: CancellationToken) -> Result<(), TransportError> {
            let child_shutdown = shutdown.child_token();
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    accepted = self.listener.accept() => {
                        let (stream, _) = accepted?;
                        let runtime = self.runtime.clone();
                        let connection_shutdown = child_shutdown.clone();
                        tasks.spawn(async move {
                            if let Err(error) = handle_connection(stream, runtime, connection_shutdown).await {
                                tracing::debug!(%error, "local socket connection ended");
                            }
                        });
                    }
                }
            }
            child_shutdown.cancel();
            while tasks.join_next().await.is_some() {}
            Ok(())
        }
    }

    /// Socket-backed implementation consumed by the TUI.
    pub struct LocalSocketRuntimeClient {
        path: PathBuf,
        events: broadcast::Sender<RuntimeEvent>,
        session_events:
            Arc<Mutex<std::collections::HashMap<SessionId, broadcast::Sender<RuntimeEvent>>>>,
        shutdown: CancellationToken,
    }

    impl LocalSocketRuntimeClient {
        pub async fn connect(path: impl AsRef<Path>) -> Result<Self, TransportError> {
            let path = path.as_ref().to_path_buf();
            let stream = open_subscription(&path, None).await?;
            let (events, _) = broadcast::channel(2048);
            let session_events = Arc::new(Mutex::new(std::collections::HashMap::new()));
            let shutdown = CancellationToken::new();
            tokio::spawn(subscription_loop(
                path.clone(),
                stream,
                events.clone(),
                None,
                shutdown.clone(),
            ));
            Ok(Self {
                path,
                events,
                session_events,
                shutdown,
            })
        }

        async fn request(&self, request: WireRequest) -> Result<WireResponse, TransportError> {
            request_path(&self.path, request).await
        }

        async fn ensure_session_subscription(
            &self,
            session_id: &SessionId,
        ) -> Result<broadcast::Sender<RuntimeEvent>, TransportError> {
            if let Some(events) = self.session_events.lock().unwrap().get(session_id).cloned() {
                return Ok(events);
            }
            let stream = open_subscription(&self.path, Some(session_id.clone())).await?;
            let (events, _) = broadcast::channel(2048);
            self.session_events
                .lock()
                .unwrap()
                .insert(session_id.clone(), events.clone());
            tokio::spawn(subscription_loop(
                self.path.clone(),
                stream,
                events.clone(),
                Some(session_id.clone()),
                self.shutdown.clone(),
            ));
            Ok(events)
        }

        pub async fn create_session(
            &self,
            request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            match self
                .request(WireRequest::CreateSession(request))
                .await
                .map_err(transport_client_error)?
            {
                WireResponse::SessionCreated(bootstrap) => {
                    self.ensure_session_subscription(&bootstrap.session.id)
                        .await
                        .map_err(transport_client_error)?;
                    Ok(bootstrap)
                }
                WireResponse::Error(error) => Err(error.into_client_error()),
                response => Err(unexpected_response(response)),
            }
        }
    }

    impl Drop for LocalSocketRuntimeClient {
        fn drop(&mut self) {
            self.shutdown.cancel();
        }
    }

    #[async_trait]
    impl InteractiveRuntimeClient for LocalSocketRuntimeClient {
        async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
            if let ClientCommand::OpenSession { session_id }
            | ClientCommand::OpenSessionFor { session_id, .. } = &command
            {
                // Subscribe before asking the daemon for the switch snapshot,
                // closing the snapshot→subscribe gap for an already-running
                // target session.
                self.ensure_session_subscription(session_id)
                    .await
                    .map_err(transport_client_error)?;
            }
            match self
                .request(WireRequest::Send(command))
                .await
                .map_err(transport_client_error)?
            {
                WireResponse::Ack => Ok(()),
                WireResponse::Error(error) => Err(error.into_client_error()),
                response => Err(unexpected_response(response)),
            }
        }

        async fn deliver(&self, envelope: CommandEnvelope) -> Result<(), ClientError> {
            match self
                .request(WireRequest::Deliver(ProtocolEnvelope::wrap(envelope)))
                .await
                .map_err(transport_client_error)?
            {
                WireResponse::Ack => Ok(()),
                WireResponse::Error(error) => Err(error.into_client_error()),
                response => Err(unexpected_response(response)),
            }
        }

        fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
            self.events.subscribe()
        }

        fn subscribe_session(&self, session_id: &SessionId) -> broadcast::Receiver<RuntimeEvent> {
            if let Some(events) = self.session_events.lock().unwrap().get(session_id).cloned() {
                return events.subscribe();
            }
            let (events, receiver) = broadcast::channel(2048);
            self.session_events
                .lock()
                .unwrap()
                .insert(session_id.clone(), events.clone());
            let path = self.path.clone();
            let session_id = session_id.clone();
            let shutdown = self.shutdown.clone();
            tokio::spawn(async move {
                loop {
                    match open_subscription(&path, Some(session_id.clone())).await {
                        Ok(stream) => {
                            subscription_loop(path, stream, events, Some(session_id), shutdown)
                                .await;
                            return;
                        }
                        Err(_) => tokio::select! {
                            _ = shutdown.cancelled() => return,
                            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                        },
                    }
                }
            });
            receiver
        }

        async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
            // Establish the event stream first. Events racing the snapshot are
            // buffered in the local broadcast channel and applied afterwards.
            self.ensure_session_subscription(session_id)
                .await
                .map_err(transport_client_error)?;
            match self
                .request(WireRequest::Snapshot {
                    session_id: session_id.clone(),
                })
                .await
                .map_err(transport_client_error)?
            {
                WireResponse::Snapshot(snapshot) => Ok(snapshot),
                WireResponse::Error(error) => Err(error.into_client_error()),
                response => Err(unexpected_response(response)),
            }
        }
    }

    async fn request_path(
        path: &Path,
        request: WireRequest,
    ) -> Result<WireResponse, TransportError> {
        let mut stream = UnixStream::connect(path).await?;
        write_frame(&mut stream, &ProtocolEnvelope::wrap(request)).await?;
        read_frame::<WireResponse>(&mut stream)
            .await?
            .into_body()
            .map_err(Into::into)
    }

    async fn open_subscription(
        path: &Path,
        session_id: Option<SessionId>,
    ) -> Result<UnixStream, TransportError> {
        let mut stream = UnixStream::connect(path).await?;
        write_frame(
            &mut stream,
            &ProtocolEnvelope::wrap(WireRequest::Subscribe { session_id }),
        )
        .await?;
        match read_frame::<WireResponse>(&mut stream).await?.into_body()? {
            WireResponse::Ack => Ok(stream),
            WireResponse::Error(error) => Err(TransportError::Unavailable(error.message)),
            response => Err(TransportError::Unavailable(format!(
                "unexpected subscription response: {response:?}"
            ))),
        }
    }

    async fn subscription_loop(
        path: PathBuf,
        mut stream: UnixStream,
        events: broadcast::Sender<RuntimeEvent>,
        session_id: Option<SessionId>,
        shutdown: CancellationToken,
    ) {
        loop {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    response = read_frame::<WireResponse>(&mut stream) => match response {
                        Ok(envelope) => match envelope.into_body() {
                            Ok(WireResponse::Event(event)) => { let _ = events.send(event); }
                            Ok(WireResponse::Error(error)) => {
                                let _ = events.send(RuntimeEvent::Notification {
                                    level: leveler_client_protocol::NotificationLevel::Error,
                                    message: error.message,
                                });
                            }
                            Ok(_) => {}
                            Err(error) => tracing::warn!(%error, "local event protocol mismatch"),
                        },
                        Err(_) => break,
                    }
                }
            }

            while !shutdown.is_cancelled() {
                match open_subscription(&path, session_id.clone()).await {
                    Ok(new_stream) => {
                        stream = new_stream;
                        if let Some(session_id) = session_id.clone()
                            && let Ok(WireResponse::Snapshot(snapshot)) =
                                request_path(&path, WireRequest::Snapshot { session_id }).await
                        {
                            let _ = events.send(RuntimeEvent::SessionOpened { session: snapshot });
                        }
                        break;
                    }
                    Err(_) => tokio::select! {
                        _ = shutdown.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                    },
                }
            }
        }
    }

    fn unexpected_response(response: WireResponse) -> ClientError {
        ClientError::Runtime(format!("unexpected local runtime response: {response:?}"))
    }

    fn transport_client_error(error: TransportError) -> ClientError {
        ClientError::Runtime(error.to_string())
    }
}

#[cfg(unix)]
pub use unix::{LocalSocketRuntimeClient, LocalSocketServer};

#[cfg(not(unix))]
mod unsupported {
    use tokio::sync::broadcast;

    use super::*;

    pub struct LocalSocketServer;

    impl LocalSocketServer {
        pub async fn bind(
            _path: impl AsRef<Path>,
            _runtime: Arc<dyn LocalRuntimeService>,
        ) -> Result<Self, TransportError> {
            Err(TransportError::Unavailable(
                "Unix sockets are not supported on this platform".to_string(),
            ))
        }

        pub fn path(&self) -> &Path {
            Path::new("")
        }

        pub async fn serve(self, _shutdown: CancellationToken) -> Result<(), TransportError> {
            Err(TransportError::Unavailable(
                "Unix sockets are not supported on this platform".to_string(),
            ))
        }
    }

    pub struct LocalSocketRuntimeClient;

    impl LocalSocketRuntimeClient {
        pub async fn connect(_path: impl AsRef<Path>) -> Result<Self, TransportError> {
            Err(TransportError::Unavailable(
                "Unix sockets are not supported on this platform".to_string(),
            ))
        }

        pub async fn create_session(
            &self,
            _request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            Err(ClientError::Runtime(
                "Unix sockets are not supported on this platform".to_string(),
            ))
        }
    }

    #[async_trait]
    impl InteractiveRuntimeClient for LocalSocketRuntimeClient {
        async fn send(&self, _command: ClientCommand) -> Result<(), ClientError> {
            Err(ClientError::Runtime(
                "Unix sockets are not supported on this platform".to_string(),
            ))
        }

        fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
            let (_sender, receiver) = broadcast::channel(1);
            receiver
        }

        async fn snapshot(
            &self,
            _session_id: &SessionId,
        ) -> Result<UiSessionSnapshot, ClientError> {
            Err(ClientError::Runtime(
                "Unix sockets are not supported on this platform".to_string(),
            ))
        }
    }
}

#[cfg(not(unix))]
pub use unsupported::{LocalSocketRuntimeClient, LocalSocketServer};

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use leveler_client_protocol::{ClientCommand, RuntimeEvent, SessionId, UiSessionSnapshot};
    use tokio::sync::broadcast;

    use super::*;

    struct TestRuntime {
        events: broadcast::Sender<RuntimeEvent>,
        commands: Mutex<Vec<ClientCommand>>,
        snapshot: Arc<Mutex<UiSessionSnapshot>>,
    }

    impl TestRuntime {
        fn new() -> Self {
            let (events, _) = broadcast::channel(32);
            Self {
                events,
                commands: Mutex::new(Vec::new()),
                snapshot: Arc::new(Mutex::new(UiSessionSnapshot {
                    id: SessionId::new("s1"),
                    repository: "/repo".to_string(),
                    goal: "interactive session".to_string(),
                    model: Some(ModelRef::new("mock", "m")),
                    mode: PermissionProfile::Assisted,
                    branch: None,
                    status: "idle".to_string(),
                    messages: Vec::new(),
                    pending_interactions: Vec::new(),
                    available_models: vec![ModelRef::new("mock", "m")],
                    vision: false,
                    last_sequence: None,
                    active_tools: Vec::new(),
                    plan: None,
                    verification: None,
                    diff: None,
                    checkpoints: Vec::new(),
                    completion_report: None,
                })),
            }
        }
    }

    #[async_trait]
    impl InteractiveRuntimeClient for TestRuntime {
        async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
            if matches!(
                &command,
                ClientCommand::SubmitMessage { content, .. } if content == "finish after disconnect"
            ) {
                let snapshot = self.snapshot.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                    snapshot.lock().unwrap().status = "completed".to_string();
                });
            }
            self.commands.lock().unwrap().push(command);
            Ok(())
        }

        fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
            self.events.subscribe()
        }

        async fn snapshot(
            &self,
            _session_id: &SessionId,
        ) -> Result<UiSessionSnapshot, ClientError> {
            Ok(self.snapshot.lock().unwrap().clone())
        }
    }

    #[async_trait]
    impl LocalRuntimeService for TestRuntime {
        async fn create_session(
            &self,
            _request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            Ok(SessionBootstrap {
                session: self.snapshot.lock().unwrap().clone(),
                context_window: 128_000,
            })
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn socket_round_trips_session_snapshot_command_and_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.serve(shutdown.clone()));

        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        let bootstrap = client
            .create_session(CreateSessionRequest {
                goal: "interactive session".to_string(),
                model: None,
                mode: PermissionProfile::Assisted,
            })
            .await
            .unwrap();
        assert_eq!(bootstrap.session.id, SessionId::new("s1"));
        assert_eq!(bootstrap.context_window, 128_000);

        let snapshot = client.snapshot(&SessionId::new("s1")).await.unwrap();
        assert_eq!(snapshot.repository, "/repo");

        client
            .send(ClientCommand::RequestSessionList)
            .await
            .unwrap();
        assert_eq!(runtime.commands.lock().unwrap().len(), 1);

        let mut events = client.subscribe();
        runtime.events.send(RuntimeEvent::RuntimeReady).unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap(),
            RuntimeEvent::RuntimeReady
        );

        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_ui_client_does_not_send_runtime_quit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.serve(shutdown.clone()));

        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        drop(client);
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert!(
            runtime.commands.lock().unwrap().is_empty(),
            "disconnecting a UI must not shut down the daemon runtime"
        );
        let replacement = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        assert_eq!(
            replacement
                .snapshot(&SessionId::new("s1"))
                .await
                .unwrap()
                .id,
            SessionId::new("s1")
        );

        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn socket_client_cannot_shut_down_the_daemon_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.serve(shutdown.clone()));

        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        let error = client.send(ClientCommand::Quit).await.unwrap_err();
        assert!(error.to_string().contains("runtime owner"));
        assert!(runtime.commands.lock().unwrap().is_empty());

        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn accepted_work_continues_after_the_ui_disconnects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime).await.unwrap();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.serve(shutdown.clone()));

        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        client
            .send(ClientCommand::SubmitMessage {
                session_id: SessionId::new("s1"),
                content: "finish after disconnect".to_string(),
                attachments: vec![],
            })
            .await
            .unwrap();
        drop(client);

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let replacement = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        assert_eq!(
            replacement
                .snapshot(&SessionId::new("s1"))
                .await
                .unwrap()
                .status,
            "completed"
        );

        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn socket_is_owner_only_and_live_socket_is_not_replaced() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let error = LocalSocketServer::bind(&path, runtime)
            .await
            .err()
            .expect("a second daemon must be rejected");
        assert!(matches!(error, TransportError::AlreadyRunning(_)));
        drop(server);
        assert!(!path.exists(), "dropping the server removes its socket");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_never_replaces_a_non_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sock");
        std::fs::write(&path, "owned by user").unwrap();
        let error = LocalSocketServer::bind(&path, Arc::new(TestRuntime::new()))
            .await
            .err()
            .expect("a normal file at the socket path must be protected");
        assert!(matches!(error, TransportError::Unavailable(_)));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "owned by user");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_server_never_removes_a_replacement_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sock");
        let server = LocalSocketServer::bind(&path, Arc::new(TestRuntime::new()))
            .await
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "replacement owned by user").unwrap();

        drop(server);

        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "replacement owned by user"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn client_reconnects_and_resyncs_tracked_sessions_after_daemon_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sock");
        let first_runtime = Arc::new(TestRuntime::new());
        let first_server = LocalSocketServer::bind(&path, first_runtime).await.unwrap();
        let first_shutdown = CancellationToken::new();
        let first_task = tokio::spawn(first_server.serve(first_shutdown.clone()));

        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        client.snapshot(&SessionId::new("s1")).await.unwrap();
        let mut events = client.subscribe_session(&SessionId::new("s1"));
        first_shutdown.cancel();
        first_task.await.unwrap().unwrap();

        let replacement_runtime = TestRuntime::new();
        replacement_runtime.snapshot.lock().unwrap().repository = "/repo-after-restart".to_string();
        let second_shutdown = CancellationToken::new();
        let second_server = LocalSocketServer::bind(&path, Arc::new(replacement_runtime))
            .await
            .unwrap();
        let second_task = tokio::spawn(second_server.serve(second_shutdown.clone()));

        let event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let RuntimeEvent::SessionOpened { session } = events.recv().await.unwrap()
                    && session.repository == "/repo-after-restart"
                {
                    break session;
                }
            }
        })
        .await
        .expect("the existing client must reconnect and receive a fresh snapshot");
        assert_eq!(event.id, SessionId::new("s1"));

        second_shutdown.cancel();
        second_task.await.unwrap().unwrap();
    }
}
