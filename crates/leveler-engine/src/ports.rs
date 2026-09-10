//! The lifecycle ports: what the engine offers a harness that runs a turn.
//!
//! The engine owns durability, ownership and the event log; a harness owns
//! the model loop. These traits are the seam between them, and they are
//! declared here — on the engine side — so the harness depends on the engine
//! and never the other way round.
//!
//! Every port fails closed. A barrier that cannot make an event durable, a
//! fence that cannot prove ownership, or a checkpoint that cannot commit all
//! abort the run rather than let a side effect happen unrecorded.

use async_trait::async_trait;

use leveler_execution::RiskLevel;
use leveler_lifecycle::CheckpointWorkspace;
use leveler_model::{FinishReason, Message, TokenUsage};

/// Why a lifecycle port refused. Both variants are terminal for the run: the
/// engine could not make a fact durable, or this runtime no longer owns the
/// task it was writing for.
#[derive(Debug, thiserror::Error)]
pub enum PortError {
    #[error("persistence error: {0}")]
    Persistence(String),
    /// The runtime lost task ownership: a newer OwnerEpoch exists.
    #[error("stale runtime ownership: {0}")]
    StaleOwnership(String),
}

/// The ownership fence: proves the runtime still owns its task before a
/// model-proposed tool may produce an external side effect. Checked AFTER
/// the persistence barriers (ToolCallStarted + approval durable) and BEFORE
/// dispatch. It cannot make side effects exactly-once - it only guarantees a
/// runtime already known stale dispatches nothing new.
#[async_trait::async_trait]
pub trait ExecutionFence: Send + Sync {
    /// Err(reason) = the token is stale; the run must abort.
    async fn ensure_current(&self) -> Result<(), String>;
}

/// Awaitable durability barrier for canonical tool events (side-effect
/// barrier, convergence plan phase 1). `flush` resolves once every canonical
/// event emitted through the observer so far is durable. The loop awaits it
/// after announcing a tool call (before hooks/approval can act) and again
/// after authorization (before dispatch), so a crash can never leave a side
/// effect whose `ToolCallStarted` — or whose approval outcome — was lost.
/// A flush failure aborts the run: the tool is NOT executed, because an
/// unexecuted tool is recoverable but an unrecorded side effect is not.
///
/// Hosts without durable persistence (sub-agents, standalone library use)
/// leave the barrier unset; the loop then proceeds without waiting.
#[async_trait]
pub trait EventBarrier: Send + Sync {
    async fn flush(&self) -> Result<(), PortError>;

    /// Record a canonical tool event from a DELEGATED agent, attributed to it.
    ///
    /// A sub-agent's tool calls used to surface only as transient
    /// `SubAgentActivity`, so a worker child that crashed mid-edit left
    /// nothing the host could reconcile. These are the durable facts instead.
    ///
    /// The implementation MUST enqueue this on the same ordered queue that
    /// [`Self::flush`] drains. Anything else races: the flush marker could
    /// overtake the event it is supposed to be waiting for, and the barrier
    /// would report durability for a call that is not recorded yet.
    ///
    /// Required, deliberately. A default no-op would let a barrier flush
    /// successfully while silently discarding the record — the caller would
    /// then run a delegated side effect believing it was durable, which is
    /// the exact failure this method exists to prevent.
    fn record_child_tool_event(&self, event: ChildToolEvent);
}

/// A delegated agent's tool call, attributed to the child that made it.
#[derive(Debug, Clone, PartialEq)]
pub enum ChildToolEvent {
    Started {
        agent_id: String,
        call_id: String,
        name: String,
        arguments: String,
        /// The tool's declared risk. Supplied by the harness that owns the
        /// registry: the engine records the fact, it does not classify it.
        risk: Option<RiskLevel>,
    },
    Finished {
        agent_id: String,
        call_id: String,
        name: String,
        is_error: bool,
        preview: String,
    },
    /// A write-ownership transition (`claim_write_scope` granted or denied).
    ///
    /// Rides this queue, not the activity channel, because the grant must be
    /// durable BEFORE any event whose authorization depends on it — a child's
    /// writes flush here immediately, so a grant recorded anywhere else can
    /// land after the write it authorized and read as a bypass.
    ///
    /// Deliberately NOT a `Started`/`Finished` pair: `claim_write_scope` is a
    /// virtual tool the drive loop answers inline and never registers, so a
    /// `ToolCallStarted` for it would look like a dangling call to crash
    /// recovery and demand human reconciliation for an operation with no
    /// external side effect — the registry it mutates is in memory and dies
    /// with the process.
    Ownership {
        agent_id: String,
        action: String,
        detail: String,
    },
}

