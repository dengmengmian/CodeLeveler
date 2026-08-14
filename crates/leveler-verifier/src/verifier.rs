//! The verifier: runs the plan's checks, captures evidence, and enforces scope.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use leveler_execution::{CommandRunner, VerifyNetworkPolicy, process_request_for_verify_check};

use std::collections::BTreeSet;

use crate::failure::classify;
use crate::plan::{CheckKind, ScopePolicy, VerificationCommand, VerificationPlan};
use crate::report::{CheckOutcome, CheckStatus, VerificationReport};
use crate::test_results::{parse_go_failures, parse_rust_failures};

const MAX_EVIDENCE: usize = 4000;

/// Runs verification plans against a workspace.
pub struct Verifier {
    runner: Arc<CommandRunner>,
    environment: Arc<leveler_core::EnvSnapshot>,
    workspace_root: PathBuf,
}

impl Verifier {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self::with_environment(
            workspace_root,
            Arc::new(leveler_core::environment().clone()),
        )
    }

    pub fn with_environment(
        workspace_root: impl Into<PathBuf>,
        environment: Arc<leveler_core::EnvSnapshot>,
    ) -> Self {
        Self {
            runner: Arc::new(CommandRunner::with_environment(environment.clone())),
            environment,
            workspace_root: workspace_root.into(),
        }
    }

    /// Run every check in `plan`, verify scope, and return a report. `on_check`
    /// is invoked as each check finishes so the caller can stream progress.
    pub async fn verify(
        &self,
        plan: &VerificationPlan,
        allowed_paths: &[String],
        modified_files: &[String],
        cancellation: &CancellationToken,
        on_check: &mut dyn FnMut(&CheckOutcome),
    ) -> VerificationReport {
        let (scope_ok, scope_violations) = check_scope(allowed_paths, modified_files);

        let mut checks = Vec::new();
        for command in &plan.commands {
            if cancellation.is_cancelled() {
                break;
            }
            let outcome = self.run_check(command, modified_files, cancellation).await;
            on_check(&outcome);
            checks.push(outcome);
        }

        VerificationReport {
            checks,
            scope_ok,
            scope_violations,
            baseline_failures: Vec::new(),
        }
    }

    async fn run_check(
        &self,
        command: &VerificationCommand,
        modified_files: &[String],
        cancellation: &CancellationToken,
    ) -> CheckOutcome {
        let Some(resolved) = find_in_path(&command.program, &self.environment) else {
            return CheckOutcome {
                name: command.name.clone(),
                kind: command.kind,
                gating: command.gating,
                status: CheckStatus::ToolMissing,
                evidence: format!("`{}` not found on PATH", command.program),
                failure: None,
                failed_tests: BTreeSet::new(),
            };
        };

        let args = effective_args(command, modified_files, &self.workspace_root);
        // Repo / builtin verify: write confinement on, network inherits session
        // (not force-deny — K12 so cargo/go/npm cold caches still work).
        let mut request = process_request_for_verify_check(
            command.program.clone(),
            args,
            self.workspace_root.clone(),
            VerifyNetworkPolicy::InheritSession,
        );
        request.timeout = Duration::from_secs(command.timeout_seconds);

        match self.runner.run(request, cancellation.child_token()).await {
            Ok(output) => {
                let combined = combine(&output.stdout, &output.stderr);
                if output.success() {
                    CheckOutcome {
                        name: command.name.clone(),
                        kind: command.kind,
                        gating: command.gating,
                        status: CheckStatus::Passed,
                        evidence: truncate(&combined),
                        failure: None,
                        failed_tests: BTreeSet::new(),
                    }
                } else if crate::failure::is_environment_mismatch(&combined) {
                    // Toolchain/MSRV refusal: the ENVIRONMENT could not run the
                    // check, the code was never judged (R005 F-P1). Report it
                    // as environment — with provenance naming the binary that
                    // actually ran — instead of gating as a code failure.
                    CheckOutcome {
                        name: command.name.clone(),
                        kind: command.kind,
                        gating: command.gating,
                        status: CheckStatus::EnvironmentUnavailable,
                        evidence: format!(
                            "environment mismatch (ran `{}`): {}",
                            resolved.display(),
                            truncate(&combined)
                        ),
                        failure: Some(classify(command.kind, &combined)),
                        failed_tests: BTreeSet::new(),
                    }
                } else {
                    let failure = classify(command.kind, &combined);
                    CheckOutcome {
                        name: command.name.clone(),
                        kind: command.kind,
                        gating: command.gating,
                        status: CheckStatus::Failed,
                        evidence: truncate(&combined),
                        failure: Some(failure),
                        // Full (untruncated) output: the trailing `failures:`
                        // block / `--- FAIL:` lines may lie past the evidence cap.
                        failed_tests: parse_failed_tests(command, &combined),
                    }
                }
            }
            // Could not run the command at all — no test-level signal to parse.
            Err(e) => CheckOutcome {
                name: command.name.clone(),
                kind: command.kind,
                gating: command.gating,
                status: CheckStatus::Failed,
                evidence: format!("failed to run: {e}"),
                failure: Some(classify(command.kind, &e.to_string())),
                failed_tests: BTreeSet::new(),
            },
        }
    }
}

