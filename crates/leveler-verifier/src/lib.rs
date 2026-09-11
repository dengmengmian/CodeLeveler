//! `leveler-verifier` — verification gates and evidence (spec §29-32).
//!
//! The verifier is the authority for verification verdicts (spec §2.3, §30): it
//! runs the format/build/test plan, captures evidence, checks scope, classifies
//! failures, and reports whether the completion gate is satisfied. It does not
//! own completion: a passing verdict is mechanical evidence a task outcome is
//! judged against, not the runtime deciding the request was satisfied.
#![forbid(unsafe_code)]

pub mod discover;
pub mod failure;
pub mod plan;
pub mod report;
pub mod test_results;
mod toolchain;
mod verifier;

pub use failure::{ClassifiedFailure, FailureKind, RecoveryStrategy, classify};
pub use plan::{CheckKind, VerificationCommand, VerificationPlan};
pub use report::{CheckOutcome, CheckStatus, Verdict, VerificationReport};
pub use verifier::Verifier;
