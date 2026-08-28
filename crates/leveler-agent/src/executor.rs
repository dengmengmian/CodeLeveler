//! The state-driven single-agent tool loop.

pub mod closeout;
mod dispatch;
mod drive;
mod gates;
mod handlers;
pub use handlers::DelegatedChildResult;
pub(crate) mod host;
pub mod round_verdict;
mod stream;

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_context::load_rules;
use leveler_core::{ClarificationId, TurnId};
use leveler_execution::{ApprovalPolicy, Approver, AutoApprove, AutoReviewer, NeedUserReviewer};
use leveler_lifecycle::{
    EvidenceLedger, GateConfig, ObjectiveAnchor, PlanState, PlanStep, ProgressLedger, WorkProfile,
};
use leveler_memory::MemoryStore;
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelPricing, ModelRef, ModelRuntime,
    ReasoningEffort, Role, TokenUsage,
};
use leveler_tools::{ToolContext, ToolRegistry};

use self::dispatch::text_of;
use crate::nudges::first_user_text;
use crate::prompt::{PromptBuilder, TurnContext};
use crate::sub_agent::{AgentRole, DEFAULT_MAX_CONCURRENT_AGENTS, DEFAULT_MAX_TOTAL_AGENTS};

/// Secondary summarization/audit requests improve quality but must never make
/// an otherwise finished turn look hung for minutes.
///
/// Query-conditioned memory recall (tail injection). Each turn we retrieve the
/// top-scoring memories for the current request and inject their bodies as a
/// system block right before the (always-new) user message. This keeps the
/// cached system+history prefix untouched — the block rides the uncached tail —
/// and is never persisted (see `run_conversation` filtering out `System` roles),
/// so it stays fresh and never accumulates.
const RECALL_K: usize = 4;
/// Minimum BM25 score to inject a hit — `search` only returns positive matches,
/// so this just drops the weakest ties.
const RECALL_FLOOR: f64 = 0.1;
/// Total character budget for injected bodies, to keep the tail from bloating.
const RECALL_CHAR_BUDGET: usize = 1500;

/// Render scored memory hits into a tail-injection system block, or `None` when
/// empty. Bodies are included up to `RECALL_CHAR_BUDGET`; the header warns the
/// model these are retrieved, may not apply, and must be checked against code.
fn render_recall_block(
    hits: impl Iterator<Item = (leveler_memory::MemoryEntry, f64)>,
) -> Option<String> {
    let mut body = String::new();
    let mut used = 0usize;
    for (entry, _score) in hits {
        let line = format!("- {}: {}\n", entry.title.trim(), entry.body.trim());
        if used + line.len() > RECALL_CHAR_BUDGET && !body.is_empty() {
            break;
        }
        used += line.len();
        body.push_str(&line);
    }
    if body.is_empty() {
        return None;
    }
    Some(format!(
        "## Relevant memory (retrieved for this turn)\n\
         These were retrieved by relevance to the current request. They may not \
         apply — use only what is pertinent, and verify against the current code \
         before relying on any of it.\n{body}"
    ))
}

/// Status of one external verification check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentVerificationStatus {
    Passed,
    Failed,
    Skipped,
}

/// Events emitted as the loop progresses, for the CLI to render.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A model stream attempt is starting. Discard any in-flight deltas from
    /// the previous attempt before applying new ones.
    StreamAttemptStarted,
    /// A streamed chunk of assistant text (token-level, spec §16).
    AssistantDelta(String),
    /// A streamed chunk of model reasoning/summary, rendered separately from
    /// the final assistant answer.
    ReasoningDelta(String),
    /// The model produced assistant text this round (the whole message; also
    /// marks the end of any streamed deltas for the round).
    AssistantText(String),
    /// The model requested a tool call. `id` correlates with the matching
    /// [`AgentEvent::ToolResult`] (the two are NOT emitted adjacently once
    /// read-only tools run in parallel, so a UI must pair by id, not by order).
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        /// True when this call was dispatched into the concurrent read-only
        /// batch (a UI can render such calls as one parallel group).
        parallel: bool,
    },
    /// A tool finished. `id` matches its [`AgentEvent::ToolCall`]; denial/guard
    /// results carry an id with no prior `ToolCall`.
    ToolResult {
        id: String,
        name: String,
        is_error: bool,
        preview: String,
    },
    /// A recoverable pre-command workspace snapshot, correlated to the tool
    /// call that may mutate the workspace. The engine persists this with the
    /// owning turn id before forwarding it.
    WorkspaceSnapshot { call_id: String, snapshot: String },
    /// Token usage reported by the model for a request (may arrive mid-stream
    /// or at the end). Drives the context gauge.
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        /// Subset of `input_tokens` served from the provider's prefix cache.
        cached_input_tokens: u32,
    },
    /// The in-memory transcript was auto-compacted to fit the context window,
    /// shrinking from `from` to `to` messages.
    Compacted { from: usize, to: usize },
    /// C5-S3: the fold threshold climbed one tier on authoritative evidence.
    /// Persisted (via the engine event log) so a resumed task replays to the
    /// same budget instead of shrinking back to the initial tier.
    ContextExpanded {
        from: u32,
        to: u32,
        reason: &'static str,
        crossed_reliable: bool,
    },
    /// The model updated its structured plan via the `update_plan` tool. The
    /// full step list replaces any previous plan (not a delta).
    PlanUpdated { steps: Vec<PlanStep> },
    /// Exact message list the next model request will see. Emitted at a round
    /// boundary so crash recovery does not reconstruct a different context.
    ContextSnapshot { messages: Vec<Message> },
    /// Post-edit verification started.
    VerificationStarted,
    /// One post-edit verification check finished.
    VerificationCheck {
        name: String,
        status: AgentVerificationStatus,
        evidence: Option<String>,
    },
    /// Post-edit verification finished.
    VerificationFinished { passed: bool },
    /// A sub-agent was spawned and began working (concurrent delegation).
    SubAgentStarted {
        id: String,
        nickname: String,
        role: String,
        task: String,
        /// Built-in capability contract. Absent on events written before
        /// Child Profile existed (`None` means "not recorded", not "no profile").
        profile_id: Option<String>,
        profile_role: Option<String>,
        capabilities: Vec<String>,
    },
    /// A spawned sub-agent acquired an execution slot and/or reported updated
    /// cumulative token usage. Transient: the final result remains authoritative.
    SubAgentProgress {
        id: String,
        active: bool,
        input_tokens: u32,
        output_tokens: u32,
        cached_input_tokens: u32,
    },
    /// A sub-agent finished, with a short summary of its result and a compact
    /// projection of what it contributed.
    ///
    /// The projection is counts and a child id, never the finding records: the
    /// authority stays in the ledger, which is already durable. Carrying the
    /// records here would put a payload in every terminal event, which is the
    /// cost the event pipeline just finished paying down.
    SubAgentFinished {
        id: String,
        nickname: String,
        ok: bool,
        summary: String,
        /// Absent on events written before contribution tracing existed.
        contribution: Option<leveler_lifecycle::ChildResultProjection>,
    },
    /// Live step for one spawned sub-agent (tool start/finish). Transient UI
    /// signal — not full child transcript. Attributed by `id` so concurrent
    /// children stay distinguishable.
    SubAgentActivity {
        id: String,
        /// `tool_started` or `tool_finished`.
        phase: String,
        tool: String,
        /// Short capped preview (args or result head); never full output.
        preview: String,
        is_error: bool,
    },
    /// Host process gate refused `update_goal(complete)` (or similar).
    /// Persisted so resume/UI can show intercept history (not only ToolResult).
    GoalIntercepted { kind: String, detail: String },
    /// MA-WA1 delegation decision observability. `action`: `offered` (the
    /// one-shot keep-vs-delegate decision point was injected; `detail` names
    /// the trigger: `plan` | `mutation_fallback`), `delegated` (first Worker
    /// admitted; `detail` = its file scope), `kept` (first mutation after the
    /// offer with no Worker spawned — a first-class outcome, not a failure).
    /// Each fact is recorded at most once per goal epoch (ProgressLedger
    /// carries the flags across continue/resume/repair windows). Facts only:
    /// never a gate, never completion-relevant.
    DelegationStage { action: String, detail: String },
    /// Full process-evidence ledger snapshot after a mutation/verify/receipt/
    /// intercept change. SoT for resume of Delivery gates (last snapshot wins).
    EvidenceLedgerUpdated { ledger: EvidenceLedger },
    /// Cross-round progress / closeout ledger (resume + engine continue).
    ProgressUpdated { ledger: ProgressLedger },
    /// The harness started an advisory (tool-free) model call during closeout —
    /// a completeness audit or a compaction summary. These are extra model round
    /// trips that happen AFTER the visible answer, so without this a UI shows a
    /// bare "waiting for model" for many seconds with no idea why. Emitting the
    /// kind lets the status line name the wait ("completeness audit…").
    AdvisoryStarted { kind: AdvisoryKind },
    /// Heartbeat for a long-running command tool (spec: runtime observability).
    /// Emitted every few seconds while a `run_command`/`shell_command` is still
    /// executing, so the UI shows "运行 cargo test" with a live elapsed instead
    /// of a bare "等待模型" black box. `label` is the command line being run.
    CommandProgress { label: String, elapsed_ms: u64 },
    /// The loop finished with a final answer.
    Finished(String),
}

/// Which extra harness-initiated model round trip is starting during closeout.
/// Carried by [`AgentEvent::AdvisoryStarted`] so a UI can label the wait
/// instead of showing a bare "waiting for model". Audits and compaction are
/// tool-free advisory calls; a closeout nudge re-prompts the full loop once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvisoryKind {
    /// Context compaction: summarizing older transcript to fit the window.
    ContextCompaction,
    /// The unified closeout injected a nudge (executor/closeout.rs) and the
    /// model is being re-prompted. Without this the user sees the "final"
    /// answer, then an unexplained extra model round.
    CloseoutNudge(closeout::CloseoutReason),
    /// The engine opened another goal turn after a stalled one
    /// (`continue_active_goal`). This is a whole turn, not one round, and it
    /// starts after the user already read a final answer — unlabelled it is
    /// indistinguishable from a hang.
    GoalContinuation,
}

impl AdvisoryKind {
    /// Stable key for crossing the (serialized) engine event boundary.
    pub fn as_key(&self) -> &'static str {
        match self {
            AdvisoryKind::ContextCompaction => "context_compaction",
            AdvisoryKind::GoalContinuation => "goal_continuation",
            AdvisoryKind::CloseoutNudge(reason) => match reason {
                closeout::CloseoutReason::GoalUnresolved => "nudge_goal_unresolved",
                closeout::CloseoutReason::EmptyAnswer => "nudge_empty_answer",
            },
        }
    }

    /// Inverse of [`Self::as_key`] (engine → app event replay).
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "context_compaction" => Some(AdvisoryKind::ContextCompaction),
            "goal_continuation" => Some(AdvisoryKind::GoalContinuation),
            _ => {
                let reason = closeout::CloseoutReason::from_key(key.strip_prefix("nudge_")?)?;
                Some(AdvisoryKind::CloseoutNudge(reason))
            }
        }
    }
}

