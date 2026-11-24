//! Opt-in observations of the web-tool lifecycle, plus an always-on bounded
//! summary of anomalous waits. The clock and batch ID are diagnostic only. No
//! policy or scheduling decision reads these values.
//!
//! Two layers share one monotonic clock:
//!
//! - `leveler_web_latency=debug` carries every phase of every web call.
//! - A call whose pre-dispatch or delivery wait exceeds
//!   [`PRE_DISPATCH_SLOW_US`] / [`DELIVERY_SLOW_US`] emits ONE warning on the
//!   `leveler::web_slow` target, with the whole phase breakdown. This is the
//!   always-on part: it needs no debug filter, allocates nothing beyond the
//!   batch's own call records, and adds no background task or durable sink.
//!
//! The summary keeps runtime waits and tool/network time in separate fields:
//! `execution_us` is the tool's own interval (HTTP included) and is never
//! reported as a scheduling wait. No tool arguments, URL, header, credential
//! or response body is recorded, at either layer.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use leveler_model::ToolCall;
use tracing::Span;

/// Calibrated from the 2026-11-24 latency diagnosis: normal
/// `registered → dispatch` was P95 45.26 ms and the worst observed admission
/// (a cold DNS lookup) was 168.44 ms. The historical gaps this mechanism must
/// catch were 1.5 s and 3.0 s. 500 ms sits ~3x above normal and ~3x below the
/// anomaly, so a normal round never fires it.
const PRE_DISPATCH_SLOW_US: u64 = 500_000;

/// The same boundary for the batch's last `execution completed → transcript
/// delivered`. Persistence and delivery are normally sub-millisecond to a few
/// milliseconds; half a second is already an anomaly worth one line.
const DELIVERY_SLOW_US: u64 = 500_000;

/// Budget scheduling inside one request (provider rate, provider, process,
/// session and host waits) is an intentional bound, so a normal queue is not
/// an anomaly. A request that waits this long for a permit is either genuine
/// multi-session saturation or a stalled/starvation signal; either way it is
/// worth one default-on line.
const NETWORK_SCHEDULING_SLOW_US: u64 = 2_000_000;

/// At most this many anomalous calls are described in full per batch. A
/// pathological round must not turn one diagnostic into an unbounded log
/// stream; the remainder is reported as one count.
const MAX_SLOW_ALERTS_PER_BATCH: usize = 4;

