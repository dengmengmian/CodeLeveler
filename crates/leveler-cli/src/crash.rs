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
        previous(info);
    }));
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
}
