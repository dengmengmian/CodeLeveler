//! The verification plan: which commands to run to check a change (spec §29).

use serde::{Deserialize, Serialize};

use leveler_lifecycle::is_build_relevant;
use leveler_project::Language;

/// The category of a verification command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    /// Auto-formatting (best-effort, non-gating).
    Format,
    /// Compilation / type checking (gating).
    Build,
    /// Test execution (gating).
    Test,
    /// Linting (gating).
    Lint,
}

/// A single verification command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationCommand {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub kind: CheckKind,
    /// Whether failure blocks completion. Format is non-gating (best effort).
    pub gating: bool,
    /// Timeout in seconds.
    pub timeout_seconds: u64,
}

impl VerificationCommand {
    pub(crate) fn new(
        name: &str,
        program: &str,
        args: &[&str],
        kind: CheckKind,
        gating: bool,
    ) -> Self {
        Self {
            name: name.to_string(),
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            kind,
            gating,
            timeout_seconds: 600,
        }
    }
}

/// An ordered set of verification commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VerificationPlan {
    pub commands: Vec<VerificationCommand>,
}

impl VerificationPlan {
    /// Build a default plan from the detected languages. Format runs first
    /// (best-effort), then the gating build and test checks.
    pub fn for_languages(languages: &[Language]) -> Self {
        let mut commands = Vec::new();
        for lang in languages {
            match lang {
                Language::Rust => {
                    commands.push(VerificationCommand::new(
                        "cargo fmt",
                        "cargo",
                        &["fmt", "--all", "--", "--check"],
                        CheckKind::Format,
                        false,
                    ));
                    commands.push(VerificationCommand::new(
                        "cargo check",
                        "cargo",
                        &["check", "--workspace", "--quiet"],
                        CheckKind::Build,
                        true,
                    ));
                    commands.push(VerificationCommand::new(
                        "cargo test",
                        "cargo",
                        &["test", "--workspace", "--quiet"],
                        CheckKind::Test,
                        true,
                    ));
                }
                Language::Go => {
                    commands.push(VerificationCommand::new(
                        "gofmt",
                        "gofmt",
                        &["-l", "."],
                        CheckKind::Format,
                        false,
                    ));
                    commands.push(VerificationCommand::new(
                        "go build",
                        "go",
                        &["build", "./..."],
                        CheckKind::Build,
                        true,
                    ));
                    commands.push(VerificationCommand::new(
                        "go test",
                        "go",
                        &["test", "./..."],
                        CheckKind::Test,
                        true,
                    ));
                }
                Language::Python => {
                    // Best-effort format; a byte-compile as a safe build gate.
                    // Missing tools are ToolMissing (non-blocking but unverified),
                    // and repos with a real test runner should declare it in
                    // .leveler/config.yaml.
                    //
                    // Keep the inferred gate scoped to repository source. Virtual
                    // environments and cache/dependency directories frequently
                    // contain third-party code for a different interpreter, and
                    // compiling them turns verification into an environment check.
                    commands.push(VerificationCommand::new(
                        "ruff format",
                        "ruff",
                        &["format", "--check", "."],
                        CheckKind::Format,
                        false,
                    ));
                    commands.push(VerificationCommand::new(
                        "py compile",
                        "python3",
                        &[
                            "-m",
                            "compileall",
                            "-q",
                            "-x",
                            r"(^|/)(\.venv|venv|env|\.tox|\.nox|node_modules|__pycache__|\.git|\.pytest_cache|\.mypy_cache|\.ruff_cache)(/|$)",
                            ".",
                        ],
                        CheckKind::Build,
                        true,
                    ));
                }
                // These are detected for context/planning; their build/test
                // commands vary too much to guess, so they are verified via
                // `.leveler/config.yaml` (spec §37).
                Language::TypeScript
                | Language::JavaScript
                | Language::Java
                | Language::Ruby
                | Language::CSharp
                | Language::Cpp => {}
            }
        }
        Self { commands }
    }

    /// Whether the plan has any gating checks.
    pub fn has_gates(&self) -> bool {
        self.commands.iter().any(|c| c.gating)
    }

