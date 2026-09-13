//! The Agent Verification Contract is a declaration, not a suggestion.
//!
//! A `.leveler/config.yaml` verify section is the project telling the runtime
//! what evidence counts for it. These tests pin the four properties that would
//! silently rot: the declared commands replace discovery, they really execute,
//! a declared failure really propagates (nothing about "running inside a
//! sandbox" may soften it), and the sandbox-compatible selection never depends
//! on a test deciding to skip itself.
//!
//! They run under the operator/CI gate. `leveler-verifier` is excluded from the
//! agent contract precisely because its tests run real checks, which a
//! verification sandbox cannot nest — see
//! docs/VERIFICATION_RUNTIME_AND_RESULT_SEMANTICS_CLOSURE.md.

use std::path::Path;
use std::sync::Arc;

use leveler_core::EnvSnapshot;
use leveler_verifier::discover::plan_for_repo;
use leveler_verifier::plan::{ScopePolicy, VerificationPlan};
use leveler_verifier::{CheckKind, CheckStatus, Verdict, VerificationReport, Verifier};

fn project(spec: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".leveler")).expect("config dir");
    std::fs::write(dir.path().join(".leveler/config.yaml"), spec).expect("config");
    dir
}

fn verifier_for(root: &Path) -> Verifier {
    // `Verifier::new` reads the installed process capabilities, which are empty
    // for a library-only caller; the explicit snapshot is what gives the check
    // a PATH to find `sh` on.
    Verifier::with_environment(
        root.to_path_buf(),
        Arc::new(EnvSnapshot::new(
            std::env::vars_os(),
            root.to_path_buf(),
            std::env::temp_dir(),
        )),
    )
}

/// Test D — a declared contract replaces discovery, verbatim.
///
/// If this ever passed with the language-derived plan instead, an agent would
/// be judged against `cargo test --workspace` while the project believes it
/// declared something narrower — and the declaration would be a lie.
#[test]
fn a_declared_verify_section_replaces_discovered_commands() {
    let dir =
        project("verify:\n  test:\n    program: sh\n    args: [\"-c\", \"echo CONTRACT_PROBE\"]\n");
    let plan = plan_for_repo(dir.path());
    assert_eq!(
        plan.commands.len(),
        1,
        "a declared section must produce exactly the declared checks, got {:?}",
        plan.commands
    );
    let check = &plan.commands[0];
    assert_eq!(check.program, "sh");
    assert_eq!(check.args, vec!["-c", "echo CONTRACT_PROBE"]);
    assert_eq!(check.kind, CheckKind::Test);
    assert!(check.gating, "a declared test check gates");
    // User-declared commands are authority: the harness does not get to narrow
    // them, so the project's own words reach the sandbox unchanged.
    assert_eq!(check.scope_policy, ScopePolicy::Exact);
}

/// Test A — every declared command really starts and reports an exit status.
#[tokio::test]
async fn a_declared_command_really_executes() {
    let dir =
        project("verify:\n  test:\n    program: sh\n    args: [\"-c\", \"echo CONTRACT_PROBE\"]\n");
    let plan = plan_for_repo(dir.path());
    let report = run(&dir, &plan).await;
    assert_eq!(report.checks.len(), 1);
    assert_eq!(
        report.checks[0].status,
        CheckStatus::Passed,
        "evidence: {}",
        report.checks[0].evidence
    );
    assert!(
        report.checks[0].evidence.contains("CONTRACT_PROBE"),
        "the declared command's own output is the evidence, got: {}",
        report.checks[0].evidence
    );
    assert_eq!(report.verdict(), Verdict::Verified);
}

/// Test B — a declared failure propagates as a failure.
///
/// The one thing that must never happen is a selected check failing while the
/// run reports it passed. This is the shape that makes a contract trustworthy
/// inside a sandbox: the verdict follows the command's exit status and the
/// gating role, never the environment it happened to run in.
#[tokio::test]
async fn a_declared_failure_propagates() {
    let dir = project(
        "verify:\n  test:\n    program: sh\n    args: [\"-c\", \"echo CONTRACT_BROKEN >&2; exit 3\"]\n",
    );
    let plan = plan_for_repo(dir.path());
    let report = run(&dir, &plan).await;
    assert_eq!(report.checks[0].status, CheckStatus::Failed);
    assert!(
        report.checks[0].evidence.contains("CONTRACT_BROKEN"),
        "the failing command's own output must be kept, got: {}",
        report.checks[0].evidence
    );
    assert_eq!(report.verdict(), Verdict::Failed);
    assert!(!report.passed(), "a failing gating check blocks completion");
}

/// A non-gating declared failure is reported as failed without blocking, which
/// is the policy for a formatter: the check's own result stays true and the
/// verdict stays honest about scope.
#[tokio::test]
async fn a_declared_non_gating_failure_does_not_block_but_still_reads_failed() {
    let dir = project(
        "verify:\n  format:\n    program: sh\n    args: [\"-c\", \"exit 3\"]\n  test:\n    program: sh\n    args: [\"-c\", \"echo ok\"]\n",
    );
    let plan = plan_for_repo(dir.path());
    let report = run(&dir, &plan).await;
    let fmt = report
        .checks
        .iter()
        .find(|c| c.name == "format")
        .expect("format check");
    assert_eq!(fmt.status, CheckStatus::Failed, "the check failed; say so");
    assert_eq!(report.verdict(), Verdict::Verified);
    assert!(report.passed());
}

async fn run(dir: &tempfile::TempDir, plan: &VerificationPlan) -> VerificationReport {
    verifier_for(dir.path())
        .verify(
            plan,
            &[],
            &[],
            &tokio_util::sync::CancellationToken::new(),
            &mut |_| {},
        )
        .await
}
