//! Wire projection for memory clients.
//!
//! The TUI never opens the memory store. This module turns the runtime's
//! durable state into the protocol's content-free listing, reads exactly one
//! bounded file for a preview pane, and routes forget through the same
//! tombstone path the model uses. No value here comes from a model claim.

use std::path::{Component, Path, PathBuf};

use leveler_client_protocol::{
    UiMemoryActionResult, UiMemoryActivity, UiMemoryFile, UiMemoryFileContent, UiMemoryScope,
    UiMemorySession, UiMemoryStatus,
};
use leveler_core::SessionId;
use leveler_memory::{
    ForgetReason, ForgetRequest, MAX_FORGET_FILE_BYTES, MaintenanceStore, MemoryScope,
};

use super::{MemoryActivity, MemoryRuntime, MemoryScopeSummary, SessionMemoryStatus};

/// One preview is capped at 1 MiB. A topic is at most 256 KiB and an inbox
/// observation 16 KiB, so a real note is never truncated; the cap only bounds
/// what a corrupted or hand-written file can make the UI allocate.
pub(crate) const PREVIEW_BYTES_MAX: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryScopeKind {
    Global,
    Workspace,
}

impl MemoryScopeKind {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "global" => Ok(Self::Global),
            "workspace" => Ok(Self::Workspace),
            other => Err(format!("unknown memory scope `{other}`")),
        }
    }

    fn v2(self) -> MemoryScope {
        match self {
            Self::Global => MemoryScope::Global,
            Self::Workspace => MemoryScope::Workspace,
        }
    }
}

impl MemoryRuntime {
    /// Project both scopes plus one session's capture cursors for a client.
    pub async fn ui_status(&self, session: Option<&SessionId>) -> UiMemoryStatus {
        let status = self.status(session).await;
        UiMemoryStatus {
            rollout: status.rollout.as_str().to_owned(),
            enabled: status.enabled,
            capture_enabled: status.capture_enabled,
            automatic_dream_enabled: status.automatic_dream_enabled,
            manual_dream_enabled: status.manual_dream_enabled,
            batch_dream_enabled: status.batch_dream_enabled,
            workspace_ephemeral: status.workspace_ephemeral,
            global: project_scope("global", &status.global),
            workspace: project_scope("workspace", &status.workspace),
            session: status.session.as_ref().map(project_session),
            activity: project_activity(&status.activity),
        }
    }

    /// Read one scope-relative memory file for a preview pane.
    ///
    /// The path must be a browsable Markdown file inside the named scope. The
    /// read is bounded and never follows a symlink outside the scope; any
    /// failure is returned in `error` so the page never paints an empty file.
    pub async fn read_ui_file(&self, scope: &str, relative: &str) -> UiMemoryFileContent {
        let mut content = UiMemoryFileContent {
            scope: scope.to_owned(),
            path: relative.to_owned(),
            ..Default::default()
        };
        let scope = match MemoryScopeKind::parse(scope) {
            Ok(scope) => scope,
            Err(error) => {
                content.error = Some(error);
                return content;
            }
        };
        let relative = match safe_relative(relative) {
            Ok(relative) => relative,
            Err(error) => {
                content.error = Some(error);
                return content;
            }
        };
        let Some(policy) = self.access_policy().await else {
            content.error = Some("memory is off for this configuration".to_owned());
            return content;
        };
        let root = self.scope_root(scope);
        let absolute = root.join(&relative);
        let read = tokio::task::spawn_blocking(move || {
            policy.read_browsable(&absolute, PREVIEW_BYTES_MAX)
        })
        .await;
        let read = match read {
            Ok(Ok(Some(read))) => read,
            Ok(Ok(None)) => {
                content.error = Some("path is outside both memory scopes".to_owned());
                return content;
            }
            Ok(Err(error)) => {
                content.error = Some(error.to_string());
                return content;
            }
            Err(error) => {
                content.error = Some(format!("memory file read task failed: {error}"));
                return content;
            }
        };
        content.total_bytes = read.total_bytes;
        content.truncated = read.truncated;
        // A truncated read must not produce a hash: forget binds the tombstone
        // to the full file's bytes, and these are only a prefix.
        if !read.truncated {
            content.content_hash = leveler_memory::content_hash(&read.bytes);
        }
        content.content = match utf8_prefix(&read.bytes) {
            Ok(text) => text,
            Err(error) => {
                content.error = Some(error);
                return content;
            }
        };
        content
    }

