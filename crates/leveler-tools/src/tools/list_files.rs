//! `list_files` — the `ls` primitive: the direct children of one directory.
//!
//! Directory inspection, not repository search. It never descends and it hides
//! nothing: what is in a directory is a fact about that directory, and
//! `find_files` is the tool for looking across the tree.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use crate::workspace::{DirEntry, SearchError, WorkspaceSearch};

/// Entries returned when the model does not ask for a different bound.
const DEFAULT_LIMIT: usize = 1000;
const HARD_LIMIT: usize = 5000;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Directory to list, relative to the workspace root. Defaults to ".".
    #[serde(default)]
    path: Option<String>,
    /// Maximum entries to return (default 1000, hard cap 5000).
    #[serde(default)]
    limit: Option<usize>,
}

pub struct ListFilesTool;

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &'static str {
        "list_files"
    }

    fn description(&self) -> &'static str {
        "List the direct children of one directory, relative to the workspace \
         root (or any absolute path — reads are not confined to the workspace). \
         Directories are shown with a trailing `/`; dotfiles are included. This \
         does not recurse — use `find_files` to search the tree and `grep` to \
         search file contents."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    /// Pure in-process file read: no write, no subprocess, no language
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
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let rel = input.path.unwrap_or_else(|| ".".to_string());
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, HARD_LIMIT);
        let base = context.execution.workspace.resolve_for_read(&rel)?;

        let listing = match WorkspaceSearch::list_dir(&base, limit) {
            Ok(listing) => listing,
            Err(SearchError::NotDirectory) => {
                return Ok(ToolOutput::error(crate::recoverable::path_not_directory(
                    &rel,
                )));
            }
            Err(SearchError::Io(message)) => {
                return Err(ToolError::Io(format!("list {rel}: {message}")));
            }
            Err(other) => return Err(ToolError::Io(format!("list {rel}: {other:?}"))),
        };

        let mut out = String::new();
        for entry in &listing.entries {
            out.push_str(&DirEntry::display(entry));
            out.push('\n');
        }
        if listing.entries.is_empty() {
            out.push_str("(empty directory)\n");
        }
        if listing.omitted > 0 {
            out.push_str(&format!(
                "… [{} more entries not shown; raise limit to see them]\n",
                listing.omitted
            ));
        }
        Ok(ToolOutput::ok(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::Builder::new()
            .prefix("leveler-ls-")
            .tempdir()
            .unwrap();
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        (dir, ctx)
    }

    #[tokio::test]
    async fn lists_direct_children_without_descending() {
        let (dir, ctx) = workspace();
        std::fs::create_dir_all(dir.path().join("src/inner")).unwrap();
        std::fs::write(dir.path().join("src/inner/deep.rs"), "").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let out = ListFilesTool
            .execute(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.content.contains("Cargo.toml"), "{}", out.content);
        assert!(out.content.contains("src/"), "{}", out.content);
        assert!(!out.content.contains("deep.rs"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_build_directory_is_a_fact_about_the_directory() {
        // `ls` reports what is there. Hiding `target/` from a one-level
        // listing costs the model a true fact and buys nothing: the listing
        // never descends into it anyway.
        let (dir, ctx) = workspace();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        let out = ListFilesTool
            .execute(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.content.contains("target/"), "{}", out.content);
        assert!(!out.content.contains("debug"), "{}", out.content);
    }

    #[tokio::test]
    async fn capped_listing_names_what_it_left_out() {
        let (dir, ctx) = workspace();
        for i in 0..30 {
            std::fs::write(dir.path().join(format!("f{i:05}.txt")), "").unwrap();
        }
        let out = ListFilesTool
            .execute(
                serde_json::json!({"limit": 10}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("20 more entries"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_file_path_is_a_recoverable_error() {
        let (dir, ctx) = workspace();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let out = ListFilesTool
            .execute(
                serde_json::json!({"path": "a.txt"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("not a directory"), "{}", out.content);
    }
}
