//! One model round: stream a request, assemble the assistant message, retry
//! the same request on retryable failures.
//!
//! The provider's own retry only covers connection setup; once bytes flow, a
//! dropped stream would otherwise abort the whole run and lose all progress.

use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelErrorKind, ModelEvent, ModelRequest,
    ModelRuntime, Role, TokenUsage, ToolCall,
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
    const MAX_ATTEMPTS: u32 = 5;
    // Separate, smaller budget for retryable-KIND errors whose provider fast
    // retries are exhausted (request-start timeouts etc.). Worst case total
    // = provider attempts × this budget, still bounded.
    const MAX_EXHAUSTED_ATTEMPTS: u32 = 3;
    let mut attempt = 0u32;
    let mut exhausted_attempts = 0u32;
    let started = std::time::Instant::now();
    loop {
        attempt += 1;
        on_event(AgentEvent::StreamAttemptStarted);
        match stream_round(runtime, request.clone(), cancellation, on_event).await {
            Ok(mut value) => {
                value.retry_count = attempt.saturating_sub(1);
                value.latency_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                return Ok(value);
            }
            Err(AgentCoreError::Model(e))
                if e.retryable
                    && !cancellation.is_cancelled()
                    && if e.provider_retries_exhausted {
                        exhausted_attempts < MAX_EXHAUSTED_ATTEMPTS
                    } else {
                        attempt < MAX_ATTEMPTS
                    } =>
            {
                let wait = if e.provider_retries_exhausted {
                    exhausted_attempts += 1;
                    jittered(exhausted_backoff_delay(&e, exhausted_attempts))
                } else {
                    jittered(retry_backoff_delay(&e, attempt))
                };
                // A silent retry re-sends the whole (often huge) request and
                // looks identical to a hang from the outside. Name it.
                tracing::warn!(
                    attempt,
                    kind = ?e.kind,
                    wait_ms = wait.as_millis() as u64,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error = %e,
                    "model round retrying"
                );
                // Cancellable: a user Ctrl+C during a long rate-limit wait
                // must not hang until the timer fires.
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(AgentCoreError::Cancelled),
                    _ = tokio::time::sleep(wait) => {}
                }
            }
            Err(e) => return Err(e),
        }
    }
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
            Ok(ModelEvent::MessageCompleted {
                finish_reason: reason,
            }) => {
                finish_reason = Some(reason);
                completed = true;
            }
            Ok(ModelEvent::Error { error }) => return Err(AgentCoreError::Model(error)),
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
            Err(e) => return Err(AgentCoreError::Model(e)),
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
        AgentCoreError::Model(ModelError::new(
            ModelErrorKind::StreamInterrupted,
            "model stream ended without a terminal completion event",
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
