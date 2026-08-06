use std::path::Path;

use leveler_client_protocol::{UiDiff, UiDiffFile};

/// Compute the working-tree diff vs HEAD via git — staged AND unstaged, the
/// same yardstick as the web Git panel, so the two views never contradict.
/// `with_patch` also loads each file's unified diff hunk.
///
/// Untracked files are included as whole-file additions. `git diff` cannot see
/// them, and leaving them out made the review surface report 无改动 right after
/// the agent created a file — the single most common thing it does. Ignored
/// paths stay out: they are not review material.
pub(crate) fn compute_diff(repo: &Path, with_patch: bool) -> UiDiff {
    let numstat =
        leveler_core::git_stdout(repo, &["diff", "--numstat", "HEAD", "--"]).unwrap_or_default();
    let mut files = Vec::new();
    for line in numstat.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() == 3 {
            let added = parts[0].parse().unwrap_or(0);
            let removed = parts[1].parse().unwrap_or(0);
            let path = parts[2].to_string();
            let patch = with_patch.then(|| {
                leveler_core::git_stdout(repo, &["diff", "HEAD", "--", &path]).unwrap_or_default()
            });
            files.push(UiDiffFile {
                path,
                added,
                removed,
                patch,
            });
        }
    }
    files.extend(untracked_files(repo, with_patch));
    UiDiff { files }
}

/// New files the user has not staged yet, rendered as additions.
///
/// `--exclude-standard` applies .gitignore; `-z` keeps paths with spaces or
/// newlines intact instead of splitting them into phantom entries.
fn untracked_files(repo: &Path, with_patch: bool) -> Vec<UiDiffFile> {
    let listing =
        leveler_core::git_stdout(repo, &["ls-files", "--others", "--exclude-standard", "-z"])
            .unwrap_or_default();

    listing
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| {
            // `--no-index` against /dev/null is how git itself renders a
            // file with no index entry. It exits 1 for "differs", so this must
            // go through the diff-aware runner or the output is discarded.
            let patch = leveler_core::git_diff_stdout(
                repo,
                &["diff", "--no-index", "--numstat", "--", "/dev/null", path],
            )
            .unwrap_or_default();
            let added = patch
                .lines()
                .next()
                .and_then(|line| line.split('\t').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            UiDiffFile {
                path: path.to_string(),
                added,
                removed: 0,
                patch: with_patch.then(|| {
                    leveler_core::git_diff_stdout(
                        repo,
                        &["diff", "--no-index", "--", "/dev/null", path],
                    )
                    .unwrap_or_default()
                }),
            }
        })
        .collect()
}

/// Current branch label for the TUI header (`main`, `main*` when dirty, or
/// `detached@abc1234`). `None` when the path is not a git work tree.
pub(crate) fn detect_branch_label(repo: &Path) -> Option<String> {
    let name =
        leveler_core::git_stdout(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let label = if name == "HEAD" {
        let sha =
            leveler_core::git_stdout(repo, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
        let sha = sha.trim();
        if sha.is_empty() {
            return None;
        }
        format!("detached@{sha}")
    } else {
        name.to_string()
    };
    let dirty = !leveler_core::git_stdout(repo, &["status", "--porcelain"])
        .unwrap_or_default()
        .trim()
        .is_empty();
    if dirty {
        Some(format!("{label}*"))
    } else {
        Some(label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// The 改动 panel must use the same yardstick as the Git panel: the full
    /// working-tree diff vs HEAD. `git diff` without HEAD hides staged-but-
    /// uncommitted changes, so the two views contradict each other.
    #[test]
    fn compute_diff_includes_staged_changes() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-diff-staged-{}",
            std::process::id() as u64 * 173 + 99
        ));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);

        // Stage a modification without committing.
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        git(&dir, &["add", "a.txt"]);

        let diff = compute_diff(&dir, true);
        assert_eq!(
            diff.files.len(),
            1,
            "staged change must be visible: {diff:?}"
        );
        assert_eq!(diff.files[0].path, "a.txt");
        assert_eq!(diff.files[0].added, 1);
        assert!(
            diff.files[0]
                .patch
                .as_deref()
                .is_some_and(|p| p.contains("+two")),
            "patch must carry the staged hunk: {:?}",
            diff.files[0].patch
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Creating a file is the most common thing the agent does, and `git diff`
    /// does not see untracked paths — so the review surface said 无改动 while
    /// the agent's whole output sat in the working tree. Anything the user
    /// would have to review must be listed.
    #[test]
    fn compute_diff_includes_untracked_files() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-diff-untracked-{}",
            std::process::id() as u64 * 271 + 13
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);

        std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
        git(&dir, &["add", ".gitignore"]);
        git(&dir, &["commit", "-q", "-m", "ignore"]);

        std::fs::write(dir.join("NOTES.md"), "line one\nline two\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "noise\n").unwrap();

        let diff = compute_diff(&dir, true);
        let paths: Vec<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&"NOTES.md"),
            "a new untracked file must be listed: {paths:?}"
        );
        assert!(
            !paths.contains(&"ignored.txt"),
            "gitignored paths are not review material: {paths:?}"
        );

        let notes = diff.files.iter().find(|f| f.path == "NOTES.md").unwrap();
        assert_eq!(notes.added, 2, "every line of a new file is an addition");
        assert_eq!(notes.removed, 0);
        assert!(
            notes
                .patch
                .as_deref()
                .is_some_and(|p| p.contains("+line one") && p.contains("+line two")),
            "patch must carry the new file's contents: {:?}",
            notes.patch
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
