use std::path::Path;

use leveler_client_protocol::{UiDiff, UiDiffFile};

/// Why the working-tree diff could not be produced. This is a different fact
/// from `Loaded` with zero files: the review surface must never present
/// "no changes" as the answer when the answer was never established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiffError {
    /// The directory is not inside a git work tree (or git is unavailable).
    NotARepository,
    /// Git ran but the change set could not be read.
    Git(String),
}

impl std::fmt::Display for DiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiffError::NotARepository => {
                write!(f, "not a git work tree: cannot read workspace changes")
            }
            DiffError::Git(message) => write!(f, "{message}"),
        }
    }
}

/// Compute the working-tree diff vs HEAD via git — staged AND unstaged, the
/// same yardstick as the web Git panel, so the two views never contradict.
/// `with_patch` also loads each file's unified diff hunk.
///
/// Process count is O(1) in the number of changed files, not O(files): tracked
/// files come from one `--numstat` call plus one multi-file `git diff HEAD`
/// (split on `diff --git` boundaries), and untracked files are rendered as new-
/// file patches in-process. The previous shape ran 1–2 git processes per
/// changed path, which made a 1500-file workspace take minutes.
///
/// Untracked files are included as whole-file additions. `git diff` cannot see
/// them, and leaving them out made the review surface report 无改动 right after
/// the agent created a file — the single most common thing it does. Ignored
/// paths stay out: they are not review material.
pub(crate) fn compute_diff(repo: &Path, with_patch: bool) -> Result<UiDiff, DiffError> {
    // `ls-files` doubles as the "is this a git work tree" probe: it succeeds in
    // a fresh repository with no commits, unlike `git diff HEAD`. Its failure is
    // a real error, not an empty change set.
    let listing =
        leveler_core::git_stdout(repo, &["ls-files", "--others", "--exclude-standard", "-z"])
            .ok_or(DiffError::NotARepository)?;
    let mut files = tracked_files(repo, with_patch)?;
    files.extend(untracked_files(repo, &listing, with_patch));
    Ok(UiDiff { files })
}

/// Tracked changes vs HEAD, batched: one `--numstat` for the authoritative file
/// list and counts, one `git diff HEAD` for every patch body. The two outputs
/// enumerate the same files in the same order, so patch section `i` belongs to
/// numstat entry `i`. If the counts differ for any reason the code falls back to
/// per-file `git diff`, so a surprise can slow one call down but can never pair
/// a file with the wrong patch.
fn tracked_files(repo: &Path, with_patch: bool) -> Result<Vec<UiDiffFile>, DiffError> {
    let Some(numstat) = leveler_core::git_stdout(repo, &["diff", "--numstat", "HEAD", "--"]) else {
        // An unborn HEAD (fresh repo, no commits) has no tracked changes yet;
        // untracked files are still listed, so this is not a failure.
        return Ok(Vec::new());
    };
    let entries: Vec<(String, u32, u32)> = numstat
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            (parts.len() == 3).then(|| {
                (
                    parts[2].to_string(),
                    parts[0].parse().unwrap_or(0),
                    parts[1].parse().unwrap_or(0),
                )
            })
        })
        .collect();

    if !with_patch {
        return Ok(entries
            .into_iter()
            .map(|(path, added, removed)| UiDiffFile {
                path,
                added,
                removed,
                patch: None,
            })
            .collect());
    }

    let full = leveler_core::git_stdout(repo, &["diff", "HEAD", "--"])
        .ok_or_else(|| DiffError::Git("git diff HEAD failed".to_string()))?;
    let sections = split_file_patches(&full);
    if sections.len() != entries.len() {
        return Ok(entries
            .into_iter()
            .map(|(path, added, removed)| {
                let patch = leveler_core::git_stdout(repo, &["diff", "HEAD", "--", &path]);
                UiDiffFile {
                    path,
                    added,
                    removed,
                    patch,
                }
            })
            .collect());
    }
    Ok(entries
        .into_iter()
        .zip(sections)
        .map(|((path, added, removed), patch)| UiDiffFile {
            path,
            added,
            removed,
            patch: Some(patch),
        })
        .collect())
}

