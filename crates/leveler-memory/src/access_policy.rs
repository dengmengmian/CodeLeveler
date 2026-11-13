//! Filesystem access policy for model-visible memory roots.
//!
//! Reads may inspect safe in-scope files. Writes are limited to Markdown topic
//! and observation-inbox files, use atomic replacement, and require an
//! unchanged snapshot from an earlier ordinary read when replacing a file.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

use crate::access::{MemoryAccess, MemoryWrite};

use crate::scope::{
    ManifestBudget, MemoryScope, MemoryScopeError, bump_manifest_revision,
    bump_manifest_revision_in_transaction, is_durably_excluded, regenerate_scope_manifest,
};
use crate::topic_reads::record_topic_read;

const MAX_PREVIOUS_CONTENT_BYTES: u64 = 8 * 1024 * 1024;
/// Largest topic a write may produce. Dream's `read_topics` and forget both
/// refuse larger files, so accepting one here would make every later Dream fail.
pub(crate) const MAX_TOPIC_FILE_BYTES: u64 = 256 * 1024;
/// Largest inbox observation a write may produce. Mirrors the private
/// `MAX_OBSERVATION_FILE_BYTES` in `capture_store.rs`, which is the source of
/// truth for what capture recovery and Dream are willing to read back.
const MAX_WRITE_OBSERVATION_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPathClass {
    Outside,
    Manifest(MemoryScope),
    Topic(MemoryScope),
    Observation(MemoryScope),
    ArchivedObservation(MemoryScope),
    /// A path nested below `topics/` or `observations/_inbox/`. Manifest
    /// generation, browse, Dream input, and capture recovery only list
    /// immediate children, so such files would be invisible.
    Nested(MemoryScope),
    Protected(MemoryScope),
}

/// One bounded read of a browsable memory file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryBoundedRead {
    /// The file's prefix, at most the requested bound.
    pub bytes: Vec<u8>,
    /// The file was larger than the bound; `bytes` holds its prefix.
    pub truncated: bool,
    /// The file's size at read time.
    pub total_bytes: u64,
}

impl MemoryPathClass {
    fn scope(self) -> Option<MemoryScope> {
        match self {
            Self::Outside => None,
            Self::Manifest(scope)
            | Self::Topic(scope)
            | Self::Observation(scope)
            | Self::Nested(scope)
            | Self::ArchivedObservation(scope)
            | Self::Protected(scope) => Some(scope),
        }
    }

    fn is_writable(self) -> bool {
        matches!(self, Self::Topic(_) | Self::Observation(_))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryAccessError {
    #[error("memory path must be absolute: {0}")]
    RelativePath(PathBuf),
    #[error("memory path contains traversal components: {0}")]
    Traversal(PathBuf),
    #[error("memory path escapes its configured scope: {0}")]
    EscapesScope(PathBuf),
    #[error("symbolic links are not allowed in memory paths: {0}")]
    Symlink(PathBuf),
    #[error("failed to inspect memory path {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writes are not allowed to protected memory path: {0}")]
    Protected(PathBuf),
    #[error("memory files must live directly under topics/ or observations/_inbox/: {0}")]
    NestedPath(PathBuf),
    #[error("memory writes require a .md file: {0}")]
    NonMarkdown(PathBuf),
    #[error("read {0} before editing it")]
    ReadRequired(PathBuf),
    #[error("memory file changed since it was read; read it again before editing: {0}")]
    Stale(PathBuf),
    #[error("memory file is too large to edit safely (limit: {limit_bytes} bytes): {path}")]
    TooLarge { path: PathBuf, limit_bytes: u64 },
    #[error("memory path is durably excluded and cannot be recreated: {0}")]
    Excluded(PathBuf),
    #[error("memory archive is runtime retention data and is not readable as memory: {0}")]
    Archive(PathBuf),
    #[error(
        "memory observations are append-only; create a new observation file or record the correction in a topic instead of editing {0}"
    )]
    ImmutableObservation(PathBuf),
    #[error("failed to inspect memory durable exclusions for {path}: {source}")]
    ExclusionState {
        path: PathBuf,
        #[source]
        source: Box<MemoryScopeError>,
    },
    #[error("failed to atomically write memory file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "memory file write was rolled back because its generated MEMORY.md manifest could not be refreshed: {source}"
    )]
    ManifestRefresh {
        #[source]
        source: Box<MemoryScopeError>,
    },
    #[error(
        "memory file was written and its generated MEMORY.md manifest could not be refreshed; rollback also failed: {rollback}"
    )]
    ManifestRefreshRollback {
        source: Box<MemoryScopeError>,
        rollback: Box<MemoryAccessError>,
    },
}

