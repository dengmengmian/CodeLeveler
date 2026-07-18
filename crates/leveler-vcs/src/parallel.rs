//! Parallel multi-agent editing support (spec §42): git worktree management and
//! a merge that integrates several candidate branches — combining disjoint edits
//! and resolving same-region conflicts by preferring verified candidates.

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::workflow::{GitWorkflow, VcsError};

/// A candidate produced by one agent, on its own branch.
#[derive(Debug, Clone)]
pub struct MergeCandidate {
    pub branch: String,
    /// Whether this candidate passed verification (integrated first).
    pub verified: bool,
}

/// The result of integrating candidates.
#[derive(Debug, Clone, Default)]
pub struct MergeOutcome {
    /// Branches successfully merged into the integration result.
    pub integrated: Vec<String>,
    /// Branches skipped because they conflicted with already-merged edits.
    pub conflicted: Vec<String>,
}

impl GitWorkflow {
    /// The current HEAD commit sha.
    pub async fn head_sha(&self, cancellation: &CancellationToken) -> Result<String, VcsError> {
        self.git(&["rev-parse", "HEAD"], cancellation).await
    }

    /// Add a git worktree at `path` on a new `branch` based at `base_ref`.
    pub async fn add_worktree(
        &self,
        path: &Path,
        branch: &str,
        base_ref: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), VcsError> {
        let path = path.to_string_lossy();
        self.git(
            &["worktree", "add", "-b", branch, &path, base_ref],
            cancellation,
        )
        .await?;
        Ok(())
    }

    /// Remove a worktree and its branch (best effort).
    pub async fn remove_worktree(
        &self,
        path: &Path,
        branch: &str,
        cancellation: &CancellationToken,
    ) {
        let p = path.to_string_lossy().into_owned();
        let _ = self
            .git(&["worktree", "remove", "--force", &p], cancellation)
            .await;
        let _ = self.git(&["branch", "-D", branch], cancellation).await;
    }

    /// Integrate `candidates` into the current worktree (which must be a clean
    /// checkout at the base). Verified candidates merge first; a candidate that
    /// conflicts with already-merged edits is aborted and skipped (first-wins on
    /// the contested region, union on disjoint regions).
    pub async fn integrate(
        &self,
        candidates: &[MergeCandidate],
        cancellation: &CancellationToken,
    ) -> Result<MergeOutcome, VcsError> {
        let mut ordered: Vec<&MergeCandidate> = candidates.iter().collect();
        // Stable sort: verified candidates first, original order preserved otherwise.
        ordered.sort_by_key(|c| std::cmp::Reverse(c.verified));

        let mut outcome = MergeOutcome::default();
        for candidate in ordered {
            let merged = self
                .git(
                    &["merge", "--no-edit", "--no-ff", &candidate.branch],
                    cancellation,
                )
                .await;
            match merged {
                Ok(_) => outcome.integrated.push(candidate.branch.clone()),
                Err(_) => {
                    // Conflict (or nothing to merge). Abort and skip this one.
                    let _ = self.git(&["merge", "--abort"], cancellation).await;
                    outcome.conflicted.push(candidate.branch.clone());
                }
            }
        }
        Ok(outcome)
    }
}

/// A unique temp worktree path for parallel candidate `index`.
pub fn worktree_path(base_name: &str, index: usize) -> PathBuf {
    leveler_core::environment()
        .temp_dir()
        .to_path_buf()
        .join(format!("leveler-wt-{base_name}-{index}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) {
        // Under the very high concurrency of `cargo test --workspace` (dozens of
        // test binaries at once) a git subprocess can transiently fail on
        // resource contention. Silently ignoring it would leave the repo
        // half-built and surface later as a mysterious merge assertion, so retry
        // a few times, then fail loud with the git error.
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

    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "t@t"]);
        run_git(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "line1\nline2\nline3\n").unwrap();
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-qm", "base"]);
    }

    fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = leveler_core::environment()
            .temp_dir()
            .to_path_buf()
            .join(format!(
                "leveler-par-{tag}-{}",
                N.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Two candidates editing disjoint files → both integrate (union).
    #[tokio::test]
    async fn integrates_disjoint_edits() {
        let dir = tmp("disjoint");
        init_repo(&dir);
        // candidate branches, each editing a different file
        run_git(&dir, &["checkout", "-q", "-b", "cand-a"]);
        std::fs::write(dir.join("a.txt"), "line1\nCHANGED-A\nline3\n").unwrap();
        run_git(&dir, &["commit", "-qam", "a"]);
        run_git(&dir, &["checkout", "-q", "main"]);
        run_git(&dir, &["checkout", "-q", "-b", "cand-b"]);
        std::fs::write(dir.join("b.txt"), "CHANGED-B\n").unwrap();
        run_git(&dir, &["commit", "-qam", "b"]);
        run_git(&dir, &["checkout", "-q", "main"]);

        let wf = GitWorkflow::new(&dir);
        let outcome = wf
            .integrate(
                &[
                    MergeCandidate {
                        branch: "cand-a".into(),
                        verified: true,
                    },
                    MergeCandidate {
                        branch: "cand-b".into(),
                        verified: true,
                    },
                ],
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.integrated.len(), 2);
        assert!(outcome.conflicted.is_empty());
        assert!(
            std::fs::read_to_string(dir.join("a.txt"))
                .unwrap()
                .contains("CHANGED-A")
        );
        assert!(
            std::fs::read_to_string(dir.join("b.txt"))
                .unwrap()
                .contains("CHANGED-B")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Two candidates editing the SAME line → verified one wins, other skipped.
    #[tokio::test]
    async fn resolves_same_region_conflict_by_selection() {
        let dir = tmp("conflict");
        init_repo(&dir);
        run_git(&dir, &["checkout", "-q", "-b", "cand-x"]);
        std::fs::write(dir.join("a.txt"), "line1\nFROM-X\nline3\n").unwrap();
        run_git(&dir, &["commit", "-qam", "x"]);
        run_git(&dir, &["checkout", "-q", "main"]);
        run_git(&dir, &["checkout", "-q", "-b", "cand-y"]);
        std::fs::write(dir.join("a.txt"), "line1\nFROM-Y\nline3\n").unwrap();
        run_git(&dir, &["commit", "-qam", "y"]);
        run_git(&dir, &["checkout", "-q", "main"]);

        let wf = GitWorkflow::new(&dir);
        // cand-y is verified → integrated first → wins the contested line.
        let outcome = wf
            .integrate(
                &[
                    MergeCandidate {
                        branch: "cand-x".into(),
                        verified: false,
                    },
                    MergeCandidate {
                        branch: "cand-y".into(),
                        verified: true,
                    },
                ],
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.integrated, vec!["cand-y".to_string()]);
        assert_eq!(outcome.conflicted, vec!["cand-x".to_string()]);
        let content = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        assert!(content.contains("FROM-Y"));
        assert!(!content.contains("FROM-X"));
        assert!(!content.contains("<<<<<<<"), "no conflict markers left");
        std::fs::remove_dir_all(&dir).ok();
    }
}
