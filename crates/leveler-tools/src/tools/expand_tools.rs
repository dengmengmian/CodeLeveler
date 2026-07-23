//! `expand_tools` — grow the economy Core tool surface mid-session.
//!
//! The tool records the requested categories in metadata so the app/executor
//! can register matching tools on the live registry. Without a host side-effect
//! handler this still documents the request for the model.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const KNOWN: &[&str] = &[
    "search",
    "lsp",
    "git",
    "web",
    "mcp",
    "subagent",
    "skills",
    "checkpoint",
    "media",
];

#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// Categories to unlock (search, lsp, git, web, skills, checkpoint, media, …).
    categories: Vec<String>,
}

pub struct ExpandToolsTool;

#[async_trait]
impl Tool for ExpandToolsTool {
    fn name(&self) -> &'static str {
        "expand_tools"
    }

    fn description(&self) -> &'static str {
        "Request additional tool categories beyond the core set (search, lsp, \
         git, web, skills, checkpoint, media). Use when you need a capability \
         not currently available. Categories you already have are ignored."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Args>()
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
        if args.categories.is_empty() {
            return Ok(ToolOutput::error(
                "categories must list at least one category",
            ));
        }
        let mut accepted = Vec::new();
        let mut unknown = Vec::new();
        for cat in &args.categories {
            let key = cat.trim().to_ascii_lowercase();
            if KNOWN.contains(&key.as_str()) {
                // Dedup: a repeated category must not appear twice in the result
                // or the registered-categories metadata.
                if !accepted.contains(&key) {
                    accepted.push(key);
                }
            } else {
                unknown.push(cat.clone());
            }
        }
        if accepted.is_empty() {
            return Ok(ToolOutput::error(format!(
                "no known categories in request; known: {}",
                KNOWN.join(", ")
            )));
        }
        let mut body = format!(
            "Requested tool categories: {}.\nHost will register matching tools \
             for subsequent rounds when available.",
            accepted.join(", ")
        );
        if !unknown.is_empty() {
            body.push_str(&format!("\nUnknown (ignored): {}.", unknown.join(", ")));
        }
        let meta = serde_json::json!({ "expand_categories": accepted });
        Ok(ToolOutput::ok(body).with_metadata(meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval)
    }

    #[tokio::test]
    async fn accepts_search_category() {
        let out = ExpandToolsTool
            .execute(
                serde_json::json!({ "categories": ["search"] }),
                ctx(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            out.metadata.get("expand_categories").unwrap(),
            &serde_json::json!(["search"])
        );
    }

    #[tokio::test]
    async fn repeated_categories_are_deduplicated() {
        let out = ExpandToolsTool
            .execute(
                serde_json::json!({ "categories": ["search", "search", "lsp"] }),
                ctx(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            out.metadata.get("expand_categories").unwrap(),
            &serde_json::json!(["search", "lsp"]),
            "duplicate categories must collapse: {}",
            out.content
        );
    }
}
