//! Stable behavioral metadata for built-in tool names.
//!
//! Presentation remains owned by each client, but execution policy must not
//! duplicate name lists and argument-field guesses across crates.

/// Execution-relevant class of a built-in tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinToolClass {
    /// Read-only repository/context search, subject to the search-call budget.
    Search,
    /// Other safely replayable read/observe operation.
    Read,
    /// A tool with side effects.
    Write,
}

/// Behavioral metadata shared by agent, protocol, and clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinToolMetadata {
    /// Broad execution class.
    pub class: BuiltinToolClass,
    /// Primary human-facing argument, when there is one.
    pub primary_argument: Option<&'static str>,
    /// Argument used to distinguish repeated pure observations.
    pub observe_argument: Option<&'static str>,
}

/// Metadata for a known built-in tool.
pub fn builtin_tool_metadata(name: &str) -> Option<BuiltinToolMetadata> {
    let metadata = match name {
        "grep" => BuiltinToolMetadata {
            class: BuiltinToolClass::Search,
            primary_argument: Some("pattern"),
            observe_argument: Some("pattern"),
        },
        "find_files" => BuiltinToolMetadata {
            class: BuiltinToolClass::Search,
            primary_argument: Some("pattern"),
            observe_argument: Some("pattern"),
        },
        "find_symbol" | "read_symbol" | "find_references" => BuiltinToolMetadata {
            class: BuiltinToolClass::Search,
            primary_argument: Some("symbol"),
            observe_argument: None,
        },
        "list_files" => BuiltinToolMetadata {
            class: BuiltinToolClass::Read,
            primary_argument: Some("path"),
            observe_argument: Some("path"),
        },
        "read_file" | "git_status" | "git_diff" | "view_image" | "web_search" | "web_fetch" => {
            BuiltinToolMetadata {
                class: BuiltinToolClass::Read,
                primary_argument: None,
                observe_argument: None,
            }
        }
        "apply_patch" | "replace" | "run_command" | "shell_command" => BuiltinToolMetadata {
            class: BuiltinToolClass::Write,
            primary_argument: None,
            observe_argument: None,
        },
        _ => return None,
    };
    Some(metadata)
}

/// Whether this built-in counts against the per-step search budget.
pub fn is_search_tool(name: &str) -> bool {
    builtin_tool_metadata(name).is_some_and(|metadata| metadata.class == BuiltinToolClass::Search)
}

/// Whether replaying the built-in after a crash is side-effect free.
pub fn is_safe_replay_tool(name: &str) -> bool {
    builtin_tool_metadata(name).is_some_and(|metadata| metadata.class != BuiltinToolClass::Write)
}

/// Pure-observation fingerprint for built-ins that participate in loop guards.
pub fn builtin_observe_key(name: &str, arguments: &serde_json::Value) -> Option<String> {
    let metadata = builtin_tool_metadata(name)?;
    let argument = metadata.observe_argument?;
    let default = if name == "list_files" { "." } else { "" };
    let value = arguments
        .get(argument)
        .and_then(serde_json::Value::as_str)
        .unwrap_or(default);
    Some(format!("observe:{name}:{value}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_files_has_one_consistent_search_contract() {
        let metadata = builtin_tool_metadata("find_files").unwrap();
        assert_eq!(metadata.class, BuiltinToolClass::Search);
        assert_eq!(metadata.primary_argument, Some("pattern"));
        assert_eq!(metadata.observe_argument, Some("pattern"));
        assert_eq!(
            builtin_observe_key(
                "find_files",
                &serde_json::json!({"pattern": "**/*_test.go"})
            )
            .as_deref(),
            Some("observe:find_files:**/*_test.go")
        );
        assert!(builtin_tool_metadata("repository_search").is_none());
        assert!(builtin_tool_metadata("glob").is_none());
    }
}
