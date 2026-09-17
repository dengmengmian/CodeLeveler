//! Built-in tools (spec §18.3).

pub mod applied_diff;
mod apply_patch;
mod blast_radius;
mod browser;
pub(crate) mod command_execution;
pub(crate) use command_execution::CommandExecution;
mod diagnostics;
mod find_files;
mod find_references;
mod find_symbol;
mod git;
mod grep;
mod list_files;
mod locate_hint;
mod memory;
pub mod patch;
mod read_file;
mod read_symbol;
mod run_command;
mod shell_command;
mod shell_guard;
pub use shell_guard::refuse_shell_script;
mod skills;
mod symbols;
mod task_control;
mod view_image;
mod web_fetch;
mod web_search;
mod write_file;

pub use apply_patch::ApplyPatchTool;
pub use blast_radius::BlastRadiusTool;
pub use browser::{BrowserActTool, BrowserInspectTool, BrowserTabTool};
pub use diagnostics::DiagnosticsTool;
pub use find_files::FindFilesTool;
pub use find_references::FindReferencesTool;
pub use find_symbol::FindSymbolTool;
pub use git::{GitDiffTool, GitStatusTool};
pub use grep::GrepTool;
pub use list_files::ListFilesTool;
pub use memory::{ForgetTool, MemoryRoot, MemoryTool, RememberTool};
pub use read_file::ReadFileTool;
pub use read_symbol::ReadSymbolTool;
pub use run_command::RunCommandTool;
pub use shell_command::ShellCommandTool;
pub use skills::LoadSkillTool;
pub use task_control::{GetTaskTool, KillTaskTool, WaitTaskTool, wait_interval};
pub use view_image::ViewImageTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use write_file::WriteFileTool;

use crate::tool::ToolError;

/// Deserialize schema-validated arguments into a typed input, mapping failure to
/// [`ToolError::InvalidArguments`].
pub fn parse_input<T: serde::de::DeserializeOwned>(
    tool: &str,
    input: serde_json::Value,
) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|e| ToolError::InvalidArguments {
        tool: tool.to_string(),
        message: e.to_string(),
    })
}

/// Serialize a tool's input schema from its `schemars`-derived type, with every
/// reference expanded.
///
/// `schemars` 0.8 emits draft-07, which spells nested definitions `definitions`
/// and references them as `#/definitions/X`. Providers that validate tool
/// schemas against draft 2020-12 reject that outright — Moonshot/Kimi answers
/// `references must start with #/$defs/` — and we rewrite the spelling below
/// because every provider we target accepts `$defs` while the reverse is not true.
///
/// Spelling is not the whole problem, though: what a provider *does* with a
/// reference is its own invention, and Moonshot's is unusable. It refuses the
/// whole request when a reference has to be followed from inside another
/// reference (`detected infinite recursion without termination condition`) —
/// which is exactly `update_plan`, whose nested `PlanItem.status` is itself a
/// `schemars` reference — and, independently, when one points at a `oneOf`
/// (`save_agent.capability`) or at a definition that contains a nested `oneOf`.
/// No subset of reference usage survives those rules, so we send none: the
/// document is dereferenced and `$defs` is dropped with it. Tool schemas are
/// small, and expanding them is usually *smaller* than carrying the definitions.
///
/// The `$schema` key is dropped rather than bumped: a tool schema has no use for
/// it, and leaving a draft-07 declaration next to `$defs` would be self-contradictory.
///
/// A self-referential type has no finite expansion. Such a `$ref` is left where
/// it is, and `$defs` is kept for it to point at: a provider that refuses
/// references will refuse that schema, which is the honest outcome — the type
/// cannot be expressed for it at all.
///
/// Measured against the Kimi Coding endpoint on 2026-09-17: all 28 registered
/// tools are accepted once dereferenced.
pub fn schema_of<T: schemars::JsonSchema>() -> serde_json::Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(T)).unwrap_or(serde_json::Value::Null);
    normalize_to_draft_2020_12(&mut schema);
    dereference(&mut schema);
    schema
}

