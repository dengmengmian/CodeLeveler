//! Agent roles and execution modes. The crate implements the
//! parallel-review mode; the other modes are modeled as an interface for later.

use serde::{Deserialize, Serialize};

/// The specialized roles an agent can take (spec §8.6, §42).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Requirement,
    Locator,
    Planner,
    Executor,
    Debugger,
    Reviewer,
}

/// How multiple agents are coordinated (spec §42).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecutionMode {
    /// One executor (default).
    Single,
    /// Several read-only locators explore in parallel.
    ParallelExplore,
    /// Several reviewers examine the diff in parallel.
    ParallelReview,
    /// Competing diagnostic agents.
    CompetitiveSolve,
    /// Independent modules implemented in parallel.
    ParallelImplement,
}

/// A per-agent budget (spec §42).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBudget {
    pub max_tool_rounds: u32,
    pub max_output_tokens: u32,
}

impl Default for AgentBudget {
    fn default() -> Self {
        Self {
            max_tool_rounds: 10,
            max_output_tokens: 2048,
        }
    }
}
