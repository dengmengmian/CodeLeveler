//! `WorkspaceEditor` — the one guarded path from a tool to a file on disk.
//!
//! Every mutation a tool makes goes through here: an advisory cross-process
//! lock held across the compare and the rename, a compare-and-swap against the
//! content the caller observed, an unguessable staging name, a
//! capability/descriptor-relative write that a concurrent junction or symlink
//! swap cannot redirect outside the workspace, checkpoint capture for
//! rollback, and a write-scope revalidation taken under the lock.
//!
//! It used to live inside the `replace` tool, which meant the shared commit
//! path was named after one of its callers. Only the owner changed: the
//! semantics, the locking and the platform-specific write paths are the same
//! code (`docs/ARCHITECTURE.md` §18.3 C).

use crate::tool::{ToolContext, ToolError};

/// The guarded editor. Stateless: every call takes the context that carries
/// the workspace, the write scope and the checkpoint.
pub(crate) struct WorkspaceEditor;

/// Outcome of a locked commit. `Stale` and `Rejected` wrote nothing; the caller
/// turns each into the right model-facing message (CAS staleness vs. a path
/// that left the workspace), so every edit tool phrases it identically.
pub(crate) enum Commit {
    Written(Option<leveler_core::ResourceIdentity>),
    Stale,
    Rejected(String),
}

// Keep descriptor-relative CAS writes while selecting the filesystem root
// authorized by this exact call. A workspace root cannot address external files.
fn editing_workspace(
    context: &ToolContext,
    resolved: &std::path::Path,
    capability: leveler_core::Capability,
) -> Result<leveler_execution::Workspace, ToolError> {
    let exact = context
        .execution_workspace()?
        .allows_resource_path(capability, resolved);
    if context.policy.unrestricted_execution() || exact {
        let root = resolved
            .ancestors()
            .last()
            .ok_or_else(|| ToolError::Io("target has no filesystem root".into()))?;
        if context.policy.unrestricted_execution() {
            Ok(leveler_execution::Workspace::new(root)?.with_unrestricted_access(true))
        } else {
            Ok(
                leveler_execution::Workspace::new(root)?.with_resource_bindings(
                    context
                        .policy
                        .resource_grant()
                        .map(|request| request.bindings.clone())
                        .unwrap_or_default(),
                ),
            )
        }
    } else {
        context.execution_workspace()
    }
}

#[cfg(unix)]
fn descriptor_resource_authority(
    context: &ToolContext,
    resolved: &std::path::Path,
    capability: leveler_core::Capability,
) -> Option<leveler_core::ResourceIdentity> {
    context
        .policy
        .resource_grant()?
        .bindings
        .iter()
        .find_map(|binding| {
            if binding.capability != capability {
                return None;
            }
            match &binding.resource {
                leveler_core::ResourceIdentity::FilesystemPath { canonical_path, .. }
                    if std::path::Path::new(canonical_path) == resolved =>
                {
                    Some(binding.resource.clone())
                }
                _ => None,
            }
        })
}

