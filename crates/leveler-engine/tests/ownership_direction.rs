//! The engine owns lifecycle, not agent intelligence.
//!
//! This is a structural tripwire, not a unit test: it reads this crate's own
//! manifest and sources. The decoupling it guards is a dependency DIRECTION,
//! and a direction is exactly the kind of thing that comes back one
//! convenient import at a time.

use std::path::Path;

fn engine_src() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("engine src is readable") {
            let path = entry.expect("a readable entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).expect("a readable source file");
                out.push((path.display().to_string(), text));
            }
        }
    }
    assert!(!out.is_empty(), "engine has sources");
    out
}

/// A harness depends on the engine. Never the reverse: the moment this edge
/// exists, the engine is the coding agent's composition root again and
/// `Engine != Coding Agent` stops being mechanically true.
#[test]
fn the_engine_does_not_depend_on_a_harness() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("the engine manifest is readable");
    for forbidden in ["leveler-agent", "leveler-tools", "leveler-verifier"] {
        assert!(
            !manifest.contains(forbidden),
            "leveler-engine must not depend on `{forbidden}`: the engine owns \
             session/turn/task lifecycle, and a harness owns what runs inside \
             a turn. Compose it from above instead of importing it from here."
        );
    }
}

/// The manifest can be clean while the sources are not — a path dependency
/// added under another name, say. Check the imports themselves too.
#[test]
fn no_engine_source_reaches_for_a_harness_crate() {
    for (path, text) in engine_src() {
        for forbidden in ["leveler_agent", "leveler_tools", "leveler_verifier"] {
            assert!(
                !text.contains(forbidden),
                "{path} references `{forbidden}`; the engine must not know a \
                 harness, its tool surface, or its verification"
            );
        }
    }
}

/// F9.1: fresh-turn inheritance is a HARNESS decision. The engine applies one
/// mechanical rule over the bool the harness computes; if it reads a Coding
/// domain predicate itself, one harness's semantics are baked into the engine
/// again and a second harness inherits them by accident.
#[test]
fn the_engine_does_not_interpret_plan_or_progress_semantics() {
    for (path, text) in engine_src() {
        for forbidden in ["is_fully_completed", "is_terminal_for_inheritance"] {
            assert!(
                !text.contains(forbidden),
                "{path} calls `{forbidden}`; whether a prior epoch is still \
                 open is the harness's judgement. Pass the answer in \
                 `SeedRequest::Fresh::prior_epoch_open` instead of deciding it \
                 here."
            );
        }
    }
}

/// F9.3: what a lost child CONTRIBUTED is harness judgement. The engine finds
/// the ghost, orders its terminal, attributes it to the originating turn and
/// stamps `ok: false`; if it also projects the child's role over an evidence
/// ledger, it is reading one harness's findings vocabulary again — and a
/// second harness gets a contribution computed under semantics it never had.
///
/// `ChildResultProjection` itself stays in the event payload: carrying a
/// harness's answer is not the same as computing one.
#[test]
fn the_engine_does_not_compute_a_child_contribution() {
    for (path, text) in engine_src() {
        assert!(
            !text.contains("from_findings"),
            "{path} projects a child's contribution out of ledger findings; \
             that reading needs the harness's role vocabulary. Ask through \
             `LostChildVoice` instead of computing it here."
        );
    }
}
