//! `replace` — exact string replacement in one file.
//!
//! A targeted alternative to `apply_patch` for find-and-replace edits (e.g.
//! renames). The model gives an exact `old` string and its `new` replacement;
//! no surrounding-context hunks, so weak models don't thrash on context matching.
//! `replace_all` renames every occurrence in ONE call instead of one patch each.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const DESCRIPTION: &str = "Replace an exact string in a file. Use this only for a literal rename \
or exact text copied verbatim from a recent read; use apply_patch for structural edits that add, \
remove, or reshape lines. Give `path`, the exact `old` text (including whitespace/indentation), \
and the `new` replacement. By default `old` must occur exactly once; set `replace_all` to true \
to replace every occurrence in one call.";

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Path to the file, relative to the workspace root.
    path: String,
    /// The exact text to find (verbatim, including whitespace).
    old: String,
    /// The replacement text.
    new: String,
    /// Replace every occurrence. Default false: `old` must occur exactly once.
    #[serde(default)]
    replace_all: bool,
}

pub struct ReplaceTool;

#[async_trait]
impl Tool for ReplaceTool {
    fn name(&self) -> &'static str {
        "replace"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::WorkspaceWrite
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;

        if input.old.is_empty() {
            return Ok(ToolOutput::error("`old` must not be empty"));
        }
        let resolved = match context.workspace.resolve(&input.path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e.to_string())),
        };

        let existing = match tokio::fs::read_to_string(&resolved).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput::error(format!(
                    "cannot edit missing file: {}",
                    input.path
                )));
            }
            Err(e) => return Err(ToolError::Io(format!("read {}: {e}", input.path))),
        };

        // The `old` text was chosen against contents the model read. If the file
        // moved on since, refuse and make the model re-read (mirrors apply_patch).
        if context
            .file_state
            .is_stale(&input.path, existing.as_bytes())
        {
            return Ok(ToolOutput::error(format!(
                "{} changed since you read it — re-read it and redo the replace against \
                 current contents.",
                input.path
            )));
        }

        let count = existing.matches(&input.old).count();
        if count == 0 {
            let head = format!(
                "`old` string not found in {}. It must match verbatim, including whitespace \
                 and indentation.",
                input.path
            );
            // "Not found" alone sends the model back to guess the same way twice.
            // Show it the file at the line it anchored on.
            return Ok(ToolOutput::error(
                match crate::tools::locate_hint::real_text_at_anchor(&existing, &input.old, 3) {
                    Some(hint) => format!("{head}\n{hint}"),
                    None => format!("{head}\nNone of those lines exist in the file — re-read it."),
                },
            ));
        }
        if input.old == input.new {
            return Ok(ToolOutput::ok(format!(
                "No change: `old` and `new` are identical in {}",
                input.path
            ))
            .with_metadata(serde_json::json!({ "outcome": "no_change" })));
        }
        if count > 1 && !input.replace_all {
            return Ok(ToolOutput::error(format!(
                "`old` occurs {count} times in {} — ambiguous. Add surrounding context to make \
                 it unique, or set replace_all=true to change every occurrence.",
                input.path
            )));
        }

        let new_content = if input.replace_all {
            existing.replace(&input.old, &input.new)
        } else {
            existing.replacen(&input.old, &input.new, 1)
        };

        // Serialize CodeLeveler writers for this target. The lock is advisory,
        // cross-platform, and held across the final compare + rename, making
        // the CAS real for concurrent replace invocations (including separate
        // CodeLeveler processes).
        context.checkpoint.record(&resolved);
        let parent = resolved
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let lock_path = parent.join(format!(
            ".{}.leveler-lock",
            resolved
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
        ));
        #[cfg(unix)]
        let lock = tokio::task::spawn_blocking({
            let root = context.workspace.root().to_path_buf();
            let relative = resolved
                .strip_prefix(&root)
                .map_err(|_| ToolError::Io("target left workspace".into()))?
                .to_path_buf();
            let lock_name = lock_path.file_name().unwrap_or_default().to_os_string();
            move || open_descriptor_lock(&root, &relative, &lock_name)
        })
        .await
        .map_err(|e| ToolError::Io(format!("join file-lock task: {e}")))?
        .map_err(|e| ToolError::Io(format!("lock {}: {e}", lock_path.display())))?;
        #[cfg(windows)]
        let lock = tokio::task::spawn_blocking({
            let root = context.workspace.root().to_path_buf();
            let root_dir = context.workspace.root_dir();
            let relative = resolved
                .strip_prefix(&root)
                .map_err(|_| ToolError::Io("target left workspace".into()))?
                .to_path_buf();
            let lock_name = lock_path.file_name().unwrap_or_default().to_os_string();
            move || open_windows_replace_context(&root_dir, &relative, &lock_name)
        })
        .await
        .map_err(|e| ToolError::Io(format!("join file-lock task: {e}")))?
        .map_err(|e| ToolError::Io(format!("lock {}: {e}", lock_path.display())))?;
        #[cfg(all(not(unix), not(windows)))]
        let lock = tokio::task::spawn_blocking({
            let lock_path = lock_path.clone();
            move || -> std::io::Result<std::fs::File> {
                use fs2::FileExt;
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(lock_path)?;
                file.lock_exclusive()?;
                Ok(file)
            }
        })
        .await
        .map_err(|e| ToolError::Io(format!("join file-lock task: {e}")))?
        .map_err(|e| ToolError::Io(format!("lock {}: {e}", lock_path.display())))?;

        if let Err(e) = context.workspace.revalidate_write_path(&resolved) {
            drop(lock);
            return Ok(ToolOutput::error(e.to_string()));
        }
        #[cfg(not(windows))]
        let unique = format!(
            ".{}.{}.leveler-tmp",
            resolved
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file"),
            std::process::id() as u64
                ^ std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0)
        );
        #[cfg(unix)]
        {
            let root = context.workspace.root().to_path_buf();
            let root_fd = context.workspace.root_fd();
            let relative = resolved
                .strip_prefix(&root)
                .map_err(|_| {
                    ToolError::Io(format!(
                        "{} is no longer below workspace",
                        resolved.display()
                    ))
                })?
                .to_path_buf();
            let expected = existing.clone();
            let replacement = new_content.clone();
            let committed = tokio::task::spawn_blocking(move || {
                descriptor_relative_replace(&root_fd, &relative, &unique, &expected, &replacement)
            })
            .await
            .map_err(|e| ToolError::Io(format!("join descriptor write: {e}")))?
            .map_err(|e| ToolError::Io(format!("descriptor-relative replace: {e}")))?;
            if !committed {
                return Ok(ToolOutput::error(format!(
                    "{} changed since you read it — re-read it and redo the replace against current contents.",
                    input.path
                )));
            }
        }
        #[cfg(windows)]
        {
            let expected = existing.clone();
            let replacement = new_content.clone();
            let committed = tokio::task::spawn_blocking(move || {
                windows_capability_replace(lock, &expected, &replacement)
            })
            .await
            .map_err(|e| ToolError::Io(format!("join capability write: {e}")))?
            .map_err(|e| ToolError::Io(format!("capability-relative replace: {e}")))?;
            if !committed {
                return Ok(ToolOutput::error(format!(
                    "{} changed since you read it — re-read it and redo the replace against current contents.",
                    input.path
                )));
            }
        }
        #[cfg(all(not(unix), not(windows)))]
        {
            let tmp = parent.join(unique);
            // create_new: fail if the name somehow collides instead of overwriting.
            {
                use tokio::io::AsyncWriteExt;
                let mut f = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)
                    .await
                    .map_err(|e| ToolError::Io(format!("create temp {}: {e}", tmp.display())))?;
                f.write_all(new_content.as_bytes())
                    .await
                    .map_err(|e| ToolError::Io(format!("write {}: {e}", tmp.display())))?;
                f.flush()
                    .await
                    .map_err(|e| ToolError::Io(format!("flush {}: {e}", tmp.display())))?;
            }
            // Re-verify the target still matches the content we planned against.
            match tokio::fs::read_to_string(&resolved).await {
                Ok(current) if current == existing => {}
                Ok(_) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Ok(ToolOutput::error(format!(
                        "{} changed since you read it — re-read it and redo the replace against \
                     current contents.",
                        input.path
                    )));
                }
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(ToolError::Io(format!("re-read {}: {e}", input.path)));
                }
            }
            if let Err(e) = tokio::fs::rename(&tmp, &resolved).await {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(ToolError::Io(format!(
                    "rename into {}: {e}",
                    resolved.display()
                )));
            }
        }
        #[cfg(not(windows))]
        drop(lock);

        // Re-fingerprint so our own edit isn't seen as an outside change next time.
        context
            .file_state
            .record(&input.path, new_content.as_bytes());
        // Auto-format the edited file (best-effort; re-fingerprints internally).
        super::format::format_after_edit(&context, &input.path, &resolved).await;

        // Report the write: the executor folds `modified_files` into the turn's
        // change set, and the engine gates verification on it. Without this an
        // edit made here is invisible and the run finishes unverified.
        Ok(ToolOutput::ok(format!(
            "Replaced {count} occurrence{} in {}",
            if count == 1 { "" } else { "s" },
            input.path
        ))
        .with_metadata(serde_json::json!({ "modified_files": [input.path] })))
    }
}

