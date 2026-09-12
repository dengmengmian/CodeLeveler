//! The state-driven single-agent tool loop.

pub mod closeout;
mod dispatch;
mod drive;
mod handlers;
pub use handlers::DelegatedChildResult;
pub(crate) mod host;

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent_core::BudgetExhaustion;
use leveler_context::load_rules;
use leveler_engine::{
    ChildToolEvent, CompactionCheckpoint, EventBarrier, ExecutionFence, ModelRequestRecord,
    PortError, TranscriptSink,
};
use leveler_execution::{
    ApprovalPolicy, Approver, AutoApprove, AutoClarify, AutoReviewer, ClarificationRequest,
    Clarifier, ClarifyOutcome, NeedUserReviewer,
};
use leveler_lifecycle::{
    EvidenceLedger, ObjectiveAnchor, PlanState, PlanStep, ProgressLedger, StopReason,
    VerificationStatus,
};
use leveler_memory::MemoryStore;
use leveler_model::{
    ContentPart, Message, ModelError, ModelPricing, ModelRef, ModelRuntime, ReasoningEffort, Role,
};
use leveler_tools::{ToolContext, ToolRegistry};
use leveler_verifier::CheckStatus;

use self::dispatch::text_of;
use crate::nudges::first_user_text;
use crate::prompt::{PromptBuilder, TurnContext};
use crate::sub_agent::{AgentRole, DEFAULT_MAX_CONCURRENT_AGENTS, DEFAULT_MAX_TOTAL_AGENTS};

/// Secondary summarization/audit requests improve quality but must never make
/// an otherwise finished turn look hung for minutes.
///
/// Memory recall (tail injection). Each turn the lasting preferences plus the
/// memories matching this request are rendered as one system block right before
/// the (always-new) user message. The cached system+history prefix stays
/// untouched — the block rides the uncached tail — and it is never persisted
/// (`run_conversation` filters `System` roles), so it stays fresh and never
/// accumulates.
const RECALL_K: usize = 4;
/// Minimum BM25 score to inject a hit — `search` only returns positive matches,
/// so this just drops the weakest ties.
const RECALL_FLOOR: f64 = 0.1;
/// Hard ceiling on the WHOLE rendered block: header, labels, ids, titles,
/// bodies and the omission marker.
///
/// Named in bytes because that is what is actually counted and what the
/// previous `CHAR_BUDGET` was measuring anyway. It is a ceiling, not a target:
/// the old check exempted the first entry (`&& used > 0`), so one long memory
/// could push the block past any limit on its own.
const RECALL_BLOCK_MAX_BYTES: usize = 2048;
/// How many lasting preferences ride along unconditionally. Bounded because
/// they are paid for on EVERY turn, relevant or not.
const STANDING_PREFERENCE_MAX_ENTRIES: usize = 8;

/// What recall decided for one turn, and what it cost.
///
/// Built so the decision can be inspected instead of inferred from a string:
/// before this the selection happened inline while rendering, and nothing
/// could answer "which memories did that turn actually use, and what was
/// dropped for space".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct MemoryRecallPlan {
    /// Lasting preferences, newest first, injected whatever the request says.
    pub standing: Vec<leveler_memory::MemoryEntry>,
    /// Matches for this request, best score first, already free of anything
    /// standing covers.
    pub queried: Vec<leveler_memory::MemoryEntry>,
    /// Ids in the order they were rendered.
    pub selected: Vec<String>,
    /// Entries the byte ceiling left out entirely.
    pub omitted: usize,
    /// Entries whose body was cut to fit.
    pub truncated: usize,
    /// Size of the rendered block, or 0 when nothing was injected.
    pub rendered_bytes: usize,
}

