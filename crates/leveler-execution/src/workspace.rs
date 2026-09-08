//! Workspace path resolution and scope validation (spec §21).
//!
//! Reads need no authorization: [`Workspace::resolve_for_read`] canonicalizes
//! any path and applies only the credential-name denylist (`.env`, keys,
//! `.git` internals, `~/.ssh`, ...). Writes are bounded by exactly one
//! [`WriteScope`]: [`Workspace::resolve_for_write`] refuses anything outside
//! it, including `..` escapes and symlinks that point out of it.

use std::path::{Component, Path, PathBuf};

use crate::WriteScope;

/// Whether a path is resolved for reading only or for mutation. Internal:
/// decides which denials apply (trust-gated config is read-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathAccess {
    Read,
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
    fn outside(path: &Path, root: &Path) -> Self {
        WorkspaceError::OutsideWorkspace(format!(
            "path `{}` is outside the workspace root `{}`. Use a path relative \
             to that root (e.g. `src/lib.rs`), or start leveler inside the target \
             repository. Writes outside the write scope are blocked by design; \
             reading any path is allowed.",
            path.display(),
            root.display()
        ))
    }

    fn no_write_scope(path: &Path) -> Self {
        WorkspaceError::OutsideWorkspace(format!(
            "path `{}` cannot be written: no write scope is currently owned. \
             Read the relevant code, then use claim_write_scope(paths) before \
             modifying files.",
            path.display()
        ))
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
    /// Whether this root's filesystem treats `Foo.rs` and `foo.rs` as ONE
    /// file. Probed once when the workspace opens (see
    /// [`detect_case_insensitive`]) because it is a property of the volume,
    /// not of the platform: macOS ships case-insensitive APFS by default but
    /// case-sensitive volumes are supported, and a Linux root can sit on a
    /// case-insensitive mount.
    case_insensitive: bool,
}

