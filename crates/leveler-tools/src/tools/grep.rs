//! `grep` — search workspace files. Prefers ripgrep, falls back to a built-in
//! literal scan when `rg` is unavailable (spec §18.3).

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::{ProcessRequest, RiskLevel};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const DEFAULT_MAX: usize = 100;
/// Per-line character cap: a match on a minified/generated one-line file must
/// not flood the model's context with a multi-KB line.
const MAX_LINE_LEN: usize = 500;

/// Clip a single output line to [`MAX_LINE_LEN`] characters, pointing at
/// `read_file` for the rest. Counts by `char` so multibyte content isn't split.
fn clip_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_LEN {
        return line.to_string();
    }
    let clipped: String = line.chars().take(MAX_LINE_LEN).collect();
    format!("{clipped}… [line truncated at {MAX_LINE_LEN} chars; use read_file for the full line]")
}

/// Whether a pattern uses regex metacharacters — used only to warn that the
/// no-ripgrep fallback matched it as a literal substring, not a regex.
fn looks_like_regex(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|c| matches!(c, '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'))
}

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
    let total = text.lines().count();
    let mut out: Vec<String> = text.lines().take(max).map(clip_line).collect();
    if total > max {
        // `--max-count` is per-file, so `total` is itself a lower bound.
        out.push(format!(
            "… [showing first {max} of {total} matched lines; raise max_results \
             or narrow the pattern/path/glob]"
        ));
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
                matches.push(clip_line(&format!("{rel}:{}:{}", i + 1, line)));
                if matches.len() >= max {
                    break 'outer;
                }
            }
        }
    }
    // Without ripgrep this is a substring scan; a regex-shaped pattern was
    // matched literally, which silently mis-searches. Tell the model so it can
    // trust (or discount) the result.
    let literal_note = if looks_like_regex(pattern) {
        format!(
            "[note] ripgrep unavailable — `{pattern}` was matched as a literal substring, \
             not a regex.\n"
        )
    } else {
        String::new()
    };
    if matches.is_empty() {
        Ok(ToolOutput::ok(format!("{literal_note}(no matches)\n")))
    } else {
        let capped = matches.len() >= max;
        let mut s = literal_note;
        s.push_str(&matches.join("\n"));
        s.push('\n');
        if capped {
            s.push_str(&format!(
                "… [results capped at {max}; raise max_results or narrow the \
                 pattern/path]\n"
            ));
        }
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
        // The marker must say how much was dropped and how to recover.
        assert!(out.contains("of 3"), "must report the total: {out}");
        assert!(out.contains("max_results"), "must name the knob: {out}");
    }

    #[test]
    fn builtin_grep_marks_capped_results() {
        // The fallback must not silently stop at max: exactly-max output with
        // no marker is indistinguishable from a complete result.
        let dir = std::env::temp_dir().join(format!(
            "leveler-grep-cap-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "hit\nhit\nhit\n").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = builtin_grep(&ctx, "hit", &dir, 2).unwrap();
        assert_eq!(out.content.matches("hit").count(), 2, "{}", out.content);
        assert!(
            out.content.contains("max_results"),
            "capped fallback results must carry a marker: {}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn grep_clips_very_long_matched_lines() {
        // A match on a minified/one-line file must not flood the context with a
        // multi-KB line; clip it and point at read_file for the whole thing.
        let dir = std::env::temp_dir().join(format!(
            "leveler-grep-longline-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let long = "x".repeat(2000);
        std::fs::write(dir.join("min.js"), format!("needle {long}\n")).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = builtin_grep(&ctx, "needle", &dir, 10).unwrap();
        assert!(out.content.contains("needle"), "still matches: {}", out.content);
        assert!(
            out.content.contains("line truncated"),
            "long line must be clipped: {}",
            out.content
        );
        let match_line = out.content.lines().next().unwrap();
        assert!(
            match_line.chars().count() < MAX_LINE_LEN + 100,
            "clipped line must be bounded: {} chars",
            match_line.chars().count()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn builtin_fallback_flags_that_a_regex_was_matched_literally() {
        // Without ripgrep the fallback is a substring scan; a regex pattern is
        // matched literally, which silently mis-searches. Say so.
        let dir = std::env::temp_dir().join(format!(
            "leveler-grep-lit-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn foo() {}\n").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = builtin_grep(&ctx, "fn.*foo", &dir, 10).unwrap();
        assert!(
            out.content.contains("literal substring"),
            "a regex-shaped pattern in the fallback must be flagged as literal: {}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
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
