//! Crash records (`~/.leveler/logs/crash/`).
//!
//! A panic in a TUI is close to invisible: the alternate screen is torn down,
//! the message scrolls past, and all anyone can report afterwards is "it
//! closed". This writes a record before that happens so there is something to
//! read later.
//!
//! **Local only.** Nothing is transmitted. The file holds the panic message,
//! the location, a backtrace when one is available, and build/platform facts —
//! never conversation content, prompts, keys, or file contents. A user can read
//! it, and decide for themselves whether to attach it to a report.

use std::io::Write;
use std::path::{Path, PathBuf};

/// How many records to keep. Enough to catch a repeating crash, few enough that
/// the directory never becomes a problem of its own.
const KEEP: usize = 20;

pub(crate) fn crash_dir() -> Option<PathBuf> {
    leveler_core::leveler_home_dir_from(|k| std::env::var_os(k))
        .map(|root| leveler_core::LevelerHome::from_root(root).crash_dir())
}

/// Install the panic hook.
///
/// Chains to the previously installed hook, so the TUI's terminal-restore hook
/// keeps working: whichever is installed later runs first, and both run.
pub(crate) fn install(version: &str) {
    let version = version.to_string();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort: a failure while recording a crash must never mask the
        // crash itself, so every error here is swallowed deliberately.
        if let Some(dir) = crash_dir() {
            let _ = write_record(&dir, &render(info, &version));
        }
        // A second panic while this hook is running aborts the process.
        // `catch_unwind` does not intercept it: the runtime aborts as soon as
        // the nested panic starts. The default printer uses `eprintln!`, and
        // that panics when stderr is a closed PTY (EIO). Skip the printer
        // only in that case. The crash record already has the original
        // message, and a live stderr still gets the normal panic print.
        if stderr_accepts_a_write() {
            previous(info);
        }
    }));
}

/// Print a fatal error from the process entry point.
///
/// `eprintln!` panics when stderr returns an error. A TUI whose PTY master has
/// already closed makes that write return `EIO`, so the error report itself
/// becomes a panic and the hook then panics again trying to print it. A failed
/// write is copied into the crash directory so the original text is not lost.
pub(crate) fn report_fatal(prefix: &str, detail: &str) {
    let body = format!("{prefix}\n  {detail}\n");
    if write_stderr(&body).is_err()
        && let Some(dir) = crash_dir()
    {
        let _ = write_record(
            &dir,
            &format!("leveler fatal report\nstderr: unavailable\n{body}"),
        );
    }
}

/// `eprintln!` panics on a broken stderr. This write returns the error instead.
fn write_stderr(text: &str) -> std::io::Result<()> {
    let mut err = std::io::stderr();
    err.write_all(text.as_bytes())?;
    err.flush()
}

/// `write_all(&[])` returns `Ok` without a syscall. An empty `write` still
/// calls `write(2)`, and a PTY whose master is already closed returns `EIO`.
fn stderr_accepts_a_write() -> bool {
    std::io::stderr().write(&[]).is_ok()
}

/// The record body. Kept as a pure function so its content — and what it must
/// never contain — is testable.
fn render(info: &std::panic::PanicHookInfo<'_>, version: &str) -> String {
    let message = panic_message(info);
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "unknown".to_string());
    let backtrace = std::backtrace::Backtrace::force_capture();
    format!(
        "leveler {version}\n\
         platform: {} {}\n\
         time: {}\n\
         location: {location}\n\
         panic: {message}\n\
         \n\
         backtrace:\n{backtrace}\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        leveler_core::now().to_rfc3339(),
    )
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Write one record and prune old ones.
fn write_record(dir: &Path, body: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = format!(
        "{}-{}.log",
        leveler_core::now().format("%Y%m%dT%H%M%S"),
        std::process::id()
    );
    let path = dir.join(name);
    let mut file = std::fs::File::create(&path)?;
    file.write_all(body.as_bytes())?;
    prune(dir, KEEP);
    Ok(path)
}

