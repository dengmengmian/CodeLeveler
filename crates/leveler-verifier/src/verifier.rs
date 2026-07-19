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

use crate::failure::classify;
use crate::plan::{VerificationCommand, VerificationPlan};
use crate::report::{CheckOutcome, CheckStatus, VerificationReport};

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
            };
        }

        // Narrow whole-repo commands to the changed packages (spec §29.5).
        let args = scope_args(&command.args, modified_files);
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
                    }
                }
            }
            Err(e) => CheckOutcome {
                name: command.name.clone(),
                kind: command.kind,
                gating: command.gating,
                status: CheckStatus::Failed,
                evidence: format!("failed to run: {e}"),
                failure: Some(classify(command.kind, &e.to_string())),
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
                    let shell_args = vec!["-c".to_string(), cmd.to_string()];
                    let view = CommandView {
                        program: "sh",
                        args: &shell_args,
                    };
                    if classify_command(&view) == CommandClass::Dangerous {
                        (
                            AcceptanceStatus::Unverifiable,
                            "rejected: dangerous acceptance command".to_string(),
                            Some("dangerous"),
                        )
                    } else {
                        // Sandbox: write confinement + force deny network (K12/K3).
                        let mut request = process_request_for_verify_check(
                            "sh",
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
        let plan = VerificationPlan {
            commands: vec![cmd("ok", "true", &[], true)],
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
        let plan = VerificationPlan {
            commands: vec![cmd("bad", "false", &[], true)],
        };
        let report = v
            .verify(&plan, &[], &[], &CancellationToken::new(), &mut |_| {})
            .await;
        assert!(!report.passed());
        assert_eq!(report.failed_gates().len(), 1);
    }

    #[tokio::test]
    async fn acceptance_command_exit_zero_is_met_nonzero_unmet_absent_unverifiable() {
        use crate::acceptance::{AcceptanceCheck, AcceptanceStatus};
        let v = Verifier::new(std::env::temp_dir());
        let checks = vec![
            AcceptanceCheck {
                id: "AC-1".into(),
                description: "passes".into(),
                // Non-trivial: real path check (not `true`, which is rejected).
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
