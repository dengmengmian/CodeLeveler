//! `leveler-eval` — the evaluation harness (spec §45, §54).
//!
//! Defines evaluation cases, per-case results, aggregate metrics, and the
//! model-comparison "gap" that is the product's core success measure: does the
//! runtime narrow the capability gap between models?
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A single evaluation case (spec §45.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationCase {
    pub id: String,
    pub name: String,
    /// Optional: clone this real git repo as the workspace instead of starting
    /// from an empty one. When set, `files` are applied as an *overlay* on top
    /// of the clone (to inject a bug or a failing test), so the agent has to
    /// locate the relevant code inside a full repository. Path is resolved
    /// relative to the process CWD.
    #[serde(default)]
    pub repo: Option<String>,
    /// Optional git ref (branch/tag/commit) to check out after cloning. Defaults
    /// to the cloned repo's HEAD.
    #[serde(default)]
    pub base_ref: Option<String>,
    /// Files to materialize (path → content). Without `repo`, these ARE the
    /// whole workspace; with `repo`, they overlay the clone.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// The natural-language task handed to the agent.
    pub task: String,
    /// Total agent/model rounds allowed for this case. Evals stay bounded so
    /// completion and effort remain comparable across models.
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
    /// A command that must succeed for the case to pass (run in the repo).
    pub expect: ExpectCommand,
}

const fn default_max_rounds() -> u32 {
    80
}

/// A pass/fail command evaluated after the agent runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectCommand {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl EvaluationCase {
    /// Load a case from a YAML file.
    pub fn load(path: &Path) -> Result<Self, EvalError> {
        let raw = std::fs::read_to_string(path).map_err(|e| EvalError::Io(e.to_string()))?;
        let case: Self = serde_yaml::from_str(&raw).map_err(|e| EvalError::Parse(e.to_string()))?;
        if case.max_rounds == 0 {
            return Err(EvalError::Parse(format!(
                "evaluation case `{}` has max_rounds=0; evals must be bounded",
                case.id
            )));
        }
        Ok(case)
    }

    /// Load all `*.yaml`/`*.yml` cases below a directory, sorted by id.
    pub fn load_dir(dir: &Path) -> Result<Vec<Self>, EvalError> {
        fn collect(dir: &Path, paths: &mut Vec<std::path::PathBuf>) -> Result<(), EvalError> {
            let entries = std::fs::read_dir(dir).map_err(|e| EvalError::Io(e.to_string()))?;
            for entry in entries {
                let path = entry.map_err(|e| EvalError::Io(e.to_string()))?.path();
                if path.is_dir() {
                    collect(&path, paths)?;
                } else if matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("yaml") | Some("yml")
                ) {
                    paths.push(path);
                }
            }
            Ok(())
        }

        let mut paths = Vec::new();
        collect(dir, &mut paths)?;
        paths.sort();
        let mut cases = paths
            .iter()
            .map(|path| Self::load(path))
            .collect::<Result<Vec<_>, _>>()?;
        cases.sort_by(|a, b| a.id.cmp(&b.id));
        if let Some(pair) = cases.windows(2).find(|pair| pair[0].id == pair[1].id) {
            return Err(EvalError::Parse(format!(
                "duplicate evaluation case id `{}`",
                pair[0].id
            )));
        }
        Ok(cases)
    }
}

/// The outcome of one case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseResult {
    pub id: String,
    /// One-based repetition number for variance measurement.
    #[serde(default = "default_repetition")]
    pub repetition: u32,
    /// The agent reported completion (verification gate passed).
    pub completed: bool,
    /// Why execution stopped, independent from functional correctness and the
    /// runtime's completion claim. Absent on legacy baseline files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<TerminationClass>,
    /// The `expect` command succeeded.
    pub expect_passed: bool,
    /// Tool/agent rounds used.
    pub rounds: u32,
    /// End-to-end wall-clock time for this case.
    #[serde(default)]
    pub latency_ms: u64,
    /// Normalized provider usage summed across model requests.
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Provider pricing is optional. `None` means no configured, auditable
    /// price was available; the harness never invents a cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<FailureCategory>,
    /// Who assigned `failure_category` (auto classifier vs manual override).
    /// Absent when the case passed or the category predates attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_source: Option<FailureSource>,
    /// A short note (error/summary).
    pub note: String,
    /// Independent command evidence used to decide `expect_passed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_evidence: Option<VerificationEvidence>,
}

const fn default_repetition() -> u32 {
    1
}

/// The execution boundary that ended a case. This is deliberately orthogonal
/// to `expect_passed`: a budget-limited run may still have produced correct
/// code, while a cleanly completed run may still fail the external check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationClass {
    Completed,
    BudgetLimited,
    UsageLimited,
    Blocked,
    Incomplete,
    InfrastructureFailed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    Understanding,
    Localization,
    Planning,
    Editing,
    Tooling,
    Context,
    Verification,
    Environment,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationEvidence {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub passed: bool,
    /// Exit code when the command started; `None` means it could not be spawned.
    pub exit_code: Option<i32>,
}

impl CaseResult {
    /// A case passes only if the agent completed *and* the expectation holds
    /// (spec §2.3: verification-driven, not self-reported).
    pub fn passed(&self) -> bool {
        self.completed && self.expect_passed
    }
}

/// The report for one model over a case set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalReport {
    pub model: String,
    pub cases: Vec<CaseResult>,
}

impl EvalReport {
    pub fn total(&self) -> usize {
        self.cases.len()
    }