impl WorkspaceEditor {
    /// Lock the target, verify it still equals `expected`, and atomically replace it
    /// with `replacement`. Returns `Stale` (writing nothing) if the on-disk content
    /// diverged since `expected` was read.
    ///
    /// This is the one commit path every edit tool shares: an advisory
    /// cross-process lock (under the leveler home, never in the workspace) held
    /// across the compare + rename, an unguessable temp name, and — on unix and
    /// Windows — a capability/descriptor-relative write that a concurrent
    /// junction/symlink swap cannot redirect outside the workspace.
    pub(crate) async fn replace(
        context: &ToolContext,
        resolved: &std::path::Path,
        expected: &str,
        replacement: &str,
    ) -> Result<Commit, ToolError> {
        let workspace =
            editing_workspace(context, resolved, leveler_core::Capability::FilesystemWrite)?;
        let lock_path =
            leveler_project::layout::target_lock_path(&context.execution.environment, resolved);
        let lock = tokio::task::spawn_blocking({
            let lock_path = lock_path.clone();
            move || TargetLock::acquire(lock_path)
        })
        .await
        .map_err(|e| ToolError::Io(format!("join file-lock task: {e}")))?
        .map_err(|e| ToolError::Io(format!("lock {}: {e}", lock_path.display())))?;

        if let Err(e) = workspace.revalidate_write_path(resolved, &context.write_scope()) {
            drop(lock);
            return Ok(Commit::Rejected(e.to_string()));
        }
        #[cfg(not(windows))]
        let unique = unique_temp_name(resolved);
        let committed_permissions: Option<std::fs::Permissions>;
        #[allow(unused_mut)]
        let mut receipt = None;
        #[cfg(unix)]
        {
            let root = workspace.root().to_path_buf();
            let root_fd = workspace.root_fd();
            let relative = resolved
                .strip_prefix(&root)
                .map_err(|_| {
                    ToolError::Io(format!(
                        "{} is no longer below workspace",
                        resolved.display()
                    ))
                })?
                .to_path_buf();
            let expected = expected.to_string();
            let replacement = replacement.to_string();
            let resource = descriptor_resource_authority(
                context,
                resolved,
                leveler_core::Capability::FilesystemWrite,
            );
            let target = resolved.to_path_buf();
            let committed = tokio::task::spawn_blocking(move || {
                descriptor_relative_replace(
                    &root_fd,
                    &relative,
                    &unique,
                    &expected,
                    &replacement,
                    resource
                        .as_ref()
                        .map(|resource| (target.as_path(), resource)),
                )
            })
            .await
            .map_err(|e| ToolError::Io(format!("join descriptor write: {e}")))?
            .map_err(|e| ToolError::Io(format!("descriptor-relative replace: {e}")))?;
            committed_permissions = committed.map(|(permissions, metadata)| {
                receipt = leveler_execution::resource_identity::filesystem_resource_from_metadata(
                    resolved, &metadata,
                )
                .ok();
                permissions
            });
        }
        #[cfg(windows)]
        {
            let root = workspace.root().to_path_buf();
            let root_dir = workspace.root_dir();
            let relative = resolved
                .strip_prefix(&root)
                .map_err(|_| ToolError::Io("target left workspace".into()))?
                .to_path_buf();
            let expected = expected.to_string();
            let replacement = replacement.to_string();
            committed_permissions = tokio::task::spawn_blocking(
                move || -> std::io::Result<Option<std::fs::Permissions>> {
                    let commit = open_windows_replace_context(&root_dir, &relative)?;
                    windows_capability_replace(commit, &expected, &replacement)
                },
            )
            .await
            .map_err(|e| ToolError::Io(format!("join capability write: {e}")))?
            .map_err(|e| ToolError::Io(format!("capability-relative replace: {e}")))?;
        }
        #[cfg(all(not(unix), not(windows)))]
        {
            let parent = resolved
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let tmp = parent.join(&unique);
            {
                use tokio::io::AsyncWriteExt;
                let mut f = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)
                    .await
                    .map_err(|e| ToolError::Io(format!("create temp {}: {e}", tmp.display())))?;
                f.write_all(replacement.as_bytes())
                    .await
                    .map_err(|e| ToolError::Io(format!("write {}: {e}", tmp.display())))?;
                f.flush()
                    .await
                    .map_err(|e| ToolError::Io(format!("flush {}: {e}", tmp.display())))?;
            }
            let permissions = tokio::fs::symlink_metadata(resolved)
                .await
                .map_err(|e| ToolError::Io(format!("stat {}: {e}", resolved.display())))?
                .permissions();
            match tokio::fs::read_to_string(resolved).await {
                Ok(current) if current == expected => committed_permissions = Some(permissions),
                Ok(_) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    committed_permissions = None;
                }
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    drop(lock);
                    return Err(ToolError::Io(format!(
                        "re-read {}: {e}",
                        resolved.display()
                    )));
                }
            }
            if committed_permissions.is_some()
                && let Err(e) = tokio::fs::rename(&tmp, resolved).await
            {
                let _ = tokio::fs::remove_file(&tmp).await;
                drop(lock);
                return Err(ToolError::Io(format!(
                    "rename into {}: {e}",
                    resolved.display()
                )));
            }
        }
        drop(lock);
        match committed_permissions {
            Some(permissions) => {
                context.execution.checkpoint.record_captured(
                    resolved,
                    expected.as_bytes().to_vec(),
                    permissions,
                );
                Ok(Commit::Written(receipt))
            }
            None => Ok(Commit::Stale),
        }
    }

    /// Atomically create `resolved` only if it is still absent. The staged file is
    /// linked into place with no-overwrite semantics, so an Add/Move destination
    /// created by another writer is reported as [`Commit::Stale`].
    pub(crate) async fn create(
        context: &ToolContext,
        resolved: &std::path::Path,
        content: &str,
    ) -> Result<Commit, ToolError> {
        Self::create_with_permissions(context, resolved, content, None).await
    }

    /// [`Self::create`] restoring the permissions a rollback captured.
    pub(crate) async fn create_with_permissions(
        context: &ToolContext,
        resolved: &std::path::Path,
        content: &str,
        permissions: Option<std::fs::Permissions>,
    ) -> Result<Commit, ToolError> {
        #[cfg(not(unix))]
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }
        let workspace =
            editing_workspace(context, resolved, leveler_core::Capability::FilesystemWrite)?;
        let lock_path =
            leveler_project::layout::target_lock_path(&context.execution.environment, resolved);
        let lock = tokio::task::spawn_blocking({
            let lock_path = lock_path.clone();
            move || TargetLock::acquire(lock_path)
        })
        .await
        .map_err(|e| ToolError::Io(format!("join file-lock task: {e}")))?
        .map_err(|e| ToolError::Io(format!("lock {}: {e}", lock_path.display())))?;

        if let Err(e) = workspace.revalidate_write_path(resolved, &context.write_scope()) {
            drop(lock);
            return Ok(Commit::Rejected(e.to_string()));
        }
        #[cfg(unix)]
        let result: Result<(bool, Option<std::fs::Metadata>), ToolError> = {
            let root = workspace.root().to_path_buf();
            let root_fd = workspace.root_fd();
            let relative = resolved
                .strip_prefix(&root)
                .map_err(|_| {
                    ToolError::Io(format!(
                        "{} is no longer below workspace",
                        resolved.display()
                    ))
                })?
                .to_path_buf();
            let temp_name = unique_temp_name(resolved);
            let content = content.to_string();
            let resource = descriptor_resource_authority(
                context,
                resolved,
                leveler_core::Capability::FilesystemWrite,
            );
            let target = resolved.to_path_buf();
            tokio::task::spawn_blocking(move || {
                descriptor_relative_create(
                    &root_fd,
                    &relative,
                    &temp_name,
                    &content,
                    permissions,
                    resource
                        .as_ref()
                        .map(|resource| (target.as_path(), resource)),
                )
            })
            .await
            .map_err(|e| ToolError::Io(format!("join descriptor create: {e}")))?
            .map_err(|e| ToolError::Io(format!("descriptor-relative create: {e}")))
            .map(|metadata| (metadata.is_some(), metadata))
        };
        #[cfg(not(unix))]
        let result: Result<(bool, Option<std::fs::Metadata>), ToolError> = {
            let parent = resolved
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let tmp = parent.join(unique_temp_name(resolved));
            let write = async {
                use tokio::io::AsyncWriteExt;
                let mut f = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)
                    .await
                    .map_err(|e| ToolError::Io(format!("create temp {}: {e}", tmp.display())))?;
                f.write_all(content.as_bytes())
                    .await
                    .map_err(|e| ToolError::Io(format!("write {}: {e}", tmp.display())))?;
                if let Some(permissions) = permissions {
                    f.set_permissions(permissions)
                        .await
                        .map_err(|e| ToolError::Io(format!("chmod {}: {e}", tmp.display())))?;
                }
                f.flush()
                    .await
                    .map_err(|e| ToolError::Io(format!("flush {}: {e}", tmp.display())))?;
                f.sync_all()
                    .await
                    .map_err(|e| ToolError::Io(format!("sync {}: {e}", tmp.display())))?;
                match tokio::fs::hard_link(&tmp, resolved).await {
                    Ok(()) => Ok(true),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                    Err(e) => Err(ToolError::Io(format!(
                        "link into {}: {e}",
                        resolved.display()
                    ))),
                }
            };
            let result = write.await;
            let _ = tokio::fs::remove_file(&tmp).await;
            result.map(|created| (created, None))
        };
        let outcome = match result {
            Ok((true, metadata)) => {
                context.execution.checkpoint.record_absent(resolved);
                let receipt = metadata.and_then(|metadata| {
                    leveler_execution::resource_identity::filesystem_resource_from_metadata(
                        resolved, &metadata,
                    )
                    .ok()
                });
                Ok(Commit::Written(receipt))
            }
            Ok((false, _)) => Ok(Commit::Stale),
            Err(e) => Err(e),
        };
        drop(lock);
        outcome
    }

    /// Remove `resolved` only while it still equals `expected`.
    pub(crate) async fn remove(
        context: &ToolContext,
        resolved: &std::path::Path,
        expected: &str,
    ) -> Result<Commit, ToolError> {
        let workspace = editing_workspace(
            context,
            resolved,
            leveler_core::Capability::FilesystemDelete,
        )?;
        let lock_path =
            leveler_project::layout::target_lock_path(&context.execution.environment, resolved);
        let lock = tokio::task::spawn_blocking({
            let lock_path = lock_path.clone();
            move || TargetLock::acquire(lock_path)
        })
        .await
        .map_err(|e| ToolError::Io(format!("join file-lock task: {e}")))?
        .map_err(|e| ToolError::Io(format!("lock {}: {e}", lock_path.display())))?;

        if let Err(e) = workspace.revalidate_delete_path(resolved, &context.write_scope()) {
            drop(lock);
            return Ok(Commit::Rejected(e.to_string()));
        }
        #[cfg(unix)]
        {
            let root = workspace.root().to_path_buf();
            let root_fd = workspace.root_fd();
            let relative = resolved
                .strip_prefix(&root)
                .map_err(|_| {
                    ToolError::Io(format!(
                        "{} is no longer below workspace",
                        resolved.display()
                    ))
                })?
                .to_path_buf();
            let expected_owned = expected.to_string();
            let resource = descriptor_resource_authority(
                context,
                resolved,
                leveler_core::Capability::FilesystemDelete,
            );
            let target = resolved.to_path_buf();
            let temp_name = unique_temp_name(resolved);
            let permissions = tokio::task::spawn_blocking(move || {
                descriptor_relative_remove(
                    &root_fd,
                    &relative,
                    &temp_name,
                    &expected_owned,
                    resource
                        .as_ref()
                        .map(|resource| (target.as_path(), resource)),
                )
            })
            .await
            .map_err(|e| ToolError::Io(format!("join descriptor remove: {e}")))?
            .map_err(|e| ToolError::Io(format!("descriptor-relative remove: {e}")))?;
            let Some((permissions, parent_metadata)) = permissions else {
                drop(lock);
                return Ok(Commit::Stale);
            };
            context.execution.checkpoint.record_captured(
                resolved,
                expected.as_bytes().to_vec(),
                permissions,
            );
            drop(lock);
            let receipt = leveler_execution::resource_identity::filesystem_resource_from_metadata(
                resolved,
                &parent_metadata,
            )
            .ok();
            Ok(Commit::Written(receipt))
        }
        #[cfg(not(unix))]
        {
            let current = match tokio::fs::read_to_string(resolved).await {
                Ok(current) => current,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    drop(lock);
                    return Ok(Commit::Stale);
                }
                Err(e) => {
                    drop(lock);
                    return Err(ToolError::Io(format!("read {}: {e}", resolved.display())));
                }
            };
            if current != expected {
                drop(lock);
                return Ok(Commit::Stale);
            }
            let permissions = tokio::fs::symlink_metadata(resolved)
                .await
                .map_err(|e| ToolError::Io(format!("stat {}: {e}", resolved.display())))?
                .permissions();
            tokio::fs::remove_file(resolved)
                .await
                .map_err(|e| ToolError::Io(format!("remove {}: {e}", resolved.display())))?;
            context.execution.checkpoint.record_captured(
                resolved,
                current.into_bytes(),
                permissions,
            );
            drop(lock);
            Ok(Commit::Written(None))
        }
    }
}