    /// Downgrade build/test/lint gates to non-gating when the change cannot
    /// affect them, so a whole-workspace `cargo test` (etc.) is neither run nor
    /// blamed for its pre-existing red on a commit that only touched docs,
    /// scripts, lock files, or other non-compiled inputs.
    ///
    /// Conservative by construction — the gates stand unless EVERY modified file
    /// is provably inert. The heuristic itself lives in
    /// [`leveler_lifecycle::is_build_relevant`] (shared with the readiness gate,
    /// so both layers judge "does this change need verification" identically);
    /// see its docs for the full trade-off. Empty `modified_files` (unknown
    /// blast radius) keeps the gates.
    pub fn scope_gates_to_changes(&mut self, modified_files: &[String]) {
        if modified_files.is_empty() || modified_files.iter().any(|f| is_build_relevant(f)) {
            return;
        }
        for cmd in &mut self.commands {
            if matches!(
                cmd.kind,
                CheckKind::Build | CheckKind::Test | CheckKind::Lint
            ) {
                cmd.gating = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_plan_has_build_and_test_gates() {
        let plan = VerificationPlan::for_languages(&[Language::Rust]);
        assert!(plan.has_gates());
        let gates: Vec<_> = plan
            .commands
            .iter()
            .filter(|c| c.gating)
            .map(|c| c.kind)
            .collect();
        assert!(gates.contains(&CheckKind::Build));
        assert!(gates.contains(&CheckKind::Test));
    }

    #[test]
    fn default_format_commands_are_check_only() {
        let rust = VerificationPlan::for_languages(&[Language::Rust]);
        let cargo_fmt = rust
            .commands
            .iter()
            .find(|c| c.name == "cargo fmt")
            .expect("cargo fmt");
        assert!(
            cargo_fmt.args.iter().any(|a| a == "--check"),
            "cargo fmt must use --check, got {:?}",
            cargo_fmt.args
        );
        assert!(!cargo_fmt.gating);

        let go = VerificationPlan::for_languages(&[Language::Go]);
        let gofmt = go
            .commands
            .iter()
            .find(|c| c.name == "gofmt")
            .expect("gofmt");
        assert_eq!(gofmt.args, vec!["-l".to_string(), ".".to_string()]);
        assert!(!gofmt.args.iter().any(|a| a == "-w"));

        let py = VerificationPlan::for_languages(&[Language::Python]);
        let ruff = py
            .commands
            .iter()
            .find(|c| c.name == "ruff format")
            .expect("ruff format");
        assert!(
            ruff.args.iter().any(|a| a == "--check"),
            "ruff format must use --check, got {:?}",
            ruff.args
        );
    }

    #[test]
    fn empty_languages_has_no_gates() {
        assert!(!VerificationPlan::for_languages(&[]).has_gates());
    }

    // ── Blast-radius gate scoping ─────────────────────────────────────────
    // A change that cannot affect compilation/tests must not run (and be blamed
    // by) the whole-workspace `cargo test`.

    fn gating_kinds(plan: &VerificationPlan) -> Vec<CheckKind> {
        plan.commands
            .iter()
            .filter(|c| c.gating)
            .map(|c| c.kind)
            .collect()
    }

    #[test]
    fn inert_change_downgrades_all_gates() {
        let mut plan = VerificationPlan::for_languages(&[Language::Rust]);
        plan.scope_gates_to_changes(&["scripts/foo.sh".into(), "docs/notes.md".into()]);
        assert!(
            !plan.has_gates(),
            "docs+script change must not gate cargo test, got {:?}",
            gating_kinds(&plan)
        );
        // The commands are still present (for evidence), just non-gating.
        assert!(plan.commands.iter().any(|c| c.name == "cargo test"));
    }

    #[test]
    fn screenshot_case_sh_lock_and_symlink_are_inert() {
        // The exact reported case: a shell script, its `.leveler-lock`, and an
        // extensionless symlink — none touch Rust.
        let mut plan = VerificationPlan::for_languages(&[Language::Rust]);
        plan.scope_gates_to_changes(&[
            "scripts/atomcode-doc-test-audit.sh".into(),
            "scripts/.atomcode-doc-test-audit.sh.leveler-lock".into(),
            "到桌面".into(),
        ]);
        assert!(!plan.has_gates(), "got {:?}", gating_kinds(&plan));
    }

    #[test]
    fn empty_modified_keeps_gates() {
        let mut plan = VerificationPlan::for_languages(&[Language::Rust]);
        plan.scope_gates_to_changes(&[]);
        assert!(plan.has_gates(), "unknown blast radius must keep gates");
    }

    #[test]
    fn rust_source_change_keeps_gates() {
        let mut plan = VerificationPlan::for_languages(&[Language::Rust]);
        plan.scope_gates_to_changes(&["crates/x/src/lib.rs".into()]);
        assert!(plan.has_gates());
    }

    #[test]
    fn manifest_change_keeps_gates() {
        let mut plan = VerificationPlan::for_languages(&[Language::Rust]);
        plan.scope_gates_to_changes(&["Cargo.toml".into()]);
        assert!(plan.has_gates());
    }

    #[test]
    fn any_relevant_file_keeps_gates_even_amid_docs() {
        let mut plan = VerificationPlan::for_languages(&[Language::Rust]);
        plan.scope_gates_to_changes(&["README.md".into(), "crates/x/src/main.rs".into()]);
        assert!(plan.has_gates(), "one source file keeps the whole gate");
    }

    #[test]
    fn fixture_under_tests_dir_keeps_gates() {
        // A non-source extension living under a source tree may be an
        // include_str! input, so it stays build-relevant.
        let mut plan = VerificationPlan::for_languages(&[Language::Rust]);
        plan.scope_gates_to_changes(&["crates/x/tests/data/golden.txt".into()]);
        assert!(plan.has_gates());
    }

    #[test]
    fn python_compile_gate_skips_virtualenvs_and_caches() {
        let plan = VerificationPlan::for_languages(&[Language::Python]);
        let compile = plan
            .commands
            .iter()
            .find(|c| c.name == "py compile")
            .expect("python plan should include compile gate");

        assert_eq!(compile.program, "python3");
        assert_eq!(compile.kind, CheckKind::Build);
        assert!(compile.gating);
        assert!(compile.args.iter().any(|arg| arg == "-x"));
        let exclude = compile
            .args
            .iter()
            .skip_while(|arg| *arg != "-x")
            .nth(1)
            .expect("compileall -x should include an exclude regex");
        assert!(exclude.contains(r"\.venv"));
        assert!(exclude.contains("node_modules"));
        assert!(exclude.contains("__pycache__"));
    }
}
