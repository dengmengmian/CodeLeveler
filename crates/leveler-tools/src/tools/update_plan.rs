//! `update_plan` — a lightweight TODO/checklist the model maintains across a
//! long task. No side effects: it only
//! records the plan and echoes it back so the model stays oriented and the user
//! can see progress. Keeps weaker models from drifting on multi-step work.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum StepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct PlanItem {
    /// The step text.
    step: String,
    /// One of `pending`, `in_progress`, `completed`.
    status: StepStatus,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// Optional note about this plan update.
    #[serde(default)]
    explanation: Option<String>,
    /// The ordered plan items. At most one may be `in_progress`.
    plan: Vec<PlanItem>,
}

pub struct UpdatePlanTool;

#[async_trait]
impl Tool for UpdatePlanTool {
    fn name(&self) -> &'static str {
        "update_plan"
    }

    fn description(&self) -> &'static str {
        "Record or update your task plan as a checklist. Provide an optional \
         explanation and a list of plan items, each with a `step` and a `status` \
         (pending | in_progress | completed). At most one step may be in_progress \
         at a time. Use this on multi-step tasks to track progress; it has no side \
         effects on the workspace."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Args>()
    }

    fn normalize_input(&self, input: serde_json::Value) -> serde_json::Value {
        normalize_nested_envelope(input)
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let args: Args = super::parse_input(self.name(), input)?;

        let in_progress = args
            .plan
            .iter()
            .filter(|p| p.status == StepStatus::InProgress)
            .count();
        if in_progress > 1 {
            return Ok(ToolOutput::error(format!(
                "at most one step may be in_progress, but {in_progress} are; mark \
                 the others pending or completed"
            )));
        }
        if args.plan.is_empty() {
            return Ok(ToolOutput::error("plan must have at least one step"));
        }

        let mut body = String::new();
        if let Some(note) = args.explanation.as_deref().filter(|s| !s.trim().is_empty()) {
            body.push_str(note.trim());
            body.push_str("\n\n");
        }
        for item in &args.plan {
            let mark = match item.status {
                StepStatus::Pending => "[ ]",
                StepStatus::InProgress => "[~]",
                StepStatus::Completed => "[x]",
            };
            body.push_str(&format!("{mark} {}\n", item.step));
        }

        // Carry the structured plan in metadata so the UI can render it natively.
        let meta = serde_json::json!({ "plan": args.plan });
        Ok(ToolOutput::ok(body).with_metadata(meta))
    }
}

/// Some models occasionally place the complete argument object inside the
/// first `plan` element: `{ "plan": [{ "explanation": ..., "plan": [...] }] }`.
/// Unwrap only that exact single-layer envelope. The registry validates the
/// returned canonical shape normally, so this does not relax plan-item rules.
fn normalize_nested_envelope(input: serde_json::Value) -> serde_json::Value {
    let Some(outer) = input.as_object() else {
        return input;
    };
    if !outer
        .keys()
        .all(|key| matches!(key.as_str(), "explanation" | "plan"))
    {
        return input;
    }
    let Some([nested]) = outer
        .get("plan")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
    else {
        return input;
    };
    let Some(nested) = nested.as_object() else {
        return input;
    };
    if !nested
        .keys()
        .all(|key| matches!(key.as_str(), "explanation" | "plan"))
        || !nested.get("plan").is_some_and(serde_json::Value::is_array)
    {
        return input;
    }

    let mut normalized = nested.clone();
    if !normalized.contains_key("explanation")
        && let Some(explanation) = outer.get("explanation")
    {
        normalized.insert("explanation".to_string(), explanation.clone());
    }
    serde_json::Value::Object(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval)
    }

    #[tokio::test]
    async fn renders_a_checklist() {
        let out = UpdatePlanTool
            .execute(
                serde_json::json!({
                    "explanation": "starting",
                    "plan": [
                        {"step": "read code", "status": "completed"},
                        {"step": "fix bug", "status": "in_progress"},
                        {"step": "run tests", "status": "pending"}
                    ]
                }),
                ctx(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("[x] read code"));
        assert!(out.content.contains("[~] fix bug"));
        assert!(out.content.contains("[ ] run tests"));
    }

    #[tokio::test]
    async fn rejects_two_in_progress() {
        let out = UpdatePlanTool
            .execute(
                serde_json::json!({
                    "plan": [
                        {"step": "a", "status": "in_progress"},
                        {"step": "b", "status": "in_progress"}
                    ]
                }),
                ctx(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("in_progress"));
    }
}
