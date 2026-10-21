//! Mechanical acceptance for `update_goal(complete)`.
//!
//! The runtime does not judge whether the work satisfies the user's objective
//! — that reading belongs to the model and the user. What it *can* check is
//! the model's own verification claim against its own execution record:
//!
//! - a command the model cites as proof must actually have run,
//! - its last run must have succeeded, and
//! - it must have run after the most recent file change.
//!
//! A Goal that changed files must therefore close in one of two honest ways:
//! cite verification that mechanically matches the record, or declare
//! `not_applicable` with a reason (the completion is then accepted with a
//! warning, never as a silently verified success). A failed test/build in the
//! task is hard evidence and cannot be overridden by either declaration.
//!
//! Everything here is a fact about commands and exit codes. No LLM call, no
//! keyword reading of the user's goal, no opinion about whether the change is
//! good.

/// One command execution the runtime observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationRun {
    /// The command as executed (`go test ./...`, `sh verify.sh`).
    pub label: String,
    /// The tool did not report an error (for a command, exit 0).
    pub ok: bool,
    /// Model step that ran it; ordering only.
    pub model_step: u32,
    /// `EvidenceLedger::total_mutation_ops` when it ran, so "after the last
    /// file change" is a comparison, not a timestamp guess.
    pub mutation_ops: u64,
}

/// What the model said about verification in `update_goal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerificationClaim {
    /// Commands the model names as proof. Matched against [`VerificationRun`].
    Passed(Vec<String>),
    /// The objective has no executable check; the reason is shown to the user.
    NotApplicable(String),
    /// No verification field at all.
    Missing,
}

/// The gate's answer for one `update_goal(complete)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerificationDecision {
    /// Resolve the goal. `warning` is persisted on the terminal.
    Accept { warning: Option<String> },
    /// Refuse this resolution and tell the model exactly which fact failed.
    Refuse(String),
}

/// Decide whether `update_goal(complete)` may resolve the goal.
pub(crate) fn decide(
    claim: &VerificationClaim,
    runs: &[VerificationRun],
    mutation_ops: u64,
    modified: bool,
) -> VerificationDecision {
    // A verification-class command that failed and has not been followed by a
    // successful one is hard evidence against completion. It outranks every
    // declaration, including `not_applicable`.
    if let Some(failed) = last_unrecovered_failure(runs) {
        return VerificationDecision::Refuse(format!(
            "the last verification command `{}` failed (step {}). Fix the failure and run it \
             again, or call update_goal(status=\"blocked\") with the exact obstruction. A failed \
             verification cannot be closed as complete.",
            failed.label, failed.model_step
        ));
    }

    match claim {
        VerificationClaim::Passed(commands) if !commands.is_empty() => {
            for command in commands {
                let cited = command.trim();
                if cited.is_empty() {
                    continue;
                }
                let Some(last) = runs.iter().rev().find(|run| run.label.contains(cited)) else {
                    return VerificationDecision::Refuse(format!(
                        "`{cited}` was cited as verification but never ran in this task. Run \
                         the command that proves the objective, or cite one you actually ran."
                    ));
                };
                if !last.ok {
                    return VerificationDecision::Refuse(format!(
                        "`{cited}` was cited as verification but its last run failed (step {}).",
                        last.model_step
                    ));
                }
                if last.mutation_ops < mutation_ops {
                    return VerificationDecision::Refuse(format!(
                        "`{cited}` passed before the most recent file change. Re-run it against \
                         the current workspace before marking the goal complete."
                    ));
                }
            }
            VerificationDecision::Accept { warning: None }
        }
        VerificationClaim::NotApplicable(reason) => {
            let reason = reason.trim();
            let why = if reason.is_empty() {
                "no reason given"
            } else {
                reason
            };
            VerificationDecision::Accept {
                warning: Some(format!(
                    "goal completed without executable verification ({why})"
                )),
            }
        }
        // An empty `passed.commands` list is not evidence. Treat it like an
        // omission. The runtime does not force a command to exist (a Goal may
        // have no executable check), but it refuses to let a stale pass stand
        // in for a fresh one, and it records the absence on the terminal
        // instead of reporting a silently verified success.
        VerificationClaim::Passed(_) | VerificationClaim::Missing => {
            let fresh_pass = runs.iter().rev().any(|run| {
                is_verification_command(&run.label) && run.ok && run.mutation_ops >= mutation_ops
            });
            let stale_pass = runs
                .iter()
                .rev()
                .find(|run| is_verification_command(&run.label) && run.ok);
            if modified
                && !fresh_pass
                && let Some(stale) = stale_pass
            {
                return VerificationDecision::Refuse(format!(
                    "`{}` passed before the most recent file change (step {}). Re-run it against \
                     the current workspace, or declare why the change needs no executable check.",
                    stale.label, stale.model_step
                ));
            }
            if modified && !fresh_pass {
                VerificationDecision::Accept {
                    warning: Some(
                        "goal completed without executable verification (no verification \
                         command ran after the last file change)"
                            .to_string(),
                    ),
                }
            } else {
                // A read-only goal has nothing to verify; its answer is the
                // deliverable.
                VerificationDecision::Accept { warning: None }
            }
        }
    }
}