/// An unguessable sibling temp name (`.<file>.<uuid>.leveler-tmp`) so two
/// concurrent writers to the same target never collide on the staging file.
fn unique_temp_name(resolved: &std::path::Path) -> String {
    format!(
        ".{}.{}.leveler-tmp",
        resolved
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file"),
        leveler_core::new_uuid_string()
    )
}

/// Windows commit context rooted in directory handles which deny delete
/// sharing. While this value is alive, neither the workspace root nor the
/// target's parent directory can be swapped for a junction/reparse point.
#[cfg(windows)]
struct WindowsReplaceContext {
    parent: cap_std::fs::Dir,
    target_name: std::ffi::OsString,
}

#[cfg(windows)]
fn open_windows_replace_context(
    root: &cap_std::fs::Dir,
    target_relative: &std::path::Path,
) -> std::io::Result<WindowsReplaceContext> {
    let target_name = target_relative
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing file name"))?
        .to_os_string();
    if target_relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "non-normal workspace path",
        ));
    }

    let parent = match target_relative.parent() {
        Some(path) if !path.as_os_str().is_empty() => root.open_dir(path)?,
        _ => root.try_clone()?,
    };
    Ok(WindowsReplaceContext {
        parent,
        target_name,
    })
}

/// Replace a file using only a stable Windows parent-directory capability.
///
/// `cap-std` resolves every component from the held directory handle and keeps
/// Windows directory handles open without `FILE_SHARE_DELETE`. Therefore a
/// concurrent junction/symlink swap cannot redirect the final read, temporary
/// creation, or rename outside the workspace. `cap-tempfile` supplies an
/// unguessable create-new name and cleans it up on every error path.
#[cfg(windows)]
fn windows_capability_replace(
    context: WindowsReplaceContext,
    expected: &str,
    replacement: &str,
) -> std::io::Result<Option<std::fs::Permissions>> {
    use std::io::{Read, Write};

    let mut target = context.parent.open(&context.target_name)?;
    // Keep cap-std permissions for the temp write path (TempFile::as_file uses
    // cap_std::fs::Permissions). Map only the readonly bit to std for the
    // checkpoint return type used by the rest of the crate.
    let cap_permissions = target.metadata()?.permissions();
    let std_permissions = windows_std_permissions_matching(cap_permissions.readonly())?;
    let mut current = String::new();
    target.read_to_string(&mut current)?;
    if current != expected {
        return Ok(None);
    }
    drop(target);

    let mut temp = cap_tempfile::TempFile::new(&context.parent)?;
    temp.write_all(replacement.as_bytes())?;
    temp.flush()?;
    temp.as_file().set_permissions(cap_permissions)?;
    temp.as_file().sync_all()?;

    // Re-read at the commit boundary. This is cooperative CAS for normal
    // editors; the stable parent capability is the security boundary.
    let mut current = String::new();
    context
        .parent
        .open(&context.target_name)?
        .read_to_string(&mut current)?;
    if current != expected {
        return Ok(None);
    }
    temp.replace(&context.target_name)?;
    Ok(Some(std_permissions))
}