    pub fn passed_count(&self) -> usize {
        self.cases.iter().filter(|c| c.passed()).count()
    }

    /// Task completion rate in `0.0..=1.0` (spec §45.2).
    pub fn completion_rate(&self) -> f32 {
        if self.cases.is_empty() {
            return 0.0;
        }
        self.passed_count() as f32 / self.total() as f32
    }

    /// Average rounds across cases.
    pub fn avg_rounds(&self) -> f32 {
        if self.cases.is_empty() {
            return 0.0;
        }
        self.cases.iter().map(|c| c.rounds as f32).sum::<f32>() / self.total() as f32
    }

    /// Case ids that did not pass (failed completion and/or expectation).
    pub fn failed_case_ids(&self) -> Vec<String> {
        self.cases
            .iter()
            .filter(|c| !c.passed())
            .map(|c| c.id.clone())
            .collect()
    }
}

/// Run conditions captured with every baseline write so later compares are fair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineMeta {
    /// RFC 3339 UTC timestamp when the run finished.
    pub created_at: String,
    /// Optional git commit of the leveler tree that produced the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    /// Path to the cases directory (as given on the CLI).
    pub cases_dir: String,
    /// `"orchestrated"` or `"direct"`.
    pub mode: String,
    /// Number of times each case was run under the same condition.
    #[serde(default = "default_repetition")]
    pub repetitions: u32,
    /// Exact model references evaluated by this artifact.
    #[serde(default)]
    pub model_refs: Vec<String>,
    /// CodeLeveler engine build version that produced the artifact.
    #[serde(default)]
    pub engine_version: String,
    /// Context needed to understand the case mix without reopening the suite.
    #[serde(default)]
    pub context: BaselineContext,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineContext {
    pub case_count: usize,
    pub repository_cases: usize,
    pub synthetic_cases: usize,
}

/// A durable eval artifact written by `leveler eval … --json-out`.
///
/// This is the product's north-star measurement on disk: completion rate,
/// failed case ids, and (for compare) model/effort gap — with enough meta to
/// reproduce the run conditions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BaselineDocument {
    Run {
        meta: BaselineMeta,
        report: EvalReport,
        completion_rate: f32,
        passed: usize,
        total: usize,
        failed_case_ids: Vec<String>,
    },
    Compare {
        meta: BaselineMeta,
        report_a: EvalReport,
        report_b: EvalReport,
        comparison: Comparison,
        failed_a: Vec<String>,
        failed_b: Vec<String>,
    },
}

impl BaselineDocument {
    /// Build a single-model run artifact from a finished report.
    pub fn from_run(meta: BaselineMeta, report: EvalReport) -> Self {
        let passed = report.passed_count();
        let total = report.total();
        let completion_rate = report.completion_rate();
        let failed_case_ids = report.failed_case_ids();
        Self::Run {
            meta,
            report,
            completion_rate,
            passed,
            total,
            failed_case_ids,
        }
    }

    /// Build a two-model compare artifact (reports + gap metrics).
    pub fn from_compare(meta: BaselineMeta, report_a: EvalReport, report_b: EvalReport) -> Self {
        let comparison = Comparison::of(&report_a, &report_b);
        let failed_a = report_a.failed_case_ids();
        let failed_b = report_b.failed_case_ids();
        Self::Compare {
            meta,
            report_a,
            report_b,
            comparison,
            failed_a,
            failed_b,
        }
    }

    /// Write pretty-printed JSON, creating parent directories as needed.
    pub fn write_json(&self, path: &Path) -> Result<(), EvalError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| EvalError::Io(e.to_string()))?;
        }
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| EvalError::Parse(format!("serialize baseline: {e}")))?;
        std::fs::write(path, body).map_err(|e| EvalError::Io(e.to_string()))
    }

    /// Load a baseline previously written by [`Self::write_json`].
    pub fn load_json(path: &Path) -> Result<Self, EvalError> {
        let raw = std::fs::read_to_string(path).map_err(|e| EvalError::Io(e.to_string()))?;
        serde_json::from_str(&raw).map_err(|e| EvalError::Parse(e.to_string()))
    }
}

/// A comparison of two models' reports (spec §45.3).
///
/// `model_gap` is the completion-rate difference. It goes to zero once the case
/// set is easy enough for both models to pass everything, and then it says
/// nothing at all — which is exactly when `effort_gap` matters: reaching the
/// same result in more rounds is what a weaker model actually looks like.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    pub a: String,
    pub b: String,
    pub a_rate: f32,
    pub b_rate: f32,
    pub model_gap: f32,
    /// Cases BOTH models passed. Rounds are only comparable across these.
    pub paired_cases: usize,
    /// `a`'s mean rounds over the paired cases (0.0 when there are none).
    pub a_avg_rounds: f32,
    /// `b`'s mean rounds over the paired cases (0.0 when there are none).
    pub b_avg_rounds: f32,
    /// `b_avg_rounds - a_avg_rounds`. Positive means `a` needed fewer rounds to
    /// reach the same verified result.
    pub effort_gap: f32,
}

