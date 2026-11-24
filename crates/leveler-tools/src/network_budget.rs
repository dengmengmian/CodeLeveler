//! Process-wide budget for outbound web requests.
//!
//! The executor bounds one model round (`max_parallel_tools`, at most three
//! network calls). That is a batch bound, not a process bound: several
//! sessions, goals, background tasks and scheduled runs can each start a
//! batch, so the total number of sockets one process opens must be governed
//! here, at the single boundary every request crosses.
//!
//! Layers, acquired in this order:
//!
//! 1. provider rate (search only; no concurrency permit is held while waiting)
//! 2. provider in-flight (search only)
//! 3. session in-flight
//! 4. process in-flight
//! 5. destination host in-flight (per hop)
//!
//! The session layer precedes the process layer so a request queued on its
//! session's cap cannot occupy a process permit: a single session can hold at
//! most [`SESSION_IN_FLIGHT`] process permits, even when it has more requests
//! waiting. The host layer must stay last: a redirect acquires the next
//! host's permit while it already holds the request-wide layers, so any other
//! order would let one request hold a host permit while waiting for a process
//! permit and another hold a process permit while waiting for that host — a
//! cycle.
//!
//! Permits are owned by the future that awaited them; dropping that future
//! releases every permit it held. Semaphores are FIFO, so a later session is
//! not starved by an earlier burst. One request acquires each layer at most
//! once; a redirect acquires only a new host permit for its new hop.
//!
//! These are runtime resource bounds, not permissions and not vendor quotas.
//! A budget wait delays a request; it never grants a destination that
//! admission or the SSRF checks denied. The provider rate is CodeLeveler's
//! own runaway guard: a provider's real quota is external and its 429 stays
//! the authoritative signal.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Concurrent outbound requests one process may have in flight. Two full
/// rounds (the executor's network width is three) can overlap, plus two
/// requests from other sessions: enough for a handful of sessions to make
/// progress together without letting a hundred-call loop open a hundred
/// sockets.
pub(crate) const GLOBAL_IN_FLIGHT: usize = 8;

/// One session may keep at most this many requests in flight. A normal round
/// asks for at most three, so the cap only binds when one session runs a
/// foreground turn, a background task and a scheduled run at once; it exists
/// so those can never consume the whole process budget. This permit is
/// acquired before the process permit, so a request queued on this cap holds
/// no process permit and cannot starve another session.
pub(crate) const SESSION_IN_FLIGHT: usize = 4;

/// Requests in flight against one destination host (host + port). Four lets
/// one full round plus a second session's request overlap.
///
/// The host layer is acquired last. That is what keeps redirects deadlock-free:
/// a request that moves to another host must acquire that host's permit while
/// holding its process permit, so every request has to acquire the host layer
/// after the request-wide layers, never before. The trade-off is that a
/// saturated host can transiently occupy process permits with its queue; the
/// in-flight requests still finish (or time out) and release them, so this is
/// bounded contention, not permanent starvation.
pub(crate) const HOST_IN_FLIGHT: usize = 4;

/// Requests in flight against one search provider. Providers are separate
/// accounts; two concurrent searches is conservative and leaves the round
/// width for fetches.
pub(crate) const PROVIDER_IN_FLIGHT: usize = 2;

/// Search starts allowed in one rolling second, per provider. This is a
/// runaway guard, deliberately not a claim about any vendor's published
/// limit: an interactive round starts at most a handful, and a sustained
/// programmatic loop is what this bounds. A provider's own 429 remains the
/// authoritative quota signal.
pub(crate) const PROVIDER_STARTS_PER_SECOND: usize = 20;

const PROVIDER_RATE_WINDOW: Duration = Duration::from_secs(1);

/// The class of outbound request, as declared by the tool that made it.
#[derive(Clone, Copy)]
pub(crate) enum RequestClass<'a> {
    /// A direct document fetch.
    Fetch,
    /// A search provider call, keyed by provider so two providers never share
    /// one account's budget.
    Search { provider: &'a str },
}

impl<'a> RequestClass<'a> {
    fn provider(self) -> Option<&'a str> {
        match self {
            Self::Fetch => None,
            Self::Search { provider } => Some(provider),
        }
    }
}

/// Wait observations for one tool call. Purely diagnostic: no scheduling
/// decision reads these values.
#[derive(Debug, Default)]
pub struct NetworkTrace {
    global_us: AtomicU64,
    session_us: AtomicU64,
    provider_us: AtomicU64,
    provider_rate_us: AtomicU64,
    host_us: AtomicU64,
}

