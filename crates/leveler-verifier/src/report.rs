//! The verification report and completion gate (spec §29, §30).

use std::collections::BTreeSet;

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
    /// Test-level failure identifiers parsed from a failed Test check's output
    /// (empty for non-Test checks, passing checks, or unparsable output). This
    /// is what baseline delta attribution diffs against the baseline run so a
    /// whole-suite command red for the SAME tests on both trees is judged
    /// pre-existing, while a newly-failing test still gates.
    #[serde(default)]
    pub failed_tests: BTreeSet<String>,
}

/// The full verification report for a task (spec §29).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    pub checks: Vec<CheckOutcome>,
    /// Whether all modified files stayed within the allowed scope.
    pub scope_ok: bool,
    /// Paths modified outside the allowed scope.
    pub scope_violations: Vec<String>,
    /// Names of gating checks whose failure was attributed to the pre-change
    /// baseline (see [`Self::attribute_baseline`]) and therefore does NOT gate
    /// completion. Empty when no baseline was consulted.
    #[serde(default)]
    pub baseline_failures: Vec<String>,
}

impl VerificationReport {
    /// The completion gate (spec §30): scope holds and no gating check failed.
    /// Note this does not mean the run is verified — see [`Self::verdict`].
    pub fn passed(&self) -> bool {
        self.verdict() != Verdict::Failed
    }

    /// Reconcile this (working-tree) report against the same plan run on the
    /// pre-change baseline, recording which gating failures pre-date the change
    /// so they stop gating completion. `base` is the baseline run's report.
    ///
    /// A currently-failing gating check is pre-existing when:
    /// - **Test check**: it has parsed failing tests AND every one of them was
    ///   already failing on the baseline. No parsed tests (compile/infra error)
    ///   → cannot prove sameness → still gates (never suppress on no evidence).
    /// - **Non-Test check** (build/fmt/lint, no test granularity): the same
    ///   check also failed on the baseline (exit-code level).
    ///
    /// A check the baseline did not fail (passed or absent) is always a genuine
    /// new failure and keeps gating.
    pub fn attribute_baseline(&mut self, base: &VerificationReport) {
        self.baseline_failures = self
            .checks
            .iter()
            .filter(|c| c.gating && c.status == CheckStatus::Failed && pre_dates_change(c, base))
            .map(|c| c.name.clone())
            .collect();
    }

    /// Whether a failing check was attributed to the baseline by the most recent
    /// [`Self::attribute_baseline`] call.
    fn is_pre_existing(&self, check: &CheckOutcome) -> bool {
        check.status == CheckStatus::Failed
            && self.baseline_failures.iter().any(|n| n == &check.name)
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
                .any(|c| c.gating && c.status == CheckStatus::Failed && !self.is_pre_existing(c))
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
            .map(|c| {
                if self.is_pre_existing(c) {
                    return format!("{} (pre-existing failure)", c.name);
                }
                match c.status {
                    CheckStatus::ToolMissing => format!("{} (tool missing)", c.name),
                    _ => format!("{} (skipped)", c.name),
                }
            })
            .collect();
        Verdict::Unverified(format!("gating checks did not run: {}", unrun.join(", ")))
    }

    /// The gating checks that failed and are NOT attributed to the baseline.
    pub fn failed_gates(&self) -> Vec<&CheckOutcome> {
        self.checks
            .iter()
            .filter(|c| c.gating && c.status == CheckStatus::Failed && !self.is_pre_existing(c))
            .collect()
    }
}

