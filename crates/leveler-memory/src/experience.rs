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

use crate::looks_like_secret;
use crate::validation::ValidationError;

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

/// What the runtime's evidence actually supports, as presented to a reader.
///
/// Stored verification answers "did the mechanical contract accept this
/// record"; it cannot distinguish a constraint the runtime observed from a
/// failure that later passed. Presentation must not blur that difference:
/// an observation-only lesson is not a verified resolution, and a claim with
/// no mechanical link is model inference, not a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExperienceStanding {
    /// A failing check and a later pass of the same check, both observed.
    VerifiedResolution,
    /// The runtime observed the condition itself, but no resolution.
    Observed,
    /// The explanation has no failure-to-pass link; it is model inference.
    Inferred,
}

impl ExperienceStanding {
    /// The short label rendered next to an experience entry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VerifiedResolution => "verified",
            Self::Observed => "observed",
            Self::Inferred => "inferred",
        }
    }

    /// Evidence strength for replacement. A failure that later passed is
    /// stronger than a bare observation, and both are stronger than a model
    /// explanation with no mechanical link.
    pub fn evidence_rank(self) -> u8 {
        match self {
            Self::VerifiedResolution => 2,
            Self::Observed => 1,
            Self::Inferred => 0,
        }
    }
}

impl ExperienceRecord {
    /// Whether this record claims a mechanically observed resolution.
    pub fn is_verified(&self) -> bool {
        self.verification == ExperienceVerification::Verified
    }

    /// The strongest claim the evidence supports. `Verified` alone is not
    /// enough: the stored flag also covers an observed constraint, which is
    /// not a resolution of a failure.
    pub fn standing(&self) -> ExperienceStanding {
        if !self.is_verified() {
            return ExperienceStanding::Inferred;
        }
        let has = |kind: &str| self.evidence.iter().any(|item| item.kind.trim() == kind);
        if has("failure") && has("resolution") {
            ExperienceStanding::VerifiedResolution
        } else {
            ExperienceStanding::Observed
        }
    }

    /// Whether every evidence reference is an observation.
    ///
    /// Such a record verifies only the observed fact. Its cause and its advice
    /// remain model inference, so a later runtime-observed claim under the same
    /// identity must not silently replace it: the later claim may be a
    /// contradictory reading of the same output.
    pub fn is_observation_only(&self) -> bool {
        !self.evidence.is_empty()
            && self
                .evidence
                .iter()
                .all(|evidence| evidence.kind.trim() == "observation")
    }
}

/// Validate the structural evidence contract. This is deterministic and runs
/// on every write path, including a candidate restored from disk.
pub fn validate_experience_record(record: &ExperienceRecord) -> Result<(), ValidationError> {
    let applies = record.applies_when.trim();
    if applies.is_empty() || applies.chars().count() > MAX_APPLIES_WHEN_CHARS {
        return Err(ValidationError::Invalid(
            "experience applies_when must be 1..=400 characters".to_string(),
        ));
    }
    if looks_like_secret(applies) {
        return Err(ValidationError::Invalid(
            "experience applies_when looks like a secret".to_string(),
        ));
    }
    if let Some(invalid_if) = record.invalid_if.as_deref() {
        let invalid_if = invalid_if.trim();
        if invalid_if.is_empty() || invalid_if.chars().count() > MAX_INVALID_IF_CHARS {
            return Err(ValidationError::Invalid(
                "experience invalid_if must be 1..=400 characters when present".to_string(),
            ));
        }
        if looks_like_secret(invalid_if) {
            return Err(ValidationError::Invalid(
                "experience invalid_if looks like a secret".to_string(),
            ));
        }
    }
    if record.evidence.is_empty() || record.evidence.len() > MAX_EVIDENCE_REFS {
        return Err(ValidationError::Invalid(
            "experience requires 1..=8 evidence references".to_string(),
        ));
    }
    for evidence in &record.evidence {
        let kind = evidence.kind.trim();
        let reference = evidence.reference.trim();
        if kind.is_empty() || kind.chars().count() > MAX_EVIDENCE_KIND_CHARS {
            return Err(ValidationError::Invalid(
                "experience evidence kind is empty or too long".to_string(),
            ));
        }
        if !matches!(kind, "failure" | "resolution" | "mutation" | "observation") {
            return Err(ValidationError::Invalid(format!(
                "experience evidence kind `{kind}` is not a runtime fact"
            )));
        }
        if reference.is_empty() || reference.chars().count() > MAX_EVIDENCE_REF_CHARS {
            return Err(ValidationError::Invalid(
                "experience evidence reference is empty or too long".to_string(),
            ));
        }
        if looks_like_secret(reference) {
            return Err(ValidationError::Invalid(
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
            return Err(ValidationError::Invalid(
                "verified experience needs the runtime facts its category claims".to_string(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        category: ExperienceCategory,
        verification: ExperienceVerification,
        kinds: &[&str],
    ) -> ExperienceRecord {
        ExperienceRecord {
            category,
            verification,
            applies_when: "the same check runs".to_string(),
            invalid_if: None,
            evidence: kinds
                .iter()
                .map(|kind| ExperienceEvidence {
                    kind: (*kind).to_string(),
                    reference: format!("step 1: {kind}"),
                })
                .collect(),
        }
    }

    /// A constraint observed by the runtime is real, but it is not a failed
    /// check that later passed. Presentation must say `observed`.
    #[test]
    fn an_observation_only_record_stands_as_observed() {
        let observed = record(
            ExperienceCategory::EnvironmentConstraint,
            ExperienceVerification::Verified,
            &["observation"],
        );
        assert_eq!(observed.standing(), ExperienceStanding::Observed);
        assert_eq!(observed.standing().as_str(), "observed");
    }

    /// Only a failure AND a later pass support a verified resolution claim.
    #[test]
    fn a_failure_that_later_passed_stands_as_a_verified_resolution() {
        let resolved = record(
            ExperienceCategory::Fix,
            ExperienceVerification::Verified,
            &["failure", "resolution", "mutation"],
        );
        assert_eq!(resolved.standing(), ExperienceStanding::VerifiedResolution);
        assert_eq!(resolved.standing().as_str(), "verified");
    }

    /// A verified constraint with only a failure observed is still not a
    /// resolution.
    #[test]
    fn a_verified_record_without_a_pass_is_not_a_resolution() {
        let failed = record(
            ExperienceCategory::EnvironmentConstraint,
            ExperienceVerification::Verified,
            &["failure"],
        );
        assert_eq!(failed.standing(), ExperienceStanding::Observed);
    }

    /// No mechanical link means the explanation is inference.
    #[test]
    fn an_unverified_claim_stands_as_inferred() {
        let inferred = record(
            ExperienceCategory::RootCause,
            ExperienceVerification::Unverified,
            &["observation"],
        );
        assert_eq!(inferred.standing(), ExperienceStanding::Inferred);
        assert_eq!(inferred.standing().as_str(), "inferred");
    }
}