impl NetworkTrace {
    pub fn global_queue_us(&self) -> u64 {
        self.global_us.load(Ordering::Relaxed)
    }

    pub fn session_queue_us(&self) -> u64 {
        self.session_us.load(Ordering::Relaxed)
    }

    pub fn provider_queue_us(&self) -> u64 {
        self.provider_us.load(Ordering::Relaxed)
    }

    pub fn provider_rate_us(&self) -> u64 {
        self.provider_rate_us.load(Ordering::Relaxed)
    }

    pub fn host_queue_us(&self) -> u64 {
        self.host_us.load(Ordering::Relaxed)
    }

    /// Sum of the sequential budget waits. The layers never overlap, so this
    /// is the time the request spent waiting for a permit rather than talking
    /// to the network.
    pub fn scheduling_us(&self) -> u64 {
        self.global_queue_us()
            + self.session_queue_us()
            + self.provider_queue_us()
            + self.provider_rate_us()
            + self.host_queue_us()
    }

    fn add(slot: &AtomicU64, elapsed: Duration) {
        slot.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
    }
}

/// Which budget layer a measurement belongs to. The values are stable for the
/// test-support sampler; they are not a wire format.
pub const LAYER_GLOBAL: u8 = 0;
pub const LAYER_SESSION: u8 = 1;
pub const LAYER_PROVIDER: u8 = 2;
pub const LAYER_PROVIDER_RATE: u8 = 3;
pub const LAYER_HOST: u8 = 4;

/// Test-support sampler: every budget wait, per layer. Compiled only with the
/// test-support feature, so a production binary carries no collector at all.
#[cfg(feature = "test-support")]
mod metrics {
    use std::sync::{Mutex, PoisonError};

    const MAX_SAMPLES: usize = 1_000_000;
    static SAMPLES: Mutex<Vec<(u8, u64)>> = Mutex::new(Vec::new());

    pub(super) fn record(layer: u8, wait_us: u64) {
        let mut samples = SAMPLES.lock().unwrap_or_else(PoisonError::into_inner);
        if samples.len() < MAX_SAMPLES {
            samples.push((layer, wait_us));
        }
    }

    pub(super) fn take() -> Vec<(u8, u64)> {
        std::mem::take(&mut *SAMPLES.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Take every budget wait observed in this process, as `(layer, wait_us)`.
/// Test-support only; used by the multi-session measurement harness.
#[cfg(feature = "test-support")]
pub fn take_budget_wait_samples() -> Vec<(u8, u64)> {
    metrics::take()
}

/// Discard stale samples before a measurement window.
#[cfg(feature = "test-support")]
pub fn clear_budget_wait_samples() {
    let _ = metrics::take();
}

#[inline]
fn record_layer_wait(layer: u8, wait: Duration) {
    #[cfg(feature = "test-support")]
    metrics::record(layer, wait.as_micros() as u64);
    #[cfg(not(feature = "test-support"))]
    let _ = (layer, wait);
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    global: usize,
    per_session: usize,
    per_provider: usize,
    provider_starts_per_second: usize,
    per_host: usize,
}

impl Limits {
    const fn production() -> Self {
        Self {
            global: GLOBAL_IN_FLIGHT,
            per_session: SESSION_IN_FLIGHT,
            per_provider: PROVIDER_IN_FLIGHT,
            provider_starts_per_second: PROVIDER_STARTS_PER_SECOND,
            per_host: HOST_IN_FLIGHT,
        }
    }
}

type KeyedMap = Arc<Mutex<HashMap<String, Arc<Semaphore>>>>;

fn keyed_map() -> KeyedMap {
    Arc::new(Mutex::new(HashMap::new()))
}

fn lock_map<'a>(map: &'a KeyedMap) -> std::sync::MutexGuard<'a, HashMap<String, Arc<Semaphore>>> {
    map.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An owned permit for a lazily created per-key semaphore. When the last
/// in-flight request for an idle key releases, the entry is removed so a long
/// daemon does not retain one semaphore per host or session ever seen.
#[derive(Debug)]
pub(crate) struct KeyedPermit {
    map: KeyedMap,
    key: String,
    semaphore: Arc<Semaphore>,
    permit: Option<OwnedSemaphorePermit>,
}

impl KeyedPermit {
    async fn acquire(map: KeyedMap, key: String, capacity: usize) -> Self {
        let semaphore = {
            let mut entries = lock_map(&map);
            entries
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(capacity)))
                .clone()
        };
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("network budget semaphore is never closed");
        Self {
            map,
            key,
            semaphore,
            permit: Some(permit),
        }
    }
}

