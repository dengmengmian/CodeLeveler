//! The verification report and completion gate (spec §29, §30).

use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::failure::ClassifiedFailure;
use crate::plan::CheckKind;

/// Why a planned check did not produce a pass/fail observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotRunReason {
    ToolMissing,
    EnvironmentUnavailable,
    VerificationIncomplete,
    DependencyUnavailable,
}

impl NotRunReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolMissing => "tool_missing",
            Self::EnvironmentUnavailable => "environment_unavailable",
            Self::VerificationIncomplete => "verification_incomplete",
            Self::DependencyUnavailable => "dependency_unavailable",
        }
    }
}

/// What the verifier actually observed when it attempted a check.
/// This is independent of whether that observation gates completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "reason", rename_all = "snake_case")]
pub enum CheckObservation {
    Passed,
    Failed,
    NotRun(NotRunReason),
}

/// How a baseline comparison was mechanically produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineSource {
    DetachedWorktreeRerun,
}

/// Evidence that a failed test was already failing before the change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineProvenance {
    pub source: BaselineSource,
    pub failed_tests: BTreeSet<String>,
}

/// Why a configured gate was not charged to the current change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "evidence", rename_all = "snake_case")]
pub enum GateSkipReason {
    ConfirmedBaselineFailure {
        revision: String,
        provenance: BaselineProvenance,
    },
    NotApplicable,
    Superseded,
}

/// Whether this check's observation participates in the completion gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "reason", rename_all = "snake_case")]
pub enum GateDisposition {
    Required,
    Skipped(GateSkipReason),
}

impl Default for GateDisposition {
    fn default() -> Self {
        Self::Required
    }
}

/// Legacy one-dimensional wire status.
///
/// This type remains only to read and project older event rows. Verdicts use
/// [`CheckObservation`] and [`GateDisposition`] as their sole authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    Skipped,
    ToolMissing,
    EnvironmentUnavailable,
}

/// The process result obtained while producing a check observation.
/// `None` on [`CheckOutcome`] means no process result was obtained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckExecution {
    /// The exact executable spelling passed to the process authority.
    #[serde(default)]
    pub program: String,
    /// The effective arguments after verifier-owned scoping or completion
    /// flags were applied.
    #[serde(default)]
    pub args: Vec<String>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

impl CheckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::ToolMissing => "tool_missing",
            Self::EnvironmentUnavailable => "environment_unavailable",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "passed" => Some(Self::Passed),
            "failed" => Some(Self::Failed),
            "skipped" => Some(Self::Skipped),
            "tool_missing" | "toolmissing" => Some(Self::ToolMissing),
            "environment_unavailable" | "environmentunavailable" => {
                Some(Self::EnvironmentUnavailable)
            }
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for CheckStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_wire(&value).ok_or_else(|| D::Error::custom(format!("unknown status: {value}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", content = "reason", rename_all = "snake_case")]
pub enum Verdict {
    Verified,
    Unverified(String),
    Failed,
}

/// The outcome of one planned verification command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub name: String,
    pub kind: CheckKind,
    pub gating: bool,
    pub observation: CheckObservation,
    pub disposition: GateDisposition,
    pub execution: Option<CheckExecution>,
    pub evidence: String,
    pub failure: Option<ClassifiedFailure>,
    pub failed_tests: BTreeSet<String>,
}

impl CheckOutcome {
    pub fn passed(
        name: String,
        kind: CheckKind,
        gating: bool,
        execution: CheckExecution,
        evidence: String,
    ) -> Self {
        Self {
            name,
            kind,
            gating,
            observation: CheckObservation::Passed,
            disposition: GateDisposition::Required,
            execution: Some(execution),
            evidence,
            failure: None,
            failed_tests: BTreeSet::new(),
        }
    }

    pub fn failed(
        name: String,
        kind: CheckKind,
        gating: bool,
        execution: Option<CheckExecution>,
        evidence: String,
        failure: ClassifiedFailure,
        failed_tests: BTreeSet<String>,
    ) -> Self {
        Self {
            name,
            kind,
            gating,
            observation: CheckObservation::Failed,
            disposition: GateDisposition::Required,
            execution,
            evidence,
            failure: Some(failure),
            failed_tests,
        }
    }

