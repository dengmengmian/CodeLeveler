//! Trusted local transport between CodeLeveler UI clients and the runtime.
#![forbid(unsafe_code)]

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use leveler_client_protocol::{
    ApprovalPolicy, ClientCommand, ClientError, InteractiveRuntimeClient, ModelRef,
    PermissionProfile, ProtocolError, RuntimeEvent, SessionId, UiSessionSnapshot,
};
#[cfg(unix)]
use leveler_client_protocol::{CommandEnvelope, ProtocolEnvelope};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// What kind of client opened a subscription.
///
/// The daemon serves every subscriber identically; this exists so the *count*
/// of attached local interactive UIs is knowable. The remote approval timeout
/// must not fire while a person could still be reading the prompt in their own
/// terminal, and that question cannot be answered without knowing who is
/// attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    /// A UI on this machine: the TUI, or the loopback web server.
    ///
    /// The default, and deliberately the safe direction for an older client
    /// that omits the field: counting an unknown subscriber as local can only
    /// suppress an auto-deny, whereas counting it as remote could expire a
    /// prompt a real person was answering.
    #[default]
    LocalInteractive,
    /// The remote agent, bridging a paired device.
    Remote,
}

/// Live count of attached local interactive UIs.
///
/// Cloned handles share one counter; a subscription holds a
/// [`LocalWaiterGuard`] for exactly as long as its connection is served, so a
/// dropped connection decrements even when the client vanished without saying
/// goodbye.
#[derive(Debug, Clone, Default)]
pub struct LocalWaiters(Arc<std::sync::atomic::AtomicUsize>);

impl LocalWaiters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Count one attached local UI until the returned guard drops.
    pub fn attach(&self) -> LocalWaiterGuard {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        LocalWaiterGuard(self.0.clone())
    }
}

/// Decrements its [`LocalWaiters`] on drop.
#[derive(Debug)]
pub struct LocalWaiterGuard(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for LocalWaiterGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Brings a dead local runtime back: called by the socket client when the
/// daemon stops answering. The implementation (the CLI's ensure-daemon path)
/// probes and, if needed, starts a fresh detached daemon for the same state
/// directory — same durable RuntimeId, fresh OwnerEpoch on reacquire; task
/// recovery itself stays in the daemon/engine. Idempotent: losing a
/// concurrent revival race and connecting to the winner is success.
#[async_trait]
pub trait RuntimeReviver: Send + Sync {
    /// Ensure a runtime is serving the endpoint again. `Err` = revival
    /// failed (reported, then retried by the caller's loop).
    async fn revive(&self) -> Result<(), String>;
}

/// Everything the daemon needs to create a new interactive session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub goal: String,
    pub model: Option<ModelRef>,
    pub mode: PermissionProfile,
    /// Whether this session auto-approves risky actions (unattended) or prompts.
    /// Carried on the trusted local transport so `--auto-approve` selects a
    /// per-session policy instead of forcing an in-process runtime. `default`
    /// keeps old clients (and any request that omits it) at `Interactive`; the
    /// remote/web boundary force-resets it so a remote client cannot elevate.
    #[serde(default)]
    pub approval_policy: ApprovalPolicy,
}

/// Initial client state returned atomically with session creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBootstrap {
    pub session: UiSessionSnapshot,
    pub context_window: u32,
}

/// Bytes of one registered attachment, loaded from the runtime media store.
///
/// Remote clients never see a filesystem path. The content hash is the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentBytes {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

/// Runtime operations needed in addition to the stable interactive client
/// contract when a daemon owns session creation.
#[async_trait]
pub trait LocalRuntimeService: InteractiveRuntimeClient {
    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError>;

    /// Re-assert the effective approval policy for an EXISTING session on
    /// attach/resume (R006 R6-P2: `tui --session --auto-approve` silently lost
    /// the policy). Gated at the transport exactly like `create_session`; the
    /// default errs so test doubles and older daemons fail loudly instead of
    /// silently downgrading.
    async fn attach_session_policy(
        &self,
        _session_id: &SessionId,
        _policy: ApprovalPolicy,
    ) -> Result<(), ClientError> {
        Err(ClientError::Runtime(
            "attach_session_policy is not supported by this runtime".to_string(),
        ))
    }

    /// How many local interactive UIs are attached to this runtime right now.
    ///
    /// The remote approval timeout needs this: it must not expire a prompt while
    /// a person could still be reading it in their own terminal. The default
    /// answers `1` — "assume someone is watching" — because that direction can
    /// only suppress an auto-deny, whereas guessing zero would let a paired
    /// phone's policy cut off a decision a human was making. The socket client
    /// asks the daemon for the real number.
    async fn local_waiter_count(&self) -> Result<usize, ClientError> {
        Ok(1)
    }

    /// This runtime's identity and process diagnostics.
    ///
    /// Used by clients to verify which runtime they discovered/reconnected to.
    /// The default errs so test doubles need not fake an identity; every
    /// production service (in-process runtime, socket client, daemon bridge,
    /// web router) overrides it.
    async fn runtime_info(&self) -> Result<leveler_client_protocol::RuntimeInfo, ClientError> {
        Err(ClientError::Runtime(
            "runtime identity is not available on this service".to_string(),
        ))
    }

