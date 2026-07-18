//! Model-facing copy for recoverable tool errors.
//!
//! Prefer these over raw OS errno strings so the agent knows the next step.

/// File path does not exist.
pub fn missing_file(path: &str) -> String {
    format!(
        "file not found: `{path}`. Check the path with `list_files` or `grep`, \
         then `read_file` a concrete file under the workspace."
    )
}

/// Path exists but is a directory.
pub fn path_is_directory(path: &str) -> String {
    format!(
        "`{path}` is a directory, not a file. Use `list_files` with path `{path}` \
         (or a parent) to list entries, then `read_file` on a concrete file."
    )
}

/// Path exists but is not a directory (for list_files).
pub fn path_not_directory(path: &str) -> String {
    format!(
        "`{path}` is not a directory. Use `read_file` for files, or `list_files` \
         on a parent directory."
    )
}

/// OS sandbox blocked a write outside allowed roots (or under protected `.git`).
pub fn sandbox_write_denied() -> &'static str {
    "\n[recoverable] Command ran under workspace write confinement: writes outside \
     the workspace (except temp/toolchain caches) are blocked, and the workspace \
     `.git` tree is write-protected (so `git pull`/`commit`/`fetch` that touch \
     index/refs will fail with Operation not permitted). Next: call \
     `request_permissions` with `filesystem=unrestricted` (and `network=true` for \
     remote git), or `full_access=true`, wait for approval, then retry. Do not claim \
     the failure is pre-existing or unrelated.\n"
}

/// Generic permission/preflight refusal with next action.
pub fn permission_refused(detail: &str, next: &str) -> String {
    format!("[recoverable] {detail} Next: {next}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_mentions_list_files() {
        let s = missing_file("nope.rs");
        assert!(s.contains("file not found"));
        assert!(s.contains("list_files"));
        assert!(s.contains("read_file"));
    }

    #[test]
    fn directory_mentions_list_files() {
        let s = path_is_directory("configs");
        assert!(s.contains("directory"));
        assert!(s.contains("list_files"));
    }

    #[test]
    fn path_not_directory_mentions_read_file() {
        let s = path_not_directory("file.rs");
        assert!(s.contains("not a directory"));
        assert!(s.contains("read_file"));
    }

    #[test]
    fn sandbox_hint_mentions_request_permissions() {
        let s = sandbox_write_denied();
        assert!(s.contains("request_permissions"));
        assert!(s.contains("filesystem=unrestricted") || s.contains("full_access"));
        assert!(s.contains("[recoverable]"));
        assert!(
            s.contains(".git") && s.contains("git pull"),
            "must steer git mutate through FS elevation: {s}"
        );
    }

    #[test]
    fn permission_refused_includes_next_step() {
        let s = permission_refused("network blocked", "request_permissions with network=true");
        assert!(s.contains("[recoverable]"));
        assert!(s.contains("network blocked"));
        assert!(s.contains("Next:"));
        assert!(s.contains("request_permissions"));
    }
}
