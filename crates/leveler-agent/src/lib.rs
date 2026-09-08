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

pub mod admission;
mod authorization;
mod budget;
mod child_profile;
mod compaction;
pub mod executor;
mod injected_tools;
pub mod named_agent;
mod nudges;
pub mod ownership;
mod prompt;
mod sub_agent;
pub mod usage;

pub use budget::{BudgetDimension, BudgetExhaustion};
pub use child_profile::child_profile_trace;
pub use compaction::{
    COMPACT_KEEP_RECENT, CompactionSummary, PRE_REQUEST_COMPACT_THRESHOLD, PRUNE_BATCH_BYTES,
    PRUNE_MARKER, PRUNE_TRIGGER_BYTES, compact_messages, estimate_tokens, prune_tool_results,
    reclaimable_tool_result_bytes, summarize_with_model,
};
pub use executor::DelegatedChildResult;
pub use executor::host::{PriorlyAdmitted, reconcile};
pub use executor::{
    AdvisoryKind, AdvisorySpend, AgentError, AgentEvent, AgentOutcome, AgentVerificationStatus,
    AutoClarify, ChildToolEvent, ClarificationRequest, Clarifier, ClarifyOutcome,
    CompactionCheckpoint, ContinuationPolicy, EventBarrier, ExecutionFence, Executor,
    ModelCallKind, ModelRequestRecord, NoopSink, SteeringSource, StepLimits, StopReason,
    SubAgentExecutionPolicies, SubAgentExecutionPolicy, TranscriptSink, TurnPolicy, closeout,
};
pub use leveler_lifecycle::{
    CollaborationMode, DepthUseMetrics, EvidenceLedger, GateConfig, ObjectiveAnchor,
    ObjectiveSource, PlanOrigin, PlanState, PlanStep, ProgressCaps, ProgressLedger, TurnPhase,
    WorkProfile, check,
};
pub use sub_agent::{ChildResult, ChildStatus, SettledChildNotice};
pub use sub_agent::{multi_agent_steer_hint, should_inject_delegation_hint};
