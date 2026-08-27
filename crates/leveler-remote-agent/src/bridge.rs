//! The admission path: what a frame from a phone must survive to reach the
//! runtime.
//!
//! Ordered so that each check runs only on input the previous one vouched for:
//!
//! 1. **Trust** — resolve the device's key from [`TrustedDevices`], the local
//!    store. A relay-supplied key is compared, never used.
//! 2. **Signature** — verify the envelope against that key, including its
//!    audience and freshness. Everything after this point is attributable.
//! 3. **Parse** — decode the inner session message. Still untrusted *content*,
//!    but now of known origin.
//! 4. **Policy** — the exhaustive capability gate.
//! 5. **Deliver** — hand to the runtime through `deliver_protocol`, the same
//!    entry point local clients use, so idempotency receipts and version
//!    checks are not re-implemented here.
//!
//! Nothing short-circuits: a frame that fails at any step is refused with a
//! wire code and never reaches step 5.

use std::sync::Arc;

use leveler_client_protocol::{
    ClientCommand, ClientOrigin, CommandEnvelope, CommandId, ProtocolEnvelope, SessionId,
};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService};
use leveler_remote_protocol::pairing::PairingScope;
use leveler_remote_protocol::policy::{RemotePolicy, RemoteVerdict};
use leveler_remote_protocol::tunnel::{RpcMethod, RpcRequestPayload};
use leveler_remote_protocol::{
    ContentType, Sender, SignedEnvelope, SigningKey, VerifyParams, VerifyingKey,
};
use leveler_session_wire::UpstreamMessage;

use crate::attachments::{
    ChunkOutcome, FETCH_CHUNK_BYTES, FetchChunkRequest, FetchChunkResponse, MAX_ATTACHMENT_BYTES,
    UploadChunk, UploadError, Uploads, is_sha256_hex,
};
use crate::audit::{AuditEvent, AuditLog, hashed};
use crate::devices::{TrustError, TrustedDevices};
use crate::projects::{ProjectRoutes, RouteError};