/// The most recent verification-class failure with no later verification
/// success. `runs` is chronological.
fn last_unrecovered_failure(runs: &[VerificationRun]) -> Option<&VerificationRun> {
    let mut failure: Option<&VerificationRun> = None;
    for run in runs
        .iter()
        .filter(|run| is_verification_command(&run.label))
    {
        if run.ok {
            failure = None;
        } else {
            failure = Some(run);
        }
    }
    failure
}

/// Parse the optional `verification` object from `update_goal` arguments.
///
/// A malformed declaration is an error the model can correct, never an
/// implicit `not_applicable`.
pub(crate) fn parse_claim(arguments: &serde_json::Value) -> Result<VerificationClaim, String> {
    let Some(verification) = arguments.get("verification") else {
        return Ok(VerificationClaim::Missing);
    };
    let Some(status) = verification.get("status").and_then(|s| s.as_str()) else {
        return Err(
            "`verification` requires `status` set to \"passed\" or \"not_applicable\"".to_string(),
        );
    };
    let detail = verification
        .get("detail")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    match status {
        "passed" => {
            let commands = detail
                .lines()
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            Ok(VerificationClaim::Passed(commands))
        }
        "not_applicable" => Ok(VerificationClaim::NotApplicable(detail.to_string())),
        other => Err(format!(
            "`verification.status` must be \"passed\" or \"not_applicable\" (got \"{other}\")"
        )),
    }
}

/// The command a tool call actually asked to run, in one display string.
///
/// Only the two command tools have one. Every other tool is not executable
/// verification, so a call returns `None`.
pub(crate) fn command_label(name: &str, arguments: &serde_json::Value) -> Option<String> {
    match name {
        "run_command" => {
            let program = arguments.get("program")?.as_str()?.trim();
            if program.is_empty() {
                return None;
            }
            let args: Vec<&str> = arguments
                .get("args")
                .and_then(|value| value.as_array())
                .map(|args| args.iter().filter_map(|arg| arg.as_str()).collect())
                .unwrap_or_default();
            Some(if args.is_empty() {
                program.to_string()
            } else {
                format!("{program} {}", args.join(" "))
            })
        }
        "shell_command" => {
            let command = arguments
                .get("cmd")
                .or_else(|| arguments.get("command"))?
                .as_str()?
                .trim();
            (!command.is_empty()).then(|| command.to_string())
        }
        _ => None,
    }
}

/// Whether a command is a test/build/check-class command.
///
/// Deliberately narrow and mechanical: it only decides which FAILURES are hard
/// evidence. A command that is not recognised here is still usable as cited
/// verification (`README` inspection, a custom CI script the model names).
pub(crate) fn is_verification_command(label: &str) -> bool {
    const VERB_TARGETS: &[&str] = &[
        "test",
        "tests",
        "build",
        "check",
        "clippy",
        "vet",
        "lint",
        "typecheck",
        "type-check",
        "verify",
        "acceptance",
        "e2e",
        "ci",
    ];
    // `;`, `&&`, `||` and pipes run several programs; a verification in any
    // segment counts, because a failure in that segment fails the command.
    for segment in label.split([';', '&', '|', '\n']) {
        let words: Vec<&str> = segment
            .split_whitespace()
            .map(|word| word.trim_matches(|c| c == '"' || c == '\''))
            .filter(|word| !word.is_empty())
            .collect();
        let Some(first) = words.first().copied() else {
            continue;
        };
        let second = words.get(1).copied().unwrap_or("");
        let basename = first.rsplit('/').next().unwrap_or(first);
        let matches = match basename {
            "go" => matches!(second, "test" | "build" | "vet"),
            "cargo" => matches!(second, "test" | "build" | "check" | "clippy"),
            "npm" | "pnpm" | "yarn" | "bun" | "deno" => {
                second == "test"
                    || (second == "run" && words.get(2).is_some_and(|w| VERB_TARGETS.contains(w)))
            }
            "make" | "just" | "bazel" => {
                words.iter().skip(1).any(|word| VERB_TARGETS.contains(word))
            }
            "mvn" | "gradle" | "dotnet" | "swift" | "flutter" | "dart" | "mix" | "ctest" => {
                words.iter().skip(1).any(|word| VERB_TARGETS.contains(word))
            }
            "pytest" | "phpunit" => true,
            "python" | "python3" | "py" => words
                .windows(2)
                .any(|pair| pair[0] == "-m" && matches!(pair[1], "unittest" | "pytest" | "nose2")),
            "node" => words.iter().skip(1).any(|word| *word == "--test"),
            // A shell entry point named for verification: `sh verify.sh`,
            // `bash ./scripts/check.sh`, `./accept.sh`.
            "sh" | "bash" | "zsh" | "pwsh" | "powershell" | "cmd" => {
                words.iter().skip(1).any(|word| {
                    let name = word.rsplit('/').next().unwrap_or(word);
                    VERB_TARGETS.iter().any(|target| name.contains(target))
                })
            }
            _ => {
                // `./verify.sh`, `./acceptance.sh`, `scripts/test.sh` run
                // directly.
                VERB_TARGETS.iter().any(|target| basename.contains(target))
                    && (basename.ends_with(".sh")
                        || basename.ends_with(".py")
                        || basename.ends_with(".ps1")
                        || basename.ends_with(".bat"))
            }
        };
        if matches {
            return true;
        }
    }
    false
}