    pub fn not_run(
        name: String,
        kind: CheckKind,
        gating: bool,
        reason: NotRunReason,
        execution: Option<CheckExecution>,
        evidence: String,
        failure: Option<ClassifiedFailure>,
    ) -> Self {
        Self {
            name,
            kind,
            gating,
            observation: CheckObservation::NotRun(reason),
            disposition: GateDisposition::Required,
            execution,
            evidence,
            failure,
            failed_tests: BTreeSet::new(),
        }
    }

    /// Read-only compatibility projection. Never use this for verdicts.
    pub fn legacy_status(&self) -> CheckStatus {
        match (&self.observation, &self.disposition) {
            (_, GateDisposition::Skipped(_)) => CheckStatus::Skipped,
            (CheckObservation::Passed, GateDisposition::Required) => CheckStatus::Passed,
            (CheckObservation::Failed, GateDisposition::Required) => CheckStatus::Failed,
            (CheckObservation::NotRun(NotRunReason::ToolMissing), GateDisposition::Required) => {
                CheckStatus::ToolMissing
            }
            (
                CheckObservation::NotRun(NotRunReason::EnvironmentUnavailable),
                GateDisposition::Required,
            ) => CheckStatus::EnvironmentUnavailable,
            (CheckObservation::NotRun(_), GateDisposition::Required) => CheckStatus::Skipped,
        }
    }

    pub fn confirmed_baseline_failure(&self) -> Option<(&str, &BaselineProvenance)> {
        match &self.disposition {
            GateDisposition::Skipped(GateSkipReason::ConfirmedBaselineFailure {
                revision,
                provenance,
            }) => Some((revision, provenance)),
            _ => None,
        }
    }
}

/// Emit the typed truth plus a derived `status` for old readers.
impl Serialize for CheckOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct Wire<'a> {
            name: &'a str,
            kind: CheckKind,
            gating: bool,
            observation: &'a CheckObservation,
            disposition: &'a GateDisposition,
            execution: &'a Option<CheckExecution>,
            status: CheckStatus,
            evidence: &'a str,
            failure: &'a Option<ClassifiedFailure>,
            failed_tests: &'a BTreeSet<String>,
        }

        Wire {
            name: &self.name,
            kind: self.kind,
            gating: self.gating,
            observation: &self.observation,
            disposition: &self.disposition,
            execution: &self.execution,
            status: self.legacy_status(),
            evidence: &self.evidence,
            failure: &self.failure,
            failed_tests: &self.failed_tests,
        }
        .serialize(serializer)
    }
}

/// Old rows contain only `status`. Ambiguous `skipped` is upgraded
/// conservatively, never fabricated into a baseline exemption.
impl<'de> Deserialize<'de> for CheckOutcome {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            name: String,
            kind: CheckKind,
            gating: bool,
            #[serde(default)]
            observation: Option<CheckObservation>,
            #[serde(default)]
            disposition: Option<GateDisposition>,
            #[serde(default)]
            execution: Option<CheckExecution>,
            #[serde(default)]
            status: Option<CheckStatus>,
            #[serde(default)]
            evidence: String,
            #[serde(default)]
            failure: Option<ClassifiedFailure>,
            #[serde(default)]
            failed_tests: BTreeSet<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let observation = match (wire.observation, wire.status) {
            (Some(observation), _) => observation,
            (None, Some(CheckStatus::Passed)) => CheckObservation::Passed,
            (None, Some(CheckStatus::Failed)) => CheckObservation::Failed,
            (None, Some(CheckStatus::ToolMissing)) => {
                CheckObservation::NotRun(NotRunReason::ToolMissing)
            }
            (None, Some(CheckStatus::EnvironmentUnavailable)) => {
                CheckObservation::NotRun(NotRunReason::EnvironmentUnavailable)
            }
            (None, Some(CheckStatus::Skipped)) => {
                CheckObservation::NotRun(NotRunReason::VerificationIncomplete)
            }
            (None, None) => return Err(D::Error::missing_field("observation or legacy status")),
        };