/// Probe the root's actual case semantics WITHOUT writing to the user's
/// workspace: take an existing entry whose name has letters, flip its case,
/// and ask the filesystem whether that resolves to the same file.
///
/// Unknown (empty root, no letters in any name, unreadable) answers `true`.
/// That is the conservative direction for the one caller that matters —
/// ownership treats the spellings as the same file and DENIES the second
/// claim, which costs a false denial rather than handing two owners the same
/// bytes.
fn detect_case_insensitive(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return true;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let flipped: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_uppercase() {
                    c.to_ascii_lowercase()
                } else {
                    c.to_ascii_uppercase()
                }
            })
            .collect();
        if flipped == name {
            continue; // no letters to flip — tells us nothing
        }
        let (original, alternate) = (root.join(name), root.join(&flipped));
        return match (
            std::fs::symlink_metadata(&original),
            std::fs::symlink_metadata(&alternate),
        ) {
            // Both spellings resolve: the same file means the volume folds
            // case; two different files mean it does not (a case-sensitive
            // root legitimately holding both `README` and `readme`).
            (Ok(a), Ok(b)) => same_file(&a, &b),
            // The flipped spelling does not exist: case-sensitive.
            (Ok(_), Err(_)) => false,
            _ => true,
        };
    }
    true
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_a: &std::fs::Metadata, _b: &std::fs::Metadata) -> bool {
    // Windows resolves a flipped spelling only when the volume folds case.
    true
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
        let case_insensitive = detect_case_insensitive(&canonical);
        Ok(Self {
            root: canonical,
            #[cfg(unix)]
            root_fd: std::sync::Arc::new(root_fd),
            #[cfg(windows)]
            root_dir: std::sync::Arc::new(root_dir),
            case_insensitive,
        })
    }

    /// Whether this workspace's volume treats two spellings that differ only
    /// in case as the SAME file. Fixed when the workspace opens. Write
    /// ownership uses it so exclusivity follows the file, not the spelling.
    pub fn path_case_insensitive(&self) -> bool {
        self.case_insensitive
    }

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

    /// Resolve for **read**: any path, inside or outside the workspace.
    ///
    /// Relative paths anchor on the workspace root. The result is canonical
    /// when the path exists. Only the credential-name denylist applies —
    /// there is no scope check and nothing here raises an approval.
    pub fn resolve_for_read(&self, input: impl AsRef<Path>) -> Result<PathBuf, WorkspaceError> {
        let input = input.as_ref();
        let normalized = self.normalize(input);
        self.check_sensitive(&normalized, input, PathAccess::Read)?;
        if let Ok(real) = std::fs::canonicalize(&normalized) {
            self.check_sensitive(&real, input, PathAccess::Read)?;
            return Ok(real);
        }
        Ok(normalized)
    }

    /// Resolve for **write** under `scope`.
    ///
    /// - [`WriteScope::None`]: every write is refused.
    /// - [`WriteScope::Workspace`]: the canonical location must sit under the
    ///   scope root — a `..` escape or a symlink pointing out is refused.
    /// - [`WriteScope::Unrestricted`]: any location.
    ///
    /// Credential names and the trust-gated project config are refused under
    /// every scope: a tool that could rewrite `.leveler/hooks.yaml` would be
    /// choosing its own hooks.
    pub fn resolve_for_write(
        &self,
        input: impl AsRef<Path>,
        scope: &WriteScope,
    ) -> Result<PathBuf, WorkspaceError> {
        let input = input.as_ref();
        let bound = match scope {
            WriteScope::None => return Err(WorkspaceError::no_write_scope(input)),
            WriteScope::Workspace { root } => Some(root.as_path()),
            WriteScope::Unrestricted => None,
        };
        self.resolve_bounded(input, bound, PathAccess::Write)
    }

    /// Revalidate a previously resolved write path immediately before mutation.
    ///
    /// Resolution is intentionally repeated at the commit boundary: an
    /// ancestor may have been replaced by a symlink while a tool was preparing
    /// its output. The returned path must still identify the same location.
    /// This closes ordinary symlink swaps; a hostile process that can race the
    /// final syscall still requires descriptor-relative OS APIs for a complete
    /// guarantee.
    pub fn revalidate_write_path(
        &self,
        resolved: &Path,
        scope: &WriteScope,
    ) -> Result<(), WorkspaceError> {
        let checked = self.resolve_for_write(resolved, scope)?;
        if checked != resolved {
            return Err(WorkspaceError::OutsideWorkspace(format!(
                "path `{}` changed identity before write",
                resolved.display()
            )));
        }
        Ok(())
    }

    /// Resolve a command's working directory.
    ///
    /// Not a write, but a confined scope (`Workspace` or `None`) keeps the
    /// cwd inside the workspace root — the OS sandbox anchors on it — while
    /// `Unrestricted` may start anywhere.
    pub fn resolve_command_cwd(
        &self,
        input: impl AsRef<Path>,
        scope: &WriteScope,
    ) -> Result<PathBuf, WorkspaceError> {
        let bound = scope.confines().then_some(self.root.as_path());
        self.resolve_bounded(input.as_ref(), bound, PathAccess::Read)
    }

    fn normalize(&self, input: &Path) -> PathBuf {
        let joined = if input.is_absolute() {
            input.to_path_buf()
        } else {
            // Relative paths always anchor on the primary root.
            self.root.join(input)
        };
        lexical_normalize(&joined)
    }

    /// Resolve `input`, refusing any canonical location outside `bound` when
    /// one is given.
    fn resolve_bounded(
        &self,
        input: &Path,
        bound: Option<&Path>,
        access: PathAccess,
    ) -> Result<PathBuf, WorkspaceError> {
        let normalized = self.normalize(input);
        // On macOS, `/var/...` and `/private/var/...` differ lexically but are
        // the same tree after canonicalize — probe with the real ancestor too.
        let ancestor = canonicalize_existing_ancestor(&normalized);

        if let Some(bound) = bound {
            let in_scope = normalized.starts_with(bound)
                || ancestor.as_ref().is_some_and(|a| a.starts_with(bound));
            if !in_scope {
                return Err(WorkspaceError::outside(input, bound));
            }
        }

        self.check_sensitive(&normalized, input, access)?;

        // Full path exists: return canonical form and re-check scope (symlink escape).
        if let Ok(real) = std::fs::canonicalize(&normalized) {
            if let Some(bound) = bound
                && !real.starts_with(bound)
            {
                return Err(WorkspaceError::outside(input, bound));
            }
            self.check_sensitive(&real, input, access)?;
            return Ok(real);
        }

        // Path not created yet: ensure the existing ancestor stays in scope.
        if let (Some(bound), Some(real_anc)) = (bound, ancestor)
            && !real_anc.starts_with(bound)
        {
            return Err(WorkspaceError::outside(input, bound));
        }

        Ok(normalized)
    }

    fn check_sensitive(
        &self,
        normalized: &Path,
        original: &Path,
        access: PathAccess,
    ) -> Result<(), WorkspaceError> {
        let denied = |p: &Path| WorkspaceError::Denied(p.display().to_string());

        // Writing these would let the agent pick its own hooks and its own
        // standing permissions, defeating the content-keyed trust gate.
        if access == PathAccess::Write && self.is_trust_gated_project_file(normalized) {
            return Err(denied(original));
        }

        for comp in normalized.components() {
            if let Component::Normal(os) = comp {
                let name = os.to_string_lossy();
                if matches!(name.as_ref(), ".git" | ".ssh" | ".aws") {
                    return Err(denied(original));
                }
            }
        }

        if let Some(file) = normalized.file_name().map(|f| f.to_string_lossy())
            && is_sensitive_file_name(&file)
        {
            return Err(denied(original));
        }

        Ok(())
    }

    /// Whether `normalized` is one of this repository's trust-gated config
    /// files. Matched against the primary root, so a same-named file elsewhere
    /// in the tree (`ci/hooks.yaml`) is untouched.
    fn is_trust_gated_project_file(&self, normalized: &Path) -> bool {
        crate::trust::TRUSTED_PROJECT_FILES
            .iter()
            .any(|relative| normalized == self.root.join(relative))
    }
}

