//! Workspace path resolution and scope validation (spec §21).
//!
//! Reads need no authorization: [`Workspace::resolve_for_read`] canonicalizes
//! any path and applies only the credential-name denylist (`.env`, keys,
//! `.git` internals, `~/.ssh`, ...). Writes are bounded by exactly one
//! [`WriteScope`]: [`Workspace::resolve_for_write`] refuses anything outside
//! it, including `..` escapes and symlinks that point out of it.
//! A per-call unrestricted execution view bypasses these permission fences;
//! filesystem scope alone does not grant that authority.

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
    /// An agent definition, which only the agent store writes.
    #[error(
        "path `{0}` is an agent definition; tools do not edit it. Change agents \
         with save_agent or delete_agent, which validate the change and ask the \
         user to confirm it"
    )]
    AgentDefinition(String),
    /// A skill definition, which only the skill store writes.
    #[error(
        "path `{0}` is a skill definition; tools do not edit it. Change skills \
         with save_skill or delete_skill, which validate the change and ask the \
         user to confirm it"
    )]
    SkillDefinition(String),
    #[error("failed to canonicalize workspace root {0}")]
    Root(String),
    /// The file could not be opened for reading. `kind` is preserved so a
    /// model-facing missing-file case stays recoverable.
    #[error("failed to open `{path}` for reading: {message}")]
    ReadOpen {
        path: String,
        message: String,
        kind: std::io::ErrorKind,
    },
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

    fn review_confined(path: &Path, root: &Path) -> Self {
        WorkspaceError::OutsideWorkspace(format!(
            "path `{}` is outside the reviewed workspace `{}`. A review seat may \
             read the review object and its bound evidence tools only; it cannot \
             read arbitrary host files.",
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
    /// Capability handle for the root, on every platform.
    ///
    /// Confined reads open through this handle instead of re-opening a
    /// resolved path. `cap_std::fs::Dir` resolves component by component and
    /// refuses a symlink that leaves the root, so a path swapped between the
    /// canonicalize check and the open cannot redirect the read. On Windows it
    /// also denies delete sharing, so the root cannot be renamed beneath it.
    root_dir: std::sync::Arc<cap_std::fs::Dir>,
    /// Extra trees allowed for [`PathAccess::Read`] only (canonicalized).
    /// Whether this root's filesystem treats `Foo.rs` and `foo.rs` as ONE
    /// file. Probed once when the workspace opens (see
    /// [`detect_case_insensitive`]) because it is a property of the volume,
    /// not of the platform: macOS ships case-insensitive APFS by default but
    /// case-sensitive volumes are supported, and a Linux root can sit on a
    /// case-insensitive mount.
    case_insensitive: bool,
    unrestricted_access: bool,
    resource_bindings: Vec<leveler_core::GrantBinding>,
    /// A review/investigation seat may read only the review workspace. The
    /// ordinary agent keeps the product's "absolute reads are allowed" rule;
    /// a review must not turn read-only into a licence for arbitrary host
    /// files.
    read_confinement: bool,
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
        let root_dir = cap_std::fs::Dir::open_ambient_dir(&canonical, cap_std::ambient_authority())
            .map_err(|_| WorkspaceError::Root(root.display().to_string()))?;
        let case_insensitive = detect_case_insensitive(&canonical);
        Ok(Self {
            root: canonical,
            #[cfg(unix)]
            root_fd: std::sync::Arc::new(root_fd),
            root_dir: std::sync::Arc::new(root_dir),
            case_insensitive,
            unrestricted_access: false,
            resource_bindings: Vec::new(),
            read_confinement: false,
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

    /// Bind the host's execution authority to this per-call workspace view.
    /// A filesystem scope alone does not imply unrestricted execution authority.
    pub fn with_unrestricted_access(mut self, unrestricted: bool) -> Self {
        self.unrestricted_access = unrestricted;
        self
    }

    /// Keep read resolution inside this workspace. Set only on a review or
    /// investigation seat; the ordinary agent's absolute-read contract is
    /// unchanged.
    pub fn with_read_confinement(mut self, confined: bool) -> Self {
        self.read_confinement = confined;
        self
    }

    /// Exact bindings attached to this call by admission, never a tree grant.
    pub fn with_resource_bindings(mut self, bindings: Vec<leveler_core::GrantBinding>) -> Self {
        self.resource_bindings = bindings;
        self
    }

    /// Re-resolve the object before using the exact frozen resource permission.
    pub fn allows_resource_path(&self, capability: leveler_core::Capability, path: &Path) -> bool {
        use leveler_core::{Capability, ResourceIdentity};
        let path = self.normalize(path);
        let Ok(resource) = crate::resource_identity::resolve_filesystem_resource(&path) else {
            return false;
        };
        let contains = |capability| {
            self.resource_bindings
                .iter()
                .any(|binding| binding.capability == capability && binding.resource == resource)
        };
        if !contains(capability) {
            return false;
        }
        let canonical_sensitive = match &resource {
            ResourceIdentity::FilesystemPath { canonical_path, .. } => {
                is_credential_path(Path::new(canonical_path))
            }
            _ => false,
        };
        if is_credential_path(&path) || canonical_sensitive {
            match capability {
                Capability::FilesystemRead => contains(Capability::CredentialRawRead),
                Capability::FilesystemWrite | Capability::FilesystemDelete => {
                    contains(Capability::CredentialRawWrite)
                        && (!path.exists()
                            || (contains(Capability::FilesystemRead)
                                && contains(Capability::CredentialRawRead)))
                }
                _ => true,
            }
        } else {
            true
        }
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

    /// Open a file for reading, with the confinement decision bound to the
    /// object actually opened.
    ///
    /// Ordinary workspaces keep the path-based contract (`resolve_for_read`
    /// then open). A confined review/investigation seat opens through the
    /// workspace's capability handle: cap-std resolves each component and
    /// refuses any symlink that leaves the root, so the canonicalize check and
    /// the open cannot disagree because a path was swapped in between. The
    /// returned path is the captured identity used for fingerprints; the file
    /// handle is the object actually read.
    pub fn open_read(
        &self,
        input: impl AsRef<Path>,
    ) -> Result<(PathBuf, std::fs::File), WorkspaceError> {
        let input = input.as_ref();
        let resolved = self.resolve_for_read(input)?;
        // Windows reports opening a directory as `PermissionDenied`; Unix may
        // report `IsADirectory` from the open itself. Classify the resolved
        // entry first so every platform gives read tools the same answer, and
        // a confined seat does not misreport the directory as a confinement
        // denial.
        if resolved.is_dir() {
            return Err(WorkspaceError::ReadOpen {
                path: input.display().to_string(),
                message: "path is a directory".to_string(),
                kind: std::io::ErrorKind::IsADirectory,
            });
        }
        if !self.read_confinement {
            let file =
                std::fs::File::open(&resolved).map_err(|error| WorkspaceError::ReadOpen {
                    path: input.display().to_string(),
                    message: error.to_string(),
                    kind: error.kind(),
                })?;
            return Ok((resolved, file));
        }
        let relative = self.confined_relative(&resolved, input)?;
        let file = self.root_dir.open(relative).map_err(|error| {
            // cap-std refuses a component that resolves outside the root. That
            // is a confinement denial, not an I/O failure, and it is exactly
            // the race this entry point closes.
            match error.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    WorkspaceError::review_confined(input, &self.root)
                }
                kind => WorkspaceError::ReadOpen {
                    path: input.display().to_string(),
                    message: error.to_string(),
                    kind,
                },
            }
        })?;
        Ok((resolved, file.into_std()))
    }

    /// Whether this view confines reads to the reviewed workspace.
    pub fn reads_confined(&self) -> bool {
        self.read_confinement
    }

    /// Open one walked path through the workspace capability.
    ///
    /// Used by the search tools to turn a path produced by a directory walk
    /// into an object that is provably under the root. `None` means the path
    /// is outside the workspace or could not be opened; a search must skip it
    /// rather than surface it to the model.
    pub fn open_confined(&self, path: &Path) -> Option<std::fs::File> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let relative = if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        };
        self.root_dir
            .open(relative)
            .ok()
            .map(cap_std::fs::File::into_std)
    }

    /// The workspace-relative form of an already-resolved path.
    ///
    /// The root itself is `.` for the capability handle: an empty path is not
    /// a valid component, and `list_files` defaults to the root.
    fn confined_relative<'a>(
        &self,
        resolved: &'a Path,
        input: &Path,
    ) -> Result<&'a Path, WorkspaceError> {
        let relative = resolved
            .strip_prefix(&self.root)
            .map_err(|_| WorkspaceError::review_confined(input, &self.root))?;
        Ok(if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        })
    }

    /// List one directory through the workspace capability.
    ///
    /// The confined variant never enumerates outside the root, even if a
    /// directory entry is swapped for an escaping symlink between resolution
    /// and the read. Returns `(name, is_dir)`; `is_dir` follows an
    /// inside-root symlink and is false for one that escapes.
    pub fn read_dir_confined(
        &self,
        input: impl AsRef<Path>,
    ) -> Result<Vec<(String, bool)>, WorkspaceError> {
        let input = input.as_ref();
        let resolved = self.resolve_for_read(input)?;
        let relative = self.confined_relative(&resolved, input)?;
        let dir = self
            .root_dir
            .open_dir(relative)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    WorkspaceError::review_confined(input, &self.root)
                }
                kind => WorkspaceError::ReadOpen {
                    path: input.display().to_string(),
                    message: error.to_string(),
                    kind,
                },
            })?;
        let mut entries = Vec::new();
        for entry in dir.entries().map_err(|error| WorkspaceError::ReadOpen {
            path: input.display().to_string(),
            message: error.to_string(),
            kind: error.kind(),
        })? {
            let entry = entry.map_err(|error| WorkspaceError::ReadOpen {
                path: input.display().to_string(),
                message: error.to_string(),
                kind: error.kind(),
            })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = dir.open_dir(&name).is_ok();
            entries.push((name, is_dir));
        }
        Ok(entries)
    }

    /// Resolve for **read**: any path, inside or outside the workspace.
    ///
    /// Relative paths anchor on the workspace root. The result is canonical
    /// when the path exists. Only the credential-name denylist applies —
    /// there is no scope check and nothing here raises an approval.
    pub fn resolve_for_read(&self, input: impl AsRef<Path>) -> Result<PathBuf, WorkspaceError> {
        let input = input.as_ref();
        let normalized = self.normalize(input);
        if self.read_confinement {
            // Check both the lexical form and the real path: a symlink inside
            // the workspace can point outside it, and canonicalize is the only
            // reading that sees that.
            let real = std::fs::canonicalize(&normalized).unwrap_or_else(|_| normalized.clone());
            if !real.starts_with(&self.root) {
                return Err(WorkspaceError::review_confined(input, &self.root));
            }
        }
        if self.allows_resource_path(leveler_core::Capability::FilesystemRead, input) {
            return self.clone().with_unrestricted_access(true).resolve_bounded(
                input,
                None,
                PathAccess::Read,
            );
        }
        self.check_sensitive(&normalized, input, PathAccess::Read)?;
        if let Ok(real) = std::fs::canonicalize(&normalized) {
            if self.read_confinement && !real.starts_with(&self.root) {
                return Err(WorkspaceError::review_confined(input, &self.root));
            }
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
        self.resolve_for_mutation(
            input.as_ref(),
            scope,
            leveler_core::Capability::FilesystemWrite,
        )
    }

    /// Resolve a deletion under its independent capability.
    pub fn resolve_for_delete(
        &self,
        input: impl AsRef<Path>,
        scope: &WriteScope,
    ) -> Result<PathBuf, WorkspaceError> {
        self.resolve_for_mutation(
            input.as_ref(),
            scope,
            leveler_core::Capability::FilesystemDelete,
        )
    }

    fn resolve_for_mutation(
        &self,
        input: &Path,
        scope: &WriteScope,
        capability: leveler_core::Capability,
    ) -> Result<PathBuf, WorkspaceError> {
        if self.unrestricted_access || self.allows_resource_path(capability, input) {
            return self.clone().with_unrestricted_access(true).resolve_bounded(
                input,
                None,
                PathAccess::Write,
            );
        }
        let bound = match scope {
            WriteScope::None => return Err(WorkspaceError::no_write_scope(input)),
            WriteScope::Workspace { root } | WriteScope::WorkspaceWithGit { root } => {
                Some(root.as_path())
            }
            WriteScope::Unrestricted => None,
            WriteScope::ScopedWorkspace {
                root,
                allowed,
                excluded,
            } => {
                let path = self.resolve_bounded(input, Some(root), PathAccess::Write)?;
                if !allowed.iter().any(|p| path.starts_with(p))
                    || excluded.iter().any(|p| path.starts_with(p))
                {
                    return Err(WorkspaceError::no_write_scope(&path));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if std::fs::metadata(&path)
                        .is_ok_and(|metadata| metadata.is_file() && metadata.nlink() > 1)
                    {
                        crate::command::validate_scoped_hardlinks(root, allowed, excluded)
                            .map_err(|error| WorkspaceError::OutsideWorkspace(error.to_string()))?;
                    }
                }
                return Ok(path);
            }
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

    /// Revalidate the exact deletion resource at the mutation boundary.
    pub fn revalidate_delete_path(
        &self,
        resolved: &Path,
        scope: &WriteScope,
    ) -> Result<(), WorkspaceError> {
        let checked = self.resolve_for_delete(resolved, scope)?;
        if checked != resolved {
            return Err(WorkspaceError::outside(resolved, self.root()));
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
        let bound = (scope.confines() && !self.unrestricted_access).then_some(self.root.as_path());
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
        if self.unrestricted_access {
            let mut ancestor = normalized.as_path();
            loop {
                if let Ok(real) = ancestor.canonicalize() {
                    let suffix = normalized.strip_prefix(ancestor).unwrap();
                    return Ok(if suffix.as_os_str().is_empty() {
                        real
                    } else {
                        real.join(suffix)
                    });
                }
                let Some(parent) = ancestor.parent() else {
                    return Ok(normalized);
                };
                ancestor = parent;
            }
        }
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
        if self.unrestricted_access {
            return Ok(());
        }
        let denied = |p: &Path| WorkspaceError::Denied(p.display().to_string());

        // Writing these would let the agent pick its own hooks and its own
        // standing permissions, defeating the content-keyed trust gate.
        if access == PathAccess::Write && self.is_trust_gated_project_file(normalized) {
            return Err(denied(original));
        }
        // Agent definitions change what future children may do. They are
        // written through the agent store (validated, confirmed by a person,
        // atomic), so no tool patches them in place.
        if access == PathAccess::Write && self.is_in_project_agents_dir(normalized) {
            return Err(WorkspaceError::AgentDefinition(
                original.display().to_string(),
            ));
        }
        // Skill definitions are written through the skill store (validated,
        // confirmed by a person, atomic), so no tool patches them in place.
        if access == PathAccess::Write && self.is_in_project_skills_dir(normalized) {
            return Err(WorkspaceError::SkillDefinition(
                original.display().to_string(),
            ));
        }

        for comp in normalized.components() {
            if let Component::Normal(os) = comp {
                let name = os.to_string_lossy();
                // The volume may resolve any casing to the same directory.
                if name.eq_ignore_ascii_case(".git")
                    || name.eq_ignore_ascii_case(".ssh")
                    || name.eq_ignore_ascii_case(".aws")
                {
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

    /// Whether `normalized` is inside `<root>/.leveler/agents`. The two names
    /// compare ASCII-case-insensitively: on macOS and Windows `.LEVELER/Agents`
    /// is the directory the registry reads.
    fn is_in_project_agents_dir(&self, normalized: &Path) -> bool {
        self.is_in_leveler_dir(normalized, "agents")
    }

    /// Whether `normalized` is inside `<root>/.leveler/skills`.
    fn is_in_project_skills_dir(&self, normalized: &Path) -> bool {
        self.is_in_leveler_dir(normalized, "skills")
    }

    fn is_in_leveler_dir(&self, normalized: &Path, child: &str) -> bool {
        let Ok(rest) = normalized.strip_prefix(&self.root) else {
            return false;
        };
        let mut parts = rest.components();
        matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(dir)), Some(Component::Normal(name)))
                if dir.eq_ignore_ascii_case(".leveler") && name.eq_ignore_ascii_case(child)
        )
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
    // macOS and Windows resolve `.ENV` to the same file as `.env`, and a
    // credential's actual identity is the file, not the spelling the tool was
    // given. Compare ASCII-insensitively on every platform: on a
    // case-sensitive volume this can over-deny a differently-cased ordinary
    // file, which is the safe direction.
    let lower = name.to_ascii_lowercase();
    if lower == ".env" || lower.starts_with(".env.") {
        return true;
    }
    if matches!(
        Path::new(&lower).extension().and_then(|e| e.to_str()),
        Some("pem") | Some("key")
    ) {
        return true;
    }
    matches!(
        lower.as_str(),
        "credentials.json"
            | ".git-credentials"
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

/// Classify raw credential files using the workspace's existing denylist.
/// This is metadata classification, never credential discovery or loading.
pub fn is_credential_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(component, Component::Normal(name) if name.to_string_lossy().eq_ignore_ascii_case(".ssh") || name.to_string_lossy().eq_ignore_ascii_case(".aws"))
    }) || path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_sensitive_file_name)
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

    #[test]
    fn full_authority_allows_credentials_external_paths_and_trusted_config() {
        let (ws, dir) = workspace();
        let outside = tempfile::tempdir().unwrap();
        let credential = outside.path().join(".env");
        std::fs::write(&credential, "private fixture").unwrap();
        assert!(ws.resolve_for_read(&credential).is_err());
        assert!(
            ws.resolve_for_write(&credential, &WriteScope::Unrestricted)
                .is_err()
        );
        let full = ws.clone().with_unrestricted_access(true);
        assert_eq!(
            full.resolve_for_read(&credential).unwrap(),
            credential.canonicalize().unwrap()
        );
        assert_eq!(
            full.resolve_for_write(&credential, &WriteScope::None)
                .unwrap(),
            credential.canonicalize().unwrap()
        );
        for name in [
            ".env",
            ".git/config",
            ".leveler/hooks.yaml",
            ".leveler/agents/a.md",
        ] {
            assert!(
                full.resolve_for_write(name, &WriteScope::None).is_ok(),
                "{name}"
            );
        }
        assert!(
            full.resolve_command_cwd(outside.path(), &WriteScope::None)
                .is_ok()
        );
        assert!(
            ws.resolve_for_write(".env", &WriteScope::Unrestricted)
                .is_err()
        );
        // The open workspace root is what owns the directory on Windows, so a
        // teardown there is best-effort for the same reason its siblings are.
        std::fs::remove_dir_all(dir).ok();
    }

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
        // A case-insensitive volume resolves these to the same files.
        for path in [".ENV", ".Env.Local", "ID_RSA", ".GIT/config", ".SSH/id"] {
            assert!(
                matches!(
                    ws.resolve_for_write(path, &ws_scope(&ws)).unwrap_err(),
                    WorkspaceError::Denied(_) | WorkspaceError::OutsideWorkspace(_)
                ),
                "{path} must not be writable"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn credential_classification_ignores_case() {
        for name in [
            ".env",
            ".ENV",
            ".Env.local",
            "server.PEM",
            "server.KEY",
            "ID_RSA",
            "Credentials.JSON",
        ] {
            assert!(is_sensitive_file_name(name), "{name}");
        }
        for name in ["environment.md", "keynote.md", "id_rsa.pub", "README.md"] {
            assert!(!is_sensitive_file_name(name), "{name}");
        }
        assert!(is_credential_path(Path::new("/home/u/.SSH/config")));
        assert!(is_credential_path(Path::new("/tmp/.ENV")));
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
            ".git-credentials",
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

    /// Agent definitions are written through the agent store — validated,
    /// confirmed by a person, atomic — never patched in place by a tool: an
    /// edit tool could otherwise turn a read-only agent into a writer without
    /// anyone seeing it. Reads stay allowed so the agent can show them.
    #[test]
    fn writes_to_project_agent_definitions_are_denied() {
        let (ws, dir) = workspace();
        for name in [
            ".leveler/agents/security-reviewer/agent.yaml",
            ".leveler/agents/security-reviewer/instructions.md",
            ".leveler/agents/new-agent/agent.yaml",
            ".leveler/agents",
        ] {
            match ws.resolve_for_write(name, &ws_scope(&ws)) {
                // The refusal says where to go instead: a model told only
                // "sensitive file" guesses at its next move.
                Err(error @ WorkspaceError::AgentDefinition(_)) => {
                    assert!(error.to_string().contains("save_agent"), "{error}");
                }
                other => panic!("{name} must not be writable: {other:?}"),
            }
            assert!(
                ws.resolve_for_read(name).is_ok(),
                "{name} must stay readable"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The same refusal for every spelling that lands in the agents directory:
    /// nested files, `..` detours, and case variants — macOS and Windows
    /// resolve `.LEVELER/Agents` to the very directory the registry reads.
    #[test]
    fn agent_definition_refusal_covers_nested_detoured_and_case_variant_paths() {
        let (ws, dir) = workspace();
        std::fs::create_dir_all(dir.join(".leveler/agents/existing")).unwrap();
        std::fs::write(dir.join(".leveler/agents/existing/agent.yaml"), "x").unwrap();
        for name in [
            ".leveler/agents/existing/notes/deep.md",
            "src/../.leveler/agents/new-agent/agent.yaml",
            ".LEVELER/agents/new-agent/agent.yaml",
            ".leveler/Agents/existing/agent.yaml",
            ".Leveler/AGENTS/existing/instructions.md",
        ] {
            assert!(
                matches!(
                    ws.resolve_for_write(name, &ws_scope(&ws)),
                    Err(WorkspaceError::AgentDefinition(_))
                ),
                "{name} must not be writable"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Skill definitions are written through the skill store — validated,
    /// confirmed by a person, atomic — never patched in place by a tool. Reads
    /// stay allowed so the agent can show them.
    #[test]
    fn writes_to_project_skill_definitions_are_denied() {
        let (ws, dir) = workspace();
        std::fs::create_dir_all(dir.join(".leveler/skills/existing")).unwrap();
        std::fs::write(dir.join(".leveler/skills/existing/SKILL.md"), "x").unwrap();
        for name in [
            ".leveler/skills/existing/SKILL.md",
            ".leveler/skills/new/references/note.md",
            "src/../.leveler/skills/new/SKILL.md",
            ".LEVELER/skills/new/SKILL.md",
            ".leveler/Skills/new/SKILL.md",
            ".leveler/skills",
        ] {
            match ws.resolve_for_write(name, &ws_scope(&ws)) {
                Err(error @ WorkspaceError::SkillDefinition(_)) => {
                    assert!(error.to_string().contains("save_skill"), "{error}");
                }
                other => panic!("{name} must not be writable: {other:?}"),
            }
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
            // Only the trust-gated files and the agents/skills stores under
            // `.leveler/` are guarded — a same-named file elsewhere in the
            // tree is an ordinary file.
            "ci/hooks.yaml",
            "config/permissions.yaml",
            // Only the project's own agents directory is guarded.
            "docs/agents/security-reviewer/agent.yaml",
            "docs/skills/security-reviewer/SKILL.md",
            ".leveler/agents-notes.md",
            ".leveler/skills-notes.md",
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
    #[cfg(unix)]
    #[test]
    fn scoped_file_tools_reject_a_hardlinked_write_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("owned")).unwrap();
        std::fs::write(dir.path().join("foreign"), "original").unwrap();
        std::fs::hard_link(dir.path().join("foreign"), dir.path().join("owned/alias")).unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let root = ws.root().to_path_buf();
        let scope = WriteScope::ScopedWorkspace {
            allowed: vec![root.join("owned")],
            root,
            excluded: Vec::new(),
        };
        assert!(ws.resolve_for_write("owned/alias", &scope).is_err());
    }
}

/// Filesystem object binding exists on unix only: the incarnation proof the
/// grant is built on has no Windows implementation, so those grants are simply
/// never offered there.
#[cfg(all(test, unix))]
mod resource_path_tests {
    use super::*;
    use leveler_core::{Capability, GrantBinding};
    #[test]
    fn exact_external_resource_never_grants_neighbor_or_replacement() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let target = external.path().join("approved.txt");
        let neighbor = external.path().join("other.txt");
        std::fs::write(&target, "original").unwrap();
        std::fs::write(&neighbor, "other").unwrap();
        let resource = crate::resource_identity::resolve_filesystem_resource(&target).unwrap();
        let workspace = Workspace::new(root.path())
            .unwrap()
            .with_resource_bindings(vec![GrantBinding {
                capability: Capability::FilesystemWrite,
                resource,
            }]);
        assert!(
            workspace
                .resolve_for_write(&target, &WriteScope::None)
                .is_ok()
        );
        assert!(
            workspace
                .resolve_for_write(&neighbor, &WriteScope::None)
                .is_err()
        );
        std::fs::rename(&target, external.path().join("old.txt")).unwrap();
        std::fs::write(&target, "replacement").unwrap();
        assert!(
            workspace
                .resolve_for_write(&target, &WriteScope::None)
                .is_err()
        );
    }
    #[test]
    fn ordinary_file_grant_and_credential_use_do_not_authorize_raw_secret() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join(".env");
        std::fs::write(&target, "synthetic-fixture").unwrap();
        let resource = crate::resource_identity::resolve_filesystem_resource(&target).unwrap();
        let ordinary = GrantBinding {
            capability: Capability::FilesystemRead,
            resource: resource.clone(),
        };
        let workspace = Workspace::new(root.path())
            .unwrap()
            .with_resource_bindings(vec![
                ordinary.clone(),
                GrantBinding {
                    capability: Capability::CredentialUse,
                    resource: resource.clone(),
                },
            ]);
        assert!(workspace.resolve_for_read(&target).is_err());
        let raw = workspace.with_resource_bindings(vec![
            ordinary,
            GrantBinding {
                capability: Capability::CredentialRawRead,
                resource,
            },
        ]);
        assert!(raw.resolve_for_read(&target).is_ok());
    }
}

#[cfg(test)]
mod read_confinement_tests {
    use super::Workspace;

    #[test]
    fn a_read_confined_workspace_keeps_reads_inside_the_review_object() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("inside.txt"), "inside").unwrap();
        let outside_file = outside.path().join("outside.txt");
        std::fs::write(&outside_file, "outside").unwrap();

        let confined = Workspace::new(root.path())
            .unwrap()
            .with_read_confinement(true);
        assert!(confined.resolve_for_read("inside.txt").is_ok());
        assert!(
            confined
                .resolve_for_read(root.path().join("inside.txt"))
                .is_ok()
        );
        let error = confined.resolve_for_read(&outside_file).unwrap_err();
        assert!(error.to_string().contains("reviewed workspace"), "{error}");
        assert!(
            confined.resolve_for_read("../outside.txt").is_err(),
            "a lexical escape must not resolve"
        );

        // The ordinary agent keeps the product's absolute-read contract.
        let ordinary = Workspace::new(root.path()).unwrap();
        assert_eq!(
            ordinary.resolve_for_read(&outside_file).unwrap(),
            std::fs::canonicalize(&outside_file).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_the_workspace_cannot_smuggle_an_outside_read() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "outside").unwrap();
        std::os::unix::fs::symlink(&outside_file, root.path().join("link.txt")).unwrap();
        let confined = Workspace::new(root.path())
            .unwrap()
            .with_read_confinement(true);
        let error = confined.resolve_for_read("link.txt").unwrap_err();
        assert!(error.to_string().contains("reviewed workspace"), "{error}");
    }

    /// Deterministic check-then-swap: resolve the path while it is a regular
    /// file, replace it with a symlink that escapes, then open. The open is
    /// bound to the capability handle, so it refuses the swapped object
    /// instead of following it. A path-based open would read `outside`.
    #[cfg(unix)]
    #[test]
    fn a_path_swapped_between_resolution_and_open_cannot_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("canary.txt");
        std::fs::write(&outside_file, "OUTSIDE CANARY").unwrap();
        let inside = root.path().join("safe.txt");
        std::fs::write(&inside, "inside").unwrap();

        let confined = Workspace::new(root.path())
            .unwrap()
            .with_read_confinement(true);
        // The authorized observation: this path is a regular file inside.
        let resolved = confined.resolve_for_read("safe.txt").unwrap();
        assert_eq!(
            std::fs::read(&resolved).unwrap(),
            b"inside",
            "the pre-swap file is legitimate"
        );

        // Swap AFTER the check, BEFORE the open.
        std::fs::remove_file(&resolved).unwrap();
        std::os::unix::fs::symlink(&outside_file, &resolved).unwrap();

        // The path-based open (what the old reader did) would now read the
        // canary; the capability open must refuse.
        assert!(
            std::fs::read(&resolved).is_ok(),
            "fixture sanity: the swapped path does resolve outside"
        );
        assert!(
            confined.open_confined(&resolved).is_none(),
            "a symlink swapped in after resolution must not open through the workspace capability"
        );

        // The combined entry point refuses as well (it re-checks and then the
        // capability refuses).
        let denied = confined.open_read("safe.txt");
        assert!(denied.is_err(), "the review seat must not read the canary");
    }

    /// The same swap at a DIRECTORY component: replacing `sub/` with a symlink
    /// to an outside tree must not let a file beneath it open.
    #[cfg(unix)]
    #[test]
    fn a_directory_swapped_between_resolution_and_open_cannot_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        let inside = root.path().join("sub/file.txt");
        std::fs::write(&inside, "inside").unwrap();
        let outside_file = outside.path().join("file.txt");
        std::fs::write(&outside_file, "OUTSIDE CANARY").unwrap();

        let confined = Workspace::new(root.path())
            .unwrap()
            .with_read_confinement(true);
        let resolved = confined.resolve_for_read("sub/file.txt").unwrap();

        std::fs::remove_file(&inside).unwrap();
        std::fs::remove_dir(root.path().join("sub")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("sub")).unwrap();

        assert!(
            std::fs::read(&resolved).is_ok(),
            "fixture sanity: the swapped path now resolves outside"
        );
        assert!(
            confined.open_confined(&resolved).is_none(),
            "an escaping directory component must not open"
        );
    }

    /// The default `list_files` path is `.`; a confined listing of the
    /// workspace ROOT must work, not fail on an empty relative path.
    #[test]
    fn a_confined_listing_of_the_workspace_root_lists_its_children() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "a").unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        let confined = Workspace::new(root.path())
            .unwrap()
            .with_read_confinement(true);
        let mut entries = confined.read_dir_confined(".").unwrap();
        entries.sort();
        assert!(
            entries
                .iter()
                .any(|(name, is_dir)| name == "a.txt" && !is_dir)
        );
        assert!(
            entries
                .iter()
                .any(|(name, is_dir)| name == "src" && *is_dir)
        );
    }

    /// A legitimate symlink that stays inside the workspace is still readable:
    /// the capability refuses ESCAPES, not indirection.
    #[cfg(unix)]
    #[test]
    fn a_symlink_that_stays_inside_is_still_readable() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("real.txt"), "inside").unwrap();
        std::os::unix::fs::symlink(root.path().join("real.txt"), root.path().join("link.txt"))
            .unwrap();
        let confined = Workspace::new(root.path())
            .unwrap()
            .with_read_confinement(true);
        let resolved = confined.resolve_for_read("link.txt").unwrap();
        let mut file = confined
            .open_confined(&resolved)
            .expect("an inside symlink opens");
        use std::io::Read;
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "inside");
    }
}