/// Split a multi-file unified diff into one string per file. A section starts at
/// each `diff --git ` line; patch body lines are always prefixed with `+`, `-`
/// or a space, so a diff line inside a hunk can never be mistaken for a header.
fn split_file_patches(diff: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// New files the user has not staged yet, rendered as additions.
///
/// The listing came from `ls-files --others --exclude-standard -z`, so paths
/// with spaces or newlines stay intact. The patch body is built in-process
/// rather than shelling out to `git diff --no-index` once per file.
fn untracked_files(repo: &Path, listing: &str, with_patch: bool) -> Vec<UiDiffFile> {
    listing
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| {
            let absolute = repo.join(path);
            let (added, patch) = match std::fs::read(&absolute) {
                Ok(bytes) => {
                    let added = if is_binary(&bytes) {
                        0
                    } else {
                        count_lines(&bytes)
                    };
                    let patch = with_patch.then(|| new_file_patch(path, &absolute, &bytes));
                    (added, patch)
                }
                // An unreadable path is listed but has no readable content;
                // git's own `--no-index` run failed the same way and yielded
                // an empty body.
                Err(_) => (0, with_patch.then(String::new)),
            };
            UiDiffFile {
                path: path.to_string(),
                added,
                removed: 0,
                patch,
            }
        })
        .collect()
}

/// Git's own binary heuristic: a NUL byte in the first 8000 bytes.
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&byte| byte == 0)
}

/// Number of lines git's numstat would report for a new text file.
fn count_lines(bytes: &[u8]) -> u32 {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|&&byte| byte == b'\n').count() as u32;
    if bytes.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    }
}

/// Render a new file the way `git diff --no-index /dev/null <path>` does:
/// `new file mode`, `--- /dev/null`, `+++ b/<path>`, and a single all-added
/// hunk. The `index` blob-hash line is omitted — it carries no change
/// information, and no consumer here needs it (the web parser skips meta lines).
fn new_file_patch(path: &str, absolute: &Path, bytes: &[u8]) -> String {
    let mode = file_mode(absolute);
    let mut out = format!("diff --git a/{path} b/{path}\nnew file mode {mode}\n");
    if is_binary(bytes) {
        out.push_str(&format!("Binary files /dev/null and b/{path} differ\n"));
        return out;
    }
    if bytes.is_empty() {
        return out;
    }
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    out.push_str("--- /dev/null\n");
    out.push_str(&format!("+++ b/{path}\n"));
    out.push_str(&format!("@@ -0,0 +1,{} @@\n", lines.len()));
    for line in &lines {
        out.push('+');
        out.push_str(line.strip_suffix('\n').unwrap_or(line));
        out.push('\n');
    }
    if !text.ends_with('\n') {
        out.push_str("\\ No newline at end of file\n");
    }
    out
}

