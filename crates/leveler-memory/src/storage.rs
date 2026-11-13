//! Memory storage path resolution.
//!
//! Every workspace-scoped memory file lives under the CodeLeveler state root,
//! never inside the user's repository. The workspace directory name combines a
//! readable project slug with a short hash of the repository identity so two
//! clones of the same remote share memory while unrelated directories do not.

use std::path::{Path, PathBuf};

use crate::v2::{self, V2MemoryScope, V2StorageError, ensure_scope_initialized};

/// Root-relative directory layout for one workspace's memory.
#[derive(Debug, Clone)]
pub struct MemoryStorage {
    /// `<root>/global/`
    global_dir: PathBuf,
    /// `<root>/workspaces/{slug}-{hash8}/`
    workspace_dir: PathBuf,
    /// The original workspace path (diagnostics and display).
    workspace_path: PathBuf,
    /// When true, workspace writes are skipped (temporary working directories).
    ephemeral: bool,
}

impl MemoryStorage {
    /// Resolve memory paths for `cwd` under an explicit memory root.
    ///
    /// The caller owns the root (normally `<LevelerHome>/memory`) so
    /// this crate never invents a second notion of the application home.
    pub fn for_workspace(cwd: &Path, root: &Path) -> Self {
        Self {
            global_dir: root.join("global"),
            workspace_dir: root.join("workspaces").join(workspace_dir_name(cwd)),
            workspace_path: cwd.to_path_buf(),
            ephemeral: is_ephemeral_cwd(cwd),
        }
    }

    /// A flat root where the global and workspace scopes are the same directory.
    ///
    /// The v2 stores use this when they already hold an explicit scope
    /// directory and only need the index's storage handle.
    pub fn new_flat(cwd: &Path, root: &Path) -> Self {
        Self {
            global_dir: root.to_path_buf(),
            workspace_dir: root.to_path_buf(),
            workspace_path: cwd.to_path_buf(),
            ephemeral: false,
        }
    }

    pub fn global_dir(&self) -> &Path {
        &self.global_dir
    }

    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }

    pub fn workspace_path(&self) -> &Path {
        &self.workspace_path
    }

    pub fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// Storage root that contains the scope directories.
    fn storage_root(&self) -> &Path {
        self.global_dir.parent().unwrap_or(&self.global_dir)
    }

    /// Initialize both scopes and their generated manifests.
    ///
    /// Ephemeral workspaces keep their memory in the global scope only, so
    /// temporary directories never accumulate workspace state.
    pub fn ensure_initialized(&self) -> Result<(), V2StorageError> {
        ensure_scope_initialized(self.storage_root(), &self.global_dir, V2MemoryScope::Global)?;
        if !self.ephemeral {
            ensure_scope_initialized(
                self.storage_root(),
                &self.workspace_dir,
                V2MemoryScope::Workspace,
            )?;
        } else {
            // No memory belongs to a temporary working directory, but the
            // scope root is still passed to V2MemoryAccessPolicy, which
            // canonicalizes both roots. Create only the empty directory so
            // that a global-scope read or maintenance open does not fail on
            // a root the policy is told to guard.
            std::fs::create_dir_all(&self.workspace_dir).map_err(|source| V2StorageError::Io {
                operation: "create ephemeral v2 workspace directory",
                path: self.workspace_dir.clone(),
                source,
            })?;
        }
        Ok(())
    }

    /// Publish a user-authored note into the global observation inbox.
    ///
    /// The note is normalized into Markdown (a short first line becomes the
    /// observation heading), initialized lazily so `/remember` works in a fresh
    /// installation, and rejected before any write when it exceeds the limit.
    pub fn save_remember_note(&self, content: &str) -> Result<Option<PathBuf>, V2StorageError> {
        let normalized = normalize_memory_content(content);
        if normalized.is_empty() {
            return Ok(None);
        }
        if normalized.len() > crate::v2::MAX_MANUAL_OBSERVATION_BYTES {
            return Err(V2StorageError::ObservationTooLarge {
                actual_bytes: normalized.len(),
                limit_bytes: crate::v2::MAX_MANUAL_OBSERVATION_BYTES,
            });
        }
        ensure_scope_initialized(self.storage_root(), &self.global_dir, V2MemoryScope::Global)?;
        v2::persist_observation(&self.global_dir, &normalized).map(Some)
    }

    pub fn global_memory_file(&self) -> PathBuf {
        self.global_dir.join("MEMORY.md")
    }

    pub fn workspace_memory_file(&self) -> PathBuf {
        self.workspace_dir.join("MEMORY.md")
    }

    /// Files the Memory UI may browse, in manifest order with per-scope caps.
    pub fn list_memory_files(&self) -> std::io::Result<Vec<PathBuf>> {
        crate::storage_v2::list_memory_files(&self.global_dir, &self.workspace_dir)
    }
}

