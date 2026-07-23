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
        cancellation: CancellationToken,
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

        let exact = existing.matches(&input.old).count();
        let (new_content, count, fuzzy) = if exact == 0 {
            // Exact match failed. Try a conservative fuzzy fallback (trailing
            // whitespace + typographic-Unicode tolerance, mapped back to the
            // real byte span so indentation is preserved) before giving up, so a
            // tiny drift in the model's `old` doesn't force a retry. A no-op
            // (`old == new`) skips it.
            match (input.old != input.new)
                .then(|| fuzzy_replace(&existing, &input.old, &input.new, input.replace_all))
                .flatten()
            {
                Some((content, n)) => (content, n, true),
                None => {
                    let head = format!(
                        "`old` string not found in {}. It must match verbatim, including whitespace \
                         and indentation.",
                        input.path
                    );
                    // "Not found" alone sends the model back to guess the same way
                    // twice. Show it the file at the line it anchored on.
                    return Ok(ToolOutput::error(
                        match crate::tools::locate_hint::real_text_at_anchor(
                            &existing, &input.old, 3,
                        ) {
                            Some(hint) => format!("{head}\n{hint}"),
                            None => {
                                format!(
                                    "{head}\nNone of those lines exist in the file — re-read it."
                                )
                            }
                        },
                    ));
                }
            }
        } else {
            if input.old == input.new {
                return Ok(ToolOutput::ok(format!(
                    "No change: `old` and `new` are identical in {}",
                    input.path
                ))
                .with_metadata(serde_json::json!({ "outcome": "no_change" })));
            }
            if exact > 1 && !input.replace_all {
                return Ok(ToolOutput::error(format!(
                    "`old` occurs {exact} times in {} — ambiguous. Add surrounding context to make \
                     it unique, or set replace_all=true to change every occurrence.",
                    input.path
                )));
            }
            let content = if input.replace_all {
                existing.replace(&input.old, &input.new)
            } else {
                existing.replacen(&input.old, &input.new, 1)
            };
            (content, exact, false)
        };

        match commit_replace(&context, &resolved, &existing, &new_content).await? {
            Commit::Written => {}
            Commit::Stale => {
                return Ok(ToolOutput::error(format!(
                    "{} changed since you read it — re-read it and redo the replace against current contents.",
                    input.path
                )));
            }
            Commit::Rejected(message) => return Ok(ToolOutput::error(message)),
        }

        // Re-fingerprint so our own edit isn't seen as an outside change next time.
        context
            .file_state
            .record(&input.path, new_content.as_bytes());
        // Auto-format the edited file (best-effort; re-fingerprints internally).
        super::format::format_after_edit(&context, &input.path, &resolved, &cancellation).await;

        // Report the write: the executor folds `modified_files` into the turn's
        // change set, and the engine gates verification on it. Without this an
        // edit made here is invisible and the run finishes unverified.
        let note = if fuzzy {
            " (located with whitespace/quote tolerance — the file's exact text \
             differed slightly from `old`)"
        } else {
            ""
        };
        Ok(ToolOutput::ok(format!(
            "Replaced {count} occurrence{} in {}{note}",
            if count == 1 { "" } else { "s" },
            input.path
        ))
        .with_metadata(serde_json::json!({ "modified_files": [input.path] })))
    }
}

/// Fold typographic Unicode punctuation to its ASCII equivalent so a patch
/// authored in plain ASCII still matches file text containing fancy quotes,
/// dashes, or exotic spaces. Mirrors the `apply_patch` seek normalizer; the
/// mapping is 1 char → 1 char so match positions stay aligned to the original.
fn fold_char(c: char) -> char {
    match c {
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
        | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
        | '\u{3000}' => ' ',
        other => other,
    }
}

/// Normalize `s` for fuzzy matching: fold typographic punctuation and drop
/// whitespace that trails a line (before `\n` or EOF). Returns the normalized
/// chars paired with each one's index in the original `char` stream, so a match
/// in normalized space can be mapped back to the exact original span (trailing
/// whitespace that was dropped still lives inside that span and is replaced too).
fn normalize_indexed(s: &str) -> (Vec<char>, Vec<usize>) {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    let mut map = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\n' && c.is_whitespace() {
            // Skip this whitespace run if nothing but whitespace remains before
            // the next newline / EOF (i.e. it is trailing).
            let mut j = i;
            while j < chars.len() && chars[j] != '\n' && chars[j].is_whitespace() {
                j += 1;
            }
            if j == chars.len() || chars[j] == '\n' {
                i = j;
                continue;
            }
        }
        out.push(fold_char(c));
        map.push(i);
        i += 1;
    }
    (out, map)
}

