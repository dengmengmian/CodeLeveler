//! Final task completion verdict from health + acceptance + expected mutation.
//!
//! Pure composition of the closed-loop formula (design §1.3–§1.4 + post
//! hard-screen false-Verified guard):
//!
//! ```text
//! Verified =
//!   RepositoryHealthEvidence  // VerificationReport::verdict == Verified
//!   ∧ TaskAcceptanceEvidence  // no required Unmet/Unverifiable
//!   ∧ ExpectedMutation        // if needs_mutation then has_mutation
//!   ∧ ProvenAcceptance        // if needs_mutation then ≥1 required Met
//! ```
//!
//! `needs_mutation` doubles as the implementation-class signal (same decision
//! surface as ExpectedMutation). It is decided by the caller and must
//! **never** be derived from `has_mutation`.
//!
//! For implementation-class runs, empty required AC / only optional fallback /
//! only Unverifiable required criteria are **not** proven — at most
//! `CompletedUnverified`. Non-implementation runs may still Verified without
//! proven required AC.

use crate::acceptance::AcceptanceLedger;
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

/// Whether acceptance evidence is sufficient for Verified given the task class.
///
/// - Non-impl (`needs_proven_acceptance == false`): empty / `None` / all-Met
///   required set is fine; only Unmet/Unverifiable required items block.
/// - Impl (`needs_proven_acceptance == true`): must have **at least one**
///   required criterion in `Met` status. `None`, empty, only-optional, or
///   only-Unverifiable required → unproven.
fn acceptance_allows_verified(
    acceptance: Option<&AcceptanceLedger>,
    needs_proven_acceptance: bool,
) -> bool {
    match acceptance {
        None => !needs_proven_acceptance,
        Some(ledger) => {
            if !ledger.all_required_met() {
                return false;
            }
            if needs_proven_acceptance {
                return ledger.has_proven_required_met();
            }
            true
        }
    }
}

