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

fn engine_file(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(name))
        .unwrap_or_else(|error| panic!("engine source `{name}` is readable: {error}"))
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

/// F9.1: fresh-turn inheritance and typed workflow state are harness concerns.
#[test]
fn the_engine_does_not_interpret_plan_or_progress_semantics() {
    for (path, text) in engine_src() {
        if path.ends_with("event.rs") {
            continue;
        }
        for forbidden in ["is_fully_completed", "is_terminal_for_inheritance"] {
            assert!(
                !text.contains(forbidden),
                "{path} calls `{forbidden}`; prior workflow inheritance is the harness's judgement"
            );
        }
        for forbidden in ["PlanState", "EvidenceLedger", "ProgressLedger"] {
            assert!(
                !text.contains(forbidden),
                "{path} interprets Coding state `{forbidden}` outside the durable event schema"
            );
        }
    }
}

#[test]
fn checkpoint_projection_and_rendering_live_above_the_engine() {
    for (path, text) in engine_src() {
        if path.ends_with("event.rs") {
            continue;
        }
        for forbidden in [
            "WorkspaceFacts",
            "context_block()",
            "create_goal_checkpoint",
        ] {
            assert!(
                !text.contains(forbidden),
                "{path} owns Coding checkpoint behavior through `{forbidden}`"
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

/// The Engine persists a workflow state chosen by the Harness; it must not
/// select Coding's Execute phase itself.
#[test]
fn the_engine_does_not_select_the_coding_execute_state() {
    for name in ["engine.rs", "turn.rs", "reaper.rs"] {
        let source = engine_file(name);
        assert!(
            !source.contains("AgentState::Execute"),
            "{name} must receive the Harness-selected workflow state instead of choosing Coding's Execute phase"
        );
    }
}

/// Harnesses receive append-only emitters for canonical turn events. Exposing
/// the writable EventStore here would let them bypass persistence ordering,
/// ownership fencing, and observer delivery while calling it "read-only".
#[test]
fn turn_ports_do_not_expose_the_writable_event_store() {
    let source = engine_file("turn.rs");
    assert!(
        !source.contains("pub events: Arc<dyn EventStore>"),
        "TurnPorts must expose EventEmitter, not the writable EventStore"
    );
}

/// Generic recovery facts cannot assume that the side effect belongs to a
/// repository or workspace. Coding-specific instructions are rendered above
/// the Engine boundary.
#[test]
fn engine_recovery_text_is_domain_neutral() {
    let error = leveler_engine::EngineError::RecoveryConfirmationRequired {
        call_id: "call-1".to_string(),
        tool: "side_effect".to_string(),
    }
    .to_string();
    for forbidden in ["workspace", "repository"] {
        assert!(
            !error.contains(forbidden),
            "generic recovery error mentions `{forbidden}`: {error}"
        );
    }

    let engine = engine_file("engine.rs");
    assert!(
        !engine.contains("workspace was verified manually"),
        "the canonical recovery marker must record acknowledgement and non-replay, not a \
         Coding-specific verification claim"
    );
}

/// `tempfile` supports this crate's tests only; keeping it in normal
/// dependencies expands the production Engine graph for no runtime behavior.
#[test]
fn tempfile_is_a_dev_dependency_only() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("the engine manifest is readable");
    let dependencies = manifest
        .split_once("[dependencies]")
        .expect("manifest has dependencies")
        .1
        .split_once("[dev-dependencies]")
        .expect("manifest has dev-dependencies")
        .0;
    assert!(
        !dependencies
            .lines()
            .any(|line| line.trim_start().starts_with("tempfile")),
        "tempfile must not be a normal leveler-engine dependency"
    );
    let dev_dependencies = manifest
        .split_once("[dev-dependencies]")
        .expect("manifest has dev-dependencies")
        .1;
    assert!(
        dev_dependencies
            .lines()
            .any(|line| line.trim_start().starts_with("tempfile")),
        "leveler-engine integration tests require tempfile as a dev-dependency"
    );
}
