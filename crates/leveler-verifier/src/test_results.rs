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
}
