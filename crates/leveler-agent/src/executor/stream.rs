use std::time::Duration;

use tokio_util::sync::CancellationToken;

use leveler_model::{ContentPart, Message, ModelError, ModelRequest, Role, TokenUsage, ToolCall};

use super::{AgentError, AgentEvent, Executor, StreamRoundResult};

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
        leveler_model::ModelErrorKind::RateLimit => {
            let exp = 1u64 << attempt.saturating_sub(1).min(6);
            Duration::from_secs(exp).min(MAX_RATE_LIMIT)
        }
        _ => Duration::from_millis(200 * attempt.min(10) as u64),
    }
}

/// Cheap ±0–20% jitter (no `rand` dependency) so N concurrent turns hitting
/// the same rate limit do not retry in lockstep.
fn jittered(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    base + base.mul_f64((nanos % 200) as f64 / 1000.0)
}

impl Executor {
    /// Run one model round over a stream and assemble the final assistant
    /// message (spec §16). Each attempt starts with an explicit reset event so
    /// retries can stream a divergent prefix without corrupting consumers.
    /// Stream a round, retrying the SAME request from history on a retryable
    /// error (rate-limit, timeout, mid-stream interruption). The provider's own
    /// retry only covers connection setup; once bytes flow, a dropped stream
    /// would otherwise abort the whole task and lose all progress. Non-retryable
    /// errors and cancellation propagate immediately.
    pub(crate) async fn stream_round_with_retry(
        &self,
        request: ModelRequest,
        observer: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<StreamRoundResult, AgentError> {
        const MAX_ATTEMPTS: u32 = 5;
        let mut attempt = 0u32;
        let started = std::time::Instant::now();
        loop {
            attempt += 1;
            observer(AgentEvent::StreamAttemptStarted);
            match self
                .stream_round(request.clone(), observer, cancellation)
                .await
            {
                Ok(mut value) => {
                    value.retry_count = attempt.saturating_sub(1);
                    value.latency_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    return Ok(value);
                }
                Err(AgentError::Model(e))
                    if e.retryable && attempt < MAX_ATTEMPTS && !cancellation.is_cancelled() =>
                {
                    let wait = jittered(retry_backoff_delay(&e, attempt));
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
                        _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
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
        &self,
        request: ModelRequest,
        observer: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<StreamRoundResult, AgentError> {
        use futures::StreamExt;
        use leveler_model::ModelEvent;

        let request_id = request.request_id.as_str().to_string();
        let mut usage = TokenUsage::default();
        let mut finish_reason = None;

        // A model round is the one thing a stuck turn is almost always waiting
        // on, and until now it left no trace at all: a turn could sit for a
        // minute with an unchanging spinner and `RUST_LOG=debug` would show
        // nothing. Timing each phase separately is what makes the wait
        // attributable — connect vs. first byte vs. streaming.
        let round_started = std::time::Instant::now();
        let message_count = request.messages.len();
        tracing::info!(
            request_id = %request_id,
            messages = message_count,
            "model round started"
        );

        let mut stream = match self
            .runtime
            .stream(request, cancellation.child_token())
            .await
        {
            Ok(s) => s,
            Err(e)
                if cancellation.is_cancelled()
                    || e.kind == leveler_model::ModelErrorKind::Cancelled =>
            {
                return Err(AgentError::Cancelled);
            }
            Err(e) => return Err(AgentError::Model(e)),
        };

        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        // Some providers (and some gateways) emit `usage` in a chunk *after*
        // `finish_reason`. Breaking at MessageCompleted drops that chunk and the
        // TUI context gauge stays stuck at "— / window". After completion we
        // only accept UsageUpdated / terminal errors until the stream ends.
        let mut completed = false;
        let connect_ms = round_started.elapsed().as_millis() as u64;
        let mut first_event_ms: Option<u64> = None;

        // `MessageStarted` is synthesized locally by the protocol layer, so the
        // first event says nothing about the provider. What matters for a frozen
        // screen is the longest stretch with NO event at all — that is exactly
        // how long the UI had nothing to redraw.
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
                return Err(AgentError::Cancelled);
            }
            match event {
                Ok(ModelEvent::TextDelta { delta }) if !completed => {
                    if !delta.is_empty() {
                        text.push_str(&delta);
                        observer(AgentEvent::AssistantDelta(delta));
                    }
                }
                Ok(ModelEvent::ReasoningDelta { delta }) if !completed => {
                    if !delta.is_empty() {
                        observer(AgentEvent::ReasoningDelta(delta));
                    }
                }
                Ok(ModelEvent::ToolCallCompleted { call }) if !completed => calls.push(call),
                Ok(ModelEvent::MessageCompleted {
                    finish_reason: reason,
                }) => {
                    finish_reason = Some(reason);
                    completed = true;
                }
                Ok(ModelEvent::Error { error }) => return Err(AgentError::Model(error)),
                Ok(ModelEvent::UsageUpdated {
                    usage: latest_usage,
                }) => {
                    observer(AgentEvent::Usage {
                        input_tokens: latest_usage.input_tokens.min(u32::MAX as u64) as u32,
                        output_tokens: latest_usage.output_tokens.min(u32::MAX as u64) as u32,
                        cached_input_tokens: latest_usage.cached_input_tokens.min(u32::MAX as u64)
                            as u32,
                    });
                    usage = latest_usage;
                }
                // Reasoning / start / partial tool-call fragments, or content
                // after completion — no assembly state we need here.
                Ok(_) => {}
                Err(e)
                    if cancellation.is_cancelled()
                        || e.kind == leveler_model::ModelErrorKind::Cancelled =>
                {
                    return Err(AgentError::Cancelled);
                }
                Err(e) => return Err(AgentError::Model(e)),
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
            AgentError::Model(ModelError::new(
                leveler_model::ModelErrorKind::StreamInterrupted,
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

        // Guarantee one final usage signal for the UI even when the provider
        // only reported tokens once mid-stream (or only after completion).
        if usage.total() > 0 {
            observer(AgentEvent::Usage {
                input_tokens: usage.input_tokens.min(u32::MAX as u64) as u32,
                output_tokens: usage.output_tokens.min(u32::MAX as u64) as u32,
                cached_input_tokens: usage.cached_input_tokens.min(u32::MAX as u64) as u32,
            });
        }

        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentPart::Text { text });
        }
        for call in calls {
            content.push(ContentPart::ToolCall { call });
        }
        Ok(StreamRoundResult {
            request_id,
            message: Message {
                role: Role::Assistant,
                content,
            },
            usage,
            finish_reason,
            latency_ms: 0,
            retry_count: 0,
        })
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
    ) -> Option<String> {
        crate::compaction::summarize_with_model(
            self.runtime.as_ref(),
            &self.model,
            self.reasoning_effort,
            messages,
            keep_recent,
            keep_recent_tokens,
            cancellation,
        )
        .await
    }
}

#[cfg(test)]
mod backoff_tests {
    use super::*;
    use leveler_model::ModelErrorKind;

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
