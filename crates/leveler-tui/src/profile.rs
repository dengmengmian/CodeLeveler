//! Opt-in streaming/render profiler.
//!
//! Disabled unless `LEVELER_TUI_PROFILE=1`. When disabled every entry point is
//! one cached boolean load, so the normal path is unchanged. When enabled it
//! records **monotonic** durations (`std::time::Instant`, never wall clock)
//! under stable metric names and emits a percentile summary on exit.
//!
//! The profiler observes only. It never mutates `AppState`, never changes what
//! is drawn, and never alters event ordering — so enabling it cannot change the
//! behaviour being measured.
//!
//! Output goes to `LEVELER_TUI_PROFILE_OUT` when set (a file, appended after
//! the terminal is restored), otherwise to stderr.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const ENV_ENABLE: &str = "LEVELER_TUI_PROFILE";
const ENV_OUT: &str = "LEVELER_TUI_PROFILE_OUT";

/// Whether profiling is enabled for this process (read once).
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        leveler_core::environment()
            .var(ENV_ENABLE)
            .is_some_and(|v| !v.is_empty() && v != "0")
    })
}

/// A monotonic start stamp, or `None` when profiling is off.
///
/// Callers thread this through unchanged; a disabled process pays one boolean
/// check and no `Instant::now`.
#[inline]
pub fn start() -> Option<Instant> {
    enabled().then(Instant::now)
}

/// Record the elapsed monotonic time of `started` under `metric` (milliseconds).
#[inline]
pub fn stop(started: Option<Instant>, metric: &'static str) {
    if let Some(t0) = started {
        record_us(metric, t0.elapsed().as_micros() as u64);
    }
}

/// Record `elapsed` directly (milliseconds as micros), for call sites that
/// already own an `Instant`.
#[inline]
pub fn record_elapsed(started: Instant, metric: &'static str) {
    if enabled() {
        record_us(metric, started.elapsed().as_micros() as u64);
    }
}

/// Add `n` to a counter.
#[inline]
pub fn add(metric: &'static str, n: u64) {
    if !enabled() {
        return;
    }
    with_data(|d| *d.counters.entry(metric).or_default() += n);
}

/// Record one histogram sample in microseconds.
#[inline]
pub fn record_us(metric: &'static str, us: u64) {
    with_data(|d| d.hist.entry(metric).or_default().push(us));
}

/// One markdown parse: duration (from `started`) plus input size.
#[inline]
pub fn markdown_parse(started: Option<Instant>, bytes: usize) {
    if started.is_none() {
        return;
    }
    stop(started, "tui.markdown_parse_ms");
    add("tui.markdown_parse_count", 1);
    add("tui.markdown_parse_bytes", bytes as u64);
}

fn with_data<R>(f: impl FnOnce(&mut Data) -> R) -> R {
    static DATA: OnceLock<Mutex<Data>> = OnceLock::new();
    let m = DATA.get_or_init(|| Mutex::new(Data::default()));
    let mut guard = match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

#[derive(Default)]
struct Data {
    counters: BTreeMap<&'static str, u64>,
    /// Duration samples in microseconds, per metric.
    hist: BTreeMap<&'static str, Vec<u64>>,
}

/// Emit the collected report once. Safe to call from any exit path; the report
/// is written to `LEVELER_TUI_PROFILE_OUT` or stderr and the data is retained.
pub fn emit_report() {
    if !enabled() {
        return;
    }
    let report = report();
    match leveler_core::environment().var(ENV_OUT) {
        Some(path) if !path.is_empty() => {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = f.write_all(report.as_bytes());
                let _ = f.write_all(b"\n");
            }
        }
        _ => eprintln!("{report}"),
    }
}

/// Build the textual report: counters first, then per-metric percentiles.
pub fn report() -> String {
    let (counters, hist) = with_data(|d| (d.counters.clone(), std::mem::take(&mut d.hist)));
    let mut out = String::new();
    out.push_str("=== LEVELER TUI PROFILE ===\n");
    out.push_str("-- counters --\n");
    for (k, v) in &counters {
        out.push_str(&format!("{k} = {v}\n"));
    }
    out.push_str("-- latency (ms) --\n");
    out.push_str(&format!(
        "{:<34} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}\n",
        "metric", "n", "p50", "p90", "p95", "p99", "max"
    ));
    for (k, mut samples) in hist {
        if samples.is_empty() {
            continue;
        }
        samples.sort_unstable();
        let p = |q: f64| -> f64 {
            let idx = ((samples.len() as f64 - 1.0) * q).round() as usize;
            samples[idx.min(samples.len() - 1)] as f64 / 1000.0
        };
        out.push_str(&format!(
            "{:<34} {:>8} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2}\n",
            k,
            samples.len(),
            p(0.50),
            p(0.90),
            p(0.95),
            p(0.99),
            *samples.last().unwrap() as f64 / 1000.0,
        ));
    }
    out
}