/// Normalize a `/remember` note into Markdown: keep existing headings, promote
/// a short first line to an `##` heading, and wrap long first lines as a body.
pub fn normalize_memory_content(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with('#') {
        return trimmed.to_string();
    }
    match trimmed.find('\n') {
        None => format!("## {trimmed}"),
        Some(position) => {
            let first_line = trimmed.get(..position).unwrap_or("").trim();
            let rest = trimmed.get(position..).unwrap_or("").trim();
            if first_line.len() <= 80 {
                format!("## {first_line}\n\n{rest}")
            } else {
                format!("## Note\n\n{trimmed}")
            }
        }
    }
}

/// Human-friendly workspace directory name: `{slug}-{hash8}`.
pub fn workspace_dir_name(cwd: &Path) -> String {
    let (slug, hash_input) = match repo_identity(cwd) {
        Some(repo_id) => {
            let slug_source = repo_id.rsplit('/').next().unwrap_or(&repo_id);
            (slugify(slug_source, 40), repo_id)
        }
        None => {
            let canonical = dunce::canonicalize(cwd).unwrap_or_else(|_| {
                tracing::warn!(
                    path = %cwd.display(),
                    "could not canonicalize workspace path for memory hash; using raw path"
                );
                cwd.to_path_buf()
            });
            let dir_name = canonical
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace");
            (
                slugify(dir_name, 40),
                canonical.to_string_lossy().into_owned(),
            )
        }
    };
    let slug = if slug.is_empty() { "workspace" } else { &slug };
    let hash = blake3::hash(hash_input.as_bytes());
    let hex = hash.to_hex();
    let hash8 = hex.get(..8).unwrap_or(&*hex);
    format!("{slug}-{hash8}")
}

/// Normalized `org/repo` identity of the git repository containing `cwd`.
///
/// Uses the `git` CLI rather than a linked libgit2: the workspace already owns
/// a subprocess-based VCS layer, and a subdirectory must resolve to its
/// repository so memory follows the project rather than the launch directory.
fn repo_identity(cwd: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["config", "--get", "remote.origin.url"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    normalize_remote_url(url.trim())
}

/// Normalize a git remote URL to `org/repo` form.
/// Strips protocol prefix, host, and trailing `.git`.
fn normalize_remote_url(url: &str) -> Option<String> {
    let path = if let Some(colon_pos) = url.find(':') {
        // SSH format: git@github.com:org/repo.git
        if url
            .get(..colon_pos)
            .is_some_and(|head| head.contains('@') && !head.contains('/'))
        {
            url.get(colon_pos + 1..)?
        } else {
            // HTTPS/SSH-with-scheme: https://github.com/org/repo.git
            url.split("//")
                .nth(1)
                .and_then(|after_scheme| after_scheme.split_once('/'))
                .map(|(_, path)| path)?
        }
    } else {
        return None;
    };

    let cleaned = path
        .trim_end_matches(".git")
        .trim_end_matches('/')
        .trim_start_matches('/');

    if cleaned.is_empty() || !cleaned.contains('/') {
        return None;
    }
    Some(cleaned.to_string())
}

/// URL-safe slug: lowercase, non-alphanumerics become `-`, consecutive dashes
/// collapse, truncated to `max_len` characters and trimmed of edge dashes.
pub fn slugify(input: &str, max_len: usize) -> String {
    let slug: String = input
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut collapsed = String::with_capacity(slug.len());
    let mut last_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !last_dash {
                collapsed.push(c);
            }
            last_dash = true;
        } else {
            collapsed.push(c);
            last_dash = false;
        }
    }
    collapsed
        .trim_matches('-')
        .chars()
        .take(max_len)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

