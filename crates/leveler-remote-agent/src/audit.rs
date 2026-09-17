//! What the host writes down about remote activity.
//!
//! One JSONL line per event under `~/.leveler/remote/audit/`, `0600`, rotated
//! by day and pruned after a retention window. It exists so a user can answer
//! "what did that phone do on my machine" — and it is written so that having
//! the answer never becomes a second exposure:
//!
//! - **No bodies.** A command's *kind* is recorded, never its text. Message
//!   content, tool output and file contents are the things a remote log would
//!   be most tempting to keep and most damaging to leak.
//! - **No plaintext paths.** A project is named by its id, which is already a
//!   hash of the repository path.
//! - **Hashed device ids.** Truncated `SHA-256`, so lines can be correlated
//!   with each other without the file being a list of the user's devices.
//!
//! Failures here are swallowed. An audit log that could refuse a command by
//! failing to write would be a new way to take the machine down, and the record
//! is not worth that.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// How long audit files are kept, in days.
pub const DEFAULT_RETENTION_DAYS: i64 = 30;

/// A truncated hash, for identifiers that should be correlatable but not
/// readable.
///
/// One definition, shared with the relay through the protocol crate: two sides
/// that hashed differently could not have their logs read together, which is
/// the only reason to log an identifier at all.
pub use leveler_remote_protocol::hashed_label as hashed;

/// One recorded fact. Every variant carries only ids and outcomes.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEvent {
    /// A device's session stream was accepted.
    StreamOpened { device: String, project: String },
    /// A stream ended, with the reason the agent knows.
    StreamClosed {
        device: String,
        project: String,
        reason: String,
    },
    /// A command reached the runtime.
    Delivered {
        device: String,
        project: String,
        command: String,
    },
    /// A frame was refused, and by which check.
    Refused {
        device: String,
        project: String,
        /// The command kind when the frame parsed far enough to have one.
        command: Option<String>,
        code: String,
    },
    /// An RPC was served.
    Rpc {
        device: String,
        project: String,
        method: String,
        result: String,
    },
    /// The host denied an approval on its own, because nobody could answer it.
    ApprovalTimeout {
        device: String,
        project: String,
        /// Kept readable so a line can be matched with the runtime's own log;
        /// it names a request, not a person.
        approval: String,
    },
}

/// Appends [`AuditEvent`]s to a rotating file.
#[derive(Debug)]
pub struct AuditLog {
    dir: PathBuf,
    retention_days: i64,
    /// The day of the file currently being written, so pruning runs once per
    /// rotation rather than on every line.
    current_day: std::sync::Mutex<Option<String>>,
}

impl AuditLog {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            retention_days: DEFAULT_RETENTION_DAYS,
            current_day: std::sync::Mutex::new(None),
        }
    }

    pub fn with_retention_days(mut self, days: i64) -> Self {
        self.retention_days = days;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Record one event, best effort.
    pub fn record(&self, event: AuditEvent) {
        let now = chrono::Utc::now();
        if let Err(error) = self.append(&now, &event) {
            // Warn once per failure rather than escalating: losing a log line
            // must not cost the user a command.
            tracing::warn!(%error, dir = %self.dir.display(), "could not write the remote audit log");
        }
    }

    fn append(
        &self,
        now: &chrono::DateTime<chrono::Utc>,
        event: &AuditEvent,
    ) -> std::io::Result<()> {
        let day = now.format("%Y-%m-%d").to_string();
        let path = self.dir.join(format!("audit-{day}.jsonl"));

        let rotated = {
            let mut current = self.current_day.lock().unwrap();
            let rotated = current.as_deref() != Some(day.as_str());
            *current = Some(day.clone());
            rotated
        };
        if rotated {
            self.ensure_dir()?;
            self.prune(now);
        }

        let mut line = serde_json::to_value(event).map_err(std::io::Error::other)?;
        if let Some(object) = line.as_object_mut() {
            object.insert(
                "ts".to_string(),
                serde_json::Value::String(now.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            );
        }

        let mut options = std::fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        writeln!(file, "{line}")
    }

    fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Delete files older than the retention window.
    ///
    /// By filename rather than by mtime: the name states which day a file is
    /// for, and a touched file should not get a longer life than its contents
    /// deserve.
    fn prune(&self, now: &chrono::DateTime<chrono::Utc>) {
        let cutoff = *now - chrono::Duration::days(self.retention_days);
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(day) = name
                .strip_prefix("audit-")
                .and_then(|rest| rest.strip_suffix(".jsonl"))
            else {
                continue;
            };
            let Ok(date) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") else {
                continue;
            };
            if date < cutoff.date_naive() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_one_line_with_hashed_ids_and_no_body() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(dir.path().join("audit"));
        log.record(AuditEvent::Delivered {
            device: hashed("dev_phone"),
            project: "a1b2c3d4e5f60708".to_string(),
            command: "submit_message".to_string(),
        });

        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let path = dir.path().join("audit").join(format!("audit-{day}.jsonl"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);

        let line: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(line["event"], "delivered");
        assert_eq!(line["command"], "submit_message");
        assert!(line["ts"].as_str().unwrap().ends_with('Z'));
        assert_ne!(
            line["device"], "dev_phone",
            "a device id must not appear in the clear"
        );
        assert_eq!(line["device"], hashed("dev_phone"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "the audit log is not for other users");
        }
    }

    /// The same input must hash the same way every time, or lines about one
    /// device cannot be correlated — which is the only reason to log it at all.
    #[test]
    fn hashing_is_stable_and_short() {
        assert_eq!(hashed("dev_phone"), hashed("dev_phone"));
        assert_ne!(hashed("dev_phone"), hashed("dev_other"));
        assert_eq!(hashed("dev_phone").len(), 16);
    }

    #[test]
    fn files_older_than_the_retention_window_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit");
        std::fs::create_dir_all(&audit).unwrap();

        let old = (chrono::Utc::now() - chrono::Duration::days(45))
            .format("%Y-%m-%d")
            .to_string();
        let recent = (chrono::Utc::now() - chrono::Duration::days(2))
            .format("%Y-%m-%d")
            .to_string();
        std::fs::write(audit.join(format!("audit-{old}.jsonl")), "{}\n").unwrap();
        std::fs::write(audit.join(format!("audit-{recent}.jsonl")), "{}\n").unwrap();
        // Not ours; must survive.
        std::fs::write(audit.join("notes.txt"), "keep me").unwrap();

        let log = AuditLog::new(&audit);
        log.record(AuditEvent::StreamOpened {
            device: hashed("dev"),
            project: "p".to_string(),
        });

        assert!(!audit.join(format!("audit-{old}.jsonl")).exists());
        assert!(audit.join(format!("audit-{recent}.jsonl")).exists());
        assert!(audit.join("notes.txt").exists());
    }
}