pub type MemoryAccessResult<T> = std::result::Result<T, MemoryAccessError>;

/// Lock a short-lived policy mutex. A poisoned lock still carries valid state,
/// so recover the guard rather than failing the memory operation.
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shared per-session access policy and read-snapshot tracker.
#[derive(Debug)]
pub struct MemoryAccessPolicy {
    global_root: PathBuf,
    global_canonical_root: PathBuf,
    workspace_root: PathBuf,
    workspace_canonical_root: PathBuf,
    snapshots: std::sync::Mutex<HashMap<PathBuf, blake3::Hash>>,
    write_lock: std::sync::Mutex<()>,
}

struct ValidatedMemoryWrite {
    class: MemoryPathClass,
    canonical: PathBuf,
    previous_content: Option<Vec<u8>>,
}

impl MemoryAccessPolicy {
    /// Build a policy over already initialized scope roots.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryAccessError::Inspect`] if either root cannot be canonicalized.
    pub fn new(global_root: &Path, workspace_root: &Path) -> MemoryAccessResult<Self> {
        Ok(Self {
            global_root: global_root.to_path_buf(),
            global_canonical_root: canonicalize_root(global_root)?,
            workspace_root: workspace_root.to_path_buf(),
            workspace_canonical_root: canonicalize_root(workspace_root)?,
            snapshots: std::sync::Mutex::new(HashMap::new()),
            write_lock: std::sync::Mutex::new(()),
        })
    }

    /// Classify a path against both configured and resolved scope roots.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryAccessError::Inspect`] if the nearest existing path prefix
    /// cannot be resolved.
    pub fn classify_path(&self, path: &Path) -> MemoryAccessResult<MemoryPathClass> {
        if !path.is_absolute() {
            return Err(MemoryAccessError::RelativePath(path.to_path_buf()));
        }
        let configured_class = [
            (&self.workspace_root, MemoryScope::Workspace),
            (&self.global_root, MemoryScope::Global),
        ]
        .into_iter()
        .find_map(|(root, scope)| classify_against_root(path, root, scope));
        let configured_scope = [
            (&self.workspace_root, MemoryScope::Workspace),
            (&self.global_root, MemoryScope::Global),
        ]
        .into_iter()
        .find_map(|(root, scope)| path.strip_prefix(root).ok().map(|_| scope));
        let resolved = resolve_nearest_existing(path)?;
        let resolved_class = [
            (&self.workspace_canonical_root, MemoryScope::Workspace),
            (&self.global_canonical_root, MemoryScope::Global),
        ]
        .into_iter()
        .find_map(|(root, scope)| classify_against_root(&resolved, root, scope));

        if let Some(resolved_class) = resolved_class {
            if configured_class.is_none()
                && let Some(scope) = configured_scope
            {
                return Ok(if matches!(resolved_class, MemoryPathClass::Manifest(_)) {
                    resolved_class
                } else {
                    MemoryPathClass::Protected(scope)
                });
            }
            if configured_class
                .and_then(MemoryPathClass::scope)
                .is_some_and(|scope| Some(scope) != resolved_class.scope())
            {
                return Err(MemoryAccessError::EscapesScope(path.to_path_buf()));
            }
            return Ok(resolved_class);
        }
        Ok(configured_class.unwrap_or(MemoryPathClass::Outside))
    }

    /// Remove a previously read topic file with the same containment and stale
    /// snapshot checks used by ordinary memory writes.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryAccessError::Protected`] unless `path` is a topic Markdown
    /// file, or [`MemoryAccessError::ReadRequired`] / [`MemoryAccessError::Stale`] when
    /// the caller has not observed the current bytes.
    pub fn remove_topic_file(&self, path: &Path) -> MemoryAccessResult<()> {
        self.remove_topic_file_inner(path, None)
    }

