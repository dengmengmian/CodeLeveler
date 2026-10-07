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
    ApproveProject,
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

/// Whether a started call acts on the turn's answer.
///
/// A protocol-owned DTO: the runtime decides this once (in its client
/// projection, via the tool vocabulary) and every surface reads the stamped
/// value instead of re-classifying the tool name. Four renderers each guessing
/// is what made `FinalAnswer` a second truth source.
///
/// The outcome is frozen by execution presentation contract v1 (§I9) and
/// locked by the shared fixture corpus (`testdata/execution_presentation/v1/`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AnswerEffect {
    /// Substantive work: a message before it was interim narration, not the
    /// turn's answer.
    Work,
    /// Answer-preserving bookkeeping or observation: an answer written before
    /// it survives.
    Bookkeeping,
}

impl AnswerEffect {
    /// Whether a message in front of this call was still the turn's answer.
    pub const fn acts_on_answer(self) -> bool {
        matches!(self, Self::Work)
    }

    /// What a peer that started before this fact existed must be read as.
    ///
    /// Unstated is `Work`: the conservative direction, which demotes rather
    /// than promotes. A new runtime always states the field, so this only ever
    /// applies to an older peer.
    pub const fn unstated() -> Self {
        Self::Work
    }
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