/// Keep the newest `keep` records by filename (timestamped, so lexical order is
/// chronological).
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("log"))
        .collect();
    if logs.len() <= keep {
        return;
    }
    logs.sort();
    for old in &logs[..logs.len() - keep] {
        let _ = std::fs::remove_file(old);
    }
}

/// Newest-first crash records, for `leveler doctor`.
pub(crate) fn recent(limit: usize) -> Vec<PathBuf> {
    let Some(dir) = crash_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut logs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("log"))
        .collect();
    logs.sort();
    logs.reverse();
    logs.truncate(limit);
    logs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "leveler-crash-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_record_is_written_and_readable() {
        let dir = tmp("write");
        let path = write_record(&dir, "leveler 0.1\npanic: boom\n").unwrap();
        let body = std::fs::read_to_string(path).unwrap();
        assert!(body.contains("panic: boom"));
    }

    /// An unbounded crash directory turns one bug into a second one.
    #[test]
    fn old_records_are_pruned() {
        let dir = tmp("prune");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..30 {
            std::fs::write(dir.join(format!("2026010{i:02}T000000-1.log")), "x").unwrap();
        }
        prune(&dir, KEEP);
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, KEEP, "must keep exactly the newest {KEEP}");
        assert!(
            dir.join("20260100029T000000-1.log").exists()
                || std::fs::read_dir(&dir)
                    .unwrap()
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().contains("29")),
            "the newest record must survive"
        );
    }

    #[test]
    fn pruning_is_a_no_op_below_the_limit() {
        let dir = tmp("nokeep");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.log"), "x").unwrap();
        prune(&dir, KEEP);
        assert!(dir.join("a.log").exists());
    }

    /// Non-log files in the directory are not ours to delete.
    #[test]
    fn pruning_ignores_other_files() {
        let dir = tmp("other");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..30 {
            std::fs::write(dir.join(format!("2026010{i:02}T000000-1.log")), "x").unwrap();
        }
        std::fs::write(dir.join("NOTES.txt"), "keep me").unwrap();
        prune(&dir, KEEP);
        assert!(dir.join("NOTES.txt").exists());
    }

    /// The record is meant to be safe to attach to a bug report.
    #[test]
    fn the_record_carries_diagnosis_and_nothing_else() {
        let rendered = std::panic::catch_unwind(|| {
            let hook = std::panic::take_hook();
            let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let sink = captured.clone();
            std::panic::set_hook(Box::new(move |info| {
                *sink.lock().unwrap() = render(info, "9.9.9");
            }));
            let _ = std::panic::catch_unwind(|| panic!("boom in a test"));
            std::panic::set_hook(hook);
            captured.lock().unwrap().clone()
        })
        .unwrap();

        assert!(rendered.contains("boom in a test"), "{rendered}");
        assert!(rendered.contains("leveler 9.9.9"), "{rendered}");
        assert!(rendered.contains("location:"), "{rendered}");
        assert!(rendered.contains(std::env::consts::OS), "{rendered}");
        // Whatever else changes, the record must stay a diagnosis, not a dump.
        assert!(!rendered.contains("api_key"), "{rendered}");
        assert!(!rendered.contains("sk-"), "{rendered}");
    }

    /// A normal stderr still receives the fatal report.
    #[test]
    fn a_fatal_report_is_printed_when_stderr_is_open() {
        const CHILD: &str = "LEVELER_TEST_FATAL_REPORT_OPEN_STDERR";
        if std::env::var_os(CHILD).is_some() {
            report_fatal("error:", "ordinary-failure");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crash::tests::a_fatal_report_is_printed_when_stderr_is_open",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "status {:?}", output.status);
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains("ordinary-failure"), "{err}");
    }

    /// The production failure: stderr is a PTY slave whose master is already
    /// gone, so the write returns EIO. Reporting that must not panic, and the
    /// original text must still land in the crash directory.
    #[test]
    fn a_fatal_report_is_kept_when_the_pty_stderr_is_already_closed() {
        const CHILD: &str = "LEVELER_TEST_FATAL_REPORT_CLOSED_PTY";
        if std::env::var_os(CHILD).is_some() {
            report_fatal("error:", "original-model-error");
            return;
        }
        let home = tmp("fatal-pty");
        let output = on_closed_pty(
            "crash::tests::a_fatal_report_is_kept_when_the_pty_stderr_is_already_closed",
            "2",
            &[("LEVELER_HOME", home.to_str().unwrap()), (CHILD, "1")],
        );
        assert!(
            output.status.success(),
            "closed stderr must not panic the reporter: status {:?} stderr {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let recorded = crash_text(&home);
        assert!(
            recorded.contains("original-model-error"),
            "the original error must be recorded, got {recorded}"
        );
    }

    /// The panic hook itself must not panic when stderr is a closed PTY.
    /// A second panic aborts the process and drops the original message.
    #[test]
    fn a_panic_hook_records_the_original_panic_when_stderr_is_a_closed_pty() {
        const CHILD: &str = "LEVELER_TEST_PANIC_HOOK_CLOSED_PTY";
        if std::env::var_os(CHILD).is_some() {
            // The default printer panics when stderr returns EIO. Stand in for
            // it here: this crate cannot install the real default hook after
            // the test harness has replaced it.
            std::panic::set_hook(Box::new(|_| panic!("stderr-printer")));
            install("test-version");
            panic!("sentinel-original-panic");
        }
        let home = tmp("hook-pty");
        let output = on_closed_pty(
            "crash::tests::a_panic_hook_records_the_original_panic_when_stderr_is_a_closed_pty",
            "2",
            &[("LEVELER_HOME", home.to_str().unwrap()), (CHILD, "1")],
        );
        assert!(
            output.status.code() == Some(101),
            "the original panic must unwind instead of aborting inside the hook: status {:?}",
            output.status
        );
        let recorded = crash_text(&home);
        assert!(
            recorded.contains("sentinel-original-panic"),
            "crash record lost the original panic: {recorded}"
        );
    }

    /// A live stderr must still reach the previous printer. Skipping it
    /// whenever the hook runs would hide ordinary panics.
    #[test]
    fn a_panic_hook_still_prints_when_stderr_is_open() {
        const CHILD: &str = "LEVELER_TEST_PANIC_HOOK_OPEN_STDERR";
        if std::env::var_os(CHILD).is_some() {
            std::panic::set_hook(Box::new(|_| {
                let _ = write_stderr("printer-was-called\n");
            }));
            install("test-version");
            panic!("sentinel-open-stderr");
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crash::tests::a_panic_hook_still_prints_when_stderr_is_open",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(101),
            "an ordinary panic must still fail the process: {:?}",
            output.status
        );
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(
            err.contains("printer-was-called"),
            "open stderr must still reach the previous printer: {err}"
        );
    }

    fn crash_text(home: &std::path::Path) -> String {
        let dir = home.join("logs").join("crash");
        let mut out = String::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return out;
        };
        for entry in entries.flatten() {
            if let Ok(body) = std::fs::read_to_string(entry.path()) {
                out.push_str(&body);
            }
        }
        out
    }

    /// Re-exec this test on a PTY slave whose master is already closed.
    /// The helper is Python so this crate can keep `forbid(unsafe_code)`.
    fn on_closed_pty(test: &str, fds: &str, env: &[(&str, &str)]) -> std::process::Output {
        let script = r#"
import os, pty, sys
master, slave = pty.openpty()
os.close(master)
for fd in os.environ["LEVELER_TEST_PTY_FDS"].split(","):
    os.dup2(slave, int(fd))
os.close(slave)
os.execv(sys.argv[1], sys.argv[1:])
"#;
        let mut cmd = std::process::Command::new("python3");
        cmd.arg("-c")
            .arg(script)
            .arg(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test)
            .arg("--nocapture")
            .env("LEVELER_TEST_PTY_FDS", fds);
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.output().expect("python pty helper")
    }
}
