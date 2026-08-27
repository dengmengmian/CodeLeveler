//! The durable goal-checkpoint payload: a derived semantic projection.
//!
//! A checkpoint answers one question: "if execution continues from this
//! point, what must the agent know to continue correctly without replaying
//! the entire old context?" It is a PROJECTION of authoritative facts (the
//! event log, the evidence ledger, the goal row) — never a second truth.
//! A checkpoint can summarize truth; it cannot manufacture it.
//!
//! Truth rules are baked into the types rather than left to call sites:
//! [`CheckpointVerification`] and [`CheckpointFindings`] default to
//! `Unmeasured` / `Unknown`, so a fact that was never projected reads as
//! absent — never as passed, never as zero. The same discipline as
//! `Verdict::Unverified` and `UiChildContribution.measured`.
//!
//! The payload is persisted as versioned JSON (see
//! [`GOAL_CHECKPOINT_SCHEMA_VERSION`]); every field added later must carry
//! `#[serde(default)]` so old rows keep decoding, and a reader refuses a
//! version newer than it understands rather than guessing.

use serde::{Deserialize, Serialize};

use crate::findings::ChildResultProjection;
use crate::plan::PlanState;

/// Version of the persisted checkpoint payload. Bump on incompatible change;
/// readers refuse anything newer than they understand (fail closed).
pub const GOAL_CHECKPOINT_SCHEMA_VERSION: i64 = 1;

/// Caps that keep a checkpoint compact (spec: bounded summaries plus
/// canonical references, never embedded transcripts or diffs).
const MAX_LIST_ITEMS: usize = 20;
const MAX_TEXT_LEN: usize = 2_000;

/// Why this checkpoint was cut. Deterministic and explicit — there is no
/// "the model felt this was a milestone" trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointReason {
    /// The user asked (`/recap`).
    Manual,
    /// A deterministic phase boundary (a work window ended while the goal
    /// continues, or an explicit internal milestone call).
    Milestone,
    /// Cut immediately before old context is folded away, so continuity
    /// survives the fold.
    ContextCompaction,
    /// Cut at a durable interruption boundary. Structured-only by contract:
    /// interruption handling must never wait on a model call.
    Interrupted,
}

impl CheckpointReason {
    /// The persisted spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckpointReason::Manual => "manual",
            CheckpointReason::Milestone => "milestone",
            CheckpointReason::ContextCompaction => "context_compaction",
            CheckpointReason::Interrupted => "interrupted",
        }
    }

    /// Parse a persisted reason. Unknown values are refused, not defaulted:
    /// a reason we cannot interpret must not silently become `Manual`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "manual" => Some(CheckpointReason::Manual),
            "milestone" => Some(CheckpointReason::Milestone),
            "context_compaction" => Some(CheckpointReason::ContextCompaction),
            "interrupted" => Some(CheckpointReason::Interrupted),
            _ => None,
        }
    }
}

/// Verification state at the checkpoint boundary.
///
/// `Unmeasured` is the default on purpose: "no verification evidence" must
/// never decay into "passed".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CheckpointVerification {
    /// Authoritative evidence says the gating checks passed.
    Passed {
        /// What proved it (command fingerprint / check names). Bounded.
        evidence: String,
    },
    /// Authoritative evidence says verification failed.
    Failed {
        /// What failed. Bounded.
        detail: String,
    },
    /// No verification was measured at this boundary. NOT a pass.
    #[default]
    Unmeasured,
}

/// Findings state at the checkpoint boundary.
///
/// `Unknown` is the default on purpose: "the ledger was unavailable" must
/// never decay into "zero findings".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CheckpointFindings {
    /// The ledger was read; these counts are real (zero included).
    Known {
        total: u32,
        /// Neither rejected nor verified — still owed a judgment or a fix.
        open: u32,
        /// Still blocking a verified closure.
        open_blocking: u32,
        /// Finding ids (`f-{n}`), references into the authoritative ledger.
        /// Bounded; counts above stay authoritative when truncated.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        refs: Vec<String>,
    },
    /// The ledger could not be read. NOT zero findings.
    #[default]
    Unknown,
}