#[cfg(test)]
mod advisory_kind_tests {
    use super::AdvisoryKind;
    use super::closeout::CloseoutReason;

    #[test]
    fn advisory_kind_keys_round_trip() {
        let all = [
            AdvisoryKind::ContextCompaction,
            AdvisoryKind::GoalContinuation,
            AdvisoryKind::CloseoutNudge(CloseoutReason::GoalUnresolved),
            AdvisoryKind::CloseoutNudge(CloseoutReason::EmptyAnswer),
        ];
        for kind in all {
            assert_eq!(
                AdvisoryKind::from_key(kind.as_key()),
                Some(kind),
                "key {} must round-trip",
                kind.as_key()
            );
        }
        assert_eq!(AdvisoryKind::from_key("nudge_bogus"), None);
        assert_eq!(AdvisoryKind::from_key("bogus"), None);
    }
}

// PlanStep lives in leveler-lifecycle; re-exported from crate root.

/// A request for the user to clarify something mid-task (spec §35): the model
/// calls `request_user_input` (or legacy `ask_user`), which blocks until the UI answers.
#[derive(Debug, Clone)]
pub struct ClarificationRequest {
    pub id: ClarificationId,
    /// Filled by the engine recorder once the persisted turn exists.
    pub turn_id: Option<TurnId>,
    pub tool: String,
    pub call_id: String,
    pub action_fingerprint: String,
    pub question: String,
    pub options: Vec<String>,
}

/// How a clarification request ended. `Answered` is the ONLY variant that may
/// be presented to the model as the user speaking; every other variant must
/// surface as "no user reply", never as an (empty) answer. This is the
/// clarification-side counterpart of `Approver::has_human` (R004 F2: an empty
/// string silently impersonated the user).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClarifyOutcome {
    /// A human answered with this text (may still be empty = explicit skip).
    Answered(String),
    /// A human saw the question and explicitly skipped it.
    Skipped,
    /// No human is attached to this run (headless run, sub-agent, or the
    /// question could not be delivered to any client).
    Unattended,
    /// The question was delivered but nobody responded before the deadline.
    TimedOut,
    /// The turn is being cancelled; the answer no longer matters.
    Cancelled,
}

impl ClarifyOutcome {
    /// Short machine label for recording/observability.
    pub fn label(&self) -> &'static str {
        match self {
            ClarifyOutcome::Answered(_) => "answered",
            ClarifyOutcome::Skipped => "skipped",
            ClarifyOutcome::Unattended => "unattended",
            ClarifyOutcome::TimedOut => "timed_out",
            ClarifyOutcome::Cancelled => "cancelled",
        }
    }
}

/// Something that can answer clarification requests.
#[async_trait]
pub trait Clarifier: Send + Sync {
    async fn clarify(&self, request: &ClarificationRequest) -> ClarifyOutcome;
}

/// Non-interactive default: no human is attached, and the model must be told
/// so instead of receiving a fabricated empty "answer".
pub struct AutoClarify;

#[async_trait]
impl Clarifier for AutoClarify {
    async fn clarify(&self, _request: &ClarificationRequest) -> ClarifyOutcome {
        ClarifyOutcome::Unattended
    }
}

/// Block a `(tool, args)` call once it has already produced an identical result
/// this many times (no-progress loop guard). Two "maybe this time" attempts run;
/// the third identical repeat is short-circuited.
const LOOP_GUARD_THRESHOLD: u32 = 2;

/// What decides whether another model/tool round may start.
///
/// Top-level user turns run until a semantic terminal state. Bounded work is
/// reserved for measured units whose ownership requires a hard edge
/// (orchestration nodes and eval cases). Sub-agents use the same semantic
/// completion rule as their parent, with a wall-clock safety limit instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationPolicy {
    UntilTerminal,
    Bounded { max_rounds: std::num::NonZeroU32 },
}

impl ContinuationPolicy {
    pub fn bounded(max_rounds: u32) -> Self {
        Self::Bounded {
            max_rounds: std::num::NonZeroU32::new(max_rounds.max(1)).expect("max(1) is non-zero"),
        }
    }

    pub fn round_limit(self) -> Option<u32> {
        match self {
            Self::UntilTerminal => None,
            Self::Bounded { max_rounds } => Some(max_rounds.get()),
        }
    }

    fn allows_round_after(self, round: u32) -> bool {
        self.round_limit().is_none_or(|max| round < max)
    }
}

/// Optional per-run resource limits, enforced at model/tool boundaries (spec §27).
///
/// **Semantics (all dimensions):**
/// - `None` = unlimited
/// - `Some(0)` = hard exhausted (no further spend allowed)
/// - `Some(n)` = at most `n` remaining / absolute cap depending on call site
///
/// Residual budgets for sub-agents use the same encoding so a depleted parent
/// cannot re-open an unlimited child via `0 == unlimited` confusion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StepLimits {
    /// Max `run_command` / `shell_command` executions. `None` = unlimited.
    pub max_commands: Option<u32>,
    /// Max distinct files this run may modify. `None` = unlimited.
    pub max_modified_files: Option<usize>,
    /// Max wall-clock duration for this run.
    pub max_duration: Option<std::time::Duration>,
    /// Max provider-reported input + output tokens across model requests.
    pub max_model_tokens: Option<u64>,
    /// Max auditable model cost in micro-USD. Requires pricing in the selected
    /// model profile; callers must reject a configured cost cap when pricing is
    /// unavailable rather than inventing a price.
    pub max_cost_usd_micros: Option<u64>,
    /// Absolute per-turn round ceiling. `None` falls back to a built-in default.
    /// This is an unconditional circuit breaker — independent of progress
    /// heuristics — so an `UntilTerminal` turn always terminates.
    pub max_rounds: Option<u32>,
}

/// Why the loop stopped. Serialized (snake_case) into terminal engine events
/// so blocked/budget/complete stay machine-discriminable after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The task has explicit completion evidence, rather than merely a natural
    /// end to one model response.
    Completed,
    /// The model naturally ended its answer. This closes the conversational
    /// turn but does not prove that an external task is complete.
    Answered,
    /// The run ended cleanly, but its semantic completeness could not be
    /// established after bounded audit/repair attempts.
    Incomplete,
    /// The plan was already complete, but the model kept doing redundant
    /// closeout work (re-running builds/tests or re-observing) and a guard
    /// force-stopped the turn after the closeout cap. The task's work IS done —
    /// completeness is the verify layer's call — so this is an abnormal *end*,
    /// not an incomplete *task*: it maps to a completed session, never to the
    /// Execute state (which would misreport a finished task as needing more
    /// work). Distinct from Incomplete so "how the turn ended" and "was the task
    /// finished" are not encoded in one value.
    CloseoutForced,
    /// A token or cost budget was exhausted first.
    BudgetExhausted,
    /// The absolute per-turn round ceiling was hit. This is the unconditional
    /// circuit breaker that fires even when every progress watchdog was evaded
    /// (a "busy" loop that fakes progress each round). It guarantees termination
    /// but is not a budget the user can lift by saying "继续" — so it is kept
    /// distinct from `BudgetExhausted` for honest logs/telemetry. Maps to the
    /// same session outcome as `BudgetExhausted` (Incomplete / Execute).
    TurnLimitReached,
    /// Goal mode: the model declared the goal unreachable via `update_goal(blocked)`.
    Blocked,
    /// The turn ended because HARNESS POLICY refused every attempted action
    /// for several rounds (plan gate / budgets / allowlist) and the injected
    /// corrective directive was also ignored. This is the harness blocking
    /// itself — distinct from `Incomplete` (agent stagnation) so a policy
    /// dead-end is never reported as "the model made no progress" (R006 R6-P1).
    PolicyBlocked,
    /// Goal mode: the model went quiet without ever resolving the goal via
    /// `update_goal`, even after the quiet-nudge cap. Not a success.
    Stalled,
    /// The run finished its work, but no verification gate produced passing
    /// evidence, so leveler will not claim it verified. Synthesized by the
    /// app's verification mapping — the token loop never emits this. It means
    /// "done, but unverified", NOT "failed" or "gave up".
    CompletedUnverified,
}

/// The result of an executor run.
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub final_text: String,
    pub rounds: u32,
    pub modified_files: Vec<String>,
    pub stop_reason: StopReason,
    /// Human-readable cause for non-success stops (audit gaps, stall, budget…).
    /// Empty when the stop reason is self-explanatory.
    pub stop_detail: Option<String>,
    /// When `stop_reason` is [`StopReason::BudgetExhausted`], which limit fired
    /// and spent vs cap. `None` for other stops (and for legacy bounded-round
    /// exits that reuse the BudgetExhausted label without a resource dimension).
    pub budget_exhaustion: Option<crate::budget::BudgetExhaustion>,
    /// Continuous-use / latency counters for S0/S3 hard gates.
    pub metrics: leveler_lifecycle::DepthUseMetrics,
    /// Final progress / closeout state (engine continue_active_goal reads this).
    pub progress: ProgressLedger,
    /// Active objective used for this drive (host-pinned).
    pub objective: ObjectiveAnchor,
}

impl AgentOutcome {
    /// Build a drive outcome from the fields that vary per exit plus the drive's
    /// running state (`metrics`/`progress`/`objective`, always cloned here).
    /// Every early return in `drive` funnels through this, so a new shared field
    /// is added once here instead of at each of the ~10 return sites.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn drive_result(
        final_text: String,
        rounds: u32,
        modified_files: Vec<String>,
        stop_reason: StopReason,
        stop_detail: Option<String>,
        metrics: &leveler_lifecycle::DepthUseMetrics,
        progress: &ProgressLedger,
        objective: &ObjectiveAnchor,
    ) -> Self {
        Self {
            final_text,
            rounds,
            modified_files,
            stop_reason,
            stop_detail,
            budget_exhaustion: None,
            metrics: metrics.clone(),
            progress: progress.clone(),
            objective: objective.clone(),
        }
    }

    /// Like [`Self::drive_result`], but stamps structured budget-exhaust facts
    /// and a parseable `stop_detail` contract.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn drive_budget_exhausted(
        final_text: String,
        rounds: u32,
        modified_files: Vec<String>,
        exhaustion: crate::budget::BudgetExhaustion,
        metrics: &leveler_lifecycle::DepthUseMetrics,
        progress: &ProgressLedger,
        objective: &ObjectiveAnchor,
    ) -> Self {
        let stop_detail = Some(exhaustion.stop_detail());
        Self {
            final_text,
            rounds,
            modified_files,
            stop_reason: StopReason::BudgetExhausted,
            stop_detail,
            budget_exhaustion: Some(exhaustion),
            metrics: metrics.clone(),
            progress: progress.clone(),
            objective: objective.clone(),
        }
    }
}

