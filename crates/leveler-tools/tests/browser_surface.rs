//! What the three browser tools put in front of the model.
//!
//! The collapse from twelve tools to three is only a win if the three are
//! still unambiguous, so the parts a model reads are pinned here: the actions
//! each tool offers, and the fact that each tool has one job rather than
//! being an `action=`-shaped door onto the other two.

use leveler_tools::{CapabilityPacks, ToolRegistry, model_surface};

fn surface() -> ToolRegistry {
    let capabilities = leveler_tools::Capabilities::in_process(std::sync::Arc::new(
        leveler_core::environment().clone(),
    ))
    .with_browser(std::sync::Arc::new(leveler_browser::Browser::new(
        leveler_core::environment().clone(),
        std::env::temp_dir().join("leveler-browser-surface-profile"),
        None,
    )));
    model_surface(CapabilityPacks::ALL, &capabilities)
}

fn schema(name: &str) -> serde_json::Value {
    surface()
        .definitions()
        .into_iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("{name} must be registered"))
        .input_schema
}

/// Read the allowed values of an enum-typed property. `schemars` emits the
/// enum into `$defs` and points at it, so follow the reference.
fn actions(schema: &serde_json::Value, property: &str) -> Vec<String> {
    let prop = &schema["properties"][property];
    let reference = prop
        .get("$ref")
        .or_else(|| {
            prop.get("allOf")
                .and_then(|a| a.get(0))
                .and_then(|f| f.get("$ref"))
        })
        .and_then(|r| r.as_str())
        .and_then(|r| r.rsplit('/').next());
    let values = prop
        .get("enum")
        .or_else(|| reference.and_then(|name| schema["$defs"][name].get("enum")))
        .and_then(|e| e.as_array())
        .unwrap_or_else(|| panic!("{property} must be an enum: {schema}"));
    values
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

#[test]
fn the_browser_surface_is_exactly_three_tools() {
    let names: Vec<String> = surface()
        .definitions()
        .into_iter()
        .map(|d| d.name)
        .filter(|n| n.starts_with("browser"))
        .collect();
    assert_eq!(names.len(), 3, "{names:?}");
    for expected in ["browser_tab", "browser_act", "browser_inspect"] {
        assert!(names.iter().any(|n| n == expected), "missing {expected}");
    }
}

/// Every job the twelve tools did still has a home, and the homes do not
/// overlap: nothing that moves between pages is in `browser_act`, nothing that
/// behaves like a user is in `browser_tab`.
#[test]
fn each_tool_owns_a_distinct_job() {
    let tab = actions(&schema("browser_tab"), "action");
    let act = actions(&schema("browser_act"), "action");
    let inspect = actions(&schema("browser_inspect"), "what");

    for expected in [
        "navigate",
        "snapshot",
        "screenshot",
        "reload",
        "list_tabs",
        "new_tab",
        "select_tab",
        "close_tab",
    ] {
        assert!(
            tab.contains(&expected.to_string()),
            "browser_tab lost {expected}: {tab:?}"
        );
    }
    for expected in ["click", "fill", "type", "press", "select", "scroll"] {
        assert!(
            act.contains(&expected.to_string()),
            "browser_act lost {expected}: {act:?}"
        );
    }
    for expected in ["console", "page_errors", "network"] {
        assert!(
            inspect.contains(&expected.to_string()),
            "browser_inspect lost {expected}: {inspect:?}"
        );
    }
    // No action appears in two tools: three jobs, not one door repeated.
    for a in &act {
        assert!(
            !tab.contains(a),
            "{a} is in both browser_tab and browser_act"
        );
    }
}

/// A browser can be named per call, and only from the set CodeLeveler drives.
#[test]
fn the_browser_can_be_named_on_the_call_that_starts_one() {
    let tab = schema("browser_tab");
    assert!(
        tab["properties"].get("browser").is_some(),
        "browser_tab must accept an explicit browser: {tab}"
    );
    let description = surface()
        .definitions()
        .into_iter()
        .find(|d| d.name == "browser_tab")
        .unwrap()
        .description;
    assert!(
        description.contains("default browser"),
        "the default-browser rule belongs in the tool's own description: {description}"
    );
}

/// The description says what the capability IS, never what the model should do
/// with it. No workflow advice on the surface.
#[test]
fn the_descriptions_state_capability_not_procedure() {
    for name in ["browser_tab", "browser_act", "browser_inspect"] {
        let d = surface()
            .definitions()
            .into_iter()
            .find(|x| x.name == name)
            .unwrap()
            .description
            .to_lowercase();
        for advice in [
            "you should",
            "always ",
            "first ",
            "make sure",
            "remember to",
            "if it fails",
        ] {
            assert!(!d.contains(advice), "{name} carries procedure advice: {d}");
        }
    }
}
