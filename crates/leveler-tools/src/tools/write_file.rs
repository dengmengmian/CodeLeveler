//! `write_file` — the `write` primitive: materialize a complete file.
//!
//! Creating a file, and deliberately replacing one end to end, is a different
//! model intent from changing part of one. That is why this is a primitive
//! beside `apply_patch` rather than a convenience over it — and it is the only
//! reason. It goes through the same [`WorkspaceEditor`] as every other
//! mutation, so the write scope, the compare-and-swap, the checkpoint and
//! rollback are identical guarantees.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use crate::workspace::{Commit, WorkspaceEditor};

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Path to the file, relative to the workspace root.
    path: String,
    /// The file's complete new contents.
    content: String,
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Write a file's complete contents, creating it if it does not exist. \
         `content` becomes the whole file — anything already there is replaced. \
         Use this to create a new file or to deliberately rewrite one end to \
         end; use `apply_patch` to change part of an existing file. Overwriting \
         a file that changed since you read it is refused."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::WorkspaceWrite
    }

    fn mutates_files(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;

        if let Some(denied) = context.policy.write_path_denied(&input.path) {
            return Ok(ToolOutput::error(denied));
        }
        let resolved = match context
            .execution
            .workspace
            .resolve_for_write(&input.path, &context.write_scope())
        {
            Ok(resolved) => resolved,
            Err(e) => return Ok(ToolOutput::error(e.to_string())),
        };

        let existing = match tokio::fs::read_to_string(&resolved).await {
            Ok(existing) => Some(existing),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                // A file that exists but is not UTF-8 cannot be compared
                // against, so it cannot be safely overwritten by content.
                return Ok(ToolOutput::error(format!(
                    "cannot read the current contents of {} to replace them: {e}",
                    input.path
                )));
            }
        };

        let commit = match &existing {
            Some(existing) => {
                // The decision to replace the whole file was made against what
                // the model read. If the file moved on since, refuse — exactly
                // as `apply_patch` does.
                if context
                    .execution
                    .file_state
                    .is_stale(&input.path, existing.as_bytes())
                {
                    return Ok(ToolOutput::error(format!(
                        "{} changed since you read it — re-read it and decide again \
                         whether to replace the whole file.",
                        input.path
                    )));
                }
                WorkspaceEditor::replace(&context, &resolved, existing, &input.content).await?
            }
            None => WorkspaceEditor::create(&context, &resolved, &input.content).await?,
        };

        match commit {
            Commit::Written => {}
            Commit::Stale => {
                return Ok(ToolOutput::error(format!(
                    "{} changed on disk while this write was being committed — \
                     re-read it and decide again.",
                    input.path
                )));
            }
            Commit::Rejected(message) => return Ok(ToolOutput::error(message)),
        }

        // Re-fingerprint so this write is not mistaken for an outside change.
        context
            .execution
            .file_state
            .record(&input.path, input.content.as_bytes());

        let hunks = super::applied_diff::whole_file_hunks(
            existing.as_deref().unwrap_or(""),
            &input.content,
        );
        let mut meta = serde_json::json!({ "modified_files": [input.path.clone()] });
        if let Some(diff) = super::applied_diff::unified_diff(&input.path, &hunks) {
            meta["applied_diff"] = serde_json::Value::String(diff);
        }
        let verb = if existing.is_some() {
            "Replaced"
        } else {
            "Created"
        };
        let lines = input.content.lines().count();
        Ok(ToolOutput::ok(format!("{verb} {} ({lines} lines)", input.path)).with_metadata(meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(ctx: ToolContext, args: serde_json::Value) -> ToolOutput {
        WriteFileTool
            .execute(args, ctx, CancellationToken::new())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_created_file_reports_a_diff_numbered_from_one() {
        let (ctx, dir) =
            super::super::test_ctx(leveler_execution::PermissionProfile::Assisted, &[]);
        let out = run(
            ctx,
            serde_json::json!({"path": "src/new.rs", "content": "a\nb\n"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        let diff = out.metadata["applied_diff"].as_str().expect("applied diff");
        assert!(diff.contains("+a") && diff.contains("+b"), "{diff}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn replacing_a_file_reports_both_sides() {
        let (ctx, dir) = super::super::test_ctx(
            leveler_execution::PermissionProfile::Assisted,
            &[("a.txt", "before\n")],
        );
        let out = run(
            ctx,
            serde_json::json!({"path": "a.txt", "content": "after\n"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        let diff = out.metadata["applied_diff"].as_str().expect("applied diff");
        assert!(diff.contains("-before"), "{diff}");
        assert!(diff.contains("+after"), "{diff}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_empty_content_is_a_real_empty_file_not_a_refusal() {
        let (ctx, dir) = super::super::test_ctx(
            leveler_execution::PermissionProfile::Assisted,
            &[("a.txt", "before\n")],
        );
        let out = run(ctx, serde_json::json!({"path": "a.txt", "content": ""})).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "");
        std::fs::remove_dir_all(&dir).ok();
    }
}
