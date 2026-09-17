//! Failure classification and recovery strategy (spec §31, §32).

use serde::{Deserialize, Serialize};

use crate::plan::CheckKind;

/// A coarse classification of a verification failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    SyntaxError,
    TypeError,
    BuildFailure,
    TestFailure,
    LintFailure,
    EnvironmentFailure,
    Timeout,
    Unknown,
}

/// The recommended recovery action. The verifier acts on
/// `RepairCurrentNode`; the others are modeled for later phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStrategy {
    RepairCurrentNode,
    Relocalize,
    RestoreCheckpoint,
    EscalateModel,
    StopAndReport,
}

/// A classified failure with evidence pointers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifiedFailure {
    pub kind: FailureKind,
    pub summary: String,
    pub likely_files: Vec<String>,
    pub retryable: bool,
    pub suggested_recovery: RecoveryStrategy,
}

/// Whether a check's output is a toolchain/version mismatch between the
/// verification environment and what the tree requires — cargo's MSRV refusal
/// or a missing rustup toolchain (R005 F-P1: the gate ran the HOST default
/// rustc against a tree pinned newer, and the refusal was mis-reported as a
/// code failure). Deliberately narrow: only compiler/toolchain wording trips
/// it, so ordinary build/test failures keep gating.
pub fn is_environment_mismatch(output: &str) -> bool {
    let lower = output.to_lowercase();
    // cargo ≥1.56 MSRV refusal: "rustc 1.96.0 is not supported by the
    // following package(s): …" — cargo uses the singular "package" for one
    // offender, so match the singular prefix (contained in the plural too).
    lower.contains("is not supported by the following package")
        // older cargo wording: "requires rustc 1.97.0 or newer, while the
        // currently active rustc version is 1.96.0".
        || (lower.contains("requires rustc") && lower.contains("currently active rustc version"))
        // rustup: "toolchain '1.97.0-…' is not installed".
        || (lower.contains("toolchain") && lower.contains("is not installed"))
}

/// Classify a failed check from its command kind and captured output.
pub fn classify(kind: CheckKind, output: &str) -> ClassifiedFailure {
    let likely_files = extract_paths(output);
    let lower = output.to_lowercase();

    if is_environment_mismatch(output) {
        return ClassifiedFailure {
            kind: FailureKind::EnvironmentFailure,
            summary: "toolchain mismatch: the verification environment's compiler \
                      does not satisfy what the tree requires — not a code defect"
                .to_string(),
            likely_files,
            retryable: false,
            suggested_recovery: RecoveryStrategy::StopAndReport,
        };
    }

    // Environment problems are not the code's fault — don't try to "repair".
    //
    // R003 F1: a correct hugo fix was recorded `failed` because the repo-wide
    // suite exhausted the sandbox disk and wanted a global npm package.
    // Exhausted resources and absent tooling say nothing about the code, so
    // they must never reach the repair loop as a defect.
    if lower.contains("command not found")
        || lower.contains("could not find `cargo.toml`")
        || lower.contains("no such file or directory")
        || lower.contains("permission denied")
        // resource exhaustion
        || lower.contains("no space left on device")
        || lower.contains("os error 28")
        || lower.contains("resource temporarily unavailable")
        || lower.contains("too many open files")
        || lower.contains("os error 24")
        || lower.contains("out of memory")
        || lower.contains("cannot allocate memory")
        || lower.contains("killed: 9")
        // a toolchain that cannot even start: rustup/cargo refusing because its
        // home is not creatable is a fact about the machine, never about the
        // tree being verified (seen on a Windows runner whose profile
        // directory the sandbox could not create).
        || lower.contains("could not create home directory")
        // tooling the environment does not provide
        || lower.contains("executable not found")
        || lower.contains("externally-managed-environment")
        || lower.contains("npm err! code enoent")
    {
        return ClassifiedFailure {
            kind: FailureKind::EnvironmentFailure,
            summary: "environment/tooling problem, not a code defect".to_string(),
            likely_files,
            retryable: false,
            suggested_recovery: RecoveryStrategy::StopAndReport,
        };
    }

    let (failure_kind, summary) = match kind {
        CheckKind::Test => (FailureKind::TestFailure, "tests failed".to_string()),
        CheckKind::Lint => (
            FailureKind::LintFailure,
            "lint reported problems".to_string(),
        ),
        CheckKind::Format => (
            FailureKind::LintFailure,
            "formatting check failed".to_string(),
        ),
        CheckKind::Build => {
            // Rust type errors carry an `error[Exxxx]` code.
            if output.contains("error[E") {
                (FailureKind::TypeError, "compilation type error".to_string())
            } else if lower.contains("expected") || lower.contains("syntax") {
                (FailureKind::SyntaxError, "syntax error".to_string())
            } else {
                (FailureKind::BuildFailure, "build failed".to_string())
            }
        }
    };

    ClassifiedFailure {
        kind: failure_kind,
        summary,
        likely_files,
        retryable: true,
        suggested_recovery: RecoveryStrategy::RepairCurrentNode,
    }
}

