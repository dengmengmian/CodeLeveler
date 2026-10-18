//! The release transaction: one version, one candidate, four stages, persisted.
//!
//! This is the single source of truth for "where is this release". The dogfood
//! lab owns whether a stage's *evidence* is valid; this owns which stages have
//! been decided, what they were decided on, and what may be reused after a
//! crash. Nothing here re-derives a verdict from a log line.

use std::path::{Path, PathBuf};

use crate::error::Fail;
use crate::json::Json;

pub const STATE_VERSION: u32 = 1;

/// Stage numbers are the persisted identity of a stage; renaming one changes
/// what a resumed transaction means, so they are fixed.
pub const STAGE_BUILD: u8 = 1;
pub const STAGE_TESTS: u8 = 2;
pub const STAGE_DOGFOOD: u8 = 3;
pub const STAGE_PUBLISH: u8 = 4;
pub const STAGES: [u8; 4] = [STAGE_BUILD, STAGE_TESTS, STAGE_DOGFOOD, STAGE_PUBLISH];

pub fn stage_name(stage: u8) -> &'static str {
    match stage {
        STAGE_BUILD => "build-static",
        STAGE_TESTS => "automated-tests",
        STAGE_DOGFOOD => "real-dogfood",
        STAGE_PUBLISH => "ci-cd-publish",
        _ => "unknown",
    }
}

pub fn stage_title(stage: u8) -> &'static str {
    match stage {
        STAGE_BUILD => "Stage 1: Build & Static",
        STAGE_TESTS => "Stage 2: Automated Tests",
        STAGE_DOGFOOD => "Stage 3: Real Dogfood",
        STAGE_PUBLISH => "Stage 4: CI/CD & Publish",
        _ => "Stage ?",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    Running,
    Pass,
    Fail,
    Blocked,
    Invalidated,
    Reused,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Pending => "PENDING",
            Status::Running => "RUNNING",
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Blocked => "BLOCKED",
            Status::Invalidated => "INVALIDATED",
            Status::Reused => "REUSED",
        }
    }

    pub fn parse(text: &str) -> Option<Status> {
        match text {
            "PENDING" => Some(Status::Pending),
            "RUNNING" => Some(Status::Running),
            "PASS" => Some(Status::Pass),
            "FAIL" => Some(Status::Fail),
            "BLOCKED" => Some(Status::Blocked),
            "INVALIDATED" => Some(Status::Invalidated),
            "REUSED" => Some(Status::Reused),
            _ => None,
        }
    }

    /// A stage that has produced a reusable verdict for its own inputs.
    pub fn is_decided_pass(self) -> bool {
        matches!(self, Status::Pass | Status::Reused)
    }
}

/// Case-level roll-up of the model-driven stage. Counts only, never a verdict:
/// L3's own gate decides, this records what it decided.
///
/// The fields are the ones the L3 case rows actually carry (`eval/scripts/
/// release_check.py -> collect_cases`): `passed`, `attempts`, `completed`,
/// `expect_passed`, `reused`. Reading a `status` string reported `0 passed` for
/// a 19/19 run, and reading `unstable_case_ids` from the top level found nothing
/// because the layer writes it under `metrics`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cases {
    /// Unique cases the layer judged.
    pub total: u64,
    /// Sum of every case's attempts. Greater than `total` means a retry ran.
    pub attempted: u64,
    /// Cases whose final verdict is a pass.
    pub passed: u64,
    /// Cases that finished but whose own acceptance did not hold.
    pub failed: u64,
    /// Cases that did not finish (the run ended before the task did).
    pub incomplete: u64,
    /// Cases that needed more than one attempt.
    pub retried: u64,
    /// The layer's own flaky list.
    pub unstable: u64,
    /// Cases restored from this candidate's earlier L3 results.
    pub reused: u64,
}

/// The product's own pass rule, from the L3 case row.
///
/// `leveler-eval` defines `CaseResult::passed()` as `completed && expect_passed`,
/// and the lab publishes that as the row's `passed` field. A document that
/// predates the field gets the same derivation from its two ingredients — never
/// `expect_passed` alone, which would report a case that never finished as a
/// pass, and never a missing field as a failure.
pub fn case_passed(case: &Json) -> bool {
    if let Some(passed) = case.at("passed").as_bool() {
        return passed;
    }
    case.at("completed").as_bool().unwrap_or(false)
        && case.at("expect_passed").as_bool().unwrap_or(false)
}

