//! Plan, diff, verification, and completion projections (spec §20–§23).
//!
//! These are render-ready views built by the runtime client from the
//! orchestrator's events and git — the UI never inspects the task graph or runs
//! git itself.

use serde::{Deserialize, Serialize};

/// The lifecycle state of a plan step (mirrors the orchestrator's `NodeStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

/// One step in the execution plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiPlanStep {
    pub index: usize,
    pub description: String,
    pub status: PlanStepStatus,
}

/// The execution plan (spec §20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiPlan {
    pub steps: Vec<UiPlanStep>,
}

/// The state of one verification check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Running,
    Passed,
    Failed,
    Skipped,
}

/// One verification check (spec §22).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiCheck {
    pub name: String,
    pub status: CheckState,
    /// Captured evidence (command output), for failures.
    pub evidence: Option<String>,
}

/// The verification result. `passed` is `None` while still running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiVerification {
    pub checks: Vec<UiCheck>,
    pub passed: Option<bool>,
}

/// One changed file (spec §21).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiDiffFile {
    pub path: String,
    pub added: u32,
    pub removed: u32,
    /// The unified diff hunk text, loaded on demand.
    pub patch: Option<String>,
}

/// A summary of working-tree changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiDiff {
    pub files: Vec<UiDiffFile>,
}

impl UiDiff {
    pub fn total_added(&self) -> u32 {
        self.files.iter().map(|f| f.added).sum()
    }

    pub fn total_removed(&self) -> u32 {
        self.files.iter().map(|f| f.removed).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_are_zero_for_empty_diff() {
        let diff = UiDiff { files: vec![] };
        assert_eq!(diff.total_added(), 0);
        assert_eq!(diff.total_removed(), 0);
    }

    #[test]
    fn totals_sum_across_files() {
        let diff = UiDiff {
            files: vec![
                UiDiffFile {
                    path: "a.rs".to_string(),
                    added: 10,
                    removed: 2,
                    patch: None,
                },
                UiDiffFile {
                    path: "b.rs".to_string(),
                    added: 5,
                    removed: 7,
                    patch: None,
                },
            ],
        };
        assert_eq!(diff.total_added(), 15);
        assert_eq!(diff.total_removed(), 9);
    }
}

/// The final completion report (spec §23).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiCompletionReport {
    pub files_changed: usize,
    pub added: u32,
    pub removed: u32,
    pub checks_passed: usize,
    pub checks_total: usize,
    /// Whether the run completed and verified successfully.
    pub success: bool,
}