    /// Read one browsable file for a UI browser.
    ///
    /// Applies the same containment, symlink, exclusion and regular-file rules
    /// as a tool read, but records no read snapshot or topic-read count: a
    /// preview is not an edit, and forget records its own evidence. Unlike a
    /// tool read, the user's own audit preview may open retained archive
    /// observations. `None` means the path is outside both memory roots.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryAccessError::Protected`] for manifests' neighbours such as
    /// state databases, nested files, and anything the store hides.
    pub fn read_browsable(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> MemoryAccessResult<Option<MemoryBoundedRead>> {
        let class = self.classify_path(path)?;
        if class == MemoryPathClass::Outside {
            return Ok(None);
        }
        if !matches!(
            class,
            MemoryPathClass::Manifest(_)
                | MemoryPathClass::Topic(_)
                | MemoryPathClass::Observation(_)
                | MemoryPathClass::ArchivedObservation(_)
        ) {
            return Err(MemoryAccessError::Protected(path.to_path_buf()));
        }
        self.validate_read_inner(path)?;
        let canonical = self.validate_containment(path, class)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options
            .open(&canonical)
            .map_err(|source| MemoryAccessError::Inspect {
                path: canonical.clone(),
                source,
            })?;
        let metadata = file
            .metadata()
            .map_err(|source| MemoryAccessError::Inspect {
                path: canonical.clone(),
                source,
            })?;
        if !metadata.is_file() {
            return Err(MemoryAccessError::Inspect {
                path: canonical.clone(),
                source: std::io::Error::other("memory path is not a regular file"),
            });
        }
        let total_bytes = metadata.len();
        let mut bytes = Vec::with_capacity(total_bytes.min(max_bytes) as usize);
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|source| MemoryAccessError::Inspect {
                path: canonical.clone(),
                source,
            })?;
        let truncated = bytes.len() as u64 > max_bytes;
        bytes.truncate(max_bytes as usize);
        Ok(Some(MemoryBoundedRead {
            bytes,
            truncated,
            total_bytes,
        }))
    }

    pub(crate) fn remove_topic_file_in_transaction(
        &self,
        path: &Path,
        transaction: &rusqlite::Transaction<'_>,
    ) -> MemoryAccessResult<()> {
        self.remove_topic_file_inner(path, Some(transaction))
    }

