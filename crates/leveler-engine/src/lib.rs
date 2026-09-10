//! `leveler-engine` — the persistent task/turn engine (plan 阶段B).
//!
//! One execution kernel: session lifecycle, turn boundaries, an append-only
//! event log (persist-before-forward), resume and crash recovery.
//!
//! The engine owns lifecycle, not agent intelligence. What runs inside a turn
//! is a harness's business: it arrives as a closure over [`TurnPorts`], and
//! nothing here knows whether that harness is driving a coding agent or
//! anything else.
#![forbid(unsafe_code)]

mod checkpoint;
mod engine;
mod event;
mod log;
pub mod ports;
mod reaper;
mod recorders;
mod session_context;
mod turn;
pub mod window;

pub use engine::{NewSession, TaskEngine, acknowledge_crash_window, budget_prior_messages};
pub use event::{
    DataClass, EngineEvent, ExecutionKind, NodeStatus, PublicAcceptanceStatus, PublicEvent,
    PublicTurnKind, TurnKind,
};
// The engine produces terminal outcomes, but the type is owned by the shared
// lifecycle vocabulary so storage and clients speak it without a back-edge.
pub use checkpoint::{
    ProjectedCheckpoint, SemanticRecap, checkpoint_created_event, create_goal_checkpoint,
    project_goal_checkpoint, resume_prior_from_checkpoint,
};
pub use leveler_lifecycle::{TaskOutcome, TurnOutcome};
pub use log::{DanglingCall, EventLog, SnapshotView};
pub use ports::{
    ChildToolEvent, CompactionCheckpoint, EventBarrier, ExecutionFence, ModelCallKind,
    ModelRequestRecord, PortError, TranscriptSink, WorkspaceFacts,
};
pub use reaper::{ReapConflict, ReapOutcome, reap_after_restart, reap_running_turns_owned};
pub use recorders::{EventEmitter, RecordingApprover, RecordingClarifier};
pub use session_context::{ContextSummarizer, RawTranscript, SessionContext};
pub use turn::{
    SeedRequest, SettledChild, TurnFacts, TurnFailure, TurnPorts, TurnRecordedOutcome, TurnRunner,
    TurnSeeds, TurnSink, last_persisted_ledger, last_persisted_plan, last_persisted_progress,
    storage_model_request,
};

/// Engine-level errors. Persistence and replay failures are hard errors —
/// the engine never silently drops history or runs ungated.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("storage error: {0}")]
    Storage(#[from] leveler_storage::StorageError),
    /// The harness stopped this turn without reporting facts. The engine
    /// records how the turn ended; it does not interpret why.
    ///
    /// `model` carries a provider failure when that is what ended the turn.
    /// Calling a model is not domain knowledge — any harness does it — and
    /// classifying an infrastructure fault from a typed error beats parsing
    /// the sentence back out of `detail`.
    #[error("execution error: {detail}")]
    Execution {
        detail: String,
        model: Option<leveler_model::ModelError>,
    },
    /// The run was cancelled. Kept distinct from [`EngineError::Execution`]
    /// because a cancelled turn is `interrupted`, not `failed`.
    #[error("cancelled")]
    Cancelled,
    /// The run aborted because this runtime no longer owns the task. Kept
    /// distinct from a plain execution failure: a stale runtime writes no
    /// further canonical facts, not even a terminal one.
    #[error("stale runtime ownership: {0}")]
    StaleOwnership(String),
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