/// Errors that abort the loop (model failures; tool failures are fed back to
/// the model instead).
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    // `ModelError` already renders as "model error [Kind]: …" — don't prepend
    // another "model error:" prefix (that produced "model error: model error").
    #[error("{0}")]
    Model(#[from] ModelError),
    #[error("cancelled")]
    Cancelled,
    /// The runtime lost task ownership: a newer OwnerEpoch exists. The run
    /// must abort; a stale runtime writes no further canonical facts and
    /// dispatches no further tools.
    #[error("stale runtime ownership: {0}")]
    StaleOwnership(String),
    #[error("invalid execution budget: {0}")]
    InvalidBudget(String),
    #[error("persistence error: {0}")]
    Persistence(String),
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

#[async_trait]
pub trait EventBarrier: Send + Sync {
    async fn flush(&self) -> Result<(), AgentError>;

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
    ) -> Result<Option<String>, AgentError>;
}

/// A delegated agent's tool call, attributed to the child that made it.
#[derive(Debug, Clone, PartialEq)]
pub enum ChildToolEvent {
    Started {
        agent_id: String,
        call_id: String,
        name: String,
        arguments: String,
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

/// A sink that persists the transcript as the loop advances, enabling resume.
/// Called with the messages appended in each step (seed, then per round).
#[async_trait]
pub trait TranscriptSink: Send {
    async fn append(&mut self, messages: &[Message]) -> Result<(), AgentError>;

    async fn record_model_request(
        &mut self,
        _record: &ModelRequestRecord,
    ) -> Result<(), AgentError> {
        Ok(())
    }
}

/// Diagnostic facts for a completed provider request. Persisting the normalized
/// finish reason makes truncation distinguishable from semantic completion.
#[derive(Debug, Clone)]
pub struct ModelRequestRecord {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub usage: TokenUsage,
    pub finish_reason: FinishReason,
    pub latency_ms: u64,
    pub retry_count: u32,
}

/// A sink that discards everything (non-persistent runs, tests).
pub struct NoopSink;

#[async_trait]
impl TranscriptSink for NoopSink {
    async fn append(&mut self, _messages: &[Message]) -> Result<(), AgentError> {
        Ok(())
    }
}

struct SubAgentProgressSink {
    id: String,
    events: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
}

impl SubAgentProgressSink {
    fn new(id: String, events: tokio::sync::mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self {
            id,
            events,
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
        }
    }

    fn capped(value: u64) -> u32 {
        value.min(u32::MAX as u64) as u32
    }
}

#[async_trait]
impl TranscriptSink for SubAgentProgressSink {
    async fn append(&mut self, _messages: &[Message]) -> Result<(), AgentError> {
        Ok(())
    }

    async fn record_model_request(
        &mut self,
        record: &ModelRequestRecord,
    ) -> Result<(), AgentError> {
        self.input_tokens = self.input_tokens.saturating_add(record.usage.input_tokens);
        self.output_tokens = self
            .output_tokens
            .saturating_add(record.usage.output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(record.usage.cached_input_tokens);
        let _ = self.events.send(AgentEvent::SubAgentProgress {
            id: self.id.clone(),
            active: true,
            input_tokens: Self::capped(self.input_tokens),
            output_tokens: Self::capped(self.output_tokens),
            cached_input_tokens: Self::capped(self.cached_input_tokens),
        });
        Ok(())
    }
}

pub(crate) struct StreamRoundResult {
    request_id: String,
    message: Message,
    usage: TokenUsage,
    finish_reason: FinishReason,
    latency_ms: u64,
    retry_count: u32,
}

/// The execution-policy slice a delegated executor needs. The engine resolves
/// one value per role from model facts, task facts, and eval-only overrides;
/// the agent loop only consumes the already-resolved values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubAgentExecutionPolicy {
    pub max_search_calls_per_step: usize,
    pub max_parallel_tools: usize,
    pub require_explicit_plan: bool,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Resolved execution policy for each delegatable role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubAgentExecutionPolicies {
    pub default: SubAgentExecutionPolicy,
    pub explorer: SubAgentExecutionPolicy,
    pub worker: SubAgentExecutionPolicy,
}

impl SubAgentExecutionPolicies {
    fn for_role(self, role: AgentRole) -> SubAgentExecutionPolicy {
        match role {
            AgentRole::Default => self.default,
            // A reviewer is an explorer with a different brief: same read-only
            // shape, same loop budget.
            AgentRole::Explorer | AgentRole::Reviewer => self.explorer,
            AgentRole::Worker => self.worker,
        }
    }
}

/// Where mid-turn user input comes from.
///
/// A correction like "actually use the other module" is worthless once the work
/// is finished, so it must reach the model at the next round rather than after
/// the turn. The loop only asks; the host decides what (if anything) is
/// waiting — the same shape as `Approver` and `Clarifier`.
pub trait SteeringSource: Send + Sync {
    /// Take everything queued since the last call. Returning empty is the
    /// normal case and must be cheap.
    fn take_pending(&self) -> Vec<String>;
}

/// How one turn runs.
///
/// Grouped as one value so a caller — or an agent definition — can carry a
/// complete policy instead of setting a dozen independent knobs and hoping they
/// are consistent. The loop reads it; it never derives policy of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnPolicy {
    // ── Loop shape ──────────────────────────────────────────────────────────
    /// Max search-tool calls allowed within a single step/round (0 = off);
    /// excess calls are denied so the model acts on what it already gathered.
    pub max_search_calls_per_step: usize,
    /// Max read-only tools executed concurrently within one round's parallel
    /// batch (0 = unbounded).
    pub max_parallel_tools: usize,
    /// Ask the model to write an explicit plan before acting (spec §17).
    pub require_explicit_plan: bool,
    /// Per-request reasoning effort selected by the execution-policy resolver.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// The usable context window in tokens (0 = disabled). When the last
    /// request's reported token count exceeds this, the in-memory transcript is
    /// compacted before the next round so a long task never overflows.
    /// C5-S3 note: this stays the INITIAL fold threshold (the resolver's
    /// compatibility mirror). The live threshold is `ContextBudgetState` in
    /// the drive loop; adaptive expansion never mutates this field.
    pub context_budget: u32,
    /// C5-S3: allow the fold threshold to climb the tier ladder on evidence.
    pub adaptive_context: bool,
    /// The resolver's budget ladder (sorted, deduped, clamped). Empty when
    /// adaptive context is off.
    pub context_tiers: Vec<u32>,
    /// The model's quality boundary: plain evidence never expands past it.
    pub reliable_context: u32,
    /// One-shot strong-evidence grant from the engine (a repair turn whose
    /// failure evidence may reference folded state).
    pub repair_expansion_evidence: bool,
    /// Budget the engine recovered from prior `ContextExpanded` events, so a
    /// resumed task does not silently shrink back to the initial tier.
    pub restored_context_budget: Option<u32>,

    // ── Completion gates ────────────────────────────────────────────────────
    /// The run ends only when the model explicitly calls
    /// `update_goal(complete|blocked)`. Going quiet does not finish it.
    pub goal_mode: bool,
    /// Refuse `update_goal(complete)` while model-declared todos are open.
    pub goal_todo_gate: bool,
    /// Delivery process evidence gate (mutation → verify).
    pub delivery_gate: bool,
    /// Product progress heuristics: the identical-result loop guard and the
    /// round-verdict machinery (observe-only streaks, closeout-thrash forcing,
    /// all-refused streaks). OFF disables the heuristics but never the safety
    /// boundary — the absolute round ceiling, step limits, wall clock, and
    /// cancellation are host safety and remain unconditional.
    pub progress_guards: bool,

    // ── Delegation ──────────────────────────────────────────────────────────
    /// When false, `spawn_agent` is not advertised (delegation kill-switch).
    pub allow_delegation: bool,
    /// H-C experiment: WHEN the keep-vs-delegate surface is raised. Default is
    /// `PlanRegistration`, the shipped behaviour; the other arm exists solely
    /// so the timing hypothesis can be tested causally.
    pub delegation_timing: crate::sub_agent::DelegationTiming,
    /// Max sub-agents running at once (within a spawn batch).
    pub max_concurrent_agents: usize,
    /// Max sub-agents spawned across the whole top-level run.
    pub max_total_agents: usize,
}

impl Default for TurnPolicy {
    fn default() -> Self {
        Self {
            max_search_calls_per_step: 0,
            max_parallel_tools: 0,
            require_explicit_plan: false,
            reasoning_effort: None,
            context_budget: 0,
            adaptive_context: false,
            context_tiers: Vec::new(),
            reliable_context: 0,
            repair_expansion_evidence: false,
            restored_context_budget: None,
            goal_mode: false,
            // Matches the historical `Executor::new` default — the gate is ON.
            goal_todo_gate: true,
            delivery_gate: false,
            progress_guards: true,
            allow_delegation: true,
            delegation_timing: crate::sub_agent::DelegationTiming::default(),
            max_concurrent_agents: DEFAULT_MAX_CONCURRENT_AGENTS,
            max_total_agents: DEFAULT_MAX_TOTAL_AGENTS,
        }
    }
}

impl TurnPolicy {
    /// The minimal direct policy (convergence plan phase 5): every PRODUCT
    /// heuristic off — no plan gate, no search cap, no todo/delivery
    /// completion gates, no progress guards. Safety is untouched: admission
    /// (hooks/rules/approval), the side-effect barrier, step limits, the
    /// absolute round ceiling, and cancellation apply exactly as always.
    pub fn minimal() -> Self {
        Self {
            goal_todo_gate: false,
            progress_guards: false,
            ..Self::default()
        }
    }
}

/// A single-agent tool executor.
pub struct Executor {
    /// How this turn runs (loop shape, gates, delegation).
    policy: TurnPolicy,
    /// Mid-turn user input, when the host offers any.
    steering: Option<Arc<dyn SteeringSource>>,
    runtime: Arc<dyn ModelRuntime>,
    registry: Arc<ToolRegistry>,
    tool_context: ToolContext,
    model: ModelRef,
    continuation: ContinuationPolicy,
    max_output_tokens: u32,
    pricing: Option<ModelPricing>,
    approver: Arc<dyn Approver>,
    auto_reviewer: Arc<dyn AutoReviewer>,
    approval_policy: ApprovalPolicy,
    clarifier: Arc<dyn Clarifier>,
    /// Sub-agent nesting depth (0 = the top-level agent). Bounds `spawn_agent`
    /// recursion.
    depth: u32,
    /// This agent's delegation role (drives its prompt framing).
    agent_role: AgentRole,
    /// When `Some`, `apply_patch` may only touch these files (worker ownership).
    /// `None` = unrestricted.
    write_allowlist: Option<Vec<String>>,
    /// Workspace-wide write-ownership truth, shared by the whole execution
    /// tree (late-bound ownership: spawned children claim scopes here).
    ownership: Arc<crate::ownership::OwnershipRegistry>,
    /// Role-specific policies resolved by the engine for delegated executors.
    /// Direct library users fall back to the parent's settings, with writes
    /// serialized, so the safety invariant does not depend on the app layer.
    sub_agent_policies: Option<SubAgentExecutionPolicies>,
    /// Seeded plan mirror (resume / host-preseed). Local drive state starts here.
    seeded_plan: PlanState,
    /// Seeded process evidence (resume from last EvidenceLedgerUpdated).
    seeded_ledger: EvidenceLedger,
    /// Seeded progress (engine continue / resume).
    seeded_progress: ProgressLedger,
    /// Settlements from a previous window the host proved durable but the
    /// parent may not have acted on (MA-RT-3 C10). Re-delivered once at the
    /// top of a depth-0 run; the pruned outstanding record is the
    /// once-per-restart mark.
    restart_settled_children: Vec<crate::sub_agent::SettledChildNotice>,
    /// Cross-model completion judge: the model the Completion Reconciliation
    /// Gate calls. `None` = the executor's own model (documented fallback).
    /// Separating judge from executor breaks the correlated-interpretation
    /// failure where the same model approves its own semantic rescoping.
    /// If a configured judge model is unreachable the gate fails closed —
    /// never a silent same-model fallback.
    reconciliation_model: Option<ModelRef>,
    /// Optional host-provided objective (overrides first-user fallback).
    seeded_objective: Option<ObjectiveAnchor>,
    /// Short memory INDEX for cache-stable system injection (titles only).
    memory_index: String,
    /// Hard per-run limits on commands / modified files / wall-clock time,
    /// checked before each tool call (spec §27).
    step_limits: StepLimits,
    /// This model's own system prompt, replacing the default base. Comes from
    /// the model profile; None uses `prompts/base.md`.
    base_instructions: Option<String>,
    /// Ask agent-created git commits to carry a model-aware CodeLeveler trailer.
    commit_co_author: bool,
    /// Optional project/global permission rules (SEC-1). Behind a lock so an
    /// `ApproveAlways` decision can extend the live set after persisting the
    /// new project rule.
    permission_rules: std::sync::RwLock<leveler_execution::PermissionRuleSet>,
    /// Project permission-rules file; `ApproveAlways` persists new rules here.
    /// `None` degrades `ApproveAlways` to session-only.
    permission_rules_path: Option<std::path::PathBuf>,
    /// Optional Pre/Post tool hooks (SEC-8).
    hook_runner: leveler_execution::HookRunner,
    /// Project state dir for durable permission grants (SEC-2); None disables.
    grants_state_dir: Option<std::path::PathBuf>,
    /// Side-effect barrier: canonical tool events must be durable before a
    /// tool with possible side effects is dispatched. `None` = no durable
    /// host (standalone library use); the loop proceeds without waiting.
    event_barrier: Option<Arc<dyn EventBarrier>>,
    /// Durable-checkpoint port for context compaction (long-goal P3).
    /// `None` = no durable host; folds keep their pre-checkpoint behavior.
    compaction_checkpoint: Option<Arc<dyn CompactionCheckpoint>>,
    /// Ownership fence consulted after the admission barriers and before
    /// tool dispatch; None = unfenced (non-engine/test executors).
    execution_fence: Option<Arc<dyn ExecutionFence>>,
    /// This agent's delegation id, when it IS a delegated agent. Its tool
    /// events are attributed to it so the host can tell whose side effect a
    /// dangling call belongs to.
    agent_id: Option<String>,
}

impl Executor {
    pub fn new(
        runtime: Arc<dyn ModelRuntime>,
        registry: Arc<ToolRegistry>,
        tool_context: ToolContext,
        model: ModelRef,
        max_rounds: u32,
    ) -> Self {
        // Ownership exclusivity must follow the workspace volume's real path
        // identity, not a platform guess: a case-folding volume makes
        // `src/Parser.rs` and `src/parser.rs` one file, and treating them as
        // two hands both children "exclusive" ownership of the same bytes.
        let case_insensitive = tool_context.execution.workspace.path_case_insensitive();
        Self {
            base_instructions: None,
            commit_co_author: true,
            runtime,
            registry,
            tool_context,
            model,
            continuation: if max_rounds == 0 {
                ContinuationPolicy::UntilTerminal
            } else {
                ContinuationPolicy::bounded(max_rounds)
            },
            max_output_tokens: 4096,
            pricing: None,
            approver: Arc::new(AutoApprove),
            auto_reviewer: Arc::new(NeedUserReviewer),
            approval_policy: ApprovalPolicy::default(),
            clarifier: Arc::new(AutoClarify),
            policy: TurnPolicy::default(),
            steering: None,
            depth: 0,
            agent_role: AgentRole::Default,
            write_allowlist: None,
            ownership: Arc::new(crate::ownership::OwnershipRegistry::new(case_insensitive)),
            sub_agent_policies: None,
            seeded_plan: PlanState::default(),
            seeded_ledger: EvidenceLedger::default(),
            seeded_progress: ProgressLedger::default(),
            restart_settled_children: Vec::new(),
            reconciliation_model: None,
            seeded_objective: None,
            memory_index: String::new(),
            step_limits: StepLimits::default(),
            permission_rules: std::sync::RwLock::new(
                leveler_execution::PermissionRuleSet::default(),
            ),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(
                leveler_core::environment().current_dir().to_path_buf(),
            ),
            grants_state_dir: None,
            event_barrier: None,
            compaction_checkpoint: None,
            execution_fence: None,
            agent_id: None,
        }
    }

    /// Install permission rules evaluated before profile approval policy.
    pub fn with_permission_rules(mut self, rules: leveler_execution::PermissionRuleSet) -> Self {
        self.permission_rules = std::sync::RwLock::new(rules);
        self
    }

    /// Set the project permission-rules file `ApproveAlways` appends to
    /// (None degrades `ApproveAlways` to session-only).
    pub fn with_permission_rules_path(mut self, path: Option<std::path::PathBuf>) -> Self {
        self.permission_rules_path = path;
        self
    }

    /// Install Pre/Post tool hooks.
    pub fn with_hook_runner(mut self, hooks: leveler_execution::HookRunner) -> Self {
        self.hook_runner = hooks;
        self
    }

    /// Enable durable project grants under this state directory.
    pub fn with_grants_state_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.grants_state_dir = Some(dir.into());
        self
    }

    /// Optional durable grants directory (None disables).
    pub fn with_grants_state_dir_opt(mut self, dir: Option<std::path::PathBuf>) -> Self {
        self.grants_state_dir = dir;
        self
    }

    /// Seed the in-memory plan mirror (resume from last PlanUpdated, or host).
    pub fn with_seeded_plan(mut self, plan: PlanState) -> Self {
        self.seeded_plan = plan;
        self
    }

    /// Seed the process evidence ledger (resume from last EvidenceLedgerUpdated).
    pub fn with_seeded_ledger(mut self, ledger: EvidenceLedger) -> Self {
        self.seeded_ledger = ledger;
        self
    }

    /// Seed progress ledger (engine continue_active_goal carries streak/closeout).
    pub fn with_seeded_progress(mut self, progress: ProgressLedger) -> Self {
        self.seeded_progress = progress;
        self
    }

    /// Select an independent model for the Completion Reconciliation Gate
    /// (cross-model judge). The main execution model is untouched.
    pub fn with_reconciliation_model(mut self, model: ModelRef) -> Self {
        self.reconciliation_model = Some(model);
        self
    }

    pub fn with_reconciliation_model_opt(mut self, model: Option<ModelRef>) -> Self {
        self.reconciliation_model = model;
        self
    }

    /// Hand the run the durable settlements a dead window left unconsumed
    /// (derived by the host from `SubAgentFinished` facts, MA-RT-3 C10).
    pub fn with_restart_settled_children(
        mut self,
        children: Vec<crate::sub_agent::SettledChildNotice>,
    ) -> Self {
        self.restart_settled_children = children;
        self
    }

    /// Pin the active objective for this drive (Chat/Goal host path).
    pub fn with_objective(mut self, objective: ObjectiveAnchor) -> Self {
        self.seeded_objective = Some(objective);
        self
    }

    /// Enable/disable the S2 goal todo gate on update_goal(complete).
    pub fn with_goal_todo_gate(mut self, on: bool) -> Self {
        self.policy.goal_todo_gate = on;
        self
    }

    /// Apply work-profile process gates (Delivery enables delivery_gate + audit).
    pub fn with_work_profile(mut self, profile: WorkProfile) -> Self {
        let gate = GateConfig::for_work_profile(profile);
        self.policy.goal_todo_gate = gate.goal_todo_gate;
        self.policy.delivery_gate = gate.delivery_gate;
        if matches!(profile, WorkProfile::Delivery) {
            // Delivery may enable answer_audit as a tax when eval/host requests it;
            // default remains off unless explicitly re-enabled by factory.
        }
        self
    }

    /// Force the Delivery evidence gate on or off after the work profile is set.
    /// The eval ablation seam uses this to lower the rail without rebuilding
    /// the whole profile; production hosts should prefer [`Self::with_work_profile`].
    pub fn with_delivery_gate(mut self, on: bool) -> Self {
        self.policy.delivery_gate = on;
        self
    }

    /// Short INDEX lines injected into the system prompt (bodies never go here).
    pub fn with_memory_index(mut self, index: impl Into<String>) -> Self {
        self.memory_index = index.into();
        self
    }

    /// Whether delivery process evidence is enforced on update_goal(complete).
    pub fn delivery_gate_enabled(&self) -> bool {
        self.policy.delivery_gate
    }

    /// Set hard per-run limits on commands, modified files, and duration.
    pub fn with_step_limits(mut self, limits: StepLimits) -> Self {
        self.step_limits = limits;
        self
    }

    /// Select whether this executor runs to a semantic terminal state or owns
    /// a fixed number of rounds.
    pub fn with_continuation_policy(mut self, policy: ContinuationPolicy) -> Self {
        self.continuation = policy;
        self
    }

    /// Restrict edits (`apply_patch`/`replace`) to these paths (files or
    /// directory prefixes). Enforced BEFORE the tool runs; `None` = unrestricted.
    /// The write authority actually in force for THIS executor right now.
    /// `None` = unrestricted (the top-level agent / orchestrated hosts without
    /// a static allowlist). For a spawned child the answer is the live
    /// ownership registry: everything it has claimed (a legacy Worker's
    /// `files` arrive there as a pre-claim), which is EMPTY before its first
    /// grant — so every mutation is refused until it claims.
    pub(crate) fn effective_write_allowlist(&self) -> Option<Vec<String>> {
        if crate::sub_agent::ChildProfile::resolve(self.agent_role).read_only() {
            // Structurally read-only: no write tools exist; an empty list is
            // a consistent answer for the command pipeline.
            return Some(Vec::new());
        }
        // Any executor stamped with a delegated identity is governed by the
        // registry, even if depth were wrongly left at 0. The parent has no
        // agent_id and stays unrestricted except for others' claims.
        if let Some(id) = &self.agent_id {
            return Some(self.ownership.owned_by(id));
        }
        if self.depth > 0 {
            // Fail closed: a child factory that forgot `with_agent_id` must
            // not become a full-workspace writer.
            return Some(Vec::new());
        }
        self.write_allowlist.clone()
    }

    pub fn with_write_allowlist(mut self, paths: Option<Vec<String>>) -> Self {
        self.write_allowlist = paths.filter(|p| !p.is_empty());
        self
    }

    /// `Some(msg)` when a DELEGATED agent asks for a tool whose workspace
    /// effect the ownership model cannot bound.
    ///
    /// An MCP tool is a JSON-RPC proxy to a separate process that CodeLeveler
    /// launches with no sandbox, no workspace path preflight and no
    /// checkpoint, and `McpTool::execute` discards its `ToolContext`. So a
    /// claimed scope does not constrain it — admitting it through the
    /// ownership fence would assert a safety property that does not hold.
    /// It also declares no `mutates_files()`, which is the predicate all
    /// three fences key on, so today it passes every one of them untouched.
    ///
    /// Depth 0 is the user's own agent and keeps MCP, gated by approval.
    pub(crate) fn refuse_unboundable_delegated_tool(
        &self,
        call: &leveler_model::ToolCall,
    ) -> Option<String> {
        let delegated = self.depth > 0 || self.agent_id.is_some();
        (delegated && call.name.starts_with("mcp__")).then(|| {
            format!(
                "{} is unavailable to a delegated agent: an MCP server runs outside \
                 the workspace sandbox and outside any claimed write scope, so its \
                 effect cannot be bounded to yours. Report what you need in your \
                 result and let the main agent run it.",
                call.name
            )
        })
    }

    /// `Some(msg)` when this mutating call is not allowed under live write
    /// authority. An empty claimed set is zero authority: refuse even if the
    /// patch parser cannot name the target files.
    pub(crate) fn refuse_unscoped_mutation(
        &self,
        call: &leveler_model::ToolCall,
    ) -> Option<String> {
        if !self.registry.mutates_files(&call.name) {
            return None;
        }
        match self.effective_write_allowlist() {
            Some(allow) if allow.is_empty() => Some(
                "Edit rejected: no write scope is currently owned. Read the relevant \
                 code, then use claim_write_scope(paths) before modifying files."
                    .to_string(),
            ),
            Some(allow) => {
                let outside = crate::authorization::write_targets_outside_allowlist(call, &allow);
                if outside.is_empty() {
                    None
                } else {
                    Some(format!(
                        "Edit rejected: {} is outside your claimed scope ({}). \
                         Claim it with claim_write_scope first, or stay within your scope.",
                        outside.join(", "),
                        allow.join(", ")
                    ))
                }
            }
            None => None,
        }
    }

    /// Enable goal mode: require an explicit `update_goal(complete|blocked)` to
    /// end the run (see [`Executor::goal_mode`]).
    /// Supply mid-turn user input for this run.
    pub fn with_steering(mut self, source: Arc<dyn SteeringSource>) -> Self {
        self.steering = Some(source);
        self
    }

    /// `with_steering` for callers that may or may not have a source.
    pub fn with_steering_opt(mut self, source: Option<Arc<dyn SteeringSource>>) -> Self {
        self.steering = source;
        self
    }

    pub fn with_goal_mode(mut self, on: bool) -> Self {
        self.policy.goal_mode = on;
        self
    }

    /// Build a sub-agent that reuses this agent's runtime, tools, model, and
    /// permissions, but runs silently on its own fresh conversation with a
    /// wall-clock safety budget and one deeper nesting level.
    ///
    /// The runtime routes per request by `model.provider`, so a different model
    /// needs no different runtime. `base_instructions` is deliberately dropped
    /// when the model changes: it is that model's tailored prompt, and handing
    /// it to another model is worse than falling back to the shared base.
    /// Apply a named agent definition's own policy to this (child) executor.
    ///
    /// `tools` empty and `max_rounds` 0 both mean "inherit" — see
    /// [`crate::named_agent::NamedAgent`]. Narrowing only: a definition can take
    /// capability away, never add it back, so an explorer persona cannot name a
    /// write tool into existence.
    pub(crate) fn apply_agent_policy(&mut self, tools: &[String], max_rounds: u32) {
        if !tools.is_empty() {
            self.registry = Arc::new(self.registry.named_subset(tools));
        }
        if max_rounds > 0 {
            self.continuation = ContinuationPolicy::bounded(max_rounds);
        }
    }

    pub(crate) fn child_for_role_on(
        &self,
        role: AgentRole,
        files: Vec<String>,
        model_override: Option<ModelRef>,
    ) -> Executor {
        // The role's capability profile decides the toolset shape in ONE
        // place: read-only roles get a registry that physically holds no
        // write tools; a writer drops MCP proxies whose effect no claimed
        // scope can bound. See [`crate::child_profile::ChildProfile`].
        let profile = crate::sub_agent::ChildProfile::resolve(role);
        let registry = Arc::new(profile.apply_to_registry(&self.registry));
        let write_allowlist = (!profile.read_only() && !files.is_empty()).then_some(files);
        let child_policy = self.sub_agent_policies.map_or(
            SubAgentExecutionPolicy {
                max_search_calls_per_step: self.policy.max_search_calls_per_step,
                max_parallel_tools: if profile.serial_tools() {
                    1
                } else {
                    self.policy.max_parallel_tools
                },
                require_explicit_plan: self.policy.require_explicit_plan,
                reasoning_effort: self.policy.reasoning_effort,
            },
            |policies| policies.for_role(role),
        );
        // The profile's serial-tools contract binds regardless of which policy
        // source resolved the loop shape: a writer sharing the workspace never
        // runs tools in parallel.
        let child_policy = SubAgentExecutionPolicy {
            max_parallel_tools: if profile.serial_tools() {
                1
            } else {
                child_policy.max_parallel_tools
            },
            ..child_policy
        };
        let switched = model_override.is_some();
        Executor {
            // A sub-agent runs the parent's model, so it inherits that model's
            // prompt — unless it was pinned to another one.
            base_instructions: if switched {
                None
            } else {
                self.base_instructions.clone()
            },
            commit_co_author: self.commit_co_author,
            runtime: self.runtime.clone(),
            registry,
            tool_context: self.tool_context.clone(),
            model: model_override.unwrap_or_else(|| self.model.clone()),
            continuation: match profile.max_rounds() {
                Some(n) if n > 0 => ContinuationPolicy::bounded(n),
                _ => ContinuationPolicy::UntilTerminal,
            },
            max_output_tokens: self.max_output_tokens,
            pricing: self.pricing,
            approver: self.approver.clone(),
            auto_reviewer: self.auto_reviewer.clone(),
            approval_policy: self.approval_policy,
            // A sub-agent never blocks the UI for clarifications.
            clarifier: Arc::new(AutoClarify),
            // A sub-agent runs a self-contained task; steering belongs to the
            // top-level turn the user is watching.
            steering: None,
            policy: TurnPolicy {
                // Loop shape comes from the role's resolved policy…
                max_search_calls_per_step: child_policy.max_search_calls_per_step,
                max_parallel_tools: child_policy.max_parallel_tools,
                require_explicit_plan: child_policy.require_explicit_plan,
                reasoning_effort: child_policy.reasoning_effort,
                // …the rest is inherited or deliberately reset for a child.
                context_budget: self.policy.context_budget,
                // Sub-agents keep the static behavior in S3 v1: a child's
                // transcript is short-lived and expansion evidence is a
                // top-level task concern.
                adaptive_context: false,
                context_tiers: Vec::new(),
                reliable_context: self.policy.reliable_context,
                repair_expansion_evidence: false,
                restored_context_budget: None,
                max_concurrent_agents: self.policy.max_concurrent_agents,
                max_total_agents: self.policy.max_total_agents,
                // Children never advertise spawn_agent (depth already blocks it).
                allow_delegation: false,
                // Irrelevant for a child (no offer is ever raised), carried so
                // the field stays a single source of truth.
                delegation_timing: self.policy.delegation_timing,
                // A sub-agent finishes when it goes quiet; only the top-level
                // run uses explicit goal resolution.
                goal_mode: false,
                goal_todo_gate: false,
                delivery_gate: false,
                // A child inherits the parent's product-guard stance.
                progress_guards: self.policy.progress_guards,
            },
            depth: self.depth + 1,
            agent_role: role,
            write_allowlist,
            ownership: self.ownership.clone(),
            sub_agent_policies: self.sub_agent_policies,
            seeded_plan: PlanState::default(),
            seeded_ledger: EvidenceLedger::default(),
            seeded_progress: ProgressLedger::default(),
            restart_settled_children: Vec::new(),
            reconciliation_model: None,
            seeded_objective: None,
            memory_index: String::new(),
            step_limits: StepLimits {
                max_duration: Some(crate::sub_agent::SUB_AGENT_MAX_DURATION),
                ..StepLimits::default()
            },
            permission_rules: std::sync::RwLock::new(
                self.permission_rules
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            ),
            permission_rules_path: self.permission_rules_path.clone(),
            hook_runner: self.hook_runner.clone(),
            grants_state_dir: self.grants_state_dir.clone(),
            // A child shares the parent's barrier: its tool events are
            // recorded on the SAME ordered queue the barrier flushes, so a
            // delegated side effect is as durable-before-execution as a
            // parent one. `agent_id` is stamped by the spawn handler.
            event_barrier: self.event_barrier.clone(),
            // Children never cut goal checkpoints: the goal belongs to the
            // parent loop, and a child fold summarizing its own scratch
            // context must not write the goal's continuity record.
            compaction_checkpoint: None,
            execution_fence: self.execution_fence.clone(),
            agent_id: None,
        }
    }

    /// Set the sub-agent concurrency and total caps (per top-level run).
    pub fn with_agents(mut self, max_concurrent: usize, max_total: usize) -> Self {
        self.policy.max_concurrent_agents = max_concurrent.max(1);
        self.policy.max_total_agents = max_total;
        self
    }

    /// Product kill-switch: when false, `spawn_agent` is not in the tool list.
    pub fn with_delegation(mut self, allow: bool) -> Self {
        self.policy.allow_delegation = allow;
        self
    }

    /// H-C experiment: when the keep-vs-delegate surface is raised. Leaving
    /// this unset keeps the shipped `PlanRegistration` behaviour.
    pub fn with_delegation_timing(mut self, timing: crate::sub_agent::DelegationTiming) -> Self {
        self.policy.delegation_timing = timing;
        self
    }

    /// Set the usable context window in tokens (should come from the model
    /// profile's `limits.reliable_context`). Enables in-loop auto-compaction so
    /// a long autonomous task folds its transcript instead of overflowing the
    /// window. Ignored when zero.
    pub fn with_context_budget(mut self, context_budget: u32) -> Self {
        self.policy.context_budget = context_budget;
        self
    }

    /// C5-S3: enable adaptive expansion with the resolver's tier ladder and
    /// the model's quality boundary. Off (the default) reproduces the static
    /// S2 behavior exactly.
    pub fn with_context_expansion(
        mut self,
        adaptive: bool,
        tiers: Vec<u32>,
        reliable_context: u32,
    ) -> Self {
        self.policy.adaptive_context = adaptive;
        self.policy.context_tiers = tiers;
        self.policy.reliable_context = reliable_context;
        self
    }

    /// C5-S3: one-shot strong-evidence grant for a repair turn.
    pub fn with_repair_expansion_evidence(mut self, granted: bool) -> Self {
        self.policy.repair_expansion_evidence = granted;
        self
    }

    /// C5-S3: budget recovered from prior `ContextExpanded` events, so a
    /// resumed or follow-on turn does not shrink back to the initial tier.
    pub fn with_restored_context_budget(mut self, restored: Option<u32>) -> Self {
        self.policy.restored_context_budget = restored;
        self
    }

    /// Apply the resolved per-request reasoning effort. The protocol adapter
    /// combines this with the model's reasoning style; `None` lets the profile
    /// recommendation (or provider default) stand.
    pub fn with_reasoning_effort(mut self, reasoning_effort: Option<ReasoningEffort>) -> Self {
        self.policy.reasoning_effort = reasoning_effort;
        self
    }

    /// Install engine-resolved policies for executors created by delegation.
    pub fn with_sub_agent_policies(mut self, policies: SubAgentExecutionPolicies) -> Self {
        self.sub_agent_policies = Some(policies);
        self
    }

    /// Apply execution controls: cap search-tool calls per step to
    /// `max_search_calls_per_step` (0 = off), and bound the round's concurrent
    /// read-only tool batch to `max_parallel_tools` (0 = unbounded).
    pub fn with_execution_controls(
        mut self,
        max_search_calls_per_step: usize,
        max_parallel_tools: usize,
    ) -> Self {
        self.policy.max_search_calls_per_step = max_search_calls_per_step;
        self.policy.max_parallel_tools = max_parallel_tools;
        self
    }

    /// Cap the model's output tokens per request. Should come from the model
    /// profile's `limits.max_output_tokens` so large tool-call payloads (e.g. an
    /// apply_patch) aren't truncated mid-JSON. Ignored when zero.
    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        if max_output_tokens > 0 {
            self.max_output_tokens = max_output_tokens;
        }
        self
    }

