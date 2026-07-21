//! Post-gate baseline delta attribution (design: verification is a function of
//! the CHANGE, not of the repo's prior state).
//!
//! When the working-tree gate has failing checks, re-run just those checks
//! against the task's starting commit in a throwaway detached worktree. A
//! failure that reproduces on the baseline pre-dates this change — a flaky /
//! env-dependent test, or breakage the repo already carried — so it must not
//! gate completion. [`VerificationReport::attribute_baseline`] does the
//! test-level diff; this module only materializes the baseline and feeds it in.
//!
//! Best-effort throughout: any git or setup failure yields no attribution (the
//! gate stands as-is), never a fabricated or a hidden failure. The baseline
//! worktree uses its own target directory, so it pays a cold build — the cost
//! is bounded by only ever running when the gate already failed, and only the
//! checks that failed.

use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use leveler_core::EnvSnapshot;
use leveler_verifier::{VerificationPlan, VerificationReport, Verifier};

/// The repo's current `HEAD` commit — the pre-change baseline anchor — or
/// `None` when the path is not a git work tree, has no commit, or git is
/// unavailable. Capture this ONCE at task start, before the agent edits, so it
/// stays the true "before" state even if the agent commits mid-task.
pub(crate) async fn capture_head(repo: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let hash = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!hash.is_empty()).then_some(hash)
}

/// Reconcile `report` against the baseline: re-run its failing gating checks at
/// `base_commit` and attribute the ones that also fail there to the repo's prior
/// state. No-op when nothing gating failed or the baseline cannot be built.
///
/// `full_plan` is the plan the working gate ran; the baseline runs only the
/// subset whose checks failed. `modified_files` is forwarded unchanged so the
/// baseline reproduces the SAME narrowed command (e.g. `go test ./pkg/...`) the
/// working tree ran.
pub(crate) async fn reconcile_with_baseline(
    report: &mut VerificationReport,
    repo: &Path,
    base_commit: &str,
    full_plan: &VerificationPlan,
    modified_files: &[String],
    environment: Arc<EnvSnapshot>,
    cancellation: &CancellationToken,
) {
    let failed: HashSet<String> = report
        .failed_gates()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    if failed.is_empty() {
        return;
    }
    let subset = VerificationPlan {
        commands: full_plan
            .commands
            .iter()
            .filter(|c| failed.contains(&c.name))
            .cloned()
            .collect(),
    };
    if subset.commands.is_empty() {
        return;
    }
    if let Some(base) = baseline_report(
        repo,
        base_commit,
        &subset,
        modified_files,
        environment,
        cancellation,
    )
    .await
    {
        report.attribute_baseline(&base);
    }
}

/// Check out `base_commit` in a throwaway detached worktree, run `plan` there,
/// and return the resulting report. The worktree is removed afterwards. Returns
/// `None` if the worktree cannot be created (→ no attribution).
async fn baseline_report(
    repo: &Path,
    base_commit: &str,
    plan: &VerificationPlan,
    modified_files: &[String],
    environment: Arc<EnvSnapshot>,
    cancellation: &CancellationToken,
) -> Option<VerificationReport> {
    let tmp = tempfile::Builder::new()
        .prefix("leveler-baseline-")
        .tempdir()
        .ok()?;
    // `git worktree add` creates this path; it must not pre-exist.
    let worktree = tmp.path().join("wt");
    let worktree_arg = worktree.to_string_lossy().into_owned();

    // Detached checkout of base_commit: independent of the main tree's dirty
    // state, so the agent's uncommitted edits are absent — exactly the "before"
    // tree we want to compare against.
    if !git_ok(
        repo,
        &["worktree", "add", "--detach", &worktree_arg, base_commit],
        cancellation,
    )
    .await
    {
        return None;
    }

    let verifier = Verifier::with_environment(&worktree, environment);
    let report = verifier
        .verify(plan, &[], modified_files, cancellation, &mut |_| {})
        .await;

    // Best-effort cleanup: deregister the worktree; `tmp` drop removes files.
    let _ = git_ok(
        repo,
        &["worktree", "remove", "--force", &worktree_arg],
        cancellation,
    )
    .await;

    Some(report)
}

