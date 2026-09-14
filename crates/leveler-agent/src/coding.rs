//! The Coding harness.
//!
//! Everything that makes this a *coding* agent is composed here: the executor
//! and its policy, the task's verification and baseline, the crash-recovery
//! replay that runs tools, the closure reviewer, and the mapping from loop
//! events onto the engine's durable vocabulary.
//!
//! `leveler-engine` sits underneath and owns lifecycle alone. The dependency
//! runs one way — this module knows the engine, the engine does not know it.

pub mod baseline;
pub mod checkpoint;
mod child_session;
mod event;
pub mod factory;
pub mod policy;
pub mod recovery;
pub mod run;
pub mod turn;
pub mod workspace;

pub use checkpoint::{
    ProjectedCheckpoint, SemanticRecap, WorkspaceFacts, checkpoint_created_event,
    create_goal_checkpoint, project_goal_checkpoint, resume_prior_from_checkpoint,
};
pub use factory::{ExecutorFactory, TurnProfile, profile_enables_goal_mode};
pub use policy::{
    CHAT_CONTEXT_BUDGET, ExecutionOverrides, ExecutionRole, IndependentReviewPolicy,
    ResolvedExecutionPolicy, resolve_execution_policy, resolve_tool_limits,
};
pub use run::{CodingRuntime, CodingTaskSpec, RuntimeTaskSpec, TaskReport, TaskSpec, mode_str};
pub use turn::TurnInput;
pub use workspace::GitWorkspace;
