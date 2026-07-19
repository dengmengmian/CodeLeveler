//! Workspace snapshots for command-driven mutations (spec §19).
//!
//! Edits made by tools go through [`crate::Checkpoint`], but a subprocess can
//! create/modify/delete files the checkpoint never saw (`git reset --hard`, a
//! script deleting files). For git workspaces we capture the whole tree
//! (tracked + untracked, `.gitignore` respected) as a git tree object via a
//! temporary index — cheap, content-addressed, and it survives a process crash
//! because the object lives in `.git/objects`. Non-git workspaces cannot be
//! snapshotted; callers must say so instead of silently losing rollback.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A captured workspace state: a git tree object id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotId(pub String);

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Errors from snapshot operations.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("git failed: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Captures and restores git-workspace snapshots.
pub struct WorkspaceSnapshot;

impl WorkspaceSnapshot {
    /// Capture the working tree (tracked + untracked, ignoring `.gitignore`d
    /// files) as a tree object. Returns `Ok(None)` when `root` is not inside a
    /// git repository (or git is unavailable) — the caller must surface that
    /// the command's changes will not be recoverable.
    pub async fn capture(root: &Path) -> Result<Option<SnapshotId>, SnapshotError> {
        let Some(git_dir) = git_dir(root).await else {
            return Ok(None);
        };
        // Reuse a persistent per-repo index rather than a fresh temp one. A
        // fresh (empty) index forces `git add -A` to re-hash every tracked file
        // on every snapshot — O(repo size) per command (0.3s on a 5k-file repo,
        // and this runs twice per command). A persistent index carries git's
        // stat cache, so `git add -A` only re-hashes files that actually
        // changed (measured ~7-10x faster). run_command is serial, so a single
        // shared index is safe.
        let index = git_dir.join("leveler").join("snapshot.index");
        if let Some(parent) = index.parent() {
            std::fs::create_dir_all(parent)?;
        }
        git(root, &["add", "-A", "."], Some(&index)).await?;
        let sha = git(root, &["write-tree"], Some(&index)).await?;
        Ok(Some(SnapshotId(sha.trim().to_string())))
    }

    /// Restore the working tree to `id`: rewrite every file in the snapshot
    /// and delete files that did not exist in it. The repository's real index
    /// and HEAD are left untouched.
    pub async fn restore(root: &Path, id: &SnapshotId) -> Result<(), SnapshotError> {
        let index = temp_index_path();
        let result = async {
            git(root, &["read-tree", &id.0], Some(&index)).await?;
            git(root, &["checkout-index", "-a", "-f"], Some(&index)).await?;

            // Delete files that exist now but were not in the snapshot. The
            // current set comes from tracked + untracked (ignored files are
            // left alone, same as capture).
            let in_snapshot: std::collections::HashSet<String> =
                git(root, &["ls-tree", "-r", "--name-only", &id.0], None)
                    .await?
                    .lines()
                    .map(str::to_string)
                    .collect();
            let current = git(
                root,
                &["ls-files", "--cached", "--others", "--exclude-standard"],
                None,
            )
            .await?;
            for path in current.lines() {
                if !in_snapshot.contains(path) {
                    let _ = std::fs::remove_file(root.join(path));
                }
            }
            Ok(())
        }
        .await;
        let _ = std::fs::remove_file(&index);
        result
    }

    /// The paths that changed between `id` and the current working tree.
    pub async fn changed_since(root: &Path, id: &SnapshotId) -> Result<Vec<String>, SnapshotError> {
        let Some(now) = Self::capture(root).await? else {
            return Err(SnapshotError::Git(
                "workspace is no longer a git repository".to_string(),
            ));
        };
        if now == *id {
            return Ok(Vec::new());
        }
        let out = git(
            root,
            &["diff-tree", "-r", "--name-only", &id.0, &now.0],
            None,
        )
        .await?;
        // Build tools mark their output dirs with CACHEDIR.TAG (cargo's
        // target/, restic, …). Artifacts under such dirs are incidental to
        // running a build/test command, not workspace mutations — reporting
        // them breaks scope checks and file budgets with hundreds of paths.
        let mut cache_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let changed: Vec<String> = out
            .lines()
            .filter(|path| !in_cache_dir(root, Path::new(path), &mut cache_dirs))
            .map(str::to_string)
            .collect();
        Ok(changed)
    }

    /// Persist `id` under `.git` so a crashed run can still be rolled back by
    /// hand (`git read-tree <sha> && git checkout-index -af`).
    pub async fn persist_last(root: &Path, id: &SnapshotId) -> Result<(), SnapshotError> {
        let Some(git_dir) = git_dir(root).await else {
            return Err(SnapshotError::Git("not a git repository".to_string()));
        };
        let dir = git_dir.join("leveler");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("last-command-snapshot"), format!("{}\n", id.0))?;
        Ok(())
    }
}

