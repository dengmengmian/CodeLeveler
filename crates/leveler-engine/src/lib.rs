//! `leveler-engine` — the persistent task/turn engine (plan 阶段B).
//!
//! One execution kernel: session lifecycle, turn boundaries, an append-only
//! event log (persist-before-forward), and executor construction, with
//! Direct strategy layered on top. `leveler-agent`'s `Executor` stays the
//! turn-runner; this crate wraps it with persistence.
#![forbid(unsafe_code)]

mod baseline;
mod engine;
mod event;
mod factory;
mod log;
mod policy_resolver;
mod reaper;
mod recorders;
mod session_context;
mod turn;

pub use engine::{
    TaskEngine, TaskReport, TaskSpec, acknowledge_crash_window, budget_prior_messages, mode_str,
};
pub use event::{
    DataClass, EngineEvent, ExecutionKind, NodeStatus, PublicAcceptanceStatus, PublicEvent,
    PublicTurnKind, TurnKind,
};
// The engine produces terminal outcomes, but the type is owned by the shared
// lifecycle vocabulary so storage and clients speak it without a back-edge.
pub use factory::{ExecutorFactory, TurnProfile, profile_enables_goal_mode};
pub use leveler_lifecycle::{TaskOutcome, TurnOutcome};
pub use log::{EventLog, SnapshotView};
pub use policy_resolver::{
    ExecutionOverrides, ExecutionRole, ResolvedExecutionPolicy, resolve_execution_policy,
    resolve_tool_limits,
};
pub use reaper::reap_running_turns;
pub use session_context::{RawTranscript, SessionContext};
pub use turn::{TurnInput, TurnRecordedOutcome, TurnRunner};

/// Engine-level errors. Persistence and replay failures are hard errors —
/// the engine never silently drops history or runs ungated.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("storage error: {0}")]
    Storage(#[from] leveler_storage::StorageError),
    #[error("agent error: {0}")]
    Agent(#[from] leveler_agent::AgentError),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("engine event buffer overloaded")]
    EventBufferOverloaded,
    #[error(
        "recovery requires manual confirmation: tool `{tool}` (call `{call_id}`) may have already produced a side effect; inspect the workspace before retrying"
    )]
    RecoveryConfirmationRequired { call_id: String, tool: String },
    #[error("corrupt or unreplayable history: {0}")]
    Corrupt(String),
}
