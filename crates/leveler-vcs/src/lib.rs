//! `leveler-vcs` — git/GitHub workflow for shipping an agent's changes:
//! create a branch, commit, push, and open a pull request.
//!
//! All operations go through the `git`/`gh` CLIs via the execution layer's
//! [`CommandRunner`], so there is no libgit2 dependency and behavior matches
//! what a user would run by hand.
#![forbid(unsafe_code)]

pub mod parallel;
mod workflow;

pub use parallel::{MergeCandidate, MergeOutcome, worktree_path};
pub use workflow::{GitWorkflow, VcsError, WorkflowOptions, WorkflowOutcome, slugify};

/// Shared fixtures for this crate's unit tests: temp repo dirs and git setup.
#[cfg(test)]
pub(crate) mod test_util {
    use std::path::{Path, PathBuf};

    /// Run a git command in `dir`, retrying transient failures.
    ///
    /// Under the very high concurrency of `cargo test --workspace` (dozens of
    /// test binaries at once) a git subprocess can transiently fail on
    /// resource contention. Silently ignoring it would leave the repo
    /// half-built and surface later as a mysterious merge assertion, so retry
    /// a few times, then fail loud with the git error.
    pub(crate) fn run_git(dir: &Path, args: &[&str]) {
        for attempt in 1..=3 {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            if output.status.success() {
                return;
            }
            assert!(
                attempt < 3,
                "git {args:?} failed after {attempt} attempts: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    /// Init an empty repo on branch `main` with a test identity (no commits).
    pub(crate) fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "t@t"]);
        run_git(dir, &["config", "user.name", "t"]);
    }

    /// A fresh unique temp dir for one test.
    ///
    /// Includes the pid: the counter resets to 0 each run, so without it a
    /// panicked test's leftover dir (its cleanup is skipped) is reused by the
    /// next run — a stale git repo with a pre-existing branch/origin made
    /// `git push` flaky. Also clear any residue before use.
    ///
    /// Uses `std::env::temp_dir()` deliberately: in unit tests
    /// `leveler_core::environment()` is the uninstalled empty snapshot whose
    /// temp dir is `""`, which would drop these repos into the CWD — inside
    /// this very workspace's git repo.
    pub(crate) fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "leveler-vcs-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
