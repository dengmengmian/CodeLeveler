//! AVAILABLE ≠ ENABLED ≠ EXPOSED.
//!
//! A capability the machine can provide is not a capability the model sees.
//! These pin the operator that keeps the three apart, so "the host supports
//! it" can never grow back into "advertise it".

use leveler_tools::{CapabilityPacks, core_surface, model_surface};

/// In-process capability handles: the composition under test is about which
/// tools exist, not about which host services back them.
fn caps() -> leveler_tools::Capabilities {
    leveler_tools::Capabilities::in_process(std::sync::Arc::new(
        leveler_core::environment().clone(),
    ))
}

/// The same handles, plus the search key a configured host would have found.
fn caps_with_search_key() -> leveler_tools::Capabilities {
    caps().with_search_api_key(Some("tvly-test-value".to_string()))
}

fn names(registry: &leveler_tools::ToolRegistry) -> Vec<String> {
    let mut names: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
    names.sort();
    names
}

/// The whole point: a browser runtime installed on this machine (AVAILABLE)
/// buys the model nothing while the product mode asks for no optional pack
/// (not ENABLED). The exposed surface is the core primitives, unchanged.
#[test]
fn an_available_capability_the_product_did_not_enable_is_not_exposed() {
    let available = CapabilityPacks::ALL;
    let enabled = CapabilityPacks::NONE;
    let exposed = enabled.intersect(available);

    assert_eq!(exposed, CapabilityPacks::NONE);
    assert_eq!(
        names(&model_surface(exposed, &caps())),
        names(&core_surface(&caps()))
    );
    assert!(
        !names(&model_surface(exposed, &caps()))
            .iter()
            .any(|n| n.starts_with("browser_")),
        "an unenabled capability must not reach the model"
    );
}

/// And the mirror: asking for a capability this machine cannot provide does
/// not conjure it. A host with no search key exposes no `web_search` however
/// much the product mode wants one.
#[test]
fn an_enabled_capability_the_machine_lacks_is_not_exposed() {
    let available = CapabilityPacks {
        web_search: false,
        browser: false,
        ..CapabilityPacks::ALL
    };
    let exposed = CapabilityPacks::ALL.intersect(available);

    assert!(!exposed.web_search);
    assert!(!exposed.browser);
    let got = names(&model_surface(exposed, &caps()));
    assert!(!got.iter().any(|n| n == "web_search"), "{got:?}");
    assert!(!got.iter().any(|n| n.starts_with("browser_")), "{got:?}");
    // Everything both sides agreed on is still there.
    assert!(got.iter().any(|n| n == "web_fetch"));
    assert!(got.iter().any(|n| n == "git_status"));
}

/// Intersection is symmetric and neither side can widen the other — the
/// property that makes "EXPOSED = ENABLED ∩ AVAILABLE" a boundary rather than
/// a suggestion.
#[test]
fn neither_side_of_the_intersection_can_widen_the_other() {
    let one = CapabilityPacks {
        vcs: true,
        media: true,
        ..CapabilityPacks::NONE
    };
    let other = CapabilityPacks {
        media: true,
        browser: true,
        ..CapabilityPacks::NONE
    };
    assert_eq!(one.intersect(other), other.intersect(one));
    assert_eq!(
        one.intersect(other),
        CapabilityPacks {
            media: true,
            ..CapabilityPacks::NONE
        }
    );
    assert_eq!(
        CapabilityPacks::NONE.intersect(CapabilityPacks::ALL),
        CapabilityPacks::NONE
    );
    assert_eq!(
        CapabilityPacks::ALL.intersect(CapabilityPacks::ALL),
        CapabilityPacks::ALL
    );
}

/// The exposed surface is composed once, from the packs — it does not depend
/// on how many times it is asked, and nothing about a turn can change it.
#[test]
fn composition_is_a_pure_function_of_the_exposed_packs() {
    for packs in [
        CapabilityPacks::NONE,
        CapabilityPacks::ALL,
        CapabilityPacks {
            vcs: true,
            ..CapabilityPacks::NONE
        },
    ] {
        assert_eq!(
            names(&model_surface(packs, &caps())),
            names(&model_surface(packs, &caps()))
        );
    }
}

/// Case C — a configured search key makes `web_search` AVAILABLE, and a
/// product mode that enables the pack exposes exactly that one tool.
#[test]
fn a_configured_search_key_exposes_web_search() {
    let available = CapabilityPacks {
        web_search: true,
        ..CapabilityPacks::NONE
    };
    let exposed = CapabilityPacks::ALL.intersect(available);

    assert!(exposed.web_search);
    let got = names(&model_surface(exposed, &caps_with_search_key()));
    assert!(got.iter().any(|n| n == "web_search"), "{got:?}");
    assert_eq!(
        got.len(),
        names(&core_surface(&caps())).len() + 1,
        "the search pack adds exactly its own tool"
    );
}

/// The invariant a working key must not break: `Economy` enables no optional
/// pack, so a configured search key still shows an Economy turn no
/// `web_search`. AVAILABLE alone buys nothing.
#[test]
fn a_configured_search_key_is_still_invisible_under_economy() {
    let available = CapabilityPacks::ALL;
    let enabled = CapabilityPacks::NONE; // Economy
    let exposed = enabled.intersect(available);

    assert!(!exposed.web_search);
    let got = names(&model_surface(exposed, &caps_with_search_key()));
    assert!(!got.iter().any(|n| n == "web_search"), "{got:?}");
}

/// Case D — the model's tool contract is `query` + `count`. Swapping the
/// search backend must never widen the schema with provider parameters.
#[test]
fn web_search_schema_stays_query_and_count() {
    let registry = model_surface(CapabilityPacks::ALL, &caps_with_search_key());
    let schema = registry
        .definitions()
        .into_iter()
        .find(|d| d.name == "web_search")
        .expect("web_search on an all-packs surface")
        .input_schema;
    let props = schema["properties"].as_object().expect("properties");
    let mut fields: Vec<&str> = props.keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(fields, ["count", "query"]);
}
