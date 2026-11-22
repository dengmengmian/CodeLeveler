//! The runtime-facing seam that asks a human to admit one untrusted
//! definition while a tool call is already running.
//!
//! `load_skill` is the only caller today. It cannot write a trust record
//! itself: an approval is a human decision, and the durable transaction lives
//! in the human surface (`/trust`, `leveler trust`, or the client approval
//! prompt). This trait is the one way a tool reaches that surface.
//!
//! The request carries the exact bytes the decision is about. An approver that
//! writes a persistent record MUST re-read the definition and compare
//! `current_digest` first: the person approved the bytes they were shown, not
//! whatever the path holds a moment later.

use std::path::PathBuf;

use async_trait::async_trait;

use leveler_definition_trust::{
    DefinitionIdentity, DefinitionKind, DefinitionScope, DefinitionTrust,
};

/// Longest current/approved preview handed to a client, per side.
///
/// The decision is about exact bytes; a preview is evidence for the human, not
/// an approval input. Truncation is reported so the client never presents a
/// partial document as complete.
pub const MAX_AUTHORIZATION_PREVIEW_BYTES: usize = 256 * 1024;

/// One untrusted definition a running tool wants to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionTrustAuthorizationRequest {
    /// The tool call whose execution is paused for this decision.
    pub call_id: String,
    pub identity: DefinitionIdentity,
    pub kind: DefinitionKind,
    pub scope: DefinitionScope,
    /// Ecosystem label for a skill (`native`, `codex`, `agent-skills`, ...).
    pub source: String,
    /// Absolute package directory (skills) or primary file.
    pub location: PathBuf,
    /// Current trust decision (unapproved, changed, store unavailable, ...).
    pub state: DefinitionTrust,
    /// Digest of the bytes in `current_content`.
    pub current_digest: String,
    /// The exact current content, framed for a package. Bounded by
    /// [`MAX_AUTHORIZATION_PREVIEW_BYTES`].
    pub current_content: String,
    /// The approved snapshot, when one exists and is readable.
    pub approved_content: Option<String>,
    pub content_truncated: bool,
}

/// What the human chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefinitionTrustDecision {
    /// Admit this exact content for this session only.
    AllowOnce,
    /// Persist an approval for this exact content, then use it.
    TrustCurrentVersion,
    /// Do not use it.
    Deny,
}

/// What the runtime actually did with the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionTrustAuthorizationOutcome {
    /// The exact bytes are now admitted for this session.
    AllowedOnce,
    /// The approval transaction recorded the exact bytes.
    Trusted,
    /// The user declined, or had already declined these bytes in this session.
    Denied,
    /// The content changed after the request was shown; nothing was admitted.
    ContentChanged,
    /// There is no human surface here (headless, disconnected), or FullAccess
    /// has no credible baseline, so the definition cannot be admitted.
    Unavailable,
    /// The approval surface answered, but recording the decision failed. The
    /// message is the operator-facing reason; nothing was admitted.
    Failed(String),
}

impl DefinitionTrustAuthorizationOutcome {
    /// Whether the caller may retry the load against the same trust view.
    pub fn admits(&self) -> bool {
        matches!(self, Self::AllowedOnce | Self::Trusted)
    }
}

/// Host-provided human approval surface for definition content.
///
/// Implementations must never authorize from model text. The only inputs are
/// the structured request and a human's structured decision.
#[async_trait]
pub trait DefinitionTrustAuthorizer: Send + Sync {
    async fn authorize(
        &self,
        request: DefinitionTrustAuthorizationRequest,
    ) -> DefinitionTrustAuthorizationOutcome;
}