        Ok(Self {
            name: wire.name,
            kind: wire.kind,
            gating: wire.gating,
            observation,
            disposition: wire.disposition.unwrap_or_default(),
            execution: wire.execution,
            evidence: wire.evidence,
            failure: wire.failure,
            failed_tests: wire.failed_tests,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    pub checks: Vec<CheckOutcome>,
    pub scope_ok: bool,
    pub scope_violations: Vec<String>,
}

impl VerificationReport {
    pub fn passed(&self) -> bool {
        self.verdict() != Verdict::Failed
    }

    /// Only parsed Test failures can be mechanically attributed to a baseline.
    pub fn attribute_baseline(&mut self, base: &VerificationReport, revision: &str) {
        for working in &mut self.checks {
            if !working.gating
                || working.kind != CheckKind::Test
                || working.observation != CheckObservation::Failed
                || working.disposition != GateDisposition::Required
                || working.failed_tests.is_empty()
            {
                continue;
            }
            let Some(working_execution) = working.execution.as_ref() else {
                continue;
            };
            let Some(base_check) = base.checks.iter().find(|candidate| {
                candidate.name == working.name
                    && candidate.kind == CheckKind::Test
                    && candidate.observation == CheckObservation::Failed
                    && !candidate.failed_tests.is_empty()
                    && candidate
                        .execution
                        .as_ref()
                        .is_some_and(|baseline_execution| {
                            baseline_execution.program == working_execution.program
                                && baseline_execution.args == working_execution.args
                        })
            }) else {
                continue;
            };
            if working
                .failed_tests
                .iter()
                .all(|test| base_check.failed_tests.contains(test))
            {
                working.disposition =
                    GateDisposition::Skipped(GateSkipReason::ConfirmedBaselineFailure {
                        revision: revision.to_string(),
                        provenance: BaselineProvenance {
                            source: BaselineSource::DetachedWorktreeRerun,
                            failed_tests: base_check.failed_tests.clone(),
                        },
                    });
            }
        }
    }

    pub fn has_gating_checks(&self) -> bool {
        self.checks.iter().any(|check| check.gating)
    }

    pub fn verdict(&self) -> Verdict {
        if !self.scope_ok
            || self.checks.iter().any(|check| {
                check.gating
                    && check.disposition == GateDisposition::Required
                    && check.observation == CheckObservation::Failed
            })
        {
            return Verdict::Failed;
        }
        if !self.has_gating_checks() {
            return Verdict::Unverified(
                "no gating verification checks were configured".to_string(),
            );
        }
        let gating: Vec<&CheckOutcome> = self.checks.iter().filter(|check| check.gating).collect();
        if gating.iter().all(|check| {
            check.disposition == GateDisposition::Required
                && check.observation == CheckObservation::Passed
        }) {
            return Verdict::Verified;
        }
        let incomplete = gating
            .into_iter()
            .filter(|check| {
                check.disposition != GateDisposition::Required
                    || check.observation != CheckObservation::Passed
            })
            .map(describe_incomplete_gate)
            .collect::<Vec<_>>();
        Verdict::Unverified(format!(
            "verification incomplete: {}",
            incomplete.join(", ")
        ))
    }

    pub fn failed_gates(&self) -> Vec<&CheckOutcome> {
        self.checks
            .iter()
            .filter(|check| {
                check.gating
                    && check.disposition == GateDisposition::Required
                    && check.observation == CheckObservation::Failed
            })
            .collect()
    }

    pub fn confirmed_baseline_failures(&self) -> Vec<&CheckOutcome> {
        self.checks
            .iter()
            .filter(|check| check.confirmed_baseline_failure().is_some())
            .collect()
    }
}

fn describe_incomplete_gate(check: &CheckOutcome) -> String {
    match (&check.observation, &check.disposition) {
        (
            CheckObservation::Failed,
            GateDisposition::Skipped(GateSkipReason::ConfirmedBaselineFailure { revision, .. }),
        ) => format!(
            "{} (pre-existing test failure at {})",
            check.name,
            short_revision(revision)
        ),
        (_, GateDisposition::Skipped(GateSkipReason::NotApplicable)) => {
            format!("{} (not applicable)", check.name)
        }
        (_, GateDisposition::Skipped(GateSkipReason::Superseded)) => {
            format!("{} (superseded)", check.name)
        }
        (CheckObservation::NotRun(reason), GateDisposition::Required) => {
            format!("{} ({})", check.name, describe_not_run(*reason))
        }
        (CheckObservation::Failed, GateDisposition::Required) => {
            format!("{} (failed)", check.name)
        }
        (CheckObservation::Passed, GateDisposition::Skipped(_)) => {
            format!("{} (gate skipped)", check.name)
        }
        (CheckObservation::Passed, GateDisposition::Required) => check.name.clone(),
        (CheckObservation::NotRun(reason), GateDisposition::Skipped(_)) => {
            format!("{} ({})", check.name, describe_not_run(*reason))
        }
    }
}

fn describe_not_run(reason: NotRunReason) -> &'static str {
    match reason {
        NotRunReason::ToolMissing => "tool missing",
        NotRunReason::EnvironmentUnavailable => "environment mismatch",
        NotRunReason::VerificationIncomplete => "verification incomplete",
        NotRunReason::DependencyUnavailable => "dependency unavailable",
    }
}

fn short_revision(revision: &str) -> &str {
    revision.get(..revision.len().min(12)).unwrap_or(revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(
        name: &str,
        kind: CheckKind,
        gating: bool,
        observation: CheckObservation,
    ) -> CheckOutcome {
        CheckOutcome {
            name: name.to_string(),
            kind,
            gating,
            observation,
            disposition: GateDisposition::Required,
            execution: None,
            evidence: String::new(),
            failure: None,
            failed_tests: BTreeSet::new(),
        }
    }

    fn test_failure(name: &str, tests: &[&str]) -> CheckOutcome {
        let mut outcome = check(name, CheckKind::Test, true, CheckObservation::Failed);
        outcome.execution = Some(CheckExecution {
            program: "cargo".into(),
            args: vec!["test".into()],
            exit_code: Some(1),
            timed_out: false,
        });
        outcome.failed_tests = tests.iter().map(|test| test.to_string()).collect();
        outcome
    }

    fn report(checks: Vec<CheckOutcome>) -> VerificationReport {
        VerificationReport {
            checks,
            scope_ok: true,
            scope_violations: vec![],
        }
    }

    #[test]
    fn required_passes_verify_and_required_failures_gate() {
        assert_eq!(
            report(vec![check(
                "build",
                CheckKind::Build,
                true,
                CheckObservation::Passed
            )])
            .verdict(),
            Verdict::Verified
        );
        let failed = report(vec![check(
            "test",
            CheckKind::Test,
            true,
            CheckObservation::Failed,
        )]);
        assert_eq!(failed.verdict(), Verdict::Failed);
        assert_eq!(failed.failed_gates().len(), 1);
    }

    #[test]
    fn non_gating_failure_does_not_block_but_does_not_invent_a_gate() {
        let report = report(vec![check(
            "fmt",
            CheckKind::Format,
            false,
            CheckObservation::Failed,
        )]);
        assert!(report.passed());
        assert!(!report.has_gating_checks());
        assert!(matches!(report.verdict(), Verdict::Unverified(_)));
    }

    #[test]
    fn scope_violation_blocks_completion() {
        let mut report = report(vec![check(
            "build",
            CheckKind::Build,
            true,
            CheckObservation::Passed,
        )]);
        report.scope_ok = false;
        report.scope_violations.push("../evil.rs".into());
        assert_eq!(report.verdict(), Verdict::Failed);
    }

    #[test]
    fn not_run_is_unverified_with_typed_reason() {
        for reason in [
            NotRunReason::ToolMissing,
            NotRunReason::EnvironmentUnavailable,
            NotRunReason::VerificationIncomplete,
            NotRunReason::DependencyUnavailable,
        ] {
            let report = report(vec![check(
                "test",
                CheckKind::Test,
                true,
                CheckObservation::NotRun(reason),
            )]);
            assert!(report.passed());
            match report.verdict() {
                Verdict::Unverified(detail) => assert!(detail.contains(describe_not_run(reason))),
                other => panic!("expected Unverified, got {other:?}"),
            }
        }
    }

    #[test]
    fn grounded_test_baseline_failure_keeps_failed_observation_and_skips_gate() {
        let revision = "0123456789abcdef";
        let mut working = report(vec![test_failure("cargo test", &["a::flaky", "a::env"])]);
        let base = report(vec![test_failure("cargo test", &["a::flaky", "a::env"])]);
        working.attribute_baseline(&base, revision);

        let check = &working.checks[0];
        assert_eq!(check.observation, CheckObservation::Failed);
        let (stored_revision, provenance) = check
            .confirmed_baseline_failure()
            .expect("baseline evidence is stored on the check");
        assert_eq!(stored_revision, revision);
        assert_eq!(provenance.source, BaselineSource::DetachedWorktreeRerun);
        assert_eq!(provenance.failed_tests, base.checks[0].failed_tests);
        assert!(working.failed_gates().is_empty());
        assert_eq!(working.confirmed_baseline_failures().len(), 1);
        match working.verdict() {
            Verdict::Unverified(reason) => {
                assert!(reason.contains("pre-existing test failure"));
                assert!(!reason.contains("did not run"));
            }
            other => panic!("expected Unverified, got {other:?}"),
        }
    }

    #[test]
    fn new_or_unparsed_test_failure_still_gates() {
        let base = report(vec![test_failure("cargo test", &["a::flaky"])]);
        let mut new_failure = report(vec![test_failure(
            "cargo test",
            &["a::flaky", "a::new_bug"],
        )]);
        new_failure.attribute_baseline(&base, "base");
        assert_eq!(new_failure.verdict(), Verdict::Failed);

        let mut unparsed = report(vec![test_failure("cargo test", &[])]);
        unparsed.attribute_baseline(&report(vec![test_failure("cargo test", &[])]), "base");
        assert_eq!(unparsed.verdict(), Verdict::Failed);
    }

    #[test]
    fn baseline_attribution_requires_the_same_effective_command() {
        let mut working = report(vec![test_failure("go test", &["TestSameName"])]);
        let mut base = report(vec![test_failure("go test", &["TestSameName"])]);
        working.checks[0].execution.as_mut().unwrap().args = vec!["./pkg/new/...".into()];
        base.checks[0].execution.as_mut().unwrap().args = vec!["./...".into()];

        working.attribute_baseline(&base, "base");

        assert_eq!(working.verdict(), Verdict::Failed);
        assert!(working.confirmed_baseline_failures().is_empty());
    }

    #[test]
    fn non_test_exit_failure_is_never_baseline_confirmed() {
        let mut working = report(vec![check(
            "build",
            CheckKind::Build,
            true,
            CheckObservation::Failed,
        )]);
        let base = working.clone();
        working.attribute_baseline(&base, "base");
        assert_eq!(working.verdict(), Verdict::Failed);
        assert!(working.confirmed_baseline_failures().is_empty());
        assert_eq!(working.checks[0].disposition, GateDisposition::Required);
    }

    #[test]
    fn old_check_wire_deserializes_conservatively() {
        let old = r#"{
            "name":"test", "kind":"test", "gating":true,
            "status":"skipped", "evidence":"", "failure":null,
            "failed_tests":[]
        }"#;
        let check: CheckOutcome = serde_json::from_str(old).unwrap();
        assert_eq!(
            check.observation,
            CheckObservation::NotRun(NotRunReason::VerificationIncomplete)
        );
        assert_eq!(check.disposition, GateDisposition::Required);
        assert!(check.execution.is_none());
        assert!(check.confirmed_baseline_failure().is_none());
    }

    #[test]
    fn typed_wire_round_trips_and_carries_legacy_projection() {
        let check = test_failure("test", &["a"]);
        let value = serde_json::to_value(&check).unwrap();
        assert_eq!(value["status"], "failed");
        assert!(value.get("observation").is_some());
        assert!(value.get("disposition").is_some());
        assert_eq!(
            serde_json::from_value::<CheckOutcome>(value).unwrap(),
            check
        );
    }

    #[test]
    fn legacy_status_vocabulary_remains_readable() {
        for (wire, status) in [
            ("passed", CheckStatus::Passed),
            ("failed", CheckStatus::Failed),
            ("skipped", CheckStatus::Skipped),
            ("tool_missing", CheckStatus::ToolMissing),
            ("toolmissing", CheckStatus::ToolMissing),
            (
                "environment_unavailable",
                CheckStatus::EnvironmentUnavailable,
            ),
            (
                "environmentunavailable",
                CheckStatus::EnvironmentUnavailable,
            ),
        ] {
            assert_eq!(CheckStatus::from_wire(wire), Some(status));
        }
    }
}