impl Cases {
    pub fn from_json_l3(layer: &Json) -> Option<Cases> {
        let cases = layer.get("cases")?.as_arr()?;
        let mut summary = Cases {
            total: cases.len() as u64,
            ..Cases::default()
        };
        for case in cases {
            if case_passed(case) {
                summary.passed += 1;
            } else if case.at("completed").as_bool().unwrap_or(false) {
                summary.failed += 1;
            } else {
                summary.incomplete += 1;
            }
            let attempts = case.at("attempts").as_f64().unwrap_or(1.0).max(0.0) as u64;
            summary.attempted += attempts;
            if attempts > 1 {
                summary.retried += 1;
            }
            if case.at("reused").as_bool().unwrap_or(false) {
                summary.reused += 1;
            }
        }
        summary.unstable = layer
            .at("metrics")
            .at("unstable_case_ids")
            .as_arr()
            .map(|ids| ids.len() as u64)
            .unwrap_or(0);
        Some(summary)
    }

    pub fn to_json(&self) -> Json {
        Json::obj(vec![
            ("total", Json::num(self.total as f64)),
            ("attempted", Json::num(self.attempted as f64)),
            ("passed", Json::num(self.passed as f64)),
            ("failed", Json::num(self.failed as f64)),
            ("incomplete", Json::num(self.incomplete as f64)),
            ("retried", Json::num(self.retried as f64)),
            ("unstable", Json::num(self.unstable as f64)),
            ("reused", Json::num(self.reused as f64)),
        ])
    }

    pub fn from_json(value: &Json) -> Cases {
        let count = |key: &str| value.at(key).as_f64().unwrap_or(0.0).max(0.0) as u64;
        Cases {
            total: count("total"),
            attempted: count("attempted"),
            passed: count("passed"),
            failed: count("failed"),
            incomplete: count("incomplete"),
            retried: count("retried"),
            unstable: count("unstable"),
            reused: count("reused"),
        }
    }

    /// One line an operator can read without knowing the schema.
    pub fn summary_line(&self) -> String {
        format!(
            "{} case(s), {} attempt(s), {} passed, {} failed, {} incomplete, \
             {} retried, {} unstable, {} reused",
            self.total,
            self.attempted,
            self.passed,
            self.failed,
            self.incomplete,
            self.retried,
            self.unstable,
            self.reused
        )
    }
}

#[derive(Clone, Debug)]
pub struct StageRecord {
    pub stage: u8,
    pub status: Status,
    /// What this stage's verdict was computed from. A changed fingerprint
    /// invalidates the record even when the SHA is unchanged.
    pub input_fingerprint: Option<String>,
    /// Digest of the evidence that backed the verdict.
    pub evidence_hash: Option<String>,
    pub started_at: Option<f64>,
    pub finished_at: Option<f64>,
    pub seconds: f64,
    pub failure_reason: Option<String>,
    pub retry_count: u64,
    pub reused: bool,
    pub detail: Vec<String>,
    pub artifacts: Vec<(String, String)>,
    pub cases: Option<Cases>,
    pub run_id: Option<String>,
}

impl StageRecord {
    pub fn pending(stage: u8) -> StageRecord {
        StageRecord {
            stage,
            status: Status::Pending,
            input_fingerprint: None,
            evidence_hash: None,
            started_at: None,
            finished_at: None,
            seconds: 0.0,
            failure_reason: None,
            retry_count: 0,
            reused: false,
            detail: Vec::new(),
            artifacts: Vec::new(),
            cases: None,
            run_id: None,
        }
    }

