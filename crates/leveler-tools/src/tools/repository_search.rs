//! `repository_search` — find files by path/name (spec §18.3). Complements
//! `grep` (which searches content): this locates files whose path matches a
//! query, using tracked files (`git ls-files`) when available.

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::{ProcessRequest, RiskLevel};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const DEFAULT_MAX: usize = 100;
const IGNORED: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "dist",
    "vendor",
    ".leveler",
];

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Case-insensitive substring to match against file paths.
    query: String,
    /// Optional extension filter, e.g. "rs" or "go".
    #[serde(default)]
    extension: Option<String>,
    /// Maximum results. Defaults to 100.
    #[serde(default)]
    max_results: Option<usize>,
}

pub struct RepositorySearchTool;

#[async_trait]
impl Tool for RepositorySearchTool {
    fn name(&self) -> &'static str {
        "repository_search"
    }

    fn description(&self) -> &'static str {
        "Find files whose path matches a query (case-insensitive substring), \
         optionally filtered by extension. Use this to locate files by name; use \
         `grep` to search file contents."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
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
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let query = input.query.to_lowercase();
        let max = input.max_results.unwrap_or(DEFAULT_MAX);

        let files = self.list_files(&context, &cancellation).await;
        let mut matches: Vec<String> = files
            .into_iter()
            .filter(|path| {
                path.to_lowercase().contains(&query)
                    && input
                        .extension
                        .as_deref()
                        .map(|ext| path.ends_with(&format!(".{ext}")))
                        .unwrap_or(true)
            })
            .collect();
        matches.sort();
        matches.dedup();
        let truncated = matches.len() > max;
        matches.truncate(max);

        if matches.is_empty() {
            return Ok(ToolOutput::ok("(no matching files)\n"));
        }
        let mut body = matches.join("\n");
        body.push('\n');
        if truncated {
            body.push_str("… [truncated]\n");
        }
        Ok(ToolOutput::ok(body))
    }
}

impl RepositorySearchTool {
    /// Prefer tracked files via `git ls-files`; fall back to a filesystem walk.
    async fn list_files(
        &self,
        context: &ToolContext,
        cancellation: &CancellationToken,
    ) -> Vec<String> {
        let mut request = ProcessRequest::new(
            "git",
            vec!["ls-files".into()],
            context.workspace.root().to_path_buf(),
        );
        request.timeout = Duration::from_secs(30);
        if let Ok(output) = context
            .runner
            .run(request, cancellation.child_token())
            .await
            && output.success()
            && !output.stdout.trim().is_empty()
        {
            return output.stdout.lines().map(str::to_string).collect();
        }
        // Fallback: walk the filesystem.
        let mut files = Vec::new();
        walk(
            context.workspace.root(),
            context.workspace.root(),
            &mut files,
        );
        files
    }
}

fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    if out.len() > 5000 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_files_by_name() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-repsearch-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/order_service.rs"), "").unwrap();
        std::fs::write(dir.join("src/user.rs"), "").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = RepositorySearchTool
            .execute(
                serde_json::json!({ "query": "order", "extension": "rs" }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("order_service.rs"));
        assert!(!out.content.contains("user.rs"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
