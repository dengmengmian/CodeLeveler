//! The git/GitHub workflow driver.

use std::path::PathBuf;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use leveler_execution::{CommandRunner, ProcessRequest};

/// Errors from the VCS workflow.
#[derive(Debug, thiserror::Error)]
pub enum VcsError {
    #[error("git {args:?} failed (exit {code:?}): {stderr}")]
    Git {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },
    #[error("failed to run git: {0}")]
    Spawn(String),
    #[error("not a git repository: {0}")]
    NotARepo(String),
}

/// What the workflow should do after a successful agent run.
#[derive(Debug, Clone)]
pub struct WorkflowOptions {
    /// Branch to create/switch to before committing. `None` uses the current branch.
    pub branch: Option<String>,
    /// Commit message (may be multi-line).
    pub commit_message: String,
    /// Whether to stage and commit.
    pub commit: bool,
    /// Exact paths to stage. When non-empty, only these are committed (so the
    /// agent never commits stray artifacts or CodeLeveler's own state). Empty
    /// falls back to everything except `.leveler/`.
    pub stage_paths: Vec<String>,
    /// Whether to push the branch.
    pub push: bool,
    /// Remote to push to.
    pub remote: String,
    /// Whether to open a pull request (via `gh`).
    pub open_pr: bool,
    /// PR title.
    pub pr_title: String,
    /// PR body.
    pub pr_body: String,
    /// PR base branch (`None` lets `gh` pick the default).
    pub pr_base: Option<String>,
}

impl Default for WorkflowOptions {
    fn default() -> Self {
        Self {
            branch: None,
            commit_message: String::new(),
            commit: false,
            stage_paths: Vec::new(),
            push: false,
            remote: "origin".to_string(),
            open_pr: false,
            pr_title: String::new(),
            pr_body: String::new(),
            pr_base: None,
        }
    }
}

/// The result of running the workflow.
#[derive(Debug, Clone, Default)]
pub struct WorkflowOutcome {
    pub branch: String,
    pub committed: bool,
    pub commit_sha: Option<String>,
    pub pushed: bool,
    pub pr_url: Option<String>,
    /// Human-readable notes about steps taken or skipped.
    pub notes: Vec<String>,
}

/// Runs git/GitHub operations for a repository.
pub struct GitWorkflow {
    runner: CommandRunner,
    repo_root: PathBuf,
    environment: std::sync::Arc<leveler_core::EnvSnapshot>,
}