    /// Forget one previously previewed topic or inbox observation.
    ///
    /// `expected_hash` is the preview's BLAKE3, so a file that changed after
    /// the user looked at it is refused instead of deleted. The store owns the
    /// tombstone, manifest refresh and exclusion ledger.
    pub async fn forget_ui_file(
        &self,
        scope: &str,
        relative: &str,
        expected_hash: &str,
    ) -> UiMemoryActionResult {
        let mut result = UiMemoryActionResult {
            action: "forget".to_owned(),
            ok: false,
            message: String::new(),
        };
        let scope = match MemoryScopeKind::parse(scope) {
            Ok(scope) => scope,
            Err(error) => {
                result.message = error;
                return result;
            }
        };
        let relative = match safe_relative(relative) {
            Ok(relative) => relative,
            Err(error) => {
                result.message = error;
                return result;
            }
        };
        if expected_hash.len() != 64 || !expected_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            result.message =
                "forget requires the SHA-256-style 64-hex BLAKE3 of the previewed file".to_owned();
            return result;
        }
        if scope == MemoryScopeKind::Workspace && self.is_workspace_ephemeral() {
            result.message =
                "workspace memory is ephemeral for this process; there is nothing durable to forget"
                    .to_owned();
            return result;
        }
        let storage = self.storage.clone();
        let expected_hash = expected_hash.to_owned();
        let scope_dir = self.scope_root(scope).to_path_buf();
        let forgotten = tokio::task::spawn_blocking(move || {
            let store = MaintenanceStore::open(
                &scope_dir,
                scope.v2(),
                storage.global_dir(),
                storage.workspace_dir(),
            )
            .map_err(|error| error.to_string())?;
            store
                .forget(&ForgetRequest {
                    relative_path: relative,
                    expected_content_hash: expected_hash,
                    reason: ForgetReason::UserRequest,
                    now: leveler_core::now().timestamp(),
                })
                .map_err(|error| error.to_string())
        })
        .await;
        match forgotten {
            Ok(Ok(forgotten)) => {
                result.ok = true;
                result.message = if forgotten.was_already_forgotten {
                    "This memory file had already been forgotten.".to_owned()
                } else {
                    "Forgot the selected memory file.".to_owned()
                };
            }
            Ok(Err(error)) => result.message = format!("forget refused: {error}"),
            Err(error) => result.message = format!("forget task failed: {error}"),
        }
        result
    }

    fn scope_root(&self, scope: MemoryScopeKind) -> &Path {
        match scope {
            MemoryScopeKind::Global => self.storage.global_dir(),
            MemoryScopeKind::Workspace => self.storage.workspace_dir(),
        }
    }
}

fn project_scope(scope: &str, summary: &MemoryScopeSummary) -> UiMemoryScope {
    let mut files: Vec<UiMemoryFile> = summary
        .files
        .iter()
        .filter_map(|path| project_file(scope, path, &summary.root))
        .collect();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let state = summary.state.as_ref();
    UiMemoryScope {
        scope: scope.to_owned(),
        root: summary.root.display().to_string(),
        files,
        pending_capture_jobs: state.map_or(0, |state| state.pending_capture_jobs),
        running_capture_jobs: state.map_or(0, |state| state.running_capture_jobs),
        retry_capture_jobs: state.map_or(0, |state| state.retry_capture_jobs),
        failed_capture_jobs: state.map_or(0, |state| state.failed_capture_jobs),
        completed_capture_jobs: state.map_or(0, |state| state.completed_capture_jobs),
        pending_observations: state.map_or(0, |state| state.pending_observations),
        hidden_pending_observations: state.map_or(0, |state| state.hidden_pending_observations),
        archive_count: state.map_or(0, |state| state.archive_count),
        tombstone_count: state.map_or(0, |state| state.tombstone_count),
        dream_lease: state
            .map_or("none", |state| match state.dream_lease {
                leveler_memory::DreamLeaseState::None => "none",
                leveler_memory::DreamLeaseState::Active => "active",
                leveler_memory::DreamLeaseState::Expired => "expired",
            })
            .to_owned(),
        last_success_at: state.and_then(|state| state.last_success_at),
        last_failure_at: state.and_then(|state| state.last_failure_at),
        error: summary.error.clone(),
    }
}

