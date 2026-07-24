//! `leveler-orchestrator` — the agent state machine and task graph (spec §8.5).
//!
//! Turns a raw task into a structured [`Requirement`], a [`TaskGraph`], and
//! executed changes by moving through explicit [`AgentState`]s
//! (Understand → Localize → Plan → Execute → Complete). Node execution reuses
//! [`leveler_agent::Executor`].
#![forbid(unsafe_code)]

pub mod discuss;
mod error;
pub mod graph;
pub mod json;
mod orchestrator;
pub mod planner;
pub mod requirement;
pub mod review;
pub mod roles;
pub mod state;

pub use discuss::{Discussion, DiscussionEvent, DiscussionOutcome, Participant};
pub use error::OrchestratorError;
pub use graph::{
    GraphValidationError, NodeBudgetSignals, NodeStatus, StepBudget, TaskGraph, TaskNode,
    TaskNodeKind,
};
pub use orchestrator::{Orchestrator, OrchestratorEvent};
pub use planner::{
    Planner, ReviewConfig, allowed_paths, compose_node_goal, compose_repair_goal, is_repairable,
};
pub use requirement::{AcceptanceCriterion, Requirement, TaskRisk, TaskType};
pub use review::{ReviewFailure, ReviewFinding, ReviewLens, ReviewReport, Severity};
pub use roles::{AgentBudget, AgentExecutionMode, AgentRole};
pub use state::{AgentState, StateTransition, TransitionReason};