/// True when `cwd` resides under a system temp directory.
fn is_ephemeral_cwd(cwd: &Path) -> bool {
    let canonical = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let canonical_text = canonical.to_string_lossy();
    let raw_text = cwd.to_string_lossy();

    let temp = std::env::temp_dir();
    let temp_canonical = dunce::canonicalize(&temp).unwrap_or(temp);

    canonical.starts_with(&temp_canonical)
        || raw_text.starts_with("/tmp/")
        || raw_text.starts_with("/var/tmp/")
        || (raw_text.contains("/var/folders/") && raw_text.contains("/T/"))
        || canonical_text.starts_with("/private/tmp/")
        || canonical_text.starts_with("/private/var/tmp/")
        || (canonical_text.contains("/private/var/folders/") && canonical_text.contains("/T/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_urls_normalize_to_org_repo() {
        for url in [
            "git@github.com:org/repo.git",
            "https://github.com/org/repo.git",
            "ssh://git@github.com/org/repo.git",
            "https://github.com/org/repo",
        ] {
            assert_eq!(
                normalize_remote_url(url).as_deref(),
                Some("org/repo"),
                "{url}"
            );
        }
        assert_eq!(normalize_remote_url("nonsense"), None);
        assert_eq!(normalize_remote_url("https://github.com/only"), None);
    }

    #[test]
    fn slugify_collapses_and_truncates() {
        assert_eq!(slugify("Hello, World!", 40), "hello-world");
        assert_eq!(slugify("a---b", 40), "a-b");
        assert_eq!(slugify("abcdef", 3), "abc");
        assert_eq!(slugify("---", 40), "");
    }

    #[test]
    fn workspace_name_is_stable_for_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let first = workspace_dir_name(dir.path());
        let second = workspace_dir_name(dir.path());
        assert_eq!(first, second);
        assert!(first.contains('-'));
    }

    /// An ephemeral workspace gets no workspace scope of its own, but both
    /// scope roots are still handed to the access policy that guards global
    /// reads and writes. A missing workspace directory must not make the
    /// global scope unusable.
    #[test]
    fn an_ephemeral_workspace_still_roots_the_access_policy() {
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let storage = MemoryStorage::for_workspace(cwd.path(), root.path());
        assert!(storage.is_ephemeral());
        storage.ensure_initialized().unwrap();
        let policy = crate::v2_access::V2MemoryAccessPolicy::new(
            storage.global_dir(),
            storage.workspace_dir(),
        )
        .expect("an ephemeral workspace root is still a valid access-policy root");
        assert!(policy.classify_path(&storage.global_memory_file()).is_ok());
    }

    #[test]
    fn remember_notes_are_normalized_to_markdown() {
        assert_eq!(normalize_memory_content("  "), "");
        assert_eq!(
            normalize_memory_content("Prefer concise answers"),
            "## Prefer concise answers"
        );
        assert_eq!(
            normalize_memory_content("Deployments\nRequire the staging flag"),
            "## Deployments\n\nRequire the staging flag"
        );
        assert_eq!(
            normalize_memory_content("## Existing\n\nBody"),
            "## Existing\n\nBody"
        );
        let long_first_line = "x".repeat(81);
        assert_eq!(
            normalize_memory_content(&format!("{long_first_line}\nbody")),
            format!("## Note\n\n{long_first_line}\nbody")
        );
    }
}