    /// Load a registered attachment by its content hash.
    ///
    /// Default: unsupported. Production runtimes override. Callers must not
    /// invent a second store or open workspace paths.
    async fn fetch_attachment(&self, _sha256: &str) -> Result<AttachmentBytes, ClientError> {
        Err(ClientError::Runtime(
            "fetch_attachment is not supported by this runtime".to_string(),
        ))
    }
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
    /// A request failed mid-flight and was NOT replayed: its first attempt
    /// may already have taken effect. Callers must not treat this as a
    /// transient error and re-send the same mutation automatically.
    #[error("request outcome unknown; not replayed: {0}")]
    OutcomeUnknown(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
#[cfg(unix)]
enum WireRequest {
    Ping,
    Send(ClientCommand),
    Deliver(ProtocolEnvelope<CommandEnvelope>),
    Snapshot {
        session_id: SessionId,
    },
    CreateSession {
        request: CreateSessionRequest,
        /// The kind of client that opened this connection, as it declared itself
        /// when connecting (`connect_as`). Load-bearing for approval trust: only
        /// a `LocalInteractive` client on a trusted-local transport may create an
        /// `AutoApprove` session; a `Remote` kind (the remote-agent bridge) or a
        /// TCP connection is normalized to `Interactive` at the daemon boundary
        /// (see `handle_connection`). Set by the connecting client, never from a
        /// relayed body, so a remote peer cannot spoof it.
        #[serde(default)]
        client_kind: ClientKind,
    },
    /// Re-assert the approval policy for an existing session on attach/resume.
    /// Same trust gate as `CreateSession` (R006 R6-P2). Additive: daemons built
    /// before this variant fail the request; clients surface that instead of
    /// silently downgrading.
    AttachSessionPolicy {
        session_id: SessionId,
        approval_policy: ApprovalPolicy,
        #[serde(default)]
        client_kind: ClientKind,
    },
    /// How many local interactive UIs are attached. Asked by the remote agent,
    /// which cannot see this machine's terminals from its own process.
    LocalWaiters,
    /// The runtime's identity + process diagnostics. Additive: a daemon built
    /// before this variant fails the request, which clients treat as
    /// "identity unknown", never as a fatal error.
    RuntimeInfo,
    /// Load a registered attachment by sha256. A read: safe to retry.
    FetchAttachment {
        sha256: String,
    },
    Subscribe {
        session_id: Option<SessionId>,
        /// Absent from clients built before remote control existed; `default`
        /// makes those count as local, which is the safe direction.
        #[serde(default)]
        client_kind: ClientKind,
    },
}

#[cfg(unix)]
impl WireRequest {
    /// Whether this request may be replayed after a transport failure whose
    /// outcome is unknown. Reads (Ping/Snapshot/LocalWaiters/RuntimeInfo)
    /// are always safe; Deliver is safe because the daemon deduplicates by
    /// CommandEnvelope command_id (the replay carries the SAME id, so the
    /// mutation runs at most once). Raw Send and CreateSession have no
    /// idempotency key: their first attempt may already have mutated state,
    /// so they must never be auto-replayed. Subscribe never goes through
    /// the request path (it has its own reconnect loop).
    fn safe_to_retry_after_transport_failure(&self) -> bool {
        match self {
            WireRequest::Ping
            | WireRequest::Snapshot { .. }
            | WireRequest::LocalWaiters
            | WireRequest::RuntimeInfo
            | WireRequest::FetchAttachment { .. }
            // Idempotent per-session policy assertion — replaying it after a
            // transport failure converges to the same state.
            | WireRequest::AttachSessionPolicy { .. }
            | WireRequest::Deliver(_) => true,
            WireRequest::Send(_)
            | WireRequest::CreateSession { .. }
            | WireRequest::Subscribe { .. } => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
#[cfg(unix)]
enum WireResponse {
    Ack,
    Snapshot(UiSessionSnapshot),
    SessionCreated(SessionBootstrap),
    Event(RuntimeEvent),
    LocalWaiters(usize),
    RuntimeInfo(leveler_client_protocol::RuntimeInfo),
    Attachment {
        mime_type: String,
        data_base64: String,
    },
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
        local_waiters: LocalWaiters,
        // Whether this listener is a trusted-local transport (the per-repo Unix
        // socket). `false` for the loopback TCP server: a TCP peer is never
        // trusted to grant AutoApprove, whatever `ClientKind` it declares.
        transport_trusted: bool,
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
            WireRequest::CreateSession {
                mut request,
                client_kind,
            } => {
                // The single daemon-side trust boundary for AutoApprove. The
                // effective trust is the MORE restrictive of the transport
                // (TCP → Remote) and the client's declared kind (the bridge
                // declares Remote even over the local socket). AutoApprove is
                // honoured only for a LocalInteractive client on a trusted-local
                // transport; an explicit AutoApprove from any remote/TCP origin
                // is rejected (observable), never silently downgraded — while an
                // absent/Interactive policy is simply allowed.
                let effective_kind = if transport_trusted {
                    client_kind
                } else {
                    ClientKind::Remote
                };
                if effective_kind == ClientKind::Remote
                    && request.approval_policy == ApprovalPolicy::AutoApprove
                {
                    return send_result(
                        &mut stream,
                        Err(ClientError::Runtime(
                            "auto-approve may only be requested from the trusted local transport"
                                .to_string(),
                        )),
                    )
                    .await;
                }
                // Defence in depth: never let a non-trusted origin carry an
                // AutoApprove policy further into the runtime.
                if effective_kind == ClientKind::Remote {
                    request.approval_policy = ApprovalPolicy::Interactive;
                }
                send_result(
                    &mut stream,
                    runtime
                        .create_session(request)
                        .await
                        .map(WireResponse::SessionCreated),
                )
                .await
            }
            WireRequest::AttachSessionPolicy {
                session_id,
                approval_policy,
                client_kind,
            } => {
                // Identical trust boundary to CreateSession: AutoApprove only
                // for a LocalInteractive client on a trusted-local transport;
                // remote/TCP origins are rejected observably (R006 R6-P2).
                let effective_kind = if transport_trusted {
                    client_kind
                } else {
                    ClientKind::Remote
                };
                if effective_kind == ClientKind::Remote
                    && approval_policy == ApprovalPolicy::AutoApprove
                {
                    return send_result(
                        &mut stream,
                        Err::<WireResponse, _>(ClientError::Runtime(
                            "auto-approve may only be requested from the trusted local transport"
                                .to_string(),
                        )),
                    )
                    .await;
                }
                let policy = if effective_kind == ClientKind::Remote {
                    ApprovalPolicy::Interactive
                } else {
                    approval_policy
                };
                send_result(
                    &mut stream,
                    runtime
                        .attach_session_policy(&session_id, policy)
                        .await
                        .map(|()| WireResponse::Ack),
                )
                .await
            }
            // Answered from the daemon's own counter, not from the runtime:
            // this is a fact about who is connected here, which only the
            // transport knows.
            WireRequest::LocalWaiters => {
                send_response(
                    &mut stream,
                    WireResponse::LocalWaiters(local_waiters.count()),
                )
                .await
            }
            WireRequest::RuntimeInfo => {
                send_result(
                    &mut stream,
                    runtime.runtime_info().await.map(WireResponse::RuntimeInfo),
                )
                .await
            }
            WireRequest::FetchAttachment { sha256 } => {
                send_result(
                    &mut stream,
                    runtime.fetch_attachment(&sha256).await.map(|blob| {
                        use base64::Engine as _;
                        WireResponse::Attachment {
                            mime_type: blob.mime_type,
                            data_base64: base64::engine::general_purpose::STANDARD
                                .encode(&blob.bytes),
                        }
                    }),
                )
                .await
            }
            WireRequest::Subscribe {
                session_id,
                client_kind,
            } => {
                send_response(&mut stream, WireResponse::Ack).await?;
                // Held for exactly as long as this subscription is served, so
                // a client that dies without closing still decrements.
                let _waiter = match client_kind {
                    ClientKind::LocalInteractive => Some(local_waiters.attach()),
                    ClientKind::Remote => None,
                };
                let mut events = match session_id {
                    Some(session_id) => runtime.subscribe_session(&session_id),
                    None => runtime.subscribe(),
                };
                // Split so the peer can be watched for EOF while events are
                // written. Without a reader here the loop only discovers a
                // departed client the next time it tries to write, which on an
                // idle session may be never — and a subscriber that is counted
                // long after it left would keep the remote approval timeout
                // permanently disarmed.
                let (mut reader, mut writer) = tokio::io::split(stream);
                loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => return Ok(()),
                        // A subscription carries no further client bytes, so
                        // EOF, an error, or unexpected data all mean "done".
                        _ = reader.read_u8() => return Ok(()),
                        event = events.recv() => match event {
                            Ok(event) => send_response(&mut writer, WireResponse::Event(event)).await?,
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
        local_waiters: LocalWaiters,
        /// Exclusive `flock` on `<path>.lock`, held for the server's whole
        /// lifetime. This is the single-daemon guarantee: two processes
        /// racing a stale socket used to both pass the connect probe, both
        /// remove the file, and both bind — leaving one daemon serving an
        /// unlinked socket nobody can reach. The lock makes the
        /// inspect-remove-bind sequence exclusive. Released implicitly when
        /// the file handle drops (including on crash).
        _lock: std::fs::File,
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
            // Take the daemon ownership lock BEFORE inspecting the socket
            // path. A live daemon holds it for its whole lifetime, so a
            // failed try-lock means one is running; a crashed daemon's lock
            // is released by the OS, so a stale file never blocks startup.
            let lock_path = path.with_extension("lock");
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)?;
            if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
                return Err(TransportError::AlreadyRunning(path.display().to_string()));
            }
            if let Ok(metadata) = tokio::fs::symlink_metadata(&path).await {
                if !metadata.file_type().is_socket() {
                    return Err(TransportError::Unavailable(format!(
                        "refusing to replace non-socket path {}",
                        path.display()
                    )));
                }
                // Kept as a cross-version safety net: a daemon built before
                // the flock existed holds no lock but still answers here.
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
                local_waiters: LocalWaiters::new(),
                _lock: lock,
            })
        }

        pub fn path(&self) -> &Path {
            &self.path
        }

        /// Live count of attached local interactive UIs.
        ///
        /// The remote agent reads this to decide whether an approval is
        /// remote-only; the handle stays valid after `serve` consumes the
        /// server.
        pub fn local_waiters(&self) -> LocalWaiters {
            self.local_waiters.clone()
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
                        let waiters = self.local_waiters.clone();
                        tasks.spawn(async move {
                            // The per-repo Unix socket is the trusted-local transport.
                            if let Err(error) = handle_connection(
                                stream,
                                runtime,
                                connection_shutdown,
                                waiters,
                                /*transport_trusted*/ true,
                            )
                            .await
                            {
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
        /// Announced on every subscription this client opens.
        client_kind: ClientKind,
        events: broadcast::Sender<RuntimeEvent>,
        session_events:
            Arc<Mutex<std::collections::HashMap<SessionId, broadcast::Sender<RuntimeEvent>>>>,
        shutdown: CancellationToken,
        /// Optional daemon revival hook (see [`RuntimeReviver`]); installed by
        /// the composition root, consulted by request retries and the
        /// subscription reconnect loop.
        reviver: Arc<std::sync::OnceLock<Arc<dyn RuntimeReviver>>>,
    }

    impl LocalSocketRuntimeClient {
        pub async fn connect(path: impl AsRef<Path>) -> Result<Self, TransportError> {
            Self::connect_as(path, ClientKind::LocalInteractive).await
        }

        /// Connect while declaring what kind of client this is.
        ///
        /// Only the remote agent needs this; local UIs get
        /// [`ClientKind::LocalInteractive`] from [`Self::connect`], so a phone
        /// cannot be miscounted as somebody sitting at the machine.
        pub async fn connect_as(
            path: impl AsRef<Path>,
            client_kind: ClientKind,
        ) -> Result<Self, TransportError> {
            Self::open(Endpoint::Unix(path.as_ref().to_path_buf()), client_kind).await
        }

        /// Connect to a loopback TCP daemon, authenticating with the bearer token
        /// on this and every subsequent (per-request, per-subscription) connection.
        pub async fn connect_tcp(
            addr: SocketAddr,
            token: impl Into<String>,
        ) -> Result<Self, TransportError> {
            Self::connect_tcp_as(addr, token, ClientKind::LocalInteractive).await
        }

        /// Loopback TCP variant of [`Self::connect_as`].
        pub async fn connect_tcp_as(
            addr: SocketAddr,
            token: impl Into<String>,
            client_kind: ClientKind,
        ) -> Result<Self, TransportError> {
            Self::open(
                Endpoint::Tcp {
                    addr,
                    token: Arc::from(token.into()),
                },
                client_kind,
            )
            .await
        }

        async fn open(endpoint: Endpoint, client_kind: ClientKind) -> Result<Self, TransportError> {
            let stream = open_subscription(&endpoint, None, client_kind).await?;
            let (events, _) = broadcast::channel(2048);
            let session_events = Arc::new(Mutex::new(std::collections::HashMap::new()));
            let shutdown = CancellationToken::new();
            let reviver: Arc<std::sync::OnceLock<Arc<dyn RuntimeReviver>>> =
                Arc::new(std::sync::OnceLock::new());
            tokio::spawn(subscription_loop(
                endpoint.clone(),
                stream,
                events.clone(),
                None,
                shutdown.clone(),
                client_kind,
                reviver.clone(),
            ));
            Ok(Self {
                endpoint,
                client_kind,
                events,
                session_events,
                shutdown,
                reviver,
            })
        }

        /// Install the daemon revival hook. All existing and future
        /// subscriptions and request retries use it from now on.
        pub fn set_reviver(&self, reviver: Arc<dyn RuntimeReviver>) {
            let _ = self.reviver.set(reviver);
        }

        async fn request(&self, request: WireRequest) -> Result<WireResponse, TransportError> {
            match request_endpoint(&self.endpoint, request.clone()).await {
                Ok(response) => Ok(response),
                Err(error) => {
                    // Only REPLAY a request whose outcome cannot have mutated
                    // anything (reads) or that the daemon deduplicates
                    // (Deliver's CommandEnvelope). A raw Send or
                    // CreateSession whose response was lost may already have
                    // run: no matter whether a reviver exists, succeeds, or
                    // fails, the ONLY truthful answer is outcome-unknown —
                    // revival heals the daemon, it never resolves what the
                    // first attempt did. Safety over seamlessness.
                    if !request.safe_to_retry_after_transport_failure() {
                        let revival = match self.reviver.get() {
                            Some(reviver) => match reviver.revive().await {
                                Ok(()) => "the runtime was revived".to_string(),
                                Err(revive_error) => {
                                    format!("revival also failed: {revive_error}")
                                }
                            },
                            None => "no reviver installed".to_string(),
                        };
                        return Err(TransportError::OutcomeUnknown(format!(
                            "the local runtime connection failed mid-request; the request was \
                             NOT replayed because its first attempt may already have taken \
                             effect ({error}; {revival})"
                        )));
                    }
                    // Safe request: revive, then replay exactly once.
                    let Some(reviver) = self.reviver.get() else {
                        return Err(error);
                    };
                    reviver
                        .revive()
                        .await
                        .map_err(TransportError::Unavailable)?;
                    request_endpoint(&self.endpoint, request).await
                }
            }
        }

        async fn ensure_session_subscription(
            &self,
            session_id: &SessionId,
        ) -> Result<broadcast::Sender<RuntimeEvent>, TransportError> {
            if let Some(events) = self.session_events.lock().unwrap().get(session_id).cloned() {
                return Ok(events);
            }
            let stream =
                open_subscription(&self.endpoint, Some(session_id.clone()), self.client_kind)
                    .await?;
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
                self.client_kind,
                self.reviver.clone(),
            ));
            Ok(events)
        }

        pub async fn create_session(
            &self,
            request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            match self
                .request(WireRequest::CreateSession {
                    request,
                    client_kind: self.client_kind,
                })
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
        async fn attach_session_policy(
            &self,
            session_id: &SessionId,
            policy: ApprovalPolicy,
        ) -> Result<(), ClientError> {
            match self
                .request(WireRequest::AttachSessionPolicy {
                    session_id: session_id.clone(),
                    approval_policy: policy,
                    client_kind: self.client_kind,
                })
                .await
                .map_err(transport_client_error)?
            {
                WireResponse::Ack => Ok(()),
                WireResponse::Error(error) => Err(error.into_client_error()),
                response => Err(unexpected_response(response)),
            }
        }

        async fn create_session(
            &self,
            request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            LocalSocketRuntimeClient::create_session(self, request).await
        }

        /// Ask the daemon, because a sidecar process cannot see this machine's
        /// terminals from inside its own.
        async fn local_waiter_count(&self) -> Result<usize, ClientError> {
            match self
                .request(WireRequest::LocalWaiters)
                .await
                .map_err(transport_client_error)?
            {
                WireResponse::LocalWaiters(count) => Ok(count),
                WireResponse::Error(error) => Err(error.into_client_error()),
                response => Err(unexpected_response(response)),
            }
        }

        /// Ask the daemon for its identity. A daemon predating the request
        /// (or a transport failure) surfaces as `Err`, which callers treat as
        /// "identity unknown" — discovery still works, verification degrades.
        async fn runtime_info(&self) -> Result<leveler_client_protocol::RuntimeInfo, ClientError> {
            match self
                .request(WireRequest::RuntimeInfo)
                .await
                .map_err(transport_client_error)?
            {
                WireResponse::RuntimeInfo(info) => Ok(info),
                WireResponse::Error(error) => Err(error.into_client_error()),
                response => Err(unexpected_response(response)),
            }
        }

        async fn fetch_attachment(&self, sha256: &str) -> Result<AttachmentBytes, ClientError> {
            match self
                .request(WireRequest::FetchAttachment {
                    sha256: sha256.to_string(),
                })
                .await
                .map_err(transport_client_error)?
            {
                WireResponse::Attachment {
                    mime_type,
                    data_base64,
                } => {
                    use base64::Engine as _;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data_base64.as_bytes())
                        .map_err(|error| ClientError::Runtime(error.to_string()))?;
                    Ok(AttachmentBytes { mime_type, bytes })
                }
                WireResponse::Error(error) => Err(error.into_client_error()),
                response => Err(unexpected_response(response)),
            }
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
            let client_kind = self.client_kind;
            let reviver = self.reviver.clone();
            tokio::spawn(async move {
                loop {
                    match open_subscription(&endpoint, Some(session_id.clone()), client_kind).await
                    {
                        Ok(stream) => {
                            subscription_loop(
                                endpoint,
                                stream,
                                events,
                                Some(session_id),
                                shutdown,
                                client_kind,
                                reviver,
                            )
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
        client_kind: ClientKind,
    ) -> Result<ClientStream, TransportError> {
        let mut stream = connect_endpoint(endpoint).await?;
        write_frame(
            &mut stream,
            &ProtocolEnvelope::wrap(WireRequest::Subscribe {
                session_id,
                client_kind,
            }),
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
        client_kind: ClientKind,
        reviver: Arc<std::sync::OnceLock<Arc<dyn RuntimeReviver>>>,
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
                match open_subscription(&endpoint, session_id.clone(), client_kind).await {
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
                    Err(_) => {
                        // Nobody is answering: if a reviver is installed, try
                        // to bring the daemon back (idempotent ensure; losing
                        // a concurrent revival race is fine). Then retry.
                        if let Some(reviver) = reviver.get()
                            && let Err(error) = reviver.revive().await
                        {
                            tracing::warn!(%error, "local runtime revival failed; retrying");
                        }
                        tokio::select! {
                            _ = shutdown.cancelled() => return,
                            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                        }
                    }
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
        local_waiters: LocalWaiters,
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
        // A loopback TCP peer is authenticated by the bearer token but is never
        // trusted to grant AutoApprove — it is treated as Remote regardless of
        // the ClientKind it declares.
        handle_connection(
            stream,
            runtime,
            shutdown,
            local_waiters,
            /*transport_trusted*/ false,
        )
        .await
    }

    /// A loopback TCP runtime server. Every connection presents a shared bearer
    /// token before any request is served; only loopback peers are accepted.
    pub struct TcpRuntimeServer {
        listener: TcpListener,
        runtime: Arc<dyn LocalRuntimeService>,
        token: Arc<str>,
        local_waiters: LocalWaiters,
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
                local_waiters: LocalWaiters::new(),
            })
        }

        /// The actually-bound address (resolves an ephemeral `:0` port).
        pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
            Ok(self.listener.local_addr()?)
        }

        /// Live count of attached local interactive UIs. See
        /// [`LocalSocketServer::local_waiters`].
        pub fn local_waiters(&self) -> LocalWaiters {
            self.local_waiters.clone()
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
                        let waiters = self.local_waiters.clone();
                        tasks.spawn(async move {
                            if let Err(error) =
                                serve_authenticated_tcp(
                                    stream,
                                    &token,
                                    runtime,
                                    connection_shutdown,
                                    waiters,
                                )
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

        /// The Unix build asks the daemon, because a sidecar process cannot
        /// see this machine's terminals from inside its own. Here there is no
        /// daemon to ask, so this reports unavailable like every other method
        /// on this stub rather than inventing a count. Both callers already
        /// read the error correctly: remote-agent approvals assume a person is
        /// watching, and project reattach treats the handle as dead.
        async fn local_waiter_count(&self) -> Result<usize, ClientError> {
            Err(ClientError::Runtime(
                "Unix sockets are not supported on this platform".to_string(),
            ))
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
        /// CommandEnvelope ids received via deliver (dedup regression).
        deliveries: Mutex<Vec<leveler_client_protocol::CommandId>>,
        /// How many CreateSession mutations ran (replay regression).
        creates: std::sync::atomic::AtomicUsize,
        /// The approval policy the runtime actually received on the last
        /// CreateSession — lets a test assert the daemon boundary normalized it.
        last_approval: Mutex<Option<ApprovalPolicy>>,
        /// The policy received on the last AttachSessionPolicy (R006 R6-P2).
        last_attach: Mutex<Option<ApprovalPolicy>>,
        snapshot: Arc<Mutex<UiSessionSnapshot>>,
    }

    impl TestRuntime {
        fn new() -> Self {
            let (events, _) = broadcast::channel(32);
            Self {
                events,
                commands: Mutex::new(Vec::new()),
                deliveries: Mutex::new(Vec::new()),
                creates: std::sync::atomic::AtomicUsize::new(0),
                last_approval: Mutex::new(None),
                last_attach: Mutex::new(None),
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
                    recaps: Vec::new(),
                    user_shells: Vec::new(),
                    completion_report: None,
                    reasoning: None,
                    work_profile: None,
                    collaboration: None,
                })),
            }
        }
    }

    #[async_trait]
    impl InteractiveRuntimeClient for TestRuntime {
        async fn deliver(
            &self,
            envelope: leveler_client_protocol::CommandEnvelope,
        ) -> Result<(), ClientError> {
            // Mirror the daemon's receipt dedup: the SAME command_id runs the
            // logical mutation at most once.
            let mut deliveries = self.deliveries.lock().unwrap();
            if !deliveries.contains(&envelope.command_id) {
                deliveries.push(envelope.command_id.clone());
            }
            Ok(())
        }

        async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
            // (deliver below records envelope ids; raw send records commands)
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
        async fn attach_session_policy(
            &self,
            _session_id: &SessionId,
            policy: ApprovalPolicy,
        ) -> Result<(), ClientError> {
            *self.last_attach.lock().unwrap() = Some(policy);
            Ok(())
        }

        async fn create_session(
            &self,
            request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            *self.last_approval.lock().unwrap() = Some(request.approval_policy);
            self.creates
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(SessionBootstrap {
                session: self.snapshot.lock().unwrap().clone(),
                context_window: 128_000,
            })
        }

        async fn runtime_info(&self) -> Result<leveler_client_protocol::RuntimeInfo, ClientError> {
            Ok(leveler_client_protocol::RuntimeInfo {
                runtime_id: leveler_client_protocol::RuntimeId::new("rt-test"),
                version: "test".to_string(),
                pid: std::process::id(),
                health: leveler_client_protocol::RuntimeHealth {
                    accepting_work: true,
                    active_turns: 0,
                    turn_capacity: Some(4),
                    shutting_down: false,
                },
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
            WireRequest::CreateSession {
                request: CreateSessionRequest {
                    approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
                    goal: "tcp session".to_string(),
                    model: None,
                    mode: PermissionProfile::Assisted,
                },
                client_kind: ClientKind::LocalInteractive,
            },
        )
        .await
        .unwrap();
        assert!(matches!(response, WireResponse::SessionCreated(_)));
        shutdown.cancel();
    }

    #[tokio::test]
    async fn concurrent_binds_on_a_stale_socket_elect_exactly_one_server() {
        // Leave a stale socket file behind (dead daemon): every contender
        // sees "exists, nobody answers" — the historical double-bind window.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        {
            let runtime: Arc<dyn LocalRuntimeService> = Arc::new(TestRuntime::new());
            let server = LocalSocketServer::bind(&path, runtime).await.unwrap();
            // Simulate a crash: forget the server without letting Drop clean
            // up the file (std::mem::forget would leak the listener; instead
            // recreate the stale state by hand after a clean drop).
            drop(server);
        }
        std::os::unix::net::UnixListener::bind(&path).unwrap(); // stale, no accept loop after drop below
        // (bound listener dropped immediately; the file stays)
        let contenders = 6;
        let mut join = tokio::task::JoinSet::new();
        for _ in 0..contenders {
            let path = path.clone();
            join.spawn(async move {
                let runtime: Arc<dyn LocalRuntimeService> = Arc::new(TestRuntime::new());
                LocalSocketServer::bind(&path, runtime).await
            });
        }
        let mut winners = Vec::new();
        let mut losers = 0;
        while let Some(result) = join.join_next().await {
            match result.unwrap() {
                Ok(server) => winners.push(server),
                Err(TransportError::AlreadyRunning(_)) => losers += 1,
                Err(other) => panic!("unexpected bind failure: {other}"),
            }
        }
        assert_eq!(
            winners.len(),
            1,
            "exactly one contender may own the socket ({losers} refused)"
        );
        // And the winner is actually reachable.
        let server = winners.pop().unwrap();
        let shutdown = CancellationToken::new();
        tokio::spawn(server.serve(shutdown.clone()));
        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        let info = LocalRuntimeService::runtime_info(&client).await.unwrap();
        assert_eq!(info.runtime_id.as_str(), "rt-test");
        shutdown.cancel();
    }

    /// A reviver that performs a REAL ensure: binds a fresh LocalSocketServer
    /// for the same path and serves it. Called only by the client's genuine
    /// failure paths — the test itself never restarts anything.
    struct TestReviver {
        path: PathBuf,
        runtime: Arc<TestRuntime>,
        calls: std::sync::atomic::AtomicUsize,
        shutdown: CancellationToken,
    }

    #[async_trait]
    impl RuntimeReviver for TestReviver {
        async fn revive(&self) -> Result<(), String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Idempotent ensure with a short retry (a dying predecessor may
            // still be tearing down its socket): a live daemon answering the
            // probe returns AlreadyRunning, which is success for a reviver.
            for _ in 0..40 {
                match LocalSocketServer::bind(
                    &self.path,
                    self.runtime.clone() as Arc<dyn LocalRuntimeService>,
                )
                .await
                {
                    Ok(server) => {
                        tokio::spawn(server.serve(self.shutdown.clone()));
                        return Ok(());
                    }
                    Err(TransportError::AlreadyRunning(_)) => return Ok(()),
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                }
            }
            Err("could not rebind the runtime socket".to_string())
        }
    }

    /// The transport's REAL failure path invokes the reviver, the daemon
    /// comes back, and the same client object reaches the same runtime —
    /// requests and the event stream both recover.
    #[tokio::test]
    async fn runtime_reviver_restarts_dead_daemon_for_connected_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let first_shutdown = CancellationToken::new();
        let serve = tokio::spawn(server.serve(first_shutdown.clone()));

        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        let reviver = Arc::new(TestReviver {
            path: path.clone(),
            runtime: runtime.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            shutdown: CancellationToken::new(),
        });
        client.set_reviver(reviver.clone());

        // Kill the daemon. The test does NOT restart it.
        first_shutdown.cancel();
        let _ = serve.await;

        // A safe request through the real failure path: fails → reviver runs
        // the ensure → retried → succeeds against the revived daemon. A
        // short outer retry absorbs scheduler races between the old server's
        // teardown and the revived bind under a parallel test load.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let info = loop {
            match LocalRuntimeService::runtime_info(&client).await {
                Ok(info) => break info,
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await
                }
                Err(error) => panic!("revival never produced a reachable runtime: {error}"),
            }
        };
        assert_eq!(info.runtime_id.as_str(), "rt-test");
        assert!(
            reviver.calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "the transport must have invoked the reviver"
        );
        let session = SessionId::new("s1");
        assert_eq!(client.snapshot(&session).await.unwrap().id, session);

        // The event stream also recovers: an event emitted by the revived
        // runtime reaches the same client's subscription.
        let mut rx = client.subscribe();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let _ = runtime.events.send(RuntimeEvent::Notification {
                level: leveler_client_protocol::NotificationLevel::Info,
                message: "revived".to_string(),
            });
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(_)) => break,
                _ if tokio::time::Instant::now() < deadline => continue,
                _ => panic!("the subscription never recovered after revival"),
            }
        }
        reviver.shutdown.cancel();
    }

    /// A dead-end listener: accepts connections, fully reads one request
    /// frame (the "mutation may have run" moment), counts it, and drops the
    /// connection without answering. Exits and removes its socket after the
    /// first connection so a reviver can bind the real server.
    fn deadend_server(path: &Path) -> Arc<std::sync::atomic::AtomicUsize> {
        let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = received.clone();
        let path = path.to_path_buf();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            use std::io::Read;
            if let Ok((mut stream, _)) = listener.accept() {
                let mut len = [0u8; 4];
                if stream.read_exact(&mut len).is_ok() {
                    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
                    let _ = stream.read_exact(&mut body);
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                // Drop without responding: the outcome is unknown.
            }
            let _ = std::fs::remove_file(&path);
        });
        received
    }

    /// Scenario: the daemon received CreateSession and may have created the
    /// session, but the response was lost. The client revives the daemon but
    /// must NOT replay the mutation — outcome-unknown error, and the revived
    /// runtime never runs a second CreateSession.
    #[tokio::test]
    async fn create_session_is_not_replayed_after_uncertain_transport_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");

        // Boot: a real server so the client (and its subscription) connects.
        let boot = Arc::new(TestRuntime::new());
        let boot_server = LocalSocketServer::bind(&path, boot.clone()).await.unwrap();
        let boot_shutdown = CancellationToken::new();
        let boot_serve = tokio::spawn(boot_server.serve(boot_shutdown.clone()));
        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        boot_shutdown.cancel();
        let _ = boot_serve.await;

        // The dead end now owns the path: it fully READS the CreateSession
        // frame (the mutation-may-have-run moment), counts it, and drops the
        // connection without answering.
        let received = deadend_server(&path);

        // The healthy runtime the reviver will bring back.
        let healthy = Arc::new(TestRuntime::new());
        let reviver = Arc::new(TestReviver {
            path: path.clone(),
            runtime: healthy.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            shutdown: CancellationToken::new(),
        });
        client.set_reviver(reviver.clone());

        let error = client
            .create_session(CreateSessionRequest {
                approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
                goal: "must not duplicate".to_string(),
                model: None,
                mode: PermissionProfile::Assisted,
            })
            .await
            .expect_err("an uncertain CreateSession must fail, not replay");
        assert!(
            error.to_string().contains("not replayed"),
            "outcome-unknown must be named: {error}"
        );
        assert_eq!(
            received.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the first attempt reached a server exactly once"
        );
        assert!(
            reviver.calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "revival must still have healed the daemon"
        );
        assert_eq!(
            healthy.creates.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the revived runtime must never run a replayed CreateSession"
        );
        // Safe requests work against the revived daemon (same client object).
        assert!(LocalRuntimeService::runtime_info(&client).await.is_ok());
        reviver.shutdown.cancel();
    }

    /// Deliver (CommandEnvelope) IS replayed after revival — with the SAME
    /// command_id, so the daemon's receipt dedup keeps the logical mutation
    /// at most once.
    #[tokio::test]
    async fn deliver_envelope_can_retry_after_revival_without_duplicate_effect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let first_shutdown = CancellationToken::new();
        let serve = tokio::spawn(server.serve(first_shutdown.clone()));
        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        let reviver = Arc::new(TestReviver {
            path: path.clone(),
            runtime: runtime.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            shutdown: CancellationToken::new(),
        });
        client.set_reviver(reviver.clone());
        first_shutdown.cancel();
        let _ = serve.await;

        let command_id = leveler_client_protocol::CommandId::new("cmd-retry-1");
        let envelope = leveler_client_protocol::CommandEnvelope {
            command_id: command_id.clone(),
            session_id: SessionId::new("s1"),
            expected_version: None,
            issued_at: "2026-08-07T00:00:00Z".to_string(),
            command: ClientCommand::CancelCurrentTurn {
                session_id: SessionId::new("s1"),
            },
        };
        // Dead daemon → fail → revive → REPLAY THE SAME ENVELOPE once.
        client.deliver(envelope).await.unwrap();
        assert!(reviver.calls.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        let deliveries = runtime.deliveries.lock().unwrap();
        assert_eq!(
            deliveries.as_slice(),
            &[command_id],
            "exactly one logical delivery, with the ORIGINAL command id"
        );
        reviver.shutdown.cancel();
    }

    /// Raw Send has no idempotency key: after an uncertain failure it fails
    /// outcome-unknown and is never replayed.
    #[tokio::test]
    async fn unsafe_request_without_reviver_is_outcome_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        let boot = Arc::new(TestRuntime::new());
        let boot_server = LocalSocketServer::bind(&path, boot.clone()).await.unwrap();
        let boot_shutdown = CancellationToken::new();
        let boot_serve = tokio::spawn(boot_server.serve(boot_shutdown.clone()));
        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        boot_shutdown.cancel();
        let _ = boot_serve.await;

        // The dead end fully reads the request and drops the connection; NO
        // reviver is installed.
        let received = deadend_server(&path);
        let error = client
            .create_session(CreateSessionRequest {
                approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
                goal: "must not duplicate".to_string(),
                model: None,
                mode: PermissionProfile::Assisted,
            })
            .await
            .expect_err("uncertain CreateSession without a reviver must fail");
        let text = error.to_string();
        assert!(
            text.contains("outcome unknown") && text.contains("NOT replayed"),
            "outcome-unknown is the primary semantic even without a reviver: {text}"
        );
        assert_eq!(received.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A reviver that always fails: revival failure must NOT downgrade the
    /// mutation-outcome uncertainty to a plain Unavailable.
    struct FailingReviver;

    #[async_trait]
    impl RuntimeReviver for FailingReviver {
        async fn revive(&self) -> Result<(), String> {
            Err("revival unavailable in this test".to_string())
        }
    }

    #[tokio::test]
    async fn unsafe_request_stays_outcome_unknown_when_revival_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let serve = tokio::spawn(server.serve(shutdown.clone()));
        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        client.set_reviver(Arc::new(FailingReviver));
        shutdown.cancel();
        let _ = serve.await;

        let error = client
            .send(ClientCommand::SubmitMessage {
                session_id: SessionId::new("s1"),
                content: "must not duplicate".to_string(),
                attachments: vec![],
            })
            .await
            .expect_err("uncertain raw Send with a failing reviver must fail");
        let text = error.to_string();
        assert!(
            text.contains("outcome unknown") && text.contains("NOT replayed"),
            "OutcomeUnknown must not be overridden by the revival failure: {text}"
        );
        assert!(
            !text.starts_with("runtime error: local transport is unavailable"),
            "revival failure must not become the primary error: {text}"
        );
        assert!(
            runtime.commands.lock().unwrap().is_empty(),
            "the request must not have been replayed"
        );
    }

    #[tokio::test]
    async fn raw_send_is_not_replayed_after_uncertain_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let first_shutdown = CancellationToken::new();
        let serve = tokio::spawn(server.serve(first_shutdown.clone()));
        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        let reviver = Arc::new(TestReviver {
            path: path.clone(),
            runtime: runtime.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            shutdown: CancellationToken::new(),
        });
        client.set_reviver(reviver.clone());
        first_shutdown.cancel();
        let _ = serve.await;

        let error = client
            .send(ClientCommand::SubmitMessage {
                session_id: SessionId::new("s1"),
                content: "must not duplicate".to_string(),
                attachments: vec![],
            })
            .await
            .expect_err("an uncertain raw Send must fail, not replay");
        assert!(error.to_string().contains("not replayed"), "{error}");
        assert!(
            runtime.commands.lock().unwrap().is_empty(),
            "the revived runtime must not have received a replayed Send"
        );
        reviver.shutdown.cancel();
    }

    #[tokio::test]
    async fn runtime_info_round_trips_over_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        tokio::spawn(server.serve(shutdown.clone()));

        let client = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        let info = LocalRuntimeService::runtime_info(&client).await.unwrap();
        assert_eq!(info.runtime_id.as_str(), "rt-test");
        assert_eq!(info.version, "test");
        shutdown.cancel();
    }