/// Rename the top-level `definitions` to `$defs`, drop `$schema`, and rewrite
/// every `#/definitions/…` reference to `#/$defs/…`.
///
/// Only a reference kept by [`dereference`] can survive this, but that one still
/// has to be spelled in the dialect the provider validates against.
fn normalize_to_draft_2020_12(schema: &mut serde_json::Value) {
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        if let Some(defs) = object.remove("definitions") {
            object.insert("$defs".to_string(), defs);
        }
    }
    rewrite_refs(schema);
}

/// Replace every `$ref` with the definition it names, dropping `$defs` when
/// nothing points into it any more. See [`schema_of`] for why.
fn dereference(schema: &mut serde_json::Value) {
    let Some(defs) = schema.get("$defs").cloned() else {
        return;
    };
    expand_refs(schema, &defs, &mut Vec::new());
    if !has_ref(schema)
        && let Some(object) = schema.as_object_mut()
    {
        object.remove("$defs");
    }
}

/// Expand `value` in place if it is a reference, then recurse into whatever it
/// became.
fn expand_refs(value: &mut serde_json::Value, defs: &serde_json::Value, open: &mut Vec<String>) {
    if let Some(expanded) = expanded_ref(value, defs, open) {
        *value = expanded;
        return;
    }
    match value {
        serde_json::Value::Object(map) => map.values_mut().for_each(|v| expand_refs(v, defs, open)),
        serde_json::Value::Array(items) => {
            items.iter_mut().for_each(|v| expand_refs(v, defs, open))
        }
        _ => {}
    }
}

/// The definition `value` names, with `value`'s own sibling keywords merged over
/// it (2020-12 applies both, the sibling more specifically). `None` when `value`
/// is not a reference, names nothing, or names something already open — the last
/// being the cycle [`dereference`] must leave alone.
fn expanded_ref(
    value: &serde_json::Value,
    defs: &serde_json::Value,
    open: &mut Vec<String>,
) -> Option<serde_json::Value> {
    let map = value.as_object()?;
    let name = map
        .get("$ref")?
        .as_str()?
        .strip_prefix("#/$defs/")?
        .to_string();
    if open.contains(&name) {
        return None;
    }
    let mut merged = defs.get(&name)?.clone();
    if let Some(object) = merged.as_object_mut() {
        for (key, child) in map {
            if key != "$ref" {
                object.insert(key.clone(), child.clone());
            }
        }
    }
    open.push(name);
    expand_refs(&mut merged, defs, open);
    open.pop();
    Some(merged)
}

fn has_ref(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => map.contains_key("$ref") || map.values().any(has_ref),
        serde_json::Value::Array(items) => items.iter().any(has_ref),
        _ => false,
    }
}

/// Recursively repoint `$ref` strings from the draft-07 path to the 2020-12 one.
fn rewrite_refs(value: &mut serde_json::Value) {
    const OLD: &str = "#/definitions/";
    const NEW: &str = "#/$defs/";
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if key == "$ref"
                    && let Some(reference) = child.as_str()
                    && let Some(rest) = reference.strip_prefix(OLD)
                {
                    *child = serde_json::Value::String(format!("{NEW}{rest}"));
                    continue;
                }
                rewrite_refs(child);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(rewrite_refs),
        _ => {}
    }
}

/// A process-unique counter for uniquely-named temp dirs in tests (avoids
/// pulling in rand/time).
#[cfg(test)]
pub(crate) fn test_ordinal() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// A [`crate::tool::ToolContext`] over an existing directory. The shared
/// construction for tool tests; use [`test_ctx`] when the test does not
/// already own a directory.
#[cfg(test)]
pub(crate) fn test_ctx_in(
    dir: &std::path::Path,
    profile: leveler_execution::PermissionProfile,
) -> crate::tool::ToolContext {
    crate::tool::ToolContext::new(leveler_execution::Workspace::new(dir).unwrap(), profile)
}

