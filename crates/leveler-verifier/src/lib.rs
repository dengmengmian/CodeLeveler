//! `leveler-verifier` — verification gates and completion enforcement (spec §29-32).
//!
//! Only the verifier can mark a task complete (spec §2.3, §30): it runs the
//! format/build/test plan, captures evidence, checks scope, classifies failures,
//! and reports whether the completion gate is satisfied.
#![forbid(unsafe_code)]

pub mod acceptance;
pub mod discover;
pub mod failure;
pub mod outcome;
pub mod plan;
pub mod report;
pub mod test_results;
mod verifier;

pub use acceptance::{
    AcceptanceCheck, AcceptanceEvidence, AcceptanceLedger, AcceptanceStatus,
    MAX_MUTATION_DERIVED_CHECKS, assemble_acceptance_checks, has_executable_required,
    sanitize_workspace_rel_path, synthesize_mutation_acceptance, workspace_join_rel,
};
pub use failure::{ClassifiedFailure, FailureKind, RecoveryStrategy, classify};
pub use outcome::{CompletionVerdict, ExpectedEvidence, finalize_task_outcome};
pub use plan::{CheckKind, VerificationCommand, VerificationPlan};
pub use report::{CheckOutcome, CheckStatus, Verdict, VerificationReport};
pub use verifier::Verifier;