impl MemoryRecallPlan {
    /// Select and render. Selection order IS the priority: standing
    /// preferences first because they apply to every turn, then query hits by
    /// score.
    fn build(
        standing: Vec<leveler_memory::MemoryEntry>,
        queried: Vec<leveler_memory::MemoryEntry>,
    ) -> (Self, Option<String>) {
        let mut plan = Self {
            standing: standing.clone(),
            queried: queried.clone(),
            ..Default::default()
        };
        const HEADER: &str = "## Project memory for this turn\nMemory is advisory and records \
             what was true when it was written. The code and the project's own rules win: verify \
             anything it names before relying on it, and correct it when this turn contradicts \
             it.\n";
        const STANDING_LABEL: &str =
            "\nLasting preferences (apply unless this turn says otherwise):\n";
        const QUERY_LABEL: &str = "\nRetrieved as possibly relevant to this request:\n";

        let mut budget = RECALL_BLOCK_MAX_BYTES.saturating_sub(HEADER.len());
        // Reserve room for the omission marker up front, so admitting a final
        // entry can never make the truthful "N omitted" line unaffordable.
        budget = budget.saturating_sub(48);
        let mut standing_body = String::new();
        let mut query_body = String::new();

        for (entries, out, label) in [
            (standing, &mut standing_body, STANDING_LABEL),
            (queried, &mut query_body, QUERY_LABEL),
        ] {
            let mut label_paid = false;
            for entry in entries {
                let label_cost = if label_paid { 0 } else { label.len() };
                let (line, was_truncated) =
                    render_entry_line(&entry, budget.saturating_sub(label_cost));
                match line {
                    Some(line) => {
                        if !label_paid {
                            budget = budget.saturating_sub(label.len());
                            label_paid = true;
                        }
                        budget = budget.saturating_sub(line.len());
                        plan.selected.push(entry.id.clone());
                        plan.truncated += usize::from(was_truncated);
                        out.push_str(&line);
                    }
                    // Not even a truncated form fits: leave it out and say so.
                    None => plan.omitted += 1,
                }
            }
        }

        if standing_body.is_empty() && query_body.is_empty() {
            return (plan, None);
        }
        let mut block = String::from(HEADER);
        if !standing_body.is_empty() {
            block.push_str(STANDING_LABEL);
            block.push_str(&standing_body);
        }
        if !query_body.is_empty() {
            block.push_str(QUERY_LABEL);
            block.push_str(&query_body);
        }
        if plan.omitted > 0 {
            block.push_str(&format!("\n({} more omitted for space.)\n", plan.omitted));
        }
        plan.rendered_bytes = block.len();
        (plan, Some(block))
    }
}

/// One rendered entry line within `budget` bytes, plus whether its body was
/// cut. `None` when even the shortest useful form does not fit.
///
/// The id is what makes a turn auditable: without it nobody can say which
/// memory was used.
fn render_entry_line(entry: &leveler_memory::MemoryEntry, budget: usize) -> (Option<String>, bool) {
    let prefix = format!("- [{}] {}: ", entry.id.trim(), entry.title.trim());
    let body = entry.body.trim();
    let full = format!("{prefix}{body}\n");
    if full.len() <= budget {
        return (Some(full), false);
    }
    // Keep the id and title, cut the body — an entry the model can look up is
    // worth more than a silent omission. `…\n` needs 4 bytes.
    let room = budget.saturating_sub(prefix.len() + 4);
    if room == 0 {
        return (None, false);
    }
    // Cut on a char boundary: half a UTF-8 sequence is not a shorter string,
    // it is a corrupt one.
    let mut cut = room.min(body.len());
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    if cut == 0 {
        return (None, false);
    }
    (Some(format!("{prefix}{}…\n", &body[..cut])), true)
}

/// Ids for a trace line: bounded in count and length so one oversized legacy
/// id cannot turn a debug line into a dump.
fn trace_ids(ids: &[String]) -> Vec<String> {
    ids.iter()
        .take(12)
        .map(|id| id.chars().take(40).collect())
        .collect()
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
        /// Canonical unified diff of what an edit tool ACTUALLY changed, with
        /// the line numbers it landed on. Only the execution layer knows
        /// these: an `apply_patch` hunk is located by content and `replace`
        /// matches a substring, so neither position is declared in the request.
        /// `None` for every non-edit tool, and for an edit whose location
        /// could not be established — a presenter must then show no line
        /// numbers rather than derive one from what the model asked for.
        applied_diff: Option<String>,
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
    /// The model updated its structured plan via the `update_plan` tool. The
    /// full step list replaces any previous plan (not a delta).
    PlanUpdated { steps: Vec<PlanStep> },
    /// Exact message list the next model request will see. Emitted at a round
    /// boundary so crash recovery does not reconstruct a different context.
    ContextSnapshot { messages: Vec<Message> },
    /// Post-edit verification started.
    VerificationStarted,
    /// One post-edit verification check finished.
    ///
    /// Carries the verifier's own [`CheckStatus`] rather than a second enum
    /// with the same meaning: two spellings of one fact is how `toolmissing`
    /// reached a durable row whose contract said `tool_missing`.
    VerificationCheck {
        name: String,
        status: CheckStatus,
        evidence: Option<String>,
    },
    /// Post-edit verification finished.
    ///
    /// `passed` is the completion gate, not a verification result: it is
    /// `true` for a run that owed no check and therefore proved nothing.
    /// `verification` is what the checks actually said, and any consumer
    /// answering "did this pass" must read that. `None` on rows written
    /// before the split, where only the gate was recorded.
    VerificationFinished {
        passed: bool,
        verification: Option<VerificationStatus>,
    },
    /// A sub-agent was spawned and began working (concurrent delegation).
    SubAgentStarted {
        id: String,
        nickname: String,
        role: String,
        task: String,
        /// Built-in capability contract. Absent on events written before
        /// Child Profile existed (`None` means "not recorded", not "no profile").
        /// It used to be followed by a list of semantic capability labels;
        /// what consumers needed was the structural fact below.
        profile_id: Option<String>,
        profile_role: Option<String>,
        /// Whether this child holds a physically read-only toolset.
        read_only: bool,
    },
    /// One model call made by a sub-agent, carrying the child's id so the
    /// parent can persist it. Boxed because this variant is much larger than
    /// the rest and `AgentEvent` is cloned on every hop.
    SubAgentModelRequest { record: Box<ModelRequestRecord> },
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
    /// Durable ownership provenance for a child's write scope: `action` is
    /// `ownership_granted` / `ownership_denied`, `detail` names the owner and
    /// the paths. Recorded so an offline audit can tell an authorized write
    /// from a bypass. Facts only: never a gate, never completion-relevant.
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
}