/// One child's durable contribution facts, as recorded — never re-judged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointChild {
    /// The child's id, joining to `FindingRecord::source_child`.
    pub child_id: String,
    /// Display nickname as recorded at settlement.
    pub nickname: String,
    /// Whether the child completed its scoped work. `false` is incomplete,
    /// which is NOT the same as "completed with nothing to flag".
    pub completed: bool,
    /// The recorded contribution projection. `None` = NOT MEASURED — it must
    /// never render as zero findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution: Option<ChildResultProjection>,
}

/// Bounded workspace metadata. Every field is optional because a failed
/// `git` invocation yields `None` — unknown, never assumed clean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CheckpointWorkspace {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// `None` = could not be determined. NOT clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    /// Bounded changed-path summary, never a diff.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_paths: Vec<String>,
}

/// Plan progress projected from the authoritative [`PlanState`] — counts and
/// bounded step texts, not a mirror of the whole plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointPlan {
    pub total: u32,
    pub completed: u32,
    /// Completed step texts, bounded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_steps: Vec<String>,
    /// The first not-yet-completed step, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

impl CheckpointPlan {
    /// Project plan progress from the host's plan mirror. `None` when no
    /// plan was ever recorded — absent, not "0/0 complete".
    pub fn from_state(plan: &PlanState) -> Option<Self> {
        if plan.is_empty() {
            return None;
        }
        let completed: Vec<&str> = plan
            .steps
            .iter()
            .filter(|s| s.status == "completed")
            .map(|s| s.step.as_str())
            .collect();
        let next_step = plan
            .steps
            .iter()
            .find(|s| s.status != "completed")
            .map(|s| bounded_text(&s.step));
        Some(Self {
            total: plan.steps.len() as u32,
            completed: completed.len() as u32,
            completed_steps: bounded_list(completed.into_iter().map(bounded_text)),
            next_step,
        })
    }
}

/// The durable checkpoint payload: structured facts first, semantic wording
/// second. Every semantic field is optional — a provider outage degrades the
/// wording, never the structured checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GoalCheckpoint {
    /// What the user asked, verbatim (from the goal row).
    pub objective: String,
    /// Persisted transcript messages `[0..ordinal)` are represented by this
    /// checkpoint; resume context is the checkpoint plus messages from this
    /// ordinal on. `None` = not captured (no context-continuity claim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_ordinal: Option<u64>,

    // ---- structured facts (deterministic, projected from authority) ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<CheckpointPlan>,
    #[serde(default)]
    pub verification: CheckpointVerification,
    #[serde(default)]
    pub findings: CheckpointFindings,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<CheckpointChild>,
    /// References into the canonical artifact store, never contents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_refs: Vec<String>,
    #[serde(default)]
    pub workspace: CheckpointWorkspace,

    // ---- semantic layer (optional, grounded in the facts above) ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_step: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_milestones: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_limitations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_work: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    /// One-to-two-line presentation summary. `None` → render
    /// [`Self::fallback_display_summary`] from structured facts instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_summary: Option<String>,
}

impl GoalCheckpoint {
    /// Clamp every free-text field and list so a checkpoint stays compact no
    /// matter what the semantic layer produced.
    pub fn bounded(mut self) -> Self {
        self.objective = bounded_text(&self.objective);
        for s in [
            &mut self.goal_summary,
            &mut self.phase,
            &mut self.current_step,
            &mut self.next_action,
            &mut self.display_summary,
        ]
        .into_iter()
        .flatten()
        {
            *s = bounded_text(s);
        }
        for list in [
            &mut self.artifact_refs,
            &mut self.completed_milestones,
            &mut self.known_limitations,
            &mut self.unresolved_work,
        ] {
            let items = std::mem::take(list);
            *list = bounded_list(items.iter().map(|s| bounded_text(s)));
        }
        self
    }

