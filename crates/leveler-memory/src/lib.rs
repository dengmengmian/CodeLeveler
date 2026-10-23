//! Durable memory domain types.
//!
//! The store is the Markdown Memory 2.0 implementation in [`v2`]; [`service`]
//! is the single facade every production path goes through. This module keeps
//! the pure, I/O-free parts — candidate extraction, semantic parsing,
//! experience validation and lane policy — so the runtime, the tools and the
//! tests share one definition of what a candidate, a fact or a lesson is.

#![forbid(unsafe_code)]

mod candidates;
mod experience;
mod lanes;
mod semantic;
mod util;
mod validation;

pub mod service;
pub mod v2;

pub use candidates::{
    CandidateKind, CandidateOperation, CandidateSource, MemoryAuthority, MemoryCandidate,
    is_canonical_key, looks_like_secret, parse_direct_memory_command, semantic_key_of,
    title_from_body,
};
pub use experience::{
    EXPERIENCE_KIND, ExperienceCategory, ExperienceEvidence, ExperienceRecord, ExperienceStanding,
    ExperienceVerification, validate_experience_record,
};
pub use lanes::MemoryLanes;
pub use semantic::{
    CandidateDurability, CandidateRejection, CandidateScope, OperationHint, SemanticCandidate,
    SemanticError, SemanticMemoryType, StatementKind, ValidatedCandidates, evidence_matches,
    normalize_subject, parse_semantic_candidates, validate_semantic_candidate,
    validate_semantic_candidates,
};
pub use service::{
    ExperienceView, IdentityContextEntry, MemoryRoots, MemoryService, MemoryView, RememberRequest,
    ReviewRequest, ServiceError, ServiceScope, UpdateRequest, WriteOutcome,
};
pub use validation::ValidationError;

/// What a redacted historical record shows instead of the credential.
pub const REDACTED_TITLE: &str = "[sensitive memory]";
/// What a redacted body shows instead of the credential.
pub const REDACTED_BODY: &str = "[sensitive content removed]";
