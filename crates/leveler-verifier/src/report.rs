//! The verification report and completion gate (spec §29, §30).

use serde::{Deserialize, Serialize};

use crate::failure::ClassifiedFailure;
use crate::plan::CheckKind;

/// The status of one verification check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    /// Deliberately not run (e.g. cancelled or narrowed away).
    Skipped,
    /// The check's program is not on PATH, so it could not run at all.
    ToolMissing,
}

/// The three-way completion verdict. `Unverified` is not a failure — the task
/// may still complete — but callers must not report it as verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", content = "reason", rename_all = "snake_case")]
pub enum Verdict {
    /// Every applicable gating check ran and passed; none failed.
    Verified,
    /// No gating check produced evidence (none configured, or none could run).
    Unverified(String),
    /// A gating check failed or the scope was violated.
    Failed,
}

/// The outcome of running one verification command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckOutcome {
    pub name: String,
    pub kind: CheckKind,
    pub gating: bool,
    pub status: CheckStatus,
    /// Captured command output (truncated) — the evidence.
    pub evidence: String,
    /// Present when the check failed.
    pub failure: Option<ClassifiedFailure>,
}

/// The full verification report for a task (spec §29).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    pub checks: Vec<CheckOutcome>,
    /// Whether all modified files stayed within the allowed scope.
    pub scope_ok: bool,
    /// Paths modified outside the allowed scope.
    pub scope_violations: Vec<String>,
}

impl VerificationReport {
    /// The completion gate (spec §30): scope holds and no gating check failed.
    /// Note this does not mean the run is verified — see [`Self::verdict`].
    pub fn passed(&self) -> bool {
        self.verdict() != Verdict::Failed
    }

    /// The three-way verdict: whether completion is actually evidence-backed.
    ///
    /// `Verified` requires `scope_ok`, at least one applicable (gating) check,
    /// and **every** applicable check `Passed`. ToolMissing / Skipped / not-run
    /// yield `Unverified` (v1 does not treat ToolMissing as non-applicable).
    pub fn verdict(&self) -> Verdict {
        if !self.scope_ok
            || self
                .checks
                .iter()
                .any(|c| c.gating && c.status == CheckStatus::Failed)
        {
            return Verdict::Failed;
        }

        let applicable: Vec<&CheckOutcome> = self.checks.iter().filter(|c| c.gating).collect();
        if applicable.is_empty() {
            return Verdict::Unverified(
                "no gating verification checks were configured".to_string(),
            );
        }
        if applicable.iter().all(|c| c.status == CheckStatus::Passed) {
            return Verdict::Verified;
        }

        let unrun: Vec<String> = applicable
            .iter()
            .filter(|c| c.status != CheckStatus::Passed)
            .map(|c| match c.status {
                CheckStatus::ToolMissing => format!("{} (tool missing)", c.name),
                _ => format!("{} (skipped)", c.name),
            })
            .collect();
        Verdict::Unverified(format!("gating checks did not run: {}", unrun.join(", ")))
    }

    /// The gating checks that failed.
    pub fn failed_gates(&self) -> Vec<&CheckOutcome> {
        self.checks
            .iter()
            .filter(|c| c.gating && c.status == CheckStatus::Failed)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, gating: bool, status: CheckStatus) -> CheckOutcome {
        CheckOutcome {
            name: name.to_string(),
            kind: CheckKind::Build,
            gating,
            status,
            evidence: String::new(),
            failure: None,
        }
    }

    #[test]
    fn passes_when_gates_pass_and_scope_ok() {
        let report = VerificationReport {
            checks: vec![check("build", true, CheckStatus::Passed)],
            scope_ok: true,
            scope_violations: vec![],
        };
        assert!(report.passed());
    }

    #[test]
    fn fails_when_a_gate_fails() {
        let report = VerificationReport {
            checks: vec![check("test", true, CheckStatus::Failed)],
            scope_ok: true,
            scope_violations: vec![],
        };
        assert!(!report.passed());
        assert_eq!(report.failed_gates().len(), 1);
    }

    #[test]
    fn non_gating_failure_does_not_block() {
        let report = VerificationReport {
            checks: vec![check("fmt", false, CheckStatus::Failed)],
            scope_ok: true,
            scope_violations: vec![],
        };
        assert!(report.passed());
    }

    #[test]
    fn scope_violation_blocks_completion() {
        let report = VerificationReport {
            checks: vec![check("build", true, CheckStatus::Passed)],
            scope_ok: false,
            scope_violations: vec!["../evil.rs".into()],
        };
        assert!(!report.passed());
    }

    fn report(checks: Vec<CheckOutcome>) -> VerificationReport {
        VerificationReport {
            checks,
            scope_ok: true,
            scope_violations: vec![],
        }
    }

    #[test]
    fn empty_plan_is_unverified() {
        assert!(matches!(report(vec![]).verdict(), Verdict::Unverified(_)));
    }

    #[test]
    fn non_gating_only_is_unverified() {
        let r = report(vec![check("fmt", false, CheckStatus::Passed)]);
        assert!(matches!(r.verdict(), Verdict::Unverified(_)));
    }

    #[test]
    fn all_skipped_gates_is_unverified() {
        let r = report(vec![
            check("build", true, CheckStatus::Skipped),
            check("test", true, CheckStatus::Skipped),
        ]);
        assert!(matches!(r.verdict(), Verdict::Unverified(_)));
    }

    #[test]
    fn tool_missing_gate_is_unverified_with_reason() {
        let r = report(vec![check("tsc", true, CheckStatus::ToolMissing)]);
        match r.verdict() {
            Verdict::Unverified(reason) => assert!(reason.contains("tsc"), "reason: {reason}"),
            other => panic!("expected Unverified, got {other:?}"),
        }
        // Unverified does not block completion.
        assert!(r.passed());
    }

    #[test]
    fn all_applicable_gates_passed_is_verified() {
        let r = report(vec![
            check("build", true, CheckStatus::Passed),
            check("test", true, CheckStatus::Passed),
            check("fmt", false, CheckStatus::Failed), // non-gating ignored
        ]);
        assert_eq!(r.verdict(), Verdict::Verified);
    }

    #[test]
    fn one_passed_one_tool_missing_is_unverified() {
        let r = report(vec![
            check("build", true, CheckStatus::Passed),
            check("tsc", true, CheckStatus::ToolMissing),
        ]);
        match r.verdict() {
            Verdict::Unverified(reason) => {
                assert!(reason.contains("tsc"), "reason: {reason}");
            }
            other => panic!("expected Unverified, got {other:?}"),
        }
        // Unverified does not block completion.
        assert!(r.passed());
    }

    #[test]
    fn one_passed_one_skipped_is_unverified() {
        let r = report(vec![
            check("build", true, CheckStatus::Passed),
            check("test", true, CheckStatus::Skipped),
        ]);
        assert!(matches!(r.verdict(), Verdict::Unverified(_)));
        assert!(r.passed());
    }

    #[test]
    fn failed_gate_is_failed_verdict() {
        let r = report(vec![
            check("build", true, CheckStatus::Passed),
            check("test", true, CheckStatus::Failed),
        ]);
        assert_eq!(r.verdict(), Verdict::Failed);
    }

    #[test]
    fn scope_violation_is_failed_verdict() {
        let r = VerificationReport {
            checks: vec![check("build", true, CheckStatus::Passed)],
            scope_ok: false,
            scope_violations: vec!["../evil.rs".into()],
        };
        assert_eq!(r.verdict(), Verdict::Failed);
    }
}