/// Locate `old` in `existing` under [`normalize_indexed`] tolerance and return
/// the rewritten content plus how many occurrences were replaced. Returns `None`
/// when there is no fuzzy match, or when there are several and `all` is false
/// (ambiguous — refuse rather than guess). Replaces the exact original byte
/// spans, so indentation and untouched characters are preserved verbatim.
fn fuzzy_replace(existing: &str, old: &str, new: &str, all: bool) -> Option<(String, usize)> {
    let (nx, mx) = normalize_indexed(existing);
    let (no, _) = normalize_indexed(old);
    if no.is_empty() || no.len() > nx.len() {
        return None;
    }
    let mut starts = Vec::new();
    let mut i = 0;
    while i + no.len() <= nx.len() {
        if nx[i..i + no.len()] == no[..] {
            starts.push(i);
            i += no.len(); // non-overlapping
        } else {
            i += 1;
        }
    }
    if starts.is_empty() || (starts.len() > 1 && !all) {
        return None;
    }
    // Char-index → byte-offset table for the original (plus a terminal len).
    let mut byte_of: Vec<usize> = existing.char_indices().map(|(b, _)| b).collect();
    byte_of.push(existing.len());
    let mut out = existing.to_string();
    // Rewrite from the last match backward so earlier byte offsets stay valid.
    for &s in starts.iter().rev() {
        let start_byte = byte_of[mx[s]];
        let end_byte = byte_of[mx[s + no.len() - 1] + 1];
        out.replace_range(start_byte..end_byte, new);
    }
    Some((out, starts.len()))
}

/// Outcome of a locked commit. `Stale` and `Rejected` wrote nothing; the caller
/// turns each into the right model-facing message (CAS staleness vs. a path that
/// left the workspace), so both `replace` and `apply_patch` phrase it identically.
pub(crate) enum Commit {
    Written,
    Stale,
    Rejected(String),
}

/// Lock the target, verify it still equals `expected`, and atomically replace it
/// with `replacement`. Returns `Stale` (writing nothing) if the on-disk content
/// diverged since `expected` was read.
///
/// This is the one commit path shared by `replace` and `apply_patch`: an
/// advisory cross-process lock (under the leveler home, never in the workspace)
/// held across the compare + rename, an unguessable temp name, and — on unix and
/// Windows — a capability/descriptor-relative write that a concurrent
/// junction/symlink swap cannot redirect outside the workspace.
pub(crate) async fn commit_replace(
    context: &ToolContext,
    resolved: &std::path::Path,
    expected: &str,
    replacement: &str,
) -> Result<Commit, ToolError> {
    let lock_path = leveler_project::layout::target_lock_path(&context.environment, resolved);
    let lock = tokio::task::spawn_blocking({
        let lock_path = lock_path.clone();
        move || TargetLock::acquire(lock_path)
    })
    .await
    .map_err(|e| ToolError::Io(format!("join file-lock task: {e}")))?
    .map_err(|e| ToolError::Io(format!("lock {}: {e}", lock_path.display())))?;

    if let Err(e) = context.workspace.revalidate_write_path(resolved) {
        drop(lock);
        return Ok(Commit::Rejected(e.to_string()));
    }
    #[cfg(not(windows))]
    let unique = unique_temp_name(resolved);
    let committed_permissions: Option<std::fs::Permissions>;
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
        let expected = expected.to_string();
        let replacement = replacement.to_string();
        committed_permissions = tokio::task::spawn_blocking(move || {
            descriptor_relative_replace(&root_fd, &relative, &unique, &expected, &replacement)
        })
        .await
        .map_err(|e| ToolError::Io(format!("join descriptor write: {e}")))?
        .map_err(|e| ToolError::Io(format!("descriptor-relative replace: {e}")))?;
    }
    #[cfg(windows)]
    {
        let root = context.workspace.root().to_path_buf();
        let root_dir = context.workspace.root_dir();
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
            context
                .checkpoint
                .record_captured(resolved, expected.as_bytes().to_vec(), permissions);
            Ok(Commit::Written)
        }
        None => Ok(Commit::Stale),
    }
}

