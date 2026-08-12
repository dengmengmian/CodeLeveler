//! Protocol-owned permission DTOs.
//!
//! These deliberately mirror the runtime domain values on the wire, but live
//! here so protocol compatibility is not coupled to execution internals.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    ApproveOnce,
    ApproveSession,
    ApproveAlways,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PermissionProfile {
    RequestApproval,
    #[default]
    Assisted,
    FullAccess,
}

/// Whether a session prompts for approval of risky actions or auto-approves
/// them. Unlike [`PermissionProfile`] (which actions are *allowed*), this is
/// about whether the approval overlay is shown. It is a per-session property so
/// one daemon can host an attended interactive session and an unattended
/// auto-approving goal at the same time without crossing their approval flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Round-trip every risky action to the client for approval.
    #[default]
    Interactive,
    /// Skip the approval overlay (unattended). Only honoured from the trusted
    /// local transport, never elevated by a remote client.
    AutoApprove,
}

impl PermissionProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequestApproval => "request_approval",
            Self::Assisted => "assisted",
            Self::FullAccess => "full_access",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "request_approval" | "request-approval" | "plan" => Some(Self::RequestApproval),
            "assisted" | "workspace_write" => Some(Self::Assisted),
            "full_access" | "full-access" => Some(Self::FullAccess),
            _ => None,
        }
    }
}

impl std::fmt::Display for PermissionProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
