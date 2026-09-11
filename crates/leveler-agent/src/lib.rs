//! `leveler-agent` — the agent execution loop.
//!
//! The crate provides an **executor**: given a goal and the tool registry, it
//! drives a model↔tool loop (call model → run requested tools → feed results
//! back → repeat). Top-level turns run until the model resolves the goal;
//! delegated and measured units may carry an explicit round budget.
//!
//! Authority boundary: the loop owns mechanical truth — which tools ran,
//! which files changed, which commands exited how, what the budgets allow.
//! It never judges whether the model's work satisfies the user's request;
//! that reading belongs to the model, and acceptance belongs to the user.
#![forbid(unsafe_code)]

mod authorization;
mod child_profile;
pub mod coding;
pub mod executor;
mod injected_tools;
pub mod named_agent;
mod nudges;
pub mod ownership;
mod prompt;
mod sub_agent;
mod update_plan;

// The kernel owns these: one definition of what a spent budget is, shared by
// the loop that enforces it and the outcome that reports it.
pub use child_profile::child_profile_trace;
pub use executor::DelegatedChildResult;
pub use executor::host::{PriorlyAdmitted, reconcile};
pub use executor::{
    AdvisoryKind, AdvisorySpend, AgentError, AgentEvent, AgentOutcome, ContinuationPolicy,
    Executor, NoopSink, SteeringSource, StepLimits, SubAgentExecutionPolicies,
    SubAgentExecutionPolicy, TurnPolicy, closeout,
};
pub use leveler_agent_core::{BudgetDimension, BudgetExhaustion};
pub use leveler_context::{
    COMPACT_KEEP_RECENT, CompactionSummary, PRE_REQUEST_COMPACT_THRESHOLD, compact_messages,
    estimate_tokens, summarize_with_model,
};
pub use leveler_engine::{
    ChildToolEvent, CompactionCheckpoint, EventBarrier, ExecutionFence, ModelCallKind,
    ModelRequestRecord, PortError, TranscriptSink,
};
pub use leveler_execution::{AutoClarify, ClarificationRequest, Clarifier, ClarifyOutcome};
pub use leveler_lifecycle::{
    CollaborationMode, EvidenceLedger, ObjectiveAnchor, ObjectiveSource, PlanOrigin, PlanState,
    PlanStep, ProgressCaps, ProgressLedger, StopReason, TurnPhase, WorkProfile,
};
pub use sub_agent::{ChildResult, ChildStatus, SettledChildNotice};
pub use sub_agent::{multi_agent_steer_hint, should_inject_delegation_hint};
pub use update_plan::{UpdatePlanTool, register_harness_controls};
