//! One model round: stream a request, assemble the assistant message, retry
//! the same request on retryable failures.
//!
//! This lifecycle owns all retries; provider transport performs one physical
//! invocation, and every completed attempt is settled before another send.

use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use leveler_model::{
    ContentPart, DeliveryState, FinishReason, Message, ModelError, ModelErrorKind, ModelEvent,
    ModelRequest, ModelRuntime, ReasoningSegment, Role, StreamProgress, TokenUsage, ToolCall,
    TransportFault,
};

use crate::error::AgentCoreError;
use crate::event::AgentEvent;

/// A completed model round: the assembled assistant message plus the facts a
/// host records about the call.
#[derive(Debug, Clone)]
pub struct ModelRound {
    /// The provider's request id, when it reported one.
    pub request_id: String,
    pub message: Message,
    pub usage: TokenUsage,
    pub finish_reason: FinishReason,
    pub latency_ms: u64,
    /// Wall-clock duration of the attempt whose output was used. `latency_ms`
    /// minus this is retry and backoff overhead.
    pub attempt_ms: u64,
    /// Request start to response headers.
    pub connect_ms: u64,
    /// Request start to the first generated text, reasoning, or tool data.
    /// Local startup and usage-only events do not establish a first token.
    pub ttft_ms: Option<u64>,
    /// Longest stretch inside the stream with no event at all. A frozen screen
    /// is this number, not the total.
    pub max_event_gap_ms: u64,
    /// Retries before this outcome. One round is one logical call, so the
    /// physical traffic behind it is `1 + retry_count`.
    pub retry_count: u32,
    /// Cost in micro-USD, priced by the loop against the usage the provider
    /// reported (cached share included) when the agent carries pricing.
    /// `None` means unpriced, never free.
    pub cost_usd_micros: Option<u64>,
    /// The transcript estimate that stood in for usage when the provider
    /// reported none; `None` when usage was reported.
    pub estimated_tokens: Option<u64>,
    /// Displayable reasoning segments the model produced this round, each with
    /// the runtime's own measured duration, in the order they were produced.
    /// Empty when the model reasoned nothing. A host records this on the
    /// durable assistant-message projection so a reopened transcript can
    /// render a completed Thought; it never enters the model context.
    pub reasoning_segments: Vec<ReasoningSegment>,
}