/// Why an inbound frame was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    #[error("{0}")]
    Trust(#[from] TrustError),
    #[error("{0}")]
    Route(#[from] RouteError),
    #[error("envelope rejected: {0}")]
    Envelope(#[from] leveler_remote_protocol::EnvelopeError),
    #[error("payload is not a valid session message")]
    MalformedPayload,
    #[error("{reason}")]
    Refused {
        code: &'static str,
        reason: &'static str,
    },
    #[error("runtime rejected the command: {0}")]
    Runtime(String),
    #[error("{0}")]
    Upload(#[from] UploadError),
}

impl AdmissionError {
    pub fn code(&self) -> &'static str {
        match self {
            AdmissionError::Trust(error) => error.code(),
            AdmissionError::Route(error) => error.code(),
            AdmissionError::Envelope(error) => error.code(),
            AdmissionError::MalformedPayload => "invalid_frame",
            AdmissionError::Refused { code, .. } => code,
            AdmissionError::Runtime(_) => "runtime_error",
            AdmissionError::Upload(error) => error.code(),
        }
    }
}

/// What an admitted frame turned into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admitted {
    /// Delivered to the runtime under this command id.
    Delivered {
        command_id: String,
        origin: ClientOrigin,
    },
    /// A snapshot was requested for this session.
    SnapshotRequested { session_id: SessionId },
}

/// Everything the bridge needs to judge one host's inbound frames.
///
/// One bridge per machine, not per repository: the pairing is with the machine,
/// and which project a frame belongs to is a routing question answered by
/// [`ProjectRoutes`] after the frame has proved who sent it.
pub struct AgentBridge {
    routes: Arc<dyn ProjectRoutes>,
    devices: TrustedDevices,
    runtime_id: String,
    /// Signs every response carrying a business body, so the APP can tell a
    /// genuine result from one a relay composed.
    runtime_key: SigningKey,
    allow_full_access: bool,
    /// Where remote activity is written down. Absent in tests that are not
    /// about the record.
    audit: Option<Arc<AuditLog>>,
    /// Attachments arriving in pieces, and what each session has accepted.
    uploads: Uploads,
}

impl AgentBridge {
    pub fn new(
        routes: Arc<dyn ProjectRoutes>,
        devices: TrustedDevices,
        runtime_id: impl Into<String>,
        runtime_key: SigningKey,
        allow_full_access: bool,
    ) -> Self {
        Self {
            routes,
            devices,
            runtime_id: runtime_id.into(),
            runtime_key,
            allow_full_access,
            audit: None,
            uploads: Uploads::new(),
        }
    }

    /// Write remote activity to `audit`.
    pub fn with_audit(mut self, audit: Arc<AuditLog>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// The audit log, for the tasks the bridge spawns rather than serves.
    pub(crate) fn audit_log(&self) -> Option<Arc<AuditLog>> {
        self.audit.clone()
    }

    /// Record one event, if this host keeps an audit log.
    pub(crate) fn audit(&self, event: AuditEvent) {
        if let Some(audit) = &self.audit {
            audit.record(event);
        }
    }

    /// The projects this host exposes.
    pub async fn projects(&self) -> Vec<crate::projects::ProjectInfo> {
        self.routes.projects().await
    }

    /// Resolve the project a caller named, or the only one open.
    ///
    /// A named project must exist on this host — *exist*, not be online: an id
    /// that is merely offline still binds, so a stream opened while a daemon is
    /// restarting reports `project_offline` per frame instead of the phone
    /// having to notice a closed socket. An id that is not this host's at all is
    /// refused here, at the one place the answer is knowable, rather than
    /// becoming a stream whose every frame fails.
    pub async fn resolve_project(&self, project_id: Option<&str>) -> Result<String, RouteError> {
        match project_id {
            Some(id) => {
                if self
                    .routes
                    .projects()
                    .await
                    .iter()
                    .any(|project| project.project_id == id)
                {
                    Ok(id.to_string())
                } else {
                    Err(RouteError::UnknownProject)
                }
            }
            None => self.routes.implied_project().await,
        }
    }

    /// The public half the APP anchors from the pairing QR.
    pub fn runtime_public_key(&self) -> VerifyingKey {
        self.runtime_key.verifying_key()
    }

    /// Serve one RPC and return a **runtime-signed** response.
    ///
    /// The response is signed for the same reason the upstream frame is
    /// verified: a relay must not be able to author a `SessionBootstrap` or a
    /// snapshot. Those carry session ids and pending approvals, so an APP that
    /// accepted an unsigned one would render — and let the user act on — state
    /// the relay chose. The response reuses the request's `stream_id`, which
    /// binds the pair cryptographically so a genuine response cannot be
    /// re-matched to a different request.
    pub async fn handle_rpc(
        &self,
        frame: &SignedEnvelope,
        now: &str,
        relay_claimed_pubkey: Option<&str>,
    ) -> Result<SignedEnvelope, AdmissionError> {
        let device_id = frame.sender_id.clone();
        let (key, scope) = self.devices.key_for(&device_id, relay_claimed_pubkey)?;
        let payload = frame.verify(&VerifyParams {
            expected_recipient_id: &self.runtime_id,
            public_key: &key,
            now,
        })?;

        let request: RpcRequestPayload =
            serde_json::from_slice(&payload).map_err(|_| AdmissionError::MalformedPayload)?;

        // Creating a session is a mutation, so an observe pairing may not.
        if scope == PairingScope::Observe && request.method == RpcMethod::CreateSession {
            return Err(AdmissionError::Refused {
                code: "command_not_allowed_remote",
                reason: "this device is paired for observation only",
            });
        }

        let body = match request.method {
            // Answered by the agent itself: it is the question "which projects
            // exist", so there is no one runtime to ask.
            RpcMethod::ListProjects => {
                let projects = self.routes.projects().await;
                serde_json::to_vec(&projects).map_err(|_| AdmissionError::MalformedPayload)?
            }
            RpcMethod::CreateSession => {
                // From the *signed* payload, so the project a session is created
                // in is the device's choice and not a relay's routing decision.
                let runtime = self.runtime_for(request.project_id.as_deref()).await?;
                let create: CreateSessionRequest = serde_json::from_value(request.body)
                    .map_err(|_| AdmissionError::MalformedPayload)?;
                let bootstrap = runtime
                    .create_session(create)
                    .await
                    .map_err(|error| AdmissionError::Runtime(error.to_string()))?;
                serde_json::to_vec(&bootstrap).map_err(|_| AdmissionError::MalformedPayload)?
            }
            RpcMethod::Snapshot => {
                let runtime = self.runtime_for(request.project_id.as_deref()).await?;
                let session_id = request
                    .body
                    .get("session_id")
                    .and_then(|value| value.as_str())
                    .ok_or(AdmissionError::MalformedPayload)?;
                let snapshot = runtime
                    .snapshot(&SessionId::new(session_id))
                    .await
                    .map_err(|error| AdmissionError::Runtime(error.to_string()))?;
                serde_json::to_vec(&snapshot).map_err(|_| AdmissionError::MalformedPayload)?
            }
            RpcMethod::UploadAttachment => {
                // Adding a file to a session is a mutation.
                if scope == PairingScope::Observe {
                    return Err(AdmissionError::Refused {
                        code: "command_not_allowed_remote",
                        reason: "this device is paired for observation only",
                    });
                }
                let runtime = self.runtime_for(request.project_id.as_deref()).await?;
                let chunk: UploadChunk = serde_json::from_value(request.body)
                    .map_err(|_| AdmissionError::MalformedPayload)?;
                match self.uploads.accept(&device_id, &chunk)? {
                    ChunkOutcome::Incomplete => {
                        serde_json::to_vec(&serde_json::json!({"status": "chunk_received",
                                            "chunk_index": chunk.chunk_index}))
                        .map_err(|_| AdmissionError::MalformedPayload)?
                    }
                    ChunkOutcome::Complete(bytes) => {
                        let attachment = self
                            .store_attachment(
                                runtime,
                                &frame.stream_id,
                                &chunk.session_id,
                                &chunk.name,
                                bytes,
                            )
                            .await?;
                        serde_json::to_vec(&attachment)
                            .map_err(|_| AdmissionError::MalformedPayload)?
                    }
                }
            }
            RpcMethod::FetchAttachment => {
                // A read. Observe pairings may fetch; they cannot upload.
                let runtime = self.runtime_for(request.project_id.as_deref()).await?;
                let fetch: FetchChunkRequest = serde_json::from_value(request.body)
                    .map_err(|_| AdmissionError::MalformedPayload)?;
                if !is_sha256_hex(&fetch.sha256) {
                    return Err(AdmissionError::MalformedPayload);
                }
                let blob = match runtime.fetch_attachment(&fetch.sha256).await {
                    Ok(blob) => blob,
                    Err(error) => {
                        let message = error.to_string();
                        if message.contains("attachment not found") {
                            return Err(AdmissionError::Refused {
                                code: "not_found",
                                reason: "no attachment with that sha256",
                            });
                        }
                        return Err(AdmissionError::Runtime(message));
                    }
                };
                if blob.bytes.len() > MAX_ATTACHMENT_BYTES {
                    return Err(AdmissionError::Refused {
                        code: "payload_too_large",
                        reason: "attachment exceeds the remote fetch limit",
                    });
                }
                let chunk_total =
                    std::cmp::max(1u32, blob.bytes.len().div_ceil(FETCH_CHUNK_BYTES) as u32);
                if fetch.chunk_index >= chunk_total {
                    return Err(AdmissionError::MalformedPayload);
                }
                let start = fetch.chunk_index as usize * FETCH_CHUNK_BYTES;
                let end = std::cmp::min(start + FETCH_CHUNK_BYTES, blob.bytes.len());
                let slice = &blob.bytes[start..end];
                use base64::Engine as _;
                let response = FetchChunkResponse {
                    sha256: fetch.sha256,
                    mime_type: blob.mime_type,
                    size_bytes: blob.bytes.len() as u64,
                    chunk_index: fetch.chunk_index,
                    chunk_total,
                    data_base64: base64::engine::general_purpose::STANDARD.encode(slice),
                };
                serde_json::to_vec(&response).map_err(|_| AdmissionError::MalformedPayload)?
            }
        };

        SignedEnvelope::sign(
            &self.runtime_key,
            Sender::Runtime,
            &self.runtime_id,
            &device_id,
            &frame.stream_id,
            frame.seq,
            now,
            ContentType::RpcResponse,
            &body,
        )
        .map_err(AdmissionError::Envelope)
    }

    pub fn devices(&self) -> &TrustedDevices {
        &self.devices
    }

    /// The runtime serving one project, for callers that already know which.
    pub async fn runtime_for_project(
        &self,
        project_id: &str,
    ) -> Result<Arc<dyn LocalRuntimeService>, RouteError> {
        self.routes.runtime(project_id).await
    }

    /// The runtime for a named project, or for the only open one.
    /// Hand a reassembled file to the runtime and wait for what it made of it.
    ///
    /// The bytes go in as `AddAttachmentData`, the same command a local client
    /// sends, so processing and content addressing have one implementation.
    /// The answer comes back as an *event*, which is why this subscribes first
    /// and delivers second: subscribing afterwards would race a fast runtime
    /// and lose the very event it is waiting for.
    async fn store_attachment(
        &self,
        runtime: Arc<dyn LocalRuntimeService>,
        command_id: &str,
        session_id: &str,
        name: &str,
        bytes: Vec<u8>,
    ) -> Result<leveler_client_protocol::AttachmentRef, AdmissionError> {
        use base64::Engine as _;
        use leveler_client_protocol::RuntimeEvent;

        let session = SessionId::new(session_id);
        let mut events = runtime.subscribe_session(&session);

        // The RPC's stream id is unique per request and already inside the
        // device's signature, so a retried upload carries the same command id
        // and the runtime's receipt store treats it as one command, not two.
        let command_id = CommandId::new(command_id.to_string());
        runtime
            .deliver_protocol(ProtocolEnvelope::wrap(CommandEnvelope {
                command_id,
                session_id: session.clone(),
                expected_version: None,
                issued_at: chrono::Utc::now().to_rfc3339(),
                command: ClientCommand::AddAttachmentData {
                    session_id: session.clone(),
                    name: name.to_string(),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                },
            }))
            .await
            .map_err(|error| AdmissionError::Runtime(error.to_string()))?;

        // Bounded: a runtime that never answers must not leave the phone's RPC
        // hanging until its own timeout, with no way to tell apart "still
        // processing" from "this will never finish".
        let deadline = tokio::time::Duration::from_secs(60);
        let waited = tokio::time::timeout(deadline, async {
            loop {
                match events.recv().await {
                    Ok(RuntimeEvent::AttachmentAdded { attachment }) if attachment.name == name => {
                        return Ok(attachment);
                    }
                    Ok(RuntimeEvent::AttachmentProcessingFailed { error }) => {
                        return Err(AdmissionError::Runtime(error));
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(AdmissionError::Runtime(
                            "runtime event stream closed".to_string(),
                        ));
                    }
                }
            }
        })
        .await;

        match waited {
            Ok(result) => result,
            Err(_) => Err(AdmissionError::Runtime(
                "runtime did not report the attachment in time".to_string(),
            )),
        }
    }

    async fn runtime_for(
        &self,
        project_id: Option<&str>,
    ) -> Result<Arc<dyn LocalRuntimeService>, RouteError> {
        let project_id = match project_id {
            Some(id) => id.to_string(),
            None => self.routes.implied_project().await?,
        };
        self.routes.runtime(&project_id).await
    }

    /// A session snapshot from one project, for answering a device's request.
    pub async fn snapshot(
        &self,
        project_id: &str,
        session_id: &SessionId,
    ) -> Result<leveler_client_protocol::UiSessionSnapshot, AdmissionError> {
        self.routes
            .runtime(project_id)
            .await?
            .snapshot(session_id)
            .await
            .map_err(|error| AdmissionError::Runtime(error.to_string()))
    }

    /// Subscribe to one project's runtime event stream.
    ///
    /// Per project, so a phone attached to one repository never sees another's
    /// output — the isolation the browser UI gets from subscribing per session.
    pub async fn subscribe(
        &self,
        project_id: &str,
    ) -> Result<
        tokio::sync::broadcast::Receiver<leveler_client_protocol::RuntimeEvent>,
        AdmissionError,
    > {
        Ok(self.routes.runtime(project_id).await?.subscribe())
    }

    /// Sign a bare assertion with the runtime key, for the relay registration
    /// handshake. Returns standard base64.
    pub fn sign_assertion(&self, message: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(self.runtime_key.sign_detached(message))
    }

    /// Sign an outbound frame with the runtime key.
    ///
    /// Exposed so the tunnel can sign session downstream frames without holding
    /// the key itself — one place owns it, and the transport is not it.
    #[allow(clippy::too_many_arguments)]
    pub fn sign_downstream(
        &self,
        runtime_id: &str,
        device_id: &str,
        stream_id: &str,
        seq: u64,
        ts: &str,
        content_type: ContentType,
        payload: &[u8],
    ) -> Result<SignedEnvelope, AdmissionError> {
        SignedEnvelope::sign(
            &self.runtime_key,
            Sender::Runtime,
            runtime_id,
            device_id,
            stream_id,
            seq,
            ts,
            content_type,
            payload,
        )
        .map_err(AdmissionError::Envelope)
    }

    /// Admit one upstream frame.
    ///
    /// `relay_claimed_pubkey` is whatever the relay attached, if anything; it
    /// is only ever compared against the local store.
    pub async fn admit_upstream(
        &self,
        project_id: &str,
        frame: &SignedEnvelope,
        now: &str,
        relay_claimed_pubkey: Option<&str>,
    ) -> Result<Admitted, AdmissionError> {
        let device_id = frame.sender_id.clone();
        let (key, scope) = self.devices.key_for(&device_id, relay_claimed_pubkey)?;

        // Audience and freshness are checked inside `verify`; the payload it
        // returns is the only version of the bytes this function will use.
        let payload = frame.verify(&VerifyParams {
            expected_recipient_id: &self.runtime_id,
            public_key: &key,
            now,
        })?;

        let message: UpstreamMessage =
            serde_json::from_slice(&payload).map_err(|_| AdmissionError::MalformedPayload)?;

        let policy = RemotePolicy {
            scope,
            allow_full_access: self.allow_full_access,
        };
        let origin = ClientOrigin::Remote {
            device_id: device_id.clone(),
        };

        match message {
            UpstreamMessage::Deliver {
                command_id,
                session_id,
                command,
            } => {
                let kind = command_kind(&command);
                if let RemoteVerdict::Deny { code, reason } = policy.evaluate(&command) {
                    // The kind, never the contents: a refusal is worth
                    // recording, and the text of what was refused is not.
                    tracing::info!(device = %hashed(&device_id), code, command = kind, "refused a remote command");
                    self.audit(AuditEvent::Refused {
                        device: hashed(&device_id),
                        project: project_id.to_string(),
                        command: Some(kind.to_string()),
                        code: code.to_string(),
                    });
                    return Err(AdmissionError::Refused { code, reason });
                }
                self.deliver(project_id, &command_id, &session_id, command)
                    .await?;
                self.audit(AuditEvent::Delivered {
                    device: hashed(&device_id),
                    project: project_id.to_string(),
                    command: kind.to_string(),
                });
                Ok(Admitted::Delivered { command_id, origin })
            }
            // A snapshot mutates nothing, but an observe pairing is still not
            // routed through this path — it receives snapshots pushed to it.
            UpstreamMessage::Snapshot { session_id } => Ok(Admitted::SnapshotRequested {
                session_id: SessionId::new(session_id),
            }),
        }
    }

    async fn deliver(
        &self,
        project_id: &str,
        command_id: &str,
        session_id: &str,
        command: ClientCommand,
    ) -> Result<(), AdmissionError> {
        let envelope = CommandEnvelope {
            command_id: CommandId::new(command_id),
            session_id: SessionId::new(session_id),
            expected_version: None,
            issued_at: chrono::Utc::now().to_rfc3339(),
            command,
        };
        self.routes
            .runtime(project_id)
            .await?
            .deliver_protocol(ProtocolEnvelope::wrap(envelope))
            .await
            .map_err(|error| AdmissionError::Runtime(error.to_string()))
    }
}

/// A stable label for logs and metrics. Never the command's contents — audit
/// lines must not carry message text.
fn command_kind(command: &ClientCommand) -> &'static str {
    match command {
        ClientCommand::SubmitMessage { .. } => "submit_message",
        ClientCommand::RunGoal { .. } => "run_goal",
        ClientCommand::AddAttachment { .. } => "add_attachment",
        ClientCommand::AddAttachmentData { .. } => "add_attachment_data",
        ClientCommand::AddClipboardImage { .. } => "add_clipboard_image",
        ClientCommand::CancelCurrentTurn { .. } => "cancel_current_turn",
        ClientCommand::ForceCancelCurrentTurn { .. } => "force_cancel_current_turn",
        ClientCommand::ApprovalDecision { .. } => "approval_decision",
        ClientCommand::AnswerClarification { .. } => "answer_clarification",
        ClientCommand::SelectModel { .. } => "select_model",
        ClientCommand::SetPermissionProfile { .. } => "set_permission_profile",
        ClientCommand::SetProductAxes { .. } => "set_product_axes",
        ClientCommand::ConfirmPlanToGoal { .. } => "confirm_plan_to_goal",
        ClientCommand::SteerCurrentTurn { .. } => "steer_current_turn",
        ClientCommand::ListMemory { .. } => "list_memory",
        ClientCommand::ForgetMemory { .. } => "forget_memory",
        ClientCommand::AcceptMemory { .. } => "accept_memory",
        ClientCommand::RequestDiff { .. } => "request_diff",
        ClientCommand::CompactContext { .. } => "compact_context",
        ClientCommand::ClearConversation { .. } => "clear_conversation",
        ClientCommand::RequestSessionList => "request_session_list",
        ClientCommand::RequestSessionListFor { .. } => "request_session_list_for",
        ClientCommand::NewSessionFor { .. } => "new_session_for",
        ClientCommand::OpenSession { .. } => "open_session",
        ClientCommand::OpenSessionFor { .. } => "open_session_for",
        ClientCommand::DeleteSession { .. } => "delete_session",
        ClientCommand::DeleteSessionFor { .. } => "delete_session_for",
        ClientCommand::RenameSession { .. } => "rename_session",
        ClientCommand::ArchiveSession { .. } => "archive_session",
        ClientCommand::ForkSession { .. } => "fork_session",
        ClientCommand::RestoreCheckpoint { .. } => "restore_checkpoint",
        ClientCommand::Btw { .. } => "btw",
        ClientCommand::Recap { .. } => "recap",
        ClientCommand::RunUserShell { .. } => "run_user_shell",
        ClientCommand::CancelUserShell { .. } => "cancel_user_shell",
        ClientCommand::QueryObservability { .. } => "query_observability",
        ClientCommand::QueryChildContribution { .. } => "query_child_contribution",
        ClientCommand::ListUnfinishedGoals { .. } => "list_unfinished_goals",
        ClientCommand::Quit => "quit",
    }
}