/// Compose health, acceptance, and expected-mutation into a terminal verdict.
///
/// - Health `Failed` → `Failed` (hard).
/// - Health `Unverified` → `CompletedUnverified`.
/// - Health `Verified` but required acceptance not all Met → `CompletedUnverified`.
/// - Health `Verified` + `needs_mutation` but no proven required Met →
///   `CompletedUnverified` (closes empty-AC false Verified).
/// - Health `Verified` + acceptance ok but `needs_mutation && !has_mutation`
///   → `CompletedUnverified`.
/// - Otherwise → `Verified`.
pub fn finalize_task_outcome(
    health: &VerificationReport,
    acceptance: Option<&AcceptanceLedger>,
    expected: ExpectedEvidence,
) -> CompletionVerdict {
    match health.verdict() {
        Verdict::Failed => CompletionVerdict::Failed,
        Verdict::Unverified(_) => CompletionVerdict::CompletedUnverified,
        Verdict::Verified => {
            if !acceptance_allows_verified(acceptance, expected.needs_mutation) {
                return CompletionVerdict::CompletedUnverified;
            }
            if expected.needs_mutation && !expected.has_mutation {
                return CompletionVerdict::CompletedUnverified;
            }
            CompletionVerdict::Verified
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acceptance::{AcceptanceEvidence, AcceptanceStatus};
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
            }],
            scope_ok: true,
            scope_violations: vec![],
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
            }],
            scope_ok: true,
            scope_violations: vec![],
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
            }],
            scope_ok: true,
            scope_violations: vec![],
        }
    }

    fn ac(required: bool, status: AcceptanceStatus) -> AcceptanceEvidence {
        AcceptanceEvidence {
            id: "AC-1".into(),
            description: "d".into(),
            required,
            status,
            command: None,
            evidence: String::new(),
            reject_reason: None,
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
            finalize_task_outcome(&gate_failed(), None, expected(false, false)),
            CompletionVerdict::Failed
        );
        assert_eq!(
            finalize_task_outcome(&gate_failed(), None, expected(true, true)),
            CompletionVerdict::Failed
        );
    }

    #[test]
    fn health_unverified_is_completed_unverified() {
        assert_eq!(
            finalize_task_outcome(&gate_unverified(), None, expected(false, false)),
            CompletionVerdict::CompletedUnverified
        );
    }

    #[test]
    fn verified_when_health_ok_no_acceptance_no_mutation_required() {
        assert_eq!(
            finalize_task_outcome(&gate_passed(), None, expected(false, false)),
            CompletionVerdict::Verified
        );
    }

    #[test]
    fn needs_mutation_without_mutation_downgrades() {
        assert_eq!(
            finalize_task_outcome(&gate_passed(), None, expected(true, false)),
            CompletionVerdict::CompletedUnverified
        );
    }

    #[test]
    fn needs_mutation_with_mutation_but_no_acceptance_is_not_verified() {
        // rust-h3 class hole: green gates + mutation + no proven required AC
        // must not claim Verified (implementation-class ⇒ need Met required).
        assert_eq!(
            finalize_task_outcome(&gate_passed(), None, expected(true, true)),
            CompletionVerdict::CompletedUnverified
        );
        let empty = AcceptanceLedger::default();
        assert_eq!(
            finalize_task_outcome(&gate_passed(), Some(&empty), expected(true, true)),
            CompletionVerdict::CompletedUnverified
        );
    }

    #[test]
    fn impl_class_with_only_unverifiable_required_is_not_verified() {
        let ledger = AcceptanceLedger {
            items: vec![ac(true, AcceptanceStatus::Unverifiable)],
        };
        assert_eq!(
            finalize_task_outcome(&gate_passed(), Some(&ledger), expected(true, true)),
            CompletionVerdict::CompletedUnverified
        );
    }

    #[test]
    fn impl_class_with_only_optional_fallback_is_not_verified() {
        // K11 fallback is optional Unverifiable — does not hard-block non-impl,
        // but cannot *prove* implementation-class tasks either.
        let ledger = AcceptanceLedger {
            items: vec![ac(false, AcceptanceStatus::Unverifiable)],
        };
        assert_eq!(
            finalize_task_outcome(&gate_passed(), Some(&ledger), expected(true, true)),
            CompletionVerdict::CompletedUnverified
        );
    }

    #[test]
    fn impl_class_with_met_required_acceptance_is_verified() {
        let ledger = AcceptanceLedger {
            items: vec![ac(true, AcceptanceStatus::Met)],
        };
        assert_eq!(
            finalize_task_outcome(&gate_passed(), Some(&ledger), expected(true, true)),
            CompletionVerdict::Verified
        );
    }

    #[test]
    fn has_mutation_does_not_force_needs_mutation() {
        // needs=false even with has=true still Verified (readonly task with
        // incidental files would not require mutation; engine decides needs).
        assert_eq!(
            finalize_task_outcome(&gate_passed(), None, expected(false, true)),
            CompletionVerdict::Verified
        );
    }

    #[test]
    fn required_unmet_acceptance_downgrades() {
        let ledger = AcceptanceLedger {
            items: vec![ac(true, AcceptanceStatus::Unmet)],
        };
        assert_eq!(
            finalize_task_outcome(&gate_passed(), Some(&ledger), expected(true, true)),
            CompletionVerdict::CompletedUnverified
        );
    }

    #[test]
    fn required_unverifiable_acceptance_downgrades() {
        let ledger = AcceptanceLedger {
            items: vec![ac(true, AcceptanceStatus::Unverifiable)],
        };
        assert_eq!(
            finalize_task_outcome(&gate_passed(), Some(&ledger), expected(true, true)),
            CompletionVerdict::CompletedUnverified
        );
    }

    #[test]
    fn optional_unverifiable_allows_verified_for_non_impl() {
        // Non-implementation tasks (needs_mutation=false) may Verified without
        // proven required AC; optional fallback does not block.
        let ledger = AcceptanceLedger {
            items: vec![ac(false, AcceptanceStatus::Unverifiable)],
        };
        assert_eq!(
            finalize_task_outcome(&gate_passed(), Some(&ledger), expected(false, false)),
            CompletionVerdict::Verified
        );
    }

    #[test]
    fn empty_acceptance_ledger_allows_verified_for_non_impl() {
        let ledger = AcceptanceLedger::default();
        assert_eq!(
            finalize_task_outcome(&gate_passed(), Some(&ledger), expected(false, false)),
            CompletionVerdict::Verified
        );
    }

    #[test]
    fn acceptance_failure_does_not_become_failed() {
        // Acceptance never hard-fails; only health Failed does.
        let ledger = AcceptanceLedger {
            items: vec![ac(true, AcceptanceStatus::Unmet)],
        };
        assert_eq!(
            finalize_task_outcome(&gate_passed(), Some(&ledger), expected(false, false)),
            CompletionVerdict::CompletedUnverified
        );
    }

    #[test]
    fn mutation_gap_does_not_override_health_failed() {
        assert_eq!(
            finalize_task_outcome(&gate_failed(), None, expected(true, false)),
            CompletionVerdict::Failed
        );
    }
}
