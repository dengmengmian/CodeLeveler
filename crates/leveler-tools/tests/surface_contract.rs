//! What a turn puts in front of the model, and what it does not.
//!
//! The surface is composed, not inherited: every tool is in a named category
//! with a stated condition. These tests hold that composition, and hold the
//! line that no condition is ever a judgement about the model or the task.

use leveler_tools::{CapabilityPacks, core_surface, default_registry, model_surface};

/// In-process capability handles: the composition under test is about which
/// tools exist, not about which host services back them.
fn caps() -> leveler_tools::Capabilities {
    leveler_tools::Capabilities::in_process(std::sync::Arc::new(
        leveler_core::environment().clone(),
    ))
}

fn names(registry: &leveler_tools::ToolRegistry) -> Vec<String> {
    registry.definitions().into_iter().map(|d| d.name).collect()
}

/// The seven primitives are always there. A host that can offer nothing else
/// still offers these, because they are what a coding agent is.
#[test]
fn every_core_primitive_is_present_in_every_composition() {
    for packs in [CapabilityPacks::NONE, CapabilityPacks::ALL] {
        let got = names(&model_surface(packs, &caps()));
        for primitive in [
            "read_file",     // read
            "list_files",    // ls
            "find_files",    // find
            "grep",          // grep
            "apply_patch",   // edit
            "write_file",    // write
            "run_command",   // bash
            "shell_command", // bash
        ] {
            assert!(
                got.iter().any(|n| n == primitive),
                "{primitive} missing from {packs:?}"
            );
        }
    }
}

/// `run_command` can start a background task, so the tools that observe and
/// stop one travel with it. A task the caller cannot see or kill is an orphan.
#[test]
fn the_background_lifecycle_travels_with_the_command_primitive() {
    let got = names(&core_surface(&caps()));
    assert!(got.iter().any(|n| n == "run_command"));
    for lifecycle in ["get_task", "wait_task", "kill_task"] {
        assert!(
            got.iter().any(|n| n == lifecycle),
            "{lifecycle} must accompany run_command"
        );
    }
}

/// A pack is a real capability with a real precondition, and turning one off
/// removes exactly its own tools and nothing else.
#[test]
fn a_pack_is_independent_of_every_other_pack() {
    let all = names(&default_registry());
    let without_browser = names(&model_surface(
        CapabilityPacks {
            browser: false,
            ..CapabilityPacks::ALL
        },
        &caps(),
    ));
    let dropped: Vec<&String> = all
        .iter()
        .filter(|n| !without_browser.contains(n))
        .collect();
    assert_eq!(
        dropped.len(),
        12,
        "only the browser pack moved: {dropped:?}"
    );
    assert!(
        dropped.iter().all(|n| n.starts_with("browser_")),
        "{dropped:?}"
    );
}

/// The negative half of the surface, and why each name is on it. None of these
/// left because a model looked weak.
#[test]
fn the_removed_and_demoted_tools_stay_off_the_surface() {
    let got = names(&default_registry());
    for absent in [
        "expand_tools",
        "replace",
        "create_checkpoint",
        "restore_checkpoint",
        "consolidate_memory",
        "create_skill",
    ] {
        assert!(
            !got.iter().any(|n| n == absent),
            "{absent} must not be model-visible"
        );
    }
}

/// A removed tool name must fail closed, not silently: the name is not
/// executable and is not replay-safe, so crash recovery stops for a human
/// instead of skipping a call it cannot re-run.
#[test]
fn a_removed_tool_name_fails_closed() {
    let registry = default_registry();
    for gone in ["replace", "expand_tools"] {
        assert!(registry.get(gone).is_none(), "{gone} must not dispatch");
        assert!(
            !registry.replay_is_side_effect_free(gone),
            "an unknown name must never be auto-replayed: {gone}"
        );
    }
}

/// The read-only subset a read-capable child receives is an allowlist, and it
/// still contains the read primitives after the surface was recomposed.
#[test]
fn a_read_only_child_still_gets_the_read_primitives() {
    let subset = default_registry().read_only_subset();
    for readable in ["read_file", "list_files", "find_files", "grep"] {
        assert!(subset.get(readable).is_some(), "{readable} must survive");
    }
    for writable in ["apply_patch", "write_file", "run_command", "shell_command"] {
        assert!(subset.get(writable).is_none(), "{writable} must not");
    }
}
