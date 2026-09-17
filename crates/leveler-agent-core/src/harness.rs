//! The seams a host plugs into the loop, and the per-run state the loop
//! shares with it.
//!
//! [`AgentHarness`] is the whole contract between the kernel and whatever
//! embeds it. The kernel calls each seam at a fixed point of every round; the
//! harness answers with a [`Flow`]. Every seam except the two that name the
//! tools and run them has a neutral default, so a plain tool-calling agent is
//! [`BasicHarness`] over a [`ToolRuntime`] and nothing else.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_model::{
    FinishReason, Message, ModelError, ModelErrorKind, TokenUsage, ToolCall, ToolDefinition,
};

use crate::error::AgentCoreError;
use crate::event::AgentEvent;
use crate::limits::{RoundAdmission, RoundAdmissionInput, RoundLimits, admit_next_round};
use crate::model_round::ModelRound;
use crate::stop::{LoopStop, StopReason};
use crate::tool_runtime::{ToolRuntime, dispatch_calls};
use crate::usage::UsageProjection;

/// What the loop does after a seam returns.
#[derive(Debug)]
pub enum Flow<S> {
    /// Proceed with this round as the loop would on its own.
    Continue,
    /// Skip the rest of this round and start the next one. The harness has
    /// already put whatever it wanted the model to see into the transcript.
    NextRound,
    /// End the run with the harness's own outcome.
    Stop(S),
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// The loop's own state for one run, read and (for spend) extended by the
/// harness. The kernel owns the round counter, the limits, the usage
/// projection, the wall clock, and the cancellation token every seam and
/// tool runs under.
pub struct LoopContext {
    round: u32,
    limits: RoundLimits,
    usage: UsageProjection,
    started: Instant,
    /// The run's token: a child of the caller's, also cancelled by the
    /// deadline timer. Every model stream, tool, and wait uses this one.
    cancellation: CancellationToken,
    deadline_expired: Arc<AtomicBool>,
    _deadline_guard: CancelOnDrop,
    last_text: String,
}

impl LoopContext {
    pub(crate) fn new(limits: RoundLimits, external: CancellationToken) -> Self {
        let run_cancellation = external.child_token();
        let deadline_expired = Arc::new(AtomicBool::new(false));
        let deadline_done = CancellationToken::new();
        let guard = CancelOnDrop(deadline_done.clone());
        if let Some(max) = limits.max_duration {
            let remaining = max.saturating_sub(limits.spent_before.duration);
            let expired = Arc::clone(&deadline_expired);
            let deadline_token = run_cancellation.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = external.cancelled() => {}
                    _ = deadline_done.cancelled() => {}
                    _ = tokio::time::sleep(remaining) => {
                        expired.store(true, Ordering::Release);
                        deadline_token.cancel();
                    }
                }
            });
        }
        Self {
            round: 0,
            limits,
            usage: UsageProjection::default(),
            started: Instant::now(),
            cancellation: run_cancellation,
            deadline_expired,
            _deadline_guard: guard,
            last_text: String::new(),
        }
    }

    /// Rounds started so far. Zero before the first model call; the counter
    /// advances when a round is admitted, before its model call.
    pub fn round(&self) -> u32 {
        self.round
    }

    pub fn limits(&self) -> &RoundLimits {
        &self.limits
    }

    /// Whether the pinned window limit leaves room for a round after the
    /// current one.
    pub fn has_next_round(&self) -> bool {
        self.limits.allows_round_after(self.round)
    }

    /// The token every model stream, tool, and wait in this run observes.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Whether the run's own deadline timer cancelled it (as opposed to the
    /// caller).
    pub fn deadline_expired(&self) -> bool {
        self.deadline_expired.load(Ordering::Acquire)
    }

    pub fn usage(&self) -> &UsageProjection {
        &self.usage
    }

    /// Tokens measured against the cap: prior spend plus this run's.
    pub fn model_tokens_spent(&self) -> u64 {
        self.limits
            .spent_before
            .model_tokens
            .saturating_add(self.usage.admission_model_tokens())
    }

    /// Cost measured against the cap: prior spend plus this run's.
    pub fn cost_spent_micros(&self) -> u64 {
        self.limits
            .spent_before
            .cost_usd_micros
            .saturating_add(self.usage.admission_cost_usd_micros())
    }

    /// Wall clock measured against the cap: prior spend plus this run's.
    pub fn elapsed(&self) -> Duration {
        self.limits
            .spent_before
            .duration
            .saturating_add(self.started.elapsed())
    }

    /// When this run started.
    pub fn run_started(&self) -> Instant {
        self.started
    }

    /// Fold a model call the harness made on its own account (a fold's
    /// summary, a delegated child's call) into the spend the limits read.
    /// The loop folds its own rounds automatically.
    pub fn record_spend(
        &mut self,
        usage: TokenUsage,
        cost_usd_micros: Option<u64>,
        estimated_tokens: Option<u64>,
    ) {
        self.usage.record(usage, cost_usd_micros, estimated_tokens);
    }

    /// The most recent non-empty assistant text.
    pub fn last_text(&self) -> &str {
        &self.last_text
    }

    pub(crate) fn admit(&self) -> RoundAdmission {
        admit_next_round(&RoundAdmissionInput {
            round: self.round,
            round_ceiling: self.limits.round_ceiling,
            window_round_limit: self.limits.window_round_limit,
            model_tokens_spent: self.model_tokens_spent(),
            max_model_tokens: self.limits.max_model_tokens,
            cost_spent_micros: self.cost_spent_micros(),
            max_cost_usd_micros: self.limits.max_cost_usd_micros,
            elapsed: self.elapsed(),
            max_duration: self.limits.max_duration,
            cancelled: self.cancellation.is_cancelled(),
            deadline_expired: self.deadline_expired(),
        })
    }

    pub(crate) fn advance_round(&mut self) {
        self.round = self.round.saturating_add(1);
    }

    pub(crate) fn note_text(&mut self, text: &str) {
        if !text.trim().is_empty() {
            self.last_text = text.to_string();
        }
    }

    pub(crate) fn stop(&self, reason: StopReason, rounds: u32, messages: Vec<Message>) -> LoopStop {
        LoopStop {
            reason,
            rounds,
            last_text: self.last_text.clone(),
            messages,
        }
    }
}