    /// A deterministic display line for when the semantic summary is absent.
    /// A durable structured checkpoint must still present usefully — never
    /// "recap unavailable".
    pub fn fallback_display_summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(plan) = &self.plan {
            parts.push(format!("计划 {}/{}", plan.completed, plan.total));
        }
        if let CheckpointFindings::Known {
            open,
            open_blocking,
            ..
        } = &self.findings
        {
            if *open_blocking > 0 {
                parts.push(format!("{open_blocking} 个阻塞发现未解决"));
            } else if *open > 0 {
                parts.push(format!("{open} 个发现未解决"));
            }
        }
        match &self.verification {
            CheckpointVerification::Passed { .. } => parts.push("验证已通过".to_string()),
            CheckpointVerification::Failed { .. } => parts.push("验证失败".to_string()),
            CheckpointVerification::Unmeasured => {}
        }
        if parts.is_empty() {
            parts.push("进度已记录".to_string());
        }
        parts.join(" · ")
    }

    /// The `[GOAL CHECKPOINT]` block a continuation receives instead of the
    /// replayed old context. One canonical rendering: compaction and resume
    /// must present the same persisted facts the same way.
    pub fn context_block(&self) -> String {
        let mut out = String::from("[GOAL CHECKPOINT]\n");
        push_section(&mut out, "Goal", &self.objective);
        if let Some(summary) = &self.goal_summary {
            push_section(&mut out, "Summary", summary);
        }
        if let Some(phase) = &self.phase {
            push_section(&mut out, "Phase", phase);
        }
        if !self.completed_milestones.is_empty() {
            push_list(&mut out, "Completed", &self.completed_milestones);
        }
        if let Some(plan) = &self.plan {
            let mut body = format!("{}/{} steps completed", plan.completed, plan.total);
            if let Some(next) = &plan.next_step {
                body.push_str(&format!("; next step: {next}"));
            }
            push_section(&mut out, "Plan", &body);
        }
        match &self.verification {
            CheckpointVerification::Passed { evidence } => {
                push_section(&mut out, "Verified", evidence);
            }
            CheckpointVerification::Failed { detail } => {
                push_section(&mut out, "Verification FAILED", detail);
            }
            CheckpointVerification::Unmeasured => {
                push_section(&mut out, "Verification", "not measured at this boundary");
            }
        }
        match &self.findings {
            CheckpointFindings::Known {
                total,
                open,
                open_blocking,
                refs,
            } => {
                let mut body = format!("{total} total, {open} open, {open_blocking} blocking");
                if !refs.is_empty() {
                    body.push_str(&format!(" ({})", refs.join(", ")));
                }
                push_section(&mut out, "Findings", &body);
            }
            CheckpointFindings::Unknown => {
                push_section(&mut out, "Findings", "unknown (ledger not read) — NOT zero");
            }
        }
        for child in &self.children {
            let status = match (&child.completed, &child.contribution) {
                (true, Some(c)) if c.findings_total == 0 => "completed, no findings".to_string(),
                (true, Some(c)) => format!(
                    "completed, {} findings ({} blocking)",
                    c.findings_total, c.findings_open_blocking
                ),
                (true, None) => "completed, contribution not measured".to_string(),
                (false, _) => "INCOMPLETE — produced no accepted result".to_string(),
            };
            push_section(&mut out, &format!("Child {}", child.nickname), &status);
        }
        if !self.known_limitations.is_empty() {
            push_list(&mut out, "Known limitations", &self.known_limitations);
        }
        if !self.unresolved_work.is_empty() {
            push_list(&mut out, "Unresolved", &self.unresolved_work);
        }
        if let Some(ws) = workspace_line(&self.workspace) {
            push_section(&mut out, "Workspace", &ws);
        }
        if let Some(next) = &self.next_action {
            push_section(&mut out, "Next", next);
        }
        out.push_str(
            "\nEvents and messages after this checkpoint are newer than it: \
             when they contradict this summary, they win.\n",
        );
        out
    }
}

