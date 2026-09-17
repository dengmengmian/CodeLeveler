//! `find_files` — the `find` primitive: locate paths by glob.
//!
//! One meaning for one invocation. The pattern is always a glob, the candidate
//! set always comes from the same in-process traversal, and neither depends on
//! whether Git is installed or whether this directory is a repository.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use crate::workspace::{SearchError, WorkspaceSearch};

const DEFAULT_LIMIT: usize = 100;
const HARD_LIMIT: usize = 1000;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Glob over file paths, e.g. `**/*.rs` or `Cargo.toml`.
    pattern: String,
    /// Optional directory to search below. Defaults to the workspace root.
    #[serde(default)]
    path: Option<String>,
    /// Maximum results (default 100, hard cap 1000).
    #[serde(default, alias = "max_results")]
    limit: Option<usize>,
}

pub struct FindFilesTool;

#[async_trait]
impl Tool for FindFilesTool {
    fn name(&self) -> &'static str {
        "find_files"
    }

    fn description(&self) -> &'static str {
        "Find files by path. `pattern` is a glob: a pattern with no `/` matches \
         the file name at any depth (`Cargo.toml`, `*.rs`), and a pattern \
         containing `/` matches the path relative to the search root \
         (`src/**/*.rs`). It is never a substring match. Results are paths \
         relative to the workspace root. Use `grep` for file contents and \
         `list_files` to inspect one directory."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    /// Pure in-process traversal: no write, no subprocess, no language
    /// server, no network. Re-running it after a crash changes nothing.
    fn replay_is_side_effect_free(&self) -> bool {
        true
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let base_label = input.path.as_deref().unwrap_or(".");
        let base = context.execution.workspace.resolve_for_read(base_label)?;
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, HARD_LIMIT);

        let found = match WorkspaceSearch::find(
            &base,
            context.execution.workspace.root(),
            &input.pattern,
            limit,
            &cancellation,
        )
        .await
        {
            Ok(found) => found,
            Err(SearchError::NotDirectory) => {
                return Ok(ToolOutput::error(crate::recoverable::path_not_directory(
                    base_label,
                )));
            }
            Err(SearchError::InvalidGlob { pattern, message }) => {
                return Ok(ToolOutput::error(format!(
                    "`{pattern}` is not a valid glob: {message}"
                )));
            }
            Err(SearchError::Cancelled) => return Ok(ToolOutput::error("find_files cancelled")),
            Err(other) => return Err(ToolError::Io(format!("find {base_label}: {other:?}"))),
        };

        let mut body = if found.paths.is_empty() {
            "(no matching files)\n".to_string()
        } else {
            let mut body = String::new();
            for path in &found.paths {
                body.push_str(&path.0);
                body.push('\n');
            }
            body
        };
        if found.omitted > 0 {
            body.push_str(&format!(
                "… [{} more matches not shown; raise limit or narrow pattern/path]\n",
                found.omitted
            ));
        }
        Ok(ToolOutput::ok(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::Builder::new()
            .prefix("leveler-find-")
            .tempdir()
            .unwrap();
        std::fs::create_dir_all(dir.path().join("src/inner")).unwrap();
        std::fs::write(dir.path().join("src/inner/parser.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        (dir, ctx)
    }

    async fn find(ctx: &ToolContext, args: serde_json::Value) -> ToolOutput {
        FindFilesTool
            .execute(args, ctx.clone(), CancellationToken::new())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_bare_name_glob_matches_at_any_depth() {
        let (_dir, ctx) = workspace();
        let out = find(&ctx, serde_json::json!({"pattern": "parser.rs"})).await;
        assert_eq!(out.content, "src/inner/parser.rs\n");
    }

    #[tokio::test]
    async fn a_slashed_glob_matches_the_relative_path() {
        let (_dir, ctx) = workspace();
        let out = find(&ctx, serde_json::json!({"pattern": "src/*.rs"})).await;
        assert_eq!(out.content, "src/lib.rs\n");
    }

    /// Paths come back relative to the workspace root, so the model can hand
    /// one straight to `read_file` without reconstructing the prefix it
    /// searched under.
    #[tokio::test]
    async fn results_are_relative_to_the_workspace_root_not_the_search_root() {
        let (_dir, ctx) = workspace();
        let out = find(&ctx, serde_json::json!({"pattern": "*.rs", "path": "src"})).await;
        assert!(out.content.contains("src/lib.rs"), "{}", out.content);
        assert!(
            out.content.contains("src/inner/parser.rs"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn an_invalid_glob_names_the_constraint() {
        let (_dir, ctx) = workspace();
        let out = find(&ctx, serde_json::json!({"pattern": "src/["})).await;
        assert!(out.is_error);
        assert!(out.content.contains("not a valid glob"), "{}", out.content);
    }

    /// Old event-log calls carry `max_results`. Replaying one must keep its
    /// bound rather than silently falling back to the default.
    #[tokio::test]
    async fn the_historical_max_results_name_still_binds() {
        let (_dir, ctx) = workspace();
        let out = find(
            &ctx,
            serde_json::json!({"pattern": "**/*", "max_results": 1}),
        )
        .await;
        assert_eq!(out.content.lines().count(), 2, "{}", out.content);
        assert!(
            out.content.contains("more matches not shown"),
            "{}",
            out.content
        );
    }
}