/// Confirm that every modified file falls under an allowed path. An empty
/// `allowed_paths` means no restriction (single-node/free-form runs).
fn check_scope(allowed_paths: &[String], modified_files: &[String]) -> (bool, Vec<String>) {
    if allowed_paths.is_empty() {
        return (true, Vec::new());
    }
    let violations: Vec<String> = modified_files
        .iter()
        .filter(|m| !allowed_paths.iter().any(|a| path_allows(a, m)))
        .cloned()
        .collect();
    (violations.is_empty(), violations)
}

fn path_allows(allowed: &str, modified: &str) -> bool {
    let allowed = allowed.trim_end_matches('/');
    modified == allowed || modified.starts_with(&format!("{allowed}/"))
}

/// The arguments a check actually runs with.
///
/// [`ScopePolicy::Exact`] commands run verbatim: the user declared a
/// verification contract and the harness does not get to reinterpret it —
/// neither by narrowing the target set nor by adding flags. Inferred commands
/// are the harness's own construction, so they may be narrowed to the change
/// under test and completed for baseline attribution.
fn effective_args(
    command: &VerificationCommand,
    modified_files: &[String],
    workspace_root: &Path,
) -> Vec<String> {
    if command.scope_policy == ScopePolicy::Exact {
        return command.args.clone();
    }
    // Narrow whole-repo commands to the changed packages (spec §29.5).
    let args = scope_args(&command.args, modified_files, workspace_root);
    // Complete failure sets for baseline attribution (see with_no_fail_fast).
    with_no_fail_fast(command, args)
}

/// Narrow a whole-repo package glob (`./...`) to just the packages containing the
/// modified files (spec §29.5: prefer targeted → module → full). Falls back to
/// the original args when it can't scope safely (a root-level change, or a
/// target directory that no longer exists).
fn scope_args(args: &[String], modified_files: &[String], workspace_root: &Path) -> Vec<String> {
    if modified_files.is_empty() || !args.iter().any(|a| a == "./...") {
        return args.to_vec();
    }

    let mut packages: Vec<String> = Vec::new();
    for file in modified_files {
        let dir = std::path::Path::new(file)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        // A root-level change can only be verified against the whole repo.
        let glob = if dir.is_empty() {
            "./...".to_string()
        } else {
            format!("./{dir}/...")
        };
        // `modified_files` is every path the run ever touched, so a directory
        // here may be gone by now: a scratch tree the run created and removed,
        // or a package it deleted outright. Narrowing to it would run a command
        // that cannot resolve its own target ("no such file or directory") and
        // fail the gate for a reason that has nothing to do with the code. A
        // valid broader gate beats an invalid narrow one — and skipping
        // verification is never the answer.
        if !workspace_root.join(dir).is_dir() {
            return args.to_vec();
        }
        if !packages.contains(&glob) {
            packages.push(glob);
        }
    }

    // If any change is at the repo root, we cannot narrow — run the full glob.
    if packages.iter().any(|p| p == "./...") {
        return args.to_vec();
    }

    let mut out = Vec::new();
    for arg in args {
        if arg == "./..." {
            out.extend(packages.iter().cloned());
        } else {
            out.push(arg.clone());
        }
    }
    out
}

/// `cargo test` stops at the first failing test binary by default, truncating
/// the failed-test set that baseline attribution diffs (test_results.rs): a
/// pre-existing failure hidden behind the truncation point can never be proven
/// pre-existing, and the working run's truncation can hide a genuinely new
/// failure behind an attributed one. Force the full suite for `cargo test`
/// checks; the flag goes before a `--` harness-args separator so it stays a
/// cargo flag. Other toolchains and non-Test checks are untouched.
fn with_no_fail_fast(command: &VerificationCommand, mut args: Vec<String>) -> Vec<String> {
    let is_cargo_test = command.kind == CheckKind::Test
        && program_stem(&command.program) == "cargo"
        && args.first().is_some_and(|a| a == "test");
    if !is_cargo_test || args.iter().any(|a| a == "--no-fail-fast") {
        return args;
    }
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    args.insert(at, "--no-fail-fast".to_string());
    args
}