impl ModelRound {
    /// The tool calls the model requested this round, in content order.
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::ToolCall { call } => Some(call.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Automatic retries after the first attempt fails transiently. The logical
/// request is therefore attempted at most `1 + MAX_RETRIES` times.
///
/// This is the sole owner of a model request's retry lifecycle. Provider
/// transport performs one physical attempt, so every chargeable invocation
/// passes through the same admission and settlement hooks.
pub const MAX_RETRIES: u32 = 10;

/// A connection the OS refused is not an upstream transient: nothing is
/// listening, and no 30-second wait changes that. It gets a short, fast budget
/// so a wrong `base_url` fails in about two seconds with a clear error instead
/// of after ~3 minutes of backoff. Local services that are still starting up
/// still recover, because the first retries are quick. Every other failure
/// keeps the full schedule.
const IMMEDIATE_ENDPOINT_MAX_RETRIES: u32 = 3;
const IMMEDIATE_ENDPOINT_SCHEDULE_MS: [u64; IMMEDIATE_ENDPOINT_MAX_RETRIES as usize] =
    [250, 500, 1_000];

/// Whether this failure is a machine-local "nothing accepted the connection"
/// answer that will not resolve on a long retry cadence. Read from the
/// structured fault, never from the message.
fn is_immediate_endpoint_failure(error: &ModelError) -> bool {
    error.delivery_state == DeliveryState::NotSent
        && error.transport_fault() == Some(TransportFault::ConnectionRefused)
}

/// Backoff before retry `retry` (1-based). Quick first recoveries, then a
/// steady 30s rate so a real outage is not hammered; a provider-advertised
/// `Retry-After` (capped) overrides the schedule entirely.
fn scheduled_backoff(retry: u32) -> Duration {
    const SCHEDULE_MS: [u64; MAX_RETRIES as usize] = [
        1_000, 2_000, 4_000, 8_000, 15_000, 30_000, 30_000, 30_000, 30_000, 30_000,
    ];
    let idx = (retry as usize)
        .saturating_sub(1)
        .min(SCHEDULE_MS.len() - 1);
    Duration::from_millis(SCHEDULE_MS[idx])
}

/// Delay before retry `retry` (1-based). A provider-advertised `Retry-After`
/// wins (capped); otherwise the fixed exponential schedule. Never returns a
/// value the presented countdown would disagree with: the loop waits exactly
/// this long.
pub(crate) fn retry_backoff_delay(error: &ModelError, retry: u32) -> Duration {
    const MAX_ADVERTISED: Duration = Duration::from_secs(120);
    if let Some(ms) = error.retry_after_ms {
        return Duration::from_millis(ms).min(MAX_ADVERTISED);
    }
    scheduled_backoff(retry)
}

/// Cheap 0–20% jitter (no `rand` dependency) so N concurrent runs hitting the
/// same outage do not retry in lockstep.
fn jittered(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    base + base.mul_f64((nanos % 200) as f64 / 1000.0)
}

/// How many retries a round may spend, and how long it waits between them.
/// Production uses the real time schedule; a test injects a zero-delay policy
/// so a ten-retry exhaustion is exercised without waiting out the backoff.
#[derive(Clone, Copy)]
pub(crate) struct RetryPolicy {
    pub max_retries: u32,
    /// Multiplies every computed delay. `0.0` makes retries immediate.
    pub delay_scale: f64,
}

impl RetryPolicy {
    pub(crate) fn production() -> Self {
        Self {
            max_retries: MAX_RETRIES,
            delay_scale: 1.0,
        }
    }

    /// How many retries this failure may spend. A refused connection gets the
    /// short endpoint budget; everything else gets the configured budget.
    fn max_retries_for(&self, error: &ModelError) -> u32 {
        if is_immediate_endpoint_failure(error) {
            self.max_retries.min(IMMEDIATE_ENDPOINT_MAX_RETRIES)
        } else {
            self.max_retries
        }
    }

    fn delay(&self, error: &ModelError, retry: u32) -> Duration {
        if self.delay_scale <= 0.0 {
            return Duration::ZERO;
        }
        let base = if is_immediate_endpoint_failure(error) {
            let idx = (retry as usize)
                .saturating_sub(1)
                .min(IMMEDIATE_ENDPOINT_SCHEDULE_MS.len() - 1);
            Duration::from_millis(IMMEDIATE_ENDPOINT_SCHEDULE_MS[idx])
        } else {
            retry_backoff_delay(error, retry)
        };
        // A provider-advertised wait is honored exactly: jitter must never
        // retry BEFORE the window the server named. Only our own schedule is
        // jittered, to spread concurrent recoveries.
        let delay = if error.retry_after_ms.is_some() {
            base
        } else {
            jittered(base)
        };
        delay.mul_f64(self.delay_scale)
    }
}

/// Whether the logical round may spend another bounded attempt.
///
/// The rule itself lives with the delivery truth, in
/// [`ModelError::is_retryable_by_lifecycle`]: this lifecycle owns recovery for
/// a whole model request, so it may re-send any transient failure that received
/// no response content. An upstream that swallowed the
/// request and never returned response headers is exactly such a failure, and
/// it must not take the task down.
///
/// Nothing that produced output slips through: [`stream_round`] rewrites every
/// failure seen inside a stream to `StreamInterrupted` with its progress
/// attached, and a stream that produced text or tool-call arguments is never
/// replayed.
fn should_retry_round(error: &ModelError) -> bool {
    error.is_retryable_by_lifecycle()
}

/// Stream one round, retrying the SAME request after a safe delivery failure or
/// a bounded pre-stream/send transport failure. Non-retryable errors and
/// cancellation propagate immediately. Each attempt starts with an explicit
/// [`AgentEvent::StreamAttemptStarted`] so retries can stream a divergent
/// prefix without corrupting consumers.
pub async fn run_model_round(
    runtime: &dyn ModelRuntime,
    request: ModelRequest,
    cancellation: &CancellationToken,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
) -> Result<ModelRound, AgentCoreError> {
    run_model_round_with(
        runtime,
        request,
        cancellation,
        on_event,
        RetryPolicy::production(),
    )
    .await
}

/// The retry budget is a parameter so a test can exercise the full ten-retry
/// lifecycle without waiting out the real backoff. Production always passes
/// [`RetryPolicy::production`].
async fn run_model_round_with(
    runtime: &dyn ModelRuntime,
    request: ModelRequest,
    cancellation: &CancellationToken,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
    policy: RetryPolicy,
) -> Result<ModelRound, AgentCoreError> {
    run_model_round_observed_with(
        runtime,
        request,
        cancellation,
        &mut EventObserver(on_event),
        policy,
    )
    .await
}

/// Attempt settlement is awaited before a retry may start. The host can make
/// usage durable here without buffering until a logical round succeeds.
#[async_trait::async_trait]
pub trait ModelRoundObserver: Send {
    type Error: From<AgentCoreError> + Send;
    /// Re-read host admission immediately before every physical send, including
    /// after retry backoff when concurrent work may have spent shared budget.
    async fn before_attempt(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn on_event(&mut self, event: AgentEvent);
    async fn on_attempt(&mut self, attempt: leveler_model::ModelAttempt)
    -> Result<(), Self::Error>;
}

struct EventObserver<'a>(&'a mut (dyn FnMut(AgentEvent) + Send));
#[async_trait::async_trait]
impl ModelRoundObserver for EventObserver<'_> {
    type Error = AgentCoreError;
    fn on_event(&mut self, event: AgentEvent) {
        (self.0)(event);
    }
    async fn on_attempt(&mut self, _: leveler_model::ModelAttempt) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub async fn run_model_round_observed<O: ModelRoundObserver>(
    runtime: &dyn ModelRuntime,
    request: ModelRequest,
    cancellation: &CancellationToken,
    observer: &mut O,
) -> Result<ModelRound, O::Error> {
    run_model_round_observed_with(
        runtime,
        request,
        cancellation,
        observer,
        RetryPolicy::production(),
    )
    .await
}

async fn run_model_round_observed_with<O: ModelRoundObserver>(
    runtime: &dyn ModelRuntime,
    request: ModelRequest,
    cancellation: &CancellationToken,
    observer: &mut O,
    policy: RetryPolicy,
) -> Result<ModelRound, O::Error> {
    let mut retries = 0u32;
    let started = std::time::Instant::now();
    loop {
        if cancellation.is_cancelled() {
            return Err(AgentCoreError::Cancelled.into());
        }
        if request
            .deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            return Err(AgentCoreError::Model(ModelError::new(
                ModelErrorKind::Timeout,
                "model request deadline elapsed",
            ))
            .into());
        }
        observer.before_attempt().await?;
        observer.on_event(AgentEvent::StreamAttemptStarted);
        let attempt_started = std::time::Instant::now();
        let mut observed_usage = None;
        let mut output_estimate = leveler_model::TokenEstimate::new();
        let mut diagnostics = AttemptDiagnostics::default();
        let outcome = stream_round(
            runtime,
            request.clone(),
            cancellation,
            &mut |event| observer.on_event(event),
            &mut observed_usage,
            &mut output_estimate,
            &mut diagnostics,
        )
        .await;
        let projected_input_tokens = request
            .projection
            .as_ref()
            .map(|p| p.estimated_tokens())
            .unwrap_or_else(|| {
                leveler_model::estimate_tokens(&request.messages)
                    + leveler_model::estimate_tool_definitions(&request.tools)
            });
        let error = match &outcome {
            Err(AgentCoreError::Model(error)) => Some(error.clone()),
            Err(AgentCoreError::Cancelled) => Some(ModelError::cancelled()),
            Err(error) => Some(ModelError::new(ModelErrorKind::Other, error.to_string())),
            Ok(_) => None,
        };
        let not_sent = error
            .as_ref()
            .is_some_and(|error| error.delivery_state == DeliveryState::NotSent);
        observer
            .on_attempt(leveler_model::ModelAttempt {
                request_id: request.request_id.to_string(),
                attempt: retries + 1,
                usage: observed_usage,
                finish_reason: diagnostics.finish_reason,
                partial_text: (outcome.is_err() && !diagnostics.observed_text.is_empty())
                    .then(|| std::mem::take(&mut diagnostics.observed_text)),
                error,
                latency_ms: attempt_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                cost_usd_micros: not_sent.then_some(0),
                connect_ms: diagnostics.connect_ms,
                ttft_ms: diagnostics.ttft_ms,
                max_event_gap_ms: diagnostics.max_event_gap_ms,
                estimated_tokens: (!not_sent && observed_usage.is_none_or(|u| u.total() == 0))
                    .then(|| {
                        projected_input_tokens.saturating_add(
                            outcome
                                .as_ref()
                                .ok()
                                .map(|r| {
                                    leveler_model::estimate_tokens(std::slice::from_ref(&r.message))
                                })
                                .unwrap_or_else(|| output_estimate.tokens()),
                        )
                    }),
                projected_input_tokens,
            })
            .await?;
        let error = match outcome {
            Ok(mut value) => {
                value.retry_count = retries;
                value.latency_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                if retries > 0 {
                    // The recovery half of the lifecycle, so a support log can
                    // tell a blip that healed from one that did not.
                    tracing::info!(
                        request_id = %request.request_id,
                        retries,
                        elapsed_ms = value.latency_ms,
                        "model round recovered after retries"
                    );
                }
                return Ok(value);
            }
            // Cancellation is surfaced as its own error, never retried.
            Err(e) if cancellation.is_cancelled() => return Err(e.into()),
            Err(AgentCoreError::Model(e)) => e,
            Err(e) => return Err(e.into()),
        };
        // Safe delivery failures and the narrow pre-stream/send transport
        // exception are retried. A `Caution` error (the provider may already
        // have generated/billed output, e.g. a stream cut after partial text)
        // is never blind-replayed: there is no protocol-level stream resume.
        if !should_retry_round(&error) {
            // The decision is half the evidence here. When the round *does*
            // retry, `model round retrying` records the same fields; without a
            // line on this path, "why was nothing retried?" could only be
            // answered by reading the source.
            tracing::warn!(
                request_id = %request.request_id,
                kind = ?error.kind,
                delivery = ?error.delivery_state,
                retryability = ?error.retryability(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %error,
                "model round failed terminally without a retry"
            );
            return Err(AgentCoreError::Model(error).into());
        }
        if retries >= policy.max_retries_for(&error) {
            // The budget is spent. Surface the last failure instead of hiding
            // it behind an unbounded wait: whether to try again is the
            // person's decision, and the count travels with the error.
            let mut error = error;
            error.retry_attempts = Some(retries);
            tracing::warn!(
                request_id = %request.request_id,
                retries,
                kind = ?error.kind,
                delivery = ?error.delivery_state,
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %error,
                "model round retries exhausted"
            );
            return Err(AgentCoreError::Model(error).into());
        }
        retries += 1;
        let max_retries = policy.max_retries_for(&error);
        let wait = policy.delay(&error, retries);
        // The retry controller emits this; presentation must never drive it.
        observer.on_event(AgentEvent::ModelRetrying {
            attempt: retries,
            max_attempts: max_retries,
            delay_ms: wait.as_millis() as u64,
        });
        // A silent retry re-sends the whole (often huge) request and looks
        // identical to a hang from the outside. Name it.
        tracing::warn!(
            request_id = %request.request_id,
            retry = retries,
            max_retries = max_retries,
            kind = ?error.kind,
            delivery = ?error.delivery_state,
            retryability = ?error.retryability(),
            wait_ms = wait.as_millis() as u64,
            elapsed_ms = started.elapsed().as_millis() as u64,
            error = %error,
            "model round retrying"
        );
        // Cancellable: a user Ctrl+C during a long backoff must not hang until
        // the timer fires.
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AgentCoreError::Cancelled.into()),
            _ = wait_for_deadline(request.deadline) => return Err(AgentCoreError::Model(ModelError::new(ModelErrorKind::Timeout,"model request deadline elapsed during retry backoff")).into()),
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// Mark a stream-phase failure with the delivery truth the assembler can
/// prove: the response stream began (`StreamInterrupted`) and had produced (or
/// not) assistant text and/or tool-call arguments before the cut.
///
/// Only transport-shaped kinds are upgraded — a protocol `Decode` or
/// `Truncated` keeps its own kind and its own (terminal) retry policy, and its
/// delivery is not claimed to be an interruption.
fn interrupted_error(
    mut error: ModelError,
    text: bool,
    reasoning: bool,
    tool_args: bool,
) -> ModelError {
    if matches!(
        error.kind,
        ModelErrorKind::StreamInterrupted | ModelErrorKind::Transport | ModelErrorKind::Timeout
    ) {
        error.delivery_state = DeliveryState::StreamInterrupted {
            progress: StreamProgress {
                text,
                reasoning,
                tool_args,
            },
        };
    }
    error
}

async fn wait_for_deadline(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

/// Close an open reasoning segment at a real stream boundary. `None` is a
/// no-op, so every boundary can call it unconditionally.
///
/// The segment lifecycle is synthesized here, not carried by the provider:
/// [`ModelEvent`] normalizes reasoning *content* only, and this is the one
/// place that observes the boundaries between reasoning and the answer or a
/// tool call. The closed segment is recorded with its measured duration so a
/// host can persist one displayable Thought per segment.
fn close_reasoning_segment(
    segment: &mut Option<(std::time::Instant, String)>,
    segments: &mut Vec<ReasoningSegment>,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
) {
    if let Some((started, text)) = segment.take() {
        let duration_ms = started.elapsed().as_millis() as u64;
        on_event(AgentEvent::ReasoningCompleted { elapsed_ms: duration_ms });
        segments.push(ReasoningSegment { text, duration_ms });
    }
}

/// Stream one model round, preserving the provider's terminal reason. A
/// stream that ends without a terminal event is never treated as success.
#[derive(Default)]
struct AttemptDiagnostics {
    observed_text: String,
    finish_reason: Option<FinishReason>,
    connect_ms: Option<u64>,
    ttft_ms: Option<u64>,
    max_event_gap_ms: Option<u64>,
}

async fn stream_round(
    runtime: &dyn ModelRuntime,
    request: ModelRequest,
    cancellation: &CancellationToken,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
    observed_usage: &mut Option<TokenUsage>,
    output_estimate: &mut leveler_model::TokenEstimate,
    diagnostics: &mut AttemptDiagnostics,
) -> Result<ModelRound, AgentCoreError> {
    let deadline = request.deadline;
    let request_id = request.request_id.as_str().to_string();
    let mut usage = TokenUsage::default();
    let mut finish_reason = None;

    // A model round is the one thing a stuck run is almost always waiting
    // on. Timing each phase separately is what makes the wait attributable —
    // connect vs. first byte vs. streaming.
    let round_started = std::time::Instant::now();
    let message_count = request.messages.len();
    tracing::info!(
        request_id = %request_id,
        messages = message_count,
        reasoning_effort = ?request.reasoning_effort,
        "model round started"
    );

    let started_stream = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(AgentCoreError::Cancelled),
        _ = wait_for_deadline(deadline) => return Err(ModelError::new(ModelErrorKind::Timeout, "model request deadline elapsed").into()),
        result = runtime.stream(request, cancellation.child_token()) => result,
    };
    let mut stream = match started_stream {
        Ok(s) => s,
        Err(e) if cancellation.is_cancelled() || e.kind == ModelErrorKind::Cancelled => {
            return Err(AgentCoreError::Cancelled);
        }
        Err(e) => return Err(AgentCoreError::Model(e)),
    };

    let mut canonical_content = None;
    // Keep the one text buffer outside the fallible assembly, so every exit
    // can settle already-observed output without inventing a complete answer.
    let text = &mut diagnostics.observed_text;
    // Reasoning is assistant content: accumulated so it can be assembled into
    // the message and echoed back to providers that require it
    // (`CompatibilityConfig::reasoning_replay_scope`). It is still emitted
    // live as a `ReasoningDelta` event, which is what the UI reads; the raw
    // text never enters the user-visible transcript.
    let mut reasoning = String::new();
    let mut reasoning_started = false;
    // The open reasoning SEGMENT, if any. `Some` from the segment's first
    // delta until a real boundary (answer text, a tool call, canonical
    // content, message completion, or a clean stream end). A segment that
    // never closes cleanly was interrupted: no `ReasoningCompleted` is
    // emitted for it, so presentation cannot mistake it for a finished
    // Thought. One message may carry several segments (Anthropic thinking
    // blocks), so this is per-segment state, never a single message-level
    // duration.
    let mut reasoning_segment: Option<(std::time::Instant, String)> = None;
    // Segments closed cleanly this attempt, for the durable projection.
    let mut reasoning_segments: Vec<ReasoningSegment> = Vec::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    // Whether the model had begun describing a tool call before a cut. An
    // unfinished call is never executed, but it IS output the model produced,
    // so a re-request would duplicate generation — the delivery truth below
    // records it.
    let mut tool_args_started = false;
    // Some providers (and some gateways) emit `usage` in a chunk *after*
    // `finish_reason`. Breaking at MessageCompleted would drop that chunk.
    // After completion we only accept UsageUpdated / terminal errors until
    // the stream ends.
    let mut completed = false;
    let connect_ms = round_started.elapsed().as_millis() as u64;
    diagnostics.connect_ms = Some(connect_ms);
    let mut first_event_ms: Option<u64> = None;

    // `MessageStarted` is synthesized locally by the protocol layer, so the
    // first event says nothing about the provider. What matters for a frozen
    // screen is the longest stretch with NO event at all.
    let mut event_count = 0u32;
    let mut last_event = std::time::Instant::now();
    let mut max_event_gap_ms = 0u64;
    loop {
        let event = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AgentCoreError::Cancelled),
            _ = wait_for_deadline(deadline) => return Err(ModelError::new(ModelErrorKind::Timeout, "model request deadline elapsed").into()),
            event = stream.next() => event,
        };
        let Some(event) = event else { break };
        let gap = last_event.elapsed().as_millis() as u64;
        if gap > max_event_gap_ms {
            max_event_gap_ms = gap;
        }
        diagnostics.max_event_gap_ms = Some(max_event_gap_ms);
        last_event = std::time::Instant::now();
        event_count += 1;
        if first_event_ms.is_none()
            && match &event {
                Ok(ModelEvent::TextDelta { delta } | ModelEvent::ReasoningDelta { delta }) => {
                    !delta.is_empty()
                }
                Ok(ModelEvent::ReasoningMetadata { bytes }) => *bytes > 0,
                Ok(
                    ModelEvent::ToolCallStarted { .. }
                    | ModelEvent::ToolCallArgumentsDelta { .. }
                    | ModelEvent::ToolCallCompleted { .. },
                ) => true,
                _ => false,
            }
        {
            let ms = round_started.elapsed().as_millis() as u64;
            first_event_ms = Some(ms);
            diagnostics.ttft_ms = Some(ms);
            tracing::info!(
                request_id = %request_id,
                connect_ms,
                first_event_ms = ms,
                "model round first byte"
            );
        }
        if cancellation.is_cancelled() {
            return Err(AgentCoreError::Cancelled);
        }
        match event {
            Ok(ModelEvent::MessageContent { content }) if !completed => {
                close_reasoning_segment(&mut reasoning_segment, &mut reasoning_segments, on_event);
                canonical_content = Some(content)
            }
            Ok(ModelEvent::TextDelta { delta }) if !completed => {
                if !delta.is_empty() {
                    // Answer text is the reasoning segment's clean end.
                    close_reasoning_segment(&mut reasoning_segment, &mut reasoning_segments, on_event);
                    output_estimate.add_text(&delta);
                    text.push_str(&delta);
                    on_event(AgentEvent::AssistantDelta(delta));
                }
            }
            Ok(ModelEvent::ReasoningDelta { delta }) if !completed => {
                if !delta.is_empty() {
                    if reasoning_segment.is_none() {
                        reasoning_segment = Some((std::time::Instant::now(), String::new()));
                        on_event(AgentEvent::ReasoningStarted);
                    }
                    // The segment's own text is what the durable projection
                    // records; `reasoning` below stays the provider's joined
                    // field for passback.
                    if let Some((_, segment_text)) = reasoning_segment.as_mut() {
                        segment_text.push_str(&delta);
                    }
                    reasoning_started = true;
                    output_estimate.add_text(&delta);
                    reasoning.push_str(&delta);
                    on_event(AgentEvent::ReasoningDelta(delta));
                }
            }
            Ok(ModelEvent::ReasoningMetadata { bytes }) if !completed => {
                reasoning_started |= bytes > 0;
                output_estimate.add_opaque_bytes(bytes);
            }
            Ok(ModelEvent::ToolCallCompleted { call }) if !completed => {
                close_reasoning_segment(&mut reasoning_segment, &mut reasoning_segments, on_event);
                calls.push(call)
            }
            // A tool call the model began describing but had not finished. The
            // arguments are never joined, so the call is not executable; the
            // fact that output started is what matters for retry safety.
            Ok(ModelEvent::ToolCallArgumentsDelta { delta, .. }) if !completed => {
                close_reasoning_segment(&mut reasoning_segment, &mut reasoning_segments, on_event);
                output_estimate.add_tool(&delta);
                tool_args_started = true;
            }
            Ok(ModelEvent::ToolCallStarted { .. }) if !completed => {
                close_reasoning_segment(&mut reasoning_segment, &mut reasoning_segments, on_event);
                tool_args_started = true;
            }
            Ok(ModelEvent::MessageCompleted {
                finish_reason: reason,
            }) => {
                close_reasoning_segment(&mut reasoning_segment, &mut reasoning_segments, on_event);
                finish_reason = Some(reason);
                diagnostics.finish_reason = Some(reason);
                completed = true;
            }
            Ok(ModelEvent::Error { error }) => {
                // Bytes were flowing, so this is a stream interruption with
                // whatever had been produced so far attached.
                return Err(AgentCoreError::Model(interrupted_error(
                    error,
                    !text.is_empty(),
                    reasoning_started,
                    tool_args_started || !calls.is_empty(),
                )));
            }
            Ok(ModelEvent::UsageUpdated {
                usage: latest_usage,
            }) => {
                on_event(AgentEvent::Usage(latest_usage));
                // A zero report is the ABSENCE of a measurement, not a
                // measurement of zero. A gateway that emits an empty `usage`
                // object after the real one must not erase it, or the round
                // would be billed as free and its cache line lost. A later
                // non-zero report always wins: the provider's final word is
                // the authoritative one.
                if latest_usage.total() > 0 || usage.total() == 0 {
                    usage = latest_usage;
                    *observed_usage = Some(latest_usage);
                }
            }
            // Start / partial tool-call fragments, or content after
            // completion — no assembly state we need here.
            Ok(_) => {}
            Err(e) if cancellation.is_cancelled() || e.kind == ModelErrorKind::Cancelled => {
                return Err(AgentCoreError::Cancelled);
            }
            Err(e) => {
                // The response stream broke mid-flight: bytes demonstrably
                // flowed, so delivery is `StreamInterrupted`, not a pre-send
                // failure, however the transport classified the raw error.
                return Err(AgentCoreError::Model(interrupted_error(
                    e,
                    !text.is_empty(),
                    reasoning_started,
                    tool_args_started || !calls.is_empty(),
                )));
            }
        }
    }

    let finish_reason = finish_reason.ok_or_else(|| {
        tracing::warn!(
            request_id = %request_id,
            connect_ms,
            first_event_ms,
            total_ms = round_started.elapsed().as_millis() as u64,
            text_len = text.len(),
            calls = calls.len(),
            "model round ended with no terminal event (retryable)"
        );
        AgentCoreError::Model(interrupted_error(
            ModelError::new(
                ModelErrorKind::StreamInterrupted,
                "model stream ended without a terminal completion event",
            ),
            !text.is_empty(),
            reasoning_started,
            tool_args_started || !calls.is_empty(),
        ))
    })?;

    let attempt_ms = round_started.elapsed().as_millis().min(u64::MAX as u128) as u64;

    tracing::info!(
        request_id = %request_id,
        connect_ms,
        first_event_ms,
        attempt_ms,
        total_ms = attempt_ms,
        events = event_count,
        max_event_gap_ms,
        ?finish_reason,
        input_tokens = usage.input_tokens,
        output_tokens = usage.output_tokens,
        cached_input_tokens = usage.cached_input_tokens,
        text_len = text.len(),
        calls = calls.len(),
        "model round finished"
    );

    // Guarantee one final usage signal even when the provider only reported
    // tokens once mid-stream (or only after completion).
    if usage.total() > 0 {
        on_event(AgentEvent::Usage(usage));
    }

    // Same part order as the non-streaming path
    // (`leveler_model::stream_from_response`): reasoning, then text, then the
    // tool calls, so the two response paths cannot drift apart.
    let mut content = Vec::new();
    if !reasoning.is_empty() {
        content.push(ContentPart::Reasoning { text: reasoning });
    }
    if !text.is_empty() {
        content.push(ContentPart::Text {
            text: std::mem::take(text),
        });
    }
    for call in calls {
        content.push(ContentPart::ToolCall { call });
    }
    let content = canonical_content.unwrap_or(content);
    Ok(ModelRound {
        request_id,
        message: Message {
            origin: None,
            role: Role::Assistant,
            content,
        },
        usage,
        finish_reason,
        latency_ms: 0,
        attempt_ms,
        connect_ms,
        ttft_ms: first_event_ms,
        max_event_gap_ms,
        retry_count: 0,
        cost_usd_micros: None,
        estimated_tokens: None,
        reasoning_segments,
    })
}

#[cfg(test)]
mod backoff_tests {
    use super::*;

