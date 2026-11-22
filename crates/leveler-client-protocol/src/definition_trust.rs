//! Definition-trust authorization for a running tool call.
//!
//! This is deliberately separate from [`super::UiApprovalRequest`]: a
//! permission approval answers "may this action run", while this answers "may
//! this exact instruction content enter the model's control context". They use
//! different decisions, different durable rules and different audit states.
//!
//! The wire carries display strings for provenance, but the authoritative
//! fields are the identity, the exact digest and the two content previews. A
//! client must reply with a decision, never with content or a new digest.

use serde::{Deserialize, Serialize};

use leveler_core::{DefinitionTrustRequestId, SessionId};

/// Where one definition stands with the persistent store, as a client shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiDefinitionTrustState {
    /// No approval record for these bytes.
    Unapproved,
    /// A record exists, but it covers different bytes.
    Changed,
    /// The store itself is unusable; nothing can be admitted.
    StoreUnavailable,
    /// The refusal's state is one this client does not need to distinguish,
    /// such as a built-in that should never be asked about.
    Other,
}

/// One untrusted definition a running tool wants to use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiDefinitionTrustRequest {
    pub id: DefinitionTrustRequestId,
    pub session_id: SessionId,
    /// The tool call whose execution is paused for this decision.
    pub call_id: String,
    /// Stable store key, e.g. `skill:.agents/skills/design`.
    pub identity: String,
    /// Human-facing label, e.g. `.agents/skills/design`.
    pub display: String,
    /// `skill`, `root_agents`, `agent`, ...
    pub kind: String,
    /// `project` or `user`.
    pub scope: String,
    /// Ecosystem label (`CodeLeveler`, `Codex compatible`, ...).
    pub source: String,
    /// Absolute package directory or primary file.
    pub location: String,
    pub state: UiDefinitionTrustState,
    /// The digest the persistent record covers, when `state` is `changed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_digest: Option<String>,
    /// Digest of `current_content`.
    pub current_digest: String,
    /// Exact current content, framed for a package. Bounded by the runtime.
    pub current_content: String,
    /// The approved snapshot, when one exists and is readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_content: Option<String>,
    /// Whether either preview omitted content.
    #[serde(default)]
    pub content_truncated: bool,
}

/// The human's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DefinitionTrustDecision {
    /// Admit this exact content for the current session only.
    AllowOnce,
    /// Persist a human approval for this exact content, then use it.
    TrustCurrentVersion,
    /// Do not use it.
    Deny,
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_core::SessionId;

    #[test]
    fn a_request_roundtrips_and_carries_both_previews() {
        let request = UiDefinitionTrustRequest {
            id: DefinitionTrustRequestId::new("d1"),
            session_id: SessionId::new("s1"),
            call_id: "call-1".into(),
            identity: "skill:.agents/skills/design".into(),
            display: ".agents/skills/design".into(),
            kind: "skill".into(),
            scope: "project".into(),
            source: "Agent Skills".into(),
            location: "/repo/.agents/skills/design".into(),
            state: UiDefinitionTrustState::Changed,
            approved_digest: Some("sha256:old".into()),
            current_digest: "sha256:new".into(),
            current_content: "--- SKILL.md ---\nnew\n".into(),
            approved_content: Some("--- SKILL.md ---\nold\n".into()),
            content_truncated: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        let back: UiDefinitionTrustRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, request);
        // Decisions are named on the wire, never inferred from display text.
        assert_eq!(
            serde_json::to_string(&DefinitionTrustDecision::TrustCurrentVersion).unwrap(),
            "\"trust_current_version\""
        );
    }

    #[test]
    fn a_legacy_absent_approved_content_and_optional_state_parse() {
        let json = r#"{
            "id":"d1","session_id":"s1","call_id":"c","identity":"i","display":"d",
            "kind":"skill","scope":"project","source":"s","location":"/x",
            "state":"unapproved","current_digest":"sha256:x","current_content":"c"
        }"#;
        let request: UiDefinitionTrustRequest = serde_json::from_str(json).unwrap();
        assert!(request.approved_content.is_none());
        assert!(!request.content_truncated);
        assert!(request.approved_digest.is_none());
    }
}
