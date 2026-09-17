//! `load_skill` / `create_skill` — progressive-disclosure Agent Skills.
//! The skills index (name + description) is injected into context; the model
//! calls `load_skill` to read a skill's full instructions before related work,
//! and `create_skill` to capture a reusable procedure it just worked out.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize, JsonSchema)]
struct LoadInput {
    /// The skill name (as listed in the skills index).
    name: String,
}

pub struct LoadSkillTool;

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &'static str {
        "load_skill"
    }
    fn description(&self) -> &'static str {
        "Read a skill's full instructions by name (from the injected skills \
         index). Do this before starting work the skill covers."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<LoadInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: LoadInput = super::parse_input(self.name(), input)?;
        match leveler_skills::load(context.execution.workspace.root(), &input.name) {
            Some(detail) => Ok(ToolOutput::ok(leveler_skills::render_skill_package(
                &detail,
            ))),
            None => Ok(ToolOutput::error(format!(
                "no skill named `{}` (check the skills index)",
                input.name
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_skill_surfaces_structured_scripts_and_dir() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-skillstruct-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        // Authoring a skill is user administration, not an agent tool: write
        // it through the store owner, exactly as the CLI does.
        leveler_skills::create(
            ctx.execution.workspace.root(),
            "pack",
            "Pack things",
            "UNIQUE_PACK_BODY_99",
        )
        .unwrap();
        // Derive the expected dir exactly as the tool does: `Workspace::new`
        // canonicalizes the root, and the tool renders the skill dir under that
        // canonical root. Mirroring `canonicalize` (per-component join) matches
        // the tool on every platform — native separators, `/private/var/…` on
        // macOS, and the `\\?\C:\…` verbatim prefix on Windows.
        let root = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        let skill_dir = root.join(".leveler").join("skills").join("pack");
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(skill_dir.join("scripts/run.sh"), "echo run\n").unwrap();
        std::fs::write(skill_dir.join("references/a.md"), "ref\n").unwrap();

        let loaded = LoadSkillTool
            .execute(
                serde_json::json!({ "name": "pack" }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!loaded.is_error, "{}", loaded.content);
        assert!(loaded.content.contains("UNIQUE_PACK_BODY_99"));
        assert!(loaded.content.contains("## Scripts"));
        assert!(loaded.content.contains("scripts/run.sh"));
        assert!(loaded.content.contains("## References"));
        assert!(loaded.content.contains("references/a.md"));
        assert!(
            loaded
                .content
                .contains(skill_dir.to_string_lossy().as_ref()),
            "must include absolute skill dir, not only project-relative prefix: {}",
            loaded.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn load_missing_skill_is_error() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-skillmiss-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = LoadSkillTool
            .execute(
                serde_json::json!({ "name": "nope" }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