    /// Attach auditable provider pricing for an optional cost budget.
    pub fn with_pricing(mut self, pricing: Option<ModelPricing>) -> Self {
        self.pricing = pricing;
        self
    }

    /// Ask for an explicit plan before acting (spec §17).
    pub fn with_structure(mut self, require_explicit_plan: bool) -> Self {
        self.policy.require_explicit_plan = require_explicit_plan;
        self
    }

    /// Toggle the product progress heuristics (loop guard, observe/closeout
    /// round verdicts). Safety limits are unaffected. See
    /// [`TurnPolicy::progress_guards`].
    pub fn with_progress_guards(mut self, on: bool) -> Self {
        self.policy.progress_guards = on;
        self
    }

    /// Replace the entire turn policy (minimal direct mode / custom hosts).
    /// Prefer the narrow builders unless a host really owns the whole policy.
    pub fn with_turn_policy(mut self, policy: TurnPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// The system prompt, extended with the enabled structural guidance.
    ///
    /// Depends only on the root project rules and the language of THIS turn's
    /// request, both fixed before the loop starts, so it is constant for the
    /// whole loop. Rules scoped to directories the agent later touches are
    /// appended at the transcript tail instead (see `load_scoped_rules`), which
    /// keeps this first message — and the provider's prefix cache of it —
    /// byte-identical.
    fn system_prompt(&self, request: &str) -> String {
        let project_rules = load_rules(self.tool_context.execution.workspace.root());
        let mut prompt = PromptBuilder::new()
            .base_instructions(self.base_instructions.clone())
            .commit_co_author(self.commit_co_author)
            .turn_context(TurnContext {
                model: self.model.clone(),
                mode: self.tool_context.policy.mode,
                network_allowed: self.approval_policy.network_allowed,
                deny_network: self.tool_context.policy.network_denied(),
                cwd: self.tool_context.execution.workspace.root().to_path_buf(),
                project_rules,
                user_language: crate::prompt::user_language(request),
            })
            .require_explicit_plan(self.policy.require_explicit_plan)
            .memory_index(self.memory_index.clone())
            .build();
        match self.agent_role {
            AgentRole::Explorer => prompt.push_str(
                "\n\nYou are an EXPLORER sub-agent: investigate and report back. You have \
                 read-only tools and CANNOT modify files or run commands. Answer the task \
                 precisely, citing the specific files/symbols you inspected; do not speculate. \
                 Call report_finding the moment you confirm each concrete discovery \
                 (relevant file/symbol, dependency, callsite, risk…) — findings reported \
                 early survive even if your run is cut short; prose written only at the \
                 end does not.",
            ),
            AgentRole::Worker => {
                prompt.push_str(
                    "\n\nYou are a WORKER sub-agent implementing a bounded change. Other agents \
                     may be editing the same workspace in parallel, so stay strictly within \
                     your assigned files and do not touch anything else. Call report_finding \
                     for each concrete note (risk, test, observation) you want the parent \
                     to judge; do not bury them only in the final prose.",
                );
                if let Some(files) = &self.write_allowlist {
                    prompt.push_str(&format!(
                        " You may edit ONLY these files: {}.",
                        files.join(", ")
                    ));
                }
            }
            AgentRole::Reviewer => prompt.push_str(
                "\n\nYou are a REVIEWER sub-agent. Another agent has already made the change \
                 described in your task; your job is to judge it independently, not to redo or \
                 extend it. You have read-only tools and CANNOT modify files. Work diff-first: \
                 when your task includes the unified diff, judge those hunks; read a changed \
                 file or its direct callers only where the diff's context is insufficient. \
                 Never expand into a whole-repository survey and never re-run builds or test \
                 suites — the change is judged from the code. Report EACH defect with one \
                 report_finding call the moment you confirm it (kind=correctness/risk, naming \
                 the file), setting blocking=true only for a defect that must be fixed before \
                 the change can ship. Your round budget is small and fixed: once every part of \
                 the change is judged, end immediately with a short final verdict — the \
                 defects found, or an explicit statement that nothing is blocking. Do not \
                 invent findings to look thorough.",
            ),
            AgentRole::Default => {}
        }
        if self.policy.goal_mode {
            prompt.push_str(
                "\n\nGOAL MODE: this turn ends ONLY when you call the update_goal tool — going \
                 silent does NOT finish it. There is no separate \"orchestrate\" pipeline: you \
                 stay in this direct tool loop for as long as the work needs.\n\
                 - **Greeting / small talk:** answer once in plain text (no trailing tip), then \
                 update_goal(status=\"complete\", summary=≤12 words). Do not call exploration \
                 tools for a bare greeting.\n\
                 - **Pure Q&A / advice / analysis with no repo edits:** answer fully in the \
                 prose. Optionally end with at most one soft tip line when a natural next action \
                 exists (concrete command or one clear follow-up slice) — see base prompt \
                 \"Soft follow-up tip\". Then update_goal(complete, summary=≤12 words). Prefer \
                 next_step only when it is a concrete action the user can run/send next.\n\
                 - Never write process closeout: \"任务完成\", \"已全面分析\", \"纯问答类任务\", \
                 \"纯信息查询\", \"直接结束\", \"不需要任何代码变更或测试\", restating the user \
                 question, or listing files you read as a wrap-up. update_goal is silent \
                 bookkeeping (UI does not show it); the answer text is the product.\n\
                 - **Code / config delivery:** keep working until every requirement is PROVEN \
                 against the current workspace (build/tests since last edit when you edited). \
                 Then update_goal(complete). If genuinely stuck, update_goal(blocked). Never \
                 shrink the objective to what already exists, and never reinterpret its terms \
                 into a weaker task the constraints happen to allow: an objective that \
                 conflicts with tests or constraints you must not change is `blocked` (name \
                 the conflict, revert edits that only served the abandoned attempt), not \
                 `complete` — including PARTIAL conflicts: satisfying part of the objective \
                 and quietly exempting the conflicting part is the same false completion. \
                 Use next_step for the single best \
                 follow-up action when one exists.\n\
                 - **Large multi-part goals:** break into concrete steps; use `spawn_agent` in \
                 the same turn for independent investigation or disjoint edits (explorer vs \
                 worker with disjoint `files`). After children return, integrate results and \
                 continue until the whole goal is proven — do not stop after the first sub-task.\n\
                 - **Same-session follow-ups:** use prior messages and what you already learned. \
                 Do not pretend the conversation is empty or re-scan the whole repo unless the \
                 user asks something that needs new evidence.\n\
                 - After a complete answer: final prose (optional one soft tip) + update_goal \
                 only. Zero \"done / closed / complete\" paragraphs.",
            );
        }
        prompt
    }

    /// Run this model's own system prompt instead of the default base. Comes
    /// from the model profile; None keeps `prompts/base.md`.
    pub fn with_base_instructions(mut self, instructions: Option<String>) -> Self {
        self.base_instructions = instructions;
        self
    }

    pub fn with_commit_co_author(mut self, enabled: bool) -> Self {
        self.commit_co_author = enabled;
        self
    }

    /// Use a specific approver (e.g. an interactive CLI prompt).
    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = approver;
        self
    }