    fn err(kind: ModelErrorKind) -> ModelError {
        ModelError::new(kind, "x")
    }

    /// The schedule is a fixed, bounded function of the retry number: quick
    /// first recoveries, then a steady 30s rate. The presented countdown is
    /// this value, so it must not grow without bound.
    #[test]
    fn schedule_climbs_then_caps_at_thirty_seconds() {
        let expected = [1, 2, 4, 8, 15, 30, 30, 30, 30, 30];
        for (i, secs) in expected.iter().enumerate() {
            let retry = (i + 1) as u32;
            assert_eq!(
                retry_backoff_delay(&err(ModelErrorKind::Transport), retry),
                Duration::from_secs(*secs),
                "retry {retry}"
            );
        }
        // Any retry beyond the budget keeps the cap rather than growing.
        assert_eq!(
            retry_backoff_delay(&err(ModelErrorKind::Transport), 50),
            Duration::from_secs(30)
        );
        assert_eq!(MAX_RETRIES, 10, "the budget the schedule is sized for");
    }

    #[test]
    fn provider_advertised_delay_wins_and_is_capped() {
        let e = err(ModelErrorKind::RateLimit).with_retry_after_ms(5_000);
        assert_eq!(retry_backoff_delay(&e, 1), Duration::from_secs(5));
        // …but a hostile/buggy value is capped.
        let e = err(ModelErrorKind::RateLimit).with_retry_after_ms(3_600_000);
        assert_eq!(retry_backoff_delay(&e, 1), Duration::from_secs(120));
    }
}

#[cfg(test)]
mod retry_decision_tests {
    //! The retry decision is a function of delivery truth, not of the kind or
    //! the message alone. These tests pin the boundary: safe failures and the
    //! narrow pre-stream `Transport + Unknown` send exception may be re-sent
    //! automatically, at most `MAX_RETRIES` times, after which the last failure
    //! is surfaced. A partial stream is never replayed.
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use leveler_model::{FinishReason, ModelRef, Retryability, Role};
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// A runtime that fails with `error` for the first `fail_times` calls, then
    /// returns a minimal completed stream. Counts calls.
    struct FlakyRuntime {
        error: ModelError,
        fail_times: u32,
        calls: Arc<Mutex<u32>>,
        seen: Arc<Mutex<Vec<ModelRequest>>>,
    }

