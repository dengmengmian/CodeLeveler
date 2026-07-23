//! `create_checkpoint` / `restore_checkpoint` (spec §18.3): explicit restore
//! points on top of the automatic before-first-write checkpoint (spec §28).

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize, JsonSchema)]
struct Empty {}

/// Marks the current working tree as the restore baseline.
pub struct CreateCheckpointTool;

#[async_trait]
impl Tool for CreateCheckpointTool {
    fn name(&self) -> &'static str {
        "create_checkpoint"
    }
    fn description(&self) -> &'static str {
        "Set the current state of the workspace as a restore point. A later \
         `restore_checkpoint` reverts every file changed after this call."
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
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        context.checkpoint.reset();
        Ok(ToolOutput::ok(
            "Checkpoint set. `restore_checkpoint` will revert changes made from here.\n",
        ))
    }
}

/// Reverts all files changed since the checkpoint to their captured content.
pub struct RestoreCheckpointTool;

#[async_trait]
impl Tool for RestoreCheckpointTool {
    fn name(&self) -> &'static str {
        "restore_checkpoint"
    }
    fn description(&self) -> &'static str {
        "Undo edits: revert every file changed since the last checkpoint (or the \
         start of the run) back to its captured content."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Empty>()
    }
    fn risk(&self) -> RiskLevel {
        // Reverting is a workspace write (of the captured content).
        RiskLevel::WorkspaceWrite
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let count = context.checkpoint.touched_count();
        if count == 0 {
            return Ok(ToolOutput::ok(
                "Nothing to restore (no changes recorded).\n",
            ));
        }
        match context.checkpoint.restore() {
            Ok(()) => Ok(ToolOutput::ok(format!(
                "Restored {count} file(s) to the checkpoint.\n"
            ))),
            Err(e) => Err(ToolError::Io(format!("restore failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restore_reverts_edits_after_checkpoint() {
        let dir =
            std::env::temp_dir().join(format!("leveler-ckpttool-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "original").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);

        // Set a checkpoint at "original", then simulate an edit recorded via the
        // shared checkpoint, then restore.
        CreateCheckpointTool
            .execute(serde_json::json!({}), ctx.clone(), CancellationToken::new())
            .await
            .unwrap();
        ctx.checkpoint.record(&dir.join("a.txt")).unwrap();
        std::fs::write(dir.join("a.txt"), "edited").unwrap();

        let out = RestoreCheckpointTool
            .execute(serde_json::json!({}), ctx.clone(), CancellationToken::new())
            .await
            .unwrap();
        assert!(out.content.contains("Restored 1"));
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "original"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
