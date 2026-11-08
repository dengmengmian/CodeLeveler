//! Test-only, precise process-death injection for the crash matrix.
//!
//! A normal run never enters this module's kill path: it is compiled into
//! debug builds only, requires BOTH a marker-file variable and an exact
//! failpoint name from the closed set used by the tests, and it takes no
//! input from a model, tool result, or repository file. Callers are a handful
//! of fixed points in the engine adjacent to a durable write.
//!
//! It writes the marker, syncs it, and then kills the process with a real
//! SIGKILL (Unix) so the test observes an actual death rather than a normal
//! return, an error, or a transaction rollback. Release builds are a no-op.

/// Stop the process if the test harness armed exactly this point.
///
/// `LEVELER_TEST_CRASH_POINT` names the point; `LEVELER_TEST_CRASH_MARKER` is
/// the file written before the kill so the parent test can prove the exact
/// cut was reached. Neither variable is read anywhere else in production.
pub fn point(name: &str) {
    #[cfg(debug_assertions)]
    crash(name);
    #[cfg(not(debug_assertions))]
    let _ = name;
}

#[cfg(debug_assertions)]
fn crash(name: &str) {
    let armed = std::env::var("LEVELER_TEST_CRASH_POINT").ok();
    if armed.as_deref() != Some(name) {
        return;
    }
    let Some(marker) = std::env::var_os("LEVELER_TEST_CRASH_MARKER") else {
        return;
    };
    if let Ok(mut file) = std::fs::File::create(&marker) {
        use std::io::Write;
        let _ = writeln!(file, "{name} pid={}", std::process::id());
        let _ = file.sync_all();
    }
    #[cfg(unix)]
    {
        let pid = std::process::id().to_string();
        // SIGKILL, not abort/exit: the whole point is an unclean death with no
        // destructor, no flush and no recovery path running in this process.
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid])
            .status();
    }
    // A platform without `kill`, or a failed signal, must not let the run
    // continue as if the cut happened; fail loudly and uncleanly.
    std::process::abort();
}