impl GitWorkflow {
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self::with_environment(
            repo_root,
            std::sync::Arc::new(leveler_core::environment().clone()),
        )
    }

    pub fn with_environment(
        repo_root: impl Into<PathBuf>,
        environment: std::sync::Arc<leveler_core::EnvSnapshot>,
    ) -> Self {
        Self {
            runner: CommandRunner::with_environment(environment.clone()),
            repo_root: repo_root.into(),
            environment,
        }
    }

    /// The repository root this workflow operates in.
    pub fn repo_root(&self) -> &std::path::Path {
        &self.repo_root
    }

    /// Run git with `args`, returning trimmed stdout or a [`VcsError`].
    pub(crate) async fn git(
        &self,
        args: &[&str],
        cancellation: &CancellationToken,
    ) -> Result<String, VcsError> {
        let mut request = ProcessRequest::new(
            "git",
            args.iter().map(|s| s.to_string()).collect(),
            self.repo_root.clone(),
        );
        request.timeout = Duration::from_secs(120);
        let output = self
            .runner
            .run(request, cancellation.child_token())
            .await
            .map_err(|e| VcsError::Spawn(e.to_string()))?;
        if output.success() {
            Ok(output.stdout.trim().to_string())
        } else {
            Err(VcsError::Git {
                args: args.iter().map(|s| s.to_string()).collect(),
                code: output.exit_code,
                stderr: format!("{}{}", output.stdout, output.stderr)
                    .trim()
                    .to_string(),
            })
        }
    }

    /// The current branch name.
    pub async fn current_branch(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<String, VcsError> {
        self.git(&["rev-parse", "--abbrev-ref", "HEAD"], cancellation)
            .await
    }

    /// Stage all changes except `.leveler/` and commit; returns whether a commit
    /// was made (false if there was nothing to commit).
    pub async fn commit_changes(
        &self,
        message: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, VcsError> {
        self.git(
            &["add", "-A", "--", ".", ":(exclude).leveler"],
            cancellation,
        )
        .await?;
        let staged = self
            .git(&["diff", "--cached", "--name-only"], cancellation)
            .await?;
        if staged.trim().is_empty() {
            return Ok(false);
        }
        self.git(&["commit", "-m", message], cancellation).await?;
        Ok(true)
    }

    /// Whether the working tree has uncommitted changes.
    pub async fn has_changes(&self, cancellation: &CancellationToken) -> Result<bool, VcsError> {
        Ok(!self
            .git(&["status", "--porcelain"], cancellation)
            .await?
            .trim()
            .is_empty())
    }

    /// Create `name` if it does not exist, then switch to it.
    async fn switch_branch(
        &self,
        name: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), VcsError> {
        let exists = self
            .git(&["rev-parse", "--verify", "--quiet", name], cancellation)
            .await
            .is_ok();
        if exists {
            self.git(&["checkout", name], cancellation).await?;
        } else {
            self.git(&["checkout", "-b", name], cancellation).await?;
        }
        Ok(())
    }

    /// Whether the `gh` CLI is available.
    pub fn gh_available(&self) -> bool {
        find_in_path("gh", &self.environment).is_some()
    }

    /// Execute the workflow. Branch/commit/push failures are fatal; PR failures
    /// (e.g. `gh` unauthenticated) are recorded as notes, not errors.
    pub async fn run(
        &self,
        options: &WorkflowOptions,
        cancellation: &CancellationToken,
    ) -> Result<WorkflowOutcome, VcsError> {
        // Confirm we're in a git repo.
        self.git(&["rev-parse", "--is-inside-work-tree"], cancellation)
            .await
            .map_err(|_| VcsError::NotARepo(self.repo_root.display().to_string()))?;

        let mut outcome = WorkflowOutcome::default();

        if let Some(branch) = &options.branch {
            self.switch_branch(branch, cancellation).await?;
        }
        outcome.branch = self.current_branch(cancellation).await?;

        if options.commit {
            // Stage exactly what the agent changed (never .leveler/ or artifacts).
            if options.stage_paths.is_empty() {
                self.git(
                    &["add", "-A", "--", ".", ":(exclude).leveler"],
                    cancellation,
                )
                .await?;
            } else {
                let mut args: Vec<&str> = vec!["add", "--"];
                args.extend(options.stage_paths.iter().map(String::as_str));
                self.git(&args, cancellation).await?;
            }

            // Commit only if something was actually staged.
            let staged = self
                .git(&["diff", "--cached", "--name-only"], cancellation)
                .await?;
            if staged.trim().is_empty() {
                outcome.notes.push("nothing to commit".to_string());
            } else {
                self.git(&["commit", "-m", &options.commit_message], cancellation)
                    .await?;
                outcome.committed = true;
                outcome.commit_sha = self.git(&["rev-parse", "HEAD"], cancellation).await.ok();
            }
        }

        if options.push {
            self.git(
                &["push", "--set-upstream", &options.remote, &outcome.branch],
                cancellation,
            )
            .await?;
            outcome.pushed = true;
        }

        if options.open_pr {
            if !self.gh_available() {
                outcome
                    .notes
                    .push("`gh` CLI not found; skipped PR creation".to_string());
            } else {
                match self.open_pr(options, cancellation).await {
                    Ok(url) => outcome.pr_url = Some(url),
                    Err(e) => outcome.notes.push(format!("PR creation failed: {e}")),
                }
            }
        }

        Ok(outcome)
    }

    async fn open_pr(
        &self,
        options: &WorkflowOptions,
        cancellation: &CancellationToken,
    ) -> Result<String, VcsError> {
        let mut args: Vec<String> = vec![
            "pr".into(),
            "create".into(),
            "--title".into(),
            options.pr_title.clone(),
            "--body".into(),
            options.pr_body.clone(),
        ];
        if let Some(base) = &options.pr_base {
            args.push("--base".into());
            args.push(base.clone());
        }
        let mut request = ProcessRequest::new("gh", args, self.repo_root.clone());
        request.timeout = Duration::from_secs(120);
        // `gh` is the one trusted adapter that intentionally receives its own
        // authentication variables. Provider/model secrets remain scrubbed.
        request.allow_env = vec!["GH_TOKEN".to_string(), "GITHUB_TOKEN".to_string()];
        let output = self
            .runner
            .run(request, cancellation.child_token())
            .await
            .map_err(|e| VcsError::Spawn(e.to_string()))?;
        if output.success() {
            // `gh pr create` prints the PR URL as its last line.
            let url = output
                .stdout
                .lines()
                .rev()
                .find(|l| l.starts_with("http"))
                .unwrap_or(output.stdout.trim())
                .to_string();
            Ok(url)
        } else {
            Err(VcsError::Git {
                args: vec!["gh pr create".into()],
                code: output.exit_code,
                stderr: format!("{}{}", output.stdout, output.stderr)
                    .trim()
                    .to_string(),
            })
        }
    }
}

fn find_in_path(program: &str, environment: &leveler_core::EnvSnapshot) -> Option<PathBuf> {
    let path = environment.var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Slugify a goal into a branch-name-safe string.
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let s = slug.trim_matches('-').to_string();
    if s.is_empty() { "task".to_string() } else { s }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    async fn init_repo(dir: &Path) {
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .output()
                .unwrap();
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        // Include the pid: the counter resets to 0 each run, so without it a
        // panicked test's leftover dir (its cleanup is skipped) is reused by the
        // next run — a stale git repo with a pre-existing branch/origin made
        // `git push` flaky. Also clear any residue before use.
        let d = std::env::temp_dir().join(format!(
            "leveler-vcs-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn slugify_makes_branch_safe() {
        assert_eq!(
            slugify("Add cancel_order to OrderService!"),
            "add-cancel-order-to-orderservice"
        );
        assert_eq!(slugify("   "), "task");
    }

    #[tokio::test]
    async fn branch_and_commit() {
        let dir = tmp("commit");
        init_repo(&dir).await;
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        // initial commit so HEAD exists
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(&dir)
            .output()
            .unwrap();

        std::fs::write(dir.join("a.txt"), "changed").unwrap();
        let wf = GitWorkflow::new(&dir);
        let opts = WorkflowOptions {
            branch: Some("leveler/test".into()),
            commit_message: "agent change".into(),
            commit: true,
            ..Default::default()
        };
        let out = wf.run(&opts, &CancellationToken::new()).await.unwrap();
        assert_eq!(out.branch, "leveler/test");
        assert!(out.committed);
        assert!(out.commit_sha.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn commits_only_specified_paths() {
        let dir = tmp("scoped");
        init_repo(&dir).await;
        std::fs::write(dir.join("keep.txt"), "1").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(&dir)
            .output()
            .unwrap();

        // Two files change, plus a stray artifact we must NOT commit.
        std::fs::write(dir.join("keep.txt"), "2").unwrap();
        std::fs::write(dir.join("stray.o"), "junk").unwrap();

        let wf = GitWorkflow::new(&dir);
        let opts = WorkflowOptions {
            commit: true,
            commit_message: "scoped".into(),
            stage_paths: vec!["keep.txt".into()],
            ..Default::default()
        };
        let out = wf.run(&opts, &CancellationToken::new()).await.unwrap();
        assert!(out.committed);

        let files = std::process::Command::new("git")
            .args(["show", "--name-only", "--format=", "HEAD"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let listed = String::from_utf8_lossy(&files.stdout);
        assert!(listed.contains("keep.txt"));
        assert!(
            !listed.contains("stray.o"),
            "stray artifact must not be committed: {listed}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn non_git_directory_reports_not_a_repo() {
        let dir = tmp("norepo");
        let wf = GitWorkflow::new(&dir);
        let err = wf
            .run(&WorkflowOptions::default(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, VcsError::NotARepo(_)), "got {err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn leveler_state_dir_is_never_committed() {
        let dir = tmp("exclude");
        init_repo(&dir).await;
        std::fs::write(dir.join("code.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(dir.join(".leveler")).unwrap();
        std::fs::write(dir.join(".leveler/state.db"), "internal").unwrap();

        let wf = GitWorkflow::new(&dir);
        let committed = wf
            .commit_changes("first", &CancellationToken::new())
            .await
            .unwrap();
        assert!(committed);

        let files = std::process::Command::new("git")
            .args(["show", "--name-only", "--format=", "HEAD"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let listed = String::from_utf8_lossy(&files.stdout);
        assert!(listed.contains("code.rs"));
        assert!(
            !listed.contains(".leveler"),
            "internal state must never be committed: {listed}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn switching_to_an_existing_branch_does_not_recreate_it() {
        let dir = tmp("switch");
        init_repo(&dir).await;
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        for args in [vec!["add", "-A"], vec!["commit", "-qm", "init"]] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .output()
                .unwrap();
        }
        // Pre-create the branch with a commit of its own.
        for args in [
            vec!["checkout", "-qb", "feature/y"],
            vec!["checkout", "-q", "main"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .output()
                .unwrap();
        }

        let wf = GitWorkflow::new(&dir);
        let opts = WorkflowOptions {
            branch: Some("feature/y".into()),
            ..Default::default()
        };
        let out = wf.run(&opts, &CancellationToken::new()).await.unwrap();
        assert_eq!(out.branch, "feature/y");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn push_to_local_bare_remote() {
        let remote = tmp("bare");
        std::process::Command::new("git")
            .args(["init", "-q", "--bare"])
            .current_dir(&remote)
            .output()
            .unwrap();

        let dir = tmp("push");
        init_repo(&dir).await;
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["remote", "add", "origin", remote.to_str().unwrap()])
            .current_dir(&dir)
            .output()
            .unwrap();

        let wf = GitWorkflow::new(&dir);
        let opts = WorkflowOptions {
            branch: Some("feature/x".into()),
            commit_message: "c".into(),
            commit: false,
            push: true,
            remote: "origin".into(),
            ..Default::default()
        };
        let out = wf.run(&opts, &CancellationToken::new()).await.unwrap();
        assert!(out.pushed);

        // The branch should now exist in the bare remote.
        let branches = std::process::Command::new("git")
            .args(["branch", "--list"])
            .current_dir(&remote)
            .output()
            .unwrap();
        let listed = String::from_utf8_lossy(&branches.stdout);
        assert!(listed.contains("feature/x"), "remote branches: {listed}");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&remote).ok();
    }
}
