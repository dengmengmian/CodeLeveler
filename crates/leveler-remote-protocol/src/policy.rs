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

/// Default seconds before a remote-only approval auto-denies.
/// Overridable per host via `remote.approval_timeout_secs`.
pub const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 120;

/// Who can still answer one pending approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waiters {
    /// Connected local interactive UIs subscribed to this session — a TUI or a
    /// loopback web client. Counted from the local transport's client kind.
    pub local: usize,
    /// Open interactive remote streams for this runtime.
    pub remote: usize,
}

/// What the caller should do with its timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerTransition {
    /// Start counting from now.
    Arm,
    /// Cancel any running countdown.
    Disarm,
    /// Whatever is running keeps its existing deadline.
    Unchanged,
}

/// Tracks whether a remote-only auto-deny countdown should be running.
///
/// The timeout exists so a remote-only approval cannot block a turn forever.
/// It deliberately does **not** run while a local interactive UI is attached:
/// aiming it at someone reading the prompt in their terminal would let a paired
/// phone expire a decision the person at the keyboard was still making.
///
/// Emitting [`TimerTransition::Arm`] only on the flip is what makes the
/// countdown start when the last local waiter leaves, rather than when the
/// approval was first raised. An approval that waited an hour with a person
/// watching gets the full window once they close their terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApprovalTimeoutState {
    armed: bool,
    resolved: bool,
}

impl ApprovalTimeoutState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Re-evaluate after the waiter population changed.
    pub fn observe(&mut self, waiters: Waiters) -> TimerTransition {
        // A resolved approval has nothing left to expire.
        let should_arm = !self.resolved && waiters.remote > 0 && waiters.local == 0;
        self.transition_to(should_arm)
    }

    /// The approval was answered — by a human on either side, or by the
    /// timeout itself.
    pub fn resolved(&mut self) -> TimerTransition {
        self.resolved = true;
        self.transition_to(false)
    }

    fn transition_to(&mut self, armed: bool) -> TimerTransition {
        if armed == self.armed {
            return TimerTransition::Unchanged;
        }
        self.armed = armed;
        if armed {
            TimerTransition::Arm
        } else {
            TimerTransition::Disarm
        }
    }
}

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
            | ClientCommand::SteerCurrentTurn { .. }
            | ClientCommand::RunGoal { .. }
            | ClientCommand::CancelCurrentTurn { .. }
            | ClientCommand::ForceCancelCurrentTurn { .. }
            | ClientCommand::AnswerClarification { .. }
            | ClientCommand::SelectModel { .. }
            | ClientCommand::SetProductAxes { .. }
            | ClientCommand::ConfirmPlanToGoal { .. }
            | ClientCommand::RequestDiff { .. }
            | ClientCommand::CompactContext { .. }
            | ClientCommand::ClearConversation { .. }
            | ClientCommand::RequestSessionList
            | ClientCommand::RequestSessionListFor { .. }
            // Starting a fresh session creates; it deletes nothing and reaches
            // nothing local, so it is strictly safer than the in-place clear
            // already allowed above.
            | ClientCommand::NewSessionFor { .. }
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
            // `AcceptMemory` especially: it IS the consent K36 requires, and a
            // phone is the wrong place to give it.
            ClientCommand::ListMemory { .. }
            | ClientCommand::ForgetMemory { .. }
            | ClientCommand::AcceptMemory { .. } => RemoteVerdict::Deny {
                code: DENIED_COMMAND,
                reason: "memory is not reachable remotely",
            },

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

            // Arbitrary host shell execution from a phone is exactly the
            // capability this policy exists to withhold. The user-shell
            // product surface is the local TUI; remote gets nothing, not
            // even cancel (it could kill work it cannot see).
            ClientCommand::RunUserShell { .. } | ClientCommand::CancelUserShell { .. } => {
                RemoteVerdict::Deny {
                    code: DENIED_COMMAND,
                    reason: "user shell execution is not available remotely",
                }
            }

            // Local-only observatory: path/command summaries must not leave
            // the machine. PublicEvent remains the remote projection.
            ClientCommand::QueryObservability { .. } => RemoteVerdict::Deny {
                code: DENIED_COMMAND,
                reason: "runtime observatory is local-only",
            },

            // Same class of data as the observatory: a finding carries the
            // file path it was found in and a summary of the code. The counts
            // a remote client needs already ride on SubAgentUpdated; the
            // per-finding detail stays on the machine.
            ClientCommand::QueryChildContribution { .. } => RemoteVerdict::Deny {
                code: DENIED_COMMAND,
                reason: "contribution detail is local-only",
            },

            // An unfinished goal names what the user asked, verbatim, and the
            // session it ran in. Same class as the observatory: the objective
            // is repository content in prose form.
            ClientCommand::ListUnfinishedGoals { .. } => RemoteVerdict::Deny {
                code: DENIED_COMMAND,
                reason: "goal listing is local-only",
            },

            // A recap projects plan wording, findings, and workspace paths —
            // repository content in prose form, same class as the goal list.
            ClientCommand::Recap { .. } => RemoteVerdict::Deny {
                code: DENIED_COMMAND,
                reason: "goal recap is local-only",
            },

            // Retiring the runtime is local lifecycle authority. A phone on
            // another network running a different build has no business
            // ending the process that owns this machine's work — build drift
            // between a remote client and the runtime is a compatibility
            // question, not a licence to restart someone else's daemon.
            ClientCommand::ShutdownWhenIdle { .. } => RemoteVerdict::Deny {
                code: DENIED_COMMAND,
                reason: "a remote client cannot retire the local runtime",
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
