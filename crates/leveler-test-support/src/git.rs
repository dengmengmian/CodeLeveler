//! Throwaway git repositories for tests.
//!
//! Fixtures used to hand-roll `git init` + a test identity in each crate,
//! silently inheriting the developer machine's global git config (autocrlf,
//! signing, hook templates, `init.defaultBranch`) — tests then behaved
//! differently locally vs CI. This module is the one place that builds
//! isolated scratch repos:
//!
//! - Fixture git commands run with `GIT_CONFIG_GLOBAL=/dev/null` and
//!   `GIT_CONFIG_NOSYSTEM=1`, so no host config leaks into setup.
//! - The repo gets `core.autocrlf false` as LOCAL config: the code under
//!   test runs git with the host's real config, and a global autocrlf would
//!   smudge file content on checkout/restore and break byte-exact assertions.
use std::path::{Path, PathBuf};

/// A git repository in a fresh unique temp dir, removed on drop.
pub struct ScratchRepo {
    dir: PathBuf,
}

impl ScratchRepo {
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for ScratchRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A fresh isolated repo (`main`, test identity, no commits) in its own
/// unique temp dir. Seed files with `std::fs` and commit via [`run`].
pub fn scratch_repo() -> ScratchRepo {
    let dir = unique_dir();
    init_repo(&dir);
    ScratchRepo { dir }
}

/// Turn an existing directory into an isolated repo on branch `main` with a
/// test identity (no commits). For callers whose harness owns the dir; prefer
/// [`scratch_repo`] otherwise.
pub fn init_repo(dir: &Path) {
    run(dir, &["init", "-q", "-b", "main"]);
    run(dir, &["config", "user.email", "t@t"]);
    run(dir, &["config", "user.name", "t"]);
    // No autocrlf: the host's global config must not smudge file content on
    // checkout/restore (CRLF breaks byte-exact assertions).
    run(dir, &["config", "core.autocrlf", "false"]);
}

/// Run one git command in `dir`, panicking with git's stderr on failure.
pub fn run(dir: &Path, args: &[&str]) {
    let output = command(dir, args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Run one git command in `dir`, returning whether it succeeded. For probes
/// that skip a test when git itself is unavailable.
pub fn try_run(dir: &Path, args: &[&str]) -> bool {
    command(dir, args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn command(dir: &Path, args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

/// A fresh unique temp dir.
///
/// Includes the pid: the counter resets to 0 each run, so if a test is
/// killed hard (abort, SIGKILL) its leftover dir would otherwise be reused
/// by the next run as a stale repo. Any residue is cleared before use.
///
/// Uses `std::env::temp_dir()` deliberately: in unit tests the application
/// environment capability is not installed, and an empty temp-dir snapshot
/// would drop these repos into the CWD — inside this workspace's own repo.
fn unique_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "leveler-scratch-repo-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_repo_commits_isolated_from_host_config_and_cleans_up() {
        let repo = scratch_repo();
        let path = repo.path().to_path_buf();
        assert!(path.join(".git").is_dir());

        // The identity comes from local config, so a plain commit works even
        // with the host's global config masked out.
        std::fs::write(path.join("a.txt"), "x\n").unwrap();
        run(&path, &["add", "-A"]);
        run(&path, &["commit", "-qm", "init"]);
        assert!(try_run(&path, &["rev-parse", "HEAD"]));

        drop(repo);
        assert!(!path.exists(), "scratch repo must be removed on drop");
    }
}
