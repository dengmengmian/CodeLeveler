//! Post-gate baseline delta attribution (design: verification is a function of
//! the CHANGE, not of the repo's prior state).
//!
//! When the working-tree gate has failing checks, re-run just those checks
//! against the task's starting commit in a throwaway detached worktree. A
//! test failure that reproduces on the baseline pre-dates this change, so it
//! must not gate completion. [`VerificationReport::attribute_baseline`] does
//! the test-level diff; this module only materializes the baseline and feeds it
//! in. Non-test exit failures are never attributed without stronger evidence.
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
    let repo = repo.to_path_buf();
    let stdout = tokio::task::spawn_blocking(move || {
        let dirty = leveler_core::git_stdout(&repo, &["status", "--porcelain"])?;
        if !dirty.trim().is_empty() {
            return None;
        }
        leveler_core::git_stdout(&repo, &["rev-parse", "HEAD"])
    })
    .await
    .ok()??;
    let hash = stdout.trim().to_string();
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
    // Baseline attribution decides whether a red gate is charged to this turn or
    // to the repository it started from — i.e. whether a repair turn fires at
    // all. It ran completely silently, so a run that spent tens of rounds
    // repairing someone else's failure looked identical to one doing real work.
    tracing::info!(
        base_commit,
        failed_gates = ?failed,
        "reconciling failed gates against baseline"
    );
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
        report.attribute_baseline(&base, base_commit);
        let pre_existing = report
            .confirmed_baseline_failures()
            .iter()
            .map(|check| check.name.as_str())
            .collect::<Vec<_>>();
        tracing::info!(
            pre_existing = ?pre_existing,
            still_gating = ?report
                .failed_gates()
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            "baseline attribution done"
        );
    } else {
        tracing::warn!(
            base_commit,
            "baseline report unavailable; every failure will be charged to this turn"
        );
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

    // The report is already authoritative. Worktree deregistration and temp
    // removal cannot change it, so detach exact-path cleanup instead of adding
    // up to ten seconds to Finalizing -> TaskFinished.
    detach_baseline_cleanup(repo.to_path_buf(), worktree_arg, tmp);

    Some(report)
}

fn detach_baseline_cleanup(repo: std::path::PathBuf, worktree_arg: String, tmp: tempfile::TempDir) {
    detach_cleanup_future(async move {
        let cleanup = CancellationToken::new();
        let removed = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            git_ok(
                &repo,
                &["worktree", "remove", "--force", &worktree_arg],
                &cleanup,
            ),
        )
        .await;
        match removed {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                worktree = %worktree_arg,
                "post-verification baseline worktree cleanup failed"
            ),
            Err(_) => tracing::warn!(
                worktree = %worktree_arg,
                "post-verification baseline worktree cleanup timed out"
            ),
        }
        drop(tmp);
    });
}