    /// Install the host's side-effect barrier (see [`EventBarrier`]).
    pub fn with_event_barrier(mut self, barrier: Arc<dyn EventBarrier>) -> Self {
        self.event_barrier = Some(barrier);
        self
    }

    /// Install the durable-checkpoint port consulted at every context fold.
    pub fn with_compaction_checkpoint(mut self, port: Arc<dyn CompactionCheckpoint>) -> Self {
        self.compaction_checkpoint = Some(port);
        self
    }

    /// Install the ownership fence checked before every tool dispatch.
    pub fn with_execution_fence(mut self, fence: Arc<dyn ExecutionFence>) -> Self {
        self.execution_fence = Some(fence);
        self
    }

    /// Identify this executor as a delegated agent, so its tool events are
    /// recorded against it rather than looking like the parent's.
    pub fn with_agent_id(mut self, id: impl Into<String>) -> Self {
        self.agent_id = Some(id.into());
        self
    }

    /// Use an automatic reviewer before falling back to the user approver.
    pub fn with_auto_reviewer(mut self, reviewer: Arc<dyn AutoReviewer>) -> Self {
        self.auto_reviewer = reviewer;
        self
    }

    /// Use a specific clarifier (the UI answers ask-user calls, spec §35).
    pub fn with_clarifier(mut self, clarifier: Arc<dyn Clarifier>) -> Self {
        self.clarifier = clarifier;
        self
    }