    pub fn to_json(&self) -> Json {
        let mut pairs = vec![
            ("stage", Json::num(self.stage as f64)),
            ("name", Json::str(stage_name(self.stage))),
            ("status", Json::str(self.status.label())),
            ("seconds", Json::num(round1(self.seconds))),
            ("retry_count", Json::num(self.retry_count as f64)),
            ("reused", Json::Bool(self.reused)),
            (
                "input_fingerprint",
                optional_str(self.input_fingerprint.as_deref()),
            ),
            ("evidence_hash", optional_str(self.evidence_hash.as_deref())),
            (
                "started_at",
                optional_str(self.started_at.map(crate::clock::iso8601).as_deref()),
            ),
            (
                "finished_at",
                optional_str(self.finished_at.map(crate::clock::iso8601).as_deref()),
            ),
            (
                "failure_reason",
                optional_str(self.failure_reason.as_deref()),
            ),
            (
                "detail",
                Json::Arr(self.detail.iter().map(Json::str).collect()),
            ),
            (
                "artifacts",
                Json::Obj(
                    self.artifacts
                        .iter()
                        .map(|(k, v)| (k.clone(), Json::str(v.clone())))
                        .collect(),
                ),
            ),
            ("run_id", optional_str(self.run_id.as_deref())),
        ];
        if let Some(cases) = &self.cases {
            pairs.push(("cases", cases.to_json()));
        }
        Json::obj(pairs)
    }

