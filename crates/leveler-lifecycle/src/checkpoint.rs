//! The durable goal-checkpoint payload: a derived semantic projection.
//!
//! A checkpoint answers one question: "if execution continues from this
//! point, what must the agent know to continue correctly without replaying
//! the entire old context?" It is a PROJECTION of authoritative facts (the
//! event log, the evidence ledger, the goal row) — never a second truth.
//! A checkpoint can summarize truth; it cannot manufacture it.
//!
//! Truth rules are baked into the types rather than left to call sites:
//! [`CheckpointFindings`] defaults to `Unknown`, so a fact that was never
//! projected reads as absent — never as zero.
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
///
/// The text cap matches the fold's own summary budget
/// (`COMPACTION_SUMMARY_BUDGET_TOKENS`, ~2 K tokens at 4 bytes/token). A
/// storage bound tighter than the context bound truncates the handoff the
/// model is about to receive: the old 2 KB cap cut a live briefing before its
/// final section, which is where "what remains to be done" lives.
const MAX_LIST_ITEMS: usize = 20;
const MAX_TEXT_LEN: usize = 8_192;

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

/// Findings recorded at the checkpoint boundary.
///
/// `Unknown` is the default on purpose: "the ledger was unavailable" must
/// never decay into "zero findings".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CheckpointFindings {
    /// The ledger was read; these counts are real (zero included).
    Known {
        total: u32,
        /// Finding ids (`f-{n}`), references into the authoritative ledger.
        /// Bounded; `total` stays authoritative when truncated.
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

/// One structured question inside a multi-question clarification, projected
/// from the request event so a later window sees what was actually asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CheckpointQuestion {
    /// Short tab label; empty when the request had none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub header: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

/// One decision a person actually produced, projected from the durable
/// clarification log.
///
/// Only `answered` and `skipped` outcomes ever appear here: a pending request,
/// a timed-out one, an unattended one, and a cancelled one are the ABSENCE of
/// a decision and must never be carried as if the user had spoken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointDecision {
    /// The durable clarification id (`ClarificationId`), the traceability key.
    pub request_id: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<CheckpointQuestion>,
    /// `answered` or `skipped`.
    pub outcome: String,
    /// The user's verbatim reply. Empty for a skip — which is not a choice.
    #[serde(default)]
    pub answer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub turn_id: String,
    /// When the terminal event was recorded (RFC 3339).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub settled_at: String,
}

/// One runtime-observed command or failure outcome, projected from a
/// `tool_call_finished` event — never from model prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointEvidence {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The tool reported an error (failed verification, denied command, …).
    pub failed: bool,
    /// Bounded preview of the recorded result.
    #[serde(default)]
    pub detail: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub turn_id: String,
}

/// Execution facts the next window needs to avoid repeating finished or
/// failed work. `total` stays authoritative when `recent` is truncated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CheckpointExecution {
    /// How many actionable outcomes the lineage recorded.
    pub total: u32,
    /// The most recent outcomes, bounded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<CheckpointEvidence>,
}

/// How many runtime-observed outcomes a checkpoint carries (the most recent).
pub const MAX_EXECUTION_EVIDENCE: usize = 12;

