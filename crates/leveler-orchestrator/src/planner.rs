//! Observer-free planning primitives (plan B5): understand → localize → plan,
//! the review panel, and the prompt composers. Shared by the legacy
//! [`crate::Orchestrator`] state machine and the engine's plan strategy —
//! callers emit their own progress events around these calls.

use std::sync::Arc;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_context::{ContextCompiler, ContextPackage};
use leveler_model::{ModelRef, ModelRuntime};
use leveler_tools::{ToolContext, ToolRegistry};
use leveler_verifier::VerificationReport;

use crate::error::OrchestratorError;
use crate::graph::{NodeStatus, TaskGraph, TaskNode, TaskNodeKind};
use crate::json::request_json;
use crate::requirement::Requirement;
use crate::review::{
    DEFAULT_LENSES, RawReview, ReviewFailure, ReviewFinding, ReviewLens, ReviewReport, Severity,
    merge_findings, review_user_prompt,
};

const UNDERSTAND_SYSTEM: &str = "\
You are a requirements analyst for a coding agent. Given a software task, output \
a single JSON object describing it, with this shape:\n\
{\"goal\": string, \"task_type\": one of \
[\"bug_fix\",\"feature\",\"refactor\",\"test\",\"docs\",\"other\"], \
\"constraints\": string[], \"acceptance_criteria\": [{\"id\": string, \
\"description\": string, \"verification_hint\": string, \"required\": bool}], \
\"out_of_scope\": string[], \"risk\": one of [\"low\",\"medium\",\"high\"], \
\"uncertainties\": string[]}.\n\
`verification_hint` MUST be a single shell command that exits 0 if and only if \
the criterion is satisfied (e.g. \"test -f src/x.ts\", \"grep -q 'export function foo' src/x.ts\", \
\"npm run typecheck\"); leave it \"\" when the criterion cannot be checked by a command. \
Make acceptance criteria concrete and verifiable. Output ONLY the JSON.";

const PLAN_SYSTEM: &str = "\
You are a planning engine for a coding agent. Given a requirement and a \
repository file map, break the work into an ordered list of task nodes. Output \
a single JSON object:\n\
{\"nodes\": [{\"id\": string, \"kind\": one of \
[\"inspect\",\"design\",\"edit\",\"test\",\"verify\",\"review\"], \
\"description\": string, \"allowed_paths\": string[], \
\"acceptance_criteria\": string[], \"dependencies\": string[]}]}.\n\
Keep the plan minimal: usually 1-3 nodes. Use a single \"edit\" node for small \
changes. `description` must be a precise instruction the executor can follow. \
`allowed_paths` lists the files this node may modify. Output ONLY the JSON.";

/// Parallel-review wiring (spec §44).
pub struct ReviewConfig {
    /// The reviewer lenses to run concurrently.
    pub lenses: Vec<ReviewLens>,
    /// If set, a finding at or above this severity blocks completion (§30).
    pub blocks_on: Option<Severity>,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            lenses: DEFAULT_LENSES.to_vec(),
            // Advisory by default: report findings but don't block completion.
            blocks_on: None,
        }
    }
}

/// What the planner model returns.
#[derive(Debug, Deserialize)]
struct PlanSpec {
    #[serde(default)]
    nodes: Vec<TaskNode>,
}

/// The planning brain: requirement analysis, deterministic localization, task
/// graph planning and the review panel.
pub struct Planner {
    pub runtime: Arc<dyn ModelRuntime>,
    pub registry: Arc<ToolRegistry>,
    pub tool_context: ToolContext,
    pub model: ModelRef,
}

impl Planner {
    /// Turn the raw goal into a structured requirement (spec §23). A weak
    /// model that cannot produce clean JSON gets a usable fallback rather
    /// than aborting the run.
    pub async fn understand(
        &self,
        goal: &str,
        cancellation: &CancellationToken,
    ) -> Result<Requirement, OrchestratorError> {
        let mut requirement: Requirement = match request_json(
            &*self.runtime,
            &self.model,
            UNDERSTAND_SYSTEM,
            goal,
            cancellation,
        )
        .await
        {
            Ok(r) => r,
            Err(OrchestratorError::Json(_)) => Requirement::fallback(goal),
            Err(e) => return Err(e),
        };
        requirement.raw_text = goal.to_string();
        Ok(requirement)
    }

