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
        match leveler_skills::load(context.workspace.root(), &input.name) {
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

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateInput {
    /// Skill name: letters, digits, '-' or '_'. Becomes the folder name.
    name: String,
    /// One line describing what the skill does and WHEN to use it — this is the
    /// only text loaded into context to decide relevance, so be specific.
    description: String,
    /// The skill body (Markdown): the procedure/instructions to follow.
    body: String,
}

pub struct CreateSkillTool;

#[async_trait]
impl Tool for CreateSkillTool {
    fn name(&self) -> &'static str {
        "create_skill"
    }
    fn description(&self) -> &'static str {
        "Author a reusable skill, saved to .leveler/skills/<name>/SKILL.md. Use \
         this to capture a multi-step procedure or domain knowledge worth reusing \
         later. Keep the description precise about WHEN the skill applies."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<CreateInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::WorkspaceWrite
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: CreateInput = super::parse_input(self.name(), input)?;
        match leveler_skills::create(
            context.workspace.root(),
            &input.name,
            &input.description,
            &input.body,
        ) {
            Ok(dir) => Ok(ToolOutput::ok(format!(
                "Created skill `{}` at {}/SKILL.md. It will appear in the skills index.\n",
                input.name,
                dir.display()
            ))),
            Err(e) => Ok(ToolOutput::error(format!("could not create skill: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_then_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-skilltool-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);

        let created = CreateSkillTool
            .execute(
                serde_json::json!({
                    "name": "release",
                    "description": "Cut a release. Use when asked to publish a new version.",
                    "body": "# Release\n\n1. Bump version\n2. Tag\n3. Push",
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!created.is_error, "{}", created.content);

        let loaded = LoadSkillTool
            .execute(
                serde_json::json!({ "name": "release" }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            loaded.content.contains("Bump version"),
            "{}",
            loaded.content
        );
        assert!(loaded.content.contains("Skill: release"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn load_skill_surfaces_structured_scripts_and_dir() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-skillstruct-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        CreateSkillTool
            .execute(
                serde_json::json!({
                    "name": "pack",
                    "description": "Pack things",
                    "body": "UNIQUE_PACK_BODY_99",
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        // Join per component so the expected path uses native separators,
        // matching how the tool renders it (`project_skills_dir` joins segment
        // by segment). A `".leveler/skills/pack"` literal keeps forward slashes
        // on Windows and would not match the tool's backslash rendering.
        let skill_dir = dir.join(".leveler").join("skills").join("pack");
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
        // Windows canonicalization renders verbatim paths with a `\\?\`
        // extended-length prefix that the raw `temp_dir()`-based expectation
        // lacks; strip it so the same absolute path is compared on every
        // platform (no-op on Unix, where the prefix never appears).
        let rendered = loaded.content.replace(r"\\?\", "");
        assert!(
            rendered.contains(skill_dir.to_string_lossy().as_ref()),
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