impl AdvisoryKind {
    /// Stable key for crossing the (serialized) engine event boundary.
    pub fn as_key(&self) -> &'static str {
        match self {
            AdvisoryKind::ContextCompaction => "context_compaction",
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
            _ => {
                let reason = closeout::CloseoutReason::from_key(key.strip_prefix("nudge_")?)?;
                Some(AdvisoryKind::CloseoutNudge(reason))
            }
        }
    }
}

#[cfg(test)]
mod workspace_listing_tests {
    /// A huge repository must not turn the cached prefix into the largest
    /// thing in the request, and a shortened listing has to SAY it is short:
    /// silently truncated, it reads as the complete contents of the workspace.
    #[test]
    fn a_large_workspace_is_capped_and_says_so() {
        let dir = std::env::temp_dir().join(format!("leveler-listing-cap-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        for i in 0..400 {
            std::fs::write(
                dir.join("src")
                    .join(format!("a_rather_long_file_name_{i:04}.rs")),
                "fn f() {}\n",
            )
            .unwrap();
        }
        let listing = super::workspace_listing(&dir).expect("a non-empty workspace lists");
        assert!(
            listing.len() <= 8 * 1024 + 64,
            "capped: {} bytes",
            listing.len()
        );
        assert!(listing.contains("truncated"), "a short listing must say so");
        assert!(
            listing.lines().all(|l| !l.is_empty()),
            "no ragged half-path at the cut"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A workspace with nothing readable produces no listing at all, so the
    /// prompt renders no section rather than an empty one.
    #[test]
    fn an_empty_workspace_produces_no_listing() {
        let dir =
            std::env::temp_dir().join(format!("leveler-listing-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(super::workspace_listing(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
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
    pub budget_exhaustion: Option<BudgetExhaustion>,
    /// Final progress / closeout state (engine continue_active_goal reads this).
    pub progress: ProgressLedger,
    /// Active objective used for this drive (host-pinned).
    pub objective: ObjectiveAnchor,
}

impl AgentOutcome {
    /// Build a drive outcome from the fields that vary per exit plus the drive's
    /// running state (`progress`/`objective`, always cloned here).
    /// Every early return in `drive` funnels through this, so a new shared field
    /// is added once here instead of at each of the ~10 return sites.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn drive_result(
        final_text: String,
        rounds: u32,
        modified_files: Vec<String>,
        stop_reason: StopReason,
        stop_detail: Option<String>,
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
        exhaustion: BudgetExhaustion,
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

impl From<PortError> for AgentError {
    /// A lifecycle port refused. Both cases abort the run: the engine could
    /// not make a fact durable, or this runtime no longer owns the task.
    fn from(error: PortError) -> Self {
        match error {
            PortError::Persistence(detail) => AgentError::Persistence(detail),
            PortError::StaleOwnership(detail) => AgentError::StaleOwnership(detail),
        }
    }
}

impl From<leveler_agent_core::AgentCoreError> for AgentError {
    /// The kernel's neutral failures, in this crate's vocabulary. A tool
    /// runtime never fails here — this harness dispatches its own tools — so
    /// that arm exists only to keep the mapping total.
    fn from(error: leveler_agent_core::AgentCoreError) -> Self {
        use leveler_agent_core::AgentCoreError as Kernel;
        match error {
            Kernel::Model(error) => AgentError::Model(error),
            Kernel::Cancelled => AgentError::Cancelled,
            Kernel::InvalidLimits(reason) => AgentError::InvalidBudget(reason),
            Kernel::ToolRuntime(error) => AgentError::Persistence(error.to_string()),
        }
    }
}

/// Where a bounded advisory call reports what it cost.
///
/// These calls are made by free functions that have no transcript sink, and
/// several of their paths spend tokens and then return an error — a reply the
/// judge could not parse is still a reply the provider billed. Returning the
/// cost would lose exactly those; a collector the call pushes into before it
/// returns does not.
pub type AdvisorySpend = Vec<ModelRequestRecord>;

/// A sink that discards everything (non-persistent runs, tests).
pub struct NoopSink;

#[async_trait]
impl TranscriptSink for NoopSink {
    async fn append(&mut self, _messages: &[Message]) -> Result<(), PortError> {
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
    async fn append(&mut self, _messages: &[Message]) -> Result<(), PortError> {
        Ok(())
    }

    async fn record_model_request(&mut self, record: &ModelRequestRecord) -> Result<(), PortError> {
        self.input_tokens = self.input_tokens.saturating_add(record.usage.input_tokens);
        self.output_tokens = self
            .output_tokens
            .saturating_add(record.usage.output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(record.usage.cached_input_tokens);
        // The counters above drive the live progress line. They are transient:
        // when the child ends they are gone, which is why a reviewer that
        // spent half a million tokens left no durable row. Send the record
        // itself as well, stamped with this child's id, so the parent — which
        // does hold a persistence sink — can write it down.
        let _ = self.events.send(AgentEvent::SubAgentModelRequest {
            record: Box::new(ModelRequestRecord {
                agent_id: Some(self.id.clone()),
                ..record.clone()
            }),
        });
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

/// The execution-policy slice a delegated executor needs. The engine resolves
/// one value per role from model facts, task facts, and eval-only overrides;
/// the agent loop only consumes the already-resolved values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubAgentExecutionPolicy {
    pub max_parallel_tools: usize,
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
    /// Max read-only tools executed concurrently within one round's parallel
    /// batch (0 = unbounded).
    pub max_parallel_tools: usize,
    /// Per-request reasoning effort selected by the execution-policy resolver.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// The usable context window in tokens (0 = disabled). When the last
    /// request's reported token count exceeds this, the in-memory transcript is
    /// compacted before the next round so a long task never overflows.
    pub context_budget: u32,
    /// Persist the exact model context after EVERY round (`ContextSnapshot`),
    /// not only when it diverges from the durable transcript. Measurement
    /// seam for `leveler eval` (context-cost attribution reads those rows);
    /// production leaves it off — a per-round copy of a derivable context is
    /// O(rounds × context) of near-duplicate log rows.
    pub context_trace: bool,

    // ── Completion ──────────────────────────────────────────────────────────
    /// The run ends only when the model explicitly calls
    /// `update_goal(complete|blocked)`. Going quiet does not finish it.
    pub goal_mode: bool,

    // ── Delegation ──────────────────────────────────────────────────────────
    /// When false, `spawn_agent` is not advertised (delegation kill-switch).
    pub allow_delegation: bool,
    /// Max sub-agents running at once (within a spawn batch).
    pub max_concurrent_agents: usize,
    /// Max sub-agents spawned across the whole top-level run.
    pub max_total_agents: usize,
}

impl Default for TurnPolicy {
    fn default() -> Self {
        Self {
            max_parallel_tools: 0,
            reasoning_effort: None,
            context_budget: 0,
            context_trace: false,
            goal_mode: false,
            allow_delegation: true,
            max_concurrent_agents: DEFAULT_MAX_CONCURRENT_AGENTS,
            max_total_agents: DEFAULT_MAX_TOTAL_AGENTS,
        }
    }
}

impl TurnPolicy {
    /// The minimal direct policy: the mechanical loop guards off too. Safety
    /// is untouched — admission (hooks/rules/approval), the side-effect
    /// barrier, step limits, the absolute round ceiling, and cancellation
    /// apply exactly as always.
    pub fn minimal() -> Self {
        Self { ..Self::default() }
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
    /// Optional host-provided objective (overrides first-user fallback).
    seeded_objective: Option<ObjectiveAnchor>,
    /// Short memory INDEX for cache-stable system injection (titles only).
    memory_catalog: String,
    /// Whether memory reaches the model at all (tools, index, recall, guidance).
    memory_expose: bool,
    /// Where durable project memory lives, for the RUNTIME's own reads: the
    /// per-turn recall injection, and parking a `remember` proposal that no
    /// human was there to approve. `None` = memory unconfigured.
    ///
    /// The harness needs this in its own right, so it holds it in its own
    /// right. It used to read the memory tools' root out of
    /// `ToolContext.services`, which made a tool's capability handle do double
    /// duty as runtime state.
    memory_root: Option<std::path::PathBuf>,
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

/// A bounded listing of the workspace for the system prompt.
///
/// Reuses `leveler_context::RepositoryMap` — the same walk, ignore list and
/// 400-file cap that already existed — rather than growing a second one. The
/// byte cap on top of it is what keeps a huge repository from turning the
/// cached prefix into the largest thing in the request; a truncated listing
/// says so, because a silently short list would read as a complete one.
fn workspace_listing(root: &std::path::Path) -> Option<String> {
    const MAX_BYTES: usize = 8 * 1024;
    let rendered = leveler_context::RepositoryMap::build(root).render();
    if rendered.trim().is_empty() {
        return None;
    }
    if rendered.len() <= MAX_BYTES {
        return Some(rendered);
    }
    let cut = leveler_core::floor_char_boundary(&rendered, MAX_BYTES);
    let kept = rendered[..cut]
        .rsplit_once('\n')
        .map(|(head, _)| head)
        .unwrap_or("");
    Some(format!("{kept}\n… [listing truncated]"))
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
            seeded_objective: None,
            memory_catalog: String::new(),
            memory_expose: false,
            memory_root: None,
            step_limits: StepLimits::default(),
            permission_rules: std::sync::RwLock::new(
                leveler_execution::PermissionRuleSet::default(),
            ),
            permission_rules_path: None,
            hook_runner: leveler_execution::HookRunner::empty(
                leveler_core::environment().current_dir().to_path_buf(),
            ),
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

    /// Short INDEX lines injected into the system prompt (bodies never go here).
    /// Whether memory is exposed to the model this turn. Gates the index, the
    /// prompt guidance and automatic recall together with the tools.
    pub fn with_memory_expose(mut self, expose: bool) -> Self {
        self.memory_expose = expose;
        self
    }

    pub fn with_memory_catalog(mut self, catalog: impl Into<String>) -> Self {
        self.memory_catalog = catalog.into();
        self
    }

    /// Where the runtime reads durable memory for recall injection, and parks
    /// a `remember` proposal nobody was available to approve.
    pub fn with_memory_root(mut self, root: Option<std::path::PathBuf>) -> Self {
        self.memory_root = root;
        self
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
                max_parallel_tools: if profile.serial_tools() {
                    1
                } else {
                    self.policy.max_parallel_tools
                },
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
                max_parallel_tools: child_policy.max_parallel_tools,
                reasoning_effort: child_policy.reasoning_effort,
                // …the rest is inherited or deliberately reset for a child.
                context_budget: self.policy.context_budget,
                // Sub-agents keep the static behavior in S3 v1: a child's
                // transcript is short-lived and expansion evidence is a
                // top-level task concern.
                context_trace: self.policy.context_trace,
                max_concurrent_agents: self.policy.max_concurrent_agents,
                max_total_agents: self.policy.max_total_agents,
                // Children never advertise spawn_agent (depth already blocks it).
                allow_delegation: false,
                // A sub-agent finishes when it goes quiet; only the top-level
                // run uses explicit goal resolution.
                goal_mode: false,
                // A child inherits the parent's product-guard stance.
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
            seeded_objective: None,
            memory_catalog: String::new(),
            memory_expose: self.memory_expose,
            // A child inherits the parent's memory location: recall and
            // parking mean the same thing at any depth.
            memory_root: self.memory_root.clone(),
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

    /// Persist the model context after every round instead of only when it
    /// diverges from the transcript (eval measurement seam; see
    /// [`TurnPolicy::context_trace`]).
    pub fn with_context_trace(mut self, trace: bool) -> Self {
        self.policy.context_trace = trace;
        self
    }

    /// Product kill-switch: when false, `spawn_agent` is not in the tool list.
    pub fn with_delegation(mut self, allow: bool) -> Self {
        self.policy.allow_delegation = allow;
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

    /// Bound the round's concurrent read-only tool batch to
    /// `max_parallel_tools` (0 = unbounded).
    pub fn with_execution_controls(mut self, max_parallel_tools: usize) -> Self {
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
                mode: self.tool_context.policy.mode(),
                network_allowed: self.approval_policy.network_allowed,
                deny_network: self.tool_context.policy.network_denied(),
                cwd: self.tool_context.execution.workspace.root().to_path_buf(),
                project_rules,
                user_language: crate::prompt::user_language(request),
                repo_map: workspace_listing(self.tool_context.execution.workspace.root()),
            })
            .memory_catalog(self.memory_catalog.clone())
            .memory_expose(self.memory_expose)
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
                 the file), reserving `correctness` for a defect that must be fixed before \
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

    /// Ask the model to write a handoff briefing for the rounds compaction is
    /// about to elide. Returns None when there is nothing to fold or the call
    /// fails — the caller then folds with a bare breadcrumb rather than aborting
    /// the run, because an unsummarized fold still beats overflowing the window.
    /// The breadcrumb says the details are lost, so the loss is never silent.
    pub(crate) async fn summarize_for_compaction(
        &self,
        messages: &[Message],
        keep_recent: usize,
        keep_recent_tokens: u64,
        cancellation: &CancellationToken,
    ) -> Option<leveler_context::CompactionSummary> {
        leveler_context::summarize_with_model(
            self.runtime.as_ref(),
            &self.model,
            self.policy.reasoning_effort,
            messages,
            keep_recent,
            keep_recent_tokens,
            cancellation,
        )
        .await
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

    /// The memory block for THIS turn, or `None` when memory is not exposed,
    /// unconfigured, or nothing was selected.
    ///
    /// Owns the whole decision: capability gate, standing selection, query
    /// recall, derived/sensitive exclusion, dedup, ordering, the byte ceiling
    /// and the trace. Callers push the result as a `Role::System` message
    /// immediately before the user message, so the cached prefix survives and
    /// the block is stripped next turn.
    fn relevant_memory_injection(&self, request: &str) -> Option<String> {
        if !self.memory_expose {
            tracing::debug!(memory_exposed = false, "memory recall skipped");
            return None;
        }
        let root = self.memory_root.as_ref()?;
        let store = match MemoryStore::open(root) {
            Ok(store) => store,
            Err(error) => {
                // Never silently swallowed: a store that cannot be opened is
                // the difference between "no memories" and "memory broken".
                tracing::debug!(error = %error, "memory store unavailable for recall");
                return None;
            }
        };
        let standing = store
            .standing_preferences(STANDING_PREFERENCE_MAX_ENTRIES)
            .unwrap_or_else(|error| {
                tracing::debug!(error = %error, "standing preferences unavailable");
                Vec::new()
            });
        // `recall`, not `search`: repository-derived facts are read from the
        // repository, and sensitive entries are withheld from the model.
        let queried: Vec<leveler_memory::MemoryEntry> = store
            .recall(request, RECALL_K)
            .unwrap_or_else(|error| {
                tracing::debug!(error = %error, "query recall unavailable");
                Vec::new()
            })
            .into_iter()
            .filter(|(_, score)| *score >= RECALL_FLOOR)
            .map(|(entry, _)| entry)
            // A preference injected unconditionally is not paid for twice.
            .filter(|entry| !standing.iter().any(|s| s.id == entry.id))
            .collect();

        let (plan, block) = MemoryRecallPlan::build(standing, queried);
        // Ids and counts only. Titles and bodies stay out: a trace is not a
        // place to spill what the store was careful about.
        tracing::debug!(
            memory_exposed = true,
            standing_count = plan.standing.len(),
            query_hit_count = plan.queried.len(),
            selected_count = plan.selected.len(),
            omitted_count = plan.omitted,
            truncated_count = plan.truncated,
            rendered_bytes = plan.rendered_bytes,
            selected_ids = ?trace_ids(&plan.selected),
            "memory recall"
        );
        block
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
    use super::{
        MemoryRecallPlan, RECALL_BLOCK_MAX_BYTES, RECALL_FLOOR, RECALL_K,
        STANDING_PREFERENCE_MAX_ENTRIES,
    };
    use leveler_memory::{MemoryEntry, MemoryKind, MemoryStore};

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

    fn render(
        standing: Vec<MemoryEntry>,
        queried: Vec<MemoryEntry>,
    ) -> (super::MemoryRecallPlan, Option<String>) {
        MemoryRecallPlan::build(standing, queried)
    }

    #[test]
    fn nothing_selected_injects_nothing() {
        let (plan, block) = render(Vec::new(), Vec::new());
        assert!(block.is_none());
        assert_eq!(plan.rendered_bytes, 0);
        assert!(plan.selected.is_empty());
    }

    /// The block carries entry ids, which is what makes a turn auditable.
    #[test]
    fn the_block_carries_ids_titles_and_bodies() {
        let (plan, block) = render(
            Vec::new(),
            vec![
                entry("a", "Build target", "install to ~/.cargo/bin"),
                entry("b", "Concurrency", "never git stash"),
            ],
        );
        let block = block.expect("some block");
        assert!(block.contains("Project memory for this turn"), "{block}");
        assert!(block.contains("verify anything it names"), "{block}");
        assert!(
            block.contains("[a] Build target: install to ~/.cargo/bin"),
            "{block}"
        );
        assert!(
            block.contains("[b] Concurrency: never git stash"),
            "{block}"
        );
        assert_eq!(plan.selected, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(plan.omitted, 0);
        assert_eq!(plan.truncated, 0);
    }

    /// The ceiling binds even on the FIRST entry. The old check exempted it
    /// (`&& used > 0`), so one long memory could blow past any limit alone.
    #[test]
    fn a_single_oversized_entry_cannot_exceed_the_ceiling() {
        let huge = "x".repeat(RECALL_BLOCK_MAX_BYTES * 3);
        let (plan, block) = render(Vec::new(), vec![entry("big", "first", &huge)]);
        let block = block.expect("a truncated form still reaches the model");
        assert!(
            block.len() <= RECALL_BLOCK_MAX_BYTES,
            "block was {} bytes",
            block.len()
        );
        assert_eq!(plan.rendered_bytes, block.len());
        assert_eq!(plan.truncated, 1, "the cut is reported, not hidden");
        assert!(block.contains('…'), "a cut must be visible: {block}");
        assert!(block.contains("[big]"), "the id survives so it can be read");
    }

    /// Cutting must not split a UTF-8 sequence.
    #[test]
    fn truncation_never_splits_a_character() {
        let body = "紧凑输出".repeat(RECALL_BLOCK_MAX_BYTES);
        let (_, block) = render(Vec::new(), vec![entry("cjk", "中文", &body)]);
        let block = block.expect("block");
        assert!(block.len() <= RECALL_BLOCK_MAX_BYTES);
        // Rust strings cannot hold invalid UTF-8, so the real check is that no
        // replacement character was produced and the text still ends cleanly.
        assert!(!block.contains('\u{FFFD}'), "corrupt character in {block}");
        assert!(block.contains('…'));
    }

    /// Entries that do not fit are counted, not quietly dropped.
    #[test]
    fn overflow_is_reported_truthfully() {
        let big = "y".repeat(RECALL_BLOCK_MAX_BYTES - 200);
        let (plan, block) = render(
            Vec::new(),
            vec![
                entry("first", "first", &big),
                entry("second", "second", &"z".repeat(4000)),
                entry("third", "third", &"w".repeat(4000)),
            ],
        );
        let block = block.expect("block");
        assert!(block.len() <= RECALL_BLOCK_MAX_BYTES);
        assert!(plan.omitted + plan.truncated >= 1);
        if plan.omitted > 0 {
            assert!(
                block.contains(&format!("({} more omitted for space.)", plan.omitted)),
                "{block}"
            );
        }
    }

    /// Standing preferences are rendered before query hits: they apply to
    /// every turn, so they are the ones worth the space.
    #[test]
    fn standing_preferences_come_first() {
        let (plan, block) = render(
            vec![entry("pref", "偏好", "保持紧凑")],
            vec![entry("hit", "命中", "与本轮相关")],
        );
        let block = block.expect("block");
        assert_eq!(plan.selected, vec!["pref".to_string(), "hit".to_string()]);
        assert!(
            block.find("[pref]").unwrap() < block.find("[hit]").unwrap(),
            "{block}"
        );
        assert!(block.contains("Lasting preferences"), "{block}");
        assert!(block.contains("Retrieved as possibly relevant"), "{block}");
    }

    /// A lasting preference must reach the model even when the request shares
    /// no characters with it — the case lexical recall provably misses.
    #[test]
    fn a_standing_preference_is_injected_without_any_lexical_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        store
            .activate(
                "输出偏好",
                "用户偏好紧凑的终端信息输出，不希望看到冗长的模型过程。",
                MemoryKind::Preference,
                vec![],
            )
            .unwrap();

        let query = "能不能精简一点";
        assert!(
            store.recall(query, RECALL_K).unwrap().is_empty(),
            "precondition: lexical recall misses this paraphrase"
        );
        let standing = store
            .standing_preferences(STANDING_PREFERENCE_MAX_ENTRIES)
            .unwrap();
        let (_, block) = render(standing, Vec::new());
        let block = block.expect("the preference must reach the model anyway");
        assert!(block.contains("紧凑"), "{block}");
    }

    /// Retrieval still finds a relevant entry and leaves the rest alone.
    #[test]
    fn retrieval_surfaces_the_relevant_entry_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        store
            .activate(
                "Install target",
                "cargo bin at ~/.cargo/bin/leveler",
                MemoryKind::Note,
                vec![],
            )
            .unwrap();
        store
            .activate(
                "Cooking",
                "boil pasta for nine minutes",
                MemoryKind::Note,
                vec![],
            )
            .unwrap();
        let hits: Vec<MemoryEntry> = store
            .recall("where does install put the binary", RECALL_K)
            .unwrap()
            .into_iter()
            .filter(|(_, s)| *s >= RECALL_FLOOR)
            .map(|(e, _)| e)
            .collect();
        let (_, block) = render(Vec::new(), hits);
        let block = block.expect("the install memory should be retrieved");
        assert!(block.contains("Install target"), "{block}");
        assert!(
            !block.contains("Cooking"),
            "unrelated memory leaked: {block}"
        );
    }

    /// An entry chosen by both lanes is injected once.
    #[test]
    fn an_entry_in_both_lanes_is_injected_once() {
        let pref = entry("terse", "Terse output", "keep the terminal compact");
        let standing = vec![pref.clone()];
        let queried: Vec<MemoryEntry> = vec![pref]
            .into_iter()
            .filter(|e| !standing.iter().any(|s| s.id == e.id))
            .collect();
        let (plan, block) = render(standing, queried);
        let block = block.expect("block");
        assert_eq!(block.matches("[terse]").count(), 1, "{block}");
        assert_eq!(plan.selected.len(), 1);
    }
}

#[cfg(test)]
mod compaction_tests {
    use crate::authorization::{extract_command, patch_paths, push_unique_path};
    use leveler_context::{ACTIVE_OBJECTIVE_MARKER, compact_messages, estimate_tokens};
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

#[cfg(test)]
mod child_accounting_tests {
    use super::*;
    use leveler_engine::ModelCallKind;
    use leveler_model::{FinishReason, TokenUsage};

    fn a_record() -> ModelRequestRecord {
        ModelRequestRecord {
            provider_request_id: Some("req-1".to_string()),
            provider: "deepseek".to_string(),
            model: "deepseek-v4-flash".to_string(),
            usage: TokenUsage {
                input_tokens: 1_000,
                output_tokens: 100,
                cached_input_tokens: 900,
            },
            finish_reason: FinishReason::Stop,
            latency_ms: 10,
            retry_count: 0,
            kind: ModelCallKind::Round,
            agent_id: None,
            cost_usd_micros: None,
        }
    }

    /// A child cannot borrow the parent's persistence sink, so its records
    /// have to travel as events. The sink used to keep only in-memory counters
    /// and emit a progress line, which is why a reviewer's half-million tokens
    /// left no row anywhere.
    #[tokio::test]
    async fn a_child_sink_emits_the_record_stamped_with_its_agent_id() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = SubAgentProgressSink::new("reviewer-6d8ab312".to_string(), tx);

        sink.record_model_request(&a_record()).await.unwrap();

        let mut durable = None;
        let mut progress = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::SubAgentModelRequest { record } => durable = Some(record),
                AgentEvent::SubAgentProgress { .. } => progress = Some(()),
                _ => {}
            }
        }
        let durable = durable.expect("the child must emit a durable record, not only a counter");
        assert_eq!(durable.agent_id.as_deref(), Some("reviewer-6d8ab312"));
        assert_eq!(durable.usage.input_tokens, 1_000);
        assert_eq!(durable.usage.cached_input_tokens, 900);
        assert!(progress.is_some(), "the live progress line still goes out");
    }

    /// Two calls are two records. A sink that only accumulated could not tell
    /// one expensive call from ten cheap ones.
    #[tokio::test]
    async fn every_child_call_produces_its_own_record() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = SubAgentProgressSink::new("reviewer-a".to_string(), tx);

        sink.record_model_request(&a_record()).await.unwrap();
        sink.record_model_request(&a_record()).await.unwrap();

        let mut records = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, AgentEvent::SubAgentModelRequest { .. }) {
                records += 1;
            }
        }
        assert_eq!(records, 2);
    }

    /// Pricing is applied once, against the usage the provider reported, and
    /// an unpriced model yields `None` rather than a free call.
    #[test]
    fn a_record_is_priced_from_the_usage_it_carries() {
        let priced = a_record().priced(Some(&leveler_model::ModelPricing {
            input_usd_per_mtok: 1.0,
            output_usd_per_mtok: 2.0,
            cached_input_usd_per_mtok: Some(0.1),
        }));
        // 100 uncached × 1.0 + 900 cached × 0.1 + 100 out × 2.0
        assert_eq!(priced.cost_usd_micros, Some(100 + 90 + 200));
        assert_eq!(a_record().priced(None).cost_usd_micros, None);
    }
}

#[cfg(test)]
mod reviewer_policy_tests {
    use crate::executor::handlers::CHILD_SETTLEMENT_RESERVE;
    use crate::sub_agent::SUB_AGENT_MAX_DURATION;
    use std::time::Duration;

    /// R6/R7. A child is a tail, not a claim on the deadline: it gets the
    /// parent's remainder minus what settlement needs.
    #[test]
    fn a_child_never_receives_the_parents_whole_remainder() {
        let residual = Duration::from_secs(300);
        let granted = SUB_AGENT_MAX_DURATION.min(residual.saturating_sub(CHILD_SETTLEMENT_RESERVE));
        assert!(granted < residual, "settlement must keep some of the tail");
        assert_eq!(granted, Duration::from_secs(240));
    }

    /// R5. Near the deadline the grant shrinks with the remainder, and once
    /// the reserve is all that is left the child gets nothing rather than
    /// eating the parent's ability to finish.
    #[test]
    fn a_parent_near_its_deadline_grants_a_child_nothing() {
        for residual in [
            Duration::from_secs(60),
            Duration::from_secs(30),
            Duration::ZERO,
        ] {
            let granted =
                SUB_AGENT_MAX_DURATION.min(residual.saturating_sub(CHILD_SETTLEMENT_RESERVE));
            assert_eq!(
                granted,
                Duration::ZERO,
                "with {residual:?} left there is nothing to lend"
            );
        }
    }

    /// R4. With plenty of time the profile cap still binds — the reserve is a
    /// floor on the parent's side, not a new ceiling on the child's.
    #[test]
    fn a_parent_with_plenty_of_time_still_caps_the_child_at_its_profile() {
        let residual = Duration::from_secs(6 * 3600);
        let granted = SUB_AGENT_MAX_DURATION.min(residual.saturating_sub(CHILD_SETTLEMENT_RESERVE));
        assert_eq!(granted, SUB_AGENT_MAX_DURATION);
    }
}
