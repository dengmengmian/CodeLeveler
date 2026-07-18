//! `grep` — search workspace files. Prefers ripgrep, falls back to a built-in
//! literal scan when `rg` is unavailable (spec §18.3).

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::{ProcessRequest, RiskLevel};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const DEFAULT_MAX: usize = 100;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// The pattern to search for (regex when ripgrep is available).
    pattern: String,
    /// Directory or file to search, relative to the workspace. Defaults to ".".
    #[serde(default)]
    path: Option<String>,
    /// Optional glob filter, e.g. "*.rs".
    #[serde(default)]
    glob: Option<String>,
    /// Maximum number of matching lines to return. Defaults to 100.
    #[serde(default)]
    max_results: Option<usize>,
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search files under the workspace root (relative path, or absolute under \
         the workspace / a `--readonly-root`) for a pattern. Returns matching \
         lines as `path:line:text`. Uses ripgrep when available."
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
        let rel = input.path.clone().unwrap_or_else(|| ".".to_string());
        let search_root = context.workspace.resolve_read(&rel)?;
        let max = input.max_results.unwrap_or(DEFAULT_MAX);

        // Try ripgrep first.
        let mut args = vec![
            "--line-number".to_string(),
            "--no-heading".to_string(),
            "--color=never".to_string(),
            format!("--max-count={max}"),
        ];
        if let Some(glob) = &input.glob {
            args.push("--glob".to_string());
            args.push(glob.clone());
        }
        args.push(input.pattern.clone());
        args.push(search_root.to_string_lossy().into_owned());

        let request = ProcessRequest::new("rg", args, context.workspace.root().to_path_buf());
        match context.runner.run(request, cancellation).await {
            Ok(output) if !output.timed_out => {
                // rg exits 1 when there are no matches; that is not an error.
                let text = relativize(&output.stdout, context.workspace.root());
                let body = truncate_lines(&text, max);
                if body.trim().is_empty() {
                    Ok(ToolOutput::ok("(no matches)\n"))
                } else {
                    Ok(ToolOutput::ok(body))
                }
            }
            _ => builtin_grep(&context, &input.pattern, &search_root, max),
        }
    }
}

/// Rewrite absolute paths in rg output back to workspace-relative.
fn relativize(text: &str, root: &std::path::Path) -> String {
    let prefix = format!("{}/", root.display());
    text.lines()
        .map(|l| l.strip_prefix(&prefix).unwrap_or(l).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_lines(text: &str, max: usize) -> String {
    let mut out: Vec<&str> = text.lines().take(max).collect();
    if text.lines().count() > max {
        out.push("… [truncated]");
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Literal (substring) fallback used when ripgrep is not installed.
fn builtin_grep(
    context: &ToolContext,
    pattern: &str,
    root: &std::path::Path,
    max: usize,
) -> Result<ToolOutput, ToolError> {
    let mut matches = Vec::new();
    let mut files = Vec::new();
    collect_files(root, &mut files);
    'outer: for file in files {
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (i, line) in content.lines().enumerate() {
            if line.contains(pattern) {
                let rel = file
                    .strip_prefix(context.workspace.root())
                    .unwrap_or(&file)
                    .display();
                matches.push(format!("{rel}:{}:{}", i + 1, line));
                if matches.len() >= max {
                    break 'outer;
                }
            }
        }
    }
    if matches.is_empty() {
        Ok(ToolOutput::ok("(no matches)\n"))
    } else {
        let mut s = matches.join("\n");
        s.push('\n');
        Ok(ToolOutput::ok(s))
    }
}

fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    const IGNORED: &[&str] = &[
        "target",
        "node_modules",
        ".git",
        "dist",
        "vendor",
        ".leveler",
    ];
    if dir.is_file() {
        out.push(dir.to_path_buf());
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
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_a_symbol() {
        let dir =
            std::env::temp_dir().join(format!("leveler-grep-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn OrderService() {}\nlet x = 1;\n").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = GrepTool
            .execute(
                serde_json::json!({"pattern": "OrderService"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("OrderService"), "got: {}", out.content);
        assert!(out.content.contains("a.rs"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncate_lines_caps_results() {
        let text = "line1\nline2\nline3\n";
        let out = truncate_lines(text, 2);
        assert!(out.contains("line1"));
        assert!(out.contains("line2"));
        assert!(!out.contains("line3"));
        assert!(out.contains("… [truncated]"));
    }

    #[test]
    fn builtin_grep_finds_literal_matches() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-grep-builtin-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn foo() {}\nfn bar() {}\n").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = builtin_grep(&ctx, "foo", &dir, 10).unwrap();
        assert!(out.content.contains("fn foo()"));
        assert!(!out.content.contains("fn bar()"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn builtin_grep_reports_no_matches() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-grep-empty-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn foo() {}\n").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = builtin_grep(&ctx, "missing", &dir, 10).unwrap();
        assert!(out.content.contains("(no matches)"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
