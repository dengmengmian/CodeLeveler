//! `grep` — the `grep` primitive: search file contents.
//!
//! The pattern is a regex unless the caller asks for literal matching. That
//! choice belongs to the caller and is never made for it by what happens to be
//! installed on the machine: one invocation, one meaning, every platform.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use crate::workspace::{GrepQuery, SearchError, WorkspaceSearch};

const DEFAULT_LIMIT: usize = 100;
const HARD_LIMIT: usize = 1000;
/// Per-line character cap: a match on a minified/generated one-line file must
/// not flood the model's context with a multi-KB line.
const MAX_LINE_LEN: usize = 500;

/// Clip a single output line to [`MAX_LINE_LEN`] characters. Counts by `char`
/// so multibyte content isn't split.
fn clip_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_LEN {
        return line.to_string();
    }
    let clipped: String = line.chars().take(MAX_LINE_LEN).collect();
    format!("{clipped}… [line clipped at {MAX_LINE_LEN} chars]")
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Regular expression to search for (or literal text when `literal` is set).
    pattern: String,
    /// Directory or file to search, relative to the workspace. Defaults to ".".
    #[serde(default)]
    path: Option<String>,
    /// Optional glob restricting which files are searched, e.g. "*.rs".
    #[serde(default, alias = "glob")]
    include: Option<String>,
    /// Match `pattern` as literal text instead of a regular expression.
    #[serde(default)]
    literal: bool,
    /// Match without regard to case.
    #[serde(default)]
    ignore_case: bool,
    /// Maximum matching lines to return (default 100, hard cap 1000).
    #[serde(default, alias = "max_results")]
    limit: Option<usize>,
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents under a path (relative to the workspace root, or \
         any absolute path — reads are not confined to the workspace). \
         `pattern` is a regular expression; set `literal` to match it as plain \
         text and `ignore_case` to fold case. `include` is a glob over file \
         paths using the same rule as `find_files`. Returns matching lines as \
         `path:line:text`."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    /// Pure in-process traversal: no write, no subprocess, no language
    /// server, no network. Re-running it after a crash changes nothing.
    fn replay_is_side_effect_free(&self) -> bool {
        true
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
        let rel = input.path.clone().unwrap_or_else(|| ".".to_string());
        let search_root = context.execution.workspace.resolve_for_read(&rel)?;
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, HARD_LIMIT);

        let query = GrepQuery {
            pattern: input.pattern,
            literal: input.literal,
            ignore_case: input.ignore_case,
            include: input.include,
            limit,
        };
        let result = match WorkspaceSearch::grep(
            &search_root,
            context.execution.workspace.root(),
            &query,
            &cancellation,
        )
        .await
        {
            Ok(result) => result,
            Err(SearchError::InvalidRegex { pattern, message }) => {
                return Ok(ToolOutput::error(format!(
                    "`{pattern}` is not a valid regex: {message}. Set `literal` to \
                     search for it as plain text."
                )));
            }
            Err(SearchError::InvalidGlob { pattern, message }) => {
                return Ok(ToolOutput::error(format!(
                    "`include` glob `{pattern}` is invalid: {message}"
                )));
            }
            Err(SearchError::Cancelled) => return Ok(ToolOutput::error("grep cancelled")),
            Err(other) => return Err(ToolError::Io(format!("grep {rel}: {other:?}"))),
        };

        let mut body = String::new();
        for found in &result.matches {
            body.push_str(&clip_line(&format!(
                "{}:{}:{}",
                found.path, found.line, found.text
            )));
            body.push('\n');
        }
        if result.matches.is_empty() {
            body.push_str("(no matches)\n");
        }
        if result.limited {
            body.push_str(&format!(
                "… [stopped at {limit} matching lines; raise limit or narrow the \
                 pattern/path/include]\n"
            ));
        }
        if result.skipped_binary > 0 {
            body.push_str(&format!(
                "… [{} file(s) skipped: not UTF-8 text]\n",
                result.skipped_binary
            ));
        }
        Ok(ToolOutput::ok(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::Builder::new()
            .prefix("leveler-grep-")
            .tempdir()
            .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/a.rs"),
            "fn   main() {}\nlet x = a.b();\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.md"), "main\n").unwrap();
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        (dir, ctx)
    }

    async fn grep(ctx: &ToolContext, args: serde_json::Value) -> ToolOutput {
        GrepTool
            .execute(args, ctx.clone(), CancellationToken::new())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_pattern_is_a_regex() {
        let (_dir, ctx) = workspace();
        let out = grep(&ctx, serde_json::json!({"pattern": "fn {2,}main"})).await;
        assert_eq!(out.content, "src/a.rs:1:fn   main() {}\n");
    }

    #[tokio::test]
    async fn literal_mode_is_asked_for_explicitly() {
        let (_dir, ctx) = workspace();
        let wildcard = grep(&ctx, serde_json::json!({"pattern": "a.b"})).await;
        assert!(wildcard.content.contains("a.b()"), "{}", wildcard.content);

        let literal = grep(
            &ctx,
            serde_json::json!({"pattern": "fn {2,}main", "literal": true}),
        )
        .await;
        assert_eq!(literal.content, "(no matches)\n");
    }

    #[tokio::test]
    async fn include_filters_with_the_find_glob_rule() {
        let (_dir, ctx) = workspace();
        let out = grep(
            &ctx,
            serde_json::json!({"pattern": "main", "include": "*.md"}),
        )
        .await;
        assert_eq!(out.content, "notes.md:1:main\n");
    }

    #[tokio::test]
    async fn an_invalid_regex_names_the_constraint_and_the_alternative() {
        let (_dir, ctx) = workspace();
        let out = grep(&ctx, serde_json::json!({"pattern": "a(b"})).await;
        assert!(out.is_error);
        assert!(out.content.contains("not a valid regex"), "{}", out.content);
        assert!(out.content.contains("literal"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_long_match_line_is_clipped() {
        let dir = tempfile::Builder::new()
            .prefix("leveler-grep-long-")
            .tempdir()
            .unwrap();
        std::fs::write(
            dir.path().join("min.js"),
            format!("needle{}\n", "z".repeat(4000)),
        )
        .unwrap();
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = grep(&ctx, serde_json::json!({"pattern": "needle"})).await;
        assert!(out.content.contains("line clipped"), "{}", out.content);
        assert!(out.content.len() < 700, "{} bytes", out.content.len());
    }

    /// Old event-log calls carry `glob` and `max_results`. Replaying one must
    /// keep their meaning rather than silently ignoring them.
    #[tokio::test]
    async fn the_historical_argument_names_still_bind() {
        let (_dir, ctx) = workspace();
        let out = grep(&ctx, serde_json::json!({"pattern": "main", "glob": "*.md"})).await;
        assert_eq!(out.content, "notes.md:1:main\n");

        let out = grep(
            &ctx,
            serde_json::json!({"pattern": "main", "max_results": 1}),
        )
        .await;
        assert_eq!(out.content.lines().count(), 2, "{}", out.content);
    }
}