fn workspace_line(ws: &CheckpointWorkspace) -> Option<String> {
    if ws.branch.is_none() && ws.head.is_none() && ws.dirty.is_none() {
        return None;
    }
    let branch = ws.branch.as_deref().unwrap_or("unknown-branch");
    let head = ws.head.as_deref().unwrap_or("unknown-head");
    let dirty = match ws.dirty {
        Some(true) => "dirty",
        Some(false) => "clean",
        None => "state unknown",
    };
    Some(format!("{branch} @ {head} ({dirty})"))
}

fn push_section(out: &mut String, title: &str, body: &str) {
    out.push_str(&format!("\n{title}:\n{body}\n"));
}

fn push_list(out: &mut String, title: &str, items: &[String]) {
    out.push_str(&format!("\n{title}:\n"));
    for item in items {
        out.push_str(&format!("- {item}\n"));
    }
}

fn bounded_text(s: &str) -> String {
    if s.len() <= MAX_TEXT_LEN {
        return s.to_string();
    }
    let mut end = MAX_TEXT_LEN;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn bounded_list(items: impl Iterator<Item = String>) -> Vec<String> {
    items.take(MAX_LIST_ITEMS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanStep;

    fn step(text: &str, status: &str) -> PlanStep {
        PlanStep {
            step: text.to_string(),
            status: status.to_string(),
            id: None,
            origin: Default::default(),
        }
    }

    /// The core truth rule: a payload with nothing measured decodes to
    /// explicit absence — never to passed, never to zero findings.
    #[test]
    fn absent_facts_decode_as_unknown_not_success() {
        let decoded: GoalCheckpoint =
            serde_json::from_str(r#"{"objective":"do the thing"}"#).unwrap();
        assert_eq!(decoded.verification, CheckpointVerification::Unmeasured);
        assert_eq!(decoded.findings, CheckpointFindings::Unknown);
        assert_eq!(
            decoded.workspace.dirty, None,
            "unknown git state is not clean"
        );
        assert!(decoded.plan.is_none(), "no plan is absent, not 0/0");
    }

    #[test]
    fn payload_round_trips() {
        let cp = GoalCheckpoint {
            objective: "port the parser".to_string(),
            transcript_ordinal: Some(42),
            plan: Some(CheckpointPlan {
                total: 5,
                completed: 3,
                completed_steps: vec!["a".into(), "b".into(), "c".into()],
                next_step: Some("d".into()),
            }),
            verification: CheckpointVerification::Passed {
                evidence: "cargo test: 120 passed".into(),
            },
            findings: CheckpointFindings::Known {
                total: 3,
                open: 1,
                open_blocking: 1,
                refs: vec!["f-1".into(), "f-2".into(), "f-3".into()],
            },
            next_action: Some("inspect the API boundary".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&cp).unwrap();
        let back: GoalCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cp);
    }

    #[test]
    fn reason_spelling_round_trips_and_refuses_unknown() {
        for reason in [
            CheckpointReason::Manual,
            CheckpointReason::Milestone,
            CheckpointReason::ContextCompaction,
            CheckpointReason::Interrupted,
        ] {
            assert_eq!(CheckpointReason::parse(reason.as_str()), Some(reason));
        }
        assert_eq!(CheckpointReason::parse("automatic"), None);
        assert_eq!(CheckpointReason::parse(""), None);
    }

    #[test]
    fn plan_projection_counts_and_finds_next_step() {
        let plan = PlanState {
            steps: vec![
                step("audit", "completed"),
                step("implement", "in_progress"),
                step("verify", "pending"),
            ],
        };
        let projected = CheckpointPlan::from_state(&plan).unwrap();
        assert_eq!(projected.total, 3);
        assert_eq!(projected.completed, 1);
        assert_eq!(projected.completed_steps, vec!["audit".to_string()]);
        assert_eq!(projected.next_step.as_deref(), Some("implement"));

        assert!(
            CheckpointPlan::from_state(&PlanState::default()).is_none(),
            "an empty plan is absent, not zero-of-zero"
        );
    }

    /// A structured checkpoint without a semantic summary must still present
    /// usefully (spec: never "recap unavailable").
    #[test]
    fn fallback_summary_is_derived_from_structured_facts() {
        let cp = GoalCheckpoint {
            objective: "x".into(),
            plan: Some(CheckpointPlan {
                total: 5,
                completed: 3,
                completed_steps: vec![],
                next_step: None,
            }),
            findings: CheckpointFindings::Known {
                total: 2,
                open: 2,
                open_blocking: 0,
                refs: vec![],
            },
            ..Default::default()
        };
        let line = cp.fallback_display_summary();
        assert!(line.contains("计划 3/5"), "got: {line}");
        assert!(line.contains("2 个发现未解决"), "got: {line}");
        assert!(
            !line.contains("验证"),
            "unmeasured verification must not appear as any verdict: {line}"
        );

        let empty = GoalCheckpoint::default().fallback_display_summary();
        assert!(!empty.is_empty());
    }

    /// The context block must state absence explicitly, not omit-and-imply.
    #[test]
    fn context_block_reports_unknown_truthfully() {
        let cp = GoalCheckpoint {
            objective: "fix the flaky test".into(),
            ..Default::default()
        };
        let block = cp.context_block();
        assert!(block.starts_with("[GOAL CHECKPOINT]"));
        assert!(block.contains("not measured"), "got: {block}");
        assert!(block.contains("NOT zero"), "got: {block}");
        assert!(
            block.contains("newer than it"),
            "the delta-wins rule must ride with the block: {block}"
        );
    }

    /// Incomplete children and completed-no-findings children must never
    /// read the same (D/E/F of the truth matrix).
    #[test]
    fn context_block_keeps_incomplete_distinct_from_clean() {
        let cp = GoalCheckpoint {
            objective: "x".into(),
            children: vec![
                CheckpointChild {
                    child_id: "c1".into(),
                    nickname: "Explorer".into(),
                    completed: true,
                    contribution: Some(ChildResultProjection {
                        child_id: "c1".into(),
                        role: "explorer".into(),
                        ..Default::default()
                    }),
                },
                CheckpointChild {
                    child_id: "c2".into(),
                    nickname: "Reviewer".into(),
                    completed: false,
                    contribution: None,
                },
            ],
            ..Default::default()
        };
        let block = cp.context_block();
        assert!(block.contains("completed, no findings"), "got: {block}");
        assert!(block.contains("INCOMPLETE"), "got: {block}");
    }

    #[test]
    fn bounded_clamps_oversized_content() {
        let huge = "长".repeat(5_000);
        let cp = GoalCheckpoint {
            objective: huge.clone(),
            known_limitations: (0..100).map(|i| format!("limit {i}")).collect(),
            display_summary: Some(huge),
            ..Default::default()
        }
        .bounded();
        assert!(cp.objective.len() <= MAX_TEXT_LEN + '…'.len_utf8());
        assert!(cp.display_summary.unwrap().len() <= MAX_TEXT_LEN + '…'.len_utf8());
        assert_eq!(cp.known_limitations.len(), MAX_LIST_ITEMS);
    }

    /// Old readers must keep decoding rows written with unknown extra fields
    /// (additive evolution), and the schema version constant exists.
    #[test]
    fn unknown_extra_fields_do_not_break_decode() {
        let decoded: GoalCheckpoint =
            serde_json::from_str(r#"{"objective":"x","some_future_field":{"a":1}}"#).unwrap();
        assert_eq!(decoded.objective, "x");
        assert_eq!(GOAL_CHECKPOINT_SCHEMA_VERSION, 1);
    }
}
