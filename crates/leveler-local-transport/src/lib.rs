//! Trusted local transport between CodeLeveler UI clients and the runtime.
#![forbid(unsafe_code)]

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use leveler_client_protocol::{
    ClientCommand, ClientError, InteractiveRuntimeClient, ModelRef, PermissionProfile,
    ProtocolError, RuntimeEvent, SessionId, UiSessionSnapshot,
};
#[cfg(unix)]
use leveler_client_protocol::{CommandEnvelope, ProtocolEnvelope};
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
#[cfg(unix)]
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
#[cfg(unix)]
enum WireResponse {
    Ack,
    Snapshot(UiSessionSnapshot),
    SessionCreated(SessionBootstrap),
    Event(RuntimeEvent),
    Error(WireError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg(unix)]
struct WireError {
    message: String,
    session_id: Option<SessionId>,
}

#[cfg(unix)]
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

#[cfg(unix)]
impl WireError {
    fn into_client_error(self) -> ClientError {
        match self.session_id {
            Some(session_id) => ClientError::SessionNotFound(session_id),
            None => ClientError::Runtime(self.message),
        }
    }
}

/// The first frame a TCP client must send: it presents the shared bearer token
/// before any request is read. Unix-socket clients skip this — the socket file's
/// `0600` permission is the trust boundary there; a TCP listener has none.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg(unix)]
struct Handshake {
    token: String,
}

#[cfg(unix)]
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::sync::Mutex;
    use std::time::Duration;

    use serde::de::DeserializeOwned;
    use std::net::SocketAddr;

    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
    use tokio::sync::broadcast;
    use tokio_util::either::Either;

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

    async fn send_response<S: AsyncWrite + Unpin>(
        stream: &mut S,
        response: WireResponse,
    ) -> Result<(), TransportError> {
        write_frame(stream, &ProtocolEnvelope::wrap(response)).await
    }

    async fn send_result<S: AsyncWrite + Unpin>(
        stream: &mut S,
        result: Result<WireResponse, ClientError>,
    ) -> Result<(), TransportError> {
        let response = result.unwrap_or_else(|error| WireResponse::Error(error.into()));
        send_response(stream, response).await
    }

    /// Serve one connection. Generic over the stream so the same request
    /// handling backs both the Unix-socket server and the loopback TCP daemon.
    async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(
        mut stream: S,
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
        endpoint: Endpoint,
        events: broadcast::Sender<RuntimeEvent>,
        session_events:
            Arc<Mutex<std::collections::HashMap<SessionId, broadcast::Sender<RuntimeEvent>>>>,
        shutdown: CancellationToken,
    }

    impl LocalSocketRuntimeClient {
        pub async fn connect(path: impl AsRef<Path>) -> Result<Self, TransportError> {
            Self::open(Endpoint::Unix(path.as_ref().to_path_buf())).await
        }

        /// Connect to a loopback TCP daemon, authenticating with the bearer token
        /// on this and every subsequent (per-request, per-subscription) connection.
        pub async fn connect_tcp(
            addr: SocketAddr,
            token: impl Into<String>,
        ) -> Result<Self, TransportError> {
            Self::open(Endpoint::Tcp {
                addr,
                token: Arc::from(token.into()),
            })
            .await
        }

        async fn open(endpoint: Endpoint) -> Result<Self, TransportError> {
            let stream = open_subscription(&endpoint, None).await?;
            let (events, _) = broadcast::channel(2048);
            let session_events = Arc::new(Mutex::new(std::collections::HashMap::new()));
            let shutdown = CancellationToken::new();
            tokio::spawn(subscription_loop(
                endpoint.clone(),
                stream,
                events.clone(),
                None,
                shutdown.clone(),
            ));
            Ok(Self {
                endpoint,
                events,
                session_events,
                shutdown,
            })
        }

        async fn request(&self, request: WireRequest) -> Result<WireResponse, TransportError> {
            request_endpoint(&self.endpoint, request).await
        }