/// The repository's `.git` directory, or `None` when `root` is not a git
/// repository (or git is unavailable — logged, not swallowed).
async fn git_dir(root: &Path) -> Option<PathBuf> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(root);
    scrub_credentials(&mut command);
    let out = command.output().await;
    match out {
        Ok(out) if out.status.success() => {
            let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Some(PathBuf::from(dir))
        }
        Ok(_) => None,
        Err(error) => {
            tracing::warn!("git unavailable, workspace snapshots disabled: {error}");
            None
        }
    }
}

/// Run git in `root`, optionally against a scratch index, returning stdout.
async fn git(root: &Path, args: &[&str], index: Option<&Path>) -> Result<String, SnapshotError> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args).current_dir(root);
    scrub_credentials(&mut cmd);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let out = cmd
        .output()
        .await
        .map_err(|e| SnapshotError::Git(format!("failed to run git {args:?}: {e}")))?;
    if !out.status.success() {
        return Err(SnapshotError::Git(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn scrub_credentials(command: &mut tokio::process::Command) {
    // Rebuild from the immutable application snapshot. Removing only known
    // names from the live parent is racy: a credential added after startup
    // would otherwise be inherited by git hooks and clean/smudge filters.
    command.env_clear();
    for (name, value) in leveler_core::environment().vars_os() {
        let is_credential = name.to_str().is_some_and(crate::is_credential_env_name);
        if !is_credential {
            command.env(name, value);
        }
    }
}

/// Whether `path` (relative to `root`) lies inside a directory marked with
/// CACHEDIR.TAG, memoizing positive hits. The tag file itself counts too.
fn in_cache_dir(
    root: &Path,
    path: &Path,
    cache_dirs: &mut std::collections::HashSet<PathBuf>,
) -> bool {
    let mut dir = PathBuf::new();
    let mut components: Vec<_> = path.components().collect();
    // The last component is the file itself; its parents are candidate dirs.
    components.pop();
    for component in components {
        dir.push(component);
        if cache_dirs.contains(&dir) {
            return true;
        }
        if root.join(&dir).join("CACHEDIR.TAG").is_file() {
            cache_dirs.insert(dir.clone());
            return true;
        }
    }
    // A change to the tag file itself is also cache noise.
    path.file_name().is_some_and(|name| name == "CACHEDIR.TAG")
}

static TEMP_ORDINAL: AtomicU64 = AtomicU64::new(0);

/// A unique temp path for a scratch git index.
fn temp_index_path() -> PathBuf {
    leveler_core::environment()
        .temp_dir()
        .to_path_buf()
        .join(format!(
            "leveler-snap-index-{}-{}",
            std::process::id(),
            TEMP_ORDINAL.fetch_add(1, Ordering::Relaxed)
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sh(root: &Path, script: &str) {
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(root)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "script failed: {script}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    async fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // No autocrlf: the runner's global config must not smudge file content
        // on checkout/restore (CRLF breaks byte-exact assertions).
        sh(
            dir.path(),
            "git init -q && git config user.email t@t && git config user.name t && git config core.autocrlf false",
        )
        .await;
        dir
    }

    #[tokio::test]
    async fn capture_uses_a_persistent_index_and_stays_correct_on_reuse() {
        // The persistent index (git's stat cache) is the performance fix; this
        // guards that reusing it does not corrupt capture results.
        let dir = git_repo().await;
        sh(
            dir.path(),
            "echo one > a.txt && git add -A && git commit -qm init",
        )
        .await;

        let first = WorkspaceSnapshot::capture(dir.path())
            .await
            .unwrap()
            .unwrap();
        // The index is persisted under .git/leveler, not a throwaway temp file.
        assert!(
            dir.path().join(".git/leveler/snapshot.index").is_file(),
            "capture must reuse a persistent per-repo index"
        );

        // An unchanged tree re-captures to the SAME tree (reused index is sound).
        let again = WorkspaceSnapshot::capture(dir.path())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, again);

        // A change after reuse is still detected against the reused index.
        sh(dir.path(), "echo two > a.txt && echo x > b.txt").await;
        let mut changed = WorkspaceSnapshot::changed_since(dir.path(), &first)
            .await
            .unwrap();
        changed.sort();
        assert_eq!(changed, vec!["a.txt".to_string(), "b.txt".to_string()]);
    }

    #[tokio::test]
    async fn capture_and_restore_recovers_deletes_and_creates() {
        let dir = git_repo().await;
        sh(
            dir.path(),
            "echo original > file.txt && git add -A && git commit -qm init",
        )
        .await;
        // An untracked file present at capture time must be restored too.
        sh(dir.path(), "echo untracked > extra.txt").await;

        let snap = WorkspaceSnapshot::capture(dir.path())
            .await
            .unwrap()
            .expect("git repo must be snapshottable");

        sh(
            dir.path(),
            "rm file.txt extra.txt && echo new > created.txt",
        )
        .await;
        WorkspaceSnapshot::restore(dir.path(), &snap).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("file.txt")).unwrap(),
            "original\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("extra.txt")).unwrap(),
            "untracked\n"
        );
        assert!(
            !dir.path().join("created.txt").exists(),
            "files created after the snapshot must be removed on restore"
        );
    }

    #[tokio::test]
    async fn changed_since_reports_command_mutations() {
        let dir = git_repo().await;
        sh(
            dir.path(),
            "echo one > a.txt && git add -A && git commit -qm init",
        )
        .await;
        let snap = WorkspaceSnapshot::capture(dir.path())
            .await
            .unwrap()
            .unwrap();

        sh(dir.path(), "echo two > a.txt && echo x > b.txt").await;
        let mut changed = WorkspaceSnapshot::changed_since(dir.path(), &snap)
            .await
            .unwrap();
        changed.sort();
        assert_eq!(changed, vec!["a.txt".to_string(), "b.txt".to_string()]);
    }

    #[tokio::test]
    async fn cache_directories_are_not_reported_as_mutations() {
        // Build tools mark their output dirs with CACHEDIR.TAG (cargo's
        // target/, restic, etc.). A command that runs `cargo test` must not
        // pollute modified_files with hundreds of artifacts — they break
        // scope checks and file budgets.
        let dir = git_repo().await;
        sh(
            dir.path(),
            "echo one > a.txt && git add -A && git commit -qm init",
        )
        .await;
        let snap = WorkspaceSnapshot::capture(dir.path())
            .await
            .unwrap()
            .unwrap();

        sh(
            dir.path(),
            "mkdir -p target/deps &&              printf 'Signature: 8a477f597d28d172789f06886806bc55' > target/CACHEDIR.TAG &&              echo obj > target/deps/x.o && echo two > a.txt",
        )
        .await;
        let changed = WorkspaceSnapshot::changed_since(dir.path(), &snap)
            .await
            .unwrap();
        assert_eq!(
            changed,
            vec!["a.txt".to_string()],
            "cache-dir artifacts must be filtered out"
        );
    }

    #[tokio::test]
    async fn unchanged_tree_reports_nothing() {
        let dir = git_repo().await;
        sh(
            dir.path(),
            "echo one > a.txt && git add -A && git commit -qm init",
        )
        .await;
        let snap = WorkspaceSnapshot::capture(dir.path())
            .await
            .unwrap()
            .unwrap();
        let changed = WorkspaceSnapshot::changed_since(dir.path(), &snap)
            .await
            .unwrap();
        assert!(changed.is_empty());
    }

    #[tokio::test]
    async fn non_git_dir_is_not_snapshottable() {
        let dir = tempfile::tempdir().unwrap();
        let snap = WorkspaceSnapshot::capture(dir.path()).await.unwrap();
        assert!(snap.is_none());
    }

    #[tokio::test]
    async fn persist_last_writes_under_git_dir() {
        let dir = git_repo().await;
        sh(
            dir.path(),
            "echo one > a.txt && git add -A && git commit -qm init",
        )
        .await;
        let snap = WorkspaceSnapshot::capture(dir.path())
            .await
            .unwrap()
            .unwrap();
        WorkspaceSnapshot::persist_last(dir.path(), &snap)
            .await
            .unwrap();
        let recorded =
            std::fs::read_to_string(dir.path().join(".git/leveler/last-command-snapshot")).unwrap();
        assert_eq!(recorded.trim(), snap.0);
    }
}