    /// Deterministic localization (spec §26): compile a context package —
    /// repository map, candidate files, related tests, and merged project
    /// rules — via cheap filesystem scans (no model call).
    pub fn localize(&self, goal: &str) -> ContextPackage {
        ContextCompiler::compile(self.tool_context.workspace.root(), goal)
    }

    /// Break the requirement into an ordered task graph (spec §25).
    pub async fn plan(
        &self,
        requirement: &Requirement,
        context: &ContextPackage,
        cancellation: &CancellationToken,
    ) -> Result<TaskGraph, OrchestratorError> {
        let user = format!(
            "Requirement:\n{}\n\n{}",
            serde_json::to_string_pretty(requirement).unwrap_or_default(),
            context.render()
        );
        let spec: PlanSpec = match request_json(
            &*self.runtime,
            &self.model,
            PLAN_SYSTEM,
            &user,
            cancellation,
        )
        .await
        {
            Ok(s) => s,
            Err(OrchestratorError::Json(_)) => PlanSpec { nodes: Vec::new() },
            Err(e) => return Err(e),
        };
        let nodes = if spec.nodes.is_empty() {
            vec![fallback_node(&requirement.goal)]
        } else {
            spec.nodes
        };
        let graph = TaskGraph {
            id: leveler_core::TaskId::generate(),
            goal: requirement.goal.clone(),
            nodes,
        };
        // Reject structurally broken graphs before they reach Execute/Verify.
        graph
            .validate()
            .map_err(|e| OrchestratorError::InvalidPlan(e.to_string()))?;
        Ok(graph)
    }

    /// Run the reviewer lenses concurrently over the working diff and merge
    /// their findings (spec §44). Lens failures are explicit; they are never
    /// collapsed into an empty finding list.
    pub async fn review(
        &self,
        goal: &str,
        config: &ReviewConfig,
        cancellation: &CancellationToken,
    ) -> ReviewReport {
        let diff = self.working_diff(cancellation).await;
        if diff.trim().is_empty() {
            return ReviewReport::default();
        }
        let user = review_user_prompt(goal, &truncate_diff(&diff));
        let futures = config.lenses.iter().map(|lens| {
            let user = user.clone();
            async move {
                match request_json::<RawReview>(
                    &*self.runtime,
                    &self.model,
                    lens.system,
                    &user,
                    cancellation,
                )
                .await
                {
                    Ok(raw) => Ok(raw
                        .findings
                        .into_iter()
                        .map(|rf| ReviewFinding {
                            lens: lens.name.to_string(),
                            severity: Severity::parse(&rf.severity),
                            file: rf.file,
                            issue: rf.issue,
                        })
                        .collect::<Vec<_>>()),
                    Err(error) => Err(ReviewFailure {
                        lens: lens.name.to_string(),
                        error: error.to_string(),
                    }),
                }
            }
        });
        let per_lens = futures::future::join_all(futures).await;
        finish_review(config.lenses.len(), per_lens)
    }

    /// Capture the working-tree diff via the `git_diff` tool.
    async fn working_diff(&self, cancellation: &CancellationToken) -> String {
        match self
            .registry
            .execute(
                "git_diff",
                serde_json::json!({}),
                self.tool_context.clone(),
                cancellation.child_token(),
            )
            .await
        {
            Ok(output) if !output.is_error => {
                let c = output.content;
                if c.trim() == "(clean)" {
                    String::new()
                } else {
                    strip_internal_diff(&c)
                }
            }
            _ => String::new(),
        }
    }
}

fn finish_review(
    lenses_run: usize,
    per_lens: Vec<Result<Vec<ReviewFinding>, ReviewFailure>>,
) -> ReviewReport {
    let mut findings = Vec::new();
    let mut failures = Vec::new();
    for result in per_lens {
        match result {
            Ok(mut lens_findings) => findings.append(&mut lens_findings),
            Err(failure) => failures.push(failure),
        }
    }
    ReviewReport {
        lenses_run,
        findings: merge_findings(findings),
        failures,
    }
}