fn clock_us() -> u64 {
    static CLOCK: OnceLock<Instant> = OnceLock::new();
    CLOCK.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// One web call's always-on timestamps. Interior mutability is required: a
/// deferred batch marks `dispatch_queued`/`dispatch_started`/
/// `dispatch_finished` from concurrent futures. Zero means "not observed";
/// every mark is a monotonic microsecond value on the batch clock.
pub(super) struct WebCallTiming {
    tool_call_id: String,
    tool_name: String,
    /// The debug span, so the host's phase observations stay parented under
    /// the same call. `Span::none()` when the debug target is off.
    span: Span,
    registered_us: AtomicU64,
    admitted_us: AtomicU64,
    /// Reached the concurrent batch and is waiting for a network permit.
    /// Separates an intentional queue from admission/scheduling wait.
    dispatch_queued_us: AtomicU64,
    dispatch_started_us: AtomicU64,
    dispatch_finished_us: AtomicU64,
    /// Budget waits observed inside the tool (process, session, provider,
    /// provider rate and destination host). These are runtime scheduling, not
    /// HTTP time, and are subtracted from `execution_us` when reported.
    network_global_us: AtomicU64,
    network_session_us: AtomicU64,
    network_provider_us: AtomicU64,
    network_rate_us: AtomicU64,
    network_host_us: AtomicU64,
    outcome: Mutex<&'static str>,
}

impl WebCallTiming {
    fn new(call: &ToolCall, span: Span) -> Self {
        Self {
            tool_call_id: call.id.as_str().to_string(),
            tool_name: call.name.clone(),
            span,
            registered_us: AtomicU64::new(0),
            admitted_us: AtomicU64::new(0),
            dispatch_queued_us: AtomicU64::new(0),
            dispatch_started_us: AtomicU64::new(0),
            dispatch_finished_us: AtomicU64::new(0),
            network_global_us: AtomicU64::new(0),
            network_session_us: AtomicU64::new(0),
            network_provider_us: AtomicU64::new(0),
            network_rate_us: AtomicU64::new(0),
            network_host_us: AtomicU64::new(0),
            outcome: Mutex::new("pending"),
        }
    }

    fn set_outcome(&self, outcome: &'static str) {
        *self.outcome.lock().unwrap_or_else(PoisonError::into_inner) = outcome;
    }

    fn outcome(&self) -> &'static str {
        *self.outcome.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(super) struct WebBatchTrace {
    session_id: String,
    model_round_id: u32,
    tool_batch_id: String,
    started_us: u64,
    debug: bool,
    /// One slot per call in the model's batch, `Some` only for web calls, so a
    /// mark can use the original call index without a side map.
    calls: Vec<Option<WebCallTiming>>,
    delivery_begin_us: AtomicU64,
    events_flushed_us: AtomicU64,
    delivered_us: AtomicU64,
    delivery_outcome: Mutex<&'static str>,
    summarized: AtomicBool,
}

impl WebBatchTrace {
    /// `network[i]` says whether call `i` declares the typed `Network` class.
    /// The executor derives that from the registry, so this trace cannot drift
    /// from the class that actually governs admission and scheduling.
    pub(super) fn begin(
        session_id: &str,
        model_round_id: u32,
        calls: &[ToolCall],
        network: &[bool],
    ) -> Option<Self> {
        if !network.iter().any(|is_network| *is_network) {
            return None;
        }
        let debug = tracing::enabled!(target: "leveler_web_latency", tracing::Level::DEBUG);
        let started_us = clock_us();
        let tool_batch_id = leveler_core::new_uuid_string();
        let slots = calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                if !network.get(index).copied().unwrap_or(false) {
                    return None;
                }
                let span = if debug {
                    tracing::debug_span!(
                        target: "leveler_web_latency",
                        "web_tool_call",
                        session_id,
                        model_round_id,
                        tool_batch_id = tool_batch_id.as_str(),
                        tool_call_id = call.id.as_str(),
                        tool_name = call.name.as_str(),
                    )
                } else {
                    Span::none()
                };
                Some(WebCallTiming::new(call, span))
            })
            .collect();
        let batch = Self {
            session_id: session_id.into(),
            model_round_id,
            tool_batch_id,
            started_us,
            debug,
            calls: slots,
            delivery_begin_us: AtomicU64::new(0),
            events_flushed_us: AtomicU64::new(0),
            delivered_us: AtomicU64::new(0),
            delivery_outcome: Mutex::new("pending"),
            summarized: AtomicBool::new(false),
        };
        batch.mark("batch_ready", "pending");
        Some(batch)
    }

    /// The call's debug span, or `none` when tracing is off or the index is not
    /// a web call.
    pub(super) fn span(&self, index: usize) -> Span {
        self.calls
            .get(index)
            .and_then(Option::as_ref)
            .map(|call| call.span.clone())
            .unwrap_or_else(Span::none)
    }

    /// Record one phase for one call. The timing update is always on; the
    /// structured debug event is emitted only under the debug target.
    pub(super) fn phase(&self, index: usize, phase: &'static str, outcome: &'static str) {
        let Some(call) = self.calls.get(index).and_then(Option::as_ref) else {
            return;
        };
        let now = clock_us();
        match phase {
            "registered" => store(&call.registered_us, now),
            "admission_end" => store(&call.admitted_us, now),
            "dispatch_queued" => store(&call.dispatch_queued_us, now),
            "dispatch_started" => {
                // A serial call never waits in the network queue: it is ready
                // to run at the same instant it starts.
                store(&call.dispatch_queued_us, now);
                store(&call.dispatch_started_us, now);
            }
            "dispatch_returned" | "dispatch_finished" => store(&call.dispatch_finished_us, now),
            _ => {}
        }
        if outcome != "pending" {
            call.set_outcome(outcome);
        }
        if !self.debug {
            return;
        }
        tracing::debug!(
            target: "leveler_web_latency", parent: &call.span,
            phase,
            outcome,
            clock_us = now,
            batch_elapsed_us = now.saturating_sub(self.started_us),
        );
    }

    /// Record the network-budget waits one call observed. A call that never
    /// reached the HTTP boundary leaves every slot at zero.
    pub(super) fn network(&self, index: usize, trace: &leveler_tools::NetworkTrace) {
        let Some(call) = self.calls.get(index).and_then(Option::as_ref) else {
            return;
        };
        store(&call.network_global_us, trace.global_queue_us());
        store(&call.network_session_us, trace.session_queue_us());
        store(&call.network_provider_us, trace.provider_queue_us());
        store(&call.network_rate_us, trace.provider_rate_us());
        store(&call.network_host_us, trace.host_queue_us());
    }

    /// Record a batch-level phase. The delivery timestamps are batch facts:
    /// every call in the batch is delivered by the same flush + append.
    pub(super) fn mark(&self, phase: &'static str, outcome: &'static str) {
        let now = clock_us();
        match phase {
            "delivery_begin" => store(&self.delivery_begin_us, now),
            "delivery_events_flushed" => store(&self.events_flushed_us, now),
            "delivered" => {
                store(&self.delivered_us, now);
                *self
                    .delivery_outcome
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = outcome;
            }
            _ => {}
        }
        if !self.debug {
            return;
        }
        tracing::debug!(
            target: "leveler_web_latency",
            session_id = self.session_id.as_str(),
            model_round_id = self.model_round_id,
            tool_batch_id = self.tool_batch_id.as_str(),
            phase,
            outcome,
            clock_us = now,
            batch_elapsed_us = now.saturating_sub(self.started_us),
        );
    }

    /// Emit one bounded summary line per anomalously slow call. Called from
    /// `Drop`, so an early return (fatal admission, cancellation) still
    /// reports what was observed.
    fn summarize_slow(&self) {
        let delivered = self.delivered_us.load(Ordering::Relaxed);
        let delivery_begin = self.delivery_begin_us.load(Ordering::Relaxed);
        let events_flushed = self.events_flushed_us.load(Ordering::Relaxed);
        // Delivery is one batch fact. Measuring it from THIS call's finish
        // would charge a fast call for waiting on its slower sibling — normal
        // network variance, not a runtime delivery defect. Anchor the window
        // at the batch's last observed finish and report the sibling wait
        // separately.
        let batch_last_finished = self
            .calls
            .iter()
            .flatten()
            .map(|call| call.dispatch_finished_us.load(Ordering::Relaxed))
            .max()
            .unwrap_or(0);
        let mut reported = 0usize;
        let mut suppressed = 0usize;
        for (index, call) in self.calls.iter().enumerate() {
            let Some(call) = call else {
                continue;
            };
            let registered = call.registered_us.load(Ordering::Relaxed);
            let dispatch_started = call.dispatch_started_us.load(Ordering::Relaxed);
            // A call that never reached execution has no pre-dispatch window
            // to judge: refused/cancelled calls are not latency samples.
            if registered == 0 || dispatch_started == 0 {
                continue;
            }
            let admitted = call.admitted_us.load(Ordering::Relaxed);
            let queued = call.dispatch_queued_us.load(Ordering::Relaxed);
            let finished = call.dispatch_finished_us.load(Ordering::Relaxed);
            // Pre-dispatch ends when the call REACHES the batch. Waiting for a
            // network permit after that is the intentional bound, not an
            // anomalous runtime wait: a width-1 control queues every request,
            // and a round above the ceiling queues the extras by design.
            let ready = if queued != 0 {
                queued
            } else {
                dispatch_started
            };
            let pre_dispatch = ready.saturating_sub(registered);
            let network_queue = dispatch_started.saturating_sub(ready);
            // Budget waits happen inside the tool, so the raw dispatch
            // interval contains them. Report them as scheduling, and leave
            // `execution_us` as the time the request actually spent on DNS,
            // connection and HTTP rather than charging it for a permit.
            let scheduling = call.network_global_us.load(Ordering::Relaxed)
                + call.network_session_us.load(Ordering::Relaxed)
                + call.network_provider_us.load(Ordering::Relaxed)
                + call.network_rate_us.load(Ordering::Relaxed)
                + call.network_host_us.load(Ordering::Relaxed);
            let execution = finished
                .saturating_sub(dispatch_started)
                .saturating_sub(scheduling);
            let sibling_wait = if finished != 0 && batch_last_finished != 0 {
                batch_last_finished.saturating_sub(finished)
            } else {
                0
            };
            // The delivery alarm covers the runtime's own delivery path: flush
            // plus transcript append. The queue from the last execution to the
            // start of delivery is reported separately, because a round can
            // legitimately spend that interval in a non-web serial call
            // (`web_fetch` followed by a 30 s command) that this trace must not
            // charge to persistence.
            let delivery = if delivery_begin != 0 && delivered != 0 {
                delivered.saturating_sub(delivery_begin)
            } else {
                0
            };
            let slow_pre = pre_dispatch >= PRE_DISPATCH_SLOW_US;
            let slow_delivery = delivery >= DELIVERY_SLOW_US;
            let slow_scheduling = scheduling >= NETWORK_SCHEDULING_SLOW_US;
            if !slow_pre && !slow_delivery && !slow_scheduling {
                continue;
            }
            let slow_phase = match (slow_pre, slow_delivery, slow_scheduling) {
                (true, true, true) => "pre_dispatch+delivery+network_scheduling",
                (true, true, false) => "pre_dispatch+delivery",
                (true, false, true) => "pre_dispatch+network_scheduling",
                (false, true, true) => "delivery+network_scheduling",
                (true, false, false) => "pre_dispatch",
                (false, true, false) => "delivery",
                (false, false, true) => "network_scheduling",
                (false, false, false) => unreachable!(),
            };
            if reported >= MAX_SLOW_ALERTS_PER_BATCH {
                suppressed += 1;
                continue;
            }
            reported += 1;
            // The whole breakdown goes out together: one line an operator can
            // attribute without enabling the debug target. `execution_us` is
            // tool/HTTP time and is explicitly not claimed as runtime wait.
            tracing::warn!(
                target: "leveler::web_slow",
                session_id = self.session_id.as_str(),
                model_round_id = self.model_round_id,
                tool_batch_id = self.tool_batch_id.as_str(),
                tool_call_id = call.tool_call_id.as_str(),
                tool_name = call.tool_name.as_str(),
                call_index = index,
                batch_calls = self.calls.len(),
                slow_phase,
                outcome = call.outcome(),
                delivery_outcome = *self
                    .delivery_outcome
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
                pre_dispatch_us = pre_dispatch,
                admission_us = admitted.saturating_sub(registered),
                dispatch_wait_us = ready.saturating_sub(admitted),
                network_queue_us = network_queue,
                network_scheduling_us = scheduling,
                network_global_queue_us = call.network_global_us.load(Ordering::Relaxed),
                network_session_queue_us = call.network_session_us.load(Ordering::Relaxed),
                network_provider_queue_us = call.network_provider_us.load(Ordering::Relaxed),
                network_provider_rate_us = call.network_rate_us.load(Ordering::Relaxed),
                network_host_queue_us = call.network_host_us.load(Ordering::Relaxed),
                execution_us = execution,
                sibling_wait_us = sibling_wait,
                delivery_us = delivery,
                delivery_queue_us = delivery_begin.saturating_sub(batch_last_finished),
                delivery_flush_us = events_flushed.saturating_sub(delivery_begin),
                delivery_transcript_us = delivered.saturating_sub(events_flushed),
                "web tool call exceeded its wait budget"
            );
        }
        if suppressed > 0 {
            tracing::warn!(
                target: "leveler::web_slow",
                session_id = self.session_id.as_str(),
                model_round_id = self.model_round_id,
                tool_batch_id = self.tool_batch_id.as_str(),
                suppressed,
                batch_calls = self.calls.len(),
                "additional slow web calls were not expanded"
            );
        }
    }
}

