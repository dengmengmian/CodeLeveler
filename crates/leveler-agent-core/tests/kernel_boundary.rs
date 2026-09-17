//! Architecture tripwire: the agent kernel stays embeddable.
//!
//! The kernel is the generic model↔tool loop. It may know about the
//! provider-neutral model vocabulary and nothing else in this workspace — no
//! repository, no workspace, no permission plane, no storage, no product. A
//! dependency added here would be invisible in review and would quietly make
//! the kernel un-embeddable, so the manifest is asserted rather than trusted.

use std::path::Path;

/// Workspace crates the kernel must never reach. Anything named
/// `leveler-*` that is not on the allowlist below is a violation, so a crate
/// added to the workspace later is caught without editing this list.
const ALLOWED_INTERNAL: &[&str] = &[
    // The provider-neutral request/response/message/tool-call vocabulary the
    // loop is written against.
    "leveler-model",
    // Ids and time primitives, used by the example and the tests only.
    "leveler-core",
];

fn manifest() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    std::fs::read_to_string(path).expect("the kernel's manifest must be readable")
}

/// Every `leveler-*` dependency line in the manifest, dev-dependencies
/// included: a dev-dependency on the harness would still prove the kernel
/// cannot be built without it.
fn internal_dependencies(manifest: &str) -> Vec<String> {
    manifest
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("leveler-"))
        .filter_map(|line| line.split_once(' ').map(|(name, _)| name.to_string()))
        .collect()
}

#[test]
fn the_kernel_depends_on_no_codeleveler_harness_or_runtime_crate() {
    let manifest = manifest();
    let forbidden: Vec<String> = internal_dependencies(&manifest)
        .into_iter()
        .filter(|name| !ALLOWED_INTERNAL.contains(&name.as_str()))
        .collect();
    assert!(
        forbidden.is_empty(),
        "the agent kernel must stay embeddable; these dependencies make it \
         a CodeLeveler component instead: {}",
        forbidden.join(", ")
    );
}

/// The allowlist is the contract, so it must not rot into a list of crates
/// the kernel stopped using — that would let a re-added dependency slip in.
#[test]
fn the_allowlist_names_only_dependencies_that_exist() {
    let manifest = manifest();
    let declared = internal_dependencies(&manifest);
    for allowed in ALLOWED_INTERNAL {
        assert!(
            declared.iter().any(|name| name == allowed),
            "{allowed} is allowlisted but no longer a dependency; drop it from the list"
        );
    }
}

/// The kernel owns no security decision. Permission, write scope, sandboxing
/// and approval are the host's, and a symbol that names one appearing here
/// means a policy has started to live in the wrong layer.
#[test]
fn the_kernel_names_no_permission_or_write_scope_concept() {
    // "admission" is deliberately absent: the kernel admits a *round* against
    // a budget, which is a different question from admitting a tool call
    // against a policy. The host's own vocabulary is what must not appear.
    const FORBIDDEN: &[&str] = &[
        "WriteScope",
        "PermissionProfile",
        "ResolvedExecutionPolicy",
        "ApprovalDecision",
        "AdmittedCall",
        "ToolHost",
        "Workspace",
        "Sandbox",
    ];
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("the kernel's sources must be readable") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("source must be readable");
            for term in FORBIDDEN {
                if source.contains(term) {
                    violations.push(format!("{}: mentions `{term}`", path.display()));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "security policy belongs to the host, never to the kernel:\n{}",
        violations.join("\n")
    );
}
