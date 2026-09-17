//! One model round: stream a request, assemble the assistant message, retry
//! the same request on retryable failures.
//!
//! The provider's own retry only covers connection setup; once bytes flow, a
//! dropped stream would otherwise abort the whole run and lose all progress.

use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use leveler_model::{
    ContentPart, DeliveryState, FinishReason, Message, ModelError, ModelErrorKind, ModelEvent,
    ModelRequest, ModelRuntime, Retryability, Role, StreamProgress, TokenUsage, ToolCall,
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

/// Delay before retrying a failed model round. `attempt` is the 1-based count
/// of failures so far.
///
/// Rate limits clear on second scales: a provider-advertised `Retry-After`
/// wins (capped), otherwise exponential seconds. Transient stream/transport
/// drops usually recover immediately, so they keep fast sub-second retries.
pub(crate) fn retry_backoff_delay(error: &ModelError, attempt: u32) -> Duration {
    const MAX_ADVERTISED: Duration = Duration::from_secs(120);
    const MAX_RATE_LIMIT: Duration = Duration::from_secs(30);
    if let Some(ms) = error.retry_after_ms {
        return Duration::from_millis(ms).min(MAX_ADVERTISED);
    }
    match error.kind {
        ModelErrorKind::RateLimit => {
            let exp = 1u64 << attempt.saturating_sub(1).min(6);
            Duration::from_secs(exp).min(MAX_RATE_LIMIT)
        }
        _ => Duration::from_millis(200 * attempt.min(10) as u64),
    }
}

/// Slow-lane schedule for request-start failures whose provider-level fast
/// retries are already exhausted: re-firing 200 ms after a 4-attempt transport
/// wipeout is a guaranteed second wipeout. Seconds-scale, bounded.
pub(crate) fn exhausted_backoff_delay(error: &ModelError, attempt: u32) -> Duration {
    const MAX_ADVERTISED: Duration = Duration::from_secs(120);
    if let Some(ms) = error.retry_after_ms {
        return Duration::from_millis(ms).min(MAX_ADVERTISED);
    }
    match attempt {
        0 | 1 => Duration::from_secs(2),
        2 => Duration::from_secs(8),
        _ => Duration::from_secs(20),
    }
}

/// Cheap ±0–20% jitter (no `rand` dependency) so N concurrent runs hitting
/// the same rate limit do not retry in lockstep.
fn jittered(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    base + base.mul_f64((nanos % 200) as f64 / 1000.0)
}

/// One logical model request is attempted at most this many times before the
/// round gives up its fast/slow retry budget. Small on purpose: the transport
/// layer already retries a provably pre-delivery failure once, so this is the
/// OUTER logical budget — the two layers must not multiply into a large
/// HTTP-attempt count. After it is spent on a `Safe` failure, the round waits
/// for the network instead of failing the task.
pub const MAX_ROUND_ATTEMPTS: u32 = 3;

/// Stream one round, retrying the SAME request on a retryable error
/// (rate-limit, timeout, mid-stream interruption). Non-retryable errors and
/// cancellation propagate immediately. Each attempt starts with an explicit
/// [`AgentEvent::StreamAttemptStarted`] so retries can stream a divergent
/// prefix without corrupting consumers.
pub async fn run_model_round(
    runtime: &dyn ModelRuntime,
    request: ModelRequest,
    cancellation: &CancellationToken,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
) -> Result<ModelRound, AgentCoreError> {
    const MAX_ATTEMPTS: u32 = MAX_ROUND_ATTEMPTS;
    let mut attempt = 0u32;
    let mut exhausted_attempts = 0u32;
    let started = std::time::Instant::now();
    loop {
        attempt += 1;
        on_event(AgentEvent::StreamAttemptStarted);
        let error = match stream_round(runtime, request.clone(), cancellation, on_event).await {
            Ok(mut value) => {
                value.retry_count = attempt.saturating_sub(1);
                value.latency_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                return Ok(value);
            }
            // Cancellation is surfaced as its own error, never retried.
            Err(e) if cancellation.is_cancelled() => return Err(e),
            Err(AgentCoreError::Model(e)) => e,
            Err(e) => return Err(e),
        };
        // ONLY `Safe` is auto-retried. A `Caution` error (the provider may
        // already have generated/billed output, e.g. a stream cut after
        // partial text) or an `Unknown`-delivery error is NEVER blind-replayed:
        // re-POSTing the whole logical request would be a replay, not a
        // reconnect, and there is no protocol-level stream resume here.
        if error.retryability() != Retryability::Safe {
            return Err(AgentCoreError::Model(error));
        }
        if attempt < MAX_ATTEMPTS {
            let (wait, retry_attempt, retry_max) = if error.provider_retries_exhausted {
                exhausted_attempts += 1;
                (
                    jittered(exhausted_backoff_delay(&error, exhausted_attempts)),
                    exhausted_attempts,
                    MAX_ATTEMPTS,
                )
            } else {
                (
                    jittered(retry_backoff_delay(&error, attempt)),
                    attempt,
                    MAX_ATTEMPTS,
                )
            };
            // The retry controller emits this; presentation must never drive it.
            on_event(AgentEvent::ModelRetrying {
                attempt: retry_attempt,
                max_attempts: retry_max,
                delay_ms: wait.as_millis() as u64,
            });
            // A silent retry re-sends the whole (often huge) request and looks
            // identical to a hang from the outside. Name it.
            tracing::warn!(
                attempt,
                kind = ?error.kind,
                delivery = ?error.delivery_state,
                wait_ms = wait.as_millis() as u64,
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %error,
                "model round retrying"
            );
            // Cancellable: a user Ctrl+C during a long rate-limit wait must not
            // hang until the timer fires.
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(AgentCoreError::Cancelled),
                _ = tokio::time::sleep(wait) => {}
            }
            continue;
        }
        // The fast/slow budget is spent, and the failure is `Safe` — the
        // request provably did no provider-side work. That is exactly the
        // operation that may WAIT for the network instead of failing the
        // task. `Caution`/`Unknown` never reach here.
        return wait_for_network(
            runtime,
            request,
            cancellation,
            on_event,
            started,
            attempt,
            WAIT_INTERVAL,
        )
        .await;
    }
}