impl Drop for KeyedPermit {
    fn drop(&mut self) {
        // Release before deciding the gate is idle: otherwise a new request
        // could create a second semaphore while this permit still counted
        // against the first one.
        drop(self.permit.take());
        let mut entries = lock_map(&self.map);
        if entries
            .get(&self.key)
            .is_some_and(|semaphore| Arc::ptr_eq(semaphore, &self.semaphore))
            // The map and this guard are the only owners; a waiter or another
            // in-flight request would hold a clone and keep the entry.
            && Arc::strong_count(&self.semaphore) == 2
        {
            entries.remove(&self.key);
        }
    }
}

/// A rolling-window reservation queue for one provider.
struct RateGate {
    slots: Mutex<RateSlots>,
    capacity: usize,
}

struct RateSlots {
    /// Reserved start instants, ordered by start, with the reservation id so a
    /// cancelled waiter removes its own slot rather than someone else's.
    slots: std::collections::VecDeque<(u64, Instant)>,
    next_id: u64,
}

impl RateGate {
    fn new(capacity: usize) -> Self {
        Self {
            slots: Mutex::new(RateSlots {
                slots: std::collections::VecDeque::new(),
                next_id: 0,
            }),
            capacity,
        }
    }

    /// Reserve the next allowed start. The reservation is FIFO, so a burst
    /// cannot starve a later caller, and it is released if the waiter is
    /// dropped before its start.
    fn reserve(&self) -> RateWait<'_> {
        let now = Instant::now();
        let mut state = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        while state
            .slots
            .front()
            .is_some_and(|(_, start)| now.duration_since(*start) >= PROVIDER_RATE_WINDOW)
        {
            state.slots.pop_front();
        }
        let start = if state.slots.len() < self.capacity {
            now
        } else {
            let anchor = state.slots[state.slots.len() - self.capacity].1;
            (anchor + PROVIDER_RATE_WINDOW).max(now)
        };
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        // Keep the queue ordered by start. A cancellation can remove a future
        // reservation and let the next caller take the immediate path; that
        // caller's `now` must not be appended behind a later start, or the
        // window purge and the capacity anchor would read a future slot as the
        // oldest one and over-throttle the provider.
        let position = state
            .slots
            .iter()
            .position(|(_, slot_start)| *slot_start > start)
            .unwrap_or(state.slots.len());
        state.slots.insert(position, (id, start));
        RateWait {
            gate: self,
            id,
            start,
            committed: false,
        }
    }
}

struct RateWait<'a> {
    gate: &'a RateGate,
    id: u64,
    start: Instant,
    committed: bool,
}

impl RateWait<'_> {
    async fn wait(mut self) {
        let now = Instant::now();
        let delay = self.start.saturating_duration_since(now);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        // The reservation stays in the window: it is a request start, and a
        // later caller must still see it. Only a waiter dropped before this
        // point removes its slot.
        self.committed = true;
    }
}

impl Drop for RateWait<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut state = self
            .gate
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(position) = state.slots.iter().position(|(id, _)| *id == self.id) {
            state.slots.remove(position);
        }
    }
}

struct ProviderGate {
    concurrency: Arc<Semaphore>,
    rate: RateGate,
}

/// One process's network budget. Production uses [`Self::global`]; tests
/// construct a local instance so limits can be exercised without throttling
/// every other test in the binary.
pub(crate) struct NetworkBudget {
    limits: Limits,
    global: Arc<Semaphore>,
    sessions: KeyedMap,
    hosts: KeyedMap,
    providers: Mutex<HashMap<String, Arc<ProviderGate>>>,
}

impl NetworkBudget {
    fn new(limits: Limits) -> Self {
        Self {
            limits,
            global: Arc::new(Semaphore::new(limits.global)),
            sessions: keyed_map(),
            hosts: keyed_map(),
            providers: Mutex::new(HashMap::new()),
        }
    }