/// Whether a command's output says the test runner executed no tests.
///
/// A suite that only skipped, or a runner that found no tests, cannot prove
/// that the objective's tests pass. This is a mechanical fact about the
/// output, not a judgment about the project; the markers are deliberately
/// narrow so a mixed suite that really passed is never mistaken for an empty
/// one.
pub(crate) fn reports_no_tests(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    const EMPTY_MARKERS: &[&str] = &[
        "ran 0 tests",
        "ran 0 test",
        "running 0 tests",
        "test result: ok. 0 passed",
        "test result: ok. 0 tests",
        "0 passed, 0 failed",
        "no tests ran",
        "no test files",
        "no tests to run",
        "no tests found",
        "no test matches",
        "0 tests completed",
    ];
    if EMPTY_MARKERS.iter().any(|marker| lower.contains(marker)) {
        return true;
    }
    // unittest: `Ran 3 tests ... OK (skipped=3)` means every discovered test
    // was skipped; `Ran 5 tests ... skipped=1` executed four real tests.
    if let Some(ran) = number_after(&lower, "ran ")
        && let Some(skipped) = number_after(&lower, "skipped=")
    {
        return ran > 0 && ran <= skipped;
    }
    // pytest: `3 skipped in 0.01s` with nothing passed, failed or errored.
    if lower.contains(" skipped in ")
        && !lower.contains(" passed")
        && !lower.contains(" failed")
        && !lower.contains(" error")
    {
        return true;
    }
    false
}