/// How often a waiting round re-attempts the transport. Deliberately low: a
/// reconnect storm is worse than the outage, and this only ever re-sends a
/// request that provably did no provider-side work. It is a parameter (not a
/// hard-coded sleep) so a test can exercise the wait without waiting seconds.
const WAIT_INTERVAL: Duration = Duration::from_secs(5);

/// Wait, at a low frequency, for a `Safe` request to become deliverable again.
///
/// The failure that got here is one that provably did no provider-side work
/// (a pre-delivery connect failure, a 429/5xx, or a stream that produced
/// nothing), so re-attempting is a reconnect, never a replay. A reconnect
/// storm is worse than the outage, so attempts are seconds apart. Cancellation
/// stops the wait at once — no request is sent after a cancel. A later failure
/// that is no longer provably `Safe` ends the wait as a surfaced error rather
/// than becoming a blind replay.
async fn wait_for_network(
    runtime: &dyn ModelRuntime,
    request: ModelRequest,
    cancellation: &CancellationToken,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
    started: std::time::Instant,
    attempt: u32,
    interval: Duration,
) -> Result<ModelRound, AgentCoreError> {
    let waiting_since = std::time::Instant::now();
    let mut wait_attempt = attempt;
    loop {
        on_event(AgentEvent::ModelWaitingForNetwork {
            elapsed_ms: waiting_since.elapsed().as_millis() as u64,
        });
        // Cancellable wait: the timer is abandoned the moment the caller cancels.
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AgentCoreError::Cancelled),
            _ = tokio::time::sleep(interval) => {}
        }
        wait_attempt += 1;
        on_event(AgentEvent::StreamAttemptStarted);
        match stream_round(runtime, request.clone(), cancellation, on_event).await {
            Ok(mut value) => {
                value.retry_count = wait_attempt.saturating_sub(1);
                value.latency_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                return Ok(value);
            }
            Err(e) if cancellation.is_cancelled() => return Err(e),
            Err(AgentCoreError::Model(e)) => {
                // Still unreachable (Safe) means keep waiting truthfully.
                // Anything else (Caution/Unknown/Never) must be surfaced.
                if e.retryability() != Retryability::Safe {
                    return Err(AgentCoreError::Model(e));
                }
                tracing::warn!(
                    kind = ?e.kind,
                    delivery = ?e.delivery_state,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "still waiting for network"
                );
            }
            Err(e) => return Err(e),
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
fn interrupted_error(mut error: ModelError, text: bool, tool_args: bool) -> ModelError {
    if matches!(
        error.kind,
        ModelErrorKind::StreamInterrupted | ModelErrorKind::Transport | ModelErrorKind::Timeout
    ) {
        error.delivery_state = DeliveryState::StreamInterrupted {
            progress: StreamProgress { text, tool_args },
        };
    }
    error
}

/// Stream one model round, preserving the provider's terminal reason. A
/// stream that ends without a terminal event is never treated as success.
async fn stream_round(
    runtime: &dyn ModelRuntime,
    request: ModelRequest,
    cancellation: &CancellationToken,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
) -> Result<ModelRound, AgentCoreError> {
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

    let mut stream = match runtime.stream(request, cancellation.child_token()).await {
        Ok(s) => s,
        Err(e) if cancellation.is_cancelled() || e.kind == ModelErrorKind::Cancelled => {
            return Err(AgentCoreError::Cancelled);
        }
        Err(e) => return Err(AgentCoreError::Model(e)),
    };

    let mut text = String::new();
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
    let mut first_event_ms: Option<u64> = None;

    // `MessageStarted` is synthesized locally by the protocol layer, so the
    // first event says nothing about the provider. What matters for a frozen
    // screen is the longest stretch with NO event at all.
    let mut event_count = 0u32;
    let mut last_event = std::time::Instant::now();
    let mut max_event_gap_ms = 0u64;
    while let Some(event) = stream.next().await {
        let gap = last_event.elapsed().as_millis() as u64;
        if gap > max_event_gap_ms {
            max_event_gap_ms = gap;
        }
        last_event = std::time::Instant::now();
        event_count += 1;
        if first_event_ms.is_none() {
            let ms = round_started.elapsed().as_millis() as u64;
            first_event_ms = Some(ms);
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
            Ok(ModelEvent::TextDelta { delta }) if !completed => {
                if !delta.is_empty() {
                    text.push_str(&delta);
                    on_event(AgentEvent::AssistantDelta(delta));
                }
            }
            Ok(ModelEvent::ReasoningDelta { delta }) if !completed => {
                if !delta.is_empty() {
                    on_event(AgentEvent::ReasoningDelta(delta));
                }
            }
            Ok(ModelEvent::ToolCallCompleted { call }) if !completed => calls.push(call),
            // A tool call the model began describing but had not finished. The
            // arguments are never joined, so the call is not executable; the
            // fact that output started is what matters for retry safety.
            Ok(ModelEvent::ToolCallStarted { .. })
            | Ok(ModelEvent::ToolCallArgumentsDelta { .. })
                if !completed =>
            {
                tool_args_started = true;
            }
            Ok(ModelEvent::MessageCompleted {
                finish_reason: reason,
            }) => {
                finish_reason = Some(reason);
                completed = true;
            }
            Ok(ModelEvent::Error { error }) => {
                // Bytes were flowing, so this is a stream interruption with
                // whatever had been produced so far attached.
                return Err(AgentCoreError::Model(interrupted_error(
                    error,
                    !text.is_empty(),
                    tool_args_started,
                )));
            }
            Ok(ModelEvent::UsageUpdated {
                usage: latest_usage,
            }) => {
                on_event(AgentEvent::Usage(latest_usage));
                usage = latest_usage;
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
                    tool_args_started,
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
            tool_args_started,
        ))
    })?;

    tracing::info!(
        request_id = %request_id,
        connect_ms,
        first_event_ms,
        total_ms = round_started.elapsed().as_millis() as u64,
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

    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(ContentPart::Text { text });
    }
    for call in calls {
        content.push(ContentPart::ToolCall { call });
    }
    Ok(ModelRound {
        request_id,
        message: Message {
            role: Role::Assistant,
            content,
        },
        usage,
        finish_reason,
        latency_ms: 0,
        retry_count: 0,
        cost_usd_micros: None,
        estimated_tokens: None,
    })
}