/// The window a checkpoint closes, projected from the goal's durable window
/// count and the model's own accepted `update_goal` declaration.
///
/// This exists because a Plan alone cannot tell the next window that a window
/// already ended: a step the model left `in_progress` while yielding made the
/// next window believe it was still the first one and repeat finished work.
/// The outcome is a runtime-recorded boundary fact; the summary is the model's
/// recorded prose and stays labelled unverified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointWindow {
    /// 1-based index of the window within the goal's execution history.
    pub index: u32,
    /// The turn whose boundary closed the window.
    pub turn_id: String,
    /// The accepted declaration: `yielded`, `completed`, or `blocked`.
    pub outcome: String,
    /// The model's recorded resolution summary, verbatim but bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
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
        // A legacy abandoned step is history, not current work. Its
        // count must not become the denominator of progress, and it can never
        // be counted as completed either.
        let current_total = plan
            .steps
            .iter()
            .filter(|s| s.status != crate::plan::ABANDONED)
            .count() as u32;
        // Legacy abandoned steps are not "next": resuming points at open
        // work only.
        let next_step = plan
            .open_steps()
            .first()
            .map(|step| bounded_text(&step.step));
        Some(Self {
            total: current_total,
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
    /// Identity of the continuation lineage this projection belongs to.
    ///
    /// A goal may have several unrelated execution lineages over its lifetime.
    /// Resume may consume this checkpoint only when this root exactly matches
    /// the lineage it is restoring. `None` is a legacy checkpoint: readable
    /// for presentation, but not authoritative for lineage-scoped resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage_root_turn_id: Option<String>,
    /// Persisted transcript messages `[0..ordinal)` are represented by this
    /// checkpoint; resume context is the checkpoint plus messages from this
    /// ordinal on. `None` = not captured (no context-continuity claim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_ordinal: Option<u64>,

    // ---- structured facts (deterministic, projected from authority) ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<CheckpointPlan>,
    #[serde(default)]
    pub findings: CheckpointFindings,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<CheckpointChild>,
    /// References into the canonical artifact store, never contents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_refs: Vec<String>,
    #[serde(default)]
    pub workspace: CheckpointWorkspace,
    /// Verbatim user instructions appended after the objective, oldest first.
    /// The structured amendment record is the authority; this is its durable
    /// projection, so a later instruction is visible together with the text it
    /// supersedes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_amendments: Vec<String>,
    /// Verbatim person-submitted messages from earlier windows, oldest first.
    /// Only `TranscriptOrigin::UserInput` rows qualify — harness notices are
    /// never presented as the user speaking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_messages: Vec<String>,
    /// Decisions a person actually made (answered / skipped), oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<CheckpointDecision>,
    /// Runtime-observed execution outcomes; `None` = none observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<CheckpointExecution>,
    /// The window this checkpoint closes, when it was cut at a window
    /// boundary. `None` = mid-window checkpoint or a pre-continuity row —
    /// never "no window ran".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_window: Option<CheckpointWindow>,

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
        // User text is bounded per item but never count-truncated: an
        // instruction or decision is exactly what the next window must not
        // lose, and the count is bounded by real user turns, not by generation.
        self.user_amendments = std::mem::take(&mut self.user_amendments)
            .into_iter()
            .map(|text| bounded_text(&text))
            .collect();
        self.user_messages = std::mem::take(&mut self.user_messages)
            .into_iter()
            .map(|text| bounded_text(&text))
            .collect();
        for decision in &mut self.decisions {
            decision.question = bounded_text(&decision.question);
            decision.answer = bounded_text(&decision.answer);
            decision.options = bounded_list(decision.options.iter().map(|s| bounded_text(s)));
            for question in &mut decision.questions {
                question.question = bounded_text(&question.question);
                question.options = bounded_list(question.options.iter().map(|s| bounded_text(s)));
            }
        }
        if let Some(execution) = &mut self.execution {
            if execution.recent.len() > MAX_EXECUTION_EVIDENCE {
                execution
                    .recent
                    .drain(..execution.recent.len() - MAX_EXECUTION_EVIDENCE);
            }
            for evidence in &mut execution.recent {
                evidence.tool = bounded_text(&evidence.tool);
                evidence.detail = bounded_text(&evidence.detail);
            }
            execution.total = execution.total.max(execution.recent.len() as u32);
        }
        if let Some(window) = &mut self.last_window
            && let Some(summary) = &mut window.summary
        {
            *summary = bounded_text(summary);
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
        if let CheckpointFindings::Known { total, .. } = &self.findings
            && *total > 0
        {
            parts.push(format!("{total} 项发现"));
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
        if !self.user_amendments.is_empty() {
            push_list(
                &mut out,
                "User instructions after the objective (oldest first; later instructions are newer)",
                &self.user_amendments,
            );
        }
        if !self.user_messages.is_empty() {
            push_list(
                &mut out,
                "User messages from earlier windows (verbatim, oldest first)",
                &self.user_messages,
            );
        }
        if !self.decisions.is_empty() {
            out.push_str("\nUser decisions (durable clarification log; oldest first):\n");
            for decision in &self.decisions {
                let mut line = format!("- request {} ({})", decision.request_id, decision.outcome);
                if !decision.turn_id.is_empty() {
                    line.push_str(&format!(", turn {}", decision.turn_id));
                }
                if !decision.settled_at.is_empty() {
                    line.push_str(&format!(", at {}", decision.settled_at));
                }
                line.push_str(&format!(": {}", decision.question));
                out.push_str(&line);
                out.push('\n');
                if !decision.options.is_empty() {
                    out.push_str(&format!("  options: {}\n", decision.options.join(" | ")));
                }
                for question in &decision.questions {
                    let header = if question.header.is_empty() {
                        String::new()
                    } else {
                        format!("[{}] ", question.header)
                    };
                    let options = if question.options.is_empty() {
                        String::new()
                    } else {
                        format!(" (options: {})", question.options.join(" | "))
                    };
                    out.push_str(&format!("  - {header}{}{options}\n", question.question));
                }
                match decision.outcome.as_str() {
                    "skipped" => out.push_str(
                        "  Answer: none — the user skipped this. A skip is NOT an approval and \
                         carries no choice; proceed with your best judgment.\n",
                    ),
                    "answered" => {
                        out.push_str(&format!("  Answer: {}\n", decision.answer));
                    }
                    other => out.push_str(&format!(
                        "  Answer: none — outcome `{other}` is not a decision; no user reply is implied.\n"
                    )),
                }
            }
        }
        if let Some(summary) = &self.goal_summary {
            push_section(&mut out, "Summary", summary);
        }
        if let Some(phase) = &self.phase {
            push_section(&mut out, "Phase", phase);
        }
        if !self.completed_milestones.is_empty() {
            push_list(&mut out, "Completed", &self.completed_milestones);
        }
        if let Some(window) = &self.last_window {
            let mut body = format!(
                "window {} (turn {}) closed with `{}`",
                window.index, window.turn_id, window.outcome
            );
            if let Some(summary) = &window.summary {
                body.push_str(&format!(
                    "; model-reported outcome UNVERIFIED (a claim, not evidence): {summary}"
                ));
            }
            push_section(&mut out, "Previous window", &body);
        }
        if let Some(plan) = &self.plan {
            let mut body = format!("{}/{} steps completed", plan.completed, plan.total);
            if let Some(next) = &plan.next_step {
                body.push_str(&format!("; next step: {next}"));
            }
            push_section(&mut out, "Plan", &body);
        }
        match &self.findings {
            CheckpointFindings::Known { total, refs } => {
                let mut body = format!("{total} total");
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
                (true, Some(c)) => format!("completed, {} findings", c.findings_total),
                (true, None) => "completed, contribution not measured".to_string(),
                (false, _) => "INCOMPLETE — produced no result".to_string(),
            };
            push_section(&mut out, &format!("Child {}", child.nickname), &status);
        }
        if let Some(execution) = &self.execution
            && !execution.recent.is_empty()
        {
            out.push_str(
                "\nRuntime-observed execution (durable tool outcomes, most recent last):\n",
            );
            for evidence in &execution.recent {
                let status = match (evidence.exit_code, evidence.failed) {
                    (Some(code), true) => format!("exit {code}, FAILED"),
                    (Some(code), false) => format!("exit {code}"),
                    (None, true) => "FAILED".to_string(),
                    (None, false) => "observed".to_string(),
                };
                let detail = evidence.detail.trim();
                if detail.is_empty() {
                    out.push_str(&format!("- {} ({status})\n", evidence.tool));
                } else {
                    out.push_str(&format!("- {} ({status}): {detail}\n", evidence.tool));
                }
            }
            let omitted = (execution.total as usize).saturating_sub(execution.recent.len());
            if omitted > 0 {
                out.push_str(&format!(
                    "- … {omitted} earlier outcomes not carried to bound the checkpoint\n"
                ));
            }
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
        out.push_str("\nEvents and messages after this checkpoint are newer than it.\n");
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
    // Keep BOTH ends. A handoff ends with its next steps, so a head-only cut
    // removes the part the resuming model acts on. The marker is explicit:
    // the reader must know content was elided and roughly how much.
    const MARKER_RESERVE: usize = 64;
    let tail_budget = MAX_TEXT_LEN / 3;
    let mut head = MAX_TEXT_LEN - tail_budget - MARKER_RESERVE;
    while !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = s.len() - tail_budget;
    while !s.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[… {} bytes elided to bound the checkpoint …]\n{}",
        &s[..head],
        tail - head,
        &s[tail..]
    )
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
    /// explicit absence — never to zero findings.
    #[test]
    fn absent_facts_decode_as_unknown_not_success() {
        let decoded: GoalCheckpoint =
            serde_json::from_str(r#"{"objective":"do the thing"}"#).unwrap();
        assert_eq!(decoded.findings, CheckpointFindings::Unknown);
        assert_eq!(
            decoded.workspace.dirty, None,
            "unknown git state is not clean"
        );
        assert!(decoded.plan.is_none(), "no plan is absent, not 0/0");
        assert!(
            decoded.last_window.is_none(),
            "a pre-continuity row did not record a window boundary"
        );
    }

    /// The window boundary reaches the next window as a labelled model report,
    /// never as verified evidence of finished work.
    #[test]
    fn previous_window_is_rendered_as_a_model_report() {
        let cp = GoalCheckpoint {
            objective: "x".into(),
            last_window: Some(CheckpointWindow {
                index: 2,
                turn_id: "turn-2".into(),
                outcome: "yielded".into(),
                summary: Some("implemented; window 3 verifies".into()),
            }),
            ..Default::default()
        };
        let block = cp.context_block();
        assert!(block.contains("Previous window"), "got: {block}");
        assert!(
            block.contains("window 2 (turn turn-2) closed with `yielded`"),
            "got: {block}"
        );
        assert!(
            block.contains("UNVERIFIED (a claim, not evidence)"),
            "got: {block}"
        );
        assert!(
            block.contains("implemented; window 3 verifies"),
            "got: {block}"
        );
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
            findings: CheckpointFindings::Known {
                total: 3,
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

    #[test]
    fn abandoned_steps_are_history_not_current_plan_denominator() {
        let plan = PlanState {
            steps: vec![
                step("audit", "completed"),
                step("verify", "pending"),
                step("replaced", crate::plan::ABANDONED),
            ],
        };
        let projected = CheckpointPlan::from_state(&plan).unwrap();
        assert_eq!(projected.total, 2, "the abandoned step is not current work");
        assert_eq!(
            projected.completed, 1,
            "an abandoned step is never completed"
        );
        assert_eq!(projected.next_step.as_deref(), Some("verify"));
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
                refs: vec![],
            },
            ..Default::default()
        };
        let line = cp.fallback_display_summary();
        assert!(line.contains("计划 3/5"), "got: {line}");
        assert!(line.contains("2 项发现"), "got: {line}");
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
        assert!(block.contains("unknown (ledger not read)"), "got: {block}");
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
    fn a_handoff_briefing_keeps_its_actionable_tail() {
        // The compaction prompt puts "what remains to be done" LAST. A 2 KB
        // head-only cap cut a live T8 briefing mid-sentence and dropped
        // exactly that section, so every fold handed the model a briefing
        // with no next step and it re-explored from scratch.
        let mut briefing = String::from("# Handoff Briefing\n\n## Progress\n");
        while briefing.len() < 6_000 {
            briefing.push_str("- read internal/window/window.go; Start rounds by Size/2\n");
        }
        briefing.push_str("\n## Next steps\n1. Change Start to floor division\n");
        let cp = GoalCheckpoint {
            goal_summary: Some(briefing),
            ..Default::default()
        }
        .bounded();
        let block = cp.context_block();
        assert!(
            block.contains("## Next steps") && block.contains("Change Start to floor division"),
            "the actionable tail must survive the checkpoint bound: {block}"
        );
    }

    #[test]
    fn an_oversized_briefing_states_that_content_was_elided() {
        let briefing = "x".repeat(MAX_TEXT_LEN + 500);
        let cp = GoalCheckpoint {
            goal_summary: Some(briefing),
            ..Default::default()
        }
        .bounded();
        let summary = cp.goal_summary.unwrap();
        assert!(summary.len() <= MAX_TEXT_LEN);
        assert!(
            summary.contains("elided"),
            "bounded content must say it was cut, not end in a bare ellipsis"
        );
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

    /// A row written before continuity existed decodes with no decisions, no
    /// amendments, and no execution facts — absence, never invented content.
    #[test]
    fn pre_continuity_rows_decode_as_empty_continuity() {
        let decoded: GoalCheckpoint =
            serde_json::from_str(r#"{"objective":"x","transcript_ordinal":36}"#).unwrap();
        assert!(decoded.decisions.is_empty());
        assert!(decoded.user_amendments.is_empty());
        assert!(decoded.user_messages.is_empty());
        assert!(decoded.execution.is_none());
    }

    /// Decisions and execution facts travel through persistence and render
    /// truthfully: the request id is traceable, a skip is not an approval, and
    /// an exit code is a runtime fact rather than a model claim.
    #[test]
    fn decisions_and_execution_round_trip_into_the_block() {
        let cp = GoalCheckpoint {
            objective: "settle the refund policy".into(),
            user_amendments: vec!["use plan A".into(), "use plan B instead; A is void".into()],
            decisions: vec![
                CheckpointDecision {
                    request_id: "req-1".into(),
                    question: "which policy?".into(),
                    options: vec!["accrual".into(), "cash".into()],
                    questions: vec![CheckpointQuestion {
                        header: "policy".into(),
                        question: "which policy?".into(),
                        options: vec!["accrual".into(), "cash".into()],
                    }],
                    outcome: "answered".into(),
                    answer: "accrual, with an audit log".into(),
                    turn_id: "turn-1".into(),
                    settled_at: "2026-11-11T10:00:00Z".into(),
                },
                CheckpointDecision {
                    request_id: "req-2".into(),
                    question: "currency?".into(),
                    options: Vec::new(),
                    questions: Vec::new(),
                    outcome: "skipped".into(),
                    answer: String::new(),
                    turn_id: "turn-1".into(),
                    settled_at: "2026-11-11T10:01:00Z".into(),
                },
            ],
            execution: Some(CheckpointExecution {
                total: 2,
                recent: vec![
                    CheckpointEvidence {
                        tool: "run_command".into(),
                        exit_code: Some(1),
                        failed: true,
                        detail: "cargo test failed: 1 failing".into(),
                        turn_id: "turn-1".into(),
                    },
                    CheckpointEvidence {
                        tool: "run_command".into(),
                        exit_code: Some(0),
                        failed: false,
                        detail: "cargo test: 42 passed".into(),
                        turn_id: "turn-1".into(),
                    },
                ],
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&cp).unwrap();
        let back: GoalCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cp);

        let block = back.context_block();
        assert!(
            block.contains("req-1"),
            "request id is the traceability key"
        );
        assert!(block.contains("accrual, with an audit log"));
        assert!(block.contains("req-2") && block.contains("NOT an approval"));
        assert!(block.contains("run_command (exit 1, FAILED): cargo test failed"));
        assert!(block.contains("run_command (exit 0): cargo test: 42 passed"));
        assert!(
            block.find("use plan A").unwrap() < block.find("use plan B instead").unwrap(),
            "instructions keep their durable order"
        );
    }

    /// Bounds clamp text, never silently drop decisions; evidence keeps the
    /// most recent outcomes and states how many were not carried.
    #[test]
    fn bounds_clamp_decisions_and_cap_evidence_honestly() {
        let long = "长".repeat(5_000);
        let mut decisions: Vec<CheckpointDecision> = (0..40)
            .map(|i| CheckpointDecision {
                request_id: format!("req-{i}"),
                question: long.clone(),
                options: Vec::new(),
                questions: Vec::new(),
                outcome: "answered".into(),
                answer: long.clone(),
                turn_id: "turn-1".into(),
                settled_at: String::new(),
            })
            .collect();
        decisions[0].answer = "first decision".into();
        let cp = GoalCheckpoint {
            decisions,
            execution: Some(CheckpointExecution {
                total: 40,
                recent: (0..40)
                    .map(|i| CheckpointEvidence {
                        tool: "run_command".into(),
                        exit_code: Some(i),
                        failed: false,
                        detail: format!("outcome {i}"),
                        turn_id: "turn-1".into(),
                    })
                    .collect(),
            }),
            ..Default::default()
        }
        .bounded();

        assert_eq!(
            cp.decisions.len(),
            40,
            "every decision is decision-critical and must not be count-truncated"
        );
        assert!(cp.decisions[0].answer.len() <= MAX_TEXT_LEN);
        assert_eq!(cp.decisions[0].answer, "first decision");
        let execution = cp.execution.clone().unwrap();
        assert_eq!(execution.total, 40, "the total stays authoritative");
        assert_eq!(execution.recent.len(), MAX_EXECUTION_EVIDENCE);
        assert_eq!(
            execution.recent.last().unwrap().exit_code,
            Some(39),
            "the most recent outcomes are the ones kept"
        );
        let block = cp.context_block();
        assert!(block.contains("28 earlier outcomes not carried"));
    }
}
