//! Workspace path resolution and scope validation (spec §21).
//!
//! Every file/patch tool must route paths through [`Workspace::resolve`] /
//! [`Workspace::resolve_read`] so a model can never write outside the primary
//! repository, escape via `..`, or reach sensitive files (`.env`, keys, `.git`
//! internals, `~/.ssh`, ...). Optional **readonly roots** allow cross-repo
//! reads (e.g. comparing a sibling checkout) without opening the write surface.

use std::path::{Component, Path, PathBuf};

/// Whether a path is resolved for reading only or for mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    /// Primary workspace + optional readonly roots.
    Read,
    /// Primary workspace only (writes, cwd for commands, patches).
    Write,
}

/// Errors from workspace path validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    /// Path is not under the primary root (write) or any allowed root (read).
    #[error("{0}")]
    OutsideWorkspace(String),
    #[error("path `{0}` is denied (sensitive file)")]
    Denied(String),
    #[error("failed to canonicalize workspace root {0}")]
    Root(String),
}

impl WorkspaceError {
    fn outside(path: &Path, primary: &Path, access: PathAccess, readonly: &[PathBuf]) -> Self {
        let path = path.display().to_string();
        let root = primary.display().to_string();
        let message = match access {
            PathAccess::Write => format!(
                "path `{path}` is outside the workspace root `{root}`. \
                 Use a path relative to that root (e.g. `src/lib.rs`), or start \
                 leveler inside the target repository. Cross-repo absolute paths \
                 are blocked for writes by design. To *read* another checkout, \
                 pass `--readonly-root <dir>` (or `readonly_roots` in \
                 `.leveler/config.yaml`)."
            ),
            PathAccess::Read if readonly.is_empty() => format!(
                "path `{path}` is outside the workspace root `{root}`. \
                 Use a path relative to that root, or start leveler inside the \
                 target repository. To read another repo without leaving this \
                 workspace, pass `--readonly-root <dir>` (repeatable) or set \
                 `readonly_roots` in `.leveler/config.yaml`."
            ),
            PathAccess::Read => {
                let extras: Vec<_> = readonly
                    .iter()
                    .map(|p| format!("`{}`", p.display()))
                    .collect();
                format!(
                    "path `{path}` is outside the workspace root `{root}` and \
                     outside readonly roots ({}). Use a path under the primary \
                     root or an absolute path under a configured readonly root.",
                    extras.join(", ")
                )
            }
        };
        WorkspaceError::OutsideWorkspace(message)
    }
}

/// A validated repository root plus optional readonly roots. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    #[cfg(unix)]
    root_fd: std::sync::Arc<std::os::fd::OwnedFd>,
    #[cfg(windows)]
    root_dir: std::sync::Arc<cap_std::fs::Dir>,
    /// Extra trees allowed for [`PathAccess::Read`] only (canonicalized).
    readonly_roots: Vec<PathBuf>,
}

