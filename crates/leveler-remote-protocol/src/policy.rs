//! The remote capability gate.
//!
//! One choke point, evaluated on the agent *before* a command reaches the
//! runtime. Everything is refused unless this module names it allowed, and the
//! match is exhaustive with no wildcard arm — a new `ClientCommand` variant
//! breaks the build rather than arriving remotely by default.
//!
//! Two commands are allowed while carrying values that are not, so the payload
//! is inspected rather than just the discriminant:
//!
//! - `ApprovalDecision::ApproveAlways` is persisted by the agent executor as a
//!   rule in the repository's `permissions.yaml`. Allowing it remotely would let
//!   a phone grant standing local permission that outlives the session and the
//!   pairing.
//! - `PermissionProfile::FullAccess` disarms the approval prompt altogether, so
//!   it requires a local opt-in the remote side cannot perform for itself.

use leveler_client_protocol::{ApprovalDecision, ClientCommand, PermissionProfile};

use crate::pairing::PairingScope;

/// What the gate decided, and — when refused — the wire code to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteVerdict {
    Allow,
    Deny {
        /// From the design's error-code catalogue.
        code: &'static str,
        /// Why, in terms a client can show a user.
        reason: &'static str,
    },
}

impl RemoteVerdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, RemoteVerdict::Allow)
    }

    pub fn code(&self) -> Option<&'static str> {
        match self {
            RemoteVerdict::Allow => None,
            RemoteVerdict::Deny { code, .. } => Some(code),
        }
    }

    pub fn reason(&self) -> Option<&'static str> {
        match self {
            RemoteVerdict::Allow => None,
            RemoteVerdict::Deny { reason, .. } => Some(reason),
        }
    }
}

const DENIED_COMMAND: &str = "command_not_allowed_remote";
const DENIED_DECISION: &str = "approval_decision_not_allowed_remote";
const DENIED_PROFILE: &str = "permission_profile_not_allowed_remote";

/// The policy in force for one paired device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemotePolicy {
    pub scope: PairingScope,
    /// Set from the host's own `remote.allow_full_access`, never from anything
    /// a device or relay sends.
    pub allow_full_access: bool,
}

impl RemotePolicy {
    /// Decide whether `command` may be delivered to the runtime.
    pub fn evaluate(&self, command: &ClientCommand) -> RemoteVerdict {
        // An observe pairing issues nothing at all. Checked first so no later
        // arm can hand it a capability.
        if self.scope == PairingScope::Observe {
            return RemoteVerdict::Deny {
                code: DENIED_COMMAND,
                reason: "this device is paired for observation only",
            };
        }

        // Exhaustive on purpose: no wildcard arm, so a new variant is a
        // compile error rather than an accidental remote capability.
        match command {
            ClientCommand::SubmitMessage { .. }
            | ClientCommand::RunGoal { .. }
            | ClientCommand::CancelCurrentTurn { .. }
            | ClientCommand::ForceCancelCurrentTurn { .. }
            | ClientCommand::AnswerClarification { .. }
            | ClientCommand::SelectModel { .. }
            | ClientCommand::SetProductAxes { .. }
            | ClientCommand::ConfirmPlanToGoal { .. }
            | ClientCommand::SetAgentMode { .. }
            | ClientCommand::RequestDiff { .. }
            | ClientCommand::CompactContext { .. }
            | ClientCommand::ClearConversation { .. }
            | ClientCommand::RequestSessionList
            | ClientCommand::RequestSessionListFor { .. }
            | ClientCommand::OpenSession { .. }
            | ClientCommand::OpenSessionFor { .. }
            | ClientCommand::Btw { .. } => RemoteVerdict::Allow,

            // Allowed, but only for some payloads.
            ClientCommand::ApprovalDecision { decision, .. } => self.evaluate_decision(*decision),
            ClientCommand::SetPermissionProfile { mode, .. } => self.evaluate_profile(*mode),

            // Reach the local filesystem or the clipboard, which a remote
            // caller has no business naming.
            ClientCommand::AddAttachment { .. } | ClientCommand::AddClipboardImage { .. } => {
                RemoteVerdict::Deny {
                    code: DENIED_COMMAND,
                    reason: "attachments from a remote client go through the upload RPC",
                }
            }
            // Uploads have exactly one remote path, so this second one stays
            // shut rather than becoming an unbounded inline-data channel.
            ClientCommand::AddAttachmentData { .. } => RemoteVerdict::Deny {
                code: DENIED_COMMAND,
                reason: "attachments from a remote client go through the upload RPC",
            },

            // Memory is durable, cross-session and unreviewable from a phone.
            ClientCommand::ListMemory { .. } | ClientCommand::ForgetMemory { .. } => {
                RemoteVerdict::Deny {
                    code: DENIED_COMMAND,
                    reason: "memory is not reachable remotely",
                }
            }

            // Destructive or history-rewriting, with no remote confirmation UX
            // yet to make them safe.
            ClientCommand::DeleteSession { .. }
            | ClientCommand::DeleteSessionFor { .. }
            | ClientCommand::RenameSession { .. }
            | ClientCommand::ArchiveSession { .. }
            | ClientCommand::ForkSession { .. }
            | ClientCommand::RestoreCheckpoint { .. } => RemoteVerdict::Deny {
                code: DENIED_COMMAND,
                reason: "session management is not available remotely",
            },

            // Shuts down the runtime for every client, local ones included.
            // The local socket transport refuses it too.
            ClientCommand::Quit => RemoteVerdict::Deny {
                code: DENIED_COMMAND,
                reason: "a remote client cannot shut down the runtime",
            },
        }
    }

    fn evaluate_decision(&self, decision: ApprovalDecision) -> RemoteVerdict {
        match decision {
            ApprovalDecision::ApproveOnce
            | ApprovalDecision::ApproveSession
            | ApprovalDecision::Deny => RemoteVerdict::Allow,
            // Persisted to the repository's permissions.yaml by the executor,
            // so it would outlive both the session and the pairing.
            ApprovalDecision::ApproveAlways => RemoteVerdict::Deny {
                code: DENIED_DECISION,
                reason: "a remote client cannot grant standing permission",
            },
        }
    }

    fn evaluate_profile(&self, mode: PermissionProfile) -> RemoteVerdict {
        match mode {
            PermissionProfile::RequestApproval | PermissionProfile::Assisted => {
                RemoteVerdict::Allow
            }
            PermissionProfile::FullAccess if self.allow_full_access => RemoteVerdict::Allow,
            PermissionProfile::FullAccess => RemoteVerdict::Deny {
                code: DENIED_PROFILE,
                reason: "full access requires a local opt-in on the host",
            },
        }
    }
}