impl Comparison {
    pub fn of(a: &EvalReport, b: &EvalReport) -> Self {
        let a_rate = a.completion_rate();
        let b_rate = b.completion_rate();

        // Pair by case id, not position, and only where both models passed: a
        // failed case burns the whole round budget, so its round count measures
        // the cap rather than the effort the task needed.
        let b_passed: std::collections::HashMap<(&str, u32), u32> = b
            .cases
            .iter()
            .filter(|c| c.passed())
            .map(|c| ((c.id.as_str(), c.repetition), c.rounds))
            .collect();

        let mut a_rounds = 0u64;
        let mut b_rounds = 0u64;
        let mut paired_cases = 0usize;
        for case in a.cases.iter().filter(|c| c.passed()) {
            if let Some(&rounds) = b_passed.get(&(case.id.as_str(), case.repetition)) {
                a_rounds += case.rounds as u64;
                b_rounds += rounds as u64;
                paired_cases += 1;
            }
        }

        let (a_avg_rounds, b_avg_rounds) = if paired_cases == 0 {
            (0.0, 0.0)
        } else {
            let n = paired_cases as f32;
            (a_rounds as f32 / n, b_rounds as f32 / n)
        };

        Self {
            a: a.model.clone(),
            b: b.model.clone(),
            a_rate,
            b_rate,
            model_gap: (a_rate - b_rate).abs(),
            paired_cases,
            a_avg_rounds,
            b_avg_rounds,
            effort_gap: b_avg_rounds - a_avg_rounds,
        }
    }
}

/// Single-knob ablation verdict: SAME model, same cases, same binary — the only
/// variable is one execution knob (control = knob as configured, ablated = knob
/// flipped). Run it once per model and read the deltas; no model tier is
/// inferred or assigned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ablation {
    /// The flipped knob, e.g. `require_step_summary`.
    pub knob: String,
    pub control_rate: f32,
    pub ablated_rate: f32,
    /// `ablated_rate - control_rate`. Negative = the knob was saving cases.
    pub rate_delta: f32,
    /// `id#repetition` of cases that pass under control but fail when the knob
    /// is flipped — direct evidence the knob rescues them.
    pub saved_by_knob: Vec<String>,
    /// `id#repetition` of cases that only pass when the knob is flipped —
    /// direct evidence the knob gets in the way.
    pub hurt_by_knob: Vec<String>,
    /// Cases both runs passed; rounds are only comparable across these.
    pub paired_cases: usize,
    pub control_avg_rounds: f32,
    pub ablated_avg_rounds: f32,
    /// `ablated_avg_rounds - control_avg_rounds`. Positive = the knob saves rounds.
    pub rounds_delta: f32,
    /// `id#repetition` of cases dropped from BOTH arms because at least one arm
    /// died of infrastructure (dropped stream, gateway error). These say nothing
    /// about the knob, and counting them as failures would let network flakiness
    /// masquerade as a knob effect.
    pub discarded_cases: Vec<String>,
}

impl Ablation {
    pub fn of(knob: &str, control: &EvalReport, ablated: &EvalReport) -> Self {
        // Infrastructure deaths are not evidence about the knob. Drop such a case
        // from BOTH arms (symmetry: comparing a live arm against a dead one is
        // exactly the bias this guards against) before computing any metric.
        let infra = |c: &CaseResult| c.termination == Some(TerminationClass::InfrastructureFailed);
        let discarded: std::collections::HashSet<(String, u32)> = control
            .cases
            .iter()
            .chain(ablated.cases.iter())
            .filter(|c| infra(c))
            .map(|c| (c.id.clone(), c.repetition))
            .collect();
        let keep = |report: &EvalReport| EvalReport {
            model: report.model.clone(),
            cases: report
                .cases
                .iter()
                .filter(|c| !discarded.contains(&(c.id.clone(), c.repetition)))
                .cloned()
                .collect(),
        };
        let control = keep(control);
        let ablated = keep(ablated);

        // Rates and paired-rounds share Comparison's semantics (a=control,
        // b=ablated); the flip lists are what an ablation adds on top.
        let cmp = Comparison::of(&control, &ablated);

        let ablated_by_key: std::collections::HashMap<(&str, u32), bool> = ablated
            .cases
            .iter()
            .map(|c| ((c.id.as_str(), c.repetition), c.passed()))
            .collect();

        let mut saved_by_knob = Vec::new();
        let mut hurt_by_knob = Vec::new();
        for case in &control.cases {
            let key = (case.id.as_str(), case.repetition);
            let Some(&ablated_passed) = ablated_by_key.get(&key) else {
                continue; // no counterpart run — nothing to attribute
            };
            let tag = format!("{}#{}", case.id, case.repetition);
            match (case.passed(), ablated_passed) {
                (true, false) => saved_by_knob.push(tag),
                (false, true) => hurt_by_knob.push(tag),
                _ => {}
            }
        }

        let mut discarded_cases: Vec<String> = discarded
            .into_iter()
            .map(|(id, repetition)| format!("{id}#{repetition}"))
            .collect();
        discarded_cases.sort();

        Self {
            knob: knob.to_string(),
            control_rate: cmp.a_rate,
            ablated_rate: cmp.b_rate,
            rate_delta: cmp.b_rate - cmp.a_rate,
            saved_by_knob,
            hurt_by_knob,
            paired_cases: cmp.paired_cases,
            control_avg_rounds: cmp.a_avg_rounds,
            ablated_avg_rounds: cmp.b_avg_rounds,
            rounds_delta: cmp.b_avg_rounds - cmp.a_avg_rounds,
            discarded_cases,
        }
    }
}

/// Attribute a case's failure. `expect_passed` is ground truth (an independent
/// command), `completed` is the runtime's own verdict.
///
/// The first rule is the important one: when the expectation passes but the
/// runtime says the task did not complete, the MODEL solved the task and the
/// RUNTIME's verification gate was wrong (its checks could not run, or they are
/// too strict). That is a framework failure — [`FailureCategory::Runtime`] —
/// and must never be booked against the model as an understanding failure.
///
/// Returns `None` when the case passed.
pub fn attribute_failure(
    completed: bool,
    expect_passed: bool,
    signals: &TrajectorySignals,
) -> Option<FailureCategory> {
    if completed && expect_passed {
        return None;
    }
    if expect_passed && !completed {
        return Some(FailureCategory::Runtime);
    }
    Some(classify_failure(signals))
}

