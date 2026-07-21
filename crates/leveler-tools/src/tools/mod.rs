//! Built-in tools (spec §18.3).

mod apply_patch;
mod blast_radius;
mod checkpoint;
mod diagnostics;
mod expand_tools;
mod find_references;
mod find_symbol;
mod format;
mod git;
mod glob;
mod grep;
mod list_files;
mod locate_hint;
mod memory;
pub mod patch;
mod read_file;
mod read_symbol;
mod replace;
mod repository_search;
mod run_command;
mod shell_command;
mod shell_guard;
mod skills;
mod symbols;
mod task_control;
mod update_plan;
mod view_image;
mod web_fetch;
mod web_search;

pub use apply_patch::ApplyPatchTool;
pub use blast_radius::BlastRadiusTool;
pub use checkpoint::{CreateCheckpointTool, RestoreCheckpointTool};
pub use diagnostics::DiagnosticsTool;
pub use expand_tools::ExpandToolsTool;
pub use find_references::FindReferencesTool;
pub use find_symbol::FindSymbolTool;
pub use git::{GitDiffTool, GitStatusTool};
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_files::ListFilesTool;
pub use memory::{ConsolidateMemoryTool, ForgetTool, MemoryTool, RememberTool};
pub use read_file::ReadFileTool;
pub use read_symbol::ReadSymbolTool;
pub use replace::ReplaceTool;
pub use repository_search::RepositorySearchTool;
pub use run_command::RunCommandTool;
pub use shell_command::ShellCommandTool;
pub use skills::{CreateSkillTool, LoadSkillTool};
pub use task_control::{GetTaskTool, KillTaskTool, WaitTaskTool};
pub use update_plan::UpdatePlanTool;
pub use view_image::ViewImageTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;

use crate::tool::ToolError;

/// Deserialize schema-validated arguments into a typed input, mapping failure to
/// [`ToolError::InvalidArguments`].
fn parse_input<T: serde::de::DeserializeOwned>(
    tool: &str,
    input: serde_json::Value,
) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|e| ToolError::InvalidArguments {
        tool: tool.to_string(),
        message: e.to_string(),
    })
}

/// Serialize a tool's input schema from its `schemars`-derived type.
///
/// `schemars` 0.8 emits draft-07, which spells nested definitions `definitions`
/// and references them as `#/definitions/X`. Providers that validate tool
/// schemas against draft 2020-12 reject that outright — Moonshot/Kimi answers
/// `references must start with #/$defs/` and refuses the whole request. We
/// normalize to 2020-12 (`$defs`) because every provider we target accepts it,
/// while the reverse is not true.
///
/// The `$schema` key is dropped rather than bumped: a tool schema has no use for
/// it, and leaving a draft-07 declaration next to `$defs` would be self-contradictory.
fn schema_of<T: schemars::JsonSchema>() -> serde_json::Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(T)).unwrap_or(serde_json::Value::Null);
    normalize_to_draft_2020_12(&mut schema);
    schema
}

/// Rename the top-level `definitions` to `$defs`, drop `$schema`, and rewrite
/// every `#/definitions/…` reference to `#/$defs/…`.
fn normalize_to_draft_2020_12(schema: &mut serde_json::Value) {
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        if let Some(defs) = object.remove("definitions") {
            object.insert("$defs".to_string(), defs);
        }
    }
    rewrite_refs(schema);
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

    /// schemars 0.8 emits draft-07 (`definitions` + `#/definitions/X`). Moonshot
    /// / Kimi validates tool schemas against draft 2020-12 and rejects the tool
    /// outright: "references must start with #/$defs/".
    #[test]
    fn nested_types_are_referenced_under_defs_not_definitions() {
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
            schema.get("$defs").is_some(),
            "nested type must live under `$defs`: {text}"
        );
        assert!(
            text.contains("#/$defs/Item"),
            "the $ref must point into $defs: {text}"
        );
    }
}