/// Build a `std::fs::Permissions` with the given readonly flag (Windows has no
/// mode bits). Uses a short-lived probe file so we never invent an invalid
/// permissions object.
#[cfg(windows)]
fn windows_std_permissions_matching(readonly: bool) -> std::io::Result<std::fs::Permissions> {
    let path = std::env::temp_dir().join(format!(
        ".leveler-perm-probe-{}.tmp",
        leveler_core::new_uuid_string()
    ));
    std::fs::write(&path, b"")?;
    let mut permissions = std::fs::metadata(&path)?.permissions();
    permissions.set_readonly(readonly);
    let _ = std::fs::remove_file(&path);
    Ok(permissions)
}

/// Commit relative to directory descriptors opened with `NOFOLLOW`. Holding
/// each parent descriptor makes later ancestor renames/symlink swaps irrelevant
/// to the final read/create/rename operations.
#[cfg(unix)]
fn validate_descriptor_resource(
    metadata: &std::fs::Metadata,
    authorization: Option<(&std::path::Path, &leveler_core::ResourceIdentity)>,
) -> std::io::Result<()> {
    if let Some((path, frozen)) = authorization {
        let actual = leveler_execution::filesystem_resource_from_metadata(path, metadata)
            .map_err(std::io::Error::other)?;
        if &actual != frozen {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "descriptor object does not match frozen resource authority",
            ));
        }
    }
    Ok(())
}

/// A filesystem object's incarnation key within one process. Used to compare
/// the object an exchange actually displaced against the object the caller
/// verified, without re-resolving the mutable directory entry.
#[cfg(unix)]
fn metadata_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

/// Identity of whatever the given name currently resolves to, never following
/// a final symlink. `None` when the object cannot be opened read-only (for
/// example a substituted symlink or a directory we must not read).
#[cfg(unix)]
fn current_identity<P: rustix::path::Arg>(
    directory: &impl std::os::fd::AsFd,
    name: P,
) -> Option<(u64, u64)> {
    use rustix::fs::{Mode, OFlags, openat};
    let fd = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let metadata = std::fs::File::from(fd).metadata().ok()?;
    Some(metadata_identity(&metadata))
}

/// Test-only rendezvous at the exact window an external process would use to
/// swap the directory entry between the identity check and the commit. Only
/// the object-binding race tests arm it, and `take()` makes it single-shot.
#[cfg(all(test, unix))]
static COMMIT_BARRIER: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> =
    std::sync::Mutex::new(None);

