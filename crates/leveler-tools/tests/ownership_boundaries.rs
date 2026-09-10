//! Architecture tripwires for the ownership boundaries W2 closed.
//!
//! Each of these boundaries had already grown back once, so each is checked
//! mechanically rather than trusted to review. Textual on purpose: cheap,
//! obvious, and a rename that defeats a pattern is caught in review because
//! this file names the contract in prose next to it.

use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn crate_sources(relative: &str) -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    assert!(!files.is_empty(), "no sources under {relative}");
    files
}

/// Strip whitespace so a line break cannot hide a match.
fn condensed(source: &str) -> String {
    source.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The executable half of a file: everything before the first `#[cfg(test)]`,
/// with comment lines dropped.
///
/// Both matter. A test may legitimately use `"read_file"` as a sample tool
/// name, and prose next to a boundary usually has to say what the boundary
/// keeps out — neither is the thing under test.
fn production_code(source: &str) -> String {
    source
        .split("#[cfg(test)]")
        .next()
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The FOUNDATION does not know the product's tool names.
///
/// `leveler-model` speaks the model/tool protocol: `ToolDefinition`,
/// `ToolCall`, `ToolResult`, `ToolChoice`. It used to carry a hardcoded
/// behavioral table of coding tool names — `read_file`, `grep`,
/// `apply_patch`, `git_diff` and the rest, still including a `replace` that
/// had already been deleted — so a change to the coding surface meant editing
/// a foundation crate, and the table could disagree with the tools it
/// described. The authority for "what does this tool do" is the `Tool` trait
/// itself; nothing needs a second copy keyed on the name.
#[test]
fn the_model_foundation_holds_no_coding_tool_names() {
    const CODING_TOOL_NAMES: &[&str] = &[
        "read_file",
        "list_files",
        "find_files",
        "apply_patch",
        "write_file",
        "run_command",
        "shell_command",
        "git_status",
        "git_diff",
        "view_image",
        "web_search",
        "web_fetch",
        "find_symbol",
        "read_symbol",
        "find_references",
        "update_plan",
    ];
    let mut violations = Vec::new();
    for path in crate_sources("../leveler-model/src") {
        let source =
            production_code(&std::fs::read_to_string(&path).expect("source must be readable"));
        for name in CODING_TOOL_NAMES {
            if source.contains(&format!("\"{name}\"")) {
                violations.push(format!(
                    "{}: names the coding tool `{name}`",
                    path.display()
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "the model foundation must not know the coding surface:\n{}",
        violations.join("\n")
    );
}

/// The REGISTRY is not a policy engine.
///
/// It registers, looks up, normalizes, schema-validates, dispatches, and
/// bounds the result. Whether a call may happen at all is the ToolHost's one
/// decision; the registry used to re-decide the read-only overlay, the
/// zero-write-authority refusal and the profile's hard forbid, which gave the
/// build three authorization owners that had to agree.
#[test]
fn the_registry_enforces_no_authorization_policy() {
    const POLICY_PATTERNS: &[&str] = &[
        "policy.read_only",
        "policy.mode().permits",
        "has_zero_write_authority",
        "NotPermitted",
        "claim_write_scope",
    ];
    let source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/registry.rs"))
            .expect("registry source must be readable");
    let flat = condensed(&production_code(&source));
    let found: Vec<&str> = POLICY_PATTERNS
        .iter()
        .copied()
        .filter(|p| flat.contains(&condensed(p)))
        .collect();
    assert!(
        found.is_empty(),
        "ToolRegistry must not decide authorization; move these to the \
         ToolHost: {found:?}"
    );
}

/// The tool CONTEXT is not a service locator.
///
/// It carries the execution substrate this call is anchored to, the per-call
/// authority, and the session identity. It used to carry a `ToolServices`
/// facet holding the language-server pool, the browser runtime, the memory
/// root, the artifact store and the background task registry — so `read_file`
/// was handed the browser and `grep` could start a language server. Every tool
/// is constructed with the handles it uses instead.
#[test]
fn the_tool_context_carries_no_capability_handles() {
    const CAPABILITY_FIELDS: &[&str] = &[
        "ToolServices",
        "lsp_sessions",
        "lsp_start_locks",
        "memory_root",
        "background_tasks",
        "artifact_store",
        "browser:",
    ];
    let source = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tool.rs"))
        .expect("tool source must be readable");
    let mut violations = Vec::new();
    for (number, line) in source.lines().enumerate() {
        // Prose may explain what LEFT; only declarations count.
        if line.trim_start().starts_with("//") || line.trim_start().starts_with("///") {
            continue;
        }
        for field in CAPABILITY_FIELDS {
            if line.contains(field) {
                violations.push(format!("src/tool.rs:{}: `{field}`", number + 1));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "a capability belongs to the tools that use it, not to every call's \
         context:\n{}",
        violations.join("\n")
    );
}

/// A capability tool does not carry a second copy of its runtime.
///
/// Language-server startup belongs to `leveler-lsp`'s session pool, git
/// invocation to `leveler-vcs`, image validation to `leveler-media`, process
/// launch to the shared command runtime. Each of these was, at some point,
/// also implemented inside a tool file.
#[test]
fn no_tool_reimplements_its_capability_runtime() {
    // (pattern, who owns it, files allowed to contain it)
    const OWNED_ELSEWHERE: &[(&str, &str, &[&str])] = &[
        ("LspClient::start", "leveler_lsp::LspSessions", &[]),
        (
            "ProcessRequest::new",
            "the shared command runtime / leveler-vcs",
            &["command_execution.rs"],
        ),
        ("infer::get", "leveler_media::process_image", &[]),
        ("Browser::new", "the host that owns the browser", &[]),
        (
            "CdpBackend::launch",
            "leveler_browser, which owns every browser protocol",
            &[],
        ),
        (
            "WebDriverBackend::launch",
            "leveler_browser, which owns every browser protocol",
            &[],
        ),
    ];
    let mut violations = Vec::new();
    for path in crate_sources("src/tools") {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let source =
            production_code(&std::fs::read_to_string(&path).expect("source must be readable"));
        let flat = condensed(&source);
        for (pattern, owner, allowed) in OWNED_ELSEWHERE {
            if allowed.contains(&name.as_str()) {
                continue;
            }
            if flat.contains(&condensed(pattern)) {
                violations.push(format!("{name}: `{pattern}` belongs to {owner}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "a tool must call its capability's owner, not re-implement it:\n{}",
        violations.join("\n")
    );
}