/// Atomically create `resolved` only if it is still absent. The staged file is
/// linked into place with no-overwrite semantics, so an Add/Move destination
/// created by another writer is reported as [`Commit::Stale`].
pub(crate) async fn commit_create(
    context: &ToolContext,
    resolved: &std::path::Path,
    content: &str,
) -> Result<Commit, ToolError> {
    commit_create_with_permissions(context, resolved, content, None).await
}

pub(crate) async fn commit_create_with_permissions(
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
    let lock_path = leveler_project::layout::target_lock_path(&context.environment, resolved);
    let lock = tokio::task::spawn_blocking({
        let lock_path = lock_path.clone();
        move || TargetLock::acquire(lock_path)
    })
    .await
    .map_err(|e| ToolError::Io(format!("join file-lock task: {e}")))?
    .map_err(|e| ToolError::Io(format!("lock {}: {e}", lock_path.display())))?;

    if let Err(e) = context.workspace.revalidate_write_path(resolved) {
        drop(lock);
        return Ok(Commit::Rejected(e.to_string()));
    }
    #[cfg(unix)]
    let result: Result<bool, ToolError> = {
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
        let temp_name = unique_temp_name(resolved);
        let content = content.to_string();
        tokio::task::spawn_blocking(move || {
            descriptor_relative_create(&root_fd, &relative, &temp_name, &content, permissions)
        })
        .await
        .map_err(|e| ToolError::Io(format!("join descriptor create: {e}")))?
        .map_err(|e| ToolError::Io(format!("descriptor-relative create: {e}")))
    };
    #[cfg(not(unix))]
    let result: Result<bool, ToolError> = {
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
        result
    };
    let outcome = match result {
        Ok(true) => {
            context.checkpoint.record_absent(resolved);
            Ok(Commit::Written)
        }
        Ok(false) => Ok(Commit::Stale),
        Err(e) => Err(e),
    };
    drop(lock);
    outcome
}

/// Remove `resolved` only while it still equals `expected`.
pub(crate) async fn commit_remove(
    context: &ToolContext,
    resolved: &std::path::Path,
    expected: &str,
) -> Result<Commit, ToolError> {
    let lock_path = leveler_project::layout::target_lock_path(&context.environment, resolved);
    let lock = tokio::task::spawn_blocking({
        let lock_path = lock_path.clone();
        move || TargetLock::acquire(lock_path)
    })
    .await
    .map_err(|e| ToolError::Io(format!("join file-lock task: {e}")))?
    .map_err(|e| ToolError::Io(format!("lock {}: {e}", lock_path.display())))?;

    if let Err(e) = context.workspace.revalidate_write_path(resolved) {
        drop(lock);
        return Ok(Commit::Rejected(e.to_string()));
    }
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
        let expected_owned = expected.to_string();
        let permissions = tokio::task::spawn_blocking(move || {
            descriptor_relative_remove(&root_fd, &relative, &expected_owned)
        })
        .await
        .map_err(|e| ToolError::Io(format!("join descriptor remove: {e}")))?
        .map_err(|e| ToolError::Io(format!("descriptor-relative remove: {e}")))?;
        let Some(permissions) = permissions else {
            drop(lock);
            return Ok(Commit::Stale);
        };
        context
            .checkpoint
            .record_captured(resolved, expected.as_bytes().to_vec(), permissions);
        drop(lock);
        Ok(Commit::Written)
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
        context
            .checkpoint
            .record_captured(resolved, current.into_bytes(), permissions);
        drop(lock);
        Ok(Commit::Written)
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
    let permissions = target.metadata()?.permissions();
    let mut current = String::new();
    target.read_to_string(&mut current)?;
    if current != expected {
        return Ok(None);
    }
    drop(target);

    let mut temp = cap_tempfile::TempFile::new(&context.parent)?;
    temp.write_all(replacement.as_bytes())?;
    temp.flush()?;
    temp.as_file().set_permissions(permissions.clone())?;
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
    Ok(Some(permissions))
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
) -> std::io::Result<Option<std::fs::Permissions>> {
    use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
    use std::io::{Read, Write};

    let (directory, file_name) = open_relative_parent(root, relative, false)?;
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
        return Ok(None);
    }

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
    drop(temp);
    if let Err(error) = renameat(&directory, temp_name, &directory, &file_name) {
        let _ = unlinkat(&directory, temp_name, AtFlags::empty());
        return Err(error.into());
    }
    Ok(Some(target_permissions))
}