fn project_file(scope: &str, path: &Path, root: &Path) -> Option<UiMemoryFile> {
    let relative = path.strip_prefix(root).ok()?;
    let relative_text = relative.to_string_lossy().replace('\\', "/");
    let kind = file_kind(&relative_text);
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let bytes = metadata.len();
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64);
    Some(UiMemoryFile {
        scope: scope.to_owned(),
        path: relative_text,
        kind: kind.to_owned(),
        bytes,
        modified_at,
        deletable: matches!(kind, "topic" | "observation") && bytes <= MAX_FORGET_FILE_BYTES,
    })
}

fn file_kind(relative: &str) -> &'static str {
    if relative == "MEMORY.md" {
        "manifest"
    } else if relative.starts_with("topics/") {
        "topic"
    } else if relative.starts_with("observations/_inbox/") {
        "observation"
    } else if relative.starts_with("archive/") {
        "archive"
    } else {
        "other"
    }
}

fn project_session(session: &SessionMemoryStatus) -> UiMemorySession {
    UiMemorySession {
        session_id: session.session_id.clone(),
        requested: session.requested,
        captured: session.captured,
        indexed: session.indexed,
        pending: session.work.pending,
        active: session.work.running,
        failed: session.work.failed,
    }
}

fn project_activity(activity: &MemoryActivity) -> UiMemoryActivity {
    UiMemoryActivity {
        kind: activity.kind.as_str().to_owned(),
        active: activity.active,
        last_error: activity.last_error.clone(),
        last_captured_range: activity.last_captured_range,
        last_dream_at: activity.last_dream_at,
    }
}

/// A client-supplied path must be a plain relative path. Scope containment
/// and symlink rejection stay in the store's access policy; this only refuses
/// shapes that could never name a browsable file.
fn safe_relative(value: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        return Err("memory path must not be empty".to_owned());
    }
    let path = Path::new(value);
    // `Path::components` normalizes `.` away, so the lexical segments are
    // checked as written: `.`/`..`, a doubled separator or a trailing slash
    // never come from the runtime's own listing.
    if path.is_absolute()
        || value
            .split(['/', '\\'])
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("memory path must be scope-relative: {value}"));
    }
    Ok(path.to_path_buf())
}

/// Convert the prefix a bounded read returned. A cut through a multi-byte
/// character drops only the incomplete tail; invalid bytes elsewhere are an
/// error rather than replacement characters.
fn utf8_prefix(bytes: &[u8]) -> Result<String, String> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.to_owned()),
        Err(error) if error.error_len().is_none() => {
            Ok(std::str::from_utf8(&bytes[..error.valid_up_to()])
                .unwrap_or_default()
                .to_owned())
        }
        Err(_) => Err("memory file is not valid UTF-8".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{file_kind, safe_relative, utf8_prefix};

    #[test]
    fn path_classification_matches_the_store_layout() {
        assert_eq!(file_kind("MEMORY.md"), "manifest");
        assert_eq!(file_kind("topics/editor.md"), "topic");
        assert_eq!(file_kind("observations/_inbox/t1.md"), "observation");
        assert_eq!(file_kind("archive/2026/t1.md"), "archive");
        assert_eq!(file_kind("memory_state.sqlite"), "other");
    }

    #[test]
    fn traversal_and_absolute_paths_are_refused() {
        assert!(safe_relative("../outside.md").is_err());
        assert!(safe_relative("/etc/passwd").is_err());
        assert!(safe_relative("topics/../../outside.md").is_err());
        assert!(safe_relative("topics/./editor.md").is_err());
        assert!(safe_relative("").is_err());
        assert!(safe_relative("topics/editor.md").is_ok());
    }

    #[test]
    fn a_truncated_multibyte_tail_is_dropped_not_replaced() {
        let text = "aé";
        let bytes = text.as_bytes();
        // Cut inside the two-byte `é`.
        let decoded = utf8_prefix(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(decoded, "a");
        assert!(utf8_prefix(&[0xff, 0xfe]).is_err());
    }
}
