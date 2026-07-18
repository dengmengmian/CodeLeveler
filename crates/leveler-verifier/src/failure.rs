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

/// Classify a failed check from its command kind and captured output.
pub fn classify(kind: CheckKind, output: &str) -> ClassifiedFailure {
    let likely_files = extract_paths(output);
    let lower = output.to_lowercase();

    // Environment problems are not the code's fault — don't try to "repair".
    if lower.contains("command not found")
        || lower.contains("could not find `cargo.toml`")
        || lower.contains("no such file or directory")
        || lower.contains("permission denied")
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
