//! Phase 2 architecture tripwire (core-runtime-convergence-plan): tool
//! execution has exactly ONE host boundary.
//!
//! Every tool call must pass through the ToolHost pipeline in
//! `src/executor/host.rs` — admission (pre-hooks → permission rules → profile
//! policy → approval), the side-effect barrier, then execution. This test
//! fails the build the moment any other file in this crate reaches the
//! execution entry (`registry.execute`) or the gate entry (`hook_runner
//! .run_pre`) directly, which would create a second, bypassable path.
//!
//! Type-level enforcement backs this up: `Executor::dispatch` takes an
//! `AdmittedCall`, whose only constructor is the admission pipeline itself.

use std::path::{Path, PathBuf};

const HOST_FILE: &str = "executor/host.rs";

/// Patterns that mark the host-only entries. Textual on purpose: cheap,
/// obvious, and a rename that defeats the pattern will be caught in review
/// because this file names the contract.
const HOST_ONLY_PATTERNS: &[&str] = &["registry.execute(", "run_pre("];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("crate src must be readable") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Strip whitespace so formatting (line breaks between `.` and the call)
/// cannot hide a match.
fn condensed(source: &str) -> String {
    source.chars().filter(|c| !c.is_whitespace()).collect()
}

#[test]
fn tool_execution_is_confined_to_the_host_boundary() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    assert!(
        files.iter().any(|p| p.ends_with(HOST_FILE)),
        "the host boundary file {HOST_FILE} must exist"
    );

    let mut violations = Vec::new();
    for path in files {
        if path.ends_with(HOST_FILE) {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("source must be readable");
        let flat = condensed(&source);
        for pattern in HOST_ONLY_PATTERNS {
            if flat.contains(&condensed(pattern)) {
                violations.push(format!("{}: contains `{pattern}`", path.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "tool execution/gate entries outside the ToolHost boundary:\n{}",
        violations.join("\n")
    );
}