    #[async_trait::async_trait]
    impl ModelRuntime for FlakyRuntime {
        async fn generate(
            &self,
            _r: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelResponse, ModelError> {
            unimplemented!()
        }
        async fn stream(
            &self,
            r: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelEventStream, ModelError> {
            self.seen.lock().unwrap().push(r);
            let n = {
                let mut c = self.calls.lock().unwrap();
                *c += 1;
                *c
            };
            if n <= self.fail_times {
                return Err(self.error.clone());
            }
            let events: Vec<Result<ModelEvent, ModelError>> = vec![
                Ok(ModelEvent::MessageStarted {
                    request_id: leveler_core::RequestId::new("r"),
                }),
                Ok(ModelEvent::TextDelta { delta: "ok".into() }),
                Ok(ModelEvent::MessageCompleted {
                    finish_reason: FinishReason::Stop,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
        async fn profile(&self, _m: &ModelRef) -> Result<leveler_model::ModelProfile, ModelError> {
            unimplemented!()
        }
    }

    /// A runtime that returns the scripted error for each call in order, then a
    /// clean success. It optionally holds each call open for a moment and
    /// tracks how many calls overlap, so a test can prove retries never run
    /// concurrently (ONE retry controller).
    struct ScriptedRuntime {
        errors: Vec<ModelError>,
        hold_ms: u64,
        calls: Arc<Mutex<u32>>,
        in_flight: Arc<Mutex<u32>>,
        max_in_flight: Arc<Mutex<u32>>,
    }

    impl ScriptedRuntime {
        fn new(errors: Vec<ModelError>, hold_ms: u64) -> Self {
            Self {
                errors,
                hold_ms,
                calls: Arc::new(Mutex::new(0)),
                in_flight: Arc::new(Mutex::new(0)),
                max_in_flight: Arc::new(Mutex::new(0)),
            }
        }
    }

    #[async_trait::async_trait]
    impl ModelRuntime for ScriptedRuntime {
        async fn generate(
            &self,
            _r: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelResponse, ModelError> {
            unimplemented!()
        }
        async fn stream(
            &self,
            _r: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelEventStream, ModelError> {
            let n = {
                let mut c = self.calls.lock().unwrap();
                *c += 1;
                *c
            };
            {
                let mut f = self.in_flight.lock().unwrap();
                *f += 1;
                let mut m = self.max_in_flight.lock().unwrap();
                if *f > *m {
                    *m = *f;
                }
            }
            if self.hold_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.hold_ms)).await;
            }
            {
                let mut f = self.in_flight.lock().unwrap();
                *f -= 1;
            }
            if let Some(e) = self.errors.get((n - 1) as usize) {
                return Err(e.clone());
            }
            let events: Vec<Result<ModelEvent, ModelError>> = vec![
                Ok(ModelEvent::MessageStarted {
                    request_id: leveler_core::RequestId::new("r"),
                }),
                Ok(ModelEvent::TextDelta { delta: "ok".into() }),
                Ok(ModelEvent::MessageCompleted {
                    finish_reason: FinishReason::Stop,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
        async fn profile(&self, _m: &ModelRef) -> Result<leveler_model::ModelProfile, ModelError> {
            unimplemented!()
        }
    }

    /// Streams `produced` and then fails with `error` — the shape of a
    /// connection that broke after the provider had already answered, where the
    /// transport's own classification cannot see that a body had begun.
    struct BrokenStreamRuntime {
        error: ModelError,
        produced: Produced,
        calls: Arc<Mutex<u32>>,
    }

    /// What a scripted stream managed to produce before it broke.
    #[derive(Clone, Copy)]
    enum Produced {
        Nothing,
        Reasoning,
        Text,
        ToolArgs,
    }

    #[async_trait::async_trait]
    impl ModelRuntime for BrokenStreamRuntime {
        async fn generate(
            &self,
            _r: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelResponse, ModelError> {
            unimplemented!()
        }
        async fn stream(
            &self,
            _r: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelEventStream, ModelError> {
            let n = {
                let mut c = self.calls.lock().unwrap();
                *c += 1;
                *c
            };
            let mut events: Vec<Result<ModelEvent, ModelError>> =
                vec![Ok(ModelEvent::MessageStarted {
                    request_id: leveler_core::RequestId::new("r"),
                })];
            match self.produced {
                Produced::Nothing => {}
                Produced::Reasoning => events.push(Ok(ModelEvent::ReasoningDelta {
                    delta: "thinking".into(),
                })),
                Produced::Text => events.push(Ok(ModelEvent::TextDelta {
                    delta: "half".into(),
                })),
                Produced::ToolArgs => events.push(Ok(ModelEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: "{\"pa".into(),
                })),
            }
            if n == 1 {
                events.push(Err(self.error.clone()));
            } else {
                events.push(Ok(ModelEvent::MessageCompleted {
                    finish_reason: FinishReason::Stop,
                }));
            }
            Ok(Box::pin(futures::stream::iter(events)))
        }
        async fn profile(&self, _m: &ModelRef) -> Result<leveler_model::ModelProfile, ModelError> {
            unimplemented!()
        }
    }

    fn request() -> ModelRequest {
        ModelRequest::new(
            ModelRef::new("mock", "m"),
            vec![Message::text(Role::User, "hi")],
        )
    }

    /// A retry policy that performs no real wait, so a full ten-retry
    /// exhaustion costs no wall-clock time. Production uses the real schedule.
    fn instant_policy() -> RetryPolicy {
        RetryPolicy {
            max_retries: MAX_RETRIES,
            delay_scale: 0.0,
        }
    }

    async fn run(
        error: ModelError,
        fail_times: u32,
        cancellation: &CancellationToken,
        out: &mut Vec<AgentEvent>,
    ) -> (Result<ModelRound, AgentCoreError>, u32) {
        let calls = Arc::new(Mutex::new(0));
        let runtime = FlakyRuntime {
            error,
            fail_times,
            calls: calls.clone(),
            seen: Arc::default(),
        };
        let result = run_model_round_with(
            &runtime,
            request(),
            cancellation,
            &mut |e| out.push(e),
            instant_policy(),
        )
        .await;
        let n = *calls.lock().unwrap();
        (result, n)
    }

    fn not_sent() -> ModelError {
        ModelError::new(ModelErrorKind::Transport, "connect refused")
            .with_delivery_state(DeliveryState::NotSent)
    }
    fn send_transport_unknown() -> ModelError {
        ModelError::new(ModelErrorKind::Transport, "error sending request for url")
            .with_delivery_state(DeliveryState::Unknown)
    }
    fn no_output_cut() -> ModelError {
        ModelError::new(ModelErrorKind::StreamInterrupted, "cut").with_delivery_state(
            DeliveryState::StreamInterrupted {
                progress: StreamProgress::default(),
            },
        )
    }
    fn partial_cut() -> ModelError {
        ModelError::new(ModelErrorKind::StreamInterrupted, "cut").with_delivery_state(
            DeliveryState::StreamInterrupted {
                progress: StreamProgress {
                    reasoning: false,
                    text: true,
                    tool_args: false,
                },
            },
        )
    }

    /// The transport's own classification of a request that was written and
    /// never answered (`map_reqwest_error`).
    fn no_response_timeout() -> ModelError {
        ModelError::new(ModelErrorKind::Timeout, "read timed out")
            .with_delivery_state(DeliveryState::SentNoResponse)
    }

    /// The transport's structured classification of a refused socket.
    fn connection_refused() -> ModelError {
        ModelError::new(
            ModelErrorKind::ProviderUnavailable,
            "Connection refused (os error 61)",
        )
        .with_delivery_state(DeliveryState::NotSent)
        .with_transport_fault(TransportFault::ConnectionRefused)
    }

    /// A wrong `base_url` fails in seconds, not after the full ~3-minute
    /// schedule: a refused socket gets a short, fast budget.
    #[tokio::test]
    async fn a_refused_connection_exhausts_a_short_fast_budget() {
        let mut out = Vec::new();
        let cancel = CancellationToken::new();
        let (result, calls) = run(connection_refused(), u32::MAX, &cancel, &mut out).await;
        let error = match result.expect_err("a refused socket must fail") {
            AgentCoreError::Model(error) => error,
            other => panic!("expected a model error, got {other:?}"),
        };
        assert_eq!(
            calls,
            1 + IMMEDIATE_ENDPOINT_MAX_RETRIES,
            "one attempt plus the endpoint budget"
        );
        assert_eq!(error.retry_attempts, Some(IMMEDIATE_ENDPOINT_MAX_RETRIES));
        let max_attempts: Vec<u32> = out
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ModelRetrying { max_attempts, .. } => Some(*max_attempts),
                _ => None,
            })
            .collect();
        assert_eq!(
            max_attempts,
            vec![3, 3, 3],
            "the event states the same budget"
        );
    }

    /// The endpoint budget is scoped to the structured fault: a generic
    /// pre-delivery transport failure keeps the full retry lifecycle.
    #[tokio::test]
    async fn a_generic_not_sent_transport_failure_keeps_the_full_budget() {
        let mut out = Vec::new();
        let cancel = CancellationToken::new();
        let (result, calls) = run(not_sent(), u32::MAX, &cancel, &mut out).await;
        assert!(result.is_err());
        assert_eq!(
            calls,
            1 + MAX_RETRIES,
            "the full bounded lifecycle still runs"
        );
    }

    #[test]
    fn the_endpoint_budget_and_schedule_apply_only_to_a_refused_socket() {
        let policy = RetryPolicy::production();
        assert_eq!(
            policy.max_retries_for(&connection_refused()),
            IMMEDIATE_ENDPOINT_MAX_RETRIES
        );
        assert_eq!(policy.max_retries_for(&not_sent()), MAX_RETRIES);

        // Fast schedule (250/500/1000 ms plus <=20% jitter), never 30 s.
        let first = policy.delay(&connection_refused(), 1);
        assert!(
            first >= Duration::from_millis(250) && first <= Duration::from_millis(320),
            "{first:?}"
        );
        let third = policy.delay(&connection_refused(), 3);
        assert!(
            third >= Duration::from_millis(1_000) && third <= Duration::from_millis(1_250),
            "{third:?}"
        );
        // An unrelated NotSent failure still gets the long first backoff.
        assert!(policy.delay(&not_sent(), 1) >= Duration::from_millis(1_000));
    }

    /// A retry re-sends the round's request as built — the tool exchange in
    /// it is not rebuilt, trimmed or reordered between attempts.
    #[tokio::test]
    async fn a_retried_round_resends_the_same_tool_exchange() {
        let exchange = vec![
            Message::text(Role::User, "hi"),
            Message {
                origin: None,
                role: Role::Assistant,
                content: ["a", "b"]
                    .iter()
                    .map(|id| leveler_model::ContentPart::ToolCall {
                        call: leveler_model::ToolCall {
                            id: leveler_core::ToolCallId::new(*id),
                            name: "read_file".into(),
                            arguments: serde_json::json!({}),
                        },
                    })
                    .collect(),
            },
            Message {
                origin: None,
                role: Role::Tool,
                content: ["a", "b"]
                    .iter()
                    .map(|id| leveler_model::ContentPart::ToolResult {
                        result: leveler_model::ToolResultContent {
                            call_id: leveler_core::ToolCallId::new(*id),
                            content: "ok".into(),
                            is_error: false,
                        },
                    })
                    .collect(),
            },
        ];
        let seen = Arc::new(Mutex::new(Vec::new()));
        let runtime = FlakyRuntime {
            error: not_sent(),
            fail_times: 1,
            calls: Arc::default(),
            seen: seen.clone(),
        };
        let request = ModelRequest::new(ModelRef::new("mock", "m"), exchange.clone());
        let result = run_model_round_with(
            &runtime,
            request,
            &CancellationToken::new(),
            &mut |_| {},
            instant_policy(),
        )
        .await;
        assert!(result.is_ok());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one failure, one retry");
        for attempt in seen.iter() {
            assert_eq!(attempt.messages, exchange);
            leveler_model::validate_tool_exchange(&attempt.messages).expect("paired");
        }
    }

    /// A request refused for breaking the tool-exchange invariant never left
    /// the process, yet re-sending it cannot help: it is not retried.
    #[tokio::test]
    async fn a_refused_tool_exchange_is_never_retried() {
        let error = ModelError::new(ModelErrorKind::ConversationProtocol, "orphan tool result")
            .with_delivery_state(DeliveryState::NotSent);
        let mut events = Vec::new();
        let (result, calls) = run(error, 100, &CancellationToken::new(), &mut events).await;
        assert!(result.is_err());
        assert_eq!(calls, 1, "an internal protocol error is terminal: {calls}");
    }

    /// P0-1: once output exists, re-POSTing the whole request is a REPLAY.
    #[tokio::test]
    async fn a_stream_cut_after_output_is_never_auto_replayed() {
        let mut events = Vec::new();
        let (result, calls) = run(partial_cut(), 100, &CancellationToken::new(), &mut events).await;
        assert!(result.is_err(), "a partial stream must not silently retry");
        assert_eq!(calls, 1, "the request is sent exactly once: {calls}");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ModelRetrying { .. })),
            "no retry may be announced for a partial-stream replay"
        );
    }

    #[tokio::test]
    async fn reasoning_only_failure_is_not_replayed() {
        let calls = Arc::new(Mutex::new(0));
        let runtime = BrokenStreamRuntime {
            error: no_output_cut(),
            produced: Produced::Reasoning,
            calls: calls.clone(),
        };
        let result = run_model_round_with(
            &runtime,
            request(),
            &CancellationToken::new(),
            &mut |_| {},
            instant_policy(),
        )
        .await;
        assert!(result.is_err(), "captured reasoning is real output");
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    /// A failure seen *inside* a stream is re-classified from what the stream
    /// actually produced, never from what the transport first called it. Without
    /// this, the no-response recovery above could replay a stream that had
    /// already delivered output.
    #[tokio::test]
    async fn a_mid_stream_failure_is_rewritten_to_an_interruption_with_its_progress() {
        for (produced, progress) in [
            (
                Produced::Nothing,
                StreamProgress {
                    reasoning: false,
                    text: false,
                    tool_args: false,
                },
            ),
            (
                Produced::Text,
                StreamProgress {
                    reasoning: false,
                    text: true,
                    tool_args: false,
                },
            ),
            (
                Produced::ToolArgs,
                StreamProgress {
                    reasoning: false,
                    text: false,
                    tool_args: true,
                },
            ),
        ] {
            let calls = Arc::new(Mutex::new(0));
            let runtime = BrokenStreamRuntime {
                // The transport's own view: it never read a response head, so it
                // cannot know the model had already started answering.
                error: no_response_timeout(),
                produced,
                calls: calls.clone(),
            };
            let mut events = Vec::new();
            let result = run_model_round_with(
                &runtime,
                request(),
                &CancellationToken::new(),
                &mut |e| events.push(e),
                instant_policy(),
            )
            .await;

            if progress.any() {
                let error = match result {
                    Err(AgentCoreError::Model(e)) => e,
                    other => {
                        panic!("a stream that delivered output must not be replayed: {other:?}")
                    }
                };
                assert_eq!(
                    error.delivery_state,
                    DeliveryState::StreamInterrupted { progress },
                    "the delivered output must be recorded, not dropped"
                );
                assert_eq!(*calls.lock().unwrap(), 1, "no replay");
                assert!(
                    !events
                        .iter()
                        .any(|e| matches!(e, AgentEvent::ModelRetrying { .. }))
                );
            } else {
                assert!(
                    result.is_ok(),
                    "a stream that produced nothing is recoverable: {result:?}"
                );
                assert_eq!(*calls.lock().unwrap(), 2);
            }
        }
    }

    /// A stream cut with a tool call still being described carries output too:
    /// never replayed.
    #[tokio::test]
    async fn a_stream_cut_with_tool_args_is_never_auto_replayed() {
        let error = ModelError::new(ModelErrorKind::StreamInterrupted, "cut").with_delivery_state(
            DeliveryState::StreamInterrupted {
                progress: StreamProgress {
                    reasoning: false,
                    text: false,
                    tool_args: true,
                },
            },
        );
        let mut events = Vec::new();
        let (result, calls) = run(error, 100, &CancellationToken::new(), &mut events).await;
        assert!(result.is_err());
        assert_eq!(calls, 1, "an unfinished tool call is never replayed");
    }

    /// P0-1: a stream that produced nothing carries nothing to duplicate.
    #[tokio::test]
    async fn a_stream_cut_before_any_output_is_retried() {
        let mut events = Vec::new();
        let (result, calls) = run(no_output_cut(), 1, &CancellationToken::new(), &mut events).await;
        assert!(result.is_ok(), "the retry should succeed");
        assert_eq!(calls, 2, "one no-output retry then success: {calls}");
    }

    #[tokio::test]
    async fn a_request_that_provably_never_left_is_retried() {
        let mut events = Vec::new();
        let (result, calls) = run(not_sent(), 1, &CancellationToken::new(), &mut events).await;
        assert!(result.is_ok());
        assert_eq!(calls, 2, "a NotSent failure is retried once: {calls}");
    }

    /// Reqwest can report a transient send failure without proving whether any
    /// bytes left the process. It still happens before a response stream exists,
    /// so the bounded logical retry lifecycle owns recovery.
    #[tokio::test]
    async fn an_unknown_delivery_send_transport_failure_is_retried() {
        let mut events = Vec::new();
        let (result, calls) = run(
            send_transport_unknown(),
            1,
            &CancellationToken::new(),
            &mut events,
        )
        .await;
        assert!(result.is_ok(), "the bounded retry should recover");
        assert_eq!(calls, 2, "one send failure, one retry");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ModelRetrying { attempt: 1, .. }))
        );
    }

    /// The defect this closes: an upstream that received the whole request and
    /// never returned response headers is the same transient condition as a
    /// stream cut before any output, and must not end the run. Nothing was
    /// received, so a bounded replay cannot duplicate output.
    #[tokio::test]
    async fn a_no_response_timeout_is_retried_by_the_lifecycle() {
        let mut events = Vec::new();
        let (result, calls) = run(
            no_response_timeout(),
            1,
            &CancellationToken::new(),
            &mut events,
        )
        .await;
        assert!(
            result.is_ok(),
            "the bounded retry should recover: {result:?}"
        );
        assert_eq!(calls, 2, "one unanswered attempt, then one retry: {calls}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ModelRetrying { attempt: 1, .. }))
        );
    }

    /// Recovery from an unanswered request is bounded like every other
    /// transient failure: a provider that never answers ends the round instead
    /// of looping it.
    #[tokio::test]
    async fn a_no_response_timeout_exhausts_its_budget_and_surfaces() {
        let mut events = Vec::new();
        let (result, calls) = run(
            no_response_timeout(),
            1_000,
            &CancellationToken::new(),
            &mut events,
        )
        .await;
        assert_eq!(calls, 1 + MAX_RETRIES, "the same bounded budget applies");
        match result {
            Err(AgentCoreError::Model(e)) => {
                assert_eq!(e.retry_attempts, Some(MAX_RETRIES));
                assert_eq!(e.delivery_state, DeliveryState::SentNoResponse);
            }
            other => panic!("expected a terminal model failure: {other:?}"),
        }
    }

    /// Cancellation is a non-transient kind: no delivery state may make it
    /// retryable, and the round surfaces it as cancellation.
    #[tokio::test]
    async fn a_cancellation_is_never_retried_whatever_delivery_proved() {
        let error = ModelError::cancelled().with_delivery_state(DeliveryState::SentNoResponse);
        let mut events = Vec::new();
        let (result, calls) = run(error, 100, &CancellationToken::new(), &mut events).await;
        assert!(
            matches!(result, Err(AgentCoreError::Cancelled)),
            "{result:?}"
        );
        assert_eq!(calls, 1, "a cancelled round is never re-sent");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ModelRetrying { .. }))
        );
    }

