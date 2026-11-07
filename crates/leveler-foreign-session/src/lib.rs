//! Read-only adapters for sessions produced by other coding agents.
//!
//! The crate owns two things and nothing else:
//!
//! * where Claude Code and Codex keep their local session records, and
//! * how to turn those records into a bounded, source-faithful event stream.
//!
//! It never writes to, migrates, repairs, or deletes a foreign store. A
//! caller receives ordinary values; no background watcher or cached index is
//! kept between calls, so the source file stays the single authority.

mod claude;
mod codex;
mod model;
mod read;
mod walk;

pub use model::{
    ForeignEventDetail, ForeignEventKind, ForeignEventRef, ForeignEventSearchHit, ForeignOpenCall,
    ForeignSessionDigest, ForeignSessionError, ForeignSessionIndex, ForeignSessionStats,
    ForeignSessionSummary, ForeignTool,
};
pub use walk::{
    MAX_EVENT_TEXT_BYTES, MAX_INDEX_EVENTS, MAX_LINE_BYTES, MAX_SEARCH_HITS, MAX_SESSION_AGE_DAYS,
    MAX_SESSIONS_PER_SOURCE,
};

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use leveler_core::EnvSnapshot;

/// Resolve a tool's store root without inventing a location.
///
/// `CLAUDE_CONFIG_DIR` / `CODEX_HOME` are honored exactly as the vendor tools
/// honor them. The user home is required; a missing home is an error at the
/// call site rather than a guess in this crate.
pub fn default_root(tool: ForeignTool, env: &EnvSnapshot) -> Option<PathBuf> {
    // Vendor stores use the OS user home, independently of CodeLeveler's home.
    let user_home = env
        .var_os("HOME")
        .or_else(|| env.var_os("USERPROFILE"))
        .map(PathBuf::from);
    let override_dir = match tool {
        ForeignTool::ClaudeCode => env.var_os("CLAUDE_CONFIG_DIR"),
        ForeignTool::Codex => env.var_os("CODEX_HOME"),
    }
    .map(PathBuf::from);
    override_dir.or_else(|| user_home.map(|user_home| user_home.join(tool.default_dir_name())))
}

/// Bounded discovery for a picker.
///
/// `cwd` filters to sessions whose recorded working directory matches; a
/// caller that passes `None` gets the newest sessions of every project. The
/// scan stops at `limit` (clamped to [`MAX_SESSIONS_PER_SOURCE`]) and at
/// [`MAX_SESSION_AGE_DAYS`], and reads only a bounded head of each file.
pub fn discover(
    tool: ForeignTool,
    root: &Path,
    cwd: Option<&Path>,
    limit: usize,
    now: SystemTime,
) -> Result<Vec<ForeignSessionSummary>, ForeignSessionError> {
    match tool {
        ForeignTool::ClaudeCode => claude::discover(root, cwd, limit, now),
        ForeignTool::Codex => codex::discover(root, cwd, limit, now),
    }
}

/// Resolve exactly one native session id.
///
/// The id must be a single path component. A caller-supplied id never selects
/// a directory or traverses outside the store root.
pub fn find(
    tool: ForeignTool,
    root: &Path,
    native_id: &str,
) -> Result<ForeignSessionSummary, ForeignSessionError> {
    validate_native_id(native_id)?;
    match tool {
        ForeignTool::ClaudeCode => claude::find(root, native_id),
        ForeignTool::Codex => codex::find(root, native_id),
    }
}

/// A bounded event index: one preview row per meaningful record.
///
/// The index is rebuilt from the file on every call, so a session that is
/// still being written by another agent is read as it currently is.
pub fn index(summary: &ForeignSessionSummary) -> Result<ForeignSessionIndex, ForeignSessionError> {
    walk::index(summary)
}

/// The continuation digest: task, follow-ups, tail, counts, and open calls.
///
/// This is a single streaming pass with bounded memory. It intentionally does
/// not return the whole transcript; detail stays on disk for the history tool.
pub fn digest(
    summary: &ForeignSessionSummary,
) -> Result<ForeignSessionDigest, ForeignSessionError> {
    walk::digest(summary)
}

