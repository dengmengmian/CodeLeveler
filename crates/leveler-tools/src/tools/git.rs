//! `git_status` and `git_diff` — read-only git inspection (spec §18.3).
//!
//! Thin adapters. `leveler-vcs` owns how this product invokes `git` — the
//! request, the timeout, the runner — and these two tools own only the model
//! interface: which arguments an intent maps to, and how the result reads.
//! They used to build their own `ProcessRequest`, which was a second git
//! invocation alongside `GitWorkflow`'s with its own timeout and error shape.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_vcs::GitWorkflow;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// The VCS capability for this call: the workspace it inspects and the shared,
/// sandboxed runner every other tool call goes through.
fn vcs(context: &ToolContext) -> GitWorkflow {
    GitWorkflow::with_runner(
        context.execution.workspace.root(),
        context.execution.runner.clone(),
        context.execution.environment.clone(),
    )
}

async fn inspect(
    context: &ToolContext,
    args: &[&str],
    cancellation: CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let output = match vcs(context).inspect(args, cancellation).await {
        Ok(output) => output,
        Err(error) => return Ok(ToolOutput::error(error.to_string())),
    };
    if !output.success() {
        return Ok(ToolOutput::error(format!(
            "git failed (exit {:?}):\n{}",
            output.exit_code, output.stderr
        )));
    }
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
        inspect(
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
        // Refuse credential paths before handing the path to git.
        let path_owned;
        if let Some(p) = &input.path {
            context.execution.workspace.resolve_for_read(p)?;
            args.push("--");
            path_owned = p.clone();
            args.push(&path_owned);
        }
        inspect(&context, &args, cancellation).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_reports_untracked() {
        let repo = leveler_test_support::git::scratch_repo();
        std::fs::write(repo.path().join("new.txt"), "x").unwrap();
        let ctx = super::super::test_ctx_in(
            repo.path(),
            leveler_execution::PermissionProfile::RequestApproval,
        );
        let out = GitStatusTool
            .execute(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.content.contains("new.txt"), "got: {}", out.content);
    }

    /// The tool builds no process of its own: the VCS capability is what runs
    /// `git`, so the two callers cannot drift on timeout or error shape.
    #[tokio::test]
    async fn a_diff_is_answered_through_the_vcs_capability() {
        let repo = leveler_test_support::git::scratch_repo();
        std::fs::write(repo.path().join("tracked.txt"), "before\n").unwrap();
        leveler_test_support::git::run(repo.path(), &["add", "-A"]);
        leveler_test_support::git::run(repo.path(), &["commit", "-qm", "init"]);
        std::fs::write(repo.path().join("tracked.txt"), "after\n").unwrap();
        let ctx = super::super::test_ctx_in(
            repo.path(),
            leveler_execution::PermissionProfile::RequestApproval,
        );
        let out = GitDiffTool
            .execute(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("-before"), "got: {}", out.content);
        assert!(out.content.contains("+after"), "got: {}", out.content);
    }
}
