//! Plan, diff, and completion projections (spec §20–§23).
//!
//! These are render-ready views built by the runtime client from the
//! orchestrator's events and git — the UI never inspects the task graph or runs
//! git itself.

use serde::{Deserialize, Serialize};

/// The mechanical work still running after the assistant has produced its
/// final response but before the runtime publishes the task terminal.
///
/// This is lifecycle chrome, not a second completion authority: the terminal
/// event remains the only fact that ends the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FinalizationStage {
    /// Wait for work already admitted by the turn to settle.
    SettlingDependencies,
    /// Run a completion review explicitly required by the task contract.
    Review,
    /// Resolve the task outcome from the collected facts.
    ResolvingOutcome,
    /// Commit and publish the canonical terminal fact.
    PublishingTerminal,
}

/// The lifecycle state of a plan step (mirrors the orchestrator's `NodeStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiPlanStep {
    pub index: usize,
    pub description: String,
    pub status: PlanStepStatus,
}

/// The execution plan (spec §20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiPlan {
    pub steps: Vec<UiPlanStep>,
}

/// One changed file (spec §21).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiDiffFile {
    pub path: String,
    pub added: u32,
    pub removed: u32,
    /// The unified diff hunk text, loaded on demand.
    pub patch: Option<String>,
}

/// A summary of working-tree changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiCompletionReport {
    pub files_changed: usize,
    pub added: u32,
    pub removed: u32,
    /// Whether the run completed successfully.
    pub success: bool,
}
