use std::path::Path;
use std::process::Command;

use leveler_verifier::CheckStatus;

use leveler_client_protocol::{UiCompletionReport, UiDiff, UiDiffFile};

/// Compute the working-tree diff vs HEAD via git — staged AND unstaged, the
/// same yardstick as the web Git panel, so the two views never contradict.
/// `with_patch` also loads each file's unified diff hunk. Untracked new files
/// are not listed (they are absent from `git diff`); this is a known
/// limitation of the summary.
pub(crate) fn compute_diff(repo: &Path, with_patch: bool) -> UiDiff {
    let numstat = run_git(repo, &["diff", "--numstat", "HEAD", "--"]);
    let mut files = Vec::new();
    for line in numstat.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() == 3 {
            let added = parts[0].parse().unwrap_or(0);
            let removed = parts[1].parse().unwrap_or(0);
            let path = parts[2].to_string();
            let patch = with_patch.then(|| run_git(repo, &["diff", "HEAD", "--", &path]));
            files.push(UiDiffFile {
                path,
                added,
                removed,
                patch,
            });
        }
    }
    UiDiff { files }
}

fn run_git(repo: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command.args(args).current_dir(repo);
    command.env_clear();
    for (name, value) in leveler_core::environment().vars_os() {
        if !name
            .to_str()
            .is_some_and(leveler_execution::is_credential_env_name)
        {
            command.env(name, value);
        }
    }
    command
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Current branch label for the TUI header (`main`, `main*` when dirty, or
/// `detached@abc1234`). `None` when the path is not a git work tree.
pub(crate) fn detect_branch_label(repo: &Path) -> Option<String> {
    let name = run_git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let label = if name == "HEAD" {
        let sha = run_git(repo, &["rev-parse", "--short", "HEAD"]);
        let sha = sha.trim();
        if sha.is_empty() {
            return None;
        }
        format!("detached@{sha}")
    } else {
        name.to_string()
    };
    let dirty = !run_git(repo, &["status", "--porcelain"]).trim().is_empty();
    if dirty {
        Some(format!("{label}*"))
    } else {
        Some(label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

pub(crate) fn build_report(
    report: &leveler_engine::TaskReport,
    diff: &UiDiff,
) -> UiCompletionReport {
    let (passed, total) = report
        .verification
        .as_ref()
        .map(|v| {
            (
                v.checks
                    .iter()
                    .filter(|c| c.status == CheckStatus::Passed)
                    .count(),
                v.checks.len(),
            )
        })
        .unwrap_or((0, 0));
    UiCompletionReport {
        files_changed: report.modified_files.len(),
        added: diff.total_added(),
        removed: diff.total_removed(),
        checks_passed: passed,
        checks_total: total,
        success: report.outcome.is_success(),
    }
}
