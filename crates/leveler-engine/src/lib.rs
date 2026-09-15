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

mod engine;
mod event;
mod log;
pub mod ports;
mod reaper;
mod recorders;
mod session_context;
mod turn;
pub mod window;

pub use engine::{
    EngineBoot, NewSession, NewSessionAxes, TaskEngine, TaskExecution, TaskTerminal,
    acknowledge_crash_window, budget_prior_messages,
};
pub use event::{
    DataClass, EngineEvent, ExecutionKind, NodeStatus, PublicAcceptanceStatus, PublicEvent,
    PublicTurnKind, TurnKind, VerificationDisposition, VerificationExecution,
    VerificationObservation,
};
// The engine produces terminal outcomes, but the type is owned by the shared
// lifecycle vocabulary so storage and clients speak it without a back-edge.
pub use leveler_lifecycle::{TaskOutcome, TurnOutcome};
pub use log::{DanglingCall, EventLog, FinishedChildFact, SnapshotView};
pub use ports::{
    ChildToolEvent, EventBarrier, ExecutionFence, LostChild, LostChildNote, LostChildVoice,
    ModelCallKind, ModelRequestRecord, PortError, ResumedChild, TranscriptSink,
};
pub use reaper::{
    ReapConflict, ReapOutcome, ReapRefusal, ReapScope, ReapedSession, reap_after_restart,
    reap_running_turns_owned,
};
pub use recorders::{EventEmitter, RecordingApprover, RecordingClarifier};
pub use session_context::{ContextSummarizer, RawTranscript, SessionContext};
pub use turn::{
    MAX_CHILD_RESUMES, TurnFacts, TurnFailure, TurnPorts, TurnRecordedOutcome, TurnRunner,
    TurnSink, TurnStart, storage_model_request,
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
    /// A terminal-dependent activation started, but its durable terminal fact
    /// could not be written. Publishing a task terminal would leave an open
    /// child behind it, so the caller must return the error without closing
    /// the task and let recovery reconcile the boundary.
    #[error("terminal evidence boundary is not closed: {0}")]
    UnclosedTerminalBoundary(String),
    /// The canonical task-terminal transaction did not commit. Callers must
    /// surface a recovery fault, never synthesize a client terminal event.
    #[error("task terminal was not committed: {0}")]
    TerminalCommitFailed(String),
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
        "recovery requires manual confirmation: tool `{tool}` (call `{call_id}`) may have already produced a side effect; reconcile the unknown result before retrying"
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
    /// Another live boot of this runtime owns the task. Never taken over:
    /// that boot decides the task's future.
    #[error("task {task_id} is being executed by another live boot of this runtime")]
    OwnedByLiveBoot { task_id: leveler_core::TaskId },
    /// Whether the task's executor is still alive cannot be established — its
    /// boot could not be probed, or a running turn predates boot records.
    /// Nothing is taken over on a guess.
    #[error("task {task_id} may still be executing elsewhere: its owner's liveness is unknown")]
    OwnershipUnknown { task_id: leveler_core::TaskId },
    #[error("corrupt or unreplayable history: {0}")]
    Corrupt(String),
    /// A delegated child already has a durable terminal and something tried
    /// to write a second one. A child settles once; the write is refused so
    /// the record never holds two contradictory endings.
    #[error("child {child_id} is already settled; a second terminal was refused")]
    DuplicateChildSettlement { child_id: String },
}