    fn remove_topic_file_inner(
        &self,
        path: &Path,
        transaction: Option<&rusqlite::Transaction<'_>>,
    ) -> MemoryAccessResult<()> {
        let _write_guard = lock(&self.write_lock);
        let class = self.classify_path(path)?;
        if matches!(class, MemoryPathClass::Nested(_)) {
            return Err(MemoryAccessError::NestedPath(path.to_path_buf()));
        }
        if !matches!(class, MemoryPathClass::Topic(_)) {
            return Err(MemoryAccessError::Protected(path.to_path_buf()));
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
            return Err(MemoryAccessError::NonMarkdown(path.to_path_buf()));
        }
        let canonical = self.validate_containment(path, class)?;
        let metadata =
            std::fs::symlink_metadata(&canonical).map_err(|source| MemoryAccessError::Inspect {
                path: canonical.clone(),
                source,
            })?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_TOPIC_FILE_BYTES {
            return Err(MemoryAccessError::Inspect {
                path: canonical.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "topic is not a bounded regular file",
                ),
            });
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        std::fs::File::open(&canonical)
            .and_then(|mut file| {
                std::io::Read::by_ref(&mut file)
                    .take(MAX_TOPIC_FILE_BYTES + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|source| MemoryAccessError::Inspect {
                path: canonical.clone(),
                source,
            })?;
        if bytes.len() as u64 > MAX_TOPIC_FILE_BYTES {
            return Err(MemoryAccessError::Inspect {
                path: canonical.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "topic exceeds its read limit",
                ),
            });
        }
        let observed_hash = blake3::hash(&bytes);
        let snapshots = lock(&self.snapshots);
        let Some(expected) = snapshots.get(&canonical) else {
            return Err(MemoryAccessError::ReadRequired(canonical));
        };
        if *expected != observed_hash {
            return Err(MemoryAccessError::Stale(canonical));
        }
        drop(snapshots);
        std::fs::remove_file(&canonical).map_err(|source| MemoryAccessError::Write {
            path: canonical.clone(),
            source,
        })?;
        lock(&self.snapshots).remove(&canonical);
        let scope = class
            .scope()
            .ok_or_else(|| MemoryAccessError::EscapesScope(path.to_path_buf()))?;
        self.refresh_manifest(scope, transaction)
            .map_err(|source| {
                // The manifest makes deletion observable. Restore the topic on a refresh
                // failure so the caller can retry from the same snapshot.
                match persist_atomically(&canonical, &bytes, true) {
                    Ok(()) => {
                        lock(&self.snapshots).insert(canonical.clone(), observed_hash);
                        MemoryAccessError::ManifestRefresh {
                            source: Box::new(source),
                        }
                    }
                    Err(rollback) => MemoryAccessError::ManifestRefreshRollback {
                        source: Box::new(source),
                        rollback: Box::new(rollback),
                    },
                }
            })?;
        Ok(())
    }

    pub(crate) fn validate_read_typed(&self, path: &Path) -> MemoryAccessResult<bool> {
        self.validate_read_inner(path)
    }

    pub(crate) fn record_read_typed(&self, path: &Path, contents: &[u8]) -> MemoryAccessResult<()> {
        self.record_read_inner(path, contents)
    }

    pub(crate) fn write_file_typed_in_transaction(
        &self,
        path: &Path,
        contents: &[u8],
        transaction: &rusqlite::Transaction<'_>,
    ) -> MemoryAccessResult<MemoryWrite> {
        self.write_file_inner_with_transaction(path, contents, Some(transaction))
    }

    fn root_for_scope(&self, scope: MemoryScope) -> &Path {
        match scope {
            MemoryScope::Global => &self.global_root,
            MemoryScope::Workspace => &self.workspace_root,
        }
    }

    fn canonical_root_for_scope(&self, scope: MemoryScope) -> &Path {
        match scope {
            MemoryScope::Global => &self.global_canonical_root,
            MemoryScope::Workspace => &self.workspace_canonical_root,
        }
    }

    fn refresh_manifest(
        &self,
        scope: MemoryScope,
        transaction: Option<&rusqlite::Transaction<'_>>,
    ) -> std::result::Result<(), MemoryScopeError> {
        let root = self.root_for_scope(scope);
        match transaction {
            Some(transaction) => bump_manifest_revision_in_transaction(root, transaction),
            None => bump_manifest_revision(root),
        }
        .and_then(|()| {
            regenerate_scope_manifest(root, scope, ManifestBudget::configured()).map(|_| ())
        })
    }

    fn validate_containment(
        &self,
        path: &Path,
        class: MemoryPathClass,
    ) -> MemoryAccessResult<PathBuf> {
        let Some(scope) = class.scope() else {
            return Ok(path.to_path_buf());
        };
        let root = self.root_for_scope(scope);
        let canonical_root = self.canonical_root_for_scope(scope);
        if path.starts_with(root) {
            reject_symlink_components(root, path)?;
        } else {
            let common = common_ancestor(root, path);
            reject_symlink_components(&common, path)?;
        }

        let canonical = resolve_nearest_existing(path)?;
        if !canonical.starts_with(canonical_root) {
            return Err(MemoryAccessError::EscapesScope(path.to_path_buf()));
        }
        Ok(canonical)
    }

    fn validate_read_inner(&self, path: &Path) -> MemoryAccessResult<bool> {
        let class = self.classify_path(path)?;
        if class == MemoryPathClass::Outside {
            return Ok(false);
        }
        let canonical = self.validate_containment(path, class)?;
        let scope = class
            .scope()
            .ok_or_else(|| MemoryAccessError::EscapesScope(path.to_path_buf()))?;
        let relative = canonical
            .strip_prefix(self.canonical_root_for_scope(scope))
            .map_err(|_| MemoryAccessError::EscapesScope(path.to_path_buf()))?;
        if is_durably_excluded(self.root_for_scope(scope), relative).map_err(|source| {
            MemoryAccessError::ExclusionState {
                path: path.to_path_buf(),
                source: Box::new(source),
            }
        })? {
            return Err(MemoryAccessError::Excluded(path.to_path_buf()));
        }
        Ok(true)
    }

    /// Snapshot only. Internal callers (Dream, forget, carry-over) use this
    /// path, some while holding the scope's state write lock, so it must never
    /// touch the state database.
    fn record_read_inner(&self, path: &Path, contents: &[u8]) -> MemoryAccessResult<()> {
        self.record_read_classified(path, contents).map(|_| ())
    }

    fn record_read_classified(
        &self,
        path: &Path,
        contents: &[u8],
    ) -> MemoryAccessResult<Option<(MemoryPathClass, PathBuf)>> {
        let class = self.classify_path(path)?;
        if class == MemoryPathClass::Outside {
            return Ok(None);
        }
        let canonical = self.validate_containment(path, class)?;
        lock(&self.snapshots).insert(canonical.clone(), blake3::hash(contents));
        Ok(Some((class, canonical)))
    }

    /// Ranking data only: a failed count must never fail the read itself.
    fn count_topic_read(&self, scope: MemoryScope, canonical: &Path) {
        let Ok(relative) = canonical.strip_prefix(self.canonical_root_for_scope(scope)) else {
            return;
        };
        let relative = relative.to_string_lossy();
        if let Err(error) = record_topic_read(self.root_for_scope(scope), &relative) {
            tracing::warn!(path = %canonical.display(), %error, "failed to count memory topic read");
        }
    }

    fn validate_write_locked(
        &self,
        path: &Path,
        contents: &[u8],
    ) -> MemoryAccessResult<Option<ValidatedMemoryWrite>> {
        let class = self.classify_path(path)?;
        if class == MemoryPathClass::Outside {
            return Ok(None);
        }
        if matches!(class, MemoryPathClass::Nested(_)) {
            return Err(MemoryAccessError::NestedPath(path.to_path_buf()));
        }
        if !class.is_writable() {
            return Err(MemoryAccessError::Protected(path.to_path_buf()));
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
            return Err(MemoryAccessError::NonMarkdown(path.to_path_buf()));
        }
        let limit_bytes = match class {
            MemoryPathClass::Observation(_) => MAX_WRITE_OBSERVATION_BYTES,
            _ => MAX_TOPIC_FILE_BYTES,
        };
        if contents.len() as u64 > limit_bytes {
            return Err(MemoryAccessError::TooLarge {
                path: path.to_path_buf(),
                limit_bytes,
            });
        }

        let canonical = self.validate_containment(path, class)?;
        let scope = class
            .scope()
            .ok_or_else(|| MemoryAccessError::EscapesScope(path.to_path_buf()))?;
        let relative = canonical
            .strip_prefix(self.canonical_root_for_scope(scope))
            .map_err(|_| MemoryAccessError::EscapesScope(path.to_path_buf()))?;
        if is_durably_excluded(self.root_for_scope(scope), relative).map_err(|source| {
            MemoryAccessError::ExclusionState {
                path: path.to_path_buf(),
                source: Box::new(source),
            }
        })? {
            return Err(MemoryAccessError::Excluded(path.to_path_buf()));
        }
        if matches!(class, MemoryPathClass::Observation(_))
            && std::fs::symlink_metadata(&canonical).is_ok()
        {
            return Err(MemoryAccessError::ImmutableObservation(path.to_path_buf()));
        }
        let previous_content = match read_previous_content(&canonical) {
            Ok(bytes) => {
                let snapshots = lock(&self.snapshots);
                let Some(expected) = snapshots.get(&canonical) else {
                    return Err(MemoryAccessError::ReadRequired(canonical));
                };
                if *expected != blake3::hash(&bytes) {
                    return Err(MemoryAccessError::Stale(canonical));
                }
                Some(bytes)
            }
            Err(MemoryAccessError::Inspect { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        Ok(Some(ValidatedMemoryWrite {
            class,
            canonical,
            previous_content,
        }))
    }

    fn preflight_write_inner(&self, path: &Path, contents: &[u8]) -> MemoryAccessResult<bool> {
        let _write_guard = lock(&self.write_lock);
        self.validate_write_locked(path, contents)
            .map(|validated| validated.is_some())
    }

    /// Ordinary writes hold the scope's state-database write lock while they
    /// publish, so they serialize with Dream commits instead of interleaving.
    fn write_file_inner(&self, path: &Path, contents: &[u8]) -> MemoryAccessResult<MemoryWrite> {
        let class = self.classify_path(path)?;
        let Some(scope) = class.scope() else {
            return Ok(MemoryWrite::Outside);
        };
        if !class.is_writable() {
            return self.write_file_inner_with_transaction(path, contents, None);
        }
        self.validate_containment(path, class)?;
        let state_path = self.root_for_scope(scope).join("memory_state.sqlite");
        reject_symlink_components(self.root_for_scope(scope), &state_path)?;
        let database_error = |source| MemoryAccessError::ExclusionState {
            path: path.to_path_buf(),
            source: Box::new(MemoryScopeError::Database {
                database: "state",
                path: state_path.clone(),
                source,
            }),
        };
        let mut connection = crate::sqlite_journal::JournalMode::for_db_path(&state_path)
            .open(&state_path)
            .map_err(database_error)?;
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let result = self.write_file_inner_with_transaction(path, contents, Some(&transaction))?;
        transaction.commit().map_err(database_error)?;
        Ok(result)
    }

    fn write_file_inner_with_transaction(
        &self,
        path: &Path,
        contents: &[u8],
        transaction: Option<&rusqlite::Transaction<'_>>,
    ) -> MemoryAccessResult<MemoryWrite> {
        let _write_guard = lock(&self.write_lock);
        let Some(validated) = self.validate_write_locked(path, contents)? else {
            return Ok(MemoryWrite::Outside);
        };
        let ValidatedMemoryWrite {
            class,
            mut canonical,
            previous_content,
        } = validated;

        let parent = canonical
            .parent()
            .ok_or_else(|| MemoryAccessError::EscapesScope(path.to_path_buf()))?;
        std::fs::create_dir_all(parent).map_err(|source| MemoryAccessError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        // Creating missing parents changes which prefix is canonicalized. Check
        // containment and symlink components again before opening the tempfile.
        canonical = self.validate_containment(path, class)?;
        persist_atomically(&canonical, contents, previous_content.is_none())?;

        let scope = class
            .scope()
            .ok_or_else(|| MemoryAccessError::EscapesScope(path.to_path_buf()))?;
        self.refresh_manifest(scope, transaction)
            .map_err(|source| {
                let rollback = if let Some(previous_content) = previous_content.as_deref() {
                    persist_atomically(&canonical, previous_content, false)
                } else {
                    std::fs::remove_file(&canonical).map_err(|rollback_source| {
                        MemoryAccessError::Write {
                            path: canonical.clone(),
                            source: rollback_source,
                        }
                    })
                };
                match rollback {
                    Ok(()) => MemoryAccessError::ManifestRefresh {
                        source: Box::new(source),
                    },
                    Err(rollback) => MemoryAccessError::ManifestRefreshRollback {
                        source: Box::new(source),
                        rollback: Box::new(rollback),
                    },
                }
            })?;
        lock(&self.snapshots).insert(canonical, blake3::hash(contents));
        Ok(MemoryWrite::Written { previous_content })
    }
}

impl MemoryAccess for MemoryAccessPolicy {
    fn validate_read(&self, path: &Path) -> Result<bool, String> {
        // Archived observations are retention evidence for Dream convergence
        // and audit, not a memory source. Letting the model read them would
        // resurrect facts the user forgot and that no topic or index exposes.
        if matches!(
            self.classify_path(path),
            Ok(MemoryPathClass::ArchivedObservation(_))
        ) {
            return Err(MemoryAccessError::Archive(path.to_path_buf()).to_string());
        }
        self.validate_read_inner(path)
            .map_err(|error| error.to_string())
    }

    /// The tool-facing read: the only place a topic read is counted for the
    /// compact index ranking.
    fn record_read(&self, path: &Path, contents: &[u8]) -> Result<(), String> {
        self.record_read_classified(path, contents)
            .map(|classified| {
                if let Some((MemoryPathClass::Topic(scope), canonical)) = classified {
                    self.count_topic_read(scope, &canonical);
                }
            })
            .map_err(|error| error.to_string())
    }

    fn preflight_write(&self, path: &Path, contents: &[u8]) -> Result<bool, String> {
        self.preflight_write_inner(path, contents)
            .map_err(|error| error.to_string())
    }

    fn write_file(&self, path: &Path, contents: &[u8]) -> Result<MemoryWrite, String> {
        self.write_file_inner(path, contents)
            .map_err(|error| error.to_string())
    }

    fn scope_roots(&self) -> [PathBuf; 2] {
        [self.global_root.clone(), self.workspace_root.clone()]
    }
}

fn canonicalize_root(root: &Path) -> MemoryAccessResult<PathBuf> {
    dunce::canonicalize(root).map_err(|source| MemoryAccessError::Inspect {
        path: root.to_path_buf(),
        source,
    })
}

fn classify_against_root(path: &Path, root: &Path, scope: MemoryScope) -> Option<MemoryPathClass> {
    let relative = path.strip_prefix(root).ok()?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }
    if relative == Path::new("MEMORY.md") {
        return Some(MemoryPathClass::Manifest(scope));
    }
    if let Ok(below) = relative.strip_prefix(Path::new("topics")) {
        return Some(if below.components().count() > 1 {
            MemoryPathClass::Nested(scope)
        } else {
            MemoryPathClass::Topic(scope)
        });
    }
    if let Ok(below) = relative.strip_prefix(Path::new("observations").join("_inbox")) {
        return Some(if below.components().count() > 1 {
            MemoryPathClass::Nested(scope)
        } else {
            MemoryPathClass::Observation(scope)
        });
    }
    if relative.starts_with(Path::new("archive")) {
        return Some(MemoryPathClass::ArchivedObservation(scope));
    }
    Some(MemoryPathClass::Protected(scope))
}

fn resolve_nearest_existing(path: &Path) -> MemoryAccessResult<PathBuf> {
    for ancestor in path.ancestors() {
        match dunce::canonicalize(ancestor) {
            Ok(canonical_ancestor) => {
                let relative = path
                    .strip_prefix(ancestor)
                    .map_err(|_| MemoryAccessError::EscapesScope(path.to_path_buf()))?;
                return normalize_path(&canonical_ancestor.join(relative), path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(MemoryAccessError::Inspect {
                    path: ancestor.to_path_buf(),
                    source,
                });
            }
        }
    }
    Err(MemoryAccessError::EscapesScope(path.to_path_buf()))
}

fn normalize_path(candidate: &Path, original: &Path) -> MemoryAccessResult<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(MemoryAccessError::Traversal(original.to_path_buf()));
                }
            }
        }
    }
    Ok(normalized)
}

