//! Pure lifecycle decisions.
//!
//! No file IO, no model text interpretation, and no similarity threshold lives
//! here. Every branch is driven by a structural identity, an authority derived
//! from the caller path, a receipt state, or an explicit command.

use serde::{Deserialize, Serialize};

use super::receipt::Trust;
use super::record::{
    Caller, Cas, Experience, MemoryAuthority, MemoryId, MemoryIntent, MemoryKind, MemoryRecord,
    MemoryScope, Operation,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingReason {
    ProtectedUserFact,
    ExperienceOwned,
    LowerAuthority,
    SameAuthorityWithoutNewEvidence,
    ScopeRequiresUserCommand,
    ExperienceRequiresRuntimeEvidence,
    ExternalTarget,
    UserPinned,
    UnresolvableTarget,
    TargetNotActive,
    PendingTarget,
    ForgetRequiresUserReview,
    /// An automatic writer asked for user authority without proving the body
    /// came from the user's own sentence: the evidence span is missing, or the
    /// text it would store is not a verbatim clause of that span. The candidate
    /// waits for a person instead of speaking as one.
    UnverifiedUserStatement,
    Other(String),
}

impl PendingReason {
    pub fn as_str(&self) -> &str {
        match self {
            Self::ProtectedUserFact => "protected_user_fact",
            Self::ExperienceOwned => "experience_owned",
            Self::LowerAuthority => "lower_authority",
            Self::SameAuthorityWithoutNewEvidence => "same_authority_without_new_evidence",
            Self::ScopeRequiresUserCommand => "scope_requires_user_command",
            Self::ExperienceRequiresRuntimeEvidence => "experience_requires_runtime_evidence",
            Self::ExternalTarget => "external_target",
            Self::UserPinned => "user_pinned",
            Self::UnresolvableTarget => "unresolvable_target",
            Self::TargetNotActive => "target_not_active",
            Self::PendingTarget => "pending_target",
            Self::ForgetRequiresUserReview => "forget_requires_user_review",
            Self::UnverifiedUserStatement => "unverified_user_statement",
            Self::Other(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Identical,
    CoveredByCurrent,
    RejectedPreviously,
    DirectCommandOwned,
    Other(String),
}

impl SkipReason {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Identical => "identical",
            Self::CoveredByCurrent => "covered",
            Self::RejectedPreviously => "rejected_previously",
            Self::DirectCommandOwned => "direct_command_owned",
            Self::Other(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    MissingSubjectKey,
    ScopeMismatch,
    ExternalCaller,
    InvalidIntent,
    PendingTarget,
    ForgottenTarget,
    Other(String),
}

impl RejectReason {
    pub fn as_str(&self) -> &str {
        match self {
            Self::MissingSubjectKey => "missing_subject_key",
            Self::ScopeMismatch => "scope_mismatch",
            Self::ExternalCaller => "external_caller",
            Self::InvalidIntent => "invalid_intent",
            Self::PendingTarget => "pending_target",
            Self::ForgottenTarget => "forgotten_target",
            Self::Other(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    Applied {
        id: MemoryId,
        revision: u64,
        operation: Operation,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        possible_duplicate: Vec<MemoryId>,
    },
    Replayed {
        id: MemoryId,
        revision: u64,
        operation: Operation,
    },
    Pending {
        id: MemoryId,
        reason: PendingReason,
    },
    Skipped {
        existing: MemoryId,
        reason: SkipReason,
    },
    Conflict {
        target: MemoryId,
        current: Cas,
        expected: Cas,
    },
    Rejected {
        reason: RejectReason,
    },
}

impl Admission {
    pub fn applied_id(&self) -> Option<(&MemoryId, u64, Operation)> {
        match self {
            Self::Applied {
                id,
                revision,
                operation,
                ..
            }
            | Self::Replayed {
                id,
                revision,
                operation,
            } => Some((id, *revision, *operation)),
            _ => None,
        }
    }

    pub fn is_applied(&self) -> bool {
        matches!(self, Self::Applied { .. } | Self::Replayed { .. })
    }
}

/// What the reader actually observed for a current record.
#[derive(Debug, Clone)]
pub struct TargetView<'a> {
    pub record: &'a MemoryRecord,
    pub digest: String,
    pub trust: Trust,
    pub effective_authority: MemoryAuthority,
    pub protected: bool,
    pub expired: bool,
}

pub enum TargetDecision {
    Replace,
    Pending(PendingReason),
}

pub enum CreationDecision {
    Active(MemoryAuthority),
    Pending(PendingReason),
    Rejected(RejectReason),
}

/// Whether an intent proves that its body is the user's own statement.
///
/// `user_stated` is not a claim a writer may make about itself: it says the
/// stored words are the user's. The proof is structural — a trusted user-turn
/// evidence span exists, and the body is a verbatim clause of that span — so a
/// model summary, an omitted qualifier or an inverted negation cannot borrow
/// the user's authority, however confident the writer is.
pub fn user_statement_is_proven(intent: &MemoryIntent) -> bool {
    intent.evidence.iter().any(|source| {
        source.kind == crate::v2::record::SourceRefKind::UserTurn
            && source
                .evidence_span
                .as_deref()
                .is_some_and(|span| crate::semantic::is_verbatim_excerpt(&intent.body, span))
    })
}

/// Derive the authority of a newly created record from the caller path. A
/// caller-supplied `proposed_authority` is only consulted for migration, where
/// it is capped below an explicit user decision.
///
/// `proven_user_statement` is [`user_statement_is_proven`] for the intent being
/// admitted: the runtime may record what the user said, and only what the user
/// said.
pub fn creation_decision(
    caller: &Caller,
    kind: MemoryKind,
    scope: MemoryScope,
    experience: Option<&Experience>,
    proven_user_statement: bool,
) -> CreationDecision {
    match caller {
        Caller::UserCommand { .. } => CreationDecision::Active(MemoryAuthority::UserExplicit),
        Caller::UserReview { .. } => CreationDecision::Active(MemoryAuthority::UserApproved),
        Caller::Runtime { .. } => match kind {
            MemoryKind::Experience => {
                let resolved = experience.is_some_and(Experience::has_resolution);
                CreationDecision::Active(if resolved {
                    MemoryAuthority::RuntimeObserved
                } else {
                    MemoryAuthority::ToolObserved
                })
            }
            MemoryKind::UserFact if scope == MemoryScope::Project && proven_user_statement => {
                CreationDecision::Active(MemoryAuthority::UserStated)
            }
            // The user's authority needs the user's words. Without them this is
            // a proposal for a person to read, not current truth.
            MemoryKind::UserFact if scope == MemoryScope::Project => {
                CreationDecision::Pending(PendingReason::UnverifiedUserStatement)
            }
            MemoryKind::UserFact => {
                CreationDecision::Pending(PendingReason::ScopeRequiresUserCommand)
            }
        },
        Caller::Model { .. } => match kind {
            MemoryKind::Experience => {
                CreationDecision::Pending(PendingReason::ExperienceRequiresRuntimeEvidence)
            }
            MemoryKind::UserFact if scope == MemoryScope::Project => {
                CreationDecision::Active(MemoryAuthority::ModelInferred)
            }
            MemoryKind::UserFact => {
                CreationDecision::Pending(PendingReason::ScopeRequiresUserCommand)
            }
        },
        Caller::Migration { .. } => {
            // Migration is a trusted operator path, so it may write; but an
            // imported sentence is not evidence of what the user said, so it
            // cannot mint user authority either. Only a proven user clause can.
            CreationDecision::Active(if proven_user_statement {
                MemoryAuthority::UserStated
            } else {
                MemoryAuthority::ModelInferred
            })
        }
        Caller::External => CreationDecision::Rejected(RejectReason::ExternalCaller),
    }
}

/// Decide how a new value interacts with one existing active record.
pub fn decide_target(
    caller: &Caller,
    target: &TargetView<'_>,
    new_authority: MemoryAuthority,
    has_new_evidence: bool,
) -> TargetDecision {
    if caller.is_user() {
        return TargetDecision::Replace;
    }
    if caller.is_external() {
        return TargetDecision::Pending(PendingReason::ExternalTarget);
    }
    if target.expired {
        return TargetDecision::Replace;
    }
    if target.trust != Trust::Trusted {
        return TargetDecision::Pending(PendingReason::ExternalTarget);
    }
    if target.record.frontmatter.kind == MemoryKind::Experience {
        return TargetDecision::Pending(PendingReason::ExperienceOwned);
    }
    if target.protected {
        return TargetDecision::Pending(PendingReason::ProtectedUserFact);
    }
    if new_authority > target.effective_authority {
        return TargetDecision::Replace;
    }
    if new_authority == target.effective_authority {
        return if has_new_evidence {
            TargetDecision::Replace
        } else {
            TargetDecision::Pending(PendingReason::SameAuthorityWithoutNewEvidence)
        };
    }
    TargetDecision::Pending(PendingReason::LowerAuthority)
}

/// Decide whether a destructive lifecycle action may run directly.
pub fn destructive_decision(
    caller: &Caller,
    target: &TargetView<'_>,
    new_authority: MemoryAuthority,
) -> TargetDecision {
    if caller.is_user() {
        return TargetDecision::Replace;
    }
    if caller.is_external() {
        return TargetDecision::Pending(PendingReason::ExternalTarget);
    }
    if target.trust != Trust::Trusted {
        return TargetDecision::Pending(PendingReason::ExternalTarget);
    }
    if target.record.frontmatter.kind == MemoryKind::Experience {
        return TargetDecision::Pending(PendingReason::ExperienceOwned);
    }
    if target.protected {
        return TargetDecision::Pending(PendingReason::ProtectedUserFact);
    }
    if target.effective_authority > new_authority {
        return TargetDecision::Pending(PendingReason::LowerAuthority);
    }
    TargetDecision::Replace
}

pub fn pin_decision(caller: &Caller, target: &TargetView<'_>, unpin: bool) -> TargetDecision {
    if caller.is_user() {
        return TargetDecision::Replace;
    }
    if caller.is_external() {
        return TargetDecision::Pending(PendingReason::ExternalTarget);
    }
    if target.trust != Trust::Trusted {
        return TargetDecision::Pending(PendingReason::ExternalTarget);
    }
    if unpin && target.record.frontmatter.pinned_by == Some(super::record::PinOwner::User) {
        return TargetDecision::Pending(PendingReason::UserPinned);
    }
    // §7.3: an automatic pin of a user-owned fact waits for a person. This is
    // wider than proposal protection: an extracted `user_stated` fact may be
    // corrected by the extractor's own new evidence, but it is still the
    // user's memory and must not be pinned automatically.
    if !unpin && target.effective_authority >= MemoryAuthority::UserStated {
        return TargetDecision::Pending(PendingReason::ProtectedUserFact);
    }
    if target.record.frontmatter.kind == MemoryKind::Experience
        && target.record.frontmatter.pinned_by == Some(super::record::PinOwner::User)
    {
        return TargetDecision::Pending(PendingReason::UserPinned);
    }
    TargetDecision::Replace
}

/// Exact structural identity of the proposed fact against a current record.
/// This compares only supplied fields; it never derives an identity from text.
pub fn is_identical(existing: &MemoryRecord, intent: &MemoryIntent) -> bool {
    existing.body == intent.body
        && existing.frontmatter.value == intent.value
        && existing.frontmatter.subject_key == intent.subject_key
}

/// True when the intent carries at least one evidence reference that the
/// existing record did not already carry.
///
/// Channel provenance (`channel: true`) records who performed the write, not
/// what supports the fact; it must never satisfy the "new evidence" rule by
/// itself.
pub fn has_new_evidence(existing: &MemoryRecord, intent: &MemoryIntent) -> bool {
    if intent.evidence.iter().all(is_channel_source) {
        return false;
    }
    let existing: Vec<String> = existing
        .frontmatter
        .source_refs
        .iter()
        .filter(|source| !is_channel_source(source))
        .filter_map(|source| serde_json::to_string(source).ok())
        .collect();
    intent
        .evidence
        .iter()
        .filter(|source| !is_channel_source(source))
        .any(|source| {
            serde_json::to_string(source)
                .ok()
                .is_some_and(|value| !existing.iter().any(|old| old == &value))
        })
}

fn is_channel_source(source: &super::record::SourceRef) -> bool {
    source
        .extra
        .iter()
        .any(|(key, value)| key.as_str() == Some("channel") && value.as_bool() == Some(true))
}

/// A current record must not be replaced through an automatic path when the
/// stored authority came from a user decision.
///
/// `user_stated` is an extracted statement with evidence, not a decision: a
/// later statement under the same identity with a new span may supersede it.
pub fn is_protected(authority: MemoryAuthority) -> bool {
    authority.is_user_decision()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::record::MemoryStatus;
    use chrono::TimeZone;

    fn target_with(authority: MemoryAuthority) -> crate::v2::record::MemoryRecord {
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 23, 0, 0, 0).unwrap();
        crate::v2::record::MemoryRecord {
            frontmatter: crate::v2::record::Frontmatter {
                schema: 2,
                id: MemoryId::parse("mem_00000000000000000000000000000002").unwrap(),
                kind: MemoryKind::UserFact,
                scope: MemoryScope::Project,
                status: MemoryStatus::Active,
                revision: 1,
                authority,
                title: "Billing".into(),
                subject: None,
                subject_key: Some("billing_model".into()),
                value: Some("subscription".into()),
                statement: Default::default(),
                tags: vec![],
                pinned: false,
                pinned_by: None,
                sensitive: false,
                expires_at: None,
                supersedes: vec![],
                superseded_by: None,
                proposal: None,
                experience: None,
                source_refs: vec![],
                created_at: now,
                updated_at: now,
                extra: Default::default(),
            },
            body: "subscription".into(),
        }
    }

    #[test]
    fn only_user_decisions_are_protected() {
        assert!(is_protected(MemoryAuthority::UserExplicit));
        assert!(is_protected(MemoryAuthority::UserApproved));
        assert!(!is_protected(MemoryAuthority::UserStated));
        assert!(!is_protected(MemoryAuthority::ProjectSource));
        assert!(!is_protected(MemoryAuthority::RuntimeObserved));
        assert!(!is_protected(MemoryAuthority::ModelInferred));
    }

    #[test]
    fn a_model_cannot_replace_a_user_decision() {
        let record = target_with(MemoryAuthority::UserExplicit);
        let target = TargetView {
            record: &record,
            digest: "digest".into(),
            trust: Trust::Trusted,
            effective_authority: MemoryAuthority::UserExplicit,
            protected: true,
            expired: false,
        };
        assert!(matches!(
            decide_target(
                &Caller::Model {
                    tool_call: "call-1".into()
                },
                &target,
                MemoryAuthority::ModelInferred,
                true
            ),
            TargetDecision::Pending(PendingReason::ProtectedUserFact)
        ));
    }
}
