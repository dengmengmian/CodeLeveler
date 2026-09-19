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

/// Automatic retries after the first attempt fails transiently. The logical
/// request is therefore attempted at most `1 + MAX_RETRIES` times.
///
/// This is the ONE owner of a model request's retry lifecycle. The provider
/// transport still performs a single *fast* pre-delivery retry (a connect
/// failure is cheap and common); that is a transport optimization invisible to
/// this lifecycle, and its attempts are not counted here — so the two layers
/// never multiply into `MAX_RETRIES` distinct logical waits.
pub const MAX_RETRIES: u32 = 10;

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

    fn delay(&self, error: &ModelError, retry: u32) -> Duration {
        if self.delay_scale <= 0.0 {
            return Duration::ZERO;
        }
        let base = retry_backoff_delay(error, retry);
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

/// Whether the logical round may make another bounded attempt.
///
/// `Safe` remains the normal delivery-truth rule. The one runtime-level
/// exception is a send-boundary transport failure whose delivery is unknown:
/// at this point no response stream exists, so the round may recover from the
/// transient fault within its fixed budget. Once a stream exists,
/// [`stream_round`] rewrites transport failures to `StreamInterrupted`; partial
/// output therefore remains `Caution` and is never replayed.
fn should_retry_round(error: &ModelError) -> bool {
    error.retryability() == Retryability::Safe
        || (error.kind == ModelErrorKind::Transport
            && error.delivery_state == DeliveryState::Unknown)
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
    let mut retries = 0u32;
    let started = std::time::Instant::now();
    loop {
        on_event(AgentEvent::StreamAttemptStarted);
        let error = match stream_round(runtime, request.clone(), cancellation, on_event).await {
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
            Err(e) if cancellation.is_cancelled() => return Err(e),
            Err(AgentCoreError::Model(e)) => e,
            Err(e) => return Err(e),
        };
        // Safe delivery failures and the narrow pre-stream/send transport
        // exception are retried. A `Caution` error (the provider may already
        // have generated/billed output, e.g. a stream cut after partial text)
        // is never blind-replayed: there is no protocol-level stream resume.
        if !should_retry_round(&error) {
            return Err(AgentCoreError::Model(error));
        }
        if retries >= policy.max_retries {
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
            return Err(AgentCoreError::Model(error));
        }
        retries += 1;
        let wait = policy.delay(&error, retries);
        // The retry controller emits this; presentation must never drive it.
        on_event(AgentEvent::ModelRetrying {
            attempt: retries,
            max_attempts: policy.max_retries,
            delay_ms: wait.as_millis() as u64,
        });
        // A silent retry re-sends the whole (often huge) request and looks
        // identical to a hang from the outside. Name it.
        tracing::warn!(
            request_id = %request.request_id,
            retry = retries,
            max_retries = policy.max_retries,
            kind = ?error.kind,
            delivery = ?error.delivery_state,
            wait_ms = wait.as_millis() as u64,
            elapsed_ms = started.elapsed().as_millis() as u64,
            error = %error,
            "model round retrying"
        );
        // Cancellable: a user Ctrl+C during a long backoff must not hang until
        // the timer fires.
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AgentCoreError::Cancelled),
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

    use leveler_model::{FinishReason, ModelRef, Role};
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
                    text: true,
                    tool_args: false,
                },
            },
        )
    }

    /// A retry re-sends the round's request as built — the tool exchange in
    /// it is not rebuilt, trimmed or reordered between attempts.
    #[tokio::test]
    async fn a_retried_round_resends_the_same_tool_exchange() {
        let exchange = vec![
            Message::text(Role::User, "hi"),
            Message {
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
