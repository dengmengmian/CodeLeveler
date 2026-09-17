//! The contract chooses its checks; a test never chooses to skip itself.
//!
//! `leveler_test_support::already_confined` exists so a test that has to
//! *observe* confinement can stand down instead of reporting the platform's
//! nesting limit (macOS seatbelt and Linux bubblewrap cannot nest) as a defect
//! in the product. That is legitimate exactly where the standing-down test is
//! not part of the agent verification contract — otherwise a selected test
//! would pass without running, which is the false green the two-layer contract
//! exists to prevent.
//!
//! This test ties the two together mechanically: every stand-down call site
//! must live in a crate the declared contract excludes. It runs inside the
//! agent contract itself, so the guard is active in the same place it protects.
//!
//! The declaration lives in `.leveler/config.yaml`.

use std::path::{Path, PathBuf};

use leveler_project::ProjectConfig;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn config() -> ProjectConfig {
    let root = repo_root();
    ProjectConfig::load(&root).expect("this repository declares .leveler/config.yaml")
}

/// The crates the declared test command excludes, read through the product's
/// own parser rather than re-interpreting the YAML here.
fn exclusions(config: &ProjectConfig) -> Vec<String> {
    let test = config
        .verify
        .test
        .as_ref()
        .expect("a declared test command");
    let mut out = Vec::new();
    let mut args = test.args.iter();
    while let Some(arg) = args.next() {
        if arg == "--exclude"
            && let Some(name) = args.next()
        {
            out.push(name.clone());
        }
    }
    out
}

/// Every `.rs` file under `dir`, skipping build output and dependencies.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(name.as_ref(), "target" | "node_modules" | ".git") {
                continue;
            }
            out.extend(rust_sources(&path));
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
    out
}

fn crate_of(root: &Path, file: &Path) -> Option<String> {
    file.strip_prefix(root.join("crates"))
        .ok()
        .and_then(|rest| rest.components().next())
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
}

/// Whether a file actually *calls* the stand-down helper, as opposed to talking
/// about it. This module, the helper's own docs, the closure document and even
/// the match arm below all name it in prose or in a string literal, and a guard
/// that counted prose would fail on the first sentence written about it.
fn calls_stand_down(source: &str) -> bool {
    const CALL: &str = "already_confined()";
    source.lines().any(|line| {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            return false;
        }
        match trimmed.find(CALL) {
            // A mention inside a string literal is not a call: in source, a
            // call is preceded by code (a space, `!`, `(`, `&`), never by the
            // opening quote of a literal.
            Some(at) => !trimmed[..at].ends_with('"'),
            None => false,
        }
    })
}

/// The repository declares a contract, and it covers the whole workspace minus
/// the crate-level exclusions — not a targeted subset that would let a change
/// elsewhere go unverified.
#[test]
fn the_repository_declares_a_workspace_wide_contract() {
    let config = config();
    assert!(
        !config.verify.is_empty(),
        "this repository must declare its agent verification contract"
    );
    let test = config
        .verify
        .test
        .as_ref()
        .expect("a declared test command");
    assert_eq!(test.program, "cargo");
    for required in ["--workspace", "--all-features", "--locked"] {
        assert!(
            test.args.iter().any(|arg| arg == required),
            "the contract must run the {required} workspace gates, got {:?}",
            test.args
        );
    }
    let format = config.verify.format.as_ref().expect("a declared format");
    assert_eq!(format.args, vec!["fmt", "--all", "--", "--check"]);
    let build = config.verify.build.as_ref().expect("a declared build");
    assert_eq!(
        build.args,
        vec!["check", "--workspace", "--all-features", "--locked"]
    );
}

/// Test C — nested-sandbox and daemon tests are absent from the contract, not
/// skipped at runtime.
#[test]
fn every_stand_down_lives_in_a_crate_the_contract_excludes() {
    let root = repo_root();
    let exclusions = exclusions(&config());
    assert!(
        !exclusions.is_empty(),
        "the contract must state its exclusions explicitly"
    );

    let mut offenders = Vec::new();
    let mut call_sites = 0usize;
    for file in rust_sources(&root.join("crates")) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        if !calls_stand_down(&text) {
            continue;
        }
        let Some(crate_name) = crate_of(&root, &file) else {
            continue;
        };
        // The helper's own crate defines it; nobody calls it to stand down.
        if crate_name == "leveler-test-support" {
            continue;
        }
        call_sites += 1;
        if !exclusions.contains(&crate_name) {
            offenders.push(format!("{crate_name}: {}", file.display()));
        }
    }

    assert!(
        offenders.is_empty(),
        "a crate selected by the agent contract must never stand down — that is a \
         silent pass. Either move the test to an excluded crate or leave it out of \
         the contract: {offenders:#?}"
    );
    // Exclusions name real crates, so a typo cannot quietly widen the contract.
    for name in &exclusions {
        assert!(
            root.join("crates").join(name).join("Cargo.toml").is_file(),
            "the contract excludes `{name}`, which is not a crate in this workspace"
        );
    }
    // If this ever drops to zero, the scan stopped finding the call sites and
    // the guard above has become vacuous.
    assert!(
        call_sites > 0,
        "the stand-down scan found no call sites; it is no longer proving anything"
    );
}
