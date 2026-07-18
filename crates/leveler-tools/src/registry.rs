//! The tool registry: registration, schema validation, and dispatch (spec §18.2).

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_model::ToolDefinition;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// Holds the available tools and validates arguments before dispatching.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. A later registration with the same name replaces it.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name(), tool);
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// A registry containing only the read-only (Safe-risk) tools — search,
    /// read, symbol lookup, git status/diff, plan. Used to build an `explorer`
    /// sub-agent that physically cannot modify the workspace: the write tools
    /// aren't present, so a forbidden edit fails as "unknown tool" rather than
    /// relying on the model to obey a prompt.
    pub fn read_only_subset(&self) -> ToolRegistry {
        use leveler_execution::RiskLevel;
        let mut subset = ToolRegistry::new();
        for tool in self.tools.values() {
            if tool.risk() == RiskLevel::Safe {
                subset.register(tool.clone());
            }
        }
        subset
    }

    /// The tool definitions to advertise to the model (sorted by name).
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// Validate arguments against the tool's schema, enforce the execution-mode
    /// permission, then execute. Invalid JSON is never guessed (spec §10.4).
    pub async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self
            .get(name)
            .ok_or_else(|| ToolError::NotFound(name.to_string()))?
            .clone();

        if context.read_only && tool.risk() != RiskLevel::Safe {
            return Err(ToolError::NotPermitted {
                tool: name.to_string(),
                mode: context.mode,
                risk: tool.risk(),
            });
        }
        if !context.mode.permits(tool.risk()) {
            return Err(ToolError::NotPermitted {
                tool: name.to_string(),
                mode: context.mode,
                risk: tool.risk(),
            });
        }

        let input = tool.normalize_input(input);
        validate_schema(name, &tool.input_schema(), &input)?;

        let mut output = tool.execute(input, context, cancellation).await?;
        // Central guard: no single tool result may flood the context window,
        // whatever the tool's own limits are (some search tools have none). Keep
        // the head and the tail — errors and test failures often land at the end.
        output.content = cap_output(&output.content);
        Ok(output)
    }
}

/// Hard ceiling on any single tool result (~12k tokens). Bytes, not chars.
const MAX_TOOL_OUTPUT: usize = 48 * 1024;

/// Truncate `s` to [`MAX_TOOL_OUTPUT`] bytes, keeping the head (⅔) and tail (⅓)
/// with an elision marker between. Slices only on UTF-8 boundaries.
///
/// Public because this is the ONE tool-result cap: `execute` applies it to
/// every registry tool, and the executor reuses it for content that bypasses
/// the registry (sub-agent results).
pub fn cap_output(s: &str) -> String {
    if s.len() <= MAX_TOOL_OUTPUT {
        return s.to_string();
    }
    // Keep ½ head + ¼ tail (¾ of the budget), leaving ample room for the marker
    // so the result is always strictly smaller than both the input and the cap.
    let head = floor_boundary(s, MAX_TOOL_OUTPUT / 2);
    let tail = ceil_boundary(s, s.len() - MAX_TOOL_OUTPUT / 4);
    format!(
        "{}\n… [{} bytes elided to fit the context] …\n{}",
        &s[..head],
        tail - head,
        &s[tail..]
    )
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Validate `instance` against `schema`, returning a readable error listing the
/// schema violations.
fn validate_schema(
    tool: &str,
    schema: &serde_json::Value,
    instance: &serde_json::Value,
) -> Result<(), ToolError> {
    let validator = jsonschema::validator_for(schema).map_err(|e| ToolError::InvalidArguments {
        tool: tool.to_string(),
        message: format!("tool schema is itself invalid: {e}"),
    })?;

    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{} (at {})", e, e.instance_path))
        .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ToolError::InvalidArguments {
            tool: tool.to_string(),
            message: errors.join("; "),
        })
    }
}

/// Build a registry with every built-in tool registered.
pub fn default_registry() -> ToolRegistry {
    full_registry()
}