    /// Start a fresh run for `goal`.
    pub async fn run(
        &self,
        goal: &str,
        observer: &mut (dyn FnMut(AgentEvent) + Send),
        sink: &mut dyn TranscriptSink,
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AgentError> {
        let objective = self
            .seeded_objective
            .clone()
            .unwrap_or_else(|| ObjectiveAnchor::from_session_goal(goal));
        self.run_with_content_and_objective(
            vec![ContentPart::Text {
                text: goal.to_string(),
            }],
            objective,
            observer,
            sink,
            cancellation,
        )
        .await
    }

    async fn run_with_content_and_objective(
        &self,
        content: Vec<ContentPart>,
        objective: ObjectiveAnchor,
        observer: &mut (dyn FnMut(AgentEvent) + Send),
        sink: &mut dyn TranscriptSink,
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AgentError> {
        let request = text_of(&content);
        let mut seed = vec![Message::text(Role::System, self.system_prompt(&request))];
        // `$skill` mentions: inject full SKILL.md bodies for this turn (S1).
        if let Some(injection) = self.skill_turn_injection(&request) {
            seed.push(Message::text(Role::System, injection));
        }
        if let Some(recall) = self.relevant_memory_injection(&request) {
            seed.push(Message::text(Role::System, recall));
        }
        seed.push(Message {
            role: Role::User,
            content,
        });
        sink.append(&seed).await?;
        self.drive(seed, objective, observer, sink, cancellation)
            .await
    }

    /// Resolve `$name` mentions in the user request into a system injection block.
    fn skill_turn_injection(&self, request: &str) -> Option<String> {
        let resolution =
            leveler_skills::resolve_mentions(self.tool_context.execution.workspace.root(), request);
        leveler_skills::render_turn_injection(&resolution)
    }

    /// Retrieve memories relevant to THIS turn and render them as a tail-injected
    /// system block, or `None` when memory is unconfigured or nothing matches.
    ///
    /// Uses the real BM25 `search` (not the pseudo-vector path). Callers push the
    /// result as a `Role::System` message immediately before the user message so
    /// the cached prefix is preserved and the block is stripped next turn.
    fn relevant_memory_injection(&self, request: &str) -> Option<String> {
        let root = self.tool_context.services.memory_root.as_ref()?;
        let store = MemoryStore::open(root).ok()?;
        let hits = store.search(request, RECALL_K).ok()?;
        render_recall_block(hits.into_iter().filter(|(_, score)| *score >= RECALL_FLOOR))
    }

    /// Continue a conversation: seed the model with the prior transcript plus a
    /// new user message, but persist only the new message (the prior is already
    /// stored). Makes turns share context so the agent remembers earlier turns.
    pub async fn run_conversation(
        &self,
        prior: Vec<Message>,
        content: Vec<ContentPart>,
        observer: &mut (dyn FnMut(AgentEvent) + Send),
        sink: &mut dyn TranscriptSink,
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AgentError> {
        let request = text_of(&content);
        // Active objective is THIS message — never the first user in `prior`.
        let objective = self.seeded_objective.clone().unwrap_or_else(|| {
            if self.policy.goal_mode {
                ObjectiveAnchor::from_session_goal(&request)
            } else {
                ObjectiveAnchor::from_user_message(&request)
            }
        });
        let user = Message {
            role: Role::User,
            content,
        };
        // Persist only the new user message; prior + system are not re-stored.
        sink.append(std::slice::from_ref(&user)).await?;

        let mut seed = vec![Message::text(Role::System, self.system_prompt(&request))];
        if let Some(injection) = self.skill_turn_injection(&request) {
            seed.push(Message::text(Role::System, injection));
        }
        // Drop any stale system messages from the prior transcript.
        seed.extend(prior.into_iter().filter(|m| m.role != Role::System));
        if let Some(recall) = self.relevant_memory_injection(&request) {
            seed.push(Message::text(Role::System, recall));
        }
        seed.push(user);
        self.drive(seed, objective, observer, sink, cancellation)
            .await
    }

    /// Resume from a previously-persisted transcript, continuing the loop.
    pub async fn resume(
        &self,
        prior: Vec<Message>,
        observer: &mut (dyn FnMut(AgentEvent) + Send),
        sink: &mut dyn TranscriptSink,
        cancellation: CancellationToken,
    ) -> Result<AgentOutcome, AgentError> {
        let objective = self
            .seeded_objective
            .clone()
            .unwrap_or_else(|| ObjectiveAnchor::from_user_message(first_user_text(&prior)));
        self.drive(prior, objective, observer, sink, cancellation)
            .await
    }
}

#[cfg(test)]
mod ownership_authority_tests {
    use super::*;

    struct NullRuntime;

    #[async_trait]
    impl leveler_model::ModelRuntime for NullRuntime {
        async fn generate(
            &self,
            _request: leveler_model::ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<leveler_model::ModelResponse, leveler_model::ModelError> {
            unreachable!("write-authority resolution never queries the model")
        }
        async fn stream(
            &self,
            _request: leveler_model::ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<leveler_model::ModelEventStream, leveler_model::ModelError> {
            unreachable!("write-authority resolution never queries the model")
        }
        async fn profile(
            &self,
            _model: &leveler_model::ModelRef,
        ) -> Result<leveler_model::ModelProfile, leveler_model::ModelError> {
            unreachable!("write-authority resolution never queries the model")
        }
    }

    fn executor() -> Executor {
        let dir = std::env::temp_dir().join(format!("leveler-lbo-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Executor::new(
            Arc::new(NullRuntime),
            Arc::new(leveler_tools::default_registry()),
            ToolContext::new(
                leveler_execution::Workspace::new(&dir).unwrap(),
                leveler_execution::PermissionProfile::Assisted,
            ),
            leveler_model::ModelRef::new("mock", "m"),
            4,
        )
    }

    /// The write-authority fallback is the dangerous edge: `effective_write_
    /// allowlist` returns the STATIC list when `agent_id` is None, and a
    /// static None means UNRESTRICTED. Every child construction path stamps an
    /// id (`sub_agent_run_future` → `with_agent_id`), so a child can never
    /// reach that branch — this pins both halves.
    #[test]
    fn a_child_without_a_claim_has_an_empty_write_authority() {
        let parent = executor();
        // Parent (depth 0): unrestricted unless a host pinned an allowlist.
        assert_eq!(parent.effective_write_allowlist(), None);

        // Identity, not depth, is the authority gate: a delegated id at
        // depth 0 (should not happen) still holds no write authority.
        let mis_depth = executor().with_agent_id("orphan-depth0");
        assert_eq!(
            mis_depth.effective_write_allowlist(),
            Some(Vec::new()),
            "agent_id without a claim is never unrestricted"
        );

        // A child WITH an id (the only shape the runtime builds) starts empty.
        let child = parent
            .child_for_role_on(AgentRole::Default, Vec::new(), None)
            .with_agent_id("child-1");
        assert_eq!(
            child.effective_write_allowlist(),
            Some(Vec::new()),
            "an unclaimed child must hold NO write authority"
        );

        // After a claim in the SHARED registry, the authority is exactly it.
        child
            .ownership
            .try_claim("child-1", &["src/a.rs".to_string()])
            .unwrap();
        assert_eq!(
            child.effective_write_allowlist(),
            Some(vec!["src/a.rs".to_string()])
        );
        // …and the parent sees the same truth (one registry, shared).
        assert_eq!(
            parent.ownership.owned_by("child-1"),
            vec!["src/a.rs".to_string()]
        );
    }

    /// A read-only role holds no authority regardless of registry state.
    #[test]
    fn a_read_only_child_never_gains_write_authority() {
        let parent = executor();
        for role in [AgentRole::Explorer, AgentRole::Reviewer] {
            let child = parent
                .child_for_role_on(role, Vec::new(), None)
                .with_agent_id("ro-1");
            child
                .ownership
                .try_claim("ro-1", &["src/a.rs".to_string()])
                .unwrap();
            assert_eq!(
                child.effective_write_allowlist(),
                Some(Vec::new()),
                "{role:?} must stay write-less"
            );
            child.ownership.release_all("ro-1");
        }
    }

    #[test]
    fn empty_authority_refuses_a_mutating_call_even_without_parseable_targets() {
        use leveler_core::ToolCallId;
        use leveler_model::ToolCall;
        let child = executor()
            .child_for_role_on(AgentRole::Default, Vec::new(), None)
            .with_agent_id("child-1");
        let call = ToolCall {
            id: ToolCallId::new("c1"),
            name: "apply_patch".into(),
            // Not a string — drive used to skip the fence when it could not
            // name target files. Empty authority must still refuse.
            arguments: serde_json::json!({ "patch": ["not", "a", "string"] }),
        };
        let msg = child
            .refuse_unscoped_mutation(&call)
            .expect("empty authority must refuse");
        assert!(msg.contains("no write scope"), "{msg}");
    }
}

#[cfg(test)]
mod recall_tests {
    use super::{RECALL_CHAR_BUDGET, RECALL_FLOOR, RECALL_K, render_recall_block};
    use leveler_memory::{MemoryEntry, MemoryStore};

    fn entry(id: &str, title: &str, body: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            tags: Vec::new(),
            created_at: "t".to_string(),
            updated_at: "t".to_string(),
            archived_at: None,
            key: None,
            kind: None,
        }
    }

    #[test]
    fn empty_hits_inject_nothing() {
        assert!(render_recall_block(std::iter::empty::<(MemoryEntry, f64)>()).is_none());
    }

    #[test]
    fn block_has_header_titles_and_bodies() {
        let hits = vec![
            (entry("a", "Build target", "install to ~/.cargo/bin"), 3.0),
            (entry("b", "Concurrency", "never git stash"), 2.0),
        ];
        let block = render_recall_block(hits.into_iter()).expect("some block");
        assert!(block.contains("Relevant memory"), "{block}");
        assert!(block.contains("verify against the current code"), "{block}");
        assert!(
            block.contains("Build target: install to ~/.cargo/bin"),
            "{block}"
        );
        assert!(block.contains("Concurrency: never git stash"), "{block}");
    }

    #[test]
    fn char_budget_drops_overflow_but_keeps_first() {
        let big = "x".repeat(RECALL_CHAR_BUDGET);
        let hits = vec![
            (entry("a", "first", &big), 3.0),
            (entry("b", "second", "should be dropped"), 2.0),
        ];
        let block = render_recall_block(hits.into_iter()).expect("some block");
        assert!(
            block.contains("first"),
            "first must survive: {}",
            &block[..80]
        );
        assert!(
            !block.contains("should be dropped"),
            "second must be dropped"
        );
    }

    #[test]
    fn retrieval_then_render_surfaces_the_relevant_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        store
            .remember(entry(
                "install",
                "Install target",
                "cargo bin at ~/.cargo/bin/leveler",
            ))
            .unwrap();
        store
            .remember(entry("cook", "Cooking", "boil pasta for nine minutes"))
            .unwrap();

        let hits = store
            .search("where does install put the binary", RECALL_K)
            .unwrap();
        let block = render_recall_block(hits.into_iter().filter(|(_, s)| *s >= RECALL_FLOOR))
            .expect("the install memory should be retrieved");
        assert!(block.contains("Install target"), "{block}");
        assert!(
            !block.contains("Cooking"),
            "unrelated memory must not leak: {block}"
        );
    }
}

#[cfg(test)]
mod compaction_tests {
    use super::dispatch::task_needs_structured_plan;

    use crate::authorization::{extract_command, patch_paths, push_unique_path};
    use crate::compaction::{ACTIVE_OBJECTIVE_MARKER, compact_messages, estimate_tokens};
    use leveler_core::ToolCallId;
    use leveler_model::{ContentPart, Message, Role, ToolCall, ToolResultContent};

    #[test]
    fn estimate_tokens_counts_images_not_as_free() {
        use leveler_model::{ContentPart, ImageSource};
        let with_image = vec![Message {
            role: Role::User,
            content: vec![
                ContentPart::Text {
                    text: "look".to_string(),
                },
                ContentPart::Image {
                    source: ImageSource::Url {
                        url: "https://x/y.png".to_string(),
                    },
                },
            ],
        }];
        let text_only = vec![Message {
            role: Role::User,
            content: vec![ContentPart::Text {
                text: "look".to_string(),
            }],
        }];
        // An image must add real weight, so a vision turn can trigger compaction
        // even when the gateway reports no usage.
        assert!(estimate_tokens(&with_image) >= 256);
        assert!(estimate_tokens(&with_image) > estimate_tokens(&text_only));
    }

    #[test]
    fn extract_command_drops_duplicate_program_arg() {
        let call = ToolCall {
            id: ToolCallId::new("c"),
            name: "run_command".to_string(),
            arguments: serde_json::json!({
                "program": "pytest",
                "args": ["pytest", "tests/providers/test_retry_classification.py", "-q"]
            }),
        };

        let (program, args) = extract_command(&call);

        assert_eq!(program.as_deref(), Some("pytest"));
        assert_eq!(
            args,
            vec!["tests/providers/test_retry_classification.py", "-q"]
        );
    }

    #[test]
    fn patch_paths_extracts_files_from_apply_patch_headers() {
        let paths = patch_paths(
            "*** Begin Patch\n\
             *** Add File: src/new.rs\n\
             *** Update File: src/lib.rs\n\
             *** Move to: crates/app/src/main.rs\n\
             *** End Patch",
        );

        assert_eq!(
            paths,
            vec!["src/new.rs", "src/lib.rs", "crates/app/src/main.rs"]
        );
    }

    #[test]
    fn push_unique_path_rejects_unsafe_paths() {
        let mut paths = Vec::new();

        push_unique_path(&mut paths, "./src/lib.rs");
        push_unique_path(&mut paths, "src/lib.rs");
        push_unique_path(&mut paths, "../secret");
        push_unique_path(&mut paths, "/tmp/secret");

        assert_eq!(paths, vec!["src/lib.rs"]);
    }

    #[test]
    fn long_multi_concern_request_needs_a_structured_plan() {
        let task = "先检查为什么项目规则没有生效，而且任务执行时间异常。还要把计划实时展示出来，验证失败不要突然倾倒整屏日志。最后跑完整测试并编译本地版本。";

        assert!(task_needs_structured_plan(task));
        assert!(!task_needs_structured_plan(
            "把 README 的标题改成 CodeLeveler"
        ));
    }

    #[test]
    fn url_output_labels_do_not_make_a_simple_request_complex() {
        let task = concat!(
            " - Local: http://localhost:3000\n",
            " - Network: http://10.9.9.199:3000 这些URL 为什么在TUI上能不能让他可点击。"
        );

        assert!(!task_needs_structured_plan(task));
        assert!(task_needs_structured_plan(
            "1. inspect the current implementation\n\
             2. change the behavior"
        ));
        assert!(!task_needs_structured_plan(
            "- inspect the current implementation\n\
             - change the behavior"
        ));
        assert!(task_needs_structured_plan(
            "- inspect the current implementation\n\
             - change the behavior\n\
             - run verification"
        ));
    }

    #[test]
    fn advisory_model_calls_have_a_short_independent_deadline() {}

    fn assistant_call(name: &str, path: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new("c"),
                    name: name.to_string(),
                    arguments: serde_json::json!({ "path": path }),
                },
            }],
        }
    }

