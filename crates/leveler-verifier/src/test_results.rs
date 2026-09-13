//! Extract the set of FAILED test identifiers from a check's captured output.
//!
//! Baseline delta attribution needs test-level granularity, not the check's
//! exit code: a whole-suite command that is red on both the working tree and
//! the baseline may be red for *different* tests, and only the tests that fail
//! now but did NOT fail on the baseline are this change's fault. These parsers
//! turn a check's stdout/stderr into that set so the two can be diffed.
//!
//! Both parsers read the checks' NORMAL human-readable output — no extra flags,
//! so the evidence the user and the repair loop see stays readable (a `-json`
//! run would replace it with machine noise).
//!
//! - Rust: stable `cargo test` has no machine-readable output (JSON is
//!   nightly-only); we parse the trailing `failures:` name list — a format
//!   stable for years. One block per test binary.
//! - Go: we parse the `--- FAIL: <name>` lines `go test` prints per failed
//!   test (including indented subtests).

use std::collections::BTreeSet;

/// Failed Rust test paths (e.g. `module::sub::test_name`) from `cargo test`
/// human-readable output. Handles multiple test binaries (each contributes its
/// own `failures:` block). Returns an empty set when nothing failed or the
/// output is unrecognizable.
pub fn parse_rust_failures(output: &str) -> BTreeSet<String> {
    let mut failures = BTreeSet::new();
    let mut in_block = false;
    for line in output.lines() {
        if line.trim_end() == "failures:" {
            // Start (or restart) a name-list block. The other `failures:` cargo
            // prints — the one before the per-test stdout dumps — is followed by
            // a blank line then `---- name stdout ----`, neither of which match
            // the strict `    <token>` shape below, so it collects nothing.
            in_block = true;
            continue;
        }
        if in_block {
            if let Some(name) = name_list_entry(line) {
                failures.insert(name.to_string());
            } else {
                in_block = false;
            }
        }
    }
    failures
}

/// A name-list line is exactly four leading spaces, a single whitespace-free
/// token, and nothing else (`    module::test`). Anything else — blank lines,
/// `test result:`, `---- … stdout ----`, deeper indentation — ends the block.
fn name_list_entry(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("    ")?;
    if rest.starts_with(' ') || rest.is_empty() {
        return None;
    }
    if rest.split_whitespace().count() != 1 {
        return None;
    }
    Some(rest)
}

/// Failed Go test names from `go test` output — the `--- FAIL: <name> (…s)`
/// lines it prints per failed test. Subtests (`--- FAIL: Test/case`) are
/// indented; leading whitespace is trimmed. Returns test names without a
/// package qualifier: the diff compares the SAME command's base vs working
/// output, so identical keys line up; the residual risk is two packages sharing
/// a test name, which the base-vs-working diff can conflate (rare).
pub fn parse_go_failures(output: &str) -> BTreeSet<String> {
    let mut failures = BTreeSet::new();
    for line in output.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("--- FAIL: ") {
            // `<name> (0.00s)` — the name is everything up to the timing paren.
            let name = rest.split_once(" (").map(|(n, _)| n).unwrap_or(rest).trim();
            if !name.is_empty() {
                failures.insert(name.to_string());
            }
        }
    }
    failures
}

/// Failed `node --test` test names from node's own test runner output.
///
/// Node reaches verification two ways — a `node --test` command, or the
/// package manager's `test` script running it — and the reporter writes the
/// same output into the captured evidence either way. The *markers* are
/// therefore what identify a failure, not the program name:
///
/// - the default (spec) reporter prints `✖ <name> (<duration>)`, indented for
///   subtests, and repeats every failure under a trailing `failing tests:`
///   heading. A suite that failed only because a subtest failed also gets its
///   own line; it is kept, because it is reported identically on both trees and
///   so cancels out in the baseline diff.
/// - `--test-reporter=tap` prints `not ok <n> - <name>`, likewise indented.
///
/// The trailing heading carries no duration, which is precisely what separates
/// it from a real failure line. Output from a different runner (jest, vitest)
/// matches neither form and yields the empty set: callers then fall back to
/// exit-code-level attribution, which never suppresses a gate on no evidence.
/// Evidence is captured through pipes, where node does not colorize; a name
/// that somehow carried escapes would simply not match its baseline twin, and
/// the check would keep gating.
///
/// Names are unqualified (`inner broken`), for the same reason the Go parser's
/// are: base and working run the SAME command, so identical keys line up.
pub fn parse_node_failures(output: &str) -> BTreeSet<String> {
    let mut failures = BTreeSet::new();
    for line in output.lines() {
        let line = line.trim_start();
        let name = if let Some(rest) = line.strip_prefix('✖') {
            spec_failure_name(rest)
        } else {
            tap_failure_name(line)
        };
        if let Some(name) = name {
            failures.insert(name);
        }
    }
    failures
}