#[cfg(test)]
mod backoff_tests {
    use super::*;

    fn err(kind: ModelErrorKind) -> ModelError {
        ModelError::new(kind, "x")
    }

    #[test]
    fn rate_limit_backs_off_in_seconds_not_milliseconds() {
        // 200ms after a 429 is a guaranteed second 429; rate limits clear on
        // second scales.
        let d1 = retry_backoff_delay(&err(ModelErrorKind::RateLimit), 1);
        let d2 = retry_backoff_delay(&err(ModelErrorKind::RateLimit), 2);
        assert!(d1 >= Duration::from_secs(1), "attempt 1: {d1:?}");
        assert!(d2 >= d1 * 2, "attempt 2 must grow exponentially: {d2:?}");
    }

    #[test]
    fn rate_limit_honors_provider_advertised_delay() {
        let e = err(ModelErrorKind::RateLimit).with_retry_after_ms(5_000);
        assert_eq!(retry_backoff_delay(&e, 1), Duration::from_secs(5));
        // …but a hostile/buggy value is capped.
        let e = err(ModelErrorKind::RateLimit).with_retry_after_ms(3_600_000);
        assert!(retry_backoff_delay(&e, 1) <= Duration::from_secs(120));
    }

    #[test]
    fn transient_stream_errors_keep_fast_retries() {
        // A dropped stream is usually recoverable immediately; seconds-scale
        // waits would add pure latency.
        for kind in [
            ModelErrorKind::StreamInterrupted,
            ModelErrorKind::Transport,
            ModelErrorKind::Timeout,
        ] {
            let d = retry_backoff_delay(&err(kind), 1);
            assert!(
                d <= Duration::from_millis(500),
                "{kind:?} attempt 1 should stay fast: {d:?}"
            );
        }
    }