#[cfg(unix)]
fn run_commit_barrier() {
    #[cfg(test)]
    if let Ok(mut slot) = COMMIT_BARRIER.lock()
        && let Some(run) = slot.take()
    {
        run();
    }
}

#[cfg(unix)]
fn descriptor_relative_replace(
    root: &impl std::os::fd::AsFd,
    relative: &std::path::Path,
    temp_name: &str,
    expected: &str,
    replacement: &str,
    authorization: Option<(&std::path::Path, &leveler_core::ResourceIdentity)>,
) -> std::io::Result<Option<(std::fs::Permissions, std::fs::Metadata)>> {
    use rustix::fs::{
        AtFlags, Mode, OFlags, RenameFlags, openat, renameat, renameat_with, unlinkat,
    };
    use std::io::{Read, Write};

    let (directory, file_name) = open_relative_parent(root, relative, false)?;
    let target_fd = openat(
        &directory,
        &file_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut target = std::fs::File::from(target_fd);
    let target_metadata = target.metadata()?;
    validate_descriptor_resource(&target_metadata, authorization)?;
    let target_permissions = target_metadata.permissions();
    let mut current = String::new();
    target.read_to_string(&mut current)?;
    if current != expected {
        return Ok(None);
    }
    let approved_identity = metadata_identity(&target_metadata);

    let temp_fd = openat(
        &directory,
        temp_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let mut temp = std::fs::File::from(temp_fd);
    temp.set_permissions(target_permissions.clone())?;
    if let Err(error) = temp
        .write_all(replacement.as_bytes())
        .and_then(|_| temp.sync_all())
    {
        let _ = unlinkat(&directory, temp_name, AtFlags::empty());
        return Err(error);
    }
    let receipt_metadata = temp.metadata()?;
    let staged_identity = metadata_identity(&receipt_metadata);

    if authorization.is_none() {
        // Ordinary cooperative CAS commit: verify-then-rename under the
        // advisory target lock, with no resource binding to preserve.
        if let Err(error) = renameat(&directory, temp_name, &directory, &file_name) {
            let _ = unlinkat(&directory, temp_name, AtFlags::empty());
            return Err(error.into());
        }
        return Ok(Some((target_permissions, receipt_metadata)));
    }

    // Object-bound commit. ONE atomic exchange both publishes the staged
    // content and detaches the previous entry, and the decision is then made
    // from the object that was actually displaced. No name is re-resolved
    // between proving identity and causing the effect, so a substitution can
    // only be observed, never silently committed.
    run_commit_barrier();
    renameat_with(
        &directory,
        temp_name,
        &directory,
        &file_name,
        RenameFlags::EXCHANGE,
    )?;
    if current_identity(&directory, temp_name) == Some(approved_identity) {
        // The approved object left the name; unlinking it is exactly the
        // authorized effect.
        unlinkat(&directory, temp_name, AtFlags::empty())?;
        return Ok(Some((target_permissions, receipt_metadata)));
    }
    // The entry no longer named the approved object. Restore it and remove
    // only our own staged content; an object we did not stage and verify is
    // never unlinked.
    let _ = renameat_with(
        &directory,
        temp_name,
        &directory,
        &file_name,
        RenameFlags::EXCHANGE,
    );
    if current_identity(&directory, temp_name) == Some(staged_identity) {
        let _ = unlinkat(&directory, temp_name, AtFlags::empty());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "directory entry changed during commit; resource binding refused",
    ))
}

#[cfg(unix)]
fn descriptor_relative_create(
    root: &impl std::os::fd::AsFd,
    relative: &std::path::Path,
    temp_name: &str,
    content: &str,
    permissions: Option<std::fs::Permissions>,
    authorization: Option<(&std::path::Path, &leveler_core::ResourceIdentity)>,
) -> std::io::Result<Option<std::fs::Metadata>> {
    use rustix::fs::{AtFlags, Mode, OFlags, linkat, openat, unlinkat};
    use std::io::Write;

    // Resource grants cannot invent authority for new parent directories.
    let (directory, file_name) = open_relative_parent(root, relative, authorization.is_none())?;
    let parent_metadata = std::fs::File::from(rustix::io::dup(&directory)?).metadata()?;
    validate_descriptor_resource(&parent_metadata, authorization)?;
    let temp_fd = openat(
        &directory,
        temp_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let mut temp = std::fs::File::from(temp_fd);
    if let Some(permissions) = permissions {
        temp.set_permissions(permissions)?;
    }
    if let Err(error) = temp
        .write_all(content.as_bytes())
        .and_then(|_| temp.sync_all())
    {
        let _ = unlinkat(&directory, temp_name, AtFlags::empty());
        return Err(error);
    }
    let receipt_metadata = temp.metadata()?;
    let linked = match linkat(
        &directory,
        temp_name,
        &directory,
        &file_name,
        AtFlags::empty(),
    ) {
        Ok(()) => true,
        Err(error) if error == rustix::io::Errno::EXIST => false,
        Err(error) => {
            let _ = unlinkat(&directory, temp_name, AtFlags::empty());
            return Err(error.into());
        }
    };
    let _ = unlinkat(&directory, temp_name, AtFlags::empty());
    Ok(linked.then_some(receipt_metadata))
}

#[cfg(unix)]
fn descriptor_relative_remove(
    root: &impl std::os::fd::AsFd,
    relative: &std::path::Path,
    temp_name: &str,
    expected: &str,
    authorization: Option<(&std::path::Path, &leveler_core::ResourceIdentity)>,
) -> std::io::Result<Option<(std::fs::Permissions, std::fs::Metadata)>> {
    use rustix::fs::{
        AtFlags, Mode, OFlags, RenameFlags, openat, renameat, renameat_with, unlinkat,
    };
    use std::io::Read;

    let (directory, file_name) = open_relative_parent(root, relative, false)?;
    let target_fd = match openat(
        &directory,
        &file_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(target) => target,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut target = std::fs::File::from(target_fd);
    let target_metadata = target.metadata()?;
    validate_descriptor_resource(&target_metadata, authorization)?;
    let permissions = target_metadata.permissions();
    let mut current = String::new();
    target.read_to_string(&mut current)?;
    if current != expected {
        return Ok(None);
    }
    let parent_metadata = std::fs::File::from(rustix::io::dup(&directory)?).metadata()?;

    if authorization.is_none() {
        unlinkat(&directory, &file_name, AtFlags::empty())?;
        return Ok(Some((permissions, parent_metadata)));
    }

    // Object-bound delete: ONE atomic rename detaches the entry, and the
    // decision is made from the object that was actually detached. An entry
    // substituted after verification is restored, never unlinked.
    let approved_identity = metadata_identity(&target_metadata);
    run_commit_barrier();
    renameat(&directory, &file_name, &directory, temp_name)?;
    if current_identity(&directory, temp_name) == Some(approved_identity) {
        unlinkat(&directory, temp_name, AtFlags::empty())?;
        return Ok(Some((permissions, parent_metadata)));
    }
    // Never clobber whatever now owns the approved name; if it reappeared the
    // detached object stays at the private name rather than being destroyed.
    let _ = renameat_with(
        &directory,
        temp_name,
        &directory,
        &file_name,
        RenameFlags::NOREPLACE,
    );
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "directory entry changed during commit; resource binding refused",
    ))
}

#[cfg(unix)]
fn open_relative_parent(
    root: &impl std::os::fd::AsFd,
    relative: &std::path::Path,
    create_missing: bool,
) -> std::io::Result<(std::os::fd::OwnedFd, std::ffi::OsString)> {
    use rustix::fs::{Mode, OFlags, mkdirat, openat};

    let mut directory = rustix::io::dup(root)?;
    let mut components = relative.components().peekable();
    let mut file_name = None;
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "non-normal workspace path",
            ));
        };
        if components.peek().is_none() {
            file_name = Some(name.to_os_string());
            break;
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        directory = match openat(&directory, name, flags, Mode::empty()) {
            Ok(next) => next,
            Err(error) if create_missing && error == rustix::io::Errno::NOENT => {
                match mkdirat(
                    &directory,
                    name,
                    Mode::RUSR
                        | Mode::WUSR
                        | Mode::XUSR
                        | Mode::RGRP
                        | Mode::XGRP
                        | Mode::ROTH
                        | Mode::XOTH,
                ) {
                    Ok(()) => {}
                    Err(error) if error == rustix::io::Errno::EXIST => {}
                    Err(error) => return Err(error.into()),
                }
                openat(&directory, name, flags, Mode::empty())?
            }
            Err(error) => return Err(error.into()),
        };
    }
    let file_name = file_name.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing file name")
    })?;
    Ok((directory, file_name))
}

