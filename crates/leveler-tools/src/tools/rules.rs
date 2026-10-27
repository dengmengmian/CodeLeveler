//! `read_project_rules` — read one project-rule section verbatim.
//!
//! Project rules are delivered deterministically: the host names the mandatory
//! sections (`.leveler/config.yaml`) and the runtime puts them in the prompt
//! before anything optional. This tool is therefore an OPTIONAL query path —
//! nothing about correctness depends on the model choosing to call it.
//!
//! It is a thin, read-only lens over the SAME loader the prompt uses
//! ([`leveler_context::load_rules`] and [`leveler_context::read_rule_source`]),
//! so a section read here is byte-identical to a delivered one, and a nested
//! `src/AGENTS.md` is readable here for the same reason the scoped loader can
//! deliver it. It decides nothing about authority, precedence or relevance.

use std::path::Path;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_context::{READ_PROJECT_RULES_TOOL, load_rules, read_rule_source, split_rule_sections};
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
         every root source and its section ids. `source` only: that document's \
         section ids (a nested `src/AGENTS.md` is read on demand). `source` and \
         `section`: that section's text, unchanged. A missing source or section \
         is an error. The text is the author's, including any instructions \
         inside it."
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
        let root: &Path = context.require_workspace()?.root();
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

        // One loader: a root source comes from the same `load_rules` the prompt
        // uses, and a nested one (`src/AGENTS.md`) is read through the same
        // `read_rule_source` the scoped loader uses. The two can never return
        // different bytes for the same file.
        let instruction = rules
            .iter()
            .find(|rule| rule.source == source)
            .cloned()
            .or_else(|| read_rule_source(root, source));
        let Some(instruction) = instruction else {
            return Ok(ToolOutput::error(format!(
                "`{source}` is not a project-rule document. Rule documents are \
                 `AGENTS.md` in any directory, `.leveler/instructions.md` and \
                 `.leveler/rules/*.md`; call with no arguments to list the root ones."
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
        // An absent nested document, and a file that is not a rule document at
        // all: both refused, and a tool must not launder arbitrary content
        // into project authority.
        std::fs::write(dir.path().join("src.rs"), "# Alpha\na\n").unwrap();
        for source in ["src/AGENTS.md", "src.rs", "../AGENTS.md"] {
            let out = ReadProjectRulesTool
                .execute(
                    serde_json::json!({"source": source, "section": "alpha"}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(out.is_error, "{source} must not be readable as a rule");
        }
    }

    /// The reported failure (DF-04): the tool refused the very nested document
    /// the scoped loader had just delivered, because it used a different
    /// loader. One source of bytes now.
    #[tokio::test]
    async fn a_nested_agents_md_is_readable_with_the_same_bytes_the_injector_uses() {
        let (dir, ctx) = workspace();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/AGENTS.md"),
            "# Source header\nEvery `.rs` file begins with `// v2-header`.\n",
        )
        .unwrap();
        let listing = ReadProjectRulesTool
            .execute(
                serde_json::json!({"source": "src/AGENTS.md"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!listing.is_error, "{}", listing.content);
        assert!(
            listing.content.contains("[source-header]"),
            "{}",
            listing.content
        );

        let section = ReadProjectRulesTool
            .execute(
                serde_json::json!({"source": "src/AGENTS.md", "section": "source-header"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!section.is_error);
        let loaded = leveler_context::scoped_rule_sources(dir.path(), &["src/lib.rs".to_string()]);
        assert_eq!(loaded.len(), 1);
        assert!(
            section.content.contains(loaded[0].content.trim()),
            "the tool and the injector must return the same bytes"
        );
    }
}