    #[tokio::test]
    async fn a_retry_announces_itself_as_transient_connectivity() {
        let mut events = Vec::new();
        let (result, _) = run(no_output_cut(), 1, &CancellationToken::new(), &mut events).await;
        assert!(result.is_ok());
        let retries: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ModelRetrying {
                    attempt,
                    max_attempts,
                    ..
                } => Some((*attempt, *max_attempts)),
                _ => None,
            })
            .collect();
        assert_eq!(retries.len(), 1, "exactly one retry is announced");
        assert_eq!(retries[0].0, 1, "the first retry is 1-based");
        assert_eq!(retries[0].1, MAX_RETRIES, "the bound travels with it");
    }

    /// P0-4: nothing is requested after a cancel.
    #[tokio::test]
    async fn cancel_during_a_retry_sends_no_further_request() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut events = Vec::new();
        let (result, calls) = run(not_sent(), 100, &cancel, &mut events).await;
        assert!(matches!(
            result,
            Err(AgentCoreError::Cancelled) | Err(AgentCoreError::Model(_))
        ));
        // The first attempt may happen before the cancel is observed, but no
        // retry/wait may follow it.
        assert!(
            calls <= 1,
            "a cancelled round must not keep sending requests: {calls}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ModelRetrying { .. }))
        );
    }

    /// One retry succeeds on the next physical attempt: the retry is announced
    /// as `1/MAX_RETRIES`, the round resumes with no user action, and the round
    /// reports what it spent.
    #[tokio::test]
    async fn a_single_retry_recovers_and_reports_its_count() {
        let mut events = Vec::new();
        let (result, calls) = run(not_sent(), 1, &CancellationToken::new(), &mut events).await;
        let round = result.expect("the second attempt succeeds");
        assert_eq!(calls, 2, "one retry then success");
        assert_eq!(round.retry_count, 1, "the round reports the retry it spent");
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ModelRetrying { attempt: 1, max_attempts, .. }
                if *max_attempts == MAX_RETRIES
        )));
    }

    /// A runtime that reports usage on every attempt and fails the first
    /// `fail_times` ones with a retryable, no-output stream cut.
    struct UsageReportingRuntime {
        fail_times: u32,
        calls: Arc<Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl ModelRuntime for UsageReportingRuntime {
        async fn generate(
            &self,
            _r: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelResponse, ModelError> {
            unimplemented!()
        }
        async fn stream(
            &self,
            _r: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelEventStream, ModelError> {
            let n = {
                let mut c = self.calls.lock().unwrap();
                *c += 1;
                *c
            };
            // The interrupted attempt spent 200 output tokens of which 150 were
            // reasoning; the attempt that lands spends 1000 of which 700 were.
            let (output, reasoning) = if n <= self.fail_times {
                (200u64, 150u64)
            } else {
                (1_000u64, 700u64)
            };
            let mut events: Vec<Result<ModelEvent, ModelError>> = vec![
                Ok(ModelEvent::MessageStarted {
                    request_id: leveler_core::RequestId::new("r"),
                }),
                Ok(ModelEvent::UsageUpdated {
                    usage: TokenUsage {
                        input_tokens: 100,
                        output_tokens: output,
                        cached_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                        reasoning_tokens: Some(reasoning),
                    },
                }),
            ];
            if n <= self.fail_times {
                events.push(Err(no_output_cut()));
            } else {
                events.push(Ok(ModelEvent::TextDelta { delta: "ok".into() }));
                events.push(Ok(ModelEvent::MessageCompleted {
                    finish_reason: FinishReason::Stop,
                }));
            }
            Ok(Box::pin(futures::stream::iter(events)))
        }
        async fn profile(&self, _m: &ModelRef) -> Result<leveler_model::ModelProfile, ModelError> {
            unimplemented!()
        }
    }

    /// A retry must not add the abandoned attempt's reasoning to the one that
    /// landed: the round reports the surviving attempt's breakdown exactly, and
    /// the live usage signals replace rather than accumulate.
    #[tokio::test]
    async fn a_retry_does_not_double_count_reasoning_tokens() {
        let calls = Arc::new(Mutex::new(0));
        let runtime = UsageReportingRuntime {
            fail_times: 1,
            calls: calls.clone(),
        };
        let mut events = Vec::new();
        let round = run_model_round_with(
            &runtime,
            request(),
            &CancellationToken::new(),
            &mut |e| events.push(e),
            instant_policy(),
        )
        .await
        .expect("the second attempt lands");

        assert_eq!(*calls.lock().unwrap(), 2);
        assert_eq!(round.retry_count, 1);
        assert_eq!(round.usage.output_tokens, 1_000);
        assert_eq!(
            round.usage.reasoning_tokens,
            Some(700),
            "the round reports one attempt's breakdown, not the sum of both"
        );
        assert_eq!(round.usage.visible_output_tokens(), Some(300));

        let live: Vec<Option<u64>> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Usage(usage) => Some(usage.reasoning_tokens),
                _ => None,
            })
            .collect();
        assert_eq!(
            live.last().copied().flatten(),
            Some(700),
            "the last live signal is the surviving attempt's, not a running total"
        );
        assert!(
            !live.iter().flatten().any(|v| *v == 850),
            "no signal may report 150 + 700: {live:?}"
        );
    }

    /// Three failures then success: the count is three, not two or four.
    #[tokio::test]
    async fn three_failures_then_success_counts_three_retries() {
        let mut events = Vec::new();
        let (result, calls) = run(not_sent(), 3, &CancellationToken::new(), &mut events).await;
        let round = result.expect("the fourth attempt succeeds");
        assert_eq!(calls, 4);
        assert_eq!(round.retry_count, 3);
    }

    /// The budget is exactly `MAX_RETRIES`: a `Safe` failure that never recovers
    /// is attempted `1 + MAX_RETRIES` times and then surfaced, with the spent
    /// count travelling on the error.
    #[tokio::test]
    async fn ten_retries_then_a_terminal_failure() {
        let mut events = Vec::new();
        let (result, calls) = run(not_sent(), 1_000, &CancellationToken::new(), &mut events).await;
        assert_eq!(
            calls,
            1 + MAX_RETRIES,
            "exactly ten retries, never nine or eleven"
        );
        let retries: Vec<(u32, u32)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ModelRetrying {
                    attempt,
                    max_attempts,
                    ..
                } => Some((*attempt, *max_attempts)),
                _ => None,
            })
            .collect();
        assert_eq!(retries.len() as u32, MAX_RETRIES, "one event per retry");
        assert_eq!(retries.first(), Some(&(1, MAX_RETRIES)));
        assert_eq!(retries.last(), Some(&(MAX_RETRIES, MAX_RETRIES)));
        match result {
            Err(AgentCoreError::Model(e)) => {
                assert_eq!(
                    e.retry_attempts,
                    Some(MAX_RETRIES),
                    "the count travels on the error"
                );
                assert_eq!(e.retryability(), Retryability::Safe);
            }
            other => panic!("expected a terminal model failure: {other:?}"),
        }
    }

    /// A permanent failure is never turned into a retry budget.
    #[tokio::test]
    async fn permanent_failures_are_not_retried() {
        for error in [
            ModelError::from_status(401, "bad key"),
            ModelError::from_status(400, "bad request"),
            ModelError::from_status(404, "no model"),
        ] {
            let mut events = Vec::new();
            let (result, calls) =
                run(error.clone(), 100, &CancellationToken::new(), &mut events).await;
            assert!(result.is_err(), "{error:?} must fail");
            assert_eq!(calls, 1, "{error:?} must be sent once, not retried");
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, AgentEvent::ModelRetrying { .. })),
                "{error:?} must not announce a retry"
            );
        }
    }

    /// Transient gateway statuses are retried.
    #[tokio::test]
    async fn transient_statuses_are_retried() {
        for error in [
            ModelError::from_status(429, "slow down"),
            ModelError::from_status(500, "boom"),
            ModelError::from_status(503, "down"),
        ] {
            let mut events = Vec::new();
            let (result, calls) =
                run(error.clone(), 1, &CancellationToken::new(), &mut events).await;
            assert!(result.is_ok(), "{error:?} should be retried: {result:?}");
            assert_eq!(calls, 2, "{error:?} gets one retry");
        }
    }

    /// A `Retry-After` the provider advertised is the delay the loop actually
    /// waits, so the presented countdown cannot disagree with the scheduler.
    #[tokio::test]
    async fn advertised_retry_after_is_the_announced_delay() {
        let error = ModelError::from_status(429, "slow").with_retry_after_ms(120);
        let mut events = Vec::new();
        let calls = Arc::new(Mutex::new(0));
        let runtime = FlakyRuntime {
            error,
            fail_times: 1,
            calls: calls.clone(),
            seen: Arc::default(),
        };
        let policy = RetryPolicy {
            max_retries: MAX_RETRIES,
            delay_scale: 1.0,
        };
        let result = run_model_round_with(
            &runtime,
            request(),
            &CancellationToken::new(),
            &mut |e| events.push(e),
            policy,
        )
        .await;
        assert!(result.is_ok());
        let delay = events.iter().find_map(|e| match e {
            AgentEvent::ModelRetrying { delay_ms, .. } => Some(*delay_ms),
            _ => None,
        });
        assert_eq!(delay, Some(120), "the announced delay is the provider's");
    }

    /// Cancelling during the backoff stops the timer and every later retry at
    /// once: no request is sent after the cancel.
    #[tokio::test]
    async fn cancel_during_backoff_stops_retries() {
        let cancel = CancellationToken::new();
        let mut events = Vec::new();
        let calls = Arc::new(Mutex::new(0));
        let runtime = FlakyRuntime {
            error: not_sent(),
            fail_times: 1_000,
            calls: calls.clone(),
            seen: Arc::default(),
        };
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            canceller.cancel();
        });
        // The real schedule's first backoff is 1s, so the cancel lands inside it.
        let result = run_model_round_with(
            &runtime,
            request(),
            &cancel,
            &mut |e| events.push(e),
            RetryPolicy {
                max_retries: MAX_RETRIES,
                delay_scale: 1.0,
            },
        )
        .await;
        assert!(
            matches!(result, Err(AgentCoreError::Cancelled)),
            "a cancelled backoff surfaces as Cancelled: {result:?}"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "no attempt is made after the cancel"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::ModelRetrying { .. }))
                .count(),
            1,
            "the retry is announced, then cancelled before it runs"
        );
    }

    /// P0-6: flapping must not multiply controllers. Every attempt is awaited
    /// before the next begins, so no two requests are ever in flight and a
    /// burst of failures produces no duplicate concurrent request.
    #[tokio::test]
    async fn retries_never_overlap_across_a_flap() {
        let runtime = ScriptedRuntime::new(vec![not_sent(), not_sent(), not_sent()], 20);
        let cancel = CancellationToken::new();
        let mut events = Vec::new();
        let result = run_model_round_with(
            &runtime,
            request(),
            &cancel,
            &mut |e| events.push(e),
            instant_policy(),
        )
        .await;
        assert!(result.is_ok(), "the round resumes: {result:?}");
        assert_eq!(
            *runtime.max_in_flight.lock().unwrap(),
            1,
            "at most one request may be in flight: ONE retry controller"
        );
        assert_eq!(
            *runtime.calls.lock().unwrap(),
            4,
            "one request per attempt, never duplicated"
        );
    }
}