    /// The one budget every web request in this process shares. Its lifetime
    /// is the process, independent of any client, window or turn.
    pub(crate) fn global() -> &'static Self {
        static GLOBAL: OnceLock<NetworkBudget> = OnceLock::new();
        GLOBAL.get_or_init(|| Self::new(Limits::production()))
    }

    fn provider_gate(&self, provider: &str) -> Arc<ProviderGate> {
        let mut providers = self
            .providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        providers
            .entry(provider.to_string())
            .or_insert_with(|| {
                Arc::new(ProviderGate {
                    concurrency: Arc::new(Semaphore::new(self.limits.per_provider)),
                    rate: RateGate::new(self.limits.provider_starts_per_second),
                })
            })
            .clone()
    }

    /// Acquire the request-wide layers. The destination host is acquired
    /// separately so each redirect hop pays only for the host it contacts.
    pub(crate) async fn acquire_logical(
        &self,
        class: RequestClass<'_>,
        session: Option<&str>,
        trace: Option<&NetworkTrace>,
    ) -> NetworkLease {
        let provider = class.provider().map(|id| self.provider_gate(id));
        if let Some(gate) = &provider {
            // Measure the real wait, not the planned reservation delay: a late
            // wake belongs to scheduling, not to the requested start interval.
            let started = Instant::now();
            gate.rate.reserve().wait().await;
            let waited = started.elapsed();
            record_layer_wait(LAYER_PROVIDER_RATE, waited);
            if let Some(trace) = trace {
                NetworkTrace::add(&trace.provider_rate_us, waited);
            }
        }

        let provider_permit = match &provider {
            Some(gate) => {
                let started = Instant::now();
                let permit = gate
                    .concurrency
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("network budget semaphore is never closed");
                let waited = started.elapsed();
                record_layer_wait(LAYER_PROVIDER, waited);
                if let Some(trace) = trace {
                    NetworkTrace::add(&trace.provider_us, waited);
                }
                Some(permit)
            }
            None => None,
        };

        // Session before process: a request queued on its session's cap must
        // not hold a process permit while it waits, or one saturated session
        // could occupy the whole process budget.
        let session_permit = match session {
            Some(key) => {
                let started = Instant::now();
                let permit = KeyedPermit::acquire(
                    self.sessions.clone(),
                    key.to_string(),
                    self.limits.per_session,
                )
                .await;
                let waited = started.elapsed();
                record_layer_wait(LAYER_SESSION, waited);
                if let Some(trace) = trace {
                    NetworkTrace::add(&trace.session_us, waited);
                }
                Some(permit)
            }
            None => None,
        };

        let started = Instant::now();
        let global = self
            .global
            .clone()
            .acquire_owned()
            .await
            .expect("network budget semaphore is never closed");
        let waited = started.elapsed();
        record_layer_wait(LAYER_GLOBAL, waited);
        if let Some(trace) = trace {
            NetworkTrace::add(&trace.global_us, waited);
        }

        NetworkLease {
            _global: global,
            _session: session_permit,
            _provider: provider_permit,
            _host: None,
        }
    }

    /// Acquire every layer for one request. Kept for the budget's own tests;
    /// the HTTP path uses [`Self::acquire_logical`] plus a per-hop host
    /// permit.
    #[cfg(test)]
    pub(crate) async fn acquire(
        &self,
        class: RequestClass<'_>,
        session: Option<&str>,
        host: &str,
        port: u16,
        trace: Option<&NetworkTrace>,
    ) -> NetworkLease {
        let lease = self.acquire_logical(class, session, trace).await;
        let host_permit = self.acquire_host(host, port, trace).await;
        lease.attach_host(host_permit)
    }

    pub(crate) async fn acquire_host(
        &self,
        host: &str,
        port: u16,
        trace: Option<&NetworkTrace>,
    ) -> KeyedPermit {
        let started = Instant::now();
        let permit = KeyedPermit::acquire(
            self.hosts.clone(),
            host_key(host, port),
            self.limits.per_host,
        )
        .await;
        let waited = started.elapsed();
        record_layer_wait(LAYER_HOST, waited);
        if let Some(trace) = trace {
            NetworkTrace::add(&trace.host_us, waited);
        }
        permit
    }
}

/// Normalized destination identity for the host budget.
///
/// URL parsing already lowercases and punycodes the host. Strip IPv6
/// brackets, a trailing root dot and bracketed IPv6 is re-added so `::1:443`
/// cannot be confused with a host name. This is a concurrency key only; it is
/// never used to decide whether a destination is allowed.
pub(crate) fn host_key(host: &str, port: u16) -> String {
    let bare = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let normalized = bare.trim_end_matches('.').to_ascii_lowercase();
    if normalized.contains(':') {
        format!("[{normalized}]:{port}")
    } else {
        format!("{normalized}:{port}")
    }
}

/// The budget layers held for one request. Dropping it releases them.
#[derive(Debug)]
pub(crate) struct NetworkLease {
    _global: OwnedSemaphorePermit,
    _session: Option<KeyedPermit>,
    _provider: Option<OwnedSemaphorePermit>,
    _host: Option<KeyedPermit>,
}