/// Run a git subcommand in `repo`, returning whether it exited 0. Output is
/// discarded — callers only care about success.
pub(crate) async fn git_ok(repo: &Path, args: &[&str], cancellation: &CancellationToken) -> bool {
    if cancellation.is_cancelled() {
        return false;
    }
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

// These tests drive git worktrees and a shell-based marker gate as fixtures;
// the baseline-reconciliation logic they cover is platform-independent, and the
// Windows shell/program-resolution quirks are not worth reproducing, so the
// module is gated to unix (git + /bin/sh are always present there).
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use leveler_verifier::{CheckKind, VerificationCommand};

    async fn git(repo: &Path, args: &[&str]) {
        assert!(
            git_ok(repo, args, &CancellationToken::new()).await,
            "git {args:?} failed"
        );
    }

    /// A committed git repo whose `marker.txt` holds `committed`, with the
    /// working tree overwritten to `working` (uncommitted).
    async fn repo_with_marker(committed: &str, working: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]).await;
        git(p, &["config", "user.email", "t@t"]).await;
        git(p, &["config", "user.name", "t"]).await;
        std::fs::write(p.join("marker.txt"), committed).unwrap();
        git(p, &["add", "."]).await;
        git(p, &["commit", "-qm", "base"]).await;
        std::fs::write(p.join("marker.txt"), working).unwrap();
        dir
    }

    /// A gating check that passes iff `marker.txt` reads `OK`.
    fn marker_plan() -> VerificationPlan {
        VerificationPlan {
            commands: vec![VerificationCommand {
                name: "marker".into(),
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "test \"$(cat marker.txt)\" = OK".into()],
                kind: CheckKind::Build,
                gating: true,
                timeout_seconds: 30,
            }],
        }
    }

    fn env() -> Arc<EnvSnapshot> {
        Arc::new(leveler_core::environment().clone())
    }

    async fn working_report(repo: &Path, plan: &VerificationPlan) -> VerificationReport {
        Verifier::with_environment(repo, env())
            .verify(plan, &[], &[], &CancellationToken::new(), &mut |_| {})
            .await
    }

    #[tokio::test]
    async fn capture_head_returns_none_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        assert!(capture_head(dir.path()).await.is_none());
    }

    #[tokio::test]
    async fn baseline_failure_is_attributed_and_stops_gating() {
        // Broken on the baseline AND still broken now (the change didn't touch
        // it) → pre-existing → must not gate.
        let dir = repo_with_marker("BROKEN", "BROKEN").await;
        let base = capture_head(dir.path()).await.expect("has HEAD");
        let plan = marker_plan();
        let mut report = working_report(dir.path(), &plan).await;
        assert_eq!(report.failed_gates().len(), 1, "working gate is red");

        reconcile_with_baseline(
            &mut report,
            dir.path(),
            &base,
            &plan,
            &[],
            env(),
            &CancellationToken::new(),
        )
        .await;

        assert!(
            report.failed_gates().is_empty(),
            "a failure that reproduces on the baseline must not gate"
        );
    }

    #[tokio::test]
    async fn new_failure_absent_from_baseline_still_gates() {
        // Fine on the baseline (marker=OK), broken only in the working tree →
        // this change's fault → must gate.
        let dir = repo_with_marker("OK", "BROKEN").await;
        let base = capture_head(dir.path()).await.expect("has HEAD");
        let plan = marker_plan();
        let mut report = working_report(dir.path(), &plan).await;
        assert_eq!(report.failed_gates().len(), 1);

        reconcile_with_baseline(
            &mut report,
            dir.path(),
            &base,
            &plan,
            &[],
            env(),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            report.failed_gates().len(),
            1,
            "a failure the baseline did not have must still gate"
        );
    }
}