/// Full tool surface (historical default / WorkProfile::Balanced).
pub fn full_registry() -> ToolRegistry {
    use crate::tools;
    let mut registry = core_registry();
    registry.register(Arc::new(tools::RepositorySearchTool));
    registry.register(Arc::new(tools::FindSymbolTool));
    registry.register(Arc::new(tools::ReadSymbolTool));
    registry.register(Arc::new(tools::FindReferencesTool));
    registry.register(Arc::new(tools::WebSearchTool));
    registry.register(Arc::new(tools::WebFetchTool));
    registry.register(Arc::new(tools::ViewImageTool));
    registry.register(Arc::new(tools::GitStatusTool));
    registry.register(Arc::new(tools::GitDiffTool));
    registry.register(Arc::new(tools::CreateCheckpointTool));
    registry.register(Arc::new(tools::RestoreCheckpointTool));
    registry.register(Arc::new(tools::CreateSkillTool));
    registry.register(Arc::new(tools::RememberTool));
    registry.register(Arc::new(tools::ForgetTool));
    registry.register(Arc::new(tools::ConsolidateMemoryTool));
    registry
}

/// Economy / Core tool surface (appendix B). Missing categories via expand_tools.
pub fn core_registry() -> ToolRegistry {
    use crate::tools;
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(tools::ReadFileTool));
    registry.register(Arc::new(tools::ListFilesTool));
    registry.register(Arc::new(tools::GrepTool));
    registry.register(Arc::new(tools::ApplyPatchTool));
    registry.register(Arc::new(tools::ReplaceTool));
    registry.register(Arc::new(tools::RunCommandTool));
    registry.register(Arc::new(tools::ShellCommandTool));
    registry.register(Arc::new(tools::GetTaskTool));
    registry.register(Arc::new(tools::WaitTaskTool));
    registry.register(Arc::new(tools::KillTaskTool));
    registry.register(Arc::new(tools::UpdatePlanTool));
    registry.register(Arc::new(tools::LoadSkillTool));
    registry.register(Arc::new(tools::ExpandToolsTool));
    // Read-only memory search is Core; remember/forget stay Full (Dangerous).
    registry.register(Arc::new(tools::MemoryTool));
    registry
}

