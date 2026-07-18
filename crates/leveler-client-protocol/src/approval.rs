//! Approval request projection for the UI.
//!
//! The runtime's `leveler_execution::ApprovalRequest` carries paths and a risk
//! level; this is the render-ready view the approval overlay shows .
//! The decision type ([`ApprovalDecision`]) and id ([`ApprovalId`]) are reused
//! from the runtime unchanged.

use serde::{Deserialize, Serialize};

use leveler_core::{ApprovalId, ClarificationId};

/// A pending permission request, projected for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiApprovalRequest {
    pub id: ApprovalId,
    /// The tool requesting permission (e.g. `run_command`).
    pub tool: String,
    /// A one-line summary of what will happen.
    pub summary: String,
    /// The concrete command, when the tool is `run_command`.
    pub command: Option<String>,
    /// Human-readable risk bullets (paths touched, network, etc.).
    pub risks: Vec<String>,
}

/// A mid-task clarification the agent needs answered (spec §35).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiClarificationRequest {
    pub id: ClarificationId,
    pub question: String,
    /// Candidate answers, when the model offered a choice.
    pub options: Vec<String>,
}

/// A live control request included in a reconnect snapshot. Only requests with
/// an in-process waiter are projected; interrupted turns never resurrect stale
/// buttons after a process restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "request", rename_all = "snake_case")]
pub enum UiPendingInteraction {
    Approval(UiApprovalRequest),
    Clarification(UiClarificationRequest),
}