impl Drop for WebBatchTrace {
    fn drop(&mut self) {
        if self.summarized.swap(true, Ordering::Relaxed) {
            return;
        }
        self.summarize_slow();
    }
}

fn store(slot: &AtomicU64, value: u64) {
    // First observation wins: a phase is marked once on the path that owns it.
    let _ = slot.compare_exchange(0, value, Ordering::Relaxed, Ordering::Relaxed);
}

/// Records an observed wait in the host. Capture the span, never hold an
/// entered span guard across an await, and mark a dropped wait as interrupted.
pub(super) struct WebPhase {
    span: Span,
    phase: &'static str,
    started_us: u64,
    outcome: &'static str,
}

impl WebPhase {
    pub(super) fn begin(phase: &'static str) -> Option<Self> {
        if !tracing::enabled!(target: "leveler_web_latency", tracing::Level::DEBUG) {
            return None;
        }
        let span = Span::current();
        if span
            .metadata()
            .is_none_or(|meta| meta.name() != "web_tool_call")
        {
            return None;
        }
        Some(Self {
            span,
            phase,
            started_us: clock_us(),
            outcome: "interrupted",
        })
    }

    pub(super) fn finish(wait: Option<Self>, outcome: &'static str) {
        if let Some(mut wait) = wait {
            wait.outcome = outcome;
        }
    }