        async fn ensure_session_subscription(
            &self,
            session_id: &SessionId,
        ) -> Result<broadcast::Sender<RuntimeEvent>, TransportError> {
            if let Some(events) = self.session_events.lock().unwrap().get(session_id).cloned() {
                return Ok(events);
            }
            let stream = open_subscription(&self.endpoint, Some(session_id.clone())).await?;
            let (events, _) = broadcast::channel(2048);
            self.session_events
                .lock()
                .unwrap()
                .insert(session_id.clone(), events.clone());
            tokio::spawn(subscription_loop(
                self.endpoint.clone(),
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

    /// The socket client speaks the full local-transport wire protocol, so it
    /// satisfies the daemon service contract directly; `create_session`
    /// delegates to the inherent method of the same name.
    #[async_trait]
    impl LocalRuntimeService for LocalSocketRuntimeClient {
        async fn create_session(
            &self,
            request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            LocalSocketRuntimeClient::create_session(self, request).await
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
            let endpoint = self.endpoint.clone();
            let session_id = session_id.clone();
            let shutdown = self.shutdown.clone();
            tokio::spawn(async move {
                loop {
                    match open_subscription(&endpoint, Some(session_id.clone())).await {
                        Ok(stream) => {
                            subscription_loop(endpoint, stream, events, Some(session_id), shutdown)
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

    /// How a client reaches the runtime. A Unix client trusts the socket file's
    /// `0600` perms; a TCP client presents a bearer token on every connection.
    #[derive(Clone)]
    enum Endpoint {
        Unix(PathBuf),
        Tcp { addr: SocketAddr, token: Arc<str> },
    }

    /// One connection over either transport. `Either` yields a single concrete
    /// type that impls AsyncRead + AsyncWrite for both stream kinds.
    type ClientStream = Either<UnixStream, TcpStream>;

    /// Open (and, for TCP, authenticate) one connection to the endpoint.
    async fn connect_endpoint(endpoint: &Endpoint) -> Result<ClientStream, TransportError> {
        match endpoint {
            Endpoint::Unix(path) => Ok(Either::Left(UnixStream::connect(path).await?)),
            Endpoint::Tcp { addr, token } => {
                let mut stream = TcpStream::connect(addr).await?;
                write_frame(
                    &mut stream,
                    &ProtocolEnvelope::wrap(Handshake {
                        token: token.to_string(),
                    }),
                )
                .await?;
                match read_frame::<WireResponse>(&mut stream).await?.into_body()? {
                    WireResponse::Ack => Ok(Either::Right(stream)),
                    WireResponse::Error(error) => Err(TransportError::Unavailable(error.message)),
                    other => Err(TransportError::Unavailable(format!(
                        "unexpected handshake response: {other:?}"
                    ))),
                }
            }
        }
    }

    async fn request_endpoint(
        endpoint: &Endpoint,
        request: WireRequest,
    ) -> Result<WireResponse, TransportError> {
        let mut stream = connect_endpoint(endpoint).await?;
        write_frame(&mut stream, &ProtocolEnvelope::wrap(request)).await?;
        read_frame::<WireResponse>(&mut stream)
            .await?
            .into_body()
            .map_err(Into::into)
    }

    async fn open_subscription(
        endpoint: &Endpoint,
        session_id: Option<SessionId>,
    ) -> Result<ClientStream, TransportError> {
        let mut stream = connect_endpoint(endpoint).await?;
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
        endpoint: Endpoint,
        mut stream: ClientStream,
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
                match open_subscription(&endpoint, session_id.clone()).await {
                    Ok(new_stream) => {
                        stream = new_stream;
                        if let Some(session_id) = session_id.clone()
                            && let Ok(WireResponse::Snapshot(snapshot)) =
                                request_endpoint(&endpoint, WireRequest::Snapshot { session_id })
                                    .await
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

    // ---- Loopback TCP daemon with bearer-token auth ----------------------

    /// Constant-time equality for the bearer token. Length is compared first —
    /// a token's *length* is not the secret (we mint fixed-length tokens); its
    /// *value* is, and equal-length values are compared with no early exit so a
    /// timing side channel cannot recover the token byte by byte.
    fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    /// Gate one TCP connection on the bearer token, then hand it to the shared
    /// connection handler. The client MUST send a `Handshake` frame first; a
    /// wrong or absent token gets a generic "authentication failed" (no token
    /// echo, no distinct code) and the connection is dropped without service.
    async fn serve_authenticated_tcp<S: AsyncRead + AsyncWrite + Unpin>(
        mut stream: S,
        token: &str,
        runtime: Arc<dyn LocalRuntimeService>,
        shutdown: CancellationToken,
    ) -> Result<(), TransportError> {
        let presented = read_frame::<Handshake>(&mut stream).await?.into_body()?;
        if !constant_time_eq(presented.token.as_bytes(), token.as_bytes()) {
            send_response(
                &mut stream,
                WireResponse::Error(WireError {
                    message: "authentication failed".to_string(),
                    session_id: None,
                }),
            )
            .await?;
            return Ok(());
        }
        send_response(&mut stream, WireResponse::Ack).await?;
        handle_connection(stream, runtime, shutdown).await
    }

    /// A loopback TCP runtime server. Every connection presents a shared bearer
    /// token before any request is served; only loopback peers are accepted.
    pub struct TcpRuntimeServer {
        listener: TcpListener,
        runtime: Arc<dyn LocalRuntimeService>,
        token: Arc<str>,
    }

    impl TcpRuntimeServer {
        pub async fn bind(
            addr: SocketAddr,
            token: impl Into<String>,
            runtime: Arc<dyn LocalRuntimeService>,
        ) -> Result<Self, TransportError> {
            let token = token.into();
            if token.is_empty() {
                return Err(TransportError::Unavailable(
                    "refusing to start a TCP daemon with an empty token".to_string(),
                ));
            }
            let listener = TcpListener::bind(addr).await?;
            Ok(Self {
                listener,
                runtime,
                token: Arc::from(token),
            })
        }

        /// The actually-bound address (resolves an ephemeral `:0` port).
        pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
            Ok(self.listener.local_addr()?)
        }

        pub async fn serve(self, shutdown: CancellationToken) -> Result<(), TransportError> {
            let child_shutdown = shutdown.child_token();
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    accepted = self.listener.accept() => {
                        let (stream, _peer) = accepted?;
                        // Loopback only: never serve a non-loopback peer even if
                        // the OS routed one here (defence in depth vs a misbind).
                        if !stream.peer_addr().map(|a| a.ip().is_loopback()).unwrap_or(false) {
                            continue;
                        }
                        let runtime = self.runtime.clone();
                        let token = self.token.clone();
                        let connection_shutdown = child_shutdown.clone();
                        tasks.spawn(async move {
                            if let Err(error) =
                                serve_authenticated_tcp(stream, &token, runtime, connection_shutdown)
                                    .await
                            {
                                tracing::debug!(%error, "tcp daemon connection ended");
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

    /// One authenticated request over TCP (the TCP analogue of `request_path`):
    /// connect, present the token, then send the request and read one response.
    /// Crate-internal: it speaks the private wire protocol, so a public
    /// `TcpRuntimeClient` will wrap it rather than exposing `WireRequest`.
    /// Test-only for now — it is the connect-and-authenticate primitive that a
    /// production `TcpRuntimeClient` will reuse; un-gate it when that lands.
    #[cfg(test)]
    pub(crate) async fn tcp_request(
        addr: SocketAddr,
        token: &str,
        request: WireRequest,
    ) -> Result<WireResponse, TransportError> {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        write_frame(
            &mut stream,
            &ProtocolEnvelope::wrap(Handshake {
                token: token.to_string(),
            }),
        )
        .await?;
        match read_frame::<WireResponse>(&mut stream).await?.into_body()? {
            WireResponse::Ack => {}
            WireResponse::Error(error) => return Err(TransportError::Unavailable(error.message)),
            other => {
                return Err(TransportError::Unavailable(format!(
                    "unexpected handshake response: {other:?}"
                )));
            }
        }
        write_frame(&mut stream, &ProtocolEnvelope::wrap(request)).await?;
        read_frame::<WireResponse>(&mut stream)
            .await?
            .into_body()
            .map_err(Into::into)
    }
}

#[cfg(all(unix, test))]
pub(crate) use unix::tcp_request;
#[cfg(unix)]
pub use unix::{LocalSocketRuntimeClient, LocalSocketServer, TcpRuntimeServer};

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

        pub async fn connect_tcp(
            _addr: std::net::SocketAddr,
            _token: impl Into<String>,
        ) -> Result<Self, TransportError> {
            Err(TransportError::Unavailable(
                "the TCP daemon is not supported on this platform".to_string(),
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

    #[async_trait]
    impl LocalRuntimeService for LocalSocketRuntimeClient {
        async fn create_session(
            &self,
            request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            LocalSocketRuntimeClient::create_session(self, request).await
        }
    }

    pub struct TcpRuntimeServer;

    impl TcpRuntimeServer {
        pub async fn bind(
            _addr: std::net::SocketAddr,
            _token: impl Into<String>,
            _runtime: Arc<dyn LocalRuntimeService>,
        ) -> Result<Self, TransportError> {
            Err(TransportError::Unavailable(
                "the TCP daemon is not supported on this platform".to_string(),
            ))
        }

        pub fn local_addr(&self) -> Result<std::net::SocketAddr, TransportError> {
            Err(TransportError::Unavailable(
                "the TCP daemon is not supported on this platform".to_string(),
            ))
        }

        pub async fn serve(self, _shutdown: CancellationToken) -> Result<(), TransportError> {
            Err(TransportError::Unavailable(
                "the TCP daemon is not supported on this platform".to_string(),
            ))
        }
    }
}

#[cfg(not(unix))]
pub use unsupported::{LocalSocketRuntimeClient, LocalSocketServer, TcpRuntimeServer};

#[cfg(all(test, unix))]
mod tests {
    use std::net::SocketAddr;
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

    async fn tcp_server(token: &str) -> (SocketAddr, Arc<TestRuntime>, CancellationToken) {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let runtime = Arc::new(TestRuntime::new());
        let server = TcpRuntimeServer::bind(addr, token, runtime.clone())
            .await
            .unwrap();
        let bound = server.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        tokio::spawn(server.serve(shutdown.clone()));
        (bound, runtime, shutdown)
    }

    #[tokio::test]
    async fn tcp_daemon_serves_a_request_after_a_correct_token() {
        let (addr, _runtime, shutdown) = tcp_server("s3cret-token").await;
        let response = tcp_request(
            addr,
            "s3cret-token",
            WireRequest::CreateSession(CreateSessionRequest {
                goal: "tcp session".to_string(),
                model: None,
                mode: PermissionProfile::Assisted,
            }),
        )
        .await
        .unwrap();
        assert!(matches!(response, WireResponse::SessionCreated(_)));
        shutdown.cancel();
    }

    #[tokio::test]
    async fn tcp_daemon_rejects_a_wrong_token_and_serves_nothing() {
        let (addr, runtime, shutdown) = tcp_server("s3cret-token").await;
        let result = tcp_request(
            addr,
            "wrong-token",
            WireRequest::Send(ClientCommand::OpenSession {
                session_id: SessionId::new("s1"),
            }),
        )
        .await;
        assert!(result.is_err(), "wrong token must be rejected: {result:?}");
        // The rejected connection must never have reached the runtime.
        assert!(runtime.commands.lock().unwrap().is_empty());
        shutdown.cancel();
    }

    #[tokio::test]
    async fn tcp_daemon_refuses_to_start_without_a_token() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let runtime = Arc::new(TestRuntime::new());
        let result = TcpRuntimeServer::bind(addr, "", runtime).await;
        assert!(result.is_err(), "an empty token must be refused at bind");
    }

    #[tokio::test]
    async fn tcp_client_round_trips_through_the_authenticated_daemon() {
        // End-to-end over the production client: connect_tcp authenticates, and
        // every follow-up request/subscription re-authenticates transparently.
        let (addr, runtime, shutdown) = tcp_server("e2e-token").await;
        let client = LocalSocketRuntimeClient::connect_tcp(addr, "e2e-token")
            .await
            .unwrap();
        let bootstrap = client
            .create_session(CreateSessionRequest {
                goal: "tcp e2e".to_string(),
                model: None,
                mode: PermissionProfile::Assisted,
            })
            .await
            .unwrap();
        assert_eq!(bootstrap.context_window, 128_000);
        client
            .send(ClientCommand::SubmitMessage {
                session_id: bootstrap.session.id.clone(),
                content: "hello over tcp".to_string(),
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(runtime.commands.lock().unwrap().len(), 1);
        shutdown.cancel();
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
