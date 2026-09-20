//! Lifecycle regression tests for the test harness itself.
//!
//! These do not test the product. They pin the harness properties the rest of
//! the suite depends on: a test that never resolves must fail within its
//! deadline; an owned child process tree must be reclaimed on success, failure,
//! panic and timeout; two concurrent scopes must not reclaim each other; and a
//! test binary whose parent died must still end by itself.
//!
//! Every potentially-hanging case has an outer emergency watchdog in the test
//! itself (`wait_until_gone` with a hard bound) so a bug in the harness cannot
//! hang this suite either.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use leveler_test_support::{
    TestProcessScope, bounded_test, live_children, sleep_command, sleep_shell_line,
};

/// A short deadline for these self-tests; the real default is far larger.
const SELF_TEST_TIMEOUT: Duration = Duration::from_millis(300);

fn spawn_sleeper(scope: &mut TestProcessScope, seconds: u32) -> u32 {
    let (program, args) = sleep_command(seconds);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    scope.spawn_program(program, &args).expect("spawn sleeper")
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        kill(Pid::from_raw(pid as i32), None).is_ok()
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        out.map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

/// Wait until `pid` is gone or `limit` elapses. Returns `true` if it is gone.
/// The outer bound is the emergency watchdog for these tests.
fn wait_until_gone(pid: u32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !process_alive(pid)
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_else(|| "<non-string panic>".to_string())
}

fn run_catching<F: FnOnce()>(body: F) -> Result<(), String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body))
        .map_err(|payload| panic_text(payload.as_ref()))
}

// CASE A — a never-resolving async future fails within the deadline.
#[test]
fn case_a_never_resolving_future_is_bounded() {
    let started = Instant::now();
    let error = run_catching(|| {
        bounded_test("case_a_pending", SELF_TEST_TIMEOUT, || async {
            std::future::pending::<()>().await;
        });
    })
    .expect_err("a pending future must time out");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "the deadline must fire quickly, took {elapsed:?}"
    );
    assert!(
        error.contains("TEST TIMEOUT"),
        "diagnostic missing: {error}"
    );
    assert!(
        error.contains("name=case_a_pending"),
        "diagnostic missing name: {error}"
    );
    assert!(
        error.contains("deadline="),
        "diagnostic missing deadline: {error}"
    );
}

// CASE B + C — a timeout reclaims the owned child and its descendants.
#[test]
fn case_b_c_timeout_reclaims_child_and_descendants() {
    let direct_pid = Arc::new(AtomicU32::new(0));
    let observed_direct = direct_pid.clone();
    let workdir = tempfile::tempdir().unwrap();
    let pid_file = workdir.path().join("descendant.pid");

    let error = run_catching(|| {
        let pid_file = pid_file.clone();
        let observed_direct = observed_direct.clone();
        bounded_test("case_bc_children", SELF_TEST_TIMEOUT, move || async move {
            let mut scope = TestProcessScope::new("case_bc");
            let direct = spawn_sleeper(&mut scope, 300);
            observed_direct.store(direct, Ordering::SeqCst);

            // A shell that starts a descendant in the same process group and
            // then exits, so only the descendant keeps the group alive.
            let line = format!(
                "{} & echo $! > '{}'",
                sleep_shell_line(300),
                pid_file.display()
            );
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", &line]);
            scope.spawn(&mut command).expect("spawn descendant shell");

            std::future::pending::<()>().await;
        });
    })
    .expect_err("the body must time out");

    assert!(
        error.contains("TEST TIMEOUT"),
        "diagnostic missing: {error}"
    );
    let direct = direct_pid.load(Ordering::SeqCst);
    assert_ne!(direct, 0, "the direct child must have been spawned");
    assert!(
        wait_until_gone(direct, Duration::from_secs(5)),
        "timeout must reclaim the direct child {direct}"
    );
    assert!(
        live_children().iter().all(|r| r.pid != direct),
        "the reclaimed child must leave the live registry"
    );

    // The shell wrote the descendant pid; it must be gone too.
    let descendant = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .expect("the descendant recorded its pid");
    assert!(
        wait_until_gone(descendant, Duration::from_secs(5)),
        "timeout must reclaim descendant {descendant}, not just the direct child"
    );
}

// CASE D — a panicking test still reclaims its child.
#[test]
fn case_d_panic_reclaims_the_owned_child() {
    let pid = Arc::new(AtomicU32::new(0));
    let observed = pid.clone();
    let error = run_catching(|| {
        let mut scope = TestProcessScope::new("case_d");
        let direct = spawn_sleeper(&mut scope, 300);
        observed.store(direct, Ordering::SeqCst);
        panic!("deliberate panic in the test body");
    })
    .expect_err("the body must panic");
    assert!(error.contains("deliberate panic"), "{error}");
    let direct = pid.load(Ordering::SeqCst);
    assert!(
        wait_until_gone(direct, Duration::from_secs(5)),
        "a panic must still reclaim child {direct}"
    );
}