    // ── Approval-policy trust boundary (Merge Blocker Closeout: B-1 / M-1) ──
    // AutoApprove is a trusted-local privilege: only a LocalInteractive client on
    // the trusted-local Unix socket may request it. A Remote client (the
    // remote-agent bridge) or any TCP peer is rejected — never silently elevated.

    use leveler_client_protocol::ApprovalPolicy;

    fn create_req(policy: ApprovalPolicy) -> CreateSessionRequest {
        CreateSessionRequest {
            goal: "s".to_string(),
            model: None,
            mode: PermissionProfile::Assisted,
            approval_policy: policy,
        }
    }

    /// S4 — a trusted-local interactive client may create an AutoApprove session,
    /// and the runtime actually receives AutoApprove.
    #[cfg(unix)]
    #[tokio::test]
    async fn trusted_local_may_create_an_auto_approve_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        tokio::spawn(server.serve(shutdown.clone()));

        let client = LocalSocketRuntimeClient::connect_as(&path, ClientKind::LocalInteractive)
            .await
            .unwrap();
        client
            .create_session(create_req(ApprovalPolicy::AutoApprove))
            .await
            .unwrap();
        assert_eq!(
            *runtime.last_approval.lock().unwrap(),
            Some(ApprovalPolicy::AutoApprove),
            "trusted-local auto-approve must reach the runtime as AutoApprove"
        );
        shutdown.cancel();
    }

    /// S1 / §11 — a Remote client (the bridge's connection kind) that explicitly
    /// requests AutoApprove is REJECTED, and the request never reaches the runtime.
    #[cfg(unix)]
    #[tokio::test]
    async fn remote_client_cannot_create_an_auto_approve_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        tokio::spawn(server.serve(shutdown.clone()));

        let bridge = LocalSocketRuntimeClient::connect_as(&path, ClientKind::Remote)
            .await
            .unwrap();
        let result = bridge
            .create_session(create_req(ApprovalPolicy::AutoApprove))
            .await;
        assert!(
            result.is_err(),
            "a remote client requesting auto-approve must be rejected: {result:?}"
        );
        assert_eq!(
            runtime.creates.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the rejected create must never have reached the runtime"
        );
        assert!(runtime.last_approval.lock().unwrap().is_none());
        shutdown.cancel();
    }

    /// R006 R6-P2 — the resume door is gated exactly like the create door:
    /// a trusted-local client may re-assert AutoApprove on an existing
    /// session; a Remote client (or any TCP origin) is rejected observably.
    #[cfg(unix)]
    #[tokio::test]
    async fn trusted_local_may_attach_auto_approve_but_remote_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        tokio::spawn(server.serve(shutdown.clone()));
        let sid = SessionId::new("s1");

        // Trusted local: honored, reaches the runtime.
        let local = LocalSocketRuntimeClient::connect_as(&path, ClientKind::LocalInteractive)
            .await
            .unwrap();
        local
            .attach_session_policy(&sid, ApprovalPolicy::AutoApprove)
            .await
            .unwrap();
        assert_eq!(
            *runtime.last_attach.lock().unwrap(),
            Some(ApprovalPolicy::AutoApprove)
        );

        // Remote kind: rejected, never reaches the runtime.
        *runtime.last_attach.lock().unwrap() = None;
        let bridge = LocalSocketRuntimeClient::connect_as(&path, ClientKind::Remote)
            .await
            .unwrap();
        let result = bridge
            .attach_session_policy(&sid, ApprovalPolicy::AutoApprove)
            .await;
        assert!(
            result.is_err(),
            "a remote client must not re-assert auto-approve: {result:?}"
        );
        assert!(runtime.last_attach.lock().unwrap().is_none());

        // Remote asserting Interactive is allowed (downgrade direction is safe).
        bridge
            .attach_session_policy(&sid, ApprovalPolicy::Interactive)
            .await
            .unwrap();
        assert_eq!(
            *runtime.last_attach.lock().unwrap(),
            Some(ApprovalPolicy::Interactive)
        );
        shutdown.cancel();
    }

    /// S8 — a Remote client requesting the safe Interactive policy (or omitting
    /// it, which defaults to Interactive) is allowed and stays Interactive.
    #[cfg(unix)]
    #[tokio::test]
    async fn remote_client_may_create_an_interactive_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        let runtime = Arc::new(TestRuntime::new());
        let server = LocalSocketServer::bind(&path, runtime.clone())
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        tokio::spawn(server.serve(shutdown.clone()));

        let bridge = LocalSocketRuntimeClient::connect_as(&path, ClientKind::Remote)
            .await
            .unwrap();
        bridge
            .create_session(create_req(ApprovalPolicy::Interactive))
            .await
            .unwrap();
        assert_eq!(
            *runtime.last_approval.lock().unwrap(),
            Some(ApprovalPolicy::Interactive)
        );
        shutdown.cancel();
    }

    /// S3 — a TCP peer is never trusted to grant AutoApprove, whatever ClientKind
    /// it declares: the transport itself is untrusted.
    #[tokio::test]
    async fn tcp_origin_cannot_elevate_to_auto_approve() {
        let (addr, runtime, shutdown) = tcp_server("s3cret-token").await;
        let response = tcp_request(
            addr,
            "s3cret-token",
            WireRequest::CreateSession {
                request: create_req(ApprovalPolicy::AutoApprove),
                // Even a spoofed LocalInteractive is overridden by the untrusted
                // transport ceiling.
                client_kind: ClientKind::LocalInteractive,
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(response, WireResponse::Error(_)),
            "TCP auto-approve must be rejected: {response:?}"
        );
        assert_eq!(
            runtime.creates.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the rejected TCP create must never have reached the runtime"
        );
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
                approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
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

    /// The remote approval timeout must not fire while someone is at the
    /// keyboard, so the daemon has to know how many local UIs are attached —
    /// and must not count a phone's bridge among them.
    #[cfg(unix)]
    #[tokio::test]
    async fn local_waiters_counts_local_uis_and_excludes_the_remote_agent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sock");
        let server = LocalSocketServer::bind(&path, Arc::new(TestRuntime::new()))
            .await
            .unwrap();
        let waiters = server.local_waiters();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.serve(shutdown.clone()));

        assert_eq!(waiters.count(), 0);

        let tui = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        wait_for_waiters(&waiters, 1).await;

        let web = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        wait_for_waiters(&waiters, 2).await;

        // The agent bridging a paired device is not a person at this machine.
        let agent = LocalSocketRuntimeClient::connect_as(&path, ClientKind::Remote)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            waiters.count(),
            2,
            "a remote subscriber must not be counted as a local waiter"
        );

        // Dropping decrements even though the client never said goodbye.
        drop(tui);
        wait_for_waiters(&waiters, 1).await;
        drop(web);
        wait_for_waiters(&waiters, 0).await;

        drop(agent);
        shutdown.cancel();
        let _ = task.await;
    }

    /// The remote agent runs in its own process, so the only way it can know
    /// whether a person is at this machine is to ask the daemon.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_daemon_reports_its_local_waiter_count_to_a_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sock");
        let server = LocalSocketServer::bind(&path, Arc::new(TestRuntime::new()))
            .await
            .unwrap();
        let waiters = server.local_waiters();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.serve(shutdown.clone()));

        // The agent's own connection must not inflate the count it reads.
        let agent = LocalSocketRuntimeClient::connect_as(&path, ClientKind::Remote)
            .await
            .unwrap();
        assert_eq!(agent.local_waiter_count().await.unwrap(), 0);

        let tui = LocalSocketRuntimeClient::connect(&path).await.unwrap();
        wait_for_waiters(&waiters, 1).await;
        assert_eq!(agent.local_waiter_count().await.unwrap(), 1);

        drop(tui);
        wait_for_waiters(&waiters, 0).await;
        assert_eq!(
            agent.local_waiter_count().await.unwrap(),
            0,
            "a terminal that closed must stop holding the timeout disarmed"
        );

        drop(agent);
        shutdown.cancel();
        let _ = task.await;
    }

    /// An older client omits `client_kind` entirely. It must still parse, and
    /// must count as local: suppressing an auto-deny is recoverable, expiring a
    /// prompt somebody was answering is not.
    #[cfg(unix)]
    #[test]
    fn a_subscribe_without_client_kind_defaults_to_local() {
        let request: WireRequest =
            serde_json::from_str(r#"{"type":"subscribe","body":{"session_id":null}}"#).unwrap();
        let WireRequest::Subscribe {
            client_kind,
            session_id,
        } = request
        else {
            panic!("expected subscribe");
        };
        assert_eq!(session_id, None);
        assert_eq!(client_kind, ClientKind::LocalInteractive);
    }

    #[cfg(unix)]
    async fn wait_for_waiters(waiters: &LocalWaiters, expected: usize) {
        for _ in 0..100 {
            if waiters.count() == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("expected {expected} local waiters, saw {}", waiters.count());
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
                approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
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