/// Host port: cut a durable goal checkpoint at the context-compaction
/// boundary (long-goal P3), so the fold's summary is backed by persisted
/// truth instead of an ephemeral paragraph.
///
/// Contract: the implementation must make every event emitted so far durable
/// BEFORE capturing its cursor (flush-then-read), persist the checkpoint,
/// and return its rendered context block — which the loop folds with in
/// place of the bare summary.
///
/// - `Ok(Some(block))`: a durable checkpoint exists; fold with `block`.
/// - `Ok(None)`: no goal is in scope (plain chat); fold proceeds exactly as
///   without the port.
/// - `Err(_)`: the durable boundary could not be established. The loop must
///   NOT fold this round — old context is kept and the fold retries at the
///   next boundary (fail closed: continuity is never dropped uncheckpointed).
#[async_trait]
pub trait CompactionCheckpoint: Send + Sync {
    /// `summary` is the semantic compaction summary when one was produced.
    async fn checkpoint_before_compaction(
        &self,
        summary: Option<&str>,
    ) -> Result<Option<String>, PortError>;
}

/// A sink that persists the transcript as the loop advances, enabling resume.
/// Called with the messages appended in each step (seed, then per round).
#[async_trait]
pub trait TranscriptSink: Send {
    async fn append(&mut self, messages: &[Message]) -> Result<(), PortError>;

    async fn record_model_request(
        &mut self,
        _record: &ModelRequestRecord,
    ) -> Result<(), PortError> {
        Ok(())
    }
}

/// Diagnostic facts for a completed provider request. Persisting the normalized
/// finish reason makes truncation distinguishable from semantic completion.
#[derive(Debug, Clone)]
pub struct ModelRequestRecord {
    /// The provider's request id, when it reported one. A diagnostic, not a
    /// key: the engine generates the row's identity, because a repeated
    /// provider id used to abort the turn on a UNIQUE violation.
    pub provider_request_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub usage: TokenUsage,
    pub finish_reason: FinishReason,
    pub latency_ms: u64,
    /// Retries *before* this outcome. One record is one LOGICAL call, so the
    /// physical traffic behind it is `1 + retry_count`.
    pub retry_count: u32,
    /// Which lane this call belongs to. A fold's summarization is a provider
    /// call like any other; recording it under its own lane is what lets a
    /// session's cost be attributed to the work versus the overhead.
    pub kind: ModelCallKind,
    /// The sub-agent that made this call, or `None` for the parent's own. A
    /// child runs as an owned `'static` future and cannot borrow the parent's
    /// sink, so its records travel back over the progress channel carrying
    /// this; without it a reviewer's spend has nowhere to land.
    pub agent_id: Option<String>,
    /// Estimated cost in micro-USD, priced where the model has pricing
    /// configured. `None` means unpriced, never free.
    pub cost_usd_micros: Option<u64>,
}

impl ModelRequestRecord {
    /// Fill in the estimated cost from the model's pricing, if any.
    ///
    /// Cost is priced once, here, against the usage the provider actually
    /// reported — including how much of the prompt it served from cache. A row
    /// that carries its own cost can be summed later without re-deriving it
    /// from a price table that may since have changed.
    pub fn priced(mut self, pricing: Option<&leveler_model::ModelPricing>) -> Self {
        self.cost_usd_micros = pricing.map(|p| {
            p.cost_usd_micros_cached(
                self.usage.input_tokens,
                self.usage.cached_input_tokens,
                self.usage.output_tokens,
            )
        });
        self
    }
}

/// Which lane a model call belongs to. The drive loop's rounds are the work;
/// a fold's summarization is overhead the runtime chose to spend, and cost
/// attribution has to be able to tell them apart. Mapped onto the storage
/// enum at the engine boundary — this crate does not depend on storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCallKind {
    /// A main-loop round: the model deciding what to do next.
    Round,
    /// The summarization behind a compaction fold.
    Compaction,
    /// A bounded harness-initiated call that is not a main-loop round: a
    /// compaction summary, or a closeout nudge's extra round. Never a second
    /// model judging the first — that class of call no longer exists.
    Advisory,
}

/// Bounded facts about the workspace a checkpoint is cut in.
///
/// The engine persists these on a goal checkpoint but must not know how to
/// obtain them: "which branch, which commit, is it dirty" is a question about
/// a version-control system, and which one — or whether there is one at all —
/// is the domain's business. A harness with no workspace supplies nothing and
/// the checkpoint records the fields as unknown, never as clean.
#[async_trait]
pub trait WorkspaceFacts: Send + Sync {
    async fn capture(&self) -> CheckpointWorkspace;
}