/// Whether `working`'s failure pre-dates the change, judged against the baseline
/// run `base`. See [`VerificationReport::attribute_baseline`] for the rules.
fn pre_dates_change(working: &CheckOutcome, base: &VerificationReport) -> bool {
    let Some(base_check) = base
        .checks
        .iter()
        .find(|b| b.name == working.name && b.status == CheckStatus::Failed)
    else {
        // Baseline passed this check (or never ran it) → genuinely new.
        return false;
    };
    if working.kind == CheckKind::Test {
        // Require test-level proof: every failing test was already failing on
        // the baseline. Empty (unparsable / compile error) → cannot prove.
        !working.failed_tests.is_empty()
            && working
                .failed_tests
                .iter()
                .all(|t| base_check.failed_tests.contains(t))
    } else {
        // No test granularity — the same check failing on the baseline is the
        // best signal available.
        true
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
            failed_tests: BTreeSet::new(),
        }
    }

    #[test]
    fn passes_when_gates_pass_and_scope_ok() {
        let report = VerificationReport {
            checks: vec![check("build", true, CheckStatus::Passed)],
            scope_ok: true,
            scope_violations: vec![],
            baseline_failures: vec![],
        };
        assert!(report.passed());
    }

    #[test]
    fn fails_when_a_gate_fails() {
        let report = VerificationReport {
            checks: vec![check("test", true, CheckStatus::Failed)],
            scope_ok: true,
            scope_violations: vec![],
            baseline_failures: vec![],
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
            baseline_failures: vec![],
        };
        assert!(report.passed());
    }

    #[test]
    fn scope_violation_blocks_completion() {
        let report = VerificationReport {
            checks: vec![check("build", true, CheckStatus::Passed)],
            scope_ok: false,
            scope_violations: vec!["../evil.rs".into()],
            baseline_failures: vec![],
        };
        assert!(!report.passed());
    }

    fn report(checks: Vec<CheckOutcome>) -> VerificationReport {
        VerificationReport {
            checks,
            scope_ok: true,
            scope_violations: vec![],
            baseline_failures: vec![],
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

    // ── Test-level baseline delta attribution (A) ────────────────────────

    fn test_check(name: &str, status: CheckStatus, tests: &[&str]) -> CheckOutcome {
        CheckOutcome {
            name: name.to_string(),
            kind: CheckKind::Test,
            gating: true,
            status,
            evidence: String::new(),
            failure: None,
            failed_tests: tests.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn same_failing_tests_on_both_trees_are_pre_existing() {
        // Whole-suite check red for the same tests on base and working → the
        // change introduced nothing new → does not gate.
        let mut working = report(vec![test_check(
            "cargo test",
            CheckStatus::Failed,
            &["a::flaky", "a::env"],
        )]);
        let base = report(vec![test_check(
            "cargo test",
            CheckStatus::Failed,
            &["a::flaky", "a::env"],
        )]);
        working.attribute_baseline(&base);
        assert!(working.failed_gates().is_empty());
        assert_ne!(working.verdict(), Verdict::Failed);
    }

    #[test]
    fn a_new_failing_test_still_gates_even_if_others_pre_exist() {
        // base is red for a::flaky; working is red for a::flaky AND a::new_bug.
        // The whole check exit code is red on both, but the NEW test must gate.
        let mut working = report(vec![test_check(
            "cargo test",
            CheckStatus::Failed,
            &["a::flaky", "a::new_bug"],
        )]);
        let base = report(vec![test_check(
            "cargo test",
            CheckStatus::Failed,
            &["a::flaky"],
        )]);
        working.attribute_baseline(&base);
        let failed: Vec<&str> = working
            .failed_gates()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(
            failed,
            vec!["cargo test"],
            "new test failure must not be swallowed"
        );
        assert_eq!(working.verdict(), Verdict::Failed);
    }

    #[test]
    fn failing_test_check_with_no_parsed_tests_never_suppressed() {
        // A Test check that failed but yielded no parsed tests (e.g. the test
        // target failed to COMPILE) cannot be proven pre-existing → still gates,
        // even though the baseline also failed. Safety over convenience.
        let mut working = report(vec![test_check("cargo test", CheckStatus::Failed, &[])]);
        let base = report(vec![test_check("cargo test", CheckStatus::Failed, &[])]);
        working.attribute_baseline(&base);
        assert_eq!(working.failed_gates().len(), 1);
        assert_eq!(working.verdict(), Verdict::Failed);
    }

    #[test]
    fn non_test_check_uses_exit_code_baseline() {
        // build has no test granularity: base build also red → pre-existing.
        let mut working = report(vec![check("build", true, CheckStatus::Failed)]);
        let base = report(vec![check("build", true, CheckStatus::Failed)]);
        working.attribute_baseline(&base);
        assert!(working.failed_gates().is_empty());
    }

    #[test]
    fn failure_absent_from_baseline_is_new_and_gates() {
        // base passed this check entirely → any working failure is the change's.
        let mut working = report(vec![test_check(
            "cargo test",
            CheckStatus::Failed,
            &["a::x"],
        )]);
        let base = report(vec![test_check("cargo test", CheckStatus::Passed, &[])]);
        working.attribute_baseline(&base);
        assert_eq!(working.failed_gates().len(), 1);
        assert_eq!(working.verdict(), Verdict::Failed);
    }

    #[test]
    fn scope_violation_is_failed_verdict() {
        let r = VerificationReport {
            checks: vec![check("build", true, CheckStatus::Passed)],
            scope_ok: false,
            scope_violations: vec!["../evil.rs".into()],
            baseline_failures: vec![],
        };
        assert_eq!(r.verdict(), Verdict::Failed);
    }
}