fn number_after(text: &str, needle: &str) -> Option<usize> {
    let rest = &text[text.find(needle)? + needle.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(label: &str, ok: bool, step: u32, mutation_ops: u64) -> VerificationRun {
        VerificationRun {
            label: label.to_string(),
            ok,
            model_step: step,
            mutation_ops,
        }
    }

    #[test]
    fn empty_and_all_skipped_test_runs_are_not_evidence() {
        for output in [
            "Ran 0 tests in 0.000s",
            "running 0 tests",
            "test result: ok. 0 passed; 0 failed; 3 ignored",
            "Ran 1 test in 0.000s\n\nOK (skipped=1)",
            "3 skipped in 0.01s",
            "go: no test files",
            "no tests ran in 0.01s",
        ] {
            assert!(
                reports_no_tests(output),
                "must be treated as empty: {output:?}"
            );
        }
        for output in [
            "Ran 5 tests in 0.12s\n\nOK (skipped=1)",
            "test result: ok. 5 passed; 0 failed; 1 ignored",
            "3 passed, 1 skipped in 0.2s",
            "5 tests completed",
        ] {
            assert!(
                !reports_no_tests(output),
                "a real mixed run must stay evidence: {output:?}"
            );
        }
    }

    #[test]
    fn a_cited_command_that_ran_and_passed_after_the_change_is_accepted() {
        let runs = vec![run("go test ./...", true, 4, 2)];
        assert_eq!(
            decide(
                &VerificationClaim::Passed(vec!["go test ./...".into()]),
                &runs,
                2,
                true
            ),
            VerificationDecision::Accept { warning: None }
        );
    }

    #[test]
    fn a_citation_that_never_ran_is_refused() {
        let runs = vec![run("go build ./...", true, 4, 2)];
        let decision = decide(
            &VerificationClaim::Passed(vec!["go test ./...".into()]),
            &runs,
            2,
            true,
        );
        match decision {
            VerificationDecision::Refuse(message) => {
                assert!(message.contains("never ran"), "{message}")
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_citation_whose_last_run_failed_is_refused() {
        let runs = vec![run("go test ./...", false, 4, 2)];
        assert!(matches!(
            decide(&VerificationClaim::Passed(vec!["go test".into()]), &runs, 2, true),
            VerificationDecision::Refuse(message) if message.contains("failed")
        ));
    }

    #[test]
    fn a_citation_that_predates_the_last_change_is_refused() {
        let runs = vec![run("go test ./...", true, 4, 1)];
        assert!(matches!(
            decide(&VerificationClaim::Passed(vec!["go test".into()]), &runs, 3, true),
            VerificationDecision::Refuse(message) if message.contains("before the most recent")
        ));
    }

    #[test]
    fn a_failed_verification_blocks_completion_even_without_a_claim() {
        let runs = vec![run("cargo test", false, 5, 2)];
        assert!(matches!(
            decide(&VerificationClaim::Missing, &runs, 2, true),
            VerificationDecision::Refuse(message) if message.contains("failed")
        ));
        assert!(matches!(
            decide(
                &VerificationClaim::NotApplicable("config only".into()),
                &runs,
                2,
                true
            ),
            VerificationDecision::Refuse(message) if message.contains("failed")
        ));
    }

    #[test]
    fn a_later_success_recovers_a_failed_verification() {
        let runs = vec![
            run("cargo test", false, 5, 2),
            run("cargo test", true, 7, 2),
        ];
        assert_eq!(
            decide(&VerificationClaim::Missing, &runs, 2, true),
            VerificationDecision::Accept { warning: None }
        );
    }

    #[test]
    fn changed_files_without_evidence_complete_with_a_warning_and_read_only_goals_do_not() {
        assert!(matches!(
            decide(&VerificationClaim::Missing, &[], 0, true),
            VerificationDecision::Accept { warning: Some(warning) }
                if warning.contains("without executable verification")
        ));
        assert_eq!(
            decide(&VerificationClaim::Missing, &[], 0, false),
            VerificationDecision::Accept { warning: None }
        );
    }

    #[test]
    fn a_pass_that_predates_the_last_change_cannot_stand_in_for_a_fresh_one() {
        let runs = vec![run("go test ./...", true, 4, 1)];
        assert!(matches!(
            decide(&VerificationClaim::Missing, &runs, 3, true),
            VerificationDecision::Refuse(message) if message.contains("before the most recent")
        ));
    }

    #[test]
    fn an_explicit_not_applicable_is_accepted_with_a_warning() {
        match decide(
            &VerificationClaim::NotApplicable("  documentation only  ".into()),
            &[],
            0,
            true,
        ) {
            VerificationDecision::Accept {
                warning: Some(warning),
            } => {
                assert!(warning.contains("documentation only"), "{warning}")
            }
            other => panic!("expected an accepted warning, got {other:?}"),
        }
    }

    #[test]
    fn claims_parse_only_their_two_explicit_shapes() {
        assert_eq!(
            parse_claim(&serde_json::json!({})),
            Ok(VerificationClaim::Missing)
        );
        assert_eq!(
            parse_claim(&serde_json::json!({
                "verification": {"status": "passed", "detail": "go test ./...\ngo vet ./..."}
            })),
            Ok(VerificationClaim::Passed(vec![
                "go test ./...".into(),
                "go vet ./...".into()
            ]))
        );
        assert_eq!(
            parse_claim(&serde_json::json!({
                "verification": {"status": "not_applicable", "detail": "docs"}
            })),
            Ok(VerificationClaim::NotApplicable("docs".into()))
        );
        assert!(
            parse_claim(&serde_json::json!({
                "verification": {"status": "maybe"}
            }))
            .is_err()
        );
        assert!(parse_claim(&serde_json::json!({"verification": {}})).is_err());
    }

    #[test]
    fn command_labels_are_the_commands_the_tools_ran() {
        assert_eq!(
            command_label(
                "run_command",
                &serde_json::json!({"program": "go", "args": ["test", "./..."]})
            ),
            Some("go test ./...".to_string())
        );
        assert_eq!(
            command_label("shell_command", &serde_json::json!({"cmd": "make ci"})),
            Some("make ci".to_string())
        );
        assert_eq!(command_label("read_file", &serde_json::json!({})), None);
        assert_eq!(
            command_label("run_command", &serde_json::json!({"program": "  "})),
            None
        );
    }

    #[test]
    fn verification_classification_is_narrow_enough_not_to_read_prose_as_a_test() {
        for command in [
            "go test ./...",
            "go build ./...",
            "cargo clippy --all-targets",
            "npm run build",
            "pnpm test",
            "make ci",
            "sh verify.sh",
            "bash ./scripts/check.sh",
            "./acceptance.sh",
            "pytest -q",
            "python3 -m unittest",
            "python -m pytest -q",
        ] {
            assert!(is_verification_command(command), "{command}");
        }
        for command in [
            "grep -rn validate src",
            "cat testdata/verify.md",
            "go version",
            "git status",
            "ls scripts",
            "python3 script.py",
        ] {
            assert!(!is_verification_command(command), "{command}");
        }
    }
}