/// Read a contiguous range of indexed events, with their full text.
pub fn read_events(
    summary: &ForeignSessionSummary,
    start_index: u32,
    limit: usize,
) -> Result<Vec<ForeignEventDetail>, ForeignSessionError> {
    walk::read_events(summary, start_index, limit)
}

/// Case-insensitive substring search over user-visible event text.
pub fn search(
    summary: &ForeignSessionSummary,
    needle: &str,
    limit: usize,
) -> Result<Vec<ForeignEventSearchHit>, ForeignSessionError> {
    walk::search(summary, needle, limit)
}

pub(crate) fn validate_native_id(native_id: &str) -> Result<(), ForeignSessionError> {
    let valid = (8..=64).contains(&native_id.len())
        && native_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !native_id.starts_with('-');
    if valid {
        Ok(())
    } else {
        Err(ForeignSessionError::InvalidId {
            id: native_id.to_string(),
        })
    }
}

pub(crate) fn io_error(path: &Path, error: std::io::Error) -> ForeignSessionError {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        ForeignSessionError::Permission {
            path: path.to_path_buf(),
        }
    } else {
        ForeignSessionError::Io {
            path: path.to_path_buf(),
            detail: error.to_string(),
        }
    }
}

pub(crate) fn system_time_rfc3339(time: SystemTime) -> String {
    let duration = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let day_secs = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60
    )
}

/// Howard Hinnant's civil-from-days, in the small form this crate needs.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod time_tests {
    use super::*;

    #[test]
    fn unix_epoch_formats_as_rfc3339() {
        assert_eq!(
            system_time_rfc3339(SystemTime::UNIX_EPOCH),
            "1970-01-01T00:00:00Z"
        );
    }
}

#[cfg(test)]
mod default_root_tests {
    use super::*;
    use std::ffi::OsString;

    fn snapshot(values: &[(&str, &str)]) -> EnvSnapshot {
        EnvSnapshot::new(
            values
                .iter()
                .map(|(key, value)| (OsString::from(*key), OsString::from(*value))),
            PathBuf::from("workspace"),
            PathBuf::from("temporary"),
        )
    }

    #[test]
    fn default_roots_use_the_user_home_independently_of_leveler_home() {
        let env = snapshot(&[
            ("HOME", "user-home"),
            ("USERPROFILE", "other-user-home"),
            ("LEVELER_HOME", "leveler-owned-home"),
        ]);
        assert_eq!(
            default_root(ForeignTool::Codex, &env),
            Some(PathBuf::from("user-home").join(".codex"))
        );
        assert_eq!(
            default_root(ForeignTool::ClaudeCode, &env),
            Some(PathBuf::from("user-home").join(".claude"))
        );
    }

    #[test]
    fn default_roots_fall_back_to_userprofile_without_home() {
        let env = snapshot(&[
            ("USERPROFILE", "profile-home"),
            ("LEVELER_HOME", "leveler-owned-home"),
        ]);
        assert_eq!(
            default_root(ForeignTool::Codex, &env),
            Some(PathBuf::from("profile-home").join(".codex"))
        );
        assert_eq!(
            default_root(ForeignTool::ClaudeCode, &env),
            Some(PathBuf::from("profile-home").join(".claude"))
        );
    }

    #[test]
    fn default_roots_honor_vendor_overrides_with_or_without_a_home() {
        for values in [
            vec![
                ("CLAUDE_CONFIG_DIR", "claude-store"),
                ("CODEX_HOME", "codex-store"),
            ],
            vec![
                ("HOME", "user-home"),
                ("CLAUDE_CONFIG_DIR", "claude-store"),
                ("CODEX_HOME", "codex-store"),
            ],
        ] {
            let env = snapshot(&values);
            assert_eq!(
                default_root(ForeignTool::ClaudeCode, &env),
                Some(PathBuf::from("claude-store"))
            );
            assert_eq!(
                default_root(ForeignTool::Codex, &env),
                Some(PathBuf::from("codex-store"))
            );
        }
    }

    #[test]
    fn default_roots_do_not_invent_a_vendor_home_from_leveler_home() {
        for values in [vec![], vec![("LEVELER_HOME", "leveler-owned-home")]] {
            let env = snapshot(&values);
            assert_eq!(default_root(ForeignTool::Codex, &env), None);
            assert_eq!(default_root(ForeignTool::ClaudeCode, &env), None);
        }
    }
}