    pub(super) async fn result<T, E>(
        phase: &'static str,
        wait: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let timer = Self::begin(phase);
        let result = wait.await;
        Self::finish(timer, if result.is_ok() { "ok" } else { "error" });
        result
    }
}

impl Drop for WebPhase {
    fn drop(&mut self) {
        let now = clock_us();
        tracing::debug!(
            target: "leveler_web_latency", parent: &self.span,
            phase = self.phase,
            outcome = self.outcome,
            started_clock_us = self.started_us,
            clock_us = now,
            elapsed_us = now.saturating_sub(self.started_us),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_core::ToolCallId;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(id),
            name: name.to_string(),
            arguments: serde_json::json!({"url": "https://example.test/secret-token"}),
        }
    }

    /// Test-side equivalent of the executor's registry lookup. The production
    /// path derives these flags from `ParallelClass::Network`; tests that only
    /// need the trace shape name the two web tools directly.
    fn begin(round: u32, calls: &[ToolCall]) -> Option<WebBatchTrace> {
        let network: Vec<bool> = calls
            .iter()
            .map(|call| matches!(call.name.as_str(), "web_fetch" | "web_search"))
            .collect();
        WebBatchTrace::begin("s", round, calls, &network)
    }

    #[test]
    fn a_round_without_web_calls_creates_no_batch() {
        assert!(begin(1, &[call("c1", "read_file")]).is_none());
    }

