//! Structured, machine-readable health findings shared by the CLI doctor and
//! every UI client.
//!
//! A finding states one observed fact about one check. Its `id` is a stable
//! machine key, never a sentence: presentation owns the wording, so the same
//! finding reads correctly in every language and a wording change cannot break
//! a consumer. `subject` distinguishes repeated instances of the same check
//! (a provider, a model, an MCP server) by identity, not by display text.
//!
//! `status` says what the check established. `severity` says how much the
//! finding matters. They are deliberately separate: an unperformed check
//! (`not_checked`) is not a pass, and a passing check can still be
//! informational.

use serde::{Deserialize, Serialize};

/// The area a finding belongs to. The overview groups findings by this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCategory {
    Runtime,
    Model,
    Mcp,
    Permission,
    Sandbox,
    Terminal,
    Configuration,
    Recovery,
}

/// How much a finding matters, independent of what the check established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// What the check actually established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStatus {
    /// The check ran and the condition holds.
    Pass,
    /// The check ran and found a confirmed, non-blocking problem.
    Warn,
    /// The check ran and found a definite, blocking problem.
    Fail,
    /// Authoritative data is insufficient to decide.
    Unknown,
    /// The check was not performed. Never a pass.
    NotChecked,
    /// The check does not apply to this environment.
    NotApplicable,
}

impl DiagnosticStatus {
    /// Whether this status represents a confirmed problem.
    pub fn is_problem(self) -> bool {
        matches!(self, Self::Warn | Self::Fail)
    }
}

/// One structured diagnostic finding.
///
/// `evidence` carries observed facts only, already redacted by the producer:
/// never a secret value, never a stack trace, never an instruction. Consumers
/// render it verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DiagnosticFinding {
    /// Stable machine key. See the `finding_id` constants.
    pub id: String,
    /// The instance this finding is about, when one check repeats per resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub category: DiagnosticCategory,
    pub severity: DiagnosticSeverity,
    pub status: DiagnosticStatus,
    /// Observed facts, redacted. Order is meaningful and stable.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Stable key for an existing user action that may help. Never a command
    /// the client runs on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// When the check ran, from the producer's clock.
    pub checked_at: String,
}

impl DiagnosticFinding {
    /// Identity used for stable selection: one check instance, not its text.
    pub fn key(&self) -> String {
        match &self.subject {
            Some(subject) => format!("{}:{subject}", self.id),
            None => self.id.clone(),
        }
    }
}

/// The answer to one diagnostics request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DiagnosticReport {
    /// When the report was assembled, from the producer's clock.
    pub generated_at: String,
    pub findings: Vec<DiagnosticFinding>,
}

/// Stable finding identifiers.
///
/// A consumer may match on these forever; the user-facing sentence can change
/// without touching them. Add new identifiers here rather than composing them
/// at a call site.
pub mod finding_id {
    // Runtime facts: supplied by the live client that observes them.
    pub const RUNTIME_CONNECTION: &str = "runtime.connection";
    pub const RUNTIME_BUILD: &str = "runtime.build";
    pub const RUNTIME_SESSION: &str = "runtime.session";
    pub const RUNTIME_RECOVERY: &str = "runtime.recovery";

    // Configuration and tooling: supplied by the shared doctor core.
    pub const TOOL_AVAILABILITY: &str = "tool.availability";
    pub const CONFIG_PROVIDERS: &str = "config.providers";
    pub const CONFIG_MODELS: &str = "config.models";
    pub const CONFIG_MODEL_PROVIDER: &str = "config.model-provider";
    pub const CONFIG_GLOBAL: &str = "config.global";
    pub const CONFIG_RELOAD: &str = "config.reload";
    pub const CONFIG_RETIRED_INSTRUCTIONS: &str = "config.retired-instructions";
    pub const MODEL_REASONING: &str = "model.reasoning";
    pub const MODEL_THINKING: &str = "model.thinking";
    pub const PROVIDER_API_KEY: &str = "provider.api-key";
    pub const AGENTS_REGISTRY: &str = "agents.registry";

    // MCP: configuration is known; live connection state is not exposed yet.
    pub const MCP_CONFIG: &str = "mcp.config";
    pub const MCP_CONNECTION: &str = "mcp.connection";

    // Permission, sandbox and terminal.
    pub const PERMISSION_PROFILE: &str = "permission.profile";
    pub const SANDBOX_CAPABILITY: &str = "sandbox.capability";
    pub const SANDBOX_VERIFICATION: &str = "sandbox.verification";
    pub const TERMINAL_CAPABILITY: &str = "terminal.capability";
}

/// Stable action identifiers. Presentation maps these to sentences; a client
/// never executes one by itself.
pub mod action_id {
    pub const CONFIGURE_PROVIDER_MODEL: &str = "action.configure-provider-model";
    pub const SET_PROVIDER_KEY: &str = "action.set-provider-key";
    pub const FIX_CONFIG: &str = "action.fix-config";
    pub const INSTALL_TOOL: &str = "action.install-tool";
    pub const REVIEW_MCP_CONFIG: &str = "action.review-mcp-config";
    /// Open the MCP management surface, where a person can reconnect or
    /// inspect the server. The doctor itself never connects anything.
    pub const OPEN_MCP: &str = "action.open-mcp";
    pub const REVIEW_PERMISSION: &str = "action.review-permission";
    pub const ACKNOWLEDGE_RECOVERY: &str = "action.acknowledge-recovery";
    pub const RESTART_TO_MATCH: &str = "action.restart-to-match";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_roundtrip_with_a_stable_identity() {
        let finding = DiagnosticFinding {
            id: finding_id::PROVIDER_API_KEY.into(),
            subject: Some("deepseek".into()),
            category: DiagnosticCategory::Model,
            severity: DiagnosticSeverity::Error,
            status: DiagnosticStatus::Fail,
            evidence: vec!["environment variable $DEEPSEEK_API_KEY is not set".into()],
            action: Some(action_id::SET_PROVIDER_KEY.into()),
            checked_at: "2026-11-17T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&finding).unwrap();
        let back: DiagnosticFinding = serde_json::from_str(&json).unwrap();
        assert_eq!(back, finding);
        assert_eq!(back.key(), "provider.api-key:deepseek");
        assert_eq!(back.status, DiagnosticStatus::Fail);
    }

    #[test]
    fn status_and_severity_are_independent() {
        let finding = DiagnosticFinding {
            id: finding_id::MCP_CONNECTION.into(),
            subject: None,
            category: DiagnosticCategory::Mcp,
            severity: DiagnosticSeverity::Info,
            status: DiagnosticStatus::NotChecked,
            evidence: Vec::new(),
            action: None,
            checked_at: "t".into(),
        };
        assert!(!finding.status.is_problem());
        assert_eq!(finding.key(), "mcp.connection");
    }

    #[test]
    fn the_wire_tag_is_the_stable_lowercase_name() {
        let json = serde_json::to_string(&DiagnosticStatus::NotChecked).unwrap();
        assert_eq!(json, "\"not_checked\"");
        let json = serde_json::to_string(&DiagnosticCategory::Mcp).unwrap();
        assert_eq!(json, "\"mcp\"");
    }
}