/// Who assigned `failure_category`: the automatic trajectory classifier, or a
/// human reviewer overriding it. Reports must label manual overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSource {
    Auto,
    Manual,
}

/// A trajectory summary the harness derives from the agent's event stream,
/// sufficient for first-cause failure attribution (L1 taskset doc §8). All
/// signals are observable facts; the classifier never inspects model text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrajectorySignals {
    /// Sandbox denial, network block, or a workspace/expect command that could
    /// not run at all.
    pub env_failure: bool,
    /// Times the executor's no-progress loop guard blocked a repeated call.
    pub loop_guard_trips: u32,
    /// Longest streak of consecutive tool-argument/validation errors.
    pub arg_error_streak: u32,
    /// Write-tool calls (apply_patch / replace) attempted and failed.
    pub edit_attempts: u32,
    pub edit_failures: u32,
    /// Total tool calls, for read-only-ratio style judgments.
    pub tool_calls: u32,
    /// Whether the agent ever read or edited a case-relevant file (the
    /// overlay's paths are the harness's proxy for "where the defect lives").
    pub touched_relevant_files: bool,
    /// Auto-compactions that fired during the run.
    pub compactions: u32,
    /// The run stopped on a context/budget ceiling rather than finishing.
    pub context_overflow: bool,
    /// Orchestrated path only: node totals for plan-quality judgment.
    pub node_failures: u32,
    pub node_total: u32,
    /// A verification-class command (build/test) ran during the run.
    pub verification_ran: bool,
}

/// First-cause attribution for a failed case, applied in fixed priority order
/// (environment → tooling → editing → localization → context → planning →
/// verification → understanding). Exactly one category per failure; the
/// classifier is only called when the case did NOT pass.
pub fn classify_failure(s: &TrajectorySignals) -> FailureCategory {
    if s.env_failure {
        return FailureCategory::Environment;
    }
    if s.loop_guard_trips >= 1 || s.arg_error_streak >= 3 {
        return FailureCategory::Tooling;
    }
    if s.edit_attempts > 0 && s.edit_failures * 2 > s.edit_attempts {
        return FailureCategory::Editing;
    }
    if s.tool_calls > 0 && s.edit_attempts == 0 && !s.touched_relevant_files {
        return FailureCategory::Localization;
    }
    if s.context_overflow || s.compactions >= 2 {
        return FailureCategory::Context;
    }
    if s.node_total > 0 && s.node_failures * 2 > s.node_total {
        return FailureCategory::Planning;
    }
    if s.edit_attempts > 0 && !s.verification_ran {
        return FailureCategory::Verification;
    }
    FailureCategory::Understanding
}

impl EvalReport {
    /// Failure counts per category over non-passing cases, ordered by the
    /// category's declaration order. Uncategorized failures count under `none`.
    pub fn failure_breakdown(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for case in self.cases.iter().filter(|c| !c.passed()) {
            let key = match case.failure_category {
                Some(category) => serde_json::to_value(category)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "none".to_string()),
                None => "none".to_string(),
            };
            *counts.entry(key).or_insert(0) += 1;
        }
        counts.into_iter().collect()
    }

    /// Case ids whose repetitions disagree on passing — the flaky set that a
    /// completion-rate average silently hides.
    pub fn unstable_case_ids(&self) -> Vec<String> {
        let mut by_id: std::collections::BTreeMap<&str, (bool, bool)> =
            std::collections::BTreeMap::new();
        for case in &self.cases {
            let entry = by_id.entry(case.id.as_str()).or_insert((false, false));
            if case.passed() {
                entry.0 = true;
            } else {
                entry.1 = true;
            }
        }
        by_id
            .into_iter()
            .filter(|(_, (passed, failed))| *passed && *failed)
            .map(|(id, _)| id.to_string())
            .collect()
    }
}

