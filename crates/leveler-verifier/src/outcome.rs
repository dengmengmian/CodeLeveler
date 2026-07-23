//! Final task completion verdict from repository health + expected mutation.
//!
//! ```text
//! Verified =
//!   RepositoryHealthEvidence  // VerificationReport::verdict == Verified
//!   ∧ ExpectedMutation        // if needs_mutation then has_mutation
//! ```
//!
//! **Only facts decide, and there is exactly one fact: the project's own
//! gating checks, run against the edited tree.**
//!
//! Acceptance criteria are deliberately *not* an input here. They are the
//! model's restatement of the goal plus a check command it invented on the
//! spot — a guess, whichever way that command exits. Two measured runs settled
//! it:
//!
//! - A correct, fully green turn reported "有改动但缺少系统级验收背书" only
//!   because the model never produced a provable criterion.
//! - A correct `isRequired()` added to an ESM module passed the project's
//!   suite (1377 pass / 0 fail) but was marked "验收未通过：AC2" because the
//!   model's own one-liner used CommonJS `require()` on an ESM file and threw.
//!
//! In both the harness overruled a maintained test suite with a throwaway
//! guess. The ledger is still produced and shown to the user as information —
//! it just no longer decides. Passing it here at all would invite the coupling
//! back, so the parameter is gone.
//!
//! `needs_mutation` is decided by the caller and must **never** be derived from
//! `has_mutation`.

use crate::report::{Verdict, VerificationReport};

/// Observed vs required workspace mutation evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedEvidence {
    /// Whether Verified requires a non-empty mutation set **and** proven
    /// required acceptance (implementation-class / delivery / Edit graph).
    /// Never derive this from `has_mutation`.
    pub needs_mutation: bool,
    /// Whether mutation was observed (`!modified_files.is_empty()`, ledger, …).
    pub has_mutation: bool,
}

/// Local completion verdict isomorphic to lifecycle `TaskOutcome` success
/// states that the verifier may assign. Engine maps to `TaskOutcome`.
///
/// Kept in `leveler-verifier` so this crate does not depend on lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionVerdict {
    Verified,
    CompletedUnverified,
    Failed,
}

/// Compose repository health and expected mutation into a terminal verdict.
///
/// - Health `Failed` → `Failed` (hard).
/// - Health `Unverified` → `CompletedUnverified` (nothing could be checked).
/// - Health `Verified` + `needs_mutation` but nothing changed
///   → `CompletedUnverified` (an observed fact, not a guess).
/// - Otherwise → `Verified`.
pub fn finalize_task_outcome(
    health: &VerificationReport,
    expected: ExpectedEvidence,
) -> CompletionVerdict {
    match health.verdict() {
        Verdict::Failed => CompletionVerdict::Failed,
        Verdict::Unverified(_) => CompletionVerdict::CompletedUnverified,
        Verdict::Verified if expected.needs_mutation && !expected.has_mutation => {
            CompletionVerdict::CompletedUnverified
        }
        Verdict::Verified => CompletionVerdict::Verified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::CheckKind;
    use crate::report::{CheckOutcome, CheckStatus};

    fn gate_passed() -> VerificationReport {
        VerificationReport {
            checks: vec![CheckOutcome {
                name: "build".into(),
                kind: CheckKind::Build,
                gating: true,
                status: CheckStatus::Passed,
                evidence: String::new(),
                failure: None,
                failed_tests: std::collections::BTreeSet::new(),
            }],
            scope_ok: true,
            scope_violations: vec![],
            baseline_failures: Vec::new(),
        }
    }

    fn gate_failed() -> VerificationReport {
        VerificationReport {
            checks: vec![CheckOutcome {
                name: "test".into(),
                kind: CheckKind::Test,
                gating: true,
                status: CheckStatus::Failed,
                evidence: String::new(),
                failure: None,
                failed_tests: std::collections::BTreeSet::new(),
            }],
            scope_ok: true,
            scope_violations: vec![],
            baseline_failures: Vec::new(),
        }
    }

    fn gate_unverified() -> VerificationReport {
        VerificationReport {
            checks: vec![CheckOutcome {
                name: "tsc".into(),
                kind: CheckKind::Build,
                gating: true,
                status: CheckStatus::ToolMissing,
                evidence: String::new(),
                failure: None,
                failed_tests: std::collections::BTreeSet::new(),
            }],
            scope_ok: true,
            scope_violations: vec![],
            baseline_failures: Vec::new(),
        }
    }

    fn expected(needs: bool, has: bool) -> ExpectedEvidence {
        ExpectedEvidence {
            needs_mutation: needs,
            has_mutation: has,
        }
    }

    #[test]
    fn health_failed_is_failed_regardless_of_mutation() {
        assert_eq!(
            finalize_task_outcome(&gate_failed(), expected(false, false)),
            CompletionVerdict::Failed
        );
        assert_eq!(
            finalize_task_outcome(&gate_failed(), expected(true, true)),
            CompletionVerdict::Failed
        );
    }

    #[test]
    fn health_unverified_is_completed_unverified() {
        assert_eq!(
            finalize_task_outcome(&gate_unverified(), expected(false, false)),
            CompletionVerdict::CompletedUnverified
        );
    }

    #[test]
    fn verified_when_health_ok_and_no_mutation_required() {
        assert_eq!(
            finalize_task_outcome(&gate_passed(), expected(false, false)),
            CompletionVerdict::Verified
        );
    }

    #[test]
    fn needs_mutation_without_mutation_downgrades() {
        assert_eq!(
            finalize_task_outcome(&gate_passed(), expected(true, false)),
            CompletionVerdict::CompletedUnverified
        );
    }

    #[test]
    fn green_gate_and_mutation_is_verified() {
        assert_eq!(
            finalize_task_outcome(&gate_passed(), expected(true, true)),
            CompletionVerdict::Verified
        );
    }
}