    #[test]
    fn a_web_batch_tracks_only_web_slots_and_keeps_call_identity() {
        let batch = begin(2, &[call("c1", "read_file"), call("c2", "web_fetch")])
            .expect("a web call creates a batch");
        assert!(batch.calls[0].is_none());
        let web = batch.calls[1].as_ref().expect("web slot");
        assert_eq!(web.tool_call_id, "c2");
        assert_eq!(web.tool_name, "web_fetch");
    }

    #[test]
    fn phases_record_the_first_observation_only() {
        let batch = begin(1, &[call("c1", "web_fetch")]).expect("batch");
        let slot = batch.calls[0].as_ref().unwrap();
        slot.registered_us.store(41, Ordering::Relaxed);
        batch.phase(0, "registered", "pending");
        assert_eq!(
            slot.registered_us.load(Ordering::Relaxed),
            41,
            "a later observation must not move the origin"
        );
    }

    #[test]
    fn a_call_that_never_dispatched_is_not_a_latency_sample() {
        let batch = begin(1, &[call("c1", "web_fetch")]).expect("batch");
        batch.phase(0, "registered", "pending");
        batch.phase(0, "admission_end", "refused");
        assert!(!batch.is_slow_sample(0), "refused call must not alert");
    }

    /// A call queued behind the network-bound permit is waiting by design.
    /// Its pre-dispatch window ends when it reaches the batch; the permit wait
    /// is reported separately and must not raise the slow pre-dispatch alarm.
    #[test]
    fn a_network_queue_wait_is_not_a_slow_pre_dispatch_sample() {
        let batch = begin(1, &[call("c1", "web_fetch")]).expect("batch");
        let slot = batch.calls[0].as_ref().unwrap();
        slot.registered_us.store(1, Ordering::Relaxed);
        slot.admitted_us.store(4, Ordering::Relaxed);
        slot.dispatch_queued_us.store(10, Ordering::Relaxed);
        slot.dispatch_started_us
            .store(PRE_DISPATCH_SLOW_US + 100, Ordering::Relaxed);
        assert!(
            !batch.is_pre_dispatch_slow(0),
            "admission + reaching the batch was 9 us; the permit wait is the bound working"
        );
    }