/// The seams the loop calls, in round order:
///
/// 1. [`on_round_start`](Self::on_round_start) — before the loop decides
///    whether another round may start.
/// 2. [`on_round_admitted`](Self::on_round_admitted) — a round will start;
///    last chance to shape the transcript before the request is built.
/// 3. [`tool_definitions`](Self::tool_definitions) — what the request
///    advertises.
/// 4. [`on_model_error`](Self::on_model_error) — the model round failed
///    after the loop's own retries.
/// 5. [`on_response`](Self::on_response) — the model answered; its spend is
///    already folded. Decide whether this response stands.
/// 6. [`on_quiet`](Self::on_quiet) — the response carried no tool calls.
/// 7. [`execute_calls`](Self::execute_calls) — the response carried tool
///    calls; run them and append their results.
/// 8. [`on_stop`](Self::on_stop) — the loop ended on a neutral reason; map
///    it onto the host's outcome.
#[async_trait]
pub trait AgentHarness: Send {
    /// The host's outcome type for a finished run.
    type Stop: Send;
    /// The host's error type. Kernel errors convert into it.
    type Error: From<AgentCoreError> + Send;

    /// A neutral event from the loop. Default: dropped.
    fn on_event(&mut self, _event: AgentEvent) {}

    /// The tool definitions the next request advertises.
    fn tool_definitions(&self) -> Vec<ToolDefinition>;

    /// Round top, before admission. Inject anything that must reach the model
    /// before it is asked (host input, settled background work).
    async fn on_round_start(
        &mut self,
        _ctx: &mut LoopContext,
        _messages: &mut Vec<Message>,
    ) -> Result<Flow<Self::Stop>, Self::Error> {
        Ok(Flow::Continue)
    }

    /// A round was admitted and the counter advanced; the request is built
    /// from `messages` when this returns.
    async fn on_round_admitted(
        &mut self,
        _ctx: &mut LoopContext,
        _messages: &mut Vec<Message>,
    ) -> Result<Flow<Self::Stop>, Self::Error> {
        Ok(Flow::Continue)
    }

    /// The model round failed after the loop's own retries. `Continue` or
    /// `NextRound` re-runs the round with whatever the harness appended;
    /// `Stop` ends the run. Default: the error aborts the run.
    async fn on_model_error(
        &mut self,
        _ctx: &mut LoopContext,
        error: AgentCoreError,
        _messages: &mut Vec<Message>,
    ) -> Result<Flow<Self::Stop>, Self::Error> {
        Err(error.into())
    }

