//! The verifier: runs the plan's checks, captures evidence, and enforces scope.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use leveler_execution::{
    CommandClass, CommandRunner, CommandView, VerifyNetworkPolicy, classify_command,
    is_comment_only_acceptance_command, is_trivial_acceptance_command,
    process_request_for_verify_check,
};

use std::collections::BTreeSet;

use crate::failure::classify;
use crate::plan::{CheckKind, VerificationCommand, VerificationPlan};
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
        if find_in_path(&command.program, &self.environment).is_none() {
            return CheckOutcome {
                name: command.name.clone(),
                kind: command.kind,
                gating: command.gating,
                status: CheckStatus::ToolMissing,
                evidence: format!("`{}` not found on PATH", command.program),
                failure: None,
                failed_tests: BTreeSet::new(),
            };
        }

        // Narrow whole-repo commands to the changed packages (spec §29.5).
        let args = scope_args(&command.args, modified_files);
        // Complete failure sets for baseline attribution (see with_no_fail_fast).
        let args = with_no_fail_fast(command, args);
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

    /// Evaluate acceptance criteria into a command-backed evidence ledger.
    ///
    /// Model `verification_hint` is never HostTrusted (K3/K14):
    /// - empty/missing → Unverifiable (`reject_reason: no_command`)
    /// - trivial no-op (`true`, `echo`, …) → Unverifiable (`trivial`), not run
    /// - `classify_command` Dangerous → Unverifiable (`dangerous`), not run
    /// - otherwise sandboxed: write_root + deny_network + credential scrub;
    ///   exit 0 → Met, else Unmet
    ///
    /// v1 has no interactive Approver for acceptance.
    pub async fn evaluate_acceptance(
        &self,
        checks: &[crate::acceptance::AcceptanceCheck],
        cancellation: &CancellationToken,
    ) -> crate::acceptance::AcceptanceLedger {
        use crate::acceptance::{AcceptanceEvidence, AcceptanceLedger, AcceptanceStatus};

        let mut items = Vec::new();
        for check in checks {
            let command = check
                .command
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty());
            let (status, evidence, reject_reason) = match command {
                None => (
                    AcceptanceStatus::Unverifiable,
                    String::new(),
                    Some("no_command"),
                ),
                // Pure `# comment` (no executable body) is the same self-proof
                // class as an empty hint — never Met via `sh -c` exit 0.
                Some(cmd) if is_comment_only_acceptance_command(cmd) => (
                    AcceptanceStatus::Unverifiable,
                    "rejected: no executable acceptance command".to_string(),
                    Some("no_command"),
                ),
                Some(_) if cancellation.is_cancelled() => (
                    AcceptanceStatus::Unverifiable,
                    "cancelled".to_string(),
                    Some("cancelled"),
                ),
                Some(cmd) if is_trivial_acceptance_command(cmd) => (
                    AcceptanceStatus::Unverifiable,
                    "rejected: trivial acceptance command".to_string(),
                    Some("trivial"),
                ),
                Some(cmd) => {
                    // Hints run through the platform's script shell: `sh -c`
                    // on Unix, `cmd /c` on Windows (no POSIX shell there).
                    let (shell, flag) = if cfg!(windows) {
                        ("cmd", "/c")
                    } else {
                        ("sh", "-c")
                    };
                    let shell_args = vec![flag.to_string(), cmd.to_string()];
                    let view = CommandView {
                        program: shell,
                        args: &shell_args,
                    };
                    // Publish/push is auto-run interactively (sandbox-first)
                    // but stays refused here: acceptance checks run unattended
                    // and must never have remote side effects.
                    if classify_command(&view) == CommandClass::Dangerous
                        || leveler_execution::is_remote_publish_command(&view)
                    {
                        (
                            AcceptanceStatus::Unverifiable,
                            "rejected: dangerous acceptance command".to_string(),
                            Some("dangerous"),
                        )
                    } else {
                        // Sandbox: write confinement + force deny network (K12/K3).
                        let mut request = process_request_for_verify_check(
                            shell,
                            shell_args,
                            self.workspace_root.clone(),
                            VerifyNetworkPolicy::ForceDeny,
                        );
                        request.timeout = Duration::from_secs(120);
                        match self.runner.run(request, cancellation.child_token()).await {
                            Ok(output) => {
                                let combined = combine(&output.stdout, &output.stderr);
                                let status = if output.success() {
                                    AcceptanceStatus::Met
                                } else {
                                    AcceptanceStatus::Unmet
                                };
                                (status, truncate(&combined), None)
                            }
                            Err(e) => {
                                (AcceptanceStatus::Unmet, format!("failed to run: {e}"), None)
                            }
                        }
                    }
                }
            };
            items.push(AcceptanceEvidence {
                id: check.id.clone(),
                description: check.description.clone(),
                required: check.required,
                status,
                command: command.map(str::to_string),
                evidence,
                reject_reason: reject_reason.map(str::to_string),
            });
        }
        AcceptanceLedger { items }
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

