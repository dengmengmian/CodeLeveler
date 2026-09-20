//! A bounded lifetime for an integration test.
//!
//! `cargo test` has no per-test timeout. An `#[tokio::test]` that awaits a
//! socket, a daemon response, a channel or an event that never arrives parks
//! the test thread forever; the test binary is then reparented to PID 1 when
//! cargo dies and lives on for days. [`bounded_test`] is the shared entry point
//! that makes "the test can fail, but it cannot hang forever" a property of the
//! harness rather than of each individual test.
//!
//! Why it owns the runtime instead of wrapping `tokio::time::timeout` around an
//! `async fn` body: production chat/direct turns run on a `spawn_blocking`
//! thread that calls `Handle::block_on`. If such a task wedges, dropping the
//! runtime waits for it forever (`BlockingPool::drop` waits without a bound),
//! and a top-level timeout around the body is not enough — the process still
//! hangs in teardown. Owning the runtime lets the harness shut it down with a
//! bounded grace after the deadline fires.
//!
//! ```no_run
//! #[test]
//! fn an_integration_test() {
//!     leveler_test_support::bounded_test("an_integration_test", leveler_test_support::DEFAULT_TEST_TIMEOUT, || async {
//!         // setup / act / assert, all under one deadline
//!     });
//! }
//! ```

use std::future::Future;
use std::time::{Duration, Instant};

/// A fast, self-contained test: no daemon, no network, no child process.
pub const FAST_TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The default for an integration test that talks to a runtime, a socket or a
/// child process. Generous enough for a warm CI machine, far below "hangs for
/// days".
pub const DEFAULT_TEST_TIMEOUT: Duration = Duration::from_secs(120);

/// A deliberately long campaign (soak, corpus replay) that a case-level
/// deadline would otherwise cut short. Opt in explicitly.
pub const LONG_TEST_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the harness waits for the test runtime to shut down after the body
/// ends. Bounds teardown even when a blocking task is wedged.
pub const TEST_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Environment override for the default deadline, in seconds.
///
/// A slow CI machine can raise it without editing tests. `0` and non-numeric
/// values are rejected loudly: this variable exists to move the bound, never to
/// remove it.
pub const TEST_TIMEOUT_ENV: &str = "LEVELER_TEST_TIMEOUT_SECS";

/// The default integration-test deadline, honouring [`TEST_TIMEOUT_ENV`].
pub fn test_timeout() -> Duration {
    test_timeout_or(DEFAULT_TEST_TIMEOUT)
}

/// [`test_timeout`] with an explicit fallback instead of [`DEFAULT_TEST_TIMEOUT`].
pub fn test_timeout_or(fallback: Duration) -> Duration {
    match std::env::var(TEST_TIMEOUT_ENV) {
        Ok(raw) => parse_timeout_env(&raw),
        Err(_) => fallback,
    }
}

/// Parse the [`TEST_TIMEOUT_ENV`] value. Separated from the env read so the
/// policy — positive, whole seconds, never "infinite" — is testable without
/// racing other tests over the process environment.
fn parse_timeout_env(raw: &str) -> Duration {
    let seconds: u64 = raw.trim().parse().unwrap_or_else(|_| {
        panic!("{TEST_TIMEOUT_ENV} must be a whole number of seconds, got {raw:?}")
    });
    assert!(
        seconds > 0,
        "{TEST_TIMEOUT_ENV}=0 would remove the test deadline; use a positive value"
    );
    Duration::from_secs(seconds)
}

/// Run `body` on a test-owned runtime under a single top-level deadline.
///
/// Covers setup, execution, assertions and the body's own coordination in one
/// bound. On expiry the body future is dropped — so RAII guards and
/// [`crate::TestProcessScope`]s reclaim their children — and the test fails
/// with a diagnostic report instead of hanging.
pub fn bounded_test<F, Fut>(name: &str, timeout: Duration, body: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build the test runtime");

    let started = Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            let body = body();
            tokio::pin!(body);
            // Constructed inside the runtime: `sleep` needs the timer context.
            let deadline = tokio::time::sleep(timeout);
            tokio::pin!(deadline);
            tokio::select! {
                _ = &mut body => None,
                // Snapshot before the body future is dropped, while the scopes
                // it owns are still registered.
                _ = &mut deadline => Some(crate::process::live_children()),
            }
        })
    }));

    // Bounded teardown. `shutdown_timeout` returns even if a blocking task is
    // parked in a nested `block_on`; the default `Drop` would not.
    runtime.shutdown_timeout(TEST_SHUTDOWN_GRACE);

    match outcome {
        Ok(None) => {}
        Ok(Some(children)) => panic!(
            "{}",
            timeout_report(name, timeout, started.elapsed(), &children)
        ),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn timeout_report(
    name: &str,
    timeout: Duration,
    elapsed: Duration,
    children: &[crate::process::ChildRecord],
) -> String {
    let mut report = format!(
        "TEST TIMEOUT\n  name={name}\n  deadline={:.3}s\n  elapsed={:.3}s\n  \
         runtime=multi_thread(worker_threads=2)\n  owned_children={}",
        timeout.as_secs_f64(),
        elapsed.as_secs_f64(),
        children.len()
    );
    for child in children {
        report.push_str(&format!(
            "\n    scope={} pid={} program={}",
            child.scope, child.pid, child.program
        ));
    }
    report.push_str(
        "\n  hint=raise LEVELER_TEST_TIMEOUT_SECS only for a known long test, \
         and check what the body awaits",
    );
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_zero_is_rejected_rather_than_read_as_infinite() {
        let result = std::panic::catch_unwind(|| parse_timeout_env("0"));
        assert!(
            result.is_err(),
            "0 must be rejected, not treated as infinite"
        );
    }

    #[test]
    fn env_value_replaces_the_default() {
        assert_eq!(parse_timeout_env("7"), Duration::from_secs(7));
        assert_eq!(parse_timeout_env(" 45 "), Duration::from_secs(45));
    }

    #[test]
    fn env_non_numeric_is_rejected() {
        let result = std::panic::catch_unwind(|| parse_timeout_env("soon"));
        assert!(result.is_err(), "a non-numeric override must fail loudly");
    }
}