// CASE F — an assertion failure still reclaims its child.
#[test]
fn case_f_assertion_failure_reclaims_the_owned_child() {
    let pid = Arc::new(AtomicU32::new(0));
    let observed = pid.clone();
    let error = run_catching(|| {
        let mut scope = TestProcessScope::new("case_f");
        let direct = spawn_sleeper(&mut scope, 300);
        observed.store(direct, Ordering::SeqCst);
        assert_eq!(1, 2, "deliberate assertion failure");
    })
    .expect_err("the assertion must fail");
    assert!(error.contains("deliberate assertion failure"), "{error}");
    let direct = pid.load(Ordering::SeqCst);
    assert!(
        wait_until_gone(direct, Duration::from_secs(5)),
        "an assertion failure must still reclaim child {direct}"
    );
}

// The production-shaped wedge: a chat/direct turn runs on a `spawn_blocking`
// thread that calls `Handle::block_on`. On a runtime the harness does not own,
// the default teardown waits for that task forever. This pins that
// `bounded_test` both fires the deadline and returns from teardown.
#[test]
fn a_wedged_blocking_task_cannot_hang_the_harness() {
    let started = Instant::now();
    let error = run_catching(|| {
        bounded_test("wedged_blocking", SELF_TEST_TIMEOUT, || async {
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(std::future::pending::<()>());
            });
            std::future::pending::<()>().await;
        });
    })
    .expect_err("a wedged blocking task must time out");
    assert!(
        error.contains("TEST TIMEOUT"),
        "diagnostic missing: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "bounded teardown must return even with a wedged blocking task; took {:?}",
        started.elapsed()
    );
}

// CASE E — a normal test is not delayed by the harness.
#[test]
fn case_e_success_is_not_delayed() {
    let started = Instant::now();
    bounded_test("case_e_ok", Duration::from_secs(30), || async {
        tokio::time::sleep(Duration::from_millis(20)).await;
    });
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "a passing test must not wait for the deadline, took {elapsed:?}"
    );
}

// CASE G — concurrent scopes are isolated.
#[test]
fn case_g_concurrent_scopes_do_not_reclaim_each_other() {
    let mut a = TestProcessScope::new("case_g_a");
    let mut b = TestProcessScope::new("case_g_b");
    let pa = spawn_sleeper(&mut a, 300);
    let pb = spawn_sleeper(&mut b, 300);
    a.cleanup();
    assert!(
        wait_until_gone(pa, Duration::from_secs(3)),
        "scope A's child must be gone"
    );
    assert!(
        process_alive(pb),
        "scope A's cleanup must not touch scope B's child {pb}"
    );
    b.cleanup();
    assert!(wait_until_gone(pb, Duration::from_secs(3)));
}

// CASE H is the real `command_delivery::first_message_retitles_a_placeholder_session`,
// converted to `bounded_test` in `leveler-app/tests/command_delivery.rs`; see
// the real dogfood in the task report.

// §19 — a test process whose parent died must still end by its own deadline.
//
// The worker re-invokes this same test binary with `--exact`; the intermediate
// shell exits immediately, so the worker is reparented to PID 1 exactly like the
// two-day-old orphans the harness exists to prevent.
#[test]
fn a_detached_worker_ends_by_its_own_deadline() {
    if std::env::var_os("LEVELER_HARNESS_DETACHED_WORKER").is_some() {
        // Worker path: never resolves; only its own deadline can end it.
        bounded_test(
            "detached_worker",
            Duration::from_secs(2),
            detached_worker_body,
        );
        return;
    }
    if std::env::var_os("LEVELER_HARNESS_RUN_DETACH_TEST").is_none() {
        // The normal suite must not perform the detach below (it would spawn a
        // second copy of the binary on every run). Enable it explicitly.
        return;
    }
    #[cfg(unix)]
    {
        use std::process::{Command, Stdio};

        let exe = std::env::current_exe().expect("current test binary");
        let workdir = tempfile::tempdir().unwrap();
        let pid_file = workdir.path().join("worker.pid");
        let line = format!(
            "LEVELER_HARNESS_DETACHED_WORKER=1 \"{}\" --exact a_detached_worker_ends_by_its_own_deadline --nocapture >/dev/null 2>&1 & echo $! > '{}'",
            exe.display(),
            pid_file.display()
        );
        Command::new("/bin/sh")
            .args(["-c", &line])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn detached worker");

        let worker = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            .expect("worker pid");
        // Its parent (the shell) is already gone. The worker must still end,
        // bounded by its own 2s deadline plus the shutdown grace.
        assert!(
            wait_until_gone(worker, Duration::from_secs(15)),
            "a detached test binary must end by its own deadline, {worker} did not"
        );
    }
    #[cfg(windows)]
    {
        // The detach-without-parent trick is a Unix mechanism here; the worker
        // path itself (a bounded deadline) is still exercised on Windows.
        eprintln!("detached-worker check is Unix-only; worker deadline path covered elsewhere");
    }
}

/// The worker's body, kept out of the recursive closure for clarity.
async fn detached_worker_body() {
    std::future::pending::<()>().await;
}

// A leaked registry entry from a reclaimed child would make diagnostics lie.
#[test]
fn reclaimed_children_leave_the_registry() {
    let mut scope = TestProcessScope::new("registry");
    let pid = spawn_sleeper(&mut scope, 300);
    assert!(live_children().iter().any(|r| r.pid == pid));
    scope.cleanup();
    assert!(live_children().iter().all(|r| r.pid != pid));
}
