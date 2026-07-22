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
mod compaction;
mod executor;
mod injected_tools;
mod nudges;
mod prompt;
mod sub_agent;

pub use compaction::{
    COMPACT_KEEP_RECENT, PRE_REQUEST_COMPACT_THRESHOLD, compact_messages, estimate_tokens,
    summarize_with_model,
};
pub use executor::{
    AdvisoryKind, AgentError, AgentEvent, AgentOutcome, AgentVerificationStatus, AutoClarify,
    ClarificationRequest, Clarifier, ContinuationPolicy, Executor, ModelRequestRecord, NoopSink,
    StepLimits, StopReason, SubAgentExecutionPolicies, SubAgentExecutionPolicy, TranscriptSink,
};
pub use leveler_lifecycle::{
    CollaborationMode, CompleteStepReceipt, DepthUseMetrics, EvidenceLedger, GateConfig,
    ObjectiveAnchor, ObjectiveSource, PlanOrigin, PlanState, PlanStep, ProcessEvidence,
    ProgressCaps, ProgressLedger, TaskContract, ToolSurface, TurnPhase, WorkProfile, check,
    check_goal_complete, task_looks_like_implementation,
};
