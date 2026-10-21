//! Agent experience: evidence-bound knowledge learned from execution.
//!
//! Experience is a memory KIND, not a second store. It uses the same identity,
//! authority, conflict, retention and restore machinery as every other durable
//! memory. What makes it different is the evidence contract.
//!
//! A `Verified` experience may only claim what the runtime itself observed: a
//! command that failed in the task and later succeeded under the same identity.
//! That proves a behavior changed. It does NOT prove the model's explanation of
//! why, so recall renders the explanation as model inference. A candidate that
//! cannot meet the contract stays `Unverified` and can only become current
//! after a person accepts it.
//!
//! Nothing here reads a tool log. Evidence references are short identifiers
//! into structured runtime facts; raw tool output is untrusted data and never
//! becomes a memory body.

use serde::{Deserialize, Serialize};

use crate::{MemoryError, looks_like_secret};

/// The stored `kind` label. Kept as a string on disk so old readers keep
/// loading; [`crate::MemoryKind::Experience`] is the parse boundary.
pub const EXPERIENCE_KIND: &str = "experience";
pub const MAX_APPLIES_WHEN_CHARS: usize = 400;
pub const MAX_INVALID_IF_CHARS: usize = 400;
pub const MAX_EVIDENCE_REFS: usize = 8;
pub const MAX_EVIDENCE_REF_CHARS: usize = 400;
pub const MAX_EVIDENCE_KIND_CHARS: usize = 40;

/// What kind of experience this is. The category decides how the entry is
/// rendered and which evidence shape can verify it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceCategory {
    /// A non-obvious reason a failure happens.
    RootCause,
    /// A change that actually made a failing check pass.
    Fix,
    /// A failure that recurs across attempts in the same execution.
    FailurePattern,
    /// A constraint of the environment, toolchain or workspace.
    EnvironmentConstraint,
    /// A tool invocation pattern that failed and then worked.
    ToolUse,
}

impl ExperienceCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RootCause => "root_cause",
            Self::Fix => "fix",
            Self::FailurePattern => "failure_pattern",
            Self::EnvironmentConstraint => "environment_constraint",
            Self::ToolUse => "tool_use",
        }
    }
}

impl std::str::FromStr for ExperienceCategory {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value.trim() {
            "root_cause" => Self::RootCause,
            "fix" => Self::Fix,
            "failure_pattern" => Self::FailurePattern,
            "environment_constraint" => Self::EnvironmentConstraint,
            "tool_use" => Self::ToolUse,
            _ => return Err(()),
        })
    }
}

/// How strongly the entry's behavioral claim is backed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceVerification {
    /// The failure this entry describes was observed, and the same check later
    /// passed after the change. The explanation remains model inference.
    Verified,
    /// Proposed but not mechanically observed. Never current without a person.
    #[default]
    Unverified,
}

/// One short reference into the structured facts of the task that produced the
/// experience. Never a raw tool log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperienceEvidence {
    /// `failure` | `resolution` | `mutation` | `observation`
    pub kind: String,
    /// Bounded identifier (step/call id plus the command it ran).
    pub reference: String,
}

/// The structured half of an experience memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperienceRecord {
    pub category: ExperienceCategory,
    pub verification: ExperienceVerification,
    /// Conditions that must hold for the entry to apply.
    pub applies_when: String,
    /// Conditions under which the entry stops applying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_if: Option<String>,
    /// The runtime facts the entry was drawn from.
    #[serde(default)]
    pub evidence: Vec<ExperienceEvidence>,
}

impl ExperienceRecord {
    /// Whether this record claims a mechanically observed resolution.
    pub fn is_verified(&self) -> bool {
        self.verification == ExperienceVerification::Verified
    }
}

/// Validate the structural evidence contract. This is deterministic and runs
/// on every write path, including a candidate restored from disk.
pub fn validate_experience_record(record: &ExperienceRecord) -> Result<(), MemoryError> {
    let applies = record.applies_when.trim();
    if applies.is_empty() || applies.chars().count() > MAX_APPLIES_WHEN_CHARS {
        return Err(MemoryError::Invalid(
            "experience applies_when must be 1..=400 characters".to_string(),
        ));
    }
    if looks_like_secret(applies) {
        return Err(MemoryError::Invalid(
            "experience applies_when looks like a secret".to_string(),
        ));
    }
    if let Some(invalid_if) = record.invalid_if.as_deref() {
        let invalid_if = invalid_if.trim();
        if invalid_if.is_empty() || invalid_if.chars().count() > MAX_INVALID_IF_CHARS {
            return Err(MemoryError::Invalid(
                "experience invalid_if must be 1..=400 characters when present".to_string(),
            ));
        }
        if looks_like_secret(invalid_if) {
            return Err(MemoryError::Invalid(
                "experience invalid_if looks like a secret".to_string(),
            ));
        }
    }
    if record.evidence.is_empty() || record.evidence.len() > MAX_EVIDENCE_REFS {
        return Err(MemoryError::Invalid(
            "experience requires 1..=8 evidence references".to_string(),
        ));
    }
    for evidence in &record.evidence {
        let kind = evidence.kind.trim();
        let reference = evidence.reference.trim();
        if kind.is_empty() || kind.chars().count() > MAX_EVIDENCE_KIND_CHARS {
            return Err(MemoryError::Invalid(
                "experience evidence kind is empty or too long".to_string(),
            ));
        }
        if !matches!(kind, "failure" | "resolution" | "mutation" | "observation") {
            return Err(MemoryError::Invalid(format!(
                "experience evidence kind `{kind}` is not a runtime fact"
            )));
        }
        if reference.is_empty() || reference.chars().count() > MAX_EVIDENCE_REF_CHARS {
            return Err(MemoryError::Invalid(
                "experience evidence reference is empty or too long".to_string(),
            ));
        }
        if looks_like_secret(reference) {
            return Err(MemoryError::Invalid(
                "experience evidence reference looks like a secret".to_string(),
            ));
        }
    }
    if record.verification == ExperienceVerification::Verified {
        let has = |kind: &str| record.evidence.iter().any(|item| item.kind.trim() == kind);
        let shaped = match record.category {
            // A constraint is observed, not resolved: the failure itself is
            // the fact, and it needs no "fix" to be true.
            ExperienceCategory::EnvironmentConstraint => {
                has("failure") || has("resolution") || has("observation")
            }
            _ => has("failure") && has("resolution"),
        };
        if !shaped {
            return Err(MemoryError::Invalid(
                "verified experience needs the runtime facts its category claims".to_string(),
            ));
        }
    }
    Ok(())
}