fn common_ancestor(left: &Path, right: &Path) -> PathBuf {
    let mut common = PathBuf::new();
    for (left_component, right_component) in left.components().zip(right.components()) {
        if left_component != right_component {
            break;
        }
        common.push(left_component.as_os_str());
    }
    common
}

fn reject_symlink_components(root: &Path, path: &Path) -> MemoryAccessResult<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| MemoryAccessError::EscapesScope(path.to_path_buf()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(MemoryAccessError::Symlink(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(MemoryAccessError::Inspect {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn read_previous_content(path: &Path) -> MemoryAccessResult<Vec<u8>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|source| MemoryAccessError::Inspect {
            path: path.to_path_buf(),
            source,
        })?;
    let metadata = file
        .metadata()
        .map_err(|source| MemoryAccessError::Inspect {
            path: path.to_path_buf(),
            source,
        })?;
    if !metadata.is_file() {
        return Err(MemoryAccessError::Inspect {
            path: path.to_path_buf(),
            source: std::io::Error::other("memory path is not a regular file"),
        });
    }
    if metadata.len() > MAX_PREVIOUS_CONTENT_BYTES {
        return Err(MemoryAccessError::TooLarge {
            path: path.to_path_buf(),
            limit_bytes: MAX_PREVIOUS_CONTENT_BYTES,
        });
    }

    let mut bytes = Vec::new();
    file.take(MAX_PREVIOUS_CONTENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| MemoryAccessError::Inspect {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_PREVIOUS_CONTENT_BYTES {
        return Err(MemoryAccessError::TooLarge {
            path: path.to_path_buf(),
            limit_bytes: MAX_PREVIOUS_CONTENT_BYTES,
        });
    }
    Ok(bytes)
}

fn persist_atomically(path: &Path, contents: &[u8], is_new: bool) -> MemoryAccessResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| MemoryAccessError::EscapesScope(path.to_path_buf()))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| MemoryAccessError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .write_all(contents)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| MemoryAccessError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    let persisted = if is_new {
        temporary.persist_noclobber(path)
    } else {
        temporary.persist(path)
    };
    persisted
        .map(|_| ())
        .map_err(|error| MemoryAccessError::Write {
            path: path.to_path_buf(),
            source: error.error,
        })
}

#[cfg(test)]
#[path = "access_policy_tests.rs"]
mod tests;