    /// The model answered. `Continue` lets the loop append the assistant
    /// message and proceed to [`on_quiet`](Self::on_quiet) or
    /// [`execute_calls`](Self::execute_calls); `NextRound` discards it (the
    /// harness has appended what it wants instead). The default treats a
    /// truncated, filtered, unknown, or tool-less `tool_calls` finish as a
    /// model error.
    async fn on_response(
        &mut self,
        _ctx: &mut LoopContext,
        round: &ModelRound,
        _messages: &mut Vec<Message>,
    ) -> Result<Flow<Self::Stop>, Self::Error> {
        let error = match round.finish_reason {
            FinishReason::Length => Some(ModelError::new(
                ModelErrorKind::Truncated,
                "model output ended at the token limit",
            )),
            FinishReason::ContentFilter => Some(ModelError::new(
                ModelErrorKind::ContentFiltered,
                "provider content filtering stopped the response before a complete answer",
            )),
            FinishReason::Other => Some(ModelError::new(
                ModelErrorKind::Other,
                "provider returned an unknown terminal finish reason",
            )),
            FinishReason::ToolCalls if round.tool_calls().is_empty() => Some(ModelError::new(
                ModelErrorKind::Decode,
                "provider reported tool_calls but supplied no complete tool call",
            )),
            FinishReason::Stop | FinishReason::ToolCalls => None,
        };
        match error {
            Some(error) => Err(AgentCoreError::Model(error).into()),
            None => Ok(Flow::Continue),
        }
    }

    /// The response carried no tool calls. The assistant message is already
    /// in `messages`; `assistant` is the same message for the harness to
    /// record. `Continue` ends the run with [`StopReason::ModelEnd`];
    /// `NextRound` asks the model again (the harness appended a prompt).
    async fn on_quiet(
        &mut self,
        _ctx: &mut LoopContext,
        _assistant: Message,
        _messages: &mut Vec<Message>,
    ) -> Result<Flow<Self::Stop>, Self::Error> {
        Ok(Flow::Continue)
    }

    /// The response carried tool calls. The assistant message is already in
    /// `messages`; the harness runs the calls and appends their results in
    /// call order. `Continue` and `NextRound` both start the next round.
    async fn execute_calls(
        &mut self,
        ctx: &mut LoopContext,
        assistant: Message,
        calls: Vec<ToolCall>,
        messages: &mut Vec<Message>,
    ) -> Result<Flow<Self::Stop>, Self::Error>;

    /// The loop ended on a neutral reason. Map it onto the host's outcome —
    /// or into an error, when that is what the host reports for it.
    async fn on_stop(
        &mut self,
        ctx: &mut LoopContext,
        stop: LoopStop,
    ) -> Result<Self::Stop, Self::Error>;
}

/// A plain tool-calling agent: every call goes to one [`ToolRuntime`] in
/// order, events go to one callback, and the run ends where the model stops.
pub struct BasicHarness<T: ToolRuntime> {
    tools: T,
    events: Box<dyn FnMut(AgentEvent) + Send>,
}

impl<T: ToolRuntime> BasicHarness<T> {
    pub fn new(tools: T) -> Self {
        Self {
            tools,
            events: Box::new(|_| {}),
        }
    }

    /// Receive the loop's events.
    pub fn with_events(mut self, events: impl FnMut(AgentEvent) + Send + 'static) -> Self {
        self.events = Box::new(events);
        self
    }

    pub fn tools(&self) -> &T {
        &self.tools
    }
}

#[async_trait]
impl<T: ToolRuntime> AgentHarness for BasicHarness<T> {
    type Stop = LoopStop;
    type Error = AgentCoreError;

    fn on_event(&mut self, event: AgentEvent) {
        (self.events)(event);
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.definitions()
    }

    async fn execute_calls(
        &mut self,
        ctx: &mut LoopContext,
        _assistant: Message,
        calls: Vec<ToolCall>,
        messages: &mut Vec<Message>,
    ) -> Result<Flow<Self::Stop>, Self::Error> {
        let results =
            dispatch_calls(&self.tools, calls, ctx.cancellation(), &mut *self.events).await?;
        messages.push(results);
        Ok(Flow::Continue)
    }

    async fn on_stop(
        &mut self,
        _ctx: &mut LoopContext,
        stop: LoopStop,
    ) -> Result<Self::Stop, Self::Error> {
        Ok(stop)
    }
}