#[cfg(unix)]
fn descriptor_relative_create(
    root: &impl std::os::fd::AsFd,
    relative: &std::path::Path,
    temp_name: &str,
    content: &str,
    permissions: Option<std::fs::Permissions>,
) -> std::io::Result<bool> {
    use rustix::fs::{AtFlags, Mode, OFlags, linkat, openat, unlinkat};
    use std::io::Write;

    let (directory, file_name) = open_relative_parent(root, relative, true)?;
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
    drop(temp);
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
    Ok(linked)
}

#[cfg(unix)]
fn descriptor_relative_remove(
    root: &impl std::os::fd::AsFd,
    relative: &std::path::Path,
    expected: &str,
) -> std::io::Result<Option<std::fs::Permissions>> {
    use rustix::fs::{AtFlags, Mode, OFlags, openat, unlinkat};
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
    let permissions = target.metadata()?.permissions();
    let mut current = String::new();
    target.read_to_string(&mut current)?;
    if current != expected {
        return Ok(None);
    }
    unlinkat(&directory, &file_name, AtFlags::empty())?;
    Ok(Some(permissions))
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

    /// The write lock must never touch the workspace: no `.leveler-lock`
    /// file may appear next to the target, and (on unix) the global lock
    /// file under `<home>/locks/` is unlinked once the replace completes.
    #[tokio::test]
    async fn replace_leaves_no_lock_files_behind() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-replace-lockfree-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "value = old\n").unwrap();
        let home = dir.join("leveler-home");
        let env = std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os().chain([(
                std::ffi::OsString::from("LEVELER_HOME"),
                home.clone().into_os_string(),
            )]),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        ));
        let c = ToolContext::with_environment(
            leveler_execution::Workspace::new(&dir).unwrap(),
            leveler_execution::PermissionProfile::Assisted,
            env,
        );
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "old", "new": "new"}),
        )
        .await;
        assert!(!out.is_error, "replace failed: {}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "value = new\n"
        );

        let residue: Vec<String> = std::fs::read_dir(dir.join("src"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("leveler-lock"))
            .collect();
        assert!(residue.is_empty(), "lock residue in workspace: {residue:?}");

        #[cfg(unix)]
        {
            let leftover: Vec<std::path::PathBuf> = std::fs::read_dir(home.join("locks"))
                .map(|it| it.filter_map(Result::ok).map(|e| e.path()).collect())
                .unwrap_or_default();
            assert!(
                leftover.is_empty(),
                "lock files must be unlinked on release: {leftover:?}"
            );
        }
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
        let context =
            open_windows_replace_context(&root_dir, std::path::Path::new("src/lib.rs")).unwrap();

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

    #[test]
    fn fuzzy_replace_tolerates_smart_quotes_and_trailing_ws_and_preserves_indent() {
        // Smart quotes in the file, ASCII in `old`: fuzzy locates it, and the
        // replacement keeps the line's leading indentation.
        let file = "    let title = \u{201C}Deploy\u{201D};\n";
        let (out, n) = fuzzy_replace(
            file,
            "let title = \"Deploy\";",
            "let title = \"Ship\";",
            false,
        )
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(out, "    let title = \"Ship\";\n");

        // `old` carries trailing whitespace the file lacks (byte-exact would
        // fail): the fuzzy pass strips it on both sides and still matches,
        // replacing only the real text.
        let file2 = "value = 1\nnext\n";
        let (out2, _) = fuzzy_replace(file2, "value = 1   ", "value = 2", false).unwrap();
        assert_eq!(out2, "value = 2\nnext\n");

        // Genuinely absent text yields no fuzzy match.
        assert!(fuzzy_replace("alpha\n", "zeta", "q", false).is_none());
    }

    #[tokio::test]
    async fn replace_falls_back_to_fuzzy_on_smart_quote_drift() {
        let (c, dir) = ctx("let title = \u{201C}Deploy\u{201D};\n");
        let out = run(
            c,
            serde_json::json!({
                "path": "src/lib.rs",
                "old": "let title = \"Deploy\";",
                "new": "let title = \"Ship\";",
            }),
        )
        .await;
        assert!(
            !out.is_error,
            "fuzzy should locate the smart-quote drift: {}",
            out.content
        );
        assert!(
            out.content.contains("tolerance"),
            "should flag the fuzzy match: {}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "let title = \"Ship\";\n"
        );
        std::fs::remove_dir_all(&dir).ok();
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
