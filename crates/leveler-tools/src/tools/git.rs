//! `git_status` and `git_diff` — read-only git inspection (spec §18.3).

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::{ProcessRequest, RiskLevel};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

async fn run_git(
    context: &ToolContext,
    args: &[&str],
    cancellation: CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let mut request = ProcessRequest::new(
        "git",
        args.iter().map(|s| s.to_string()).collect(),
        context.workspace.root().to_path_buf(),
    );
    request.timeout = Duration::from_secs(30);
    let output = context.runner.run(request, cancellation).await?;
    if output.success() {
        let mut body = if output.stdout.trim().is_empty() {
            "(clean)\n".to_string()
        } else {
            output.stdout
        };
        // The runner caps output; a silently-cut diff reads as complete. Say so.
        if output.truncated {
            body.push_str(
                "\n[note] output was truncated (too large); narrow with a `path`, \
                 or read specific files, for the full diff.\n",
            );
        }
        Ok(ToolOutput::ok(body))
    } else {
        Ok(ToolOutput::error(format!(
            "git failed (exit {:?}):\n{}",
            output.exit_code, output.stderr
        )))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Empty {}

pub struct GitStatusTool;

#[async_trait]
impl Tool for GitStatusTool {
    fn name(&self) -> &'static str {
        "git_status"
    }
    fn description(&self) -> &'static str {
        "Show the working tree status (porcelain format)."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Empty>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        run_git(
            &context,
            &["status", "--porcelain=v1", "--branch"],
            cancellation,
        )
        .await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DiffInput {
    /// Restrict the diff to this path (relative to the workspace).
    #[serde(default)]
    path: Option<String>,
    /// Diff staged changes instead of the working tree.
    #[serde(default)]
    staged: bool,
}

pub struct GitDiffTool;

#[async_trait]
impl Tool for GitDiffTool {
    fn name(&self) -> &'static str {
        "git_diff"
    }
    fn description(&self) -> &'static str {
        "Show a unified diff of changes in the working tree (or staged)."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<DiffInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: DiffInput = super::parse_input(self.name(), input)?;
        let mut args: Vec<&str> = vec!["diff"];
        if input.staged {
            args.push("--staged");
        }
        // Validate the path stays in the workspace before passing it to git.
        let path_owned;
        if let Some(p) = &input.path {
            context.workspace.resolve(p)?;
            args.push("--");
            path_owned = p.clone();
            args.push(&path_owned);
        }
        run_git(&context, &args, cancellation).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn git_repo() -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("leveler-git-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .output()
                .unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn status_reports_untracked() {
        let dir = git_repo().await;
        std::fs::write(dir.join("new.txt"), "x").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = GitStatusTool
            .execute(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.content.contains("new.txt"), "got: {}", out.content);
        std::fs::remove_dir_all(&dir).ok();
    }
}
