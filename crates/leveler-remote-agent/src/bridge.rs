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

use leveler_client_protocol::{
    ClientCommand, ClientOrigin, CommandEnvelope, CommandId, InteractiveRuntimeClient,
    ProtocolEnvelope, SessionId,
};
use leveler_remote_protocol::policy::{RemotePolicy, RemoteVerdict};
use leveler_remote_protocol::{SignedEnvelope, VerifyParams};
use leveler_session_wire::UpstreamMessage;

use crate::devices::{TrustError, TrustedDevices};

/// Why an inbound frame was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    #[error("{0}")]
    Trust(#[from] TrustError),
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
}

impl AdmissionError {
    pub fn code(&self) -> &'static str {
        match self {
            AdmissionError::Trust(error) => error.code(),
            AdmissionError::Envelope(error) => error.code(),
            AdmissionError::MalformedPayload => "invalid_frame",
            AdmissionError::Refused { code, .. } => code,
            AdmissionError::Runtime(_) => "runtime_error",
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

/// Everything the bridge needs to judge one runtime's inbound frames.
pub struct AgentBridge<R: InteractiveRuntimeClient> {
    runtime: R,
    devices: TrustedDevices,
    runtime_id: String,
    allow_full_access: bool,
}

impl<R: InteractiveRuntimeClient> AgentBridge<R> {
    pub fn new(
        runtime: R,
        devices: TrustedDevices,
        runtime_id: impl Into<String>,
        allow_full_access: bool,
    ) -> Self {
        Self {
            runtime,
            devices,
            runtime_id: runtime_id.into(),
            allow_full_access,
        }
    }

    pub fn devices(&self) -> &TrustedDevices {
        &self.devices
    }

    /// Admit one upstream frame.
    ///
    /// `relay_claimed_pubkey` is whatever the relay attached, if anything; it
    /// is only ever compared against the local store.
    pub async fn admit_upstream(
        &self,
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
                if let RemoteVerdict::Deny { code, reason } = policy.evaluate(&command) {
                    tracing::info!(
                        device_id = %device_id,
                        code,
                        command = command_kind(&command),
                        "refused a remote command"
                    );
                    return Err(AdmissionError::Refused { code, reason });
                }
                self.deliver(&command_id, &session_id, command).await?;
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
        self.runtime
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
        ClientCommand::ListMemory { .. } => "list_memory",
        ClientCommand::ForgetMemory { .. } => "forget_memory",
        ClientCommand::SetAgentMode { .. } => "set_agent_mode",
        ClientCommand::RequestDiff { .. } => "request_diff",
        ClientCommand::CompactContext { .. } => "compact_context",
        ClientCommand::ClearConversation { .. } => "clear_conversation",
        ClientCommand::RequestSessionList => "request_session_list",
        ClientCommand::RequestSessionListFor { .. } => "request_session_list_for",
        ClientCommand::OpenSession { .. } => "open_session",
        ClientCommand::OpenSessionFor { .. } => "open_session_for",
        ClientCommand::DeleteSession { .. } => "delete_session",
        ClientCommand::DeleteSessionFor { .. } => "delete_session_for",
        ClientCommand::RenameSession { .. } => "rename_session",
        ClientCommand::ArchiveSession { .. } => "archive_session",
        ClientCommand::ForkSession { .. } => "fork_session",
        ClientCommand::RestoreCheckpoint { .. } => "restore_checkpoint",
        ClientCommand::Btw { .. } => "btw",
        ClientCommand::Quit => "quit",
    }
}