    #[test]
    fn a_slow_pre_dispatch_call_is_reported_as_pre_dispatch_not_execution() {
        let batch = begin(1, &[call("c1", "web_fetch")]).expect("batch");
        let slot = batch.calls[0].as_ref().unwrap();
        slot.registered_us.store(1, Ordering::Relaxed);
        slot.admitted_us.store(7, Ordering::Relaxed);
        slot.dispatch_queued_us
            .store(PRE_DISPATCH_SLOW_US + 2, Ordering::Relaxed);
        slot.dispatch_started_us
            .store(PRE_DISPATCH_SLOW_US + 3, Ordering::Relaxed);
        assert!(batch.is_slow_sample(0));
        assert!(batch.is_pre_dispatch_slow(0));
        // The tool interval must not be read as the slow phase.
        assert!(!batch.is_delivery_slow(0));
    }

    #[test]
    fn a_fast_sibling_waiting_for_a_slow_request_is_not_a_delivery_alert() {
        let batch = begin(1, &[call("c1", "web_fetch"), call("c2", "web_fetch")]).expect("batch");
        for (index, finished) in [(0usize, 100u64), (1, 900_000)] {
            let slot = batch.calls[index].as_ref().unwrap();
            slot.registered_us.store(1, Ordering::Relaxed);
            slot.admitted_us.store(2, Ordering::Relaxed);
            slot.dispatch_started_us.store(3, Ordering::Relaxed);
            slot.dispatch_finished_us.store(finished, Ordering::Relaxed);
        }
        // `delivered - delivery_begin` is the real flush + append window: the
        // first call waited ~900 ms for its sibling, which is the sibling's own
        // latency, and the 10 ms after the last finish is the delivery work.
        batch.delivery_begin_us.store(910_000, Ordering::Relaxed);
        batch.delivered_us.store(920_000, Ordering::Relaxed);
        assert!(!batch.is_delivery_slow(0));
        assert!(!batch.is_delivery_slow(1));
        // A genuinely slow delivery path after its own start is still
        // reported; the queue before delivery remains a separate field.
        batch.delivery_begin_us.store(910_000, Ordering::Relaxed);
        batch.delivered_us.store(2_500_000, Ordering::Relaxed);
        assert!(batch.is_delivery_slow(0));
        assert!(batch.is_delivery_slow(1));
    }

    /// A round may run `web_fetch` and then a slow serial command. The web
    /// call finished long before delivery starts, but the interval belongs to
    /// that other tool, not to persistence; only the flush + append window is
    /// a delivery sample.
    #[test]
    fn a_slow_non_web_call_between_the_batch_and_delivery_is_not_a_delivery_alert() {
        let batch = begin(1, &[call("c1", "web_fetch")]).expect("batch");
        let slot = batch.calls[0].as_ref().unwrap();
        slot.registered_us.store(1, Ordering::Relaxed);
        slot.admitted_us.store(2, Ordering::Relaxed);
        slot.dispatch_started_us.store(3, Ordering::Relaxed);
        slot.dispatch_finished_us.store(200_000, Ordering::Relaxed);
        // The command ran for 30 s; delivery itself then took 2 ms.
        batch.delivery_begin_us.store(30_200_000, Ordering::Relaxed);
        batch.delivered_us.store(30_202_000, Ordering::Relaxed);
        assert!(
            !batch.is_delivery_slow(0),
            "the 30 s belonged to the serial command, not the delivery path"
        );
    }

    impl WebBatchTrace {
        fn is_slow_sample(&self, index: usize) -> bool {
            self.is_pre_dispatch_slow(index) || self.is_delivery_slow(index)
        }

        fn is_pre_dispatch_slow(&self, index: usize) -> bool {
            let Some(call) = self.calls.get(index).and_then(Option::as_ref) else {
                return false;
            };
            let registered = call.registered_us.load(Ordering::Relaxed);
            let queued = call.dispatch_queued_us.load(Ordering::Relaxed);
            let started = call.dispatch_started_us.load(Ordering::Relaxed);
            let ready = if queued != 0 { queued } else { started };
            registered != 0
                && started != 0
                && ready.saturating_sub(registered) >= PRE_DISPATCH_SLOW_US
        }

        fn is_delivery_slow(&self, _index: usize) -> bool {
            let begin = self.delivery_begin_us.load(Ordering::Relaxed);
            let delivered = self.delivered_us.load(Ordering::Relaxed);
            begin != 0 && delivered != 0 && delivered.saturating_sub(begin) >= DELIVERY_SLOW_US
        }
    }
}