fn detach_cleanup_future(cleanup: impl std::future::Future<Output = ()> + Send + 'static) {
    tokio::spawn(cleanup);
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

    /// A committed git repo whose `marker.txt` holds `committed`, with the
    /// working tree overwritten to `working` (uncommitted).
    fn repo_with_marker(committed: &str, working: &str) -> leveler_test_support::git::ScratchRepo {
        use leveler_test_support::git::{run, scratch_repo};
        use std::os::unix::fs::PermissionsExt;
        let repo = scratch_repo();
        let p = repo.path();
        std::fs::write(p.join("marker.txt"), committed).unwrap();
        let fake_node = p.join("node");
        std::fs::write(
            &fake_node,
            "#!/bin/sh\n\
             if test \"$(cat marker.txt)\" = OK; then exit 0; fi\n\
             echo 'not ok 1 - marker'\n\
             exit 1\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_node).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_node, permissions).unwrap();
        run(p, &["add", "."]);
        run(p, &["commit", "-qm", "base"]);
        std::fs::write(p.join("marker.txt"), working).unwrap();
        repo
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
                scope_policy: Default::default(),
            }],
        }
    }

    /// A hermetic Test check whose TAP output carries a parseable failure id.
    fn test_marker_plan(repo: &Path) -> VerificationPlan {
        VerificationPlan {
            commands: vec![VerificationCommand {
                name: "marker test".into(),
                program: repo.join("node").to_string_lossy().into_owned(),
                args: vec![],
                kind: CheckKind::Test,
                gating: true,
                timeout_seconds: 30,
                scope_policy: Default::default(),
            }],
        }
    }

    fn env() -> Arc<EnvSnapshot> {
        // Library tests do not install the application's global environment
        // capability, so `leveler_core::environment()` is intentionally empty.
        // Supply the same explicit host snapshot as the composition root;
        // otherwise confined verification fails before `/bin/sh` starts and a
        // passing baseline is misattributed as a pre-existing failure.
        Arc::new(EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        ))
    }

    fn committed_head(repo: &Path) -> String {
        leveler_core::git_stdout(repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string()
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
    async fn capture_head_refuses_a_dirty_tree_it_cannot_reconstruct() {
        let repo = repo_with_marker("OK", "BROKEN");
        assert!(
            capture_head(repo.path()).await.is_none(),
            "HEAD alone is not the task-start baseline of a dirty worktree"
        );
    }

    #[tokio::test]
    async fn non_test_exit_failure_is_not_attributed_from_matching_name_alone() {
        // A Build check that is red on both trees is not grounded enough to
        // prove the failure is identical. It must keep gating.
        let dir = repo_with_marker("BROKEN", "BROKEN");
        let base = committed_head(dir.path());
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

        assert_eq!(
            report.failed_gates().len(),
            1,
            "a matching non-test exit code must not fabricate baseline attribution"
        );
        assert!(report.confirmed_baseline_failures().is_empty());
    }

    #[tokio::test]
    async fn grounded_test_failure_is_attributed_with_revision_provenance() {
        let dir = repo_with_marker("BROKEN", "BROKEN");
        let base = committed_head(dir.path());
        let plan = test_marker_plan(dir.path());
        let mut report = working_report(dir.path(), &plan).await;
        assert_eq!(report.failed_gates().len(), 1);
        assert_eq!(
            report.checks[0].failed_tests,
            std::collections::BTreeSet::from(["marker".to_string()])
        );

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

        assert!(report.failed_gates().is_empty());
        let (revision, provenance) = report.checks[0]
            .confirmed_baseline_failure()
            .expect("grounded baseline disposition");
        assert_eq!(revision, base);
        assert_eq!(
            provenance.failed_tests,
            std::collections::BTreeSet::from(["marker".to_string()])
        );
    }

    #[tokio::test]
    async fn new_failure_absent_from_baseline_still_gates() {
        // Fine on the baseline (marker=OK), broken only in the working tree →
        // this change's fault → must gate.
        let dir = repo_with_marker("OK", "BROKEN");
        let base = committed_head(dir.path());
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

    #[tokio::test]
    async fn cancellation_during_baseline_does_not_leave_a_registered_worktree() {
        let repo = repo_with_marker("OK", "BROKEN");
        let base = committed_head(repo.path());
        let plan = VerificationPlan {
            commands: vec![VerificationCommand {
                name: "slow baseline".into(),
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "exec /bin/sleep 5".into()],
                kind: CheckKind::Test,
                gating: true,
                timeout_seconds: 30,
                scope_policy: Default::default(),
            }],
        };
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel.cancel();
        });

        let _ = baseline_report(repo.path(), &base, &plan, &[], env(), &cancellation).await;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let listing = loop {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["worktree", "list", "--porcelain"])
                .output()
                .unwrap();
            let listing = String::from_utf8_lossy(&output.stdout).into_owned();
            if !listing.contains("leveler-baseline-") || std::time::Instant::now() >= deadline {
                break listing;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert!(
            !listing.contains("leveler-baseline-"),
            "cancelled baseline leaked a git worktree registration: {listing}"
        );
    }

    #[tokio::test]
    async fn detached_cleanup_cannot_block_the_verification_result() {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started_task = started.clone();
        let release_task = release.clone();
        let finished_task = finished.clone();

        let returned = tokio::time::timeout(std::time::Duration::from_millis(100), async {
            detach_cleanup_future(async move {
                started_task.notify_one();
                release_task.notified().await;
                finished_task.store(true, std::sync::atomic::Ordering::SeqCst);
            });
        })
        .await;
        assert!(
            returned.is_ok(),
            "detaching cleanup must return immediately"
        );
        started.notified().await;
        assert!(
            !finished.load(std::sync::atomic::Ordering::SeqCst),
            "the verification caller returned while cleanup remained blocked"
        );
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !finished.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached cleanup completes once released");
    }
}
