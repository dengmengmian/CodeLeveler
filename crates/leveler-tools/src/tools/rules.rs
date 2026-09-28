//! `read_project_rules` — read one project-rule section verbatim.
//!
//! Project rules (root `AGENTS.md`, `.leveler/instructions.md`,
//! `.leveler/rules/*.md`, nested `AGENTS.md`) are delivered to the model a
//! whole section at a time, up to a per-document budget. When a document does
//! not fit, `read_project_rules` lists and returns the sections that were left out.
//!
//! This tool is a thin, read-only lens over the SAME deterministic loader the
//! prompt uses ([`leveler_context::load_rules`]): it decides nothing about
//! authority, precedence or relevance. It returns the author's exact bytes
//! with their source, so a retrieved section is as authoritative as a
//! delivered one. With no `section`, it returns the full index of what exists.

use std::path::Path;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_context::{READ_PROJECT_RULES_TOOL, load_rules, split_rule_sections};
use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadRulesInput {
    /// A rules source path exactly as shown in the prompt, e.g. `AGENTS.md`
    /// or `src/AGENTS.md`. Omit to list every source and section.
    #[serde(default)]
    source: Option<String>,
    /// A section id exactly as listed in the prompt or in the index, e.g.
    /// `architecture-gates`. Omit to list the sections of `source`.
    #[serde(default)]
    section: Option<String>,
}

pub struct ReadProjectRulesTool;

#[async_trait]
impl Tool for ReadProjectRulesTool {
    fn name(&self) -> &'static str {
        READ_PROJECT_RULES_TOOL
    }

    fn description(&self) -> &'static str {
        "Read a project-rule section verbatim, or list what exists. No arguments: \
         every source and its section ids. `source` only: that document's \
         section ids. `source` and `section`: that section's text, unchanged. \
         A missing source or section is an error. The text is the author's, \
         including any instructions inside it."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<ReadRulesInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
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
        let input: ReadRulesInput = super::parse_input(self.name(), input)?;
        let root: &Path = context.execution.workspace.root();
        let rules = load_rules(root);

        let Some(source) = input.source.as_deref() else {
            if input.section.is_some() {
                return Ok(ToolOutput::error(
                    "`section` needs a `source`; call with no arguments to list sources.",
                ));
            }
            if rules.is_empty() {
                return Ok(ToolOutput::ok("No project rules are configured."));
            }
            let mut listing = String::from("Project rules available:\n");
            for instruction in &rules {
                listing.push_str(&format!("\n=== {} ===\n", instruction.source));
                for section in split_rule_sections(&instruction.content) {
                    listing.push_str(&format!(
                        "  [{}] {} ({} bytes)\n",
                        section.id, section.heading, section.bytes
                    ));
                }
            }
            return Ok(ToolOutput::ok(listing));
        };

        let Some(instruction) = rules.iter().find(|rule| rule.source == source) else {
            return Ok(ToolOutput::error(format!(
                "`{source}` is not a loaded project-rule source; call with no arguments to list them."
            )));
        };
        let sections = split_rule_sections(&instruction.content);
        let Some(section_id) = input.section.as_deref() else {
            let mut listing = format!("=== {} ===\n", instruction.source);
            for section in &sections {
                listing.push_str(&format!(
                    "  [{}] {} ({} bytes)\n",
                    section.id, section.heading, section.bytes
                ));
            }
            return Ok(ToolOutput::ok(listing));
        };
        let Some(section) = sections.iter().find(|section| section.id == section_id) else {
            return Ok(ToolOutput::error(format!(
                "`{source}` has no section `{section_id}`"
            )));
        };
        Ok(ToolOutput::ok(format!(
            "--- from {} (section: {}) ---\n{}",
            instruction.source, section.id, section.content
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::Builder::new()
            .prefix("leveler-rules-")
            .tempdir()
            .unwrap();
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        (dir, ctx)
    }

    #[tokio::test]
    async fn lists_sources_and_sections_when_called_bare() {
        let (dir, ctx) = workspace();
        std::fs::write(dir.path().join("AGENTS.md"), "# Alpha\nA\n# Beta\nB\n").unwrap();
        let out = ReadProjectRulesTool
            .execute(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.content.contains("AGENTS.md"), "{}", out.content);
        assert!(out.content.contains("[alpha]"), "{}", out.content);
        assert!(out.content.contains("[beta]"), "{}", out.content);
    }

    #[tokio::test]
    async fn returns_one_section_verbatim_with_its_source() {
        let (dir, ctx) = workspace();
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "# Alpha\nalpha body\n# Beta\nbeta body\n",
        )
        .unwrap();
        let out = ReadProjectRulesTool
            .execute(
                serde_json::json!({"source": "AGENTS.md", "section": "beta"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.content
                .contains("--- from AGENTS.md (section: beta) ---")
        );
        assert!(out.content.contains("beta body"));
        // Verbatim: the other section is not smuggled in.
        assert!(!out.content.contains("alpha body"));
    }

    #[tokio::test]
    async fn an_unknown_section_or_source_is_a_readable_refusal() {
        let (dir, ctx) = workspace();
        std::fs::write(dir.path().join("AGENTS.md"), "# Alpha\na\n").unwrap();
        let missing_section = ReadProjectRulesTool
            .execute(
                serde_json::json!({"source": "AGENTS.md", "section": "nope"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(missing_section.is_error);
        let missing_source = ReadProjectRulesTool
            .execute(
                serde_json::json!({"source": "src/AGENTS.md", "section": "alpha"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(missing_source.is_error);
    }
}