/// A [`crate::tool::ToolContext`] over a fresh unique temp dir, optionally
/// pre-seeded with `(relative path, content)` files. Returns the dir so tests
/// can inspect or mutate the workspace behind the tool's back.
#[cfg(test)]
pub(crate) fn test_ctx(
    profile: leveler_execution::PermissionProfile,
    files: &[(&str, &str)],
) -> (crate::tool::ToolContext, std::path::PathBuf) {
    // Pid-qualified: the ordinal restarts at 0 each run, and a stale dir from
    // a previous run would leak files into this workspace.
    let dir = std::env::temp_dir().join(format!(
        "leveler-ctx-{}-{}",
        std::process::id(),
        test_ordinal()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (rel, content) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
    }
    (test_ctx_in(&dir, profile), dir)
}

/// In-process capability handles for tool tests: a fresh language-server pool
/// and background registry, no store, no memory root, no browser.
#[cfg(test)]
pub(crate) fn test_capabilities() -> crate::capabilities::Capabilities {
    crate::capabilities::Capabilities::in_process(std::sync::Arc::new(
        leveler_core::environment().clone(),
    ))
}

/// The shared command runtime for tool tests.
#[cfg(test)]
pub(crate) fn test_commands() -> std::sync::Arc<CommandExecution> {
    let capabilities = test_capabilities();
    std::sync::Arc::new(CommandExecution::new(
        capabilities.background_tasks,
        capabilities.artifact_store,
    ))
}

#[cfg(test)]
mod schema_tests {
    use super::schema_of;

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct Item {
        step: String,
    }

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct Args {
        plan: Vec<Item>,
    }

    #[derive(schemars::JsonSchema)]
    #[serde(rename_all = "snake_case")]
    #[allow(dead_code)]
    enum Status {
        Pending,
        InProgress,
    }

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct Step {
        /// What the step is doing.
        status: Status,
        owner: Assignee,
    }

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct Assignee {
        name: String,
    }

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct PlanArgs {
        plan: Vec<Step>,
        backup: Assignee,
    }

    /// A recursive type has no finite expansion, so `dereference` must leave its
    /// reference alone — and then the `$defs` it points into has to stay, or the
    /// schema would name a definition that is no longer there.
    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct Node {
        children: Vec<Node>,
    }

    /// schemars 0.8 emits draft-07 (`definitions` + `#/definitions/X`). Moonshot
    /// / Kimi validates tool schemas against draft 2020-12 and rejects the tool
    /// outright: "references must start with #/$defs/".
    #[test]
    fn nested_types_carry_no_draft_07_definitions() {
        let schema = schema_of::<Args>();
        let text = serde_json::to_string(&schema).unwrap();

        assert!(
            !text.contains("#/definitions/"),
            "draft-07 $ref path leaks to providers: {text}"
        );
        assert!(
            schema.get("definitions").is_none(),
            "draft-07 `definitions` key must be renamed: {text}"
        );
        assert!(
            schema.get("$defs").is_none(),
            "an expanded schema needs no definitions at all: {text}"
        );
    }

    /// A reference reaches a provider as the definition it names, at every
    /// depth. Moonshot refuses a reference followed from inside another
    /// reference — `update_plan`'s nested `PlanItem.status` is that shape — so
    /// no `$ref` may survive anywhere in the document.
    #[test]
    fn a_reference_is_expanded_wherever_it_appears() {
        let schema = schema_of::<PlanArgs>();
        let text = serde_json::to_string(&schema).unwrap();

        assert!(!text.contains("$ref"), "a reference leaked: {text}");
        let status =
            serde_json::to_string(&schema["properties"]["plan"]["items"]["properties"]["status"])
                .unwrap();
        assert!(status.contains(r#"["pending","in_progress"]"#), "{status}");
        assert!(status.contains("What the step is doing."), "{status}");
    }

    /// One definition used twice expands twice, with nothing left to share.
    #[test]
    fn a_definition_used_twice_is_expanded_twice() {
        let schema = schema_of::<PlanArgs>();

        assert_eq!(
            schema["properties"]["plan"]["items"]["properties"]["owner"]["properties"]["name"]["type"],
            "string"
        );
        assert_eq!(
            schema["properties"]["backup"]["properties"]["name"]["type"],
            "string"
        );
    }

    /// A recursive type has no finite expansion. `dereference` must leave its
    /// reference alone rather than hang, and must then keep the `$defs` entry
    /// that reference still names — removing it would leave a dangling `$ref`.
    #[test]
    fn a_recursive_type_keeps_its_reference_instead_of_hanging() {
        let schema = schema_of::<Node>();
        let text = serde_json::to_string(&schema).unwrap();

        assert!(text.contains("$ref"), "the cycle must survive: {text}");
        assert_eq!(schema["$defs"]["Node"]["type"], "object", "{text}");
    }
}