/// Narrow a whole-repo package glob (`./...`) to just the packages containing the
/// modified files (spec §29.5: prefer targeted → module → full). Falls back to
/// the original args when it can't scope safely (e.g. a root-level change).
fn scope_args(args: &[String], modified_files: &[String]) -> Vec<String> {
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
    if s.len() <= MAX_EVIDENCE {
        return s.to_string();
    }
    // Keep the tail, where compiler/test errors usually are.
    let start = s.len() - MAX_EVIDENCE;
    let mut boundary = start;
    while !s.is_char_boundary(boundary) {
        boundary += 1;
    }
    format!("…[truncated]\n{}", &s[boundary..])
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
    use super::*;
    use crate::plan::CheckKind;

    fn cmd(name: &str, program: &str, args: &[&str], gating: bool) -> VerificationCommand {
        VerificationCommand {
            name: name.into(),
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            kind: CheckKind::Build,
            gating,
            timeout_seconds: 30,
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
        let args = sv(&["test", "./..."]);
        let scoped = scope_args(&args, &["errors/x.go".into(), "errors/y.go".into()]);
        assert_eq!(scoped, sv(&["test", "./errors/..."]));
    }

    #[test]
    fn scope_handles_multiple_packages() {
        let scoped = scope_args(&sv(&["test", "./..."]), &["a/x.go".into(), "b/y.go".into()]);
        assert_eq!(scoped, sv(&["test", "./a/...", "./b/..."]));
    }

    #[test]
    fn scope_falls_back_to_full_on_root_change() {
        let args = sv(&["test", "./..."]);
        let scoped = scope_args(&args, &["main.go".into()]);
        assert_eq!(scoped, args);
    }

    #[test]
    fn scope_leaves_non_glob_commands_untouched() {
        let args = sv(&["check", "--workspace"]);
        assert_eq!(scope_args(&args, &["src/lib.rs".into()]), args);
    }

    fn test_check(program: &str, args: &[&str]) -> VerificationCommand {
        VerificationCommand {
            name: format!("{program} test"),
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            kind: CheckKind::Test,
            gating: true,
            timeout_seconds: 30,
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

    // Unix-only: exercises the acceptance state machine (exit 0 → Met, nonzero →
    // Unmet, no command → Unverifiable) by actually spawning `sh -c`. The logic
    // in `evaluate_acceptance` is platform-independent; on Windows the passing
    // (`Met`) path proved unreproducible through `cmd /c` under the verifier
    // sandbox (every candidate command returned nonzero). Windows keeps coverage
    // of the Unmet path (`failing_gating_command_blocks`) and the trivial /
    // no-command paths (`acceptance_rejects_trivial_and_dangerous_without_running`,
    // which never spawn).
    #[cfg(unix)]
    #[tokio::test]
    async fn acceptance_command_exit_zero_is_met_nonzero_unmet_absent_unverifiable() {
        use crate::acceptance::{AcceptanceCheck, AcceptanceStatus};
        // Library tests intentionally have no installed global environment
        // capability. Acceptance commands are confined and therefore need the
        // same explicit host snapshot supplied by the application root.
        let v = Verifier::with_environment(
            std::env::temp_dir(),
            Arc::new(leveler_core::EnvSnapshot::new(
                std::env::vars_os(),
                std::env::current_dir().unwrap_or_default(),
                std::env::temp_dir(),
            )),
        );
        let checks = vec![
            AcceptanceCheck {
                id: "AC-1".into(),
                description: "passes".into(),
                // Non-trivial (a real path test, not the rejected `true`) and
                // exits 0 → Met.
                command: Some("test -d .".into()),
                required: true,
            },
            AcceptanceCheck {
                id: "AC-2".into(),
                description: "fails".into(),
                command: Some("echo boom >&2; exit 1".into()),
                required: true,
            },
            AcceptanceCheck {
                id: "AC-3".into(),
                description: "not command-checkable".into(),
                command: None,
                required: false,
            },
            AcceptanceCheck {
                id: "AC-4".into(),
                description: "blank hint".into(),
                command: Some("   ".into()),
                required: false,
            },
        ];
        let ledger = v
            .evaluate_acceptance(&checks, &CancellationToken::new())
            .await;

        assert_eq!(ledger.items[0].status, AcceptanceStatus::Met);
        assert_eq!(ledger.items[0].reject_reason, None);
        assert_eq!(ledger.items[1].status, AcceptanceStatus::Unmet);
        assert!(ledger.items[1].evidence.contains("boom"));
        assert_eq!(ledger.items[2].status, AcceptanceStatus::Unverifiable);
        assert_eq!(ledger.items[2].reject_reason.as_deref(), Some("no_command"));
        assert_eq!(ledger.items[3].status, AcceptanceStatus::Unverifiable);
        assert_eq!(ledger.items[3].reject_reason.as_deref(), Some("no_command"));
        // A required Unmet (AC-2) means the acceptance bar was not proven.
        assert!(ledger.has_required_unmet());
        assert_eq!(ledger.unmet_required().len(), 1);
    }

    #[tokio::test]
    async fn acceptance_rejects_trivial_and_dangerous_without_running() {
        use crate::acceptance::{AcceptanceCheck, AcceptanceStatus};
        let v = Verifier::new(std::env::temp_dir());
        let checks = vec![
            AcceptanceCheck {
                id: "AC-true".into(),
                description: "self-proof true".into(),
                command: Some("true".into()),
                required: true,
            },
            AcceptanceCheck {
                id: "AC-echo".into(),
                description: "self-proof echo".into(),
                command: Some("echo ok".into()),
                required: true,
            },
            AcceptanceCheck {
                id: "AC-rm".into(),
                description: "dangerous".into(),
                command: Some("rm -rf /tmp/x".into()),
                required: true,
            },
            AcceptanceCheck {
                id: "AC-push".into(),
                description: "irreversible publish".into(),
                command: Some("git push origin main".into()),
                required: false,
            },
            // Nested shell must not launder dangerous (classify recursion).
            AcceptanceCheck {
                id: "AC-nested-rm".into(),
                description: "nested bash -c rm".into(),
                command: Some("bash -c 'rm -rf x'".into()),
                required: true,
            },
            // Pure comments must not Met via sh -c exit 0.
            AcceptanceCheck {
                id: "AC-comment".into(),
                description: "comment-only".into(),
                command: Some("# criterion holds".into()),
                required: true,
            },
            AcceptanceCheck {
                id: "AC-comment-ws".into(),
                description: "comment with leading space".into(),
                command: Some("   # bar".into()),
                required: false,
            },
        ];
        let ledger = v
            .evaluate_acceptance(&checks, &CancellationToken::new())
            .await;

        assert_eq!(ledger.items[0].status, AcceptanceStatus::Unverifiable);
        assert_eq!(ledger.items[0].reject_reason.as_deref(), Some("trivial"));
        assert_eq!(ledger.items[1].status, AcceptanceStatus::Unverifiable);
        assert_eq!(ledger.items[1].reject_reason.as_deref(), Some("trivial"));
        assert_eq!(ledger.items[2].status, AcceptanceStatus::Unverifiable);
        assert_eq!(ledger.items[2].reject_reason.as_deref(), Some("dangerous"));
        assert_eq!(ledger.items[3].status, AcceptanceStatus::Unverifiable);
        assert_eq!(ledger.items[3].reject_reason.as_deref(), Some("dangerous"));
        assert_eq!(ledger.items[4].status, AcceptanceStatus::Unverifiable);
        assert_eq!(ledger.items[4].reject_reason.as_deref(), Some("dangerous"));
        assert_eq!(ledger.items[5].status, AcceptanceStatus::Unverifiable);
        assert_eq!(ledger.items[5].reject_reason.as_deref(), Some("no_command"));
        assert_eq!(ledger.items[6].status, AcceptanceStatus::Unverifiable);
        assert_eq!(ledger.items[6].reject_reason.as_deref(), Some("no_command"));
        // Rejected criteria never "ran" as a successful check (Unmet), but
        // required Unverifiable still blocks all_required_met (K2).
        assert!(!ledger.has_required_unmet());
        assert!(ledger.has_required_unverifiable());
        assert!(!ledger.all_required_met());
    }

    #[tokio::test]
    async fn acceptance_cancelled_is_unverifiable() {
        use crate::acceptance::{AcceptanceCheck, AcceptanceStatus};
        let v = Verifier::new(std::env::temp_dir());
        let token = CancellationToken::new();
        token.cancel();
        let checks = vec![AcceptanceCheck {
            id: "AC-1".into(),
            description: "would pass".into(),
            command: Some("test -d .".into()),
            required: true,
        }];
        let ledger = v.evaluate_acceptance(&checks, &token).await;
        assert_eq!(ledger.items[0].status, AcceptanceStatus::Unverifiable);
        assert_eq!(ledger.items[0].reject_reason.as_deref(), Some("cancelled"));
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