/// ` <name> (<duration>)` → `<name>`. `None` for the reporter's
/// `failing tests:` heading, which has no duration suffix — that suffix is the
/// structural difference between a heading and a failure.
fn spec_failure_name(rest: &str) -> Option<String> {
    let head = rest.trim_end().strip_suffix(')')?;
    let open = head.rfind('(')?;
    if !is_duration(head[open + 1..].trim()) {
        return None;
    }
    let name = head[..open].trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Node's own duration rendering: `0.48ms`, `1.5s`, `2m`.
fn is_duration(text: &str) -> bool {
    let digits = text
        .strip_suffix("ms")
        .or_else(|| text.strip_suffix('s'))
        .or_else(|| text.strip_suffix('m'));
    match digits {
        Some(digits) => {
            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        }
        None => false,
    }
}

/// `not ok <n> - <name>` (TAP).
fn tap_failure_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("not ok ")?;
    let (_, name) = rest.split_once(" - ")?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rust_collects_the_trailing_name_list() {
        let output = "\
running 3 tests
test tests::ok ... ok
test tests::bad ... FAILED
test tests::also_bad ... FAILED

failures:

---- tests::bad stdout ----
thread 'tests::bad' panicked at src/lib.rs:10:5

---- tests::also_bad stdout ----
thread 'tests::also_bad' panicked at src/lib.rs:20:5


failures:
    tests::also_bad
    tests::bad

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out
";
        assert_eq!(
            parse_rust_failures(output),
            set(&["tests::also_bad", "tests::bad"])
        );
    }

    #[test]
    fn rust_handles_multiple_test_binaries() {
        let output = "\
failures:
    a::one

test result: FAILED. 0 passed; 1 failed

failures:
    b::two
    b::three

test result: FAILED. 0 passed; 2 failed
";
        assert_eq!(
            parse_rust_failures(output),
            set(&["a::one", "b::two", "b::three"])
        );
    }

    #[test]
    fn rust_all_passing_yields_empty() {
        let output = "\
running 2 tests
test tests::ok ... ok
test tests::fine ... ok

test result: ok. 2 passed; 0 failed; 0 ignored
";
        assert!(parse_rust_failures(output).is_empty());
    }

    #[test]
    fn rust_stdout_dump_failures_header_is_not_mistaken_for_names() {
        // The first `failures:` (before the `---- stdout ----` dumps) must
        // collect nothing; only the trailing name list counts.
        let output = "\
failures:

---- tests::bad stdout ----
some panic output that is indented differently
    this line has four spaces but also more words

failures:
    tests::bad

test result: FAILED. 0 passed; 1 failed
";
        assert_eq!(parse_rust_failures(output), set(&["tests::bad"]));
    }

    #[test]
    fn go_collects_fail_lines_including_subtests() {
        let output = "\
=== RUN   TestA
--- FAIL: TestA (0.01s)
    a_test.go:10: boom
=== RUN   TestB
--- PASS: TestB (0.00s)
=== RUN   TestC
    --- FAIL: TestC/case_two (0.00s)
FAIL
FAIL\tx/errors\t0.123s
";
        assert_eq!(parse_go_failures(output), set(&["TestA", "TestC/case_two"]));
    }

    #[test]
    fn go_all_passing_yields_empty() {
        let output = "=== RUN   TestB\n--- PASS: TestB (0.00s)\nok\tx/errors\t0.1s\n";
        assert!(parse_go_failures(output).is_empty());
    }

    /// Verbatim output of `node --test` (v25, default spec reporter) over one
    /// file with a plain failure, a failing subtest, and the suite that failed
    /// because of it. The trailing `failing tests:` heading is a heading, not a
    /// test named "failing tests".
    const NODE_SPEC: &str = "\
✔ addition works (0.282208ms)
✖ subtraction is broken (0.397583ms)
▶ nested group
  ✔ inner ok (0.06475ms)
  ✖ inner broken (0.1305ms)
✖ nested group (0.331042ms)
ℹ tests 5
ℹ suites 0
ℹ pass 2
ℹ fail 3
ℹ cancelled 0
ℹ skipped 0
ℹ todo 0
ℹ duration_ms 37.598084

✖ failing tests:

test at test/a.test.js:4:1
✖ subtraction is broken (0.397583ms)

test at test/a.test.js:7:11
✖ inner broken (0.1305ms)
";

    #[test]
    fn node_collects_failures_including_subtests_and_the_failing_suite() {
        assert_eq!(
            parse_node_failures(NODE_SPEC),
            set(&["subtraction is broken", "inner broken", "nested group"])
        );
    }

    #[test]
    fn node_summary_counters_are_not_failure_names() {
        // `ℹ fail 3` says how many failed; it is not itself a failed test.
        let names = parse_node_failures(NODE_SPEC);
        assert!(!names.iter().any(|n| n.contains("fail 3")), "{names:?}");
        assert!(
            !names.contains("failing tests:"),
            "the reporter's heading must not be read as a test name: {names:?}"
        );
    }

    #[test]
    fn node_all_passing_yields_empty() {
        let output = "✔ addition works (0.28ms)\nℹ tests 1\nℹ pass 1\nℹ fail 0\n";
        assert!(parse_node_failures(output).is_empty());
    }

    #[test]
    fn node_tap_reporter_failures_are_collected() {
        let output = "\
TAP version 13
# Subtest: test/a.test.js
    not ok 1 - subtraction is broken
    not ok 2 - inner broken
# fail 2
1..2
";
        assert_eq!(
            parse_node_failures(output),
            set(&["subtraction is broken", "inner broken"])
        );
    }

    /// A different runner's output must not be guessed at: an unrecognized
    /// format yields nothing, and callers fall back to exit-code attribution,
    /// which keeps the gate shut rather than suppressing it on no evidence.
    #[test]
    fn node_unknown_runner_output_yields_empty() {
        let output = "FAIL src/sum.test.ts\n  ● adds numbers\n\nTests:       1 failed, 2 passed\n";
        assert!(parse_node_failures(output).is_empty());
    }
}