    pub fn from_json(value: &Json) -> StageRecord {
        let stage = value.at("stage").as_f64().unwrap_or(0.0) as u8;
        StageRecord {
            stage,
            status: Status::parse(value.at("status").as_str().unwrap_or("PENDING"))
                .unwrap_or(Status::Pending),
            input_fingerprint: owned(value.at("input_fingerprint").as_str()),
            evidence_hash: owned(value.at("evidence_hash").as_str()),
            started_at: value
                .at("started_at")
                .as_str()
                .and_then(crate::clock::parse_iso8601),
            finished_at: value
                .at("finished_at")
                .as_str()
                .and_then(crate::clock::parse_iso8601),
            seconds: value.at("seconds").as_f64().unwrap_or(0.0),
            failure_reason: owned(value.at("failure_reason").as_str()),
            retry_count: value.at("retry_count").as_f64().unwrap_or(0.0) as u64,
            reused: value.at("reused").as_bool().unwrap_or(false),
            detail: value
                .at("detail")
                .as_arr()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            artifacts: value
                .at("artifacts")
                .as_obj()
                .map(|pairs| {
                    pairs
                        .iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
            cases: match value.get("cases") {
                Some(cases) if !cases.is_null() => Some(Cases::from_json(cases)),
                _ => None,
            },
            run_id: owned(value.at("run_id").as_str()),
        }
    }
}

/// One attempt at a stage. Kept so the report can show what was re-run after a
/// failure instead of only the final state.
#[derive(Clone, Debug)]
pub struct Attempt {
    pub stage: u8,
    pub status: Status,
    pub candidate_sha: String,
    pub at: f64,
    pub reason: Option<String>,
}

impl Attempt {
    pub fn to_json(&self) -> Json {
        Json::obj(vec![
            ("stage", Json::num(self.stage as f64)),
            ("status", Json::str(self.status.label())),
            ("candidate_sha", Json::str(self.candidate_sha.clone())),
            ("at", Json::str(crate::clock::iso8601(self.at))),
            ("reason", optional_str(self.reason.as_deref())),
        ])
    }

    pub fn from_json(value: &Json) -> Attempt {
        Attempt {
            stage: value.at("stage").as_f64().unwrap_or(0.0) as u8,
            status: Status::parse(value.at("status").as_str().unwrap_or("PENDING"))
                .unwrap_or(Status::Pending),
            candidate_sha: value
                .at("candidate_sha")
                .as_str()
                .unwrap_or_default()
                .to_string(),
            at: value
                .at("at")
                .as_str()
                .and_then(crate::clock::parse_iso8601)
                .unwrap_or(0.0),
            reason: owned(value.at("reason").as_str()),
        }
    }
}

/// What the remote has already accepted. Every field is a fact read back from
/// GitHub, never an intention: a resumed publish may only repeat work whose
/// remote effect is not already visible.
#[derive(Clone, Debug, Default)]
pub struct RemoteFacts {
    pub pushed_sha: Option<String>,
    pub workflow_run_id: Option<String>,
    pub workflow_run_status: Option<String>,
    pub workflow_run_conclusion: Option<String>,
    pub ci_conclusion: Option<String>,
    pub tag: Option<String>,
    pub tag_sha: Option<String>,
    pub release_state: Option<String>,
    pub release_url: Option<String>,
    pub assets: Vec<String>,
    pub verified_assets: Vec<String>,
}

impl RemoteFacts {
    pub fn to_json(&self) -> Json {
        Json::obj(vec![
            ("pushed_sha", optional_str(self.pushed_sha.as_deref())),
            (
                "workflow_run_id",
                optional_str(self.workflow_run_id.as_deref()),
            ),
            (
                "workflow_run_status",
                optional_str(self.workflow_run_status.as_deref()),
            ),
            (
                "workflow_run_conclusion",
                optional_str(self.workflow_run_conclusion.as_deref()),
            ),
            ("ci_conclusion", optional_str(self.ci_conclusion.as_deref())),
            ("tag", optional_str(self.tag.as_deref())),
            ("tag_sha", optional_str(self.tag_sha.as_deref())),
            ("release_state", optional_str(self.release_state.as_deref())),
            ("release_url", optional_str(self.release_url.as_deref())),
            (
                "assets",
                Json::Arr(self.assets.iter().map(Json::str).collect()),
            ),
            (
                "verified_assets",
                Json::Arr(self.verified_assets.iter().map(Json::str).collect()),
            ),
        ])
    }

    pub fn from_json(value: &Json) -> RemoteFacts {
        let strings = |key: &str| -> Vec<String> {
            value
                .at(key)
                .as_arr()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        RemoteFacts {
            pushed_sha: owned(value.at("pushed_sha").as_str()),
            workflow_run_id: owned(value.at("workflow_run_id").as_str()),
            workflow_run_status: owned(value.at("workflow_run_status").as_str()),
            workflow_run_conclusion: owned(value.at("workflow_run_conclusion").as_str()),
            ci_conclusion: owned(value.at("ci_conclusion").as_str()),
            tag: owned(value.at("tag").as_str()),
            tag_sha: owned(value.at("tag_sha").as_str()),
            release_state: owned(value.at("release_state").as_str()),
            release_url: owned(value.at("release_url").as_str()),
            assets: strings("assets"),
            verified_assets: strings("verified_assets"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Transaction {
    pub state_version: u32,
    pub release_id: String,
    pub version: String,
    pub candidate_sha: String,
    pub candidate_short: String,
    pub previous_release: Option<String>,
    pub baseline_sha: Option<String>,
    pub model: Option<String>,
    pub started_at: f64,
    pub finished_at: Option<f64>,
    pub stage: u8,
    pub status: Status,
    pub failure_reason: Option<String>,
    pub retry_count: u64,
    pub stages: Vec<StageRecord>,
    pub attempts: Vec<Attempt>,
    pub remote: RemoteFacts,
    pub dogfood_run_id: Option<String>,
    pub rc_run_id: Option<String>,
    pub version_commit: Option<String>,
    pub resume_count: u64,
    /// Paths that changed between the original candidate and a resumed one.
    /// This is what the report shows as the change-impact analysis.
    pub changed_paths: Vec<String>,
    /// The one-line change note, kept so a resumed run states it once in the
    /// returned summary instead of only on the console.
    pub changed_paths_note: Option<String>,
    /// Every candidate this release id has been qualified against, oldest
    /// first. A resumed release keeps its number; this is the audit trail.
    pub candidate_history: Vec<String>,
}

impl Transaction {
    pub fn new(
        release_id: impl Into<String>,
        version: impl Into<String>,
        candidate_sha: impl Into<String>,
        started_at: f64,
    ) -> Transaction {
        let candidate_sha = candidate_sha.into();
        Transaction {
            state_version: STATE_VERSION,
            release_id: release_id.into(),
            version: version.into(),
            candidate_short: short_sha(&candidate_sha),
            candidate_sha: candidate_sha.clone(),
            previous_release: None,
            baseline_sha: None,
            model: None,
            started_at,
            finished_at: None,
            stage: STAGE_BUILD,
            status: Status::Pending,
            failure_reason: None,
            retry_count: 0,
            stages: STAGES
                .iter()
                .map(|stage| StageRecord::pending(*stage))
                .collect(),
            attempts: Vec::new(),
            remote: RemoteFacts::default(),
            dogfood_run_id: None,
            rc_run_id: None,
            version_commit: None,
            resume_count: 0,
            changed_paths: Vec::new(),
            changed_paths_note: None,
            candidate_history: vec![candidate_sha],
        }
    }

    pub fn duration(&self) -> f64 {
        match self.finished_at {
            Some(finished) => (finished - self.started_at).max(0.0),
            None => (crate::clock::now() - self.started_at).max(0.0),
        }
    }

    /// The stage this transaction is actually at.
    ///
    /// `stage` records where the last attempt stopped, which is not the same
    /// fact once a later attempt succeeded: a run that failed at stage 3 and
    /// then passed it still said "current stage: 3" while the only work left was
    /// stage 4. While something is running, that is the stage; otherwise it is
    /// the first stage still undecided.
    pub fn current_stage(&self) -> u8 {
        if self.status == Status::Running {
            return self.stage;
        }
        for stage in STAGES {
            if !self.stage_record(stage).status.is_decided_pass() {
                return stage;
            }
        }
        STAGE_PUBLISH
    }

    /// The failure the CURRENT status reports, if any.
    ///
    /// `failure_reason` is the reason this transaction is not OK. It is written
    /// when a stage fails and cleared when a later attempt of that stage
    /// succeeds (`clear_current_failure`), so it is never a stale failure read
    /// back from an attempt that has since been superseded. History lives in
    /// `attempts`, which is never rewritten.
    pub fn current_failure(&self) -> Option<&str> {
        match self.status {
            Status::Fail | Status::Blocked => self.failure_reason.as_deref(),
            _ => None,
        }
    }

    /// The last failure in the attempt history, whatever the current status is.
    pub fn last_failure(&self) -> Option<&Attempt> {
        self.attempts
            .iter()
            .rev()
            .find(|attempt| attempt.status == Status::Fail)
    }

    /// A stage succeeded: whatever failed before it is no longer current.
    /// The attempt history keeps the failure; the current-state field does not.
    pub fn clear_current_failure(&mut self) {
        self.failure_reason = None;
    }

    pub fn stage_record(&self, stage: u8) -> &StageRecord {
        self.stages
            .iter()
            .find(|record| record.stage == stage)
            .expect("stage record is created for every stage")
    }

    pub fn stage_record_mut(&mut self, stage: u8) -> &mut StageRecord {
        self.stages
            .iter_mut()
            .find(|record| record.stage == stage)
            .expect("stage record is created for every stage")
    }

    pub fn note_attempt(&mut self, stage: u8, status: Status, reason: Option<String>, at: f64) {
        self.attempts.push(Attempt {
            stage,
            status,
            candidate_sha: self.candidate_sha.clone(),
            at,
            reason,
        });
    }

    pub fn to_json(&self) -> Json {
        Json::obj(vec![
            ("state_version", Json::num(self.state_version as f64)),
            ("release_id", Json::str(self.release_id.clone())),
            ("version", Json::str(self.version.clone())),
            ("candidate_sha", Json::str(self.candidate_sha.clone())),
            ("candidate_short", Json::str(self.candidate_short.clone())),
            (
                "previous_release",
                optional_str(self.previous_release.as_deref()),
            ),
            ("baseline_sha", optional_str(self.baseline_sha.as_deref())),
            ("model", optional_str(self.model.as_deref())),
            (
                "started_at",
                Json::str(crate::clock::iso8601(self.started_at)),
            ),
            (
                "finished_at",
                optional_str(self.finished_at.map(crate::clock::iso8601).as_deref()),
            ),
            ("duration_seconds", Json::num(round1(self.duration()))),
            ("current_stage", Json::num(self.stage as f64)),
            (
                "current_stage_name",
                Json::str(stage_name(self.stage).to_string()),
            ),
            ("status", Json::str(self.status.label())),
            (
                "failure_reason",
                optional_str(self.failure_reason.as_deref()),
            ),
            ("retry_count", Json::num(self.retry_count as f64)),
            ("resume_count", Json::num(self.resume_count as f64)),
            (
                "candidate_history",
                Json::Arr(self.candidate_history.iter().map(Json::str).collect()),
            ),
            (
                "changed_paths",
                Json::Arr(self.changed_paths.iter().map(Json::str).collect()),
            ),
            (
                "changed_paths_note",
                optional_str(self.changed_paths_note.as_deref()),
            ),
            (
                "stages",
                Json::Arr(self.stages.iter().map(StageRecord::to_json).collect()),
            ),
            (
                "attempts",
                Json::Arr(self.attempts.iter().map(Attempt::to_json).collect()),
            ),
            ("remote", self.remote.to_json()),
            (
                "dogfood_run_id",
                optional_str(self.dogfood_run_id.as_deref()),
            ),
            ("rc_run_id", optional_str(self.rc_run_id.as_deref())),
            (
                "version_commit",
                optional_str(self.version_commit.as_deref()),
            ),
        ])
    }

    pub fn from_json(value: &Json) -> Result<Transaction, Fail> {
        let state_version = value.at("state_version").as_f64().unwrap_or(0.0) as u32;
        if state_version != STATE_VERSION {
            return Err(Fail::new(
                "RELEASE_STATE_VERSION",
                format!(
                    "release state is version {state_version}, this ./dev writes {STATE_VERSION}"
                ),
            ));
        }
        let mut stages: Vec<StageRecord> = value
            .at("stages")
            .as_arr()
            .map(|items| items.iter().map(StageRecord::from_json).collect())
            .unwrap_or_default();
        for stage in STAGES {
            if !stages.iter().any(|record| record.stage == stage) {
                stages.push(StageRecord::pending(stage));
            }
        }
        stages.sort_by_key(|record| record.stage);
        let release_id = value
            .at("release_id")
            .as_str()
            .unwrap_or_default()
            .to_string();
        if release_id.is_empty() {
            return Err(Fail::new(
                "RELEASE_STATE_CORRUPT",
                "release state has no release_id; delete the release directory to start over",
            ));
        }
        Ok(Transaction {
            state_version,
            release_id,
            version: value.at("version").as_str().unwrap_or_default().to_string(),
            candidate_sha: value
                .at("candidate_sha")
                .as_str()
                .unwrap_or_default()
                .to_string(),
            candidate_short: value
                .at("candidate_short")
                .as_str()
                .unwrap_or_default()
                .to_string(),
            previous_release: owned(value.at("previous_release").as_str()),
            baseline_sha: owned(value.at("baseline_sha").as_str()),
            model: owned(value.at("model").as_str()),
            started_at: value
                .at("started_at")
                .as_str()
                .and_then(crate::clock::parse_iso8601)
                .unwrap_or(0.0),
            finished_at: value
                .at("finished_at")
                .as_str()
                .and_then(crate::clock::parse_iso8601),
            stage: value.at("current_stage").as_f64().unwrap_or(1.0) as u8,
            status: Status::parse(value.at("status").as_str().unwrap_or("PENDING"))
                .unwrap_or(Status::Pending),
            failure_reason: owned(value.at("failure_reason").as_str()),
            retry_count: value.at("retry_count").as_f64().unwrap_or(0.0) as u64,
            stages,
            attempts: value
                .at("attempts")
                .as_arr()
                .map(|items| items.iter().map(Attempt::from_json).collect())
                .unwrap_or_default(),
            remote: RemoteFacts::from_json(value.at("remote")),
            dogfood_run_id: owned(value.at("dogfood_run_id").as_str()),
            rc_run_id: owned(value.at("rc_run_id").as_str()),
            version_commit: owned(value.at("version_commit").as_str()),
            resume_count: value.at("resume_count").as_f64().unwrap_or(0.0) as u64,
            changed_paths: value
                .at("changed_paths")
                .as_arr()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            changed_paths_note: owned(value.at("changed_paths_note").as_str()),
            candidate_history: value
                .at("candidate_history")
                .as_arr()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// Where a dogfood or RC run lives inside the lab. The transaction stores the
/// bare run id; the path is derived, so one fact has one representation.
pub const RELEASE_RUN_KIND: &str = "eval/release";
pub const RC_RUN_KIND: &str = "eval/rc";

pub fn lab_run_dir(lab: &Path, kind: &str, id: &str) -> PathBuf {
    lab.join(kind).join(id)
}

pub fn bare_run_id(full: &str) -> String {
    full.rsplit(['/', '\\']).next().unwrap_or(full).to_string()
}

pub fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}

fn optional_str(value: Option<&str>) -> Json {
    match value {
        Some(text) => Json::str(text),
        None => Json::Null,
    }
}

fn owned(value: Option<&str>) -> Option<String> {
    value.map(str::to_string)
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// Where a release transaction lives. `.dev/` is already the local,
/// machine-specific home for `./dev` config, and a transaction is exactly that:
/// local working state that must not be committed into history.
pub fn release_root(repo: &Path) -> PathBuf {
    repo.join(".dev/release")
}

pub fn lock_path(repo: &Path) -> PathBuf {
    release_root(repo).join("publish.lock")
}

pub fn current_path(repo: &Path) -> PathBuf {
    release_root(repo).join("CURRENT")
}

pub fn state_dir(repo: &Path, release_id: &str) -> PathBuf {
    release_root(repo).join(release_id)
}

pub fn state_path(repo: &Path, release_id: &str) -> PathBuf {
    state_dir(repo, release_id).join("state.json")
}

pub fn log_dir(repo: &Path, release_id: &str) -> PathBuf {
    state_dir(repo, release_id).join("logs")
}

/// Write a file so a reader never observes a half-written document: the bytes
/// land in a sibling temp file and are renamed over the target.
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), Fail> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            Fail::new(
                "RELEASE_STATE_IO",
                format!("could not create {}: {err}", parent.display()),
            )
        })?;
    }
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temp, contents).map_err(|err| {
        Fail::new(
            "RELEASE_STATE_IO",
            format!("could not write {}: {err}", temp.display()),
        )
    })?;
    std::fs::rename(&temp, path).map_err(|err| {
        let _ = std::fs::remove_file(&temp);
        Fail::new(
            "RELEASE_STATE_IO",
            format!("could not replace {}: {err}", path.display()),
        )
    })
}

pub fn read_state(repo: &Path, release_id: &str) -> Result<Option<Transaction>, Fail> {
    let path = state_path(repo, release_id);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(Fail::new(
                "RELEASE_STATE_IO",
                format!("could not read {}: {err}", path.display()),
            ));
        }
    };
    let value = Json::parse(&text).map_err(|err| {
        Fail::new(
            "RELEASE_STATE_CORRUPT",
            format!(
                "{} is not valid JSON: {err}\nThis is a damaged release transaction. \
                 Move it aside to start a new one; ./dev never guesses at a half-written state.",
                path.display()
            ),
        )
    })?;
    Ok(Some(Transaction::from_json(&value)?))
}

pub fn save_state(repo: &Path, state: &Transaction) -> Result<(), Fail> {
    let path = state_path(repo, &state.release_id);
    write_atomic(&path, &state.to_json().to_pretty())
}

pub fn read_current(repo: &Path) -> Option<String> {
    let text = std::fs::read_to_string(current_path(repo)).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn write_current(repo: &Path, release_id: &str) -> Result<(), Fail> {
    write_atomic(&current_path(repo), &format!("{release_id}\n"))
}

#[cfg(test)]
mod tests {
    use super::bare_run_id;

    #[test]
    fn a_run_id_is_the_last_component_of_either_path_shape() {
        // The RC reference is built from a path's display form. Windows renders
        // separators as `\`; reading only `/` returned the whole absolute path,
        // which then reached `--reuse-from` and made the run unresolvable.
        assert_eq!(
            bare_run_id("eval/rc/20260101-000000-bbbbbbbbbbbb"),
            "20260101-000000-bbbbbbbbbbbb"
        );
        assert_eq!(
            bare_run_id(r"C:\lab\eval\rc\20260101-000000-bbbbbbbbbbbb"),
            "20260101-000000-bbbbbbbbbbbb"
        );
    }
}