/// Parse a failed check's output into test-level failure ids, dispatching on
/// the toolchain. Only Test checks carry test granularity; build/fmt/lint and
/// toolchains without a parser (Node, …) yield an empty set and fall back to
/// exit-code-level baseline attribution.
fn parse_failed_tests(command: &VerificationCommand, output: &str) -> BTreeSet<String> {
    if command.kind != CheckKind::Test {
        return BTreeSet::new();
    }
    let program = program_stem(&command.program);
    match program {
        "cargo" => parse_rust_failures(output),
        "go" => parse_go_failures(output),
        _ => BTreeSet::new(),
    }
}

/// `program` may be a bare name or an absolute path; toolchain dispatch matches
/// on the stem.
fn program_stem(program: &str) -> &str {
    std::path::Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
}

fn combine(stdout: &str, stderr: &str) -> String {
    let mut s = String::new();
    if !stdout.trim().is_empty() {
        s.push_str(stdout);
    }
    if !stderr.trim().is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(stderr);
    }
    s
}

fn truncate(s: &str) -> String {
    // Keep the tail, where compiler/test errors usually are.
    leveler_core::truncate_tail_bytes(s, MAX_EVIDENCE, "…[truncated]\n")
}

fn find_in_path(program: &str, environment: &leveler_core::EnvSnapshot) -> Option<PathBuf> {
    // An explicit path is used directly.
    if program.contains('/') || program.contains('\\') {
        let p = PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    // Windows hosts expose the variable as `Path`; the snapshot keeps the
    // original casing, so look it up case-insensitively there.
    #[cfg(windows)]
    let path = environment.var_os_case_insensitive("PATH")?;
    #[cfg(not(windows))]
    let path = environment.var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
        // Windows executables carry a PATHEXT extension (`cargo` →
        // `cargo.exe`); without this probe every gate reports ToolMissing.
        #[cfg(windows)]
        for ext in pathext_extensions(environment) {
            let candidate = dir.join(format!("{program}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Executable extensions from PATHEXT, without the leading dot, matching how
/// Windows resolves a bare program name. Falls back to the cmd default set.
#[cfg(windows)]
fn pathext_extensions(environment: &leveler_core::EnvSnapshot) -> Vec<String> {
    let value = environment
        .var_os_case_insensitive("PATHEXT")
        .and_then(|v| v.into_string().ok())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
    value
        .split(';')
        .filter_map(|ext| {
            let ext = ext.trim().trim_start_matches('.').trim();
            (!ext.is_empty()).then(|| ext.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {

    /// C1.6 real-toolchain regression (macOS host configured with a global
    /// `rustc-wrapper`): the production Verifier path over a real Rust repo
    /// must pass. Opt-in because it compiles ripgrep's dependency tree.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "runs a full cargo check over the ripgrep fixture; opt in with --ignored"]
    async fn probe_real_repo_gate_under_host_rustc_wrapper() {
        let repo =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/repos/ripgrep");
        if !repo.join("Cargo.toml").exists() {
            eprintln!("skipping: fixtures/repos/ripgrep not fetched");
            return;
        }
        let plan = VerificationPlan {
            commands: vec![VerificationCommand {
                name: "build".into(),
                program: "cargo".into(),
                args: vec!["check".into(), "--quiet".into(), "--offline".into()],
                kind: CheckKind::Build,
                gating: true,
                timeout_seconds: 900,
                scope_policy: ScopePolicy::Exact,
            }],
        };
        let environment = std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os().collect::<Vec<_>>(),
            std::env::current_dir().unwrap(),
            std::env::temp_dir(),
        ));
        let report = Verifier::with_environment(repo.canonicalize().unwrap(), environment)
            .verify(&plan, &[], &[], &CancellationToken::new(), &mut |_| {})
            .await;
        let check = &report.checks[0];
        assert_eq!(
            check.status,
            CheckStatus::Passed,
            "a real repository gate must not fail on the host's compilation cache: {}",
            &check.evidence[..check.evidence.len().min(600)]
        );
    }

    use super::*;

    fn cmd(name: &str, program: &str, args: &[&str], gating: bool) -> VerificationCommand {
        VerificationCommand {
            name: name.into(),
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            kind: CheckKind::Build,
            gating,
            timeout_seconds: 30,
            scope_policy: ScopePolicy::Auto,
        }
    }

    #[test]
    fn scope_allows_files_under_allowed_dir() {
        let (ok, v) = check_scope(&["src".into()], &["src/lib.rs".into()]);
        assert!(ok);
        assert!(v.is_empty());
    }

    fn sv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scope_narrows_go_glob_to_changed_packages() {
        let root = root_with(&["errors"]);
        let args = sv(&["test", "./..."]);
        let scoped = scope_args(
            &args,
            &["errors/x.go".into(), "errors/y.go".into()],
            root.path(),
        );
        assert_eq!(scoped, sv(&["test", "./errors/..."]));
    }

    #[test]
    fn scope_handles_multiple_packages() {
        let root = root_with(&["a", "b"]);
        let scoped = scope_args(
            &sv(&["test", "./..."]),
            &["a/x.go".into(), "b/y.go".into()],
            root.path(),
        );
        assert_eq!(scoped, sv(&["test", "./a/...", "./b/..."]));
    }

    /// F — a root-level change cannot be narrowed.
    #[test]
    fn scope_falls_back_to_full_on_root_change() {
        let root = root_with(&[]);
        let args = sv(&["test", "./..."]);
        let scoped = scope_args(&args, &["main.go".into()], root.path());
        assert_eq!(scoped, args);
    }

    #[test]
    fn scope_leaves_non_glob_commands_untouched() {
        let args = sv(&["check", "--workspace"]);
        assert_eq!(
            scope_args(&args, &["src/lib.rs".into()], Path::new(".")),
            args
        );
    }

    /// A workspace root with `dirs` materialized, for narrowing decisions that
    /// must only ever name a directory that exists.
    fn root_with(dirs: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for d in dirs {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        dir
    }

    /// A — the user declared `go test ./...`; that is the command that runs.
    #[test]
    fn explicit_args_are_never_rewritten() {
        let root = root_with(&["app"]);
        let mut command = test_check("go", &["test", "./..."]);
        command.scope_policy = ScopePolicy::Exact;
        assert_eq!(
            effective_args(&command, &["app/foo.go".into()], root.path()),
            sv(&["test", "./..."])
        );
    }

    /// A (second rewrite path) — an explicit `cargo test` keeps its exact
    /// arguments too; the harness does not slip `--no-fail-fast` in.
    #[test]
    fn explicit_cargo_test_keeps_its_exact_arguments() {
        let root = root_with(&["crates/x/src"]);
        let mut command = test_check("cargo", &["test", "--workspace"]);
        command.scope_policy = ScopePolicy::Exact;
        assert_eq!(
            effective_args(&command, &["crates/x/src/lib.rs".into()], root.path()),
            sv(&["test", "--workspace"])
        );
    }

    /// B — the inferred plan keeps narrowing to the changed package.
    #[test]
    fn inferred_args_are_narrowed_to_the_changed_package() {
        let root = root_with(&["app"]);
        let command = test_check("go", &["test", "./..."]);
        assert_eq!(
            effective_args(&command, &["app/foo.go".into()], root.path()),
            sv(&["test", "./app/..."])
        );
    }

    /// E — the P1 reproduction: a transient directory the run created and
    /// removed is still in `modified_files`, and narrowing to it would build a
    /// target that cannot resolve (`lstat: no such file or directory`). Fall
    /// back to the broader command instead of running a doomed one.
    #[test]
    fn a_vanished_target_falls_back_to_the_broader_command() {
        let root = root_with(&["duration"]);
        let command = test_check("go", &["test", "./..."]);
        let scoped = effective_args(
            &command,
            &[
                "duration/format.go".into(),
                ".acceptance-work/duration/format.go".into(),
            ],
            root.path(),
        );
        assert_eq!(
            scoped,
            sv(&["test", "./..."]),
            "a target that no longer exists must widen the gate, not break it"
        );
    }

    /// G — deleting a source file inside a package that still exists narrows
    /// normally; deleting the whole package widens instead of skipping.
    #[test]
    fn source_deletion_still_verifies() {
        let root = root_with(&["app"]);
        let command = test_check("go", &["test", "./..."]);
        assert_eq!(
            effective_args(&command, &["app/gone.go".into()], root.path()),
            sv(&["test", "./app/..."]),
            "the package survives the file deletion"
        );
        let widened = effective_args(&command, &["oldpkg/gone.go".into()], root.path());
        assert_eq!(
            widened,
            sv(&["test", "./..."]),
            "a removed package must still be verified, just more broadly"
        );
    }

    fn test_check(program: &str, args: &[&str]) -> VerificationCommand {
        VerificationCommand {
            name: format!("{program} test"),
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            kind: CheckKind::Test,
            gating: true,
            timeout_seconds: 30,
            scope_policy: ScopePolicy::Auto,
        }
    }

    #[test]
    fn cargo_test_gains_no_fail_fast() {
        // Fail-fast truncates the failed-test set baseline attribution diffs;
        // the executed command must always carry --no-fail-fast.
        let c = test_check("cargo", &["test", "--workspace", "--quiet"]);
        assert_eq!(
            with_no_fail_fast(&c, c.args.clone()),
            sv(&["test", "--workspace", "--quiet", "--no-fail-fast"])
        );
    }

    #[test]
    fn no_fail_fast_goes_before_the_harness_separator() {
        // After `--` the args belong to the test harness, which rejects the
        // flag — it must stay on cargo's side.
        let c = test_check("cargo", &["test", "--", "--nocapture"]);
        assert_eq!(
            with_no_fail_fast(&c, c.args.clone()),
            sv(&["test", "--no-fail-fast", "--", "--nocapture"])
        );
    }

    #[test]
    fn no_fail_fast_is_not_duplicated() {
        let c = test_check("cargo", &["test", "--no-fail-fast"]);
        assert_eq!(with_no_fail_fast(&c, c.args.clone()), c.args);
    }

    #[test]
    fn other_toolchains_and_non_test_checks_are_untouched() {
        let go = test_check("go", &["test", "./..."]);
        assert_eq!(with_no_fail_fast(&go, go.args.clone()), go.args);

        // `cargo check` is a Build check — no test flags.
        let build = cmd("cargo check", "cargo", &["check", "--workspace"], true);
        assert_eq!(with_no_fail_fast(&build, build.args.clone()), build.args);

        // A cargo Test check whose subcommand is not `test` (e.g. nextest)
        // takes different flags — leave it alone.
        let nextest = test_check("cargo", &["nextest", "run"]);
        assert_eq!(
            with_no_fail_fast(&nextest, nextest.args.clone()),
            nextest.args
        );
    }

    #[test]
    fn scope_flags_out_of_scope_file() {
        let (ok, v) = check_scope(
            &["src/lib.rs".into()],
            &["src/lib.rs".into(), "src/other.rs".into()],
        );
        assert!(!ok);
        assert_eq!(v, vec!["src/other.rs"]);
    }

    #[tokio::test]
    async fn passing_command_is_passed() {
        let v = Verifier::with_environment(
            std::env::temp_dir(),
            Arc::new(leveler_core::EnvSnapshot::new(
                std::env::vars_os(),
                std::env::current_dir().unwrap_or_default(),
                std::env::temp_dir(),
            )),
        );
        // `true` does not exist on Windows runners; pass via cmd there.
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd", &["/c", "exit 0"])
        } else {
            ("true", &[])
        };
        let plan = VerificationPlan {
            commands: vec![cmd("ok", program, args, true)],
        };
        let mut seen = 0;
        let report = v
            .verify(&plan, &[], &[], &CancellationToken::new(), &mut |_| {
                seen += 1
            })
            .await;
        assert!(report.passed());
        assert_eq!(seen, 1);
    }

    #[tokio::test]
    async fn failing_gating_command_blocks() {
        let v = Verifier::with_environment(
            std::env::temp_dir(),
            Arc::new(leveler_core::EnvSnapshot::new(
                std::env::vars_os(),
                std::env::current_dir().unwrap_or_default(),
                std::env::temp_dir(),
            )),
        );
        // `false` does not exist on Windows runners; fail via cmd there.
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd", &["/c", "exit 1"])
        } else {
            ("false", &[])
        };
        let plan = VerificationPlan {
            commands: vec![cmd("bad", program, args, true)],
        };
        let report = v
            .verify(&plan, &[], &[], &CancellationToken::new(), &mut |_| {})
            .await;
        assert!(!report.passed());
        assert_eq!(report.failed_gates().len(), 1);
    }

    #[test]
    fn acceptance_process_request_is_force_deny_repo_verify_inherits() {
        // Trust matrix unit check (shared helper): acceptance ForceDeny, repo Inherit.
        let root = std::path::PathBuf::from("/tmp/ws");
        let accept = process_request_for_verify_check(
            "sh",
            vec!["-c".into(), "test -d .".into()],
            root.clone(),
            VerifyNetworkPolicy::ForceDeny,
        );
        assert!(accept.deny_network);
        assert!(accept.write_root.is_some());

        let repo = process_request_for_verify_check(
            "cargo",
            vec!["test".into()],
            root,
            VerifyNetworkPolicy::InheritSession,
        );
        assert!(!repo.deny_network);
        assert!(repo.write_root.is_some());
    }

    #[tokio::test]
    async fn missing_program_is_tool_missing_and_unverified() {
        let v = Verifier::new(std::env::temp_dir());
        let plan = VerificationPlan {
            commands: vec![cmd(
                "missing",
                "definitely-not-a-real-program-xyz",
                &[],
                true,
            )],
        };
        let report = v
            .verify(&plan, &[], &[], &CancellationToken::new(), &mut |_| {})
            .await;
        // A missing tool does not fail the gate, but the run is not verified.
        assert!(report.passed());
        assert_eq!(report.checks[0].status, CheckStatus::ToolMissing);
        assert!(matches!(
            report.verdict(),
            crate::report::Verdict::Unverified(_)
        ));
    }

    /// R005 F-P1 accident: the gate ran bare `cargo` under the host's default
    /// rustc 1.96 while the task's tree required 1.97, and the MSRV refusal
    /// was reported as a CODE failure gating completion. The mismatch must be
    /// classified as environment, not code: the gate stays open (not Failed)
    /// and the verdict is honestly Unverified with an environment reason.
    #[tokio::test]
    async fn toolchain_mismatch_is_environment_unavailable_not_a_code_failure() {
        let v = Verifier::with_environment(
            std::env::temp_dir(),
            Arc::new(leveler_core::EnvSnapshot::new(
                std::env::vars_os(),
                std::env::current_dir().unwrap_or_default(),
                std::env::temp_dir(),
            )),
        );
        let msrv = "error: rustc 1.96.0 is not supported by the following packages: foo@0.1.0 requires rustc 1.97";
        let (program, args): (&str, Vec<String>) = if cfg!(windows) {
            ("cmd", vec!["/c".into(), format!("echo {msrv}& exit 1")])
        } else {
            (
                "sh",
                vec!["-c".into(), format!("echo '{msrv}' >&2; exit 1")],
            )
        };
        let plan = VerificationPlan {
            commands: vec![VerificationCommand {
                name: "build".into(),
                kind: CheckKind::Build,
                program: program.into(),
                args,
                timeout_seconds: 30,
                gating: true,
                scope_policy: ScopePolicy::Auto,
            }],
        };
        let report = v
            .verify(&plan, &[], &[], &CancellationToken::new(), &mut |_| {})
            .await;
        assert_eq!(
            report.checks[0].status,
            CheckStatus::EnvironmentUnavailable,
            "evidence: {}",
            report.checks[0].evidence
        );
        // Environment mismatch must not gate as a code failure…
        assert!(report.passed());
        assert!(report.failed_gates().is_empty());
        // …but the run is NOT silently "verified" either.
        match report.verdict() {
            crate::report::Verdict::Unverified(reason) => {
                assert!(
                    reason.contains("environment mismatch"),
                    "reason should name the environment: {reason}"
                );
            }
            other => panic!("expected Unverified, got {other:?}"),
        }
        // Provenance: the evidence names the resolved binary that actually ran.
        assert!(
            report.checks[0].evidence.contains("ran `"),
            "evidence should carry binary provenance: {}",
            report.checks[0].evidence
        );
    }

    /// Bare program names on Windows resolve via PATHEXT (`gate` → `gate.exe`),
    /// and the path variable arrives as `Path` — not `PATH` — on real hosts.
    #[cfg(windows)]
    #[test]
    fn find_in_path_probes_pathext_extensions() {
        use std::ffi::OsString;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gate.exe"), b"").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        let env = leveler_core::EnvSnapshot::new(
            vec![
                (OsString::from("Path"), path),
                (OsString::from("PATHEXT"), OsString::from(".COM;.EXE")),
            ],
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        );
        assert!(find_in_path("gate", &env).is_some());
        assert!(find_in_path("missing-gate", &env).is_none());
    }
}