/// File names that commonly hold credentials, denied at the workspace layer
/// (`read_file`, `apply_patch`, …) and best-effort refused by the shell guard
/// so both layers enforce one rule set: `.env*`, key/cert material by
/// extension, SSH private keys by conventional name, and well-known credential
/// stores. Public halves (`id_rsa.pub`) stay allowed.
pub fn is_sensitive_file_name(name: &str) -> bool {
    if name == ".env" || name.starts_with(".env.") {
        return true;
    }
    if matches!(
        Path::new(name).extension().and_then(|e| e.to_str()),
        Some("pem") | Some("key")
    ) {
        return true;
    }
    matches!(
        name,
        "credentials.json"
            | ".netrc"
            | "_netrc"
            | ".npmrc"
            | ".pgpass"
            | ".htpasswd"
            | "id_rsa"
            | "id_dsa"
            | "id_ecdsa"
            | "id_ed25519"
    )
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

    fn ws_scope(ws: &Workspace) -> WriteScope {
        WriteScope::Workspace {
            root: ws.root().to_path_buf(),
        }
    }

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
        let p = ws.resolve_for_write("src/main.rs", &ws_scope(&ws)).unwrap();
        assert!(p.starts_with(ws.root()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn allows_new_nonexistent_file() {
        let (ws, dir) = workspace();
        assert!(
            ws.resolve_for_write("src/new_module.rs", &ws_scope(&ws))
                .is_ok()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_parent_traversal() {
        let (ws, dir) = workspace();
        let err = ws
            .resolve_for_write("../../etc/passwd", &ws_scope(&ws))
            .unwrap_err();
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
        let err = ws
            .resolve_for_write("/etc/hosts", &ws_scope(&ws))
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::OutsideWorkspace(_)));
        let msg = err.to_string();
        assert!(msg.contains("outside the workspace root"), "{msg}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn denies_env_and_keys_and_git() {
        let (ws, dir) = workspace();
        assert!(matches!(
            ws.resolve_for_write(".env", &ws_scope(&ws)).unwrap_err(),
            WorkspaceError::Denied(_)
        ));
        assert!(matches!(
            ws.resolve_for_write(".env.local", &ws_scope(&ws))
                .unwrap_err(),
            WorkspaceError::Denied(_)
        ));
        assert!(matches!(
            ws.resolve_for_write("id.pem", &ws_scope(&ws)).unwrap_err(),
            WorkspaceError::Denied(_)
        ));
        assert!(matches!(
            ws.resolve_for_write(".git/config", &ws_scope(&ws))
                .unwrap_err(),
            WorkspaceError::Denied(_)
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn denies_credential_files_beyond_env_and_keys() {
        let (ws, dir) = workspace();
        for name in [
            "credentials.json",
            "id_rsa",
            "id_ed25519",
            ".netrc",
            ".npmrc",
            ".pgpass",
            ".htpasswd",
            "config/credentials.json",
        ] {
            assert!(
                matches!(
                    ws.resolve_for_write(name, &ws_scope(&ws)),
                    Err(WorkspaceError::Denied(_))
                ),
                "{name} must be denied"
            );
        }
        // Public halves and ordinary files stay readable.
        for name in ["id_rsa.pub", "src/main.rs", "package.json"] {
            assert!(
                ws.resolve_for_write(name, &ws_scope(&ws)).is_ok(),
                "{name} must stay allowed"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `.leveler/hooks.yaml` and `.leveler/permissions.yaml` are trust-gated
    /// (see `crate::trust`), and the gate is keyed on their content. If a tool
    /// could rewrite them the agent would be choosing its own hooks and its own
    /// standing permissions — so writes are refused at this layer regardless of
    /// mode. Reads stay allowed: the agent should be able to show the user what
    /// their repository ships.
    #[test]
    fn writes_to_trust_gated_project_config_are_denied() {
        let (ws, dir) = workspace();
        for name in [".leveler/hooks.yaml", ".leveler/permissions.yaml"] {
            assert!(
                matches!(
                    ws.resolve_for_write(name, &ws_scope(&ws)),
                    Err(WorkspaceError::Denied(_))
                ),
                "{name} must not be writable"
            );
            assert!(
                ws.resolve_for_read(name).is_ok(),
                "{name} must stay readable"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn other_leveler_files_and_same_named_files_elsewhere_stay_writable() {
        let (ws, dir) = workspace();
        for name in [
            ".leveler/config.yaml",
            ".leveler/skills/pack/SKILL.md",
            // Only the two files under `.leveler/` are gated — a project's own
            // `hooks.yaml` elsewhere in the tree is an ordinary file.
            "ci/hooks.yaml",
            "config/permissions.yaml",
        ] {
            assert!(
                ws.resolve_for_write(name, &ws_scope(&ws)).is_ok(),
                "{name} must stay writable"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A sibling checkout is readable with no readonly root at all, and the
    /// workspace scope still refuses to write it.
    #[test]
    fn another_checkout_is_readable_but_not_writable_under_workspace_scope() {
        let (ws, primary) = workspace();
        let other = std::env::temp_dir().join(format!("leveler-ws-ro-{}", ordinal()));
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("AGENTS.md"), "rules").unwrap();

        let abs = other.join("AGENTS.md");
        let read = ws.resolve_for_read(&abs).expect("reads are not scoped");
        assert_eq!(read, abs.canonicalize().unwrap_or(abs.clone()));

        let write_err = ws.resolve_for_write(&abs, &ws_scope(&ws)).unwrap_err();
        assert!(matches!(write_err, WorkspaceError::OutsideWorkspace(_)));

        std::fs::remove_dir_all(&primary).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    #[cfg(unix)]
    #[test]
    fn revalidation_rejects_an_ancestor_swapped_to_an_outside_symlink() {
        use std::os::unix::fs::symlink;

        let (ws, dir) = workspace();
        let resolved = ws.resolve_for_write("src/new.rs", &ws_scope(&ws)).unwrap();
        let outside = std::env::temp_dir().join(format!("leveler-ws-race-{}", ordinal()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::remove_dir_all(dir.join("src")).unwrap();
        symlink(&outside, dir.join("src")).unwrap();

        assert!(matches!(
            ws.revalidate_write_path(&resolved, &ws_scope(&ws)),
            Err(WorkspaceError::OutsideWorkspace(_))
        ));
        assert!(!outside.join("new.rs").exists());
        std::fs::remove_file(dir.join("src")).ok();
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// The probe must report what the VOLUME actually does, not what the
    /// platform usually does — ownership exclusivity is decided by it, and a
    /// wrong answer either hands two owners one file or falsely denies a
    /// legitimate claim. Checked against the filesystem's own behaviour so the
    /// test is right on a case-sensitive macOS volume too.
    #[test]
    fn case_semantics_match_what_the_filesystem_actually_does() {
        let dir = std::env::temp_dir().join(format!("leveler-ws-case-{}", ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Probe.txt"), "x").unwrap();
        let truth = std::fs::metadata(dir.join("probe.txt")).is_ok();
        let ws = Workspace::new(&dir).unwrap();
        assert_eq!(
            ws.path_case_insensitive(),
            truth,
            "probe disagreed with the filesystem at {}",
            dir.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A root holding BOTH spellings as distinct files is the case a naive
    /// "flipped name resolves" probe gets backwards: the alternate spelling
    /// does resolve, but to a different inode.
    #[test]
    fn two_distinct_spellings_are_not_read_as_case_folding() {
        let dir = std::env::temp_dir().join(format!("leveler-ws-case2-{}", ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Probe.txt"), "upper").unwrap();
        let distinct = std::fs::write(dir.join("probe.txt"), "lower").is_ok()
            && std::fs::read_to_string(dir.join("Probe.txt")).unwrap() == "upper";
        let ws = Workspace::new(&dir).unwrap();
        if distinct {
            assert!(
                !ws.path_case_insensitive(),
                "two distinct files with the same folded name means the volume is case-sensitive"
            );
        } else {
            assert!(ws.path_case_insensitive());
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// PR 2: reads need no authorization; writes are bounded by one `WriteScope`.
#[cfg(test)]
mod scope_split_tests {
    use super::*;
    use crate::WriteScope;

    fn workspace() -> (Workspace, PathBuf) {
        let dir = std::env::temp_dir().join(format!("leveler-ws2-{}", ordinal()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        (Workspace::new(&dir).unwrap(), dir)
    }

    fn ordinal() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(1000);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn outside_dir() -> PathBuf {
        let other = std::env::temp_dir().join(format!("leveler-ws2-out-{}", ordinal()));
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("notes.md"), "elsewhere").unwrap();
        other
    }

    fn workspace_scope(ws: &Workspace) -> WriteScope {
        WriteScope::Workspace {
            root: ws.root().to_path_buf(),
        }
    }

    // ---- reads ----

    /// Any path is readable without a readonly root or approval.
    #[test]
    fn a_read_outside_the_workspace_resolves_without_any_gate() {
        let (ws, dir) = workspace();
        let other = outside_dir();
        let abs = other.join("notes.md");
        let read = ws
            .resolve_for_read(&abs)
            .expect("outside read is not gated");
        assert_eq!(read, abs.canonicalize().unwrap());
        assert!(ws.resolve_for_read("/etc/hosts").is_ok() || !Path::new("/etc/hosts").exists());
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn a_relative_read_anchors_on_the_workspace_root() {
        let (ws, dir) = workspace();
        let p = ws.resolve_for_read("src/main.rs").unwrap();
        assert!(p.starts_with(ws.root()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Scope is gone from reads; the credential denylist is not. Whether
    /// `.env` / private keys become readable is a product decision the
    /// migration plan leaves to documentation, so this pins today's answer.
    #[test]
    fn sensitive_names_stay_denied_on_read_everywhere() {
        let (ws, dir) = workspace();
        let other = outside_dir();
        std::fs::write(other.join(".env"), "SECRET=1").unwrap();
        for p in [
            ws.root().join(".env"),
            other.join(".env"),
            other.join("id_rsa"),
            PathBuf::from("/tmp/anything/.ssh/config"),
        ] {
            assert!(
                matches!(ws.resolve_for_read(&p), Err(WorkspaceError::Denied(_))),
                "{} must stay denied",
                p.display()
            );
        }
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    /// The trust-gated config files stay readable — the agent may show the
    /// user what the repository ships; it may not rewrite it.
    #[test]
    fn trust_gated_config_is_readable_but_not_writable_under_any_scope() {
        let (ws, dir) = workspace();
        for name in [".leveler/hooks.yaml", ".leveler/permissions.yaml"] {
            assert!(ws.resolve_for_read(name).is_ok(), "{name} readable");
            for scope in [workspace_scope(&ws), WriteScope::Unrestricted] {
                assert!(
                    matches!(
                        ws.resolve_for_write(name, &scope),
                        Err(WorkspaceError::Denied(_))
                    ),
                    "{name} must not be writable under {scope:?}"
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- writes ----

    #[test]
    fn workspace_scope_allows_writes_inside_and_refuses_outside() {
        let (ws, dir) = workspace();
        let other = outside_dir();
        let scope = workspace_scope(&ws);
        assert!(ws.resolve_for_write("src/new.rs", &scope).is_ok());
        let err = ws
            .resolve_for_write(other.join("notes.md"), &scope)
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::OutsideWorkspace(_)), "{err}");
        assert!(
            !err.to_string().contains("--readonly-root"),
            "readonly roots are a read concept and reads are no longer gated: {err}"
        );
        assert!(matches!(
            ws.resolve_for_write("../../etc/passwd", &scope),
            Err(WorkspaceError::OutsideWorkspace(_))
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    /// `None` is the pre-claim child: nothing is writable, not even inside.
    #[test]
    fn none_scope_refuses_every_write() {
        let (ws, dir) = workspace();
        let err = ws
            .resolve_for_write("src/main.rs", &WriteScope::None)
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::OutsideWorkspace(_)), "{err}");
        assert!(
            err.to_string().contains("claim_write_scope")
                || err.to_string().contains("no write scope"),
            "must tell the child how to obtain a scope: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unrestricted_scope_allows_writes_outside_the_workspace() {
        let (ws, dir) = workspace();
        let other = outside_dir();
        let p = ws
            .resolve_for_write(other.join("notes.md"), &WriteScope::Unrestricted)
            .expect("unrestricted may write anywhere");
        assert_eq!(p, other.join("notes.md").canonicalize().unwrap());
        // ...but not credentials.
        assert!(matches!(
            ws.resolve_for_write(other.join(".env"), &WriteScope::Unrestricted),
            Err(WorkspaceError::Denied(_))
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    /// A symlink inside the workspace that points outside must not become a
    /// write path under the workspace scope — canonical location decides.
    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_pointing_outside_is_not_writable_under_workspace_scope() {
        use std::os::unix::fs::symlink;
        let (ws, dir) = workspace();
        let other = outside_dir();
        symlink(&other, dir.join("linked")).unwrap();
        let scope = workspace_scope(&ws);
        assert!(matches!(
            ws.resolve_for_write("linked/notes.md", &scope),
            Err(WorkspaceError::OutsideWorkspace(_))
        ));
        assert!(matches!(
            ws.resolve_for_write("linked/new.md", &scope),
            Err(WorkspaceError::OutsideWorkspace(_))
        ));
        // Reading through it is fine.
        assert!(ws.resolve_for_read("linked/notes.md").is_ok());
        std::fs::remove_file(dir.join("linked")).ok();
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    /// Revalidation carries the scope too: an ancestor swapped to an outside
    /// symlink between resolve and write is caught under the workspace scope.
    #[cfg(unix)]
    #[test]
    fn revalidation_under_workspace_scope_catches_an_ancestor_swap() {
        use std::os::unix::fs::symlink;
        let (ws, dir) = workspace();
        let scope = workspace_scope(&ws);
        let resolved = ws.resolve_for_write("src/new.rs", &scope).unwrap();
        let outside = outside_dir();
        std::fs::remove_dir_all(dir.join("src")).unwrap();
        symlink(&outside, dir.join("src")).unwrap();
        assert!(matches!(
            ws.revalidate_write_path(&resolved, &scope),
            Err(WorkspaceError::OutsideWorkspace(_))
        ));
        std::fs::remove_file(dir.join("src")).ok();
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    // ---- command cwd ----

    /// A command's cwd is not a write, but a confined scope still keeps it
    /// inside the workspace (today's behavior); unrestricted may cd anywhere.
    #[test]
    fn command_cwd_follows_confinement_not_writability() {
        let (ws, dir) = workspace();
        let other = outside_dir();
        for scope in [workspace_scope(&ws), WriteScope::None] {
            assert!(ws.resolve_command_cwd("src", &scope).is_ok(), "{scope:?}");
            assert!(
                matches!(
                    ws.resolve_command_cwd(&other, &scope),
                    Err(WorkspaceError::OutsideWorkspace(_))
                ),
                "{scope:?}"
            );
        }
        assert!(
            ws.resolve_command_cwd(&other, &WriteScope::Unrestricted)
                .is_ok()
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }
}
