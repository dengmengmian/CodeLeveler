//! `leveler-engine` — the persistent task/turn engine (plan 阶段B).
//!
//! One execution kernel: session lifecycle, turn boundaries, an append-only
//! event log (persist-before-forward), and executor construction, with
//! Direct strategy layered on top. `leveler-agent`'s `Executor` stays the
//! turn-runner; this crate wraps it with persistence.
#![forbid(unsafe_code)]

mod baseline;
mod continuation;
mod engine;
mod event;
mod factory;
mod log;
mod policy_resolver;
mod reaper;
mod recorders;
mod recovery;
mod session_context;
mod turn;

pub use engine::{
    CodingTaskSpec, RuntimeTaskSpec, TaskEngine, TaskReport, TaskSpec, acknowledge_crash_window,
    budget_prior_messages, mode_str,
};
pub use event::{
    DataClass, EngineEvent, ExecutionKind, NodeStatus, PublicAcceptanceStatus, PublicEvent,
    PublicTurnKind, TurnKind,
};
// The engine produces terminal outcomes, but the type is owned by the shared
// lifecycle vocabulary so storage and clients speak it without a back-edge.
pub use continuation::{
    Continuation, DefaultSupervisorPolicy, MAX_EXTENSIONS, NoContinuation, SupervisorPolicy,
    TurnEnded,
};
pub use factory::{ExecutorFactory, TurnProfile, profile_enables_goal_mode};
pub use leveler_lifecycle::{TaskOutcome, TurnOutcome};
pub use log::{EventLog, SnapshotView};
pub use policy_resolver::{
    CompactionPolicy, ContextPolicy, ExecutionOverrides, ExecutionRole, ResolvedExecutionPolicy,
    RetentionPolicy, resolve_execution_policy, resolve_tool_limits,
};
pub use reaper::{ReapConflict, ReapOutcome, reap_after_restart, reap_running_turns_owned};
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
    /// The turn's event channel filled and a canonical event could not enter
    /// it, so the run was cancelled rather than lose durable history.
    ///
    /// Carries what it takes to act on: the bare string this used to be said
    /// nothing about which producer was flooding, which is the first question
    /// anyone asks. A real Multi-Agent turn hit this and the log named neither
    /// the event nor the agent.
    #[error(
        "engine event buffer overloaded: `{event_type}` from {producer} could not enter a full \
         {capacity}-slot channel (turn {turn_id}). The run was cancelled rather than drop a \
         canonical event"
    )]
    EventBufferOverloaded {
        /// The canonical event that could not be queued.
        event_type: String,
        /// Which agent emitted it — `main`, or a child's nickname/id.
        producer: String,
        /// The channel's capacity, so the message says what was exhausted.
        capacity: usize,
        turn_id: String,
    },
    #[error(
        "recovery requires manual confirmation: tool `{tool}` (call `{call_id}`) may have already produced a side effect; inspect the workspace before retrying"
    )]
    RecoveryConfirmationRequired { call_id: String, tool: String },
    /// A fenced write or acquisition found this runtime's token stale. The
    /// run aborts; a stale runtime writes no further canonical facts.
    #[error(transparent)]
    Ownership(#[from] leveler_storage::OwnershipError),
    /// The task is owned by a different runtime. Never auto-steal: stop and
    /// report; the current owner decides the task's future.
    #[error(
        "task {task_id} is owned by runtime {owner} at epoch {epoch}; this runtime ({this_runtime}) must not touch it"
    )]
    OwnershipConflict {
        task_id: leveler_core::TaskId,
        owner: leveler_core::RuntimeId,
        epoch: leveler_core::OwnerEpoch,
        this_runtime: leveler_core::RuntimeId,
    },
    #[error("corrupt or unreplayable history: {0}")]
    Corrupt(String),
}
