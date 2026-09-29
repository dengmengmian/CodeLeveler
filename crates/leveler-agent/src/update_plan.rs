//! `update_plan` — a lightweight TODO/checklist the model maintains across a
//! long task. No side effects: it only records the plan and echoes it back, so
//! the plan is durable, visible to the user, and part of the run's evidence
//! rather than only of the transcript.
//!
//! A HARNESS CONTROL, not a capability: it touches nothing outside the harness
//! that renders and persists the plan, which is why it lives here with
//! `update_goal`, `request_user_input`, `request_permissions`, `spawn_agent`,
//! `claim_write_scope` and `report_finding` rather than in the tool crate.
//!
//! It is still a `Tool`, registered by [`register_harness_controls`], so it
//! reuses the registry's ONE mechanical seam — argument normalization, JSON
//! Schema validation, the derived schema, dispatch, the result cap. The other
//! controls are answered inside the loop from an injected `ToolDefinition`;
//! this one has a real result to render, and duplicating the validation
//! machinery to inject it would be the worse trade.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use leveler_tools::tools::{parse_input, schema_of};
use leveler_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum StepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct PlanItem {
    /// Step text.
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

/// Register the harness controls that are dispatched as tools.
///
/// Called by the composition root AFTER [`leveler_tools::model_surface`], so a
/// control is never mistaken for a capability the host could turn off.
pub fn register_harness_controls(registry: &mut ToolRegistry) {
    registry.register(std::sync::Arc::new(UpdatePlanTool));
    // Agent authoring: the top-level agent's only; children never hold them.
    registry.register(std::sync::Arc::new(crate::agent_registry::ListAgentsTool));
    registry.register(std::sync::Arc::new(crate::agent_registry::SaveAgentTool));
    registry.register(std::sync::Arc::new(crate::agent_registry::DeleteAgentTool));
    // Skill authoring: like agents, only the top-level agent holds these, and
    // only a person confirms a write.
    registry.register(std::sync::Arc::new(leveler_tools::tools::SaveSkillTool));
    registry.register(std::sync::Arc::new(leveler_tools::tools::DeleteSkillTool));
}

pub struct UpdatePlanTool;

#[async_trait]
impl Tool for UpdatePlanTool {
    fn name(&self) -> &'static str {
        "update_plan"
    }

    fn description(&self) -> &'static str {
        "Replace the task checklist with the list in this call. No workspace \
         side effect: the stored plan is exactly the items sent, and nothing \
         else advances a status.\n\n\
         Arguments: optional `explanation` (a note stored with this update) and \
         `plan` (the ordered items). Each item has `step` (text) and `status` \
         (`pending`, `in_progress`, or `completed`). The list must contain at \
         least one item. At most one item may be `in_progress`; more than one \
         is an error and the previous plan is left unchanged. Each call \
         replaces the whole list.\n\n\
         `pending` is not done. `in_progress` is the current item. `completed` \
         is done. The tool does not check those claims."
    }

    fn input_schema(&self) -> serde_json::Value {
        schema_of::<Args>()
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
        let args: Args = parse_input(self.name(), input)?;

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

    /// A context over a scratch workspace. `update_plan` reads and writes no
    /// file, so the workspace only has to have EXISTED: the directory is
    /// removed as this returns, and nothing in the tool notices.
    fn ctx() -> ToolContext {
        let dir = tempfile::tempdir().unwrap();
        let workspace = leveler_execution::Workspace::new(dir.path()).unwrap();
        ToolContext::new(
            workspace,
            leveler_execution::PermissionProfile::RequestApproval,
        )
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

    /// The description states what the call records. It does not say when to
    /// call the tool.
    #[test]
    fn the_description_states_the_record_and_not_when_to_call() {
        let d = UpdatePlanTool.description();
        assert!(d.contains("replaces the whole list"), "{d}");
        assert!(d.contains("pending"), "{d}");
        assert!(d.contains("in_progress"), "{d}");
        assert!(d.contains("completed"), "{d}");
        assert!(
            d.contains("at most one") || d.contains("At most one"),
            "{d}"
        );
        for coaching in [
            "Call this again",
            "Do NOT call this between",
            "After the last real tool",
            "readable density",
            "too thin to navigate",
        ] {
            assert!(!d.contains(coaching), "{coaching} still coaches: {d}");
        }
    }

    /// The declared schema is what the provider validates. `update_plan` is the
    /// shape Moonshot refuses: a reference (`plan.items` → `PlanItem`) with a
    /// reference and a `oneOf`-free enum behind it that `required` forces it to
    /// follow, which it answers with `detected infinite recursion without
    /// termination condition` — refusing the request, not just this tool. The
    /// declared schema therefore carries no reference at all.
    #[test]
    fn the_declared_schema_carries_no_reference() {
        let schema = UpdatePlanTool.input_schema();
        let text = serde_json::to_string(&schema).unwrap();

        assert!(!text.contains("$ref"), "{text}");
        assert!(schema.get("$defs").is_none(), "{text}");
        let status = &schema["properties"]["plan"]["items"]["properties"]["status"];
        assert!(
            serde_json::to_string(status)
                .unwrap()
                .contains(r#"["pending","in_progress","completed"]"#),
            "{text}"
        );
    }

    /// Completion is an outcome, not a movement: "mark the finished step
    /// completed and the next one in_progress, at the transition" tied the two
    /// together.
    #[test]
    fn the_description_defines_the_plan_states_mechanically() {
        let d = UpdatePlanTool.description();
        for needle in [
            "Replace the task checklist with the list in this call",
            "No workspace side effect",
            "Each call replaces the whole list",
            "`pending` is not done",
            "The tool does not check those claims",
        ] {
            assert!(d.contains(needle), "lost `{needle}`: {d}");
        }
        // The de-coached description states the protocol and does not tell the
        // model when a step may be called completed.
        assert!(!d.contains("the finished step completed"), "{d}");
        assert!(!d.contains("already read"), "{d}");
        assert!(
            !d.contains("rewrite the step instead of completing it"),
            "{d}"
        );
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

/// The control still goes through the registry's ONE mechanical seam:
/// normalization, then schema validation, then dispatch. These used to live
/// in the tool crate's registry tests; they moved with the tool, and they
/// prove the seam is reused rather than reimplemented here.
#[cfg(test)]
mod registry_dispatch_tests {
    use super::*;

    /// Same scratch workspace as the tool tests above.
    fn ctx() -> ToolContext {
        let dir = tempfile::tempdir().unwrap();
        let workspace = leveler_execution::Workspace::new(dir.path()).unwrap();
        ToolContext::new(
            workspace,
            leveler_execution::PermissionProfile::RequestApproval,
        )
    }

    #[tokio::test]
    async fn update_plan_accepts_one_accidentally_nested_argument_envelope() {
        let mut reg = ToolRegistry::new();
        register_harness_controls(&mut reg);
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
                ctx(),
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
        let mut reg = ToolRegistry::new();
        register_harness_controls(&mut reg);
        let err = reg
            .execute(
                "update_plan",
                serde_json::json!({
                    "plan": [{
                        "plan": [{"step": "定位根因", "status": "done"}]
                    }]
                }),
                ctx(),
                CancellationToken::new(),
            )
            .await
            .expect_err("normalization must not permit a non-canonical status");

        assert!(matches!(err, ToolError::InvalidArguments { .. }));
    }
}