impl Workspace {
    /// Create a workspace from a root directory, canonicalizing it so all later
    /// comparisons are against a real absolute path.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let root = root.as_ref();
        let canonical = root
            .canonicalize()
            .map_err(|_| WorkspaceError::Root(root.display().to_string()))?;
        #[cfg(unix)]
        let root_fd = rustix::fs::open(
            &canonical,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| WorkspaceError::Root(root.display().to_string()))?;
        // `cap_std::fs::Dir` opens Windows directory handles without
        // FILE_SHARE_DELETE. Holding this capability for the Workspace
        // lifetime prevents the root from being renamed or deleted beneath
        // descriptor-relative operations. It also gives callers safe
        // component-by-component traversal without in-crate `unsafe`.
        #[cfg(windows)]
        let root_dir = cap_std::fs::Dir::open_ambient_dir(&canonical, cap_std::ambient_authority())
            .map_err(|_| WorkspaceError::Root(root.display().to_string()))?;
        Ok(Self {
            root: canonical,
            #[cfg(unix)]
            root_fd: std::sync::Arc::new(root_fd),
            #[cfg(windows)]
            root_dir: std::sync::Arc::new(root_dir),
            readonly_roots: Vec::new(),
        })
    }

    /// Add directories that may be **read** via absolute paths (or paths under
    /// those roots). Writes still require the primary root. Missing paths are
    /// skipped with no error so config can list optional checkouts.
    pub fn with_readonly_roots(
        mut self,
        roots: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> Self {
        for root in roots {
            let root = root.as_ref();
            if let Ok(canonical) = root.canonicalize()
                && canonical != self.root
                && !self.readonly_roots.iter().any(|r| r == &canonical)
            {
                self.readonly_roots.push(canonical);
            }
        }
        self
    }

    /// The canonical primary workspace root (writable).
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[cfg(unix)]
    pub fn root_fd(&self) -> std::sync::Arc<std::os::fd::OwnedFd> {
        self.root_fd.clone()
    }

    /// Stable Windows capability for descriptor-relative filesystem work.
    ///
    /// The underlying directory handle denies delete sharing, so path-based
    /// rename/delete cannot swap the workspace root while it is in use.
    #[cfg(windows)]
    pub fn root_dir(&self) -> std::sync::Arc<cap_std::fs::Dir> {
        self.root_dir.clone()
    }

    /// Extra read-only roots (canonicalized).
    pub fn readonly_roots(&self) -> &[PathBuf] {
        &self.readonly_roots
    }

    /// Resolve for **write** access: primary root only.
    ///
    /// Prefer this for patches, replaces, and command `cwd`.
    pub fn resolve(&self, input: impl AsRef<Path>) -> Result<PathBuf, WorkspaceError> {
        self.resolve_with(input, PathAccess::Write)
    }

    /// Revalidate a previously resolved write path immediately before mutation.
    ///
    /// Resolution is intentionally repeated at the commit boundary: an
    /// ancestor may have been replaced by a symlink while a tool was preparing
    /// its output.  The returned path must still identify the same location.
    /// This closes ordinary symlink swaps; a hostile process that can race the
    /// final syscall still requires descriptor-relative OS APIs for a complete
    /// guarantee.
    pub fn revalidate_write_path(&self, resolved: &Path) -> Result<(), WorkspaceError> {
        let checked = self.resolve_with(resolved, PathAccess::Write)?;
        if checked != resolved {
            return Err(WorkspaceError::OutsideWorkspace(format!(
                "path `{}` changed identity before write",
                resolved.display()
            )));
        }
        Ok(())
    }

    /// Resolve for **read** access: primary root or any readonly root.
    pub fn resolve_read(&self, input: impl AsRef<Path>) -> Result<PathBuf, WorkspaceError> {
        self.resolve_with(input, PathAccess::Read)
    }

    /// Resolve `input` under the allowed roots for `access`.
    pub fn resolve_with(
        &self,
        input: impl AsRef<Path>,
        access: PathAccess,
    ) -> Result<PathBuf, WorkspaceError> {
        let input = input.as_ref();
        let joined = if input.is_absolute() {
            input.to_path_buf()
        } else {
            // Relative paths always anchor on the primary root (writable tree).
            self.root.join(input)
        };
        let normalized = lexical_normalize(&joined);
        // On macOS, `/var/...` and `/private/var/...` differ lexically but are
        // the same tree after canonicalize — probe with the real ancestor too.
        let ancestor = canonicalize_existing_ancestor(&normalized);

        let in_scope = self.containing_root(&normalized, access).is_some()
            || ancestor
                .as_ref()
                .is_some_and(|a| self.containing_root(a, access).is_some());
        if !in_scope {
            return Err(WorkspaceError::outside(
                input,
                &self.root,
                access,
                &self.readonly_roots,
            ));
        }

        self.check_sensitive(&normalized, input)?;

        // Full path exists: return canonical form and re-check scope (symlink escape).
        if let Ok(real) = std::fs::canonicalize(&normalized) {
            if self.containing_root(&real, access).is_none() {
                return Err(WorkspaceError::outside(
                    input,
                    &self.root,
                    access,
                    &self.readonly_roots,
                ));
            }
            self.check_sensitive(&real, input)?;
            return Ok(real);
        }

        // Path not created yet: ensure the existing ancestor stays in scope.
        if let Some(real_anc) = ancestor
            && self.containing_root(&real_anc, access).is_none()
        {
            return Err(WorkspaceError::outside(
                input,
                &self.root,
                access,
                &self.readonly_roots,
            ));
        }

        Ok(normalized)
    }

    fn containing_root(&self, normalized: &Path, access: PathAccess) -> Option<&Path> {
        if normalized.starts_with(&self.root) {
            return Some(&self.root);
        }
        if access == PathAccess::Read {
            for extra in &self.readonly_roots {
                if normalized.starts_with(extra) {
                    return Some(extra);
                }
            }
        }
        None
    }

    fn check_sensitive(&self, normalized: &Path, original: &Path) -> Result<(), WorkspaceError> {
        let denied = |p: &Path| WorkspaceError::Denied(p.display().to_string());

        for comp in normalized.components() {
            if let Component::Normal(os) = comp {
                let name = os.to_string_lossy();
                if matches!(name.as_ref(), ".git" | ".ssh" | ".aws") {
                    return Err(denied(original));
                }
            }
        }

        if let Some(file) = normalized.file_name().map(|f| f.to_string_lossy()) {
            let is_env = file == ".env" || file.starts_with(".env.");
            let is_key = matches!(
                normalized.extension().and_then(|e| e.to_str()),
                Some("pem") | Some("key")
            );
            if is_env || is_key {
                return Err(denied(original));
            }
        }

        Ok(())
    }
}