impl NetworkLease {
    /// Attach the permit for the destination the request actually contacted.
    pub(crate) fn attach_host(mut self, host: KeyedPermit) -> Self {
        self._host = Some(host);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn limits(global: usize, per_session: usize, per_host: usize) -> Limits {
        Limits {
            global,
            per_session,
            per_provider: 2,
            provider_starts_per_second: 100,
            per_host,
        }
    }

    async fn hold(
        budget: Arc<NetworkBudget>,
        session: &'static str,
        live: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        hold_for: Duration,
    ) {
        let _lease = budget
            .acquire(
                RequestClass::Fetch,
                Some(session),
                "example.test",
                443,
                None,
            )
            .await;
        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(hold_for).await;
        live.fetch_sub(1, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn the_process_budget_caps_in_flight_requests_across_sessions() {
        let budget = Arc::new(NetworkBudget::new(limits(2, 4, 4)));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for index in 0..6 {
            let session = if index % 2 == 0 { "a" } else { "b" };
            tasks.push(tokio::spawn(hold(
                budget.clone(),
                session,
                live.clone(),
                peak.clone(),
                Duration::from_millis(50),
            )));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn one_session_cannot_take_the_whole_process_budget() {
        // Global width 8, but one session is capped at 2. A second session
        // must progress while the first is saturated.
        let budget = Arc::new(NetworkBudget::new(limits(8, 2, 8)));
        let live_a = Arc::new(AtomicUsize::new(0));
        let peak_a = Arc::new(AtomicUsize::new(0));
        let mut a = Vec::new();
        for _ in 0..5 {
            a.push(tokio::spawn(hold(
                budget.clone(),
                "session-a",
                live_a.clone(),
                peak_a.clone(),
                Duration::from_millis(60),
            )));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        let live_b = Arc::new(AtomicUsize::new(0));
        let peak_b = Arc::new(AtomicUsize::new(0));
        hold(
            budget.clone(),
            "session-b",
            live_b.clone(),
            peak_b.clone(),
            Duration::from_millis(20),
        )
        .await;
        assert_eq!(peak_a.load(Ordering::SeqCst), 2);
        // B got a permit while A had five queued or running requests.
        assert_eq!(peak_b.load(Ordering::SeqCst), 1);
        for task in a {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn the_host_budget_is_keyed_by_normalized_destination() {
        let budget = Arc::new(NetworkBudget::new(limits(8, 8, 1)));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for host in ["example.test", "EXAMPLE.TEST.", "[::1]"] {
            tasks.push(tokio::spawn(hold_host(
                budget.clone(),
                host,
                live.clone(),
                peak.clone(),
                Duration::from_millis(60),
            )));
        }
        for task in tasks {
            task.await.unwrap();
        }
        // `example.test` and `EXAMPLE.TEST.` are the same destination and
        // share one slot; `[::1]` is a different one and proceeds without
        // waiting for them.
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(
            host_key("EXAMPLE.TEST.", 443),
            host_key("example.test", 443)
        );
        assert_eq!(host_key("[::1]", 443), host_key("::1", 443));
        assert_ne!(host_key("example.test", 443), host_key("example.test", 80));
        // The URL parser is the one place IDN is normalized; the key consumes
        // its ASCII (punycode) form, never the raw Unicode spelling.
        let idn = reqwest::Url::parse("http://bücher.example/").unwrap();
        assert_eq!(
            host_key(idn.host_str().unwrap(), 443),
            "xn--bcher-kva.example:443"
        );
    }

    async fn hold_host(
        budget: Arc<NetworkBudget>,
        host: &'static str,
        live: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        hold_for: Duration,
    ) {
        let _lease = budget
            .acquire(RequestClass::Fetch, None, host, 443, None)
            .await;
        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(hold_for).await;
        live.fetch_sub(1, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn a_dropped_waiter_releases_its_place_for_the_next_request() {
        let budget = Arc::new(NetworkBudget::new(limits(1, 1, 1)));
        let first = budget
            .acquire(RequestClass::Fetch, Some("s"), "example.test", 443, None)
            .await;
        let waiter = tokio::spawn({
            let budget = budget.clone();
            async move {
                let _permit = budget
                    .acquire(RequestClass::Fetch, Some("s"), "example.test", 443, None)
                    .await;
                true
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        waiter.abort();
        drop(first);
        // The aborted waiter held no permit once its future was dropped; the
        // next request must acquire immediately instead of waiting for the
        // budget to "expire".
        let _next = tokio::time::timeout(
            Duration::from_millis(200),
            budget.acquire(RequestClass::Fetch, Some("s"), "example.test", 443, None),
        )
        .await
        .expect("an aborted queue entry must not leak its permit");
    }

    #[tokio::test]
    async fn the_provider_rate_gate_delays_starts_beyond_the_window() {
        let budget = Arc::new(NetworkBudget {
            limits: Limits {
                global: 8,
                per_session: 8,
                per_provider: 8,
                provider_starts_per_second: 2,
                per_host: 8,
            },
            global: Arc::new(Semaphore::new(8)),
            sessions: keyed_map(),
            hosts: keyed_map(),
            providers: Mutex::new(HashMap::new()),
        });
        let first = budget
            .acquire(
                RequestClass::Search {
                    provider: "fixture",
                },
                Some("s"),
                "provider.test",
                443,
                None,
            )
            .await;
        let second = budget
            .acquire(
                RequestClass::Search {
                    provider: "fixture",
                },
                Some("s"),
                "provider.test",
                443,
                None,
            )
            .await;
        drop(first);
        drop(second);
        let started = Instant::now();
        let _third = budget
            .acquire(
                RequestClass::Search {
                    provider: "fixture",
                },
                Some("s"),
                "provider.test",
                443,
                None,
            )
            .await;
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "the third start in one window must wait for the oldest to age out: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn two_providers_do_not_share_one_budget() {
        let budget = Arc::new(NetworkBudget::new(limits(8, 8, 8)));
        let a1 = budget
            .acquire(
                RequestClass::Search { provider: "a" },
                None,
                "a.test",
                443,
                None,
            )
            .await;
        let a2 = budget
            .acquire(
                RequestClass::Search { provider: "a" },
                None,
                "a.test",
                443,
                None,
            )
            .await;
        let b = budget
            .acquire(
                RequestClass::Search { provider: "b" },
                None,
                "b.test",
                443,
                None,
            )
            .await;
        // A's two in-flight permits are both used, while B has its own gate.
        assert_eq!(budget.provider_gate("a").concurrency.available_permits(), 0);
        assert_eq!(budget.provider_gate("b").concurrency.available_permits(), 1);
        drop(a1);
        drop(a2);
        drop(b);
        assert_eq!(budget.provider_gate("a").concurrency.available_permits(), 2);
        assert_eq!(budget.provider_gate("b").concurrency.available_permits(), 2);
    }

    /// The production limit must be saturated through the real acquisition
    /// path: eight requests reach the wire, the ninth waits, and no instant
    /// exceeds eight. Distinct sessions and hosts keep the inner layers out of
    /// the way so the process layer is the one under test.
    #[tokio::test]
    async fn the_production_process_budget_saturates_at_eight_and_queues_the_ninth() {
        let budget = Arc::new(NetworkBudget::new(Limits::production()));
        let release = Arc::new(Semaphore::new(0));
        let acquired = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for index in 0..9 {
            let budget = budget.clone();
            let release = release.clone();
            let acquired = acquired.clone();
            let live = live.clone();
            let peak = peak.clone();
            tasks.push(tokio::spawn(async move {
                let session = format!("session-{index}");
                let host = format!("host-{index}.test");
                let lease = budget
                    .acquire(RequestClass::Fetch, Some(&session), &host, 443, None)
                    .await;
                acquired.fetch_add(1, Ordering::SeqCst);
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let _permit = release.acquire().await.expect("release gate");
                live.fetch_sub(1, Ordering::SeqCst);
                drop(lease);
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while acquired.load(Ordering::SeqCst) < 8 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            acquired.load(Ordering::SeqCst),
            8,
            "exactly the process budget's eight requests acquire a permit"
        );
        // Give a ninth room to (incorrectly) acquire if the cap were wider.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            acquired.load(Ordering::SeqCst),
            8,
            "the ninth request must wait for a released permit"
        );
        assert_eq!(peak.load(Ordering::SeqCst), 8);

        release.add_permits(9);
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(
            peak.load(Ordering::SeqCst),
            8,
            "no instant may exceed the process budget"
        );
        // Finish and release: the next request acquires immediately instead of
        // waiting for a permit that leaked.
        let next = tokio::time::timeout(
            Duration::from_millis(200),
            budget.acquire(RequestClass::Fetch, Some("after"), "after.test", 443, None),
        )
        .await
        .expect("a finished batch releases every process permit");
        drop(next);
    }

    /// Cancellation at the saturated production width must free the queued
    /// place without leaking a permit or starving the next request.
    #[tokio::test]
    async fn a_cancelled_waiter_does_not_leak_a_saturated_production_budget() {
        let budget = Arc::new(NetworkBudget::new(Limits::production()));
        let release = Arc::new(Semaphore::new(0));
        let mut holders = Vec::new();
        for index in 0..8 {
            let budget = budget.clone();
            let release = release.clone();
            holders.push(tokio::spawn(async move {
                let session = format!("holder-{index}");
                let host = format!("holder-{index}.test");
                let _lease = budget
                    .acquire(RequestClass::Fetch, Some(&session), &host, 443, None)
                    .await;
                let _permit = release.acquire().await.expect("release gate");
            }));
        }
        // Let every holder acquire before the ninth starts queuing.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let waiter = tokio::spawn({
            let budget = budget.clone();
            async move {
                let _lease = budget
                    .acquire(
                        RequestClass::Fetch,
                        Some("cancelled"),
                        "cancelled.test",
                        443,
                        None,
                    )
                    .await;
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        waiter.abort();
        assert!(
            waiter.await.is_err(),
            "the waiter was cancelled while queued"
        );

        release.add_permits(8);
        for holder in holders {
            holder.await.unwrap();
        }
        let started = Instant::now();
        let next = tokio::time::timeout(
            Duration::from_millis(200),
            budget.acquire(RequestClass::Fetch, Some("next"), "next.test", 443, None),
        )
        .await
        .expect("a cancelled queue entry must not leak a production permit");
        assert!(started.elapsed() < Duration::from_millis(200));
        drop(next);
    }

    /// The production rate constant is twenty starts per provider per rolling
    /// second, shared by every session. The twenty-first start must wait for
    /// the oldest reservation to age out.
    #[tokio::test]
    async fn the_production_provider_rate_queues_the_twenty_first_start_across_sessions() {
        let budget = Arc::new(NetworkBudget::new(Limits::production()));
        let first_start = Instant::now();
        for index in 0..20 {
            let session = format!("session-{index}");
            let lease = budget
                .acquire_logical(
                    RequestClass::Search {
                        provider: "provider",
                    },
                    Some(&session),
                    None,
                )
                .await;
            drop(lease);
        }
        let twenty_first = budget
            .acquire_logical(
                RequestClass::Search {
                    provider: "provider",
                },
                Some("another-session"),
                None,
            )
            .await;
        drop(twenty_first);
        assert!(
            first_start.elapsed() >= Duration::from_millis(900),
            "the twenty-first start must wait for the rolling window: {:?}",
            first_start.elapsed()
        );
    }

    /// The rolling window spaces reservations one window apart, and a waiter
    /// cancelled before its start removes its own reservation instead of
    /// leaving a hole that pushes every later caller back.
    #[tokio::test]
    async fn the_rate_gate_spaces_windows_and_a_cancelled_reservation_is_reusable() {
        let gate = RateGate::new(1);
        let first = gate.reserve();
        let first_start = first.start;
        first.wait().await; // committed at its start instant
        let second = gate.reserve();
        assert!(
            second.start.duration_since(first_start) >= PROVIDER_RATE_WINDOW,
            "a second reservation must start one window after the first"
        );
        let third = gate.reserve();
        assert!(
            third.start.duration_since(second.start) >= PROVIDER_RATE_WINDOW,
            "each further reservation keeps its own window slot"
        );
        drop(second);
        drop(third);

        // Two reservations in the next window: cancelling one must let a later
        // caller take that slot instead of being pushed a full window back.
        let gate = RateGate::new(2);
        let a = gate.reserve();
        let b = gate.reserve();
        let c = gate.reserve();
        let d = gate.reserve();
        let next_window = c.start;
        drop(c);
        let after_cancel = gate.reserve();
        assert!(
            after_cancel.start < next_window + PROVIDER_RATE_WINDOW,
            "the cancelled reservation's slot must be reusable: {:?} is a full \
             window past the next one",
            after_cancel.start
        );
        drop(a);
        drop(b);
        drop(d);
        drop(after_cancel);
    }

    /// A session with more requests than its cap must not occupy the process
    /// budget while the excess waits: the queued requests hold their session
    /// slot only after passing the session gate, so another session always
    /// finds a process permit.
    #[tokio::test]
    async fn one_session_cannot_hold_the_process_budget_while_queued() {
        let budget = Arc::new(NetworkBudget::new(Limits::production()));
        let release = Arc::new(Semaphore::new(0));
        let running = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for index in 0..9 {
            let budget = budget.clone();
            let release = release.clone();
            let running = running.clone();
            tasks.push(tokio::spawn(async move {
                let host = format!("blocked-{index}.test");
                let _lease = budget
                    .acquire(RequestClass::Fetch, Some("one-session"), &host, 443, None)
                    .await;
                running.fetch_add(1, Ordering::SeqCst);
                let _permit = release.acquire().await.expect("release gate");
                running.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while running.load(Ordering::SeqCst) < 4 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            running.load(Ordering::SeqCst),
            4,
            "the session cap is four; the other five requests must be queued"
        );

        // Four process permits are in use, so four remain. A second session
        // must acquire one immediately instead of waiting for the first
        // session's in-flight request to finish.
        let started = Instant::now();
        let other = tokio::time::timeout(
            Duration::from_millis(500),
            budget.acquire(
                RequestClass::Fetch,
                Some("other-session"),
                "other.test",
                443,
                None,
            ),
        )
        .await
        .expect("a queued session must not consume the whole process budget");
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(other);

        release.add_permits(9);
        for task in tasks {
            task.await.unwrap();
        }
    }

    /// A cancellation can remove future rate reservations and let the next
    /// caller take the immediate path. That immediate start must stay ordered
    /// before the remaining future reservation, or the capacity anchor reads a
    /// future slot as the oldest one and pushes later starts a full window
    /// late.
    #[test]
    fn a_cancelled_reservation_keeps_the_rate_queue_ordered() {
        let gate = RateGate::new(2);
        let first = gate.reserve();
        let second = gate.reserve();
        let future = gate.reserve();
        let future_start = future.start;
        drop(first);
        drop(second);
        let immediate = gate.reserve();
        let after = gate.reserve();
        assert!(
            after.start < future_start + PROVIDER_RATE_WINDOW,
            "an immediate reservation must be ordered before the remaining \
             future one: {:?} vs {:?}",
            after.start,
            future_start
        );
        drop(future);
        drop(immediate);
        drop(after);
    }

    #[tokio::test]
    async fn wait_time_is_recorded_for_the_global_layer() {
        // The session gate is wider than the process gate, so both requests
        // pass their session first and the waiter blocks on the process layer.
        let budget = Arc::new(NetworkBudget::new(limits(1, 2, 2)));
        let held = budget
            .acquire(RequestClass::Fetch, Some("s"), "example.test", 443, None)
            .await;
        let trace = Arc::new(NetworkTrace::default());
        let waiter_trace = trace.clone();
        let waiter = tokio::spawn({
            let budget = budget.clone();
            async move {
                let _permit = budget
                    .acquire(
                        RequestClass::Fetch,
                        Some("s"),
                        "example.test",
                        443,
                        Some(&waiter_trace),
                    )
                    .await;
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(held);
        waiter.await.unwrap();
        assert!(trace.global_queue_us() >= 20_000, "{trace:?}");
    }

    #[tokio::test]
    async fn wait_time_is_recorded_for_the_session_and_host_layers() {
        // Session layer: one session slot is held; the global budget is wide
        // enough that the waiter reaches the session gate.
        let budget = Arc::new(NetworkBudget::new(limits(8, 1, 8)));
        let held = budget
            .acquire(RequestClass::Fetch, Some("s"), "one.test", 443, None)
            .await;
        let trace = Arc::new(NetworkTrace::default());
        let waiter_trace = trace.clone();
        let waiter = tokio::spawn({
            let budget = budget.clone();
            async move {
                let _permit = budget
                    .acquire(
                        RequestClass::Fetch,
                        Some("s"),
                        "two.test",
                        443,
                        Some(&waiter_trace),
                    )
                    .await;
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(held);
        waiter.await.unwrap();
        assert!(trace.session_queue_us() >= 20_000, "{trace:?}");

        // Host layer: different sessions, the same destination.
        let budget = Arc::new(NetworkBudget::new(limits(8, 8, 1)));
        let held = budget
            .acquire(RequestClass::Fetch, Some("a"), "same.test", 443, None)
            .await;
        let trace = Arc::new(NetworkTrace::default());
        let waiter_trace = trace.clone();
        let waiter = tokio::spawn({
            let budget = budget.clone();
            async move {
                let _permit = budget
                    .acquire(
                        RequestClass::Fetch,
                        Some("b"),
                        "same.test",
                        443,
                        Some(&waiter_trace),
                    )
                    .await;
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(held);
        waiter.await.unwrap();
        assert!(trace.host_queue_us() >= 20_000, "{trace:?}");
    }
}