/// Advisory cross-process write lock for one target file, held across the
/// compare + rename commit. The lock file lives under `<leveler home>/locks/`
/// (see [`leveler_project::layout::target_lock_path`]), never in the
/// workspace.
///
/// On unix, release unlinks the path while the flock is still held, and
/// acquisition re-checks that the path still names the locked inode — a
/// waiter that locked a just-unlinked file detects the corpse and retries.
/// On other platforms the file persists (unlink-while-locked is not safe
/// there); it is a few bytes in a leveler-private directory.
struct TargetLock {
    path: std::path::PathBuf,
    _file: std::fs::File,
}

impl TargetLock {
    fn acquire(path: std::path::PathBuf) -> std::io::Result<Self> {
        use fs2::FileExt;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[allow(clippy::never_loop)]
        loop {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            file.lock_exclusive()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let held = file.metadata()?;
                match std::fs::metadata(&path) {
                    Ok(live) if live.ino() == held.ino() && live.dev() == held.dev() => {}
                    // The holder unlinked (and possibly a new waiter re-created)
                    // the path while we blocked: we locked a dead inode.
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                }
            }
            return Ok(Self { path, _file: file });
        }
    }
}

impl Drop for TargetLock {
    fn drop(&mut self) {
        // Unlink before `_file` drops (which releases the flock): waiters
        // verify inode identity after locking, so removing the path first is
        // race-free.
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.path);
        #[cfg(not(unix))]
        let _ = &self.path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows directory capabilities are opened without FILE_SHARE_DELETE.
    /// A hostile process therefore cannot rename the parent away and install
    /// a junction between our CAS read and final rename.
    #[cfg(windows)]
    #[test]
    fn windows_parent_capability_blocks_directory_swap_through_commit() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "old\n").unwrap();
        let workspace = leveler_execution::Workspace::new(root.path()).unwrap();
        let root_dir = workspace.root_dir();
        let context =
            open_windows_replace_context(&root_dir, std::path::Path::new("src/lib.rs")).unwrap();

        assert!(
            std::fs::rename(root.path().join("src"), root.path().join("src-old")).is_err(),
            "an open parent capability must deny the rename needed for a junction swap"
        );
        assert!(
            windows_capability_replace(context, "old\n", "new\n")
                .unwrap()
                .is_some(),
            "CAS replace should commit when expected content matches"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("src/lib.rs")).unwrap(),
            "new\n"
        );
    }

    /// Junctions do not require symlink developer mode and are the common
    /// Windows escape primitive. Capability traversal must refuse one whose
    /// destination is outside the workspace before opening the lock/target.
    #[cfg(windows)]
    #[test]
    fn windows_preexisting_junction_cannot_escape_workspace() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("victim.txt"), "outside\n").unwrap();
        let junction = root.path().join("escape");
        let status = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .status()
            .expect("launch mklink junction canary");
        assert!(status.success(), "mklink /J canary setup failed: {status}");

        let workspace = leveler_execution::Workspace::new(root.path()).unwrap();
        let result = open_windows_replace_context(
            &workspace.root_dir(),
            std::path::Path::new("escape/victim.txt"),
        );
        assert!(
            result.is_err(),
            "outside junction traversal must fail closed"
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("victim.txt")).unwrap(),
            "outside\n"
        );

        // Remove the junction itself, never recurse through it.
        std::fs::remove_dir(&junction).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_resource_check_rejects_swap_after_path_revalidation() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let target = parent.join("victim");
        std::fs::write(&target, "same-content").unwrap();
        let frozen = leveler_execution::resolve_filesystem_resource(&target).unwrap();
        let workspace = leveler_execution::Workspace::new(&parent).unwrap();
        // Deterministically model the gap before spawn_blocking opens the target.
        let granted = workspace
            .clone()
            .with_resource_bindings(vec![leveler_core::GrantBinding {
                capability: leveler_core::Capability::FilesystemWrite,
                resource: frozen.clone(),
            }]);
        granted
            .revalidate_write_path(&target, &leveler_execution::WriteScope::None)
            .unwrap();
        std::fs::rename(&target, parent.join("old-object")).unwrap();
        std::fs::write(&target, "same-content").unwrap();
        let replaced = descriptor_relative_replace(
            &workspace.root_fd(),
            std::path::Path::new("victim"),
            ".temp-replace",
            "same-content",
            "bad",
            Some((&target, &frozen)),
        );
        assert!(replaced.is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "same-content");
        let removed = descriptor_relative_remove(
            &workspace.root_fd(),
            std::path::Path::new("victim"),
            ".temp-delete",
            "same-content",
            Some((&target, &frozen)),
        );
        assert!(removed.is_err());
        assert!(target.exists());
        let missing = parent.join("child/new");
        std::fs::create_dir(parent.join("child")).unwrap();
        let frozen_missing = leveler_execution::resolve_filesystem_resource(&missing).unwrap();
        std::fs::rename(parent.join("child"), parent.join("old-parent")).unwrap();
        std::fs::create_dir(parent.join("child")).unwrap();
        let created = descriptor_relative_create(
            &workspace.root_fd(),
            std::path::Path::new("child/new"),
            ".temp-create",
            "bad",
            None,
            Some((&missing, &frozen_missing)),
        );
        assert!(created.is_err());
        assert!(!missing.exists());
    }

    /// Phase 3B Case A: a substitute swapped into the entry *after* the
    /// identity check must not be modified, and the commit must fail closed.
    /// The rendezvous runs at the real window a hostile process would use.
    #[cfg(unix)]
    #[test]
    fn granted_replace_refuses_a_substitute_swapped_after_verification() {
        let _serial = OBJECT_BINDING_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let target = parent.join("config.json");
        std::fs::write(&target, "approved\n").unwrap();
        let frozen = leveler_execution::resolve_filesystem_resource(&target).unwrap();
        let workspace = leveler_execution::Workspace::new(&parent).unwrap();
        let substitute = parent.join("substitute");
        std::fs::write(&substitute, "substitute\n").unwrap();
        let ran = arm_swap_substitute(substitute, target.clone());

        let result = descriptor_relative_replace(
            &workspace.root_fd(),
            std::path::Path::new("config.json"),
            ".config.json.tmp",
            "approved\n",
            "replacement\n",
            Some((&target, &frozen)),
        );
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "race window not exercised"
        );
        assert!(result.is_err(), "a substituted entry must be refused");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "substitute\n");
        assert!(
            !parent.join(".config.json.tmp").exists(),
            "staged content must be cleaned up"
        );
    }

    /// Phase 3B Case B: a substitute swapped into the entry *after* the
    /// identity check must not be deleted.
    #[cfg(unix)]
    #[test]
    fn granted_delete_refuses_a_substitute_swapped_after_verification() {
        let _serial = OBJECT_BINDING_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let target = parent.join("config.json");
        std::fs::write(&target, "approved\n").unwrap();
        let frozen = leveler_execution::resolve_filesystem_resource(&target).unwrap();
        let workspace = leveler_execution::Workspace::new(&parent).unwrap();
        let substitute = parent.join("substitute");
        std::fs::write(&substitute, "substitute\n").unwrap();
        let ran = arm_swap_substitute(substitute, target.clone());

        let result = descriptor_relative_remove(
            &workspace.root_fd(),
            std::path::Path::new("config.json"),
            ".config.json.del",
            "approved\n",
            Some((&target, &frozen)),
        );
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "race window not exercised"
        );
        assert!(result.is_err(), "a substituted entry must not be deleted");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "substitute\n");
        assert!(!parent.join(".config.json.del").exists());
    }

    /// The unracy path still publishes atomically, verifies content, and leaves
    /// no staged name behind.
    #[cfg(unix)]
    #[test]
    fn granted_replace_and_delete_commit_on_the_verified_object() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let target = parent.join("config.json");
        std::fs::write(&target, "approved\n").unwrap();
        let frozen = leveler_execution::resolve_filesystem_resource(&target).unwrap();
        let workspace = leveler_execution::Workspace::new(&parent).unwrap();

        let replaced = descriptor_relative_replace(
            &workspace.root_fd(),
            std::path::Path::new("config.json"),
            ".config.json.tmp",
            "approved\n",
            "replacement\n",
            Some((&target, &frozen)),
        );
        assert!(replaced.unwrap().is_some());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "replacement\n");
        assert!(!parent.join(".config.json.tmp").exists());

        let frozen = leveler_execution::resolve_filesystem_resource(&target).unwrap();
        let removed = descriptor_relative_remove(
            &workspace.root_fd(),
            std::path::Path::new("config.json"),
            ".config.json.del",
            "replacement\n",
            Some((&target, &frozen)),
        );
        assert!(removed.unwrap().is_some());
        assert!(!target.exists());
        assert!(!parent.join(".config.json.del").exists());
    }

    /// Phase 3B Case C: a symlink substituted for the approved file must not
    /// redirect the commit outside the workspace.
    #[cfg(unix)]
    #[test]
    fn granted_replace_refuses_a_symlink_substitution() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let target = parent.join("config.json");
        std::fs::write(&target, "approved\n").unwrap();
        let frozen = leveler_execution::resolve_filesystem_resource(&target).unwrap();
        let workspace = leveler_execution::Workspace::new(&parent).unwrap();
        let outside = parent.join("outside.txt");
        std::fs::write(&outside, "outside\n").unwrap();
        std::fs::remove_file(&target).unwrap();
        symlink(&outside, &target).unwrap();

        let result = descriptor_relative_replace(
            &workspace.root_fd(),
            std::path::Path::new("config.json"),
            ".config.json.tmp",
            "approved\n",
            "replacement\n",
            Some((&target, &frozen)),
        );
        assert!(
            result.is_err(),
            "NOFOLLOW must refuse the substituted symlink"
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "outside\n");
        assert!(!parent.join(".config.json.tmp").exists());
    }

    /// Phase 3B Case D: a parent directory renamed away and recreated with a
    /// substitute must not receive the commit. The pinned parent descriptor
    /// keeps the effect on the directory (and object) that was verified.
    #[cfg(unix)]
    #[test]
    fn granted_replace_binds_the_pinned_parent_when_the_path_is_swapped() {
        let _serial = OBJECT_BINDING_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap().join("src");
        std::fs::create_dir(&parent).unwrap();
        let target = parent.join("config.json");
        std::fs::write(&target, "approved\n").unwrap();
        let frozen = leveler_execution::resolve_filesystem_resource(&target).unwrap();
        let workspace = leveler_execution::Workspace::new(&parent).unwrap();
        let moved_parent = parent.with_file_name("src-old");
        let renamed_to = moved_parent.clone();
        let pinned_parent = parent.clone();
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        *COMMIT_BARRIER.lock().unwrap() = Some(Box::new(move || {
            std::fs::rename(&pinned_parent, &renamed_to).unwrap();
            std::fs::create_dir(&pinned_parent).unwrap();
            std::fs::write(pinned_parent.join("config.json"), "substitute\n").unwrap();
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }));

        let result = descriptor_relative_replace(
            &workspace.root_fd(),
            std::path::Path::new("config.json"),
            ".config.json.tmp",
            "approved\n",
            "replacement\n",
            Some((&target, &frozen)),
        );
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "race window not exercised"
        );
        assert!(
            result.is_ok(),
            "the verified object's own directory is still authoritative"
        );
        assert_eq!(
            std::fs::read_to_string(moved_parent.join("config.json")).unwrap(),
            "replacement\n"
        );
        assert_eq!(
            std::fs::read_to_string(parent.join("config.json")).unwrap(),
            "substitute\n"
        );
    }

    /// The object-binding barrier tests share one process-wide rendezvous.
    #[cfg(unix)]
    static OBJECT_BINDING_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Arm the commit rendezvous to move `substitute` over `target`, the exact
    /// substitution a hostile process would attempt.
    #[cfg(unix)]
    fn arm_swap_substitute(
        substitute: std::path::PathBuf,
        target: std::path::PathBuf,
    ) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        *COMMIT_BARRIER.lock().unwrap() = Some(Box::new(move || {
            std::fs::rename(&substitute, &target).unwrap();
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
        ran
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_commit_refuses_symlinked_parent_and_never_touches_outside() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!(
            "leveler-replace-race-{}",
            crate::tools::test_ordinal()
        ));
        let outside = std::env::temp_dir().join(format!(
            "leveler-replace-outside-{}",
            crate::tools::test_ordinal()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim.txt"), "outside").unwrap();
        symlink(&outside, root.join("src")).unwrap();
        let workspace = leveler_execution::Workspace::new(&root).unwrap();
        let root_fd = workspace.root_fd();

        let result = descriptor_relative_replace(
            &root_fd,
            std::path::Path::new("src/victim.txt"),
            ".victim.tmp",
            "outside",
            "compromised",
            None,
        );
        assert!(
            result.is_err(),
            "NOFOLLOW traversal must reject the swapped parent"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("victim.txt")).unwrap(),
            "outside"
        );
        assert!(!outside.join(".victim.tmp").exists());
        std::fs::remove_file(root.join("src")).ok();
        std::fs::remove_dir_all(root).ok();
        std::fs::remove_dir_all(outside).ok();
    }
}
