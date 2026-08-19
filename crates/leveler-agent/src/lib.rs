//! `leveler-agent` — the agent execution loop.
//!
//! The crate provides an **executor**: given a goal and the tool registry, it
//! drives a model↔tool loop (call model → run requested tools → feed results
//! back → repeat). Top-level turns run until a semantic terminal state;
//! delegated and measured units may carry an explicit round budget. The full
//! role-based state machine (Requirement /
//! Locator / Planner / Executor / Debugger / Reviewer) and verification gates
//! are owned by the orchestration and verification layers.
#![forbid(unsafe_code)]

mod authorization;
mod budget;
mod compaction;
pub mod context_budget;
mod executor;
mod injected_tools;
pub mod named_agent;
mod nudges;
pub mod ownership;
mod prompt;
mod sub_agent;

pub use budget::{
    BudgetDimension, BudgetExhaustion, MAX_BUDGET_EXTENSIONS, budget_extension_allowed,
    grant_budget_extension, stop_detail_indicates_no_progress,
};
pub use compaction::{
    COMPACT_KEEP_RECENT, PRE_REQUEST_COMPACT_THRESHOLD, compact_messages, estimate_tokens,
    summarize_with_model,
};
pub use executor::DelegatedChildResult;
pub use executor::host::{PriorlyAdmitted, reconcile};
pub use executor::{
    AdvisoryKind, AgentError, AgentEvent, AgentOutcome, AgentVerificationStatus, AutoClarify,
    ChildToolEvent, ClarificationRequest, Clarifier, ClarifyOutcome, ContinuationPolicy,
    EventBarrier, ExecutionFence, Executor, ModelRequestRecord, NoopSink, SteeringSource,
    StepLimits, StopReason, SubAgentExecutionPolicies, SubAgentExecutionPolicy, TranscriptSink,
    TurnPolicy, closeout,
};
pub use leveler_lifecycle::{
    CollaborationMode, CompleteStepReceipt, DepthUseMetrics, EvidenceLedger, GateConfig,
    ObjectiveAnchor, ObjectiveSource, PlanOrigin, PlanState, PlanStep, ProgressCaps,
    ProgressLedger, TaskContract, TurnPhase, WorkProfile, check, task_looks_like_implementation,
};
pub use sub_agent::{ChildResult, ChildStatus};
pub use sub_agent::{
    multi_agent_steer_hint, should_inject_delegation_hint, task_suggests_delegation,
};