/// The union of all node `allowed_paths` (empty means no restriction).
pub fn allowed_paths(graph: &TaskGraph) -> Vec<String> {
    // If any node is unrestricted, the whole run is unrestricted.
    if graph.nodes.iter().any(|n| n.allowed_paths.is_empty()) {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for node in &graph.nodes {
        for p in &node.allowed_paths {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
    }
    out
}

/// Whether the failure is the kind a repair agent can fix (compile/test
/// defects, not environment problems or scope violations).
pub fn is_repairable(report: &VerificationReport) -> bool {
    if !report.scope_ok {
        return false;
    }
    report
        .failed_gates()
        .iter()
        .any(|c| c.failure.as_ref().map(|f| f.retryable).unwrap_or(false))
}

/// Build the repair instruction from failing checks and their evidence.
pub fn compose_repair_goal(requirement: &Requirement, report: &VerificationReport) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "The change for goal \"{}\" did not pass verification. Fix the failures \
         below with minimal edits via apply_patch. Do NOT modify or weaken tests \
         to hide failures — fix the actual code.\n\n",
        requirement.goal
    ));
    for check in report.failed_gates() {
        s.push_str(&format!("Check `{}` failed", check.name));
        if let Some(f) = &check.failure {
            s.push_str(&format!(" ({:?})", f.kind));
            if !f.likely_files.is_empty() {
                s.push_str(&format!(" — likely files: {}", f.likely_files.join(", ")));
            }
        }
        s.push_str(":\n");
        let evidence = tail(&check.evidence, 1500);
        s.push_str(&evidence);
        s.push_str("\n\n");
    }
    s
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s.len() - max;
    let mut b = start;
    while !s.is_char_boundary(b) {
        b += 1;
    }
    format!("…{}", &s[b..])
}

/// Drop diff sections for CodeLeveler's own `.leveler/` state (session db, etc.)
/// so reviewers see only the actual code change.
fn strip_internal_diff(diff: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            skipping = line.contains("/.leveler/") || line.contains(" a/.leveler/");
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Bound the diff size sent to reviewers.
fn truncate_diff(diff: &str) -> String {
    const MAX: usize = 12_000;
    if diff.len() <= MAX {
        return diff.to_string();
    }
    let mut end = MAX;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [diff truncated]", &diff[..end])
}

/// Compose the per-node instruction handed to the executor, including the
/// merged project rules so the executor honors AGENTS.md conventions.
pub fn compose_node_goal(
    requirement: &Requirement,
    context: &ContextPackage,
    node: &TaskNode,
) -> String {
    let mut s = String::new();
    if !context.instructions.is_empty() {
        s.push_str("Project rules to follow:\n");
        s.push_str(&leveler_context::render_instructions(&context.instructions));
        s.push('\n');
    }
    if !context.candidate_files.is_empty() {
        s.push_str(&format!(
            "Likely relevant files: {}\n\n",
            context.candidate_files.join(", ")
        ));
    }
    s.push_str(&format!("Overall goal: {}\n\n", requirement.goal));
    s.push_str(&format!(
        "This step ({:?}): {}\n",
        node.kind, node.description
    ));
    if !node.allowed_paths.is_empty() {
        s.push_str(&format!(
            "You may modify only these files: {}\n",
            node.allowed_paths.join(", ")
        ));
    }
    if !requirement.constraints.is_empty() {
        s.push_str(&format!(
            "Constraints: {}\n",
            requirement.constraints.join("; ")
        ));
    }
    if !requirement.acceptance_criteria.is_empty() {
        s.push_str("Acceptance criteria:\n");
        for ac in &requirement.acceptance_criteria {
            s.push_str(&format!("- {} {}\n", ac.id, ac.description));
        }
    }
    s
}

fn fallback_node(goal: &str) -> TaskNode {
    TaskNode {
        id: leveler_core::TaskNodeId::new("n1"),
        kind: TaskNodeKind::Edit,
        description: goal.to_string(),
        dependencies: Vec::new(),
        allowed_paths: Vec::new(),
        expected_outputs: Vec::new(),
        acceptance_criteria: Vec::new(),
        budget: Default::default(),
        status: NodeStatus::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::{finish_review, strip_internal_diff};
    use crate::review::ReviewFailure;

    #[test]
    fn strips_leveler_state_from_diff() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n@@\n+code\n\
                    diff --git a/.leveler/sessions.db b/.leveler/sessions.db\n+binary\n";
        let out = strip_internal_diff(diff);
        assert!(out.contains("src/lib.rs"));
        assert!(out.contains("+code"));
        assert!(!out.contains(".leveler"));
        assert!(!out.contains("+binary"));
    }

    #[test]
    fn reviewer_failure_is_not_reported_as_a_clean_empty_review() {
        let report = finish_review(
            1,
            vec![Err(ReviewFailure {
                lens: "security".into(),
                error: "model unavailable".into(),
            })],
        );
        assert_eq!(report.lenses_run, 1);
        assert!(report.findings.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].lens, "security");
    }
}