    #[test]
    fn rate_limit_backoff_is_capped() {
        let d = retry_backoff_delay(&err(ModelErrorKind::RateLimit), 10);
        assert!(d <= Duration::from_secs(30), "uncapped: {d:?}");
    }
}

#[cfg(test)]
mod retry_decision_tests {
    //! The retry decision is a function of delivery truth, not of the kind or
    //! the message alone. These tests pin the boundary: only a failure that
    //! provably did no provider-side work may be re-sent automatically, and
    //! once the budget is spent a `Safe` failure WAITS for the network.
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use leveler_model::{FinishReason, ModelRef, Role};
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// A runtime that fails with `error` for the first `fail_times` calls, then
    /// returns a minimal completed stream. Counts calls.
    struct FlakyRuntime {
        error: ModelError,
        fail_times: u32,
        calls: Arc<Mutex<u32>>,
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
            _r: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelEventStream, ModelError> {
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

    fn request() -> ModelRequest {
        ModelRequest::new(
            ModelRef::new("mock", "m"),
            vec![Message::text(Role::User, "hi")],
        )
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
        };
        let result = run_model_round(&runtime, request(), cancellation, &mut |e| out.push(e)).await;
        let n = *calls.lock().unwrap();
        (result, n)
    }

    fn not_sent() -> ModelError {
        ModelError::new(ModelErrorKind::Transport, "connect refused")
            .with_delivery_state(DeliveryState::NotSent)
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
                    text: true,
                    tool_args: false,
                },
            },
        )
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

    /// An unfinished tool call is output too: never replayed.
    #[tokio::test]
    async fn a_stream_cut_with_tool_args_is_never_auto_replayed() {
        let error = ModelError::new(ModelErrorKind::StreamInterrupted, "cut").with_delivery_state(
            DeliveryState::StreamInterrupted {
                progress: StreamProgress {
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

    #[tokio::test]
    async fn an_unknown_delivery_is_never_blindly_retried() {
        let error = ModelError::new(ModelErrorKind::Timeout, "read timed out")
            .with_delivery_state(DeliveryState::SentNoResponse);
        let mut events = Vec::new();
        let (result, calls) = run(error, 100, &CancellationToken::new(), &mut events).await;
        assert!(result.is_err());
        assert_eq!(calls, 1, "an unknown-delivery failure is not auto-replayed");
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
        assert_eq!(
            retries[0].1, MAX_ROUND_ATTEMPTS,
            "the bound travels with it"
        );
    }

    /// P0-3: a `Safe` failure whose budget is spent WAITS for the network
    /// instead of failing the task, announcing the wait as it goes.
    #[tokio::test]
    async fn a_spent_safe_budget_waits_for_the_network() {
        let cancel = CancellationToken::new();
        let mut events = Vec::new();
        // Always unreachable; cancel from another task shortly after to end the wait.
        let calls = Arc::new(Mutex::new(0));
        let runtime = FlakyRuntime {
            error: not_sent(),
            fail_times: 100,
            calls: calls.clone(),
        };
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            canceller.cancel();
        });
        let result = run_model_round(&runtime, request(), &cancel, &mut |e| events.push(e)).await;
        assert!(
            matches!(result, Err(AgentCoreError::Cancelled)),
            "cancelling the wait must end it as Cancelled: {result:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ModelWaitingForNetwork { .. })),
            "the wait must be announced"
        );
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

    /// P0-5: once the fast budget is spent, a `Safe` failure WAITS; when the
    /// transport returns, the SAME request resumes — no new turn, no user
    /// action, and nothing that had output is replayed.
    #[tokio::test]
    async fn a_safe_wait_resumes_when_the_transport_returns() {
        // The whole fast budget fails `Safe`, then the wait's attempt succeeds.
        let runtime = ScriptedRuntime::new(vec![not_sent(), not_sent(), not_sent()], 0);
        let cancel = CancellationToken::new();
        let mut events = Vec::new();
        let result = wait_for_network(
            &runtime,
            request(),
            &cancel,
            &mut |e| events.push(e),
            std::time::Instant::now(),
            MAX_ROUND_ATTEMPTS,
            Duration::from_millis(20),
        )
        .await;
        assert!(
            result.is_ok(),
            "the wait must resume once the transport returns: {result:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ModelWaitingForNetwork { .. })),
            "the wait is announced before the resume"
        );
        assert_eq!(
            *runtime.calls.lock().unwrap(),
            4,
            "three failures then the resuming attempt: no duplicate request"
        );
    }

    /// P0-5: a wait never becomes a blind replay. A later failure that is no
    /// longer provably `Safe` (delivery became unknown) ends the wait as a
    /// surfaced error instead of re-POSTing the whole request.
    #[tokio::test]
    async fn a_wait_ends_on_an_unknown_delivery_instead_of_replaying() {
        let unknown = ModelError::new(ModelErrorKind::Timeout, "read timed out")
            .with_delivery_state(DeliveryState::SentNoResponse);
        let runtime = ScriptedRuntime::new(vec![not_sent(), not_sent(), not_sent(), unknown], 0);
        let cancel = CancellationToken::new();
        let mut events = Vec::new();
        let result = wait_for_network(
            &runtime,
            request(),
            &cancel,
            &mut |e| events.push(e),
            std::time::Instant::now(),
            MAX_ROUND_ATTEMPTS,
            Duration::from_millis(20),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(AgentCoreError::Model(ref e))
                    if e.delivery_state == DeliveryState::SentNoResponse
            ),
            "an unknown delivery must end the wait as a surfaced failure: {result:?}"
        );
        assert_eq!(
            *runtime.calls.lock().unwrap(),
            4,
            "one wait attempt, then stop — never a blind replay"
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
        let result = run_model_round(&runtime, request(), &cancel, &mut |e| events.push(e)).await;
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