fn file_mode(path: &Path) -> &'static str {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path)
            && metadata.permissions().mode() & 0o111 != 0
        {
            return "100755";
        }
    }
    let _ = path;
    "100644"
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

        let diff = compute_diff(&dir, true).expect("diff computed");
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

        let diff = compute_diff(&dir, true).expect("diff computed");
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

    /// The batched path pairs `git diff HEAD` sections with `--numstat` entries
    /// by order. This locks that pairing: a swap would put one file's body under
    /// another file's name, which is worse than being slow.
    #[test]
    fn batched_tracked_patches_are_paired_with_the_right_files() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-diff-batch-{}",
            std::process::id() as u64 * 419 + 7
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        for (name, body) in [
            ("a.txt", "base-a\n"),
            ("b.txt", "base-b\n"),
            ("c.txt", "base-c\n"),
        ] {
            std::fs::write(dir.join(name), body).unwrap();
        }
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("a.txt"), "base-a\nmarker-aaa\n").unwrap();
        std::fs::write(dir.join("b.txt"), "base-b\nmarker-bbb\n").unwrap();
        std::fs::write(dir.join("c.txt"), "base-c\nmarker-ccc\n").unwrap();

        let diff = compute_diff(&dir, true).expect("diff computed");
        assert_eq!(diff.files.len(), 3, "{diff:?}");
        for (name, marker) in [
            ("a.txt", "+marker-aaa"),
            ("b.txt", "+marker-bbb"),
            ("c.txt", "+marker-ccc"),
        ] {
            let file = diff.files.iter().find(|f| f.path == name).expect(name);
            assert_eq!(file.added, 1, "{name}: {file:?}");
            assert!(
                file.patch.as_deref().is_some_and(|p| p.contains(marker)),
                "{name} must carry its own body: {:?}",
                file.patch
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// New-file patches are built in-process now, so their format must match
    /// what `git diff --no-index /dev/null <path>` produced for the edge cases
    /// the review surface actually hits: binary, empty, no trailing newline,
    /// and paths with spaces.
    #[test]
    fn new_file_patch_handles_binary_empty_and_missing_newline() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-diff-newfile-{}",
            std::process::id() as u64 * 541 + 23
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);

        std::fs::write(dir.join("notes.md"), "line one\nline two\n").unwrap();
        std::fs::write(dir.join("no_newline.txt"), "no newline at end").unwrap();
        std::fs::write(dir.join("empty.txt"), "").unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3, 0, 9]).unwrap();
        std::fs::write(dir.join("with space.txt"), "spaced\n").unwrap();

        let diff = compute_diff(&dir, true).expect("diff computed");
        let find = |name: &str| {
            diff.files
                .iter()
                .find(|f| f.path == name)
                .unwrap_or_else(|| panic!("missing {name}: {:?}", diff.files))
        };

        let notes = find("notes.md");
        assert_eq!(notes.added, 2);
        let patch = notes.patch.as_deref().unwrap();
        assert!(patch.contains("new file mode 100644"), "{patch}");
        assert!(patch.contains("--- /dev/null"), "{patch}");
        assert!(patch.contains("+++ b/notes.md"), "{patch}");
        assert!(patch.contains("@@ -0,0 +1,2 @@"), "{patch}");
        assert!(patch.contains("+line one\n+line two\n"), "{patch}");

        let no_newline = find("no_newline.txt");
        assert_eq!(no_newline.added, 1);
        let patch = no_newline.patch.as_deref().unwrap();
        assert!(patch.contains("+no newline at end\n"), "{patch}");
        assert!(patch.contains("\\ No newline at end of file"), "{patch}");

        let empty = find("empty.txt");
        assert_eq!(empty.added, 0);
        let patch = empty.patch.as_deref().unwrap();
        assert!(patch.contains("new file mode 100644"), "{patch}");
        assert!(!patch.contains("@@"), "an empty file has no hunk: {patch}");

        let binary = find("bin.dat");
        assert_eq!(binary.added, 0, "binary additions are not line counts");
        assert!(
            binary
                .patch
                .as_deref()
                .is_some_and(|p| p.contains("Binary files")),
            "{:?}",
            binary.patch
        );

        let spaced = find("with space.txt");
        assert_eq!(spaced.added, 1, "paths with spaces stay intact");
        assert!(
            spaced
                .patch
                .as_deref()
                .is_some_and(|p| p.contains("+spaced"))
        );

        // `with_patch = false` still lists and counts, with no patch bodies.
        let listing = compute_diff(&dir, false).expect("diff computed");
        assert_eq!(listing.files.len(), diff.files.len());
        assert!(listing.files.iter().all(|f| f.patch.is_none()));
        let notes = listing.files.iter().find(|f| f.path == "notes.md").unwrap();
        assert_eq!(notes.added, 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A directory that is not a work tree must fail loudly, not report clean.
    #[test]
    fn non_repository_is_an_error_not_an_empty_diff() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-diff-nonrepo-{}",
            std::process::id() as u64 * 617 + 31
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let result = compute_diff(&dir, true);
        assert_eq!(result, Err(DiffError::NotARepository));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Manual scale benchmark: N untracked files must not cost N git processes.
    /// Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "manual performance harness; run with --ignored --nocapture"]
    fn perf_workspace_diff_scale() {
        for count in [1usize, 10, 100, 500, 1500] {
            let dir = std::env::temp_dir()
                .join(format!("leveler-diff-scale-{}-{count}", std::process::id()));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            git(&dir, &["init", "-q"]);
            std::fs::write(dir.join("seed.txt"), "seed\n").unwrap();
            git(&dir, &["add", "."]);
            git(&dir, &["commit", "-q", "-m", "init"]);
            std::fs::create_dir_all(dir.join("gen")).unwrap();
            for i in 0..count {
                std::fs::write(
                    dir.join(format!("gen/file_{i}.txt")),
                    format!("line {i}\nsecond {i}\n"),
                )
                .unwrap();
            }
            let started = std::time::Instant::now();
            let diff = compute_diff(&dir, true).expect("diff computed");
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            println!(
                "untracked={count:5} files={:5} wall_ms={elapsed:8.1} processes=3",
                diff.files.len()
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    /// The same code against the actual CodeLeveler work tree — the real dirty
    /// workspace, not a synthetic one. Also proves the whole repo reads without
    /// a failure. Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "manual real-repo check; run with --ignored --nocapture"]
    fn perf_real_repository_diff() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let started = std::time::Instant::now();
        let diff = compute_diff(&repo, true).expect("real repo diff must not fail");
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        let patched = diff.files.iter().filter(|f| f.patch.is_some()).count();
        println!(
            "real repo: files={} patched={patched} wall_ms={elapsed:.1} processes=3",
            diff.files.len()
        );
        assert!(
            !diff.files.is_empty(),
            "the work tree is dirty in this session"
        );
    }
}