/// Windows commit context rooted in directory handles which deny delete
/// sharing. While this value is alive, neither the workspace root nor the
/// target's parent directory can be swapped for a junction/reparse point.
#[cfg(windows)]
struct WindowsReplaceContext {
    parent: cap_std::fs::Dir,
    target_name: std::ffi::OsString,
    _lock: std::fs::File,
}

#[cfg(windows)]
fn open_windows_replace_context(
    root: &cap_std::fs::Dir,
    target_relative: &std::path::Path,
    lock_name: &std::ffi::OsStr,
) -> std::io::Result<WindowsReplaceContext> {
    use fs2::FileExt;

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
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    let lock = parent.open_with(lock_name, &options)?.into_std();
    lock.lock_exclusive()?;
    Ok(WindowsReplaceContext {
        parent,
        target_name,
        _lock: lock,
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
) -> std::io::Result<bool> {
    use std::io::{Read, Write};

    let mut target = context.parent.open(&context.target_name)?;
    let permissions = target.metadata()?.permissions();
    let mut current = String::new();
    target.read_to_string(&mut current)?;
    if current != expected {
        return Ok(false);
    }
    drop(target);

    let mut temp = cap_tempfile::TempFile::new(&context.parent)?;
    temp.write_all(replacement.as_bytes())?;
    temp.flush()?;
    temp.as_file().set_permissions(permissions)?;
    temp.as_file().sync_all()?;

    // Re-read at the commit boundary. This is cooperative CAS for normal
    // editors; the stable parent capability is the security boundary.
    let mut current = String::new();
    context
        .parent
        .open(&context.target_name)?
        .read_to_string(&mut current)?;
    if current != expected {
        return Ok(false);
    }
    temp.replace(&context.target_name)?;
    Ok(true)
}

/// Commit relative to directory descriptors opened with `NOFOLLOW`. Holding
/// each parent descriptor makes later ancestor renames/symlink swaps irrelevant
/// to the final read/create/rename operations.
#[cfg(unix)]
fn descriptor_relative_replace(
    root: &impl std::os::fd::AsFd,
    relative: &std::path::Path,
    temp_name: &str,
    expected: &str,
    replacement: &str,
) -> std::io::Result<bool> {
    use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
    use std::io::{Read, Write};

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
        directory = openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
    }
    let file_name = file_name.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing file name")
    })?;
    let target_fd = openat(
        &directory,
        &file_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut target = std::fs::File::from(target_fd);
    let target_permissions = target.metadata()?.permissions();
    let mut current = String::new();
    target.read_to_string(&mut current)?;
    if current != expected {
        return Ok(false);
    }

    let temp_fd = openat(
        &directory,
        temp_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let mut temp = std::fs::File::from(temp_fd);
    temp.set_permissions(target_permissions)?;
    if let Err(error) = temp
        .write_all(replacement.as_bytes())
        .and_then(|_| temp.sync_all())
    {
        let _ = unlinkat(&directory, temp_name, AtFlags::empty());
        return Err(error);
    }
    drop(temp);
    if let Err(error) = renameat(&directory, temp_name, &directory, &file_name) {
        let _ = unlinkat(&directory, temp_name, AtFlags::empty());
        return Err(error.into());
    }
    Ok(true)
}

#[cfg(unix)]
fn open_descriptor_lock(
    root: &std::path::Path,
    target_relative: &std::path::Path,
    lock_name: &std::ffi::OsStr,
) -> std::io::Result<std::fs::File> {
    use fs2::FileExt;
    use rustix::fs::{Mode, OFlags, open, openat};
    let mut directory = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut components = target_relative.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        let std::path::Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "non-normal workspace path",
            ));
        };
        directory = openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
    }
    let fd = loop {
        match openat(
            &directory,
            lock_name,
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => break fd,
            Err(rustix::io::Errno::NOENT) => match openat(
                &directory,
                lock_name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            ) {
                Ok(fd) => break fd,
                Err(rustix::io::Errno::EXIST) => continue,
                Err(error) => return Err(error.into()),
            },
            Err(error) => return Err(error.into()),
        }
    };
    let file = std::fs::File::from(fd);
    file.lock_exclusive()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(contents: &str) -> (ToolContext, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("leveler-replace-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), contents).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        (
            ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted),
            dir,
        )
    }

    async fn run(ctx: ToolContext, args: serde_json::Value) -> ToolOutput {
        ReplaceTool
            .execute(args, ctx, CancellationToken::new())
            .await
            .unwrap()
    }

    /// The executor learns a file changed ONLY from a tool's `modified_files`
    /// metadata; the engine gates verification on that list. `apply_patch`
    /// reports it, `replace` did not — so a model that edited via `replace`
    /// sailed past the verification gate and the run was declared
    /// CompletedUnverified with real, unverified edits on disk. (Caught by the
    /// L1 P0 smoke run: ts-t1-01.)
    #[tokio::test]
    async fn a_successful_replace_reports_the_file_it_modified() {
        let (c, dir) = ctx("alpha beta gamma\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "beta", "new": "BETA"}),
        )
        .await;

        assert!(!out.is_error, "should succeed: {}", out.content);
        let modified = out
            .metadata
            .get("modified_files")
            .and_then(|v| v.as_array())
            .expect("a write tool must report modified_files, or verification is skipped");
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0].as_str(), Some("src/lib.rs"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A refused replace changed nothing — it must not claim it did.
    #[tokio::test]
    async fn a_refused_replace_reports_no_modified_file() {
        let (c, dir) = ctx("hello world\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "zzz", "new": "q"}),
        )
        .await;

        assert!(out.is_error);
        assert!(
            out.metadata.get("modified_files").is_none(),
            "a failed edit must not report a modified file"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn identical_old_and_new_is_a_successful_noop() {
        let (c, dir) = ctx("hello world\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "hello", "new": "hello"}),
        )
        .await;

        assert!(!out.is_error, "a valid no-op is not an execution failure");
        assert!(out.content.to_lowercase().contains("no change"));
        assert!(out.metadata.get("modified_files").is_none());
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "hello world\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn description_reserves_replace_for_literal_renames() {
        assert!(DESCRIPTION.contains("literal rename"), "{DESCRIPTION}");
        assert!(DESCRIPTION.contains("apply_patch"), "{DESCRIPTION}");
        assert!(
            !DESCRIPTION.contains("Prefer this over apply_patch"),
            "{DESCRIPTION}"
        );
    }

    #[tokio::test]
    async fn replaces_a_unique_occurrence() {
        let (c, dir) = ctx("alpha beta gamma\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "beta", "new": "BETA"}),
        )
        .await;
        assert!(!out.is_error, "should succeed: {}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "alpha BETA gamma\n"
        );
    }

    #[tokio::test]
    async fn replace_all_renames_every_occurrence() {
        let (c, dir) = ctx("old() old() old()\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "old", "new": "renamed", "replace_all": true}),
        )
        .await;
        assert!(!out.is_error, "should succeed: {}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "renamed() renamed() renamed()\n"
        );
    }

    #[tokio::test]
    async fn ambiguous_without_replace_all_is_refused() {
        let (c, dir) = ctx("x x\n");
        let out = run(
            c.clone(),
            serde_json::json!({"path": "src/lib.rs", "old": "x", "new": "y"}),
        )
        .await;
        assert!(out.is_error, "ambiguous replace must be refused");
        assert!(
            out.content.contains("2") || out.content.to_lowercase().contains("occur"),
            "error names the ambiguity: {}",
            out.content
        );
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "x x\n"
        );
    }

    #[tokio::test]
    async fn missing_old_string_is_refused() {
        let (c, dir) = ctx("hello world\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "zzz", "new": "q"}),
        )
        .await;
        assert!(out.is_error, "not-found must be refused");
        assert!(
            out.content.to_lowercase().contains("not found") || out.content.contains("zzz"),
            "error explains: {}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "hello world\n"
        );
    }

    #[tokio::test]
    async fn refuses_a_path_outside_the_workspace() {
        let (c, _dir) = ctx("data\n");
        let out = run(
            c,
            serde_json::json!({"path": "../escape.txt", "old": "data", "new": "x"}),
        )
        .await;
        assert!(out.is_error, "must reject a path outside the workspace");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_replaces_cannot_both_commit_from_the_same_version() {
        let (first, dir) = ctx("value = old\n");
        let second = ToolContext::new(
            leveler_execution::Workspace::new(&dir).unwrap(),
            leveler_execution::PermissionProfile::Assisted,
        );
        let gate = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let launch = |context: ToolContext,
                      replacement: &'static str,
                      gate: std::sync::Arc<tokio::sync::Barrier>| async move {
            gate.wait().await;
            run(
                context,
                serde_json::json!({
                    "path": "src/lib.rs", "old": "old", "new": replacement
                }),
            )
            .await
        };
        let a = tokio::spawn(launch(first, "first", gate.clone()));
        let b = tokio::spawn(launch(second, "second", gate.clone()));
        gate.wait().await;
        let (a, b) = (a.await.unwrap(), b.await.unwrap());

        assert_ne!(
            a.is_error, b.is_error,
            "exactly one stale writer must be rejected"
        );
        let final_text = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert!(matches!(
            final_text.as_str(),
            "value = first\n" | "value = second\n"
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

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
        let context = open_windows_replace_context(
            &root_dir,
            std::path::Path::new("src/lib.rs"),
            std::ffi::OsStr::new(".lib.rs.leveler-lock"),
        )
        .unwrap();

        assert!(
            std::fs::rename(root.path().join("src"), root.path().join("src-old")).is_err(),
            "an open parent capability must deny the rename needed for a junction swap"
        );
        assert!(windows_capability_replace(context, "old\n", "new\n").unwrap());
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
            std::ffi::OsStr::new(".victim.txt.leveler-lock"),
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
    fn descriptor_commit_refuses_symlinked_parent_and_never_touches_outside() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!(
            "leveler-replace-race-{}",
            super::super::test_ordinal()
        ));
        let outside = std::env::temp_dir().join(format!(
            "leveler-replace-outside-{}",
            super::super::test_ordinal()
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

    #[tokio::test]
    async fn a_missing_old_string_shows_what_the_file_really_contains() {
        let (c, dir) = ctx("fn main() {\n    let total = a + b;\n    println!(\"{total}\");\n}\n");
        // The model approximates the line it read: right anchor, wrong body.
        let out = run(
            c,
            serde_json::json!({
                "path": "src/lib.rs",
                "old": "    let total = a+b;",
                "new": "    let total = a.checked_add(b)?;",
            }),
        )
        .await;

        assert!(out.is_error);
        assert!(
            out.content.contains("2\u{2502}     let total = a + b;"),
            "the error must show the real line, with its number, so the model can copy it \
             back verbatim instead of guessing again; got:\n{}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