/// Register tools for an expand_tools category onto `registry` (idempotent names).
pub fn expand_tool_category(registry: &mut ToolRegistry, category: &str) {
    use crate::tools;
    match category {
        "search" => {
            registry.register(Arc::new(tools::RepositorySearchTool));
        }
        "lsp" => {
            registry.register(Arc::new(tools::FindSymbolTool));
            registry.register(Arc::new(tools::ReadSymbolTool));
            registry.register(Arc::new(tools::FindReferencesTool));
        }
        "git" => {
            registry.register(Arc::new(tools::GitStatusTool));
            registry.register(Arc::new(tools::GitDiffTool));
        }
        "web" => {
            registry.register(Arc::new(tools::WebSearchTool));
            registry.register(Arc::new(tools::WebFetchTool));
        }
        "checkpoint" => {
            registry.register(Arc::new(tools::CreateCheckpointTool));
            registry.register(Arc::new(tools::RestoreCheckpointTool));
        }
        "media" => {
            registry.register(Arc::new(tools::ViewImageTool));
        }
        "skills" => {
            registry.register(Arc::new(tools::CreateSkillTool));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_advertises_all_tools() {
        let reg = default_registry();
        let names: Vec<_> = reg.definitions().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"apply_patch".to_string()));
        assert!(names.contains(&"replace".to_string()));
        assert!(names.contains(&"run_command".to_string()));
        assert!(names.contains(&"shell_command".to_string()));
        assert!(names.contains(&"repository_search".to_string()));
        assert!(names.contains(&"find_symbol".to_string()));
        assert!(names.contains(&"read_symbol".to_string()));
        assert!(names.contains(&"find_references".to_string()));
        assert!(names.contains(&"restore_checkpoint".to_string()));
        assert!(names.contains(&"update_plan".to_string()));
        assert!(names.contains(&"web_search".to_string()));
        assert!(names.contains(&"web_fetch".to_string()));
        assert!(names.contains(&"view_image".to_string()));
        assert!(names.contains(&"load_skill".to_string()));
        assert!(names.contains(&"create_skill".to_string()));
        assert!(names.contains(&"expand_tools".to_string()));
        assert!(names.contains(&"memory".to_string()));
        assert!(names.contains(&"remember".to_string()));
        assert!(names.contains(&"forget".to_string()));
        assert_eq!(names.len(), 29);
    }

    #[test]
    fn core_registry_is_smaller_than_full_and_expand_adds_search() {
        let core = core_registry();
        let full = full_registry();
        let core_n = core.definitions().len();
        let full_n = full.definitions().len();
        assert!(core_n < full_n, "core={core_n} full={full_n}");
        let mut expanded = core_registry();
        expand_tool_category(&mut expanded, "search");
        let names: Vec<_> = expanded.definitions().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&"repository_search".to_string()));
    }

    #[test]
    fn cap_output_leaves_small_content_untouched() {
        assert_eq!(cap_output("hello"), "hello");
    }

    #[test]
    fn cap_output_truncates_and_keeps_head_and_tail() {
        let big = format!("HEAD{}TAIL", "x".repeat(MAX_TOOL_OUTPUT));
        let out = cap_output(&big);
        assert!(out.len() < big.len(), "should shrink");
        assert!(out.starts_with("HEAD"), "keeps the head");
        assert!(out.trim_end().ends_with("TAIL"), "keeps the tail");
        assert!(out.contains("elided"), "marks the elision");
    }

    #[test]
    fn cap_output_slices_on_utf8_boundaries() {
        // A multibyte-char payload past the limit must not panic mid-codepoint.
        let big = "\u{4e2d}".repeat(MAX_TOOL_OUTPUT); // each char is 3 bytes
        let out = cap_output(&big);
        assert!(out.len() < big.len());
        assert!(out.contains("elided"));
    }

    #[tokio::test]
    async fn rejects_unknown_tool() {
        let reg = default_registry();
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let err = reg
            .execute("nope", serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn rejects_tool_not_permitted_in_read_only_overlay() {
        let reg = default_registry();
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        // run_command is WorkspaceWrite; collaboration-plan / read_only blocks it.
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted)
            .with_read_only(true);
        let err = reg
            .execute(
                "run_command",
                serde_json::json!({"program": "echo", "args": ["hi"]}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotPermitted { .. }));
    }

    #[tokio::test]
    async fn validates_schema_errors() {
        let reg = default_registry();
        let dir =
            std::env::temp_dir().join(format!("leveler-schema-{}", crate::tools::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        // run_command requires `program` (string); missing it should fail schema validation.
        let err = reg
            .execute(
                "run_command",
                serde_json::json!({}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn update_plan_accepts_one_accidentally_nested_argument_envelope() {
        let reg = default_registry();
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = reg
            .execute(
                "update_plan",
                serde_json::json!({
                    "plan": [{
                        "explanation": "开始处理",
                        "plan": [
                            {"step": "定位根因", "status": "in_progress"},
                            {"step": "验证修复", "status": "pending"}
                        ]
                    }]
                }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .expect("a single nested update_plan envelope should be normalized");

        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("开始处理\n\n"), "{}", out.content);
        assert_eq!(out.metadata["plan"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn normalized_update_plan_still_enforces_the_canonical_schema() {
        let reg = default_registry();
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let err = reg
            .execute(
                "update_plan",
                serde_json::json!({
                    "plan": [{
                        "plan": [{"step": "定位根因", "status": "done"}]
                    }]
                }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .expect_err("normalization must not permit a non-canonical status");

        assert!(matches!(err, ToolError::InvalidArguments { .. }));
    }

    #[test]
    fn get_returns_registered_tool() {
        let reg = default_registry();
        assert!(reg.get("read_file").is_some());
        assert!(reg.get("does_not_exist").is_none());
    }

    #[test]
    fn register_replaces_existing_tool() {
        let mut reg = ToolRegistry::new();
        let tool = Arc::new(crate::tools::ReadFileTool);
        reg.register(tool.clone());
        assert!(reg.get("read_file").is_some());
        reg.register(tool);
        assert_eq!(reg.definitions().len(), 1);
    }
}