    fn tool_result(text: &str) -> Message {
        Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                result: ToolResultContent {
                    call_id: ToolCallId::new("c"),
                    content: text.to_string(),
                    is_error: false,
                },
            }],
        }
    }

    /// System + task + 8 (assistant/tool) rounds.
    fn long_transcript() -> Vec<Message> {
        let mut m = vec![
            Message::text(Role::System, "you are an agent"),
            Message::text(Role::User, "fix the bug"),
        ];
        for i in 0..8 {
            let file = format!("src/mod{i}.rs");
            m.push(assistant_call("read_file", &file));
            m.push(tool_result("... file contents ..."));
        }
        m
    }

    /// Scoped project rules ride in mid-transcript system messages. They are
    /// persistent constraints, not elidable history: compaction must carry them
    /// forward, or the agent silently loses a directory's AGENTS.md and the
    /// injection tracker never re-adds it.
    #[test]
    fn compaction_carries_scoped_rules_out_of_the_elided_middle() {
        let mut msgs = vec![
            Message::text(Role::System, "you are an agent"),
            Message::text(Role::User, "fix the bug"),
            Message::text(
                Role::System,
                "Project rules:\n--- from src/AGENTS.md ---\nno unwrap",
            ),
        ];
        for i in 0..8 {
            msgs.push(assistant_call("read_file", &format!("src/mod{i}.rs")));
            msgs.push(tool_result("... file contents ..."));
        }

        let out = compact_messages(&msgs, 4, 0, None, None);
        assert!(out.len() < msgs.len(), "should shrink");
        assert!(
            out.iter()
                .any(|m| m.text_content().contains("from src/AGENTS.md")),
            "scoped rule was dropped by compaction"
        );
    }

    #[test]
    fn keeps_anchors_and_recent_and_reduces_length() {
        let msgs = long_transcript();
        let out = compact_messages(&msgs, 4, 0, None, Some("fix the bug"));

        assert!(
            out.len() < msgs.len(),
            "should shrink: {} -> {}",
            msgs.len(),
            out.len()
        );
        // Anchors: system first, original task second, then host objective pin.
        assert_eq!(out[0].role, Role::System);
        assert_eq!(out[1].role, Role::User);
        assert_eq!(out[1].text_content(), "fix the bug");
        assert!(
            out[2].text_content().contains(ACTIVE_OBJECTIVE_MARKER),
            "host objective pin missing: {}",
            out[2].text_content()
        );
        // Breadcrumb replaces the middle and names an elided file.
        assert_eq!(out[3].role, Role::User);
        assert!(
            out[3].text_content().contains("compacted"),
            "{}",
            out[3].text_content()
        );
        assert!(
            out[3].text_content().contains("src/mod0.rs"),
            "{}",
            out[3].text_content()
        );
        // The last 4 messages are preserved verbatim.
        assert_eq!(&out[out.len() - 4..], &msgs[msgs.len() - 4..]);
    }

    #[test]
    fn compaction_repins_active_objective_not_first_user_only() {
        // Multi-turn head: first user is an old question; host objective is new.
        let mut msgs = vec![
            Message::text(Role::System, "sys"),
            Message::text(Role::User, "how many uncommitted files?"),
            Message::text(Role::Assistant, "about 66"),
            Message::text(Role::User, "update docs/ARCHITECTURE.md for the runtime"),
        ];
        for i in 0..10 {
            msgs.push(assistant_call("list_files", &format!("p{i}")));
            msgs.push(tool_result("ok"));
        }
        let out = compact_messages(
            &msgs,
            4,
            0,
            None,
            Some("update docs/ARCHITECTURE.md for the runtime"),
        );
        assert!(out.len() < msgs.len());
        let pin = out
            .iter()
            .find(|m| m.text_content().contains(ACTIVE_OBJECTIVE_MARKER))
            .expect("objective pin");
        assert!(
            pin.text_content().contains("ARCHITECTURE")
                && pin.text_content().contains("<objective>"),
            "pin must carry host objective: {}",
            pin.text_content()
        );
        // First user history may remain in head, but pin is the active SoT.
        assert!(out.iter().any(|m| m.text_content().contains("uncommitted")));
    }

    #[test]
    fn tail_never_starts_on_an_orphan_tool_result() {
        let msgs = long_transcript();
        // keep_recent=3 would land the tail on a Tool result; it must back up.
        let out = compact_messages(&msgs, 3, 0, None, None);
        // system + first user + breadcrumb → index of first tail message.
        let first_after_breadcrumb = out
            .iter()
            .position(|m| m.text_content().contains("compacted"))
            .map(|i| &out[i + 1])
            .expect("breadcrumb");
        assert_ne!(
            first_after_breadcrumb.role,
            Role::Tool,
            "tail must not begin with an orphaned tool result"
        );
    }

    #[test]
    fn short_transcript_is_left_untouched_without_objective() {
        let msgs = vec![
            Message::text(Role::System, "sys"),
            Message::text(Role::User, "hi"),
            assistant_call("read_file", "a.rs"),
            tool_result("x"),
        ];
        assert_eq!(compact_messages(&msgs, 4, 0, None, None), msgs);
    }

    #[test]
    fn short_transcript_gets_objective_pin_when_missing() {
        let msgs = vec![
            Message::text(Role::System, "sys"),
            Message::text(Role::User, "old ask"),
            assistant_call("read_file", "a.rs"),
            tool_result("x"),
        ];
        let out = compact_messages(&msgs, 4, 0, None, Some("new objective only"));
        assert!(
            out.iter()
                .any(|m| m.text_content().contains(ACTIVE_OBJECTIVE_MARKER)
                    && m.text_content().contains("new objective only")),
            "short transcript must still receive host pin: {out:?}"
        );
    }
}