#[cfg(test)]
mod reasoning_assembly_tests {
    //! Streamed reasoning must be assembled into the round's assistant
    //! message, not just emitted as a transient event. The raw text is what a
    //! provider that requires `reasoning_content` passback needs, and it is
    //! also what context accounting charges as assistant content. The live
    //! `ReasoningDelta` events stay exactly as they were; only the assembled
    //! message gains the part.
    use super::*;

    /// Replays a fixed event sequence once, with no provider involved.
    struct ScriptedStreamRuntime {
        events: Vec<ModelEvent>,
    }

    #[async_trait::async_trait]
    impl ModelRuntime for ScriptedStreamRuntime {
        async fn generate(
            &self,
            _r: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelResponse, ModelError> {
            unimplemented!()
        }

        async fn stream(
            &self,
            _r: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelEventStream, ModelError> {
            let events: Vec<Result<ModelEvent, ModelError>> =
                self.events.iter().cloned().map(Ok).collect();
            Ok(Box::pin(futures::stream::iter(events)))
        }

        async fn profile(
            &self,
            _m: &leveler_model::ModelRef,
        ) -> Result<leveler_model::ModelProfile, ModelError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn streamed_reasoning_is_assembled_before_text_and_tool_calls() {
        let call = ToolCall {
            id: leveler_core::ToolCallId::new("call_1"),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        };
        let runtime = ScriptedStreamRuntime {
            events: vec![
                ModelEvent::MessageStarted {
                    request_id: leveler_core::RequestId::new("r"),
                },
                ModelEvent::ReasoningDelta {
                    delta: "分析...".into(),
                },
                ModelEvent::ReasoningDelta {
                    delta: "继续分析...".into(),
                },
                ModelEvent::ToolCallCompleted { call: call.clone() },
                ModelEvent::TextDelta {
                    delta: "answer".into(),
                },
                ModelEvent::MessageCompleted {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
        };
        let request = ModelRequest::new(
            leveler_model::ModelRef::new("mock", "m"),
            vec![Message::text(Role::User, "hi")],
        );
        let cancel = CancellationToken::new();
        let mut events = Vec::new();
        let round = run_model_round(&runtime, request, &cancel, &mut |e| events.push(e))
            .await
            .expect("scripted stream completes");

        // Reasoning is joined in order, ahead of the text and the call — the
        // same part order the non-streaming path assembles.
        assert_eq!(
            round.message.content,
            vec![
                ContentPart::Reasoning {
                    text: "分析...继续分析...".into()
                },
                ContentPart::Text {
                    text: "answer".into()
                },
                ContentPart::ToolCall { call },
            ]
        );

        // The live events are unchanged: each reasoning chunk is still a
        // `ReasoningDelta`, and text still streams as `AssistantDelta`.
        let reasoning_events: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ReasoningDelta(d) => Some(d.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning_events, vec!["分析...", "继续分析..."]);
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::AssistantDelta(d) if d == "answer"
        )));
    }

    fn reasoning_request() -> ModelRequest {
        ModelRequest::new(
            leveler_model::ModelRef::new("mock", "m"),
            vec![Message::text(Role::User, "hi")],
        )
    }

    /// The reasoning lifecycle as the consumer sees it, in order.
    fn reasoning_shape(events: &[AgentEvent]) -> Vec<&'static str> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ReasoningStarted => Some("started"),
                AgentEvent::ReasoningDelta(_) => Some("delta"),
                AgentEvent::ReasoningCompleted { .. } => Some("completed"),
                AgentEvent::AssistantDelta(_) => Some("assistant"),
                _ => None,
            })
            .collect()
    }

    /// REASONING-A1: answer text closes the segment cleanly, once, after every
    /// delta and before the answer delta.
    #[tokio::test]
    async fn answer_text_brackets_one_reasoning_segment() {
        let runtime = ScriptedStreamRuntime {
            events: vec![
                ModelEvent::MessageStarted {
                    request_id: leveler_core::RequestId::new("r"),
                },
                ModelEvent::ReasoningDelta { delta: "a".into() },
                ModelEvent::ReasoningDelta { delta: "b".into() },
                ModelEvent::TextDelta {
                    delta: "answer".into(),
                },
                ModelEvent::MessageCompleted {
                    finish_reason: FinishReason::Stop,
                },
            ],
        };
        let mut events = Vec::new();
        run_model_round(
            &runtime,
            reasoning_request(),
            &CancellationToken::new(),
            &mut |e| events.push(e),
        )
        .await
        .expect("scripted stream completes");

        assert_eq!(
            reasoning_shape(&events),
            vec!["started", "delta", "delta", "completed", "assistant"]
        );
    }

    /// REASONING-A2: a tool call is a real boundary too — reasoning is frozen
    /// before the call, not left dangling into the tool execution.
    #[tokio::test]
    async fn tool_call_closes_the_reasoning_segment() {
        let runtime = ScriptedStreamRuntime {
            events: vec![
                ModelEvent::MessageStarted {
                    request_id: leveler_core::RequestId::new("r"),
                },
                ModelEvent::ReasoningDelta {
                    delta: "think".into(),
                },
                ModelEvent::ToolCallCompleted {
                    call: ToolCall {
                        id: leveler_core::ToolCallId::new("c1"),
                        name: "read_file".into(),
                        arguments: serde_json::json!({}),
                    },
                },
                ModelEvent::MessageCompleted {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
        };
        let mut events = Vec::new();
        run_model_round(
            &runtime,
            reasoning_request(),
            &CancellationToken::new(),
            &mut |e| events.push(e),
        )
        .await
        .expect("scripted stream completes");

        assert_eq!(reasoning_shape(&events), vec!["started", "delta", "completed"]);
    }

    /// REASONING-A3: one message may carry several reasoning segments (block
    /// protocols such as Anthropic thinking blocks). Each is bracketed on its
    /// own, so a single message-level duration would be wrong.
    #[tokio::test]
    async fn each_reasoning_segment_is_bracketed_separately() {
        let runtime = ScriptedStreamRuntime {
            events: vec![
                ModelEvent::MessageStarted {
                    request_id: leveler_core::RequestId::new("r"),
                },
                ModelEvent::ReasoningDelta { delta: "one".into() },
                ModelEvent::TextDelta {
                    delta: "mid".into(),
                },
                ModelEvent::ReasoningDelta { delta: "two".into() },
                ModelEvent::MessageCompleted {
                    finish_reason: FinishReason::Stop,
                },
            ],
        };
        let mut events = Vec::new();
        run_model_round(
            &runtime,
            reasoning_request(),
            &CancellationToken::new(),
            &mut |e| events.push(e),
        )
        .await
        .expect("scripted stream completes");

        assert_eq!(
            reasoning_shape(&events),
            vec!["started", "delta", "completed", "assistant", "started", "delta", "completed"]
        );
    }

    /// REASONING-A4: a stream that breaks mid-reasoning never emits
    /// `ReasoningCompleted`. Presentation must therefore treat an open segment
    /// as interrupted, never as a finished Thought.
    #[tokio::test]
    async fn interrupted_reasoning_never_reports_completed() {
        let runtime = ScriptedStreamRuntime {
            events: vec![
                ModelEvent::MessageStarted {
                    request_id: leveler_core::RequestId::new("r"),
                },
                ModelEvent::ReasoningDelta {
                    delta: "half a thought".into(),
                },
                ModelEvent::Error {
                    error: ModelError::new(ModelErrorKind::Decode, "malformed chunk"),
                },
            ],
        };
        let mut events = Vec::new();
        let result = run_model_round_with(
            &runtime,
            reasoning_request(),
            &CancellationToken::new(),
            &mut |e| events.push(e),
            RetryPolicy {
                max_retries: 0,
                delay_scale: 0.0,
            },
        )
        .await;

        assert!(result.is_err(), "the broken stream is not a success");
        assert_eq!(reasoning_shape(&events), vec!["started", "delta"]);
    }
}

#[cfg(test)]
mod cancellation_boundary_tests {
    use super::*;
    struct Silent;
    #[async_trait::async_trait]
    impl ModelRuntime for Silent {
        async fn stream(
            &self,
            _: ModelRequest,
            _: CancellationToken,
        ) -> Result<leveler_model::ModelEventStream, ModelError> {
            Ok(Box::pin(futures::stream::pending()))
        }
        async fn generate(
            &self,
            _: ModelRequest,
            _: CancellationToken,
        ) -> Result<leveler_model::ModelResponse, ModelError> {
            unreachable!()
        }
        async fn profile(
            &self,
            _: &leveler_model::ModelRef,
        ) -> Result<leveler_model::ModelProfile, ModelError> {
            unreachable!()
        }
    }
    #[tokio::test]
    async fn request_deadline_wakes_silent_stream_without_starting_retry_backoff() {
        let mut request = ModelRequest::new(leveler_model::ModelRef::new("test", "silent"), vec![]);
        request.deadline = Some(std::time::Instant::now() + Duration::from_millis(10));
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            run_model_round(&Silent, request, &CancellationToken::new(), &mut |_| {}),
        )
        .await;
        assert!(
            matches!(
                result,
                Ok(Err(AgentCoreError::Model(ModelError {
                    kind: ModelErrorKind::Timeout,
                    ..
                })))
            ),
            "deadline must end the call instead of retrying: {result:?}"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn cancellation_wakes_a_silent_model_stream() {
        let cancellation = CancellationToken::new();
        let request = ModelRequest::new(leveler_model::ModelRef::new("test", "silent"), vec![]);
        let child = cancellation.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            child.cancel();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            run_model_round(&Silent, request, &cancellation, &mut |_| {}),
        )
        .await;
        assert!(
            matches!(result, Ok(Err(AgentCoreError::Cancelled))),
            "cancellation must wake the pending read: {result:?}"
        );
    }
}

#[cfg(test)]
mod attempt_contract_tests {
    use super::*;
    use leveler_model::{ModelAttempt, ModelEventStream, ModelProfile, ModelRef, ModelResponse};
    use std::sync::{Arc, Mutex};

    struct Runtime {
        scripts: Mutex<Vec<Vec<ModelEvent>>>,
        order: Arc<Mutex<Vec<&'static str>>>,
    }
    #[async_trait::async_trait]
    impl ModelRuntime for Runtime {
        async fn stream(
            &self,
            _: ModelRequest,
            _: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            self.order.lock().unwrap().push("send");
            Ok(Box::pin(futures::stream::iter(
                self.scripts.lock().unwrap().remove(0).into_iter().map(Ok),
            )))
        }
        async fn generate(
            &self,
            _: ModelRequest,
            _: CancellationToken,
        ) -> Result<ModelResponse, ModelError> {
            unreachable!()
        }
        async fn profile(&self, _: &ModelRef) -> Result<ModelProfile, ModelError> {
            unreachable!()
        }
    }
    struct Observer {
        attempts: Vec<ModelAttempt>,
        order: Arc<Mutex<Vec<&'static str>>>,
        reject: bool,
    }
    #[async_trait::async_trait]
    impl ModelRoundObserver for Observer {
        type Error = AgentCoreError;
        fn on_event(&mut self, _: AgentEvent) {}
        async fn on_attempt(&mut self, attempt: ModelAttempt) -> Result<(), Self::Error> {
            self.order.lock().unwrap().push("persist");
            self.attempts.push(attempt);
            if self.reject {
                Err(AgentCoreError::Cancelled)
            } else {
                Ok(())
            }
        }
    }
    fn usage(input: u64, output: u64) -> ModelEvent {
        ModelEvent::UsageUpdated {
            usage: TokenUsage {
                input_tokens: input,
                output_tokens: output,
                ..Default::default()
            },
        }
    }
    fn cached_usage(input: u64, output: u64, cached: u64) -> ModelEvent {
        ModelEvent::UsageUpdated {
            usage: TokenUsage {
                input_tokens: input,
                output_tokens: output,
                cached_input_tokens: cached,
                ..Default::default()
            },
        }
    }

    /// The provider's final word wins; a zero report after a real one is
    /// "unreported", never "zero".
    #[tokio::test]
    async fn a_later_zero_usage_report_does_not_erase_an_observed_one() {
        let order = Arc::new(Mutex::new(vec![]));
        let runtime = Runtime {
            scripts: Mutex::new(vec![vec![
                cached_usage(100, 10, 90),
                usage(0, 0),
                ModelEvent::MessageCompleted {
                    finish_reason: FinishReason::Stop,
                },
            ]]),
            order,
        };
        let mut observer = Observer {
            attempts: vec![],
            order: Arc::new(Mutex::new(vec![])),
            reject: false,
        };
        let round = run_model_round_observed(
            &runtime,
            request(),
            &CancellationToken::new(),
            &mut observer,
        )
        .await
        .unwrap();
        assert_eq!(round.usage.input_tokens, 100);
        assert_eq!(round.usage.cached_input_tokens, 90);
        assert_eq!(round.usage.cache_hit_rate(), 0.9);
    }
    fn error() -> ModelEvent {
        ModelEvent::Error {
            error: ModelError::new(ModelErrorKind::StreamInterrupted, "lost connection"),
        }
    }
    fn request() -> ModelRequest {
        ModelRequest::new(
            ModelRef::new("p", "m"),
            vec![Message::text(Role::User, "hello")],
        )
    }
    struct NeverSent;
    #[async_trait::async_trait]
    impl ModelRuntime for NeverSent {
        async fn stream(
            &self,
            _: ModelRequest,
            _: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            Err(ModelError::new(ModelErrorKind::Transport, "DNS failed")
                .with_delivery_state(DeliveryState::NotSent))
        }
        async fn generate(
            &self,
            _: ModelRequest,
            _: CancellationToken,
        ) -> Result<ModelResponse, ModelError> {
            unreachable!()
        }
        async fn profile(&self, _: &ModelRef) -> Result<ModelProfile, ModelError> {
            unreachable!()
        }
    }
    struct AdmissionObserver {
        admissions: usize,
        settled: usize,
    }
    #[async_trait::async_trait]
    impl ModelRoundObserver for AdmissionObserver {
        type Error = AgentCoreError;
        async fn before_attempt(&mut self) -> Result<(), Self::Error> {
            self.admissions += 1;
            if self.admissions > 1 {
                return Err(AgentCoreError::InvalidLimits("shared budget spent".into()));
            }
            Ok(())
        }
        fn on_event(&mut self, _: AgentEvent) {}
        async fn on_attempt(&mut self, _: ModelAttempt) -> Result<(), Self::Error> {
            self.settled += 1;
            Ok(())
        }
    }
    #[tokio::test]
    async fn retry_rechecks_shared_admission_before_sending() {
        let order = Arc::new(Mutex::new(vec![]));
        let runtime = Runtime {
            scripts: Mutex::new(vec![
                vec![error()],
                vec![
                    ModelEvent::TextDelta {
                        delta: "should not send".into(),
                    },
                    ModelEvent::MessageCompleted {
                        finish_reason: FinishReason::Stop,
                    },
                ],
            ]),
            order: order.clone(),
        };
        let mut observer = AdmissionObserver {
            admissions: 0,
            settled: 0,
        };
        let result = run_model_round_observed_with(
            &runtime,
            request(),
            &CancellationToken::new(),
            &mut observer,
            RetryPolicy {
                max_retries: 1,
                delay_scale: 0.0,
            },
        )
        .await;
        assert!(
            matches!(result, Err(AgentCoreError::InvalidLimits(_))),
            "retry must honor fresh admission"
        );
        assert_eq!(*order.lock().unwrap(), vec!["send"]);
        assert_eq!(observer.admissions, 2);
        assert_eq!(
            observer.settled, 1,
            "an admission refusal is not a provider attempt"
        );
    }
    #[tokio::test]
    async fn observed_text_survives_error_eof_and_cancel_without_reasoning_or_tools() {
        for ending in ["error", "eof", "cancel", "complete"] {
            let cancellation = CancellationToken::new();
            let order = Arc::new(Mutex::new(vec![]));
            let mut events = vec![
                ModelEvent::ReasoningDelta {
                    delta: "private".into(),
                },
                ModelEvent::TextDelta {
                    delta: "visible partial".into(),
                },
                ModelEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: "{unfinished".into(),
                },
            ];
            if ending == "error" {
                events.push(error());
            }
            if ending == "complete" {
                events.push(ModelEvent::MessageCompleted {
                    finish_reason: FinishReason::Stop,
                });
            }
            let runtime = Runtime {
                scripts: Mutex::new(vec![events]),
                order: order.clone(),
            };
            struct Capture {
                attempts: Vec<ModelAttempt>,
                cancel: Option<CancellationToken>,
            }
            #[async_trait::async_trait]
            impl ModelRoundObserver for Capture {
                type Error = AgentCoreError;
                fn on_event(&mut self, event: AgentEvent) {
                    if matches!(event, AgentEvent::AssistantDelta(_))
                        && let Some(token) = &self.cancel
                    {
                        token.cancel();
                    }
                }
                async fn on_attempt(&mut self, attempt: ModelAttempt) -> Result<(), Self::Error> {
                    self.attempts.push(attempt);
                    Ok(())
                }
            }
            let mut observer = Capture {
                attempts: vec![],
                cancel: (ending == "cancel").then(|| cancellation.clone()),
            };
            let result =
                run_model_round_observed(&runtime, request(), &cancellation, &mut observer).await;
            assert_eq!(
                observer.attempts.len(),
                1,
                "{ending}: output must not be replayed"
            );
            assert_eq!(
                observer.attempts[0].partial_text.as_deref(),
                if ending == "complete" {
                    None
                } else {
                    Some("visible partial")
                },
                "{ending}"
            );
            assert_eq!(result.is_ok(), ending == "complete");
            assert_eq!(*order.lock().unwrap(), vec!["send"]);
        }
    }
    #[tokio::test]
    async fn observers_receive_known_zero_spend_for_a_never_sent_attempt() {
        let mut observer = Observer {
            attempts: vec![],
            order: Arc::new(Mutex::new(vec![])),
            reject: true,
        };
        let _ = run_model_round_observed(
            &NeverSent,
            request(),
            &CancellationToken::new(),
            &mut observer,
        )
        .await;
        assert_eq!(observer.attempts[0].cost_usd_micros, Some(0));
        assert_eq!(observer.attempts[0].estimated_tokens, None);
        assert_eq!(observer.attempts[0].usage, None);
    }
    #[tokio::test]
    async fn failed_usage_is_awaited_before_retry_and_preserved_separately() {
        let order = Arc::new(Mutex::new(vec![]));
        let runtime = Runtime {
            scripts: Mutex::new(vec![
                vec![usage(50, 3), error()],
                vec![
                    ModelEvent::TextDelta { delta: "ok".into() },
                    usage(60, 5),
                    ModelEvent::MessageCompleted {
                        finish_reason: FinishReason::Stop,
                    },
                ],
            ]),
            order: order.clone(),
        };
        let mut observer = Observer {
            attempts: vec![],
            order: order.clone(),
            reject: false,
        };
        let result = run_model_round_observed_with(
            &runtime,
            request(),
            &CancellationToken::new(),
            &mut observer,
            RetryPolicy {
                max_retries: 1,
                delay_scale: 0.0,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            *order.lock().unwrap(),
            vec!["send", "persist", "send", "persist"]
        );
        assert_eq!(observer.attempts.len(), 2);
        assert_eq!(observer.attempts[0].usage.unwrap().total(), 53);
        assert!(observer.attempts[0].error.is_some());
        assert_eq!(observer.attempts[0].finish_reason, None);
        assert!(observer.attempts[0].connect_ms.is_some());
        assert_eq!(observer.attempts[1].usage.unwrap().total(), 65);
        assert_eq!(
            result.usage.total(),
            65,
            "answer usage is not an accumulated second ledger"
        );
    }
    #[tokio::test]
    async fn deadline_interrupts_retry_backoff_without_another_physical_attempt() {
        let order = Arc::new(Mutex::new(vec![]));
        let runtime = Runtime {
            scripts: Mutex::new(vec![vec![error()]]),
            order: order.clone(),
        };
        let mut observer = Observer {
            attempts: vec![],
            order: order.clone(),
            reject: false,
        };
        let mut request = request();
        request.deadline = Some(std::time::Instant::now() + Duration::from_millis(10));
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            run_model_round_observed(&runtime, request, &CancellationToken::new(), &mut observer),
        )
        .await;
        assert!(matches!(
            result,
            Ok(Err(AgentCoreError::Model(ModelError {
                kind: ModelErrorKind::Timeout,
                ..
            })))
        ));
        assert_eq!(*order.lock().unwrap(), vec!["send", "persist"]);
    }
    #[tokio::test]
    async fn persistence_failure_aborts_before_any_retry() {
        let order = Arc::new(Mutex::new(vec![]));
        let runtime = Runtime {
            scripts: Mutex::new(vec![vec![error()]]),
            order: order.clone(),
        };
        let mut observer = Observer {
            attempts: vec![],
            order: order.clone(),
            reject: true,
        };
        assert!(
            run_model_round_observed_with(
                &runtime,
                request(),
                &CancellationToken::new(),
                &mut observer,
                RetryPolicy {
                    max_retries: 1,
                    delay_scale: 0.0
                }
            )
            .await
            .is_err()
        );
        assert_eq!(*order.lock().unwrap(), vec!["send", "persist"]);
        assert!(observer.attempts[0].estimated_tokens.unwrap() > 0);
        assert_eq!(observer.attempts[0].usage, None);
    }
    #[tokio::test]
    async fn local_start_and_usage_are_not_a_first_token() {
        let order = Arc::new(Mutex::new(vec![]));
        let runtime = Runtime {
            scripts: Mutex::new(vec![vec![
                ModelEvent::MessageStarted {
                    request_id: leveler_core::RequestId::new("r"),
                },
                usage(4, 0),
                ModelEvent::MessageCompleted {
                    finish_reason: FinishReason::Stop,
                },
            ]]),
            order: order.clone(),
        };
        let mut observer = Observer {
            attempts: vec![],
            order,
            reject: false,
        };
        run_model_round_observed(
            &runtime,
            request(),
            &CancellationToken::new(),
            &mut observer,
        )
        .await
        .unwrap();
        assert_eq!(observer.attempts[0].ttft_ms, None);
    }
    #[tokio::test]
    async fn empty_signed_thinking_is_progress_and_cannot_be_replayed() {
        let order = Arc::new(Mutex::new(vec![]));
        let runtime = Runtime {
            scripts: Mutex::new(vec![vec![
                ModelEvent::ReasoningMetadata { bytes: 30 },
                usage(7, 2),
                error(),
            ]]),
            order: order.clone(),
        };
        let mut observer = Observer {
            attempts: vec![],
            order: order.clone(),
            reject: false,
        };
        assert!(
            run_model_round_observed(
                &runtime,
                request(),
                &CancellationToken::new(),
                &mut observer
            )
            .await
            .is_err()
        );
        assert_eq!(*order.lock().unwrap(), vec!["send", "persist"]);
        assert!(observer.attempts[0].ttft_ms.is_some());
        assert!(
            matches!(observer.attempts[0].error.as_ref().unwrap().delivery_state, DeliveryState::StreamInterrupted { progress } if progress.reasoning)
        );
    }
}