/// Pull out `path.ext:line` style references (rust/go/ts) as likely-culprit files.
fn extract_paths(output: &str) -> Vec<String> {
    const EXTS: &[&str] = &[".rs:", ".go:", ".ts:", ".tsx:", ".js:"];
    let mut files = Vec::new();
    for raw in output.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '"') {
        let token = raw.trim_start_matches("-->").trim();
        for ext in EXTS {
            if let Some(pos) = token.find(ext) {
                // Keep the path up to (and including) the extension.
                let end = pos + ext.len() - 1; // drop the trailing ':'
                let path = &token[..end];
                if !path.is_empty() && !files.iter().any(|f| f == path) {
                    files.push(path.to_string());
                }
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_type_error_is_classified() {
        let output = "error[E0308]: mismatched types\n --> src/lib.rs:12:5";
        let f = classify(CheckKind::Build, output);
        assert_eq!(f.kind, FailureKind::TypeError);
        assert_eq!(f.likely_files, vec!["src/lib.rs"]);
        assert_eq!(f.suggested_recovery, RecoveryStrategy::RepairCurrentNode);
    }

    #[test]
    fn test_failure_is_classified() {
        let f = classify(CheckKind::Test, "test tests::it ... FAILED");
        assert_eq!(f.kind, FailureKind::TestFailure);
        assert!(f.retryable);
    }

    /// R005 F-P1: the gate ran `cargo` under the HOST default rustc (1.96)
    /// while the task's tree required 1.97 — cargo's MSRV refusal is an
    /// environment mismatch, never a code defect to "repair".
    #[test]
    fn cargo_msrv_refusal_is_environment_mismatch() {
        let bulk = "error: rustc 1.96.0 is not supported by the following packages:\n  foo@0.1.0 requires rustc 1.97";
        let single = "error: rustc 1.96.0 is not supported by the following package:\n  envsmoke@0.1.0 requires rustc 1.99.0";
        let legacy = "error: package `foo v0.1.0` cannot be built because it requires rustc 1.97.0 or newer, while the currently active rustc version is 1.96.0";
        let rustup = "error: toolchain '1.97.0-aarch64-apple-darwin' is not installed";
        for output in [bulk, single, legacy, rustup] {
            assert!(is_environment_mismatch(output), "not detected: {output}");
            let f = classify(CheckKind::Build, output);
            assert_eq!(f.kind, FailureKind::EnvironmentFailure, "{output}");
            assert!(!f.retryable);
            assert_eq!(f.suggested_recovery, RecoveryStrategy::StopAndReport);
        }
    }

    /// R003 F1: hugo's whole-repo `go test ./...` failed with
    /// `no space left on device` and a missing global npm dependency — both
    /// limits of the machine, not defects in the agent's (correct) fix. The
    /// run was nonetheless recorded `failed`. A correct deliverable must not
    /// be blamed on the environment it happened to run in.
    #[test]
    fn a_toolchain_that_cannot_create_its_home_is_environment_not_code() {
        let out = "error: could not create home directory: \
                   'C:\\Users\\runneradmin\\.rustup'";
        let classified = classify(CheckKind::Build, out);
        assert_eq!(
            classified.kind,
            FailureKind::EnvironmentFailure,
            "a compiler that cannot start says nothing about the code: {}",
            classified.summary
        );
        assert_eq!(
            classified.suggested_recovery,
            RecoveryStrategy::StopAndReport
        );
    }

    #[test]
    fn resource_and_missing_global_tooling_are_environment_failures() {
        let cases = [
            // R003's literal trigger.
            "mkdir /var/folders/g0/T/sandboxes/abc: no space left on device",
            "error: No space left on device (os error 28)",
            // The other half of R003: a global tool the sandbox lacks.
            "npm ERR! code ENOENT\nnpm ERR! syscall spawn\nexecutable not found",
            "error: externally-managed-environment",
            // Resource exhaustion that is equally not a code defect.
            "fork failed: Resource temporarily unavailable",
            "error: too many open files (os error 24)",
            "Killed: 9",
            "fatal error: out of memory",
        ];
        for output in cases {
            let f = classify(CheckKind::Test, output);
            assert_eq!(
                f.kind,
                FailureKind::EnvironmentFailure,
                "should be environment, not a code defect: {output}"
            );
            assert!(!f.retryable, "{output}");
            assert_eq!(
                f.suggested_recovery,
                RecoveryStrategy::StopAndReport,
                "{output}"
            );
        }
    }

    #[test]
    fn ordinary_failures_are_not_environment_mismatch() {
        assert!(!is_environment_mismatch("test tests::it ... FAILED"));
        assert!(!is_environment_mismatch(
            "error[E0308]: mismatched types\n --> src/lib.rs:12:5"
        ));
        // "is not installed" alone (e.g. an npm hint) must not trip the gate
        // without toolchain context.
        assert!(!is_environment_mismatch(
            "warning: package foo is not installed"
        ));
    }

    #[test]
    fn missing_toolchain_is_environment() {
        let f = classify(CheckKind::Build, "error: could not find `Cargo.toml`");
        assert_eq!(f.kind, FailureKind::EnvironmentFailure);
        assert!(!f.retryable);
        assert_eq!(f.suggested_recovery, RecoveryStrategy::StopAndReport);
    }

    #[test]
    fn extracts_go_paths() {
        let files = extract_paths("./main.go:8:2: undefined: foo");
        assert_eq!(files, vec!["./main.go"]);
    }
}
