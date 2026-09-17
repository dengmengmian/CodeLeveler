//! Per-call execution policy (PR 5).
//!
//! Authorization happens in exactly one place — the ToolHost's admission —
//! and produces one of three things: an immutable
//! [`ResolvedExecutionPolicy`] the call executes under, a decision still to
//! be asked for, or a denial. Nothing downstream reads the live profile or a
//! turn-scoped flag: the tool sees the frozen policy, so a profile switch or a
//! grant made after admission lands on the NEXT call, never this one.

use crate::approval::ApprovalRequest;
use crate::risk::{PermissionProfile, WriteScope};

/// Why this call is allowed to run. Recorded on the admitted call so a reader
/// can tell a profile auto-allow from a human decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationEvidence {
    /// The profile policy allowed it without asking.
    Policy { profile: PermissionProfile },
    /// A standing permission rule matched.
    Rule,
    /// The auto-reviewer allowed it.
    Reviewer,
    /// The user approved this one call.
    ApprovedOnce,
    /// An earlier "approve for the session" decision covered it.
    SessionGrant { signature: String },
    /// The user approved it and asked for a standing rule.
    ApprovedAlways,
}

/// The immutable policy an admitted call executes under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedExecutionPolicy {
    /// The one write boundary. A session grant or a rule never widens it —
    /// they only skip the prompt.
    pub write: WriteScope,
    pub network_allowed: bool,
    pub authorization: AuthorizationEvidence,
}

/// A decision still to be made: the request to put to the reviewer / human,
/// and what the call runs under if they allow it.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub request: ApprovalRequest,
    /// Stable "approve for the session" key for this action.
    pub signature: String,
    pub write: WriteScope,
    pub network_allowed: bool,
    /// The rendered command line, for a durable "always" rule.
    pub command_line: Option<String>,
    /// Paths the call touches, for a durable "always" rule.
    pub scoped_paths: Vec<String>,
}

impl PendingApproval {
    /// The policy this call runs under once `evidence` says it may.
    pub fn allowed(&self, authorization: AuthorizationEvidence) -> ResolvedExecutionPolicy {
        ResolvedExecutionPolicy {
            write: self.write.clone(),
            network_allowed: self.network_allowed,
            authorization,
        }
    }
}

/// A refusal decided by policy, before anyone was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDenial {
    pub reason: String,
}

/// The only three outcomes of resolving a call's policy.
#[derive(Debug)]
pub enum PolicyResolution {
    Allow(ResolvedExecutionPolicy),
    /// Boxed: the pending request carries the full approval prompt.
    Ask(Box<PendingApproval>),
    Deny(PolicyDenial),
}