/// Normalize `.` and `..` components lexically, without touching the filesystem.
/// A `..` never pops the root (or a leading `..`).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Canonicalize the longest existing prefix of `path`, or `None` if nothing in
/// the chain exists yet.
fn canonicalize_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if let Ok(real) = current.canonicalize() {
            return Some(real);
        }
        current = current.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (Workspace, PathBuf) {
        let dir = std::env::temp_dir().join(format!("leveler-ws-{}", ordinal()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        let ws = Workspace::new(&dir).unwrap();
        (ws, dir)
    }

    fn ordinal() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn resolves_relative_inside_workspace() {
        let (ws, dir) = workspace();
        let p = ws.resolve("src/main.rs").unwrap();
        assert!(p.starts_with(ws.root()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn allows_new_nonexistent_file() {
        let (ws, dir) = workspace();
        assert!(ws.resolve("src/new_module.rs").is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_parent_traversal() {
        let (ws, dir) = workspace();
        let err = ws.resolve("../../etc/passwd").unwrap_err();
        assert!(matches!(err, WorkspaceError::OutsideWorkspace(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("outside the workspace root"),
            "actionable message: {msg}"
        );
        assert!(msg.contains(ws.root().to_string_lossy().as_ref()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_absolute_outside() {
        let (ws, dir) = workspace();
        let err = ws.resolve("/etc/hosts").unwrap_err();
        assert!(matches!(err, WorkspaceError::OutsideWorkspace(_)));
        let msg = err.to_string();
        assert!(msg.contains("outside the workspace root"), "{msg}");
        assert!(
            msg.contains("--readonly-root") || msg.contains("readonly_roots"),
            "should mention how to allow cross-repo reads: {msg}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn denies_env_and_keys_and_git() {
        let (ws, dir) = workspace();
        assert!(matches!(
            ws.resolve(".env").unwrap_err(),
            WorkspaceError::Denied(_)
        ));
        assert!(matches!(
            ws.resolve(".env.local").unwrap_err(),
            WorkspaceError::Denied(_)
        ));
        assert!(matches!(
            ws.resolve("id.pem").unwrap_err(),
            WorkspaceError::Denied(_)
        ));
        assert!(matches!(
            ws.resolve(".git/config").unwrap_err(),
            WorkspaceError::Denied(_)
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn readonly_root_allows_read_but_not_write() {
        let (ws, primary) = workspace();
        let other = std::env::temp_dir().join(format!("leveler-ws-ro-{}", ordinal()));
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("AGENTS.md"), "rules").unwrap();

        let ws = ws.with_readonly_roots([&other]);
        let abs = other.join("AGENTS.md");
        let read = ws.resolve_read(&abs).expect("read via readonly root");
        assert_eq!(read, abs.canonicalize().unwrap_or(abs.clone()));

        let write_err = ws.resolve(&abs).unwrap_err();
        assert!(matches!(write_err, WorkspaceError::OutsideWorkspace(_)));
        assert!(
            write_err.to_string().contains("blocked for writes")
                || write_err.to_string().contains("outside the workspace root"),
            "{}",
            write_err
        );

        std::fs::remove_dir_all(&primary).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn resolve_read_without_readonly_roots_still_rejects_outside() {
        let (ws, dir) = workspace();
        let err = ws.resolve_read("/etc/hosts").unwrap_err();
        assert!(err.to_string().contains("--readonly-root"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn revalidation_rejects_an_ancestor_swapped_to_an_outside_symlink() {
        use std::os::unix::fs::symlink;

        let (ws, dir) = workspace();
        let resolved = ws.resolve("src/new.rs").unwrap();
        let outside = std::env::temp_dir().join(format!("leveler-ws-race-{}", ordinal()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::remove_dir_all(dir.join("src")).unwrap();
        symlink(&outside, dir.join("src")).unwrap();

        assert!(matches!(
            ws.revalidate_write_path(&resolved),
            Err(WorkspaceError::OutsideWorkspace(_))
        ));
        assert!(!outside.join("new.rs").exists());
        std::fs::remove_file(dir.join("src")).ok();
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }
}