/// Eval harness errors.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("io error: {0}")]
    Io(String),
    #[error("parse error: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, completed: bool, expect: bool) -> CaseResult {
        CaseResult {
            id: id.into(),
            repetition: 1,
            completed,
            termination: None,
            expect_passed: expect,
            rounds: 3,
            latency_ms: 0,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd_micros: None,
            failure_category: None,
            failure_source: None,
            note: String::new(),
            verification_evidence: None,
        }
    }

    #[test]
    fn ablation_excludes_infrastructure_failures_from_both_arms() {
        // A gateway that drops the stream is not evidence about the knob. A case
        // that died of infrastructure in EITHER arm has no comparable pair, so it
        // must leave the rates entirely — otherwise a flaky network shows up as
        // "the knob costs cases" and the verdict is fiction.
        let mut infra = result("c", false, false);
        infra.termination = Some(TerminationClass::InfrastructureFailed);
        let control = EvalReport {
            model: "m".into(),
            cases: vec![result("a", true, true), result("b", true, true), infra],
        };
        let ablated = EvalReport {
            model: "m".into(),
            cases: vec![
                result("a", true, true),
                result("b", true, true),
                result("c", true, true),
            ],
        };
        let ab = Ablation::of("require_step_summary", &control, &ablated);
        assert_eq!(ab.discarded_cases, vec!["c#1".to_string()]);
        assert_eq!(ab.control_rate, 1.0, "2/2 over the comparable cases");
        assert_eq!(ab.ablated_rate, 1.0, "c is dropped from this arm too");
        assert_eq!(ab.rate_delta, 0.0, "an infra death is not a knob effect");
        assert!(
            ab.hurt_by_knob.is_empty(),
            "c must not read as 'the knob broke it'"
        );
    }

    #[test]
    fn ablation_flags_cases_the_knob_saved_and_hurt() {
        // control: a,b pass, c fails. ablated (knob off): a passes, b fails, c passes.
        let control = EvalReport {
            model: "m".into(),
            cases: vec![
                result("a", true, true),
                result("b", true, true),
                result("c", false, false),
            ],
        };
        let ablated = EvalReport {
            model: "m".into(),
            cases: vec![
                result("a", true, true),
                result("b", false, false),
                result("c", true, true),
            ],
        };
        let ab = Ablation::of("require_step_summary", &control, &ablated);
        assert_eq!(ab.knob, "require_step_summary");
        assert_eq!(ab.saved_by_knob, vec!["b#1".to_string()]);
        assert_eq!(ab.hurt_by_knob, vec!["c#1".to_string()]);
        assert!((ab.rate_delta - 0.0).abs() < f32::EPSILON, "2/3 vs 2/3");
    }

    #[test]
    fn ablation_of_identical_reports_is_a_no_op_verdict() {
        let report = EvalReport {
            model: "m".into(),
            cases: vec![result("a", true, true), result("b", false, false)],
        };
        let ab = Ablation::of("require_completion_evidence", &report, &report.clone());
        assert_eq!(ab.rate_delta, 0.0);
        assert_eq!(ab.rounds_delta, 0.0);
        assert!(ab.saved_by_knob.is_empty());
        assert!(ab.hurt_by_knob.is_empty());
    }

    #[test]
    fn ablation_measures_rounds_only_over_cases_both_runs_passed() {
        let control = EvalReport {
            model: "m".into(),
            cases: vec![result("a", true, true), result("b", true, true)],
        };
        let mut ablated_a = result("a", true, true);
        ablated_a.rounds = 6; // knob off: same result, twice the rounds
        let ablated = EvalReport {
            model: "m".into(),
            cases: vec![ablated_a, result("b", false, false)],
        };
        let ab = Ablation::of("require_explicit_plan", &control, &ablated);
        assert_eq!(ab.paired_cases, 1, "b failed when ablated — not comparable");
        assert_eq!(ab.control_avg_rounds, 3.0);
        assert_eq!(ab.ablated_avg_rounds, 6.0);
        assert_eq!(ab.rounds_delta, 3.0, "positive = the knob saves rounds");
    }

    #[test]
    fn case_result_serializes_budget_termination_separately_from_correctness() {
        let mut case = result("budgeted", false, true);
        case.termination = Some(TerminationClass::BudgetLimited);

        let json = serde_json::to_value(&case).unwrap();
        assert_eq!(json["termination"], "budget_limited");
        assert!(case.expect_passed);
        assert!(!case.completed);
        assert!(!case.passed());
    }

    /// The classifier applies first-cause priority: an environment failure
    /// outranks everything; tooling outranks editing; and a run that made no
    /// edits and never touched the relevant files is a localization failure,
    /// not "understanding".
    #[test]
    fn failure_classification_follows_first_cause_priority() {
        let base = TrajectorySignals {
            tool_calls: 10,
            ..Default::default()
        };

        assert_eq!(
            classify_failure(&TrajectorySignals {
                env_failure: true,
                loop_guard_trips: 5,
                ..base
            }),
            FailureCategory::Environment,
            "environment outranks tooling"
        );
        assert_eq!(
            classify_failure(&TrajectorySignals {
                loop_guard_trips: 1,
                edit_attempts: 4,
                edit_failures: 4,
                ..base
            }),
            FailureCategory::Tooling,
            "tooling outranks editing"
        );
        assert_eq!(
            classify_failure(&TrajectorySignals {
                edit_attempts: 4,
                edit_failures: 3,
                ..base
            }),
            FailureCategory::Editing,
            ">50% edit failure rate is an editing failure"
        );
        assert_eq!(
            classify_failure(&TrajectorySignals {
                edit_attempts: 4,
                edit_failures: 2,
                verification_ran: true,
                touched_relevant_files: true,
                ..base
            }),
            FailureCategory::Understanding,
            "half-failed edits are not (yet) an editing failure; verified but wrong lands on understanding"
        );
        assert_eq!(
            classify_failure(&TrajectorySignals { ..base }),
            FailureCategory::Localization,
            "read everything, edited nothing, never found the defect files"
        );
        assert_eq!(
            classify_failure(&TrajectorySignals {
                touched_relevant_files: true,
                compactions: 2,
                ..base
            }),
            FailureCategory::Context,
            "repeated compaction on a failed run points at context"
        );
        assert_eq!(
            classify_failure(&TrajectorySignals {
                touched_relevant_files: true,
                node_total: 4,
                node_failures: 3,
                ..base
            }),
            FailureCategory::Planning,
            "majority node failure is a planning failure"
        );
        assert_eq!(
            classify_failure(&TrajectorySignals {
                edit_attempts: 4,
                edit_failures: 0,
                touched_relevant_files: true,
                verification_ran: false,
                ..base
            }),
            FailureCategory::Verification,
            "edited but never verified"
        );
    }

    /// The P0 smoke run hit exactly this: four TS cases where the model DID fix
    /// the code (the independent `expect` command passed) but our verification
    /// gate failed because its test tool was missing. The trajectory classifier
    /// blamed the model ("understanding"). Ground truth outranks the gate: that
    /// is a framework failure, not a model failure.
    #[test]
    fn a_passing_expectation_with_a_failed_gate_is_a_runtime_failure() {
        let solved_but_gate_failed = TrajectorySignals {
            tool_calls: 20,
            edit_attempts: 3,
            edit_failures: 0,
            touched_relevant_files: true,
            verification_ran: true,
            ..Default::default()
        };

        assert_eq!(
            attribute_failure(false, true, &solved_but_gate_failed),
            Some(FailureCategory::Runtime),
            "expect passed → the model solved it; the gate is what failed"
        );
        assert_eq!(
            attribute_failure(true, true, &solved_but_gate_failed),
            None,
            "a passing case has no failure category"
        );
        // The model genuinely failed: ground truth says the task is not done, so
        // the trajectory classifier decides.
        assert_eq!(
            attribute_failure(false, false, &solved_but_gate_failed),
            Some(FailureCategory::Understanding)
        );
        assert_eq!(
            attribute_failure(
                true,
                false,
                &TrajectorySignals {
                    env_failure: true,
                    ..Default::default()
                }
            ),
            Some(FailureCategory::Environment),
            "completed but expectation failed → the model's own verdict was wrong"
        );
    }

    #[test]
    fn failure_breakdown_counts_only_failures() {
        let mut failed = result("b", true, false);
        failed.failure_category = Some(FailureCategory::Editing);
        let mut failed2 = result("c", false, false);
        failed2.failure_category = Some(FailureCategory::Editing);
        let report = EvalReport {
            model: "m".into(),
            cases: vec![
                result("a", true, true),
                failed,
                failed2,
                result("d", false, false),
            ],
        };

        let breakdown = report.failure_breakdown();
        assert_eq!(
            breakdown,
            vec![("editing".to_string(), 2), ("none".to_string(), 1)]
        );
    }

    /// Three repetitions that disagree are the flaky set a completion-rate
    /// average hides; the report must name them.
    #[test]
    fn unstable_cases_are_those_whose_repetitions_disagree() {
        let mut r1 = result("flaky", true, true);
        r1.repetition = 1;
        let mut r2 = result("flaky", true, false);
        r2.repetition = 2;
        let mut s1 = result("stable", true, true);
        s1.repetition = 1;
        let mut s2 = result("stable", true, true);
        s2.repetition = 2;
        let report = EvalReport {
            model: "m".into(),
            cases: vec![r1, r2, s1, s2],
        };

        assert_eq!(report.unstable_case_ids(), vec!["flaky".to_string()]);
    }

    /// `failure_source` distinguishes the auto-classifier from a human
    /// override, defaults to absent, and round-trips through serde.
    #[test]
    fn failure_source_defaults_absent_and_roundtrips() {
        let mut case = result("a", false, false);
        assert_eq!(case.failure_source, None);
        case.failure_source = Some(FailureSource::Manual);

        let json = serde_json::to_string(&case).unwrap();
        assert!(json.contains("\"failure_source\":\"manual\""));
        let back: CaseResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.failure_source, Some(FailureSource::Manual));

        // Old artifacts without the field still load.
        let legacy = serde_json::to_string(&result("b", true, true)).unwrap();
        let legacy_no_field = legacy.replace(",\"failure_source\":null", "");
        let _: CaseResult = serde_json::from_str(&legacy_no_field).unwrap();
    }

    #[test]
    fn passes_only_when_completed_and_expected() {
        assert!(result("a", true, true).passed());
        assert!(!result("a", true, false).passed());
        assert!(!result("a", false, true).passed());
    }

    #[test]
    fn completion_rate_and_avg() {
        let report = EvalReport {
            model: "m".into(),
            cases: vec![result("a", true, true), result("b", true, false)],
        };
        assert_eq!(report.passed_count(), 1);
        assert!((report.completion_rate() - 0.5).abs() < f32::EPSILON);
        assert!((report.avg_rounds() - 3.0).abs() < f32::EPSILON);
    }

    #[test]
    fn comparison_gap() {
        let a = EvalReport {
            model: "strong".into(),
            cases: vec![result("x", true, true), result("y", true, true)],
        };
        let b = EvalReport {
            model: "weak".into(),
            cases: vec![result("x", true, true), result("y", false, false)],
        };
        let c = Comparison::of(&a, &b);
        assert!((c.model_gap - 0.5).abs() < f32::EPSILON);
    }

    fn result_rounds(id: &str, completed: bool, expect: bool, rounds: u32) -> CaseResult {
        CaseResult {
            id: id.into(),
            repetition: 1,
            completed,
            termination: None,
            expect_passed: expect,
            rounds,
            latency_ms: 0,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd_micros: None,
            failure_category: None,
            failure_source: None,
            note: String::new(),
            verification_evidence: None,
        }
    }

    /// Once both models pass everything, `model_gap` is 0 and says nothing. The
    /// effort gap still separates them: the weaker model needs more rounds to
    /// reach the same result.
    #[test]
    fn effort_gap_separates_models_when_completion_saturates() {
        let a = EvalReport {
            model: "pro".into(),
            cases: vec![
                result_rounds("x", true, true, 5),
                result_rounds("y", true, true, 15),
            ],
        };
        let b = EvalReport {
            model: "flash".into(),
            cases: vec![
                result_rounds("x", true, true, 7),
                result_rounds("y", true, true, 31),
            ],
        };
        let c = Comparison::of(&a, &b);

        assert_eq!(c.model_gap, 0.0, "completion rate is saturated");
        assert_eq!(c.paired_cases, 2);
        assert!((c.a_avg_rounds - 10.0).abs() < f32::EPSILON);
        assert!((c.b_avg_rounds - 19.0).abs() < f32::EPSILON);
        // Positive means `a` reached the same result in fewer rounds.
        assert!((c.effort_gap - 9.0).abs() < f32::EPSILON);
    }

    /// Rounds must be averaged only over cases BOTH models passed. A failed case
    /// burns the whole round budget, so including it would report the cap rather
    /// than the effort actually needed.
    #[test]
    fn effort_gap_ignores_cases_either_model_failed() {
        let a = EvalReport {
            model: "pro".into(),
            cases: vec![
                result_rounds("x", true, true, 5),
                result_rounds("y", true, true, 6),
            ],
        };
        let b = EvalReport {
            model: "flash".into(),
            cases: vec![
                result_rounds("x", true, true, 9),
                // Hit the round cap and failed — must not pollute the average.
                result_rounds("y", false, false, 40),
            ],
        };
        let c = Comparison::of(&a, &b);

        assert_eq!(c.paired_cases, 1, "only `x` is comparable");
        assert!((c.a_avg_rounds - 5.0).abs() < f32::EPSILON);
        assert!((c.b_avg_rounds - 9.0).abs() < f32::EPSILON);
        assert!((c.effort_gap - 4.0).abs() < f32::EPSILON);
    }

    /// Case ids are matched by name, not by position: a report may order or omit
    /// cases differently.
    #[test]
    fn effort_gap_pairs_cases_by_id() {
        let a = EvalReport {
            model: "pro".into(),
            cases: vec![
                result_rounds("y", true, true, 10),
                result_rounds("x", true, true, 2),
            ],
        };
        let b = EvalReport {
            model: "flash".into(),
            cases: vec![
                result_rounds("x", true, true, 4),
                result_rounds("z", true, true, 99),
            ],
        };
        let c = Comparison::of(&a, &b);
        assert_eq!(c.paired_cases, 1, "only `x` exists in both");
        assert!((c.a_avg_rounds - 2.0).abs() < f32::EPSILON);
        assert!((c.b_avg_rounds - 4.0).abs() < f32::EPSILON);
    }

    #[test]
    fn effort_gap_pairs_repeated_runs_by_case_and_repetition() {
        let mut a_first = result_rounds("x", true, true, 2);
        a_first.repetition = 1;
        let mut a_second = result_rounds("x", true, true, 8);
        a_second.repetition = 2;
        let mut b_first = result_rounds("x", true, true, 4);
        b_first.repetition = 1;
        let mut b_second = result_rounds("x", true, true, 12);
        b_second.repetition = 2;
        let comparison = Comparison::of(
            &EvalReport {
                model: "a".into(),
                cases: vec![a_first, a_second],
            },
            &EvalReport {
                model: "b".into(),
                cases: vec![b_second, b_first],
            },
        );

        assert_eq!(comparison.paired_cases, 2);
        assert_eq!(comparison.a_avg_rounds, 5.0);
        assert_eq!(comparison.b_avg_rounds, 8.0);
    }

    /// No overlapping passes at all: report zeros rather than dividing by zero.
    #[test]
    fn effort_gap_is_zero_without_paired_cases() {
        let a = EvalReport {
            model: "pro".into(),
            cases: vec![result_rounds("x", true, true, 5)],
        };
        let b = EvalReport {
            model: "flash".into(),
            cases: vec![result_rounds("x", false, false, 40)],
        };
        let c = Comparison::of(&a, &b);
        assert_eq!(c.paired_cases, 0);
        assert_eq!(c.effort_gap, 0.0);
        assert_eq!(c.a_avg_rounds, 0.0);
        assert_eq!(c.b_avg_rounds, 0.0);
    }

    #[test]
    fn parses_case_yaml() {
        let yaml = r#"
id: rust-mul
name: Add mul
files:
  lib.rs: "pub fn add(){}"
task: "add mul"
max_rounds: 25
expect: { program: cargo, args: [test] }
"#;
        let case: EvaluationCase = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(case.id, "rust-mul");
        assert_eq!(case.expect.program, "cargo");
        assert_eq!(case.files.len(), 1);
        assert_eq!(case.max_rounds, 25);
    }

    fn meta() -> BaselineMeta {
        BaselineMeta {
            created_at: "2026-07-11T00:00:00Z".into(),
            git_sha: Some("deadbeef".into()),
            cases_dir: "evals".into(),
            mode: "orchestrated".into(),
            repetitions: 1,
            model_refs: vec!["deepseek/v4".into()],
            engine_version: "0.1.0".into(),
            context: BaselineContext {
                case_count: 2,
                repository_cases: 1,
                synthetic_cases: 1,
            },
        }
    }

    #[test]
    fn failed_case_ids_lists_only_failures() {
        let report = EvalReport {
            model: "m".into(),
            cases: vec![
                result("ok", true, true),
                result("bad-complete", false, true),
                result("bad-expect", true, false),
            ],
        };
        assert_eq!(
            report.failed_case_ids(),
            vec!["bad-complete".to_string(), "bad-expect".to_string()]
        );
    }

    #[test]
    fn run_baseline_round_trips_through_json() {
        let report = EvalReport {
            model: "deepseek/v4".into(),
            cases: vec![result("x", true, true), result("y", false, false)],
        };
        let doc = BaselineDocument::from_run(meta(), report);
        let dir = std::env::temp_dir().join(format!("leveler-baseline-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("run.json");
        doc.write_json(&path).expect("write");
        let loaded = BaselineDocument::load_json(&path).expect("load");
        assert_eq!(loaded, doc);
        match loaded {
            BaselineDocument::Run {
                meta,
                completion_rate,
                passed,
                total,
                failed_case_ids,
                ..
            } => {
                assert_eq!(meta.model_refs, vec!["deepseek/v4"]);
                assert_eq!(meta.engine_version, "0.1.0");
                assert_eq!(meta.context.case_count, 2);
                assert!((completion_rate - 0.5).abs() < f32::EPSILON);
                assert_eq!(passed, 1);
                assert_eq!(total, 2);
                assert_eq!(failed_case_ids, vec!["y".to_string()]);
            }
            other => panic!("expected Run, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_baselines_omit_retired_policy_refs() {
        let report = EvalReport {
            model: "deepseek/v4".into(),
            cases: vec![result("x", true, true)],
        };
        let encoded = serde_json::to_value(BaselineDocument::from_run(meta(), report)).unwrap();
        assert!(encoded["meta"].get("policy_refs").is_none());
    }

    #[test]
    fn legacy_policy_refs_are_accepted_but_not_reemitted() {
        let report = EvalReport {
            model: "deepseek/v4".into(),
            cases: vec![result("x", true, true)],
        };
        let mut encoded = serde_json::to_value(BaselineDocument::from_run(meta(), report)).unwrap();
        encoded["meta"]["policy_refs"] = serde_json::json!(["strong"]);

        let loaded: BaselineDocument = serde_json::from_value(encoded).unwrap();
        let rewritten = serde_json::to_value(loaded).unwrap();
        assert!(rewritten["meta"].get("policy_refs").is_none());
    }

    #[test]
    fn load_dir_reads_smoke_and_hard_suites() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals");
        let smoke = EvaluationCase::load_dir(&root.join("smoke")).expect("smoke");
        let hard = EvaluationCase::load_dir(&root.join("hard")).expect("hard");
        assert!(
            !smoke.is_empty(),
            "evals/smoke must contain at least one case"
        );
        assert!(
            !hard.is_empty(),
            "evals/hard must contain at least one case"
        );
        // Suites are selectable by path; ids must not collide within a suite.
        let mut smoke_ids: Vec<_> = smoke.iter().map(|c| c.id.as_str()).collect();
        smoke_ids.sort();
        smoke_ids.dedup();
        assert_eq!(smoke_ids.len(), smoke.len());
    }

    #[test]
    fn root_suite_is_recursive_and_covers_all_first_class_languages() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals");
        let cases = EvaluationCase::load_dir(&root).expect("recursive eval suite");
        const PUBLIC_CASE_FLOOR: usize = 29;
        assert!(
            cases.len() >= PUBLIC_CASE_FLOOR,
            "public self-contained suite needs at least {PUBLIC_CASE_FLOOR} cases, got {}",
            cases.len()
        );

        let has_extension = |extension: &str| {
            cases.iter().any(|case| {
                case.files.keys().any(|path| {
                    Path::new(path).extension().and_then(|value| value.to_str()) == Some(extension)
                })
            })
        };
        assert!(has_extension("rs"), "suite must contain Rust cases");
        assert!(has_extension("go"), "suite must contain Go cases");
        assert!(has_extension("ts"), "suite must contain TypeScript cases");
    }

    /// Eval pass requires agent success AND independent expect — matching
    /// `TaskOutcome::is_success` only for Verified plus an external check.
    #[test]
    fn case_result_never_passes_on_completion_alone() {
        assert!(
            !CaseResult {
                id: "x".into(),
                repetition: 1,
                completed: true,
                termination: Some(TerminationClass::Completed),
                expect_passed: false,
                rounds: 3,
                latency_ms: 0,
                input_tokens: 0,
                output_tokens: 0,
                cost_usd_micros: None,
                failure_category: Some(FailureCategory::Verification),
                failure_source: None,
                note: "model said done".into(),
                verification_evidence: None,
            }
            .passed()
        );
        assert!(
            !CaseResult {
                id: "x".into(),
                repetition: 1,
                completed: false,
                termination: Some(TerminationClass::BudgetLimited),
                expect_passed: true,
                rounds: 3,
                latency_ms: 0,
                input_tokens: 0,
                output_tokens: 0,
                cost_usd_micros: None,
                failure_category: Some(FailureCategory::Runtime),
                failure_source: None,
                note: "tests green but agent failed".into(),
                verification_evidence: None,
            }
            .passed()
        );
    }

    #[test]
    fn compare_baseline_embeds_gap_and_failures() {
        let a = EvalReport {
            model: "strong".into(),
            cases: vec![result("x", true, true), result("y", true, true)],
        };
        let b = EvalReport {
            model: "weak".into(),
            cases: vec![result("x", true, true), result("y", false, false)],
        };
        let doc = BaselineDocument::from_compare(meta(), a, b);
        let dir = std::env::temp_dir().join(format!("leveler-baseline-cmp-{}", std::process::id()));
        let path = dir.join("compare.json");
        doc.write_json(&path).expect("write");
        let loaded = BaselineDocument::load_json(&path).expect("load");
        match loaded {
            BaselineDocument::Compare {
                comparison,
                failed_a,
                failed_b,
                ..
            } => {
                assert!((comparison.model_gap - 0.5).abs() < f32::EPSILON);
                assert!(failed_a.is_empty());
                assert_eq!(failed_b, vec!["y".to_string()]);
            }
            other => panic!("expected Compare, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
