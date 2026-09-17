//! `read_file` — the `read` primitive: a bounded window of one text file.
//!
//! The tool owns the model-facing contract (schema, rendering, paging copy).
//! Everything filesystem-shaped belongs to
//! [`crate::workspace::WorkspaceReader`].

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use crate::workspace::{Clip, ReadError, ReadWindow, WorkspaceReader};

/// Bytes `render_line` adds around a line's own text: the right-aligned line
/// number, the tab, and the newline. The reader charges this against the
/// result budget so the rendered output really does fit.
fn line_overhead(number: usize) -> usize {
    number.to_string().len().max(6) + 2
}

fn render_line(number: usize, text: &str) -> String {
    format!("{number:>6}\t{text}\n")
}

/// Ceiling on the bytes of file content returned per call; windows page through
/// the rest. The effective budget is the turn's tool-result budget
/// ([`crate::tool::ToolPolicy::tool_output_budget`]) minus [`MARKER_RESERVE`],
/// so this tool's own paging marker is the only truncation the model sees: a
/// result the registry's central cap had to chop would carry a `start_line=N`
/// that points past an elided middle, and the model would page over lines it
/// never saw. This ceiling still applies when a budget is larger than it.
const MAX_BYTES: usize = 256 * 1024;
/// Bytes held back from the budget for the paging marker, so appending it
/// cannot push the result over the cap.
const MARKER_RESERVE: usize = 512;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Path to the file, relative to the workspace root.
    path: String,
    /// Optional 1-based first line to include.
    #[serde(default)]
    start_line: Option<usize>,
    /// Optional 1-based last line to include (inclusive).
    #[serde(default)]
    end_line: Option<usize>,
}

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text *file* under the workspace root (prefer paths relative \
         to that root, e.g. `src/lib.rs`). Paths that are directories must use \
         `list_files` instead — `read_file` does not list directories. Any \
         absolute path is readable (reads are not confined to the workspace; \
         credential files such as `.env` and private keys are refused). \
         Returns content with 1-based line numbers. `start_line`/`end_line` \
         return only that inclusive range; omitting both returns the whole \
         file. A result too large for one call is cut at a line boundary and \
         names the line to continue from, so any file is readable in windows \
         regardless of its size."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    /// Pure in-process file read: no write, no subprocess, no language
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
        let path = context.execution.workspace.resolve_for_read(&input.path)?;

        // Page at the budget the registry will enforce, less the room the
        // marker needs. `saturating_sub` keeps a pathologically small budget
        // from wrapping; the registry's own floor keeps it sane in practice.
        let max_bytes = context
            .policy
            .tool_output_budget
            .saturating_sub(MARKER_RESERVE)
            .clamp(1, MAX_BYTES);
        let window =
            ReadWindow::rendered(input.start_line, input.end_line, max_bytes, line_overhead);

        let read = match WorkspaceReader::read(&path, window, &cancellation).await {
            Ok(read) => read,
            Err(ReadError::NotFound) => {
                return Ok(ToolOutput::error(crate::recoverable::missing_file(
                    &input.path,
                )));
            }
            Err(ReadError::IsDirectory) => {
                return Ok(ToolOutput::error(crate::recoverable::path_is_directory(
                    &input.path,
                )));
            }
            Err(ReadError::NotUtf8 { line }) => {
                return Ok(ToolOutput::error(format!(
                    "`{}` is not valid UTF-8: decoding fails at line {line}. The file \
                     was not read; its bytes are not text and no substitute text was \
                     invented for them.",
                    input.path
                )));
            }
            Err(ReadError::Binary) => {
                return Ok(ToolOutput::error(format!(
                    "`{}` contains NUL bytes, so it is not a text file.",
                    input.path
                )));
            }
            Err(ReadError::Cancelled) => return Ok(ToolOutput::error("read_file cancelled")),
            Err(ReadError::Io(message)) => {
                return Err(ToolError::Io(format!("read {}: {message}", input.path)));
            }
        };

        // The observation this read took is what a later `apply_patch` checks
        // its precondition against.
        context
            .execution
            .file_state
            .record_fingerprint(&input.path, read.observation.fingerprint);

        let total_lines = read.observation.total_lines;
        let mut out = String::new();
        for (number, text) in &read.lines {
            out.push_str(&render_line(*number, text));
        }
        match read.clip {
            Clip::None => {
                if read.lines.is_empty() {
                    out.push_str(&format!(
                        "(no lines in the requested range; the file has {total_lines} lines)\n"
                    ));
                }
            }
            Clip::AtLine { next_line } => {
                out.push_str(&format!(
                    "… [truncated: lines {}–{} of {total_lines} lines shown ({} bytes \
                     / ~{} tokens total); continue with start_line={next_line}]\n",
                    read.lines.first().map(|(n, _)| *n).unwrap_or(next_line),
                    next_line.saturating_sub(1),
                    read.observation.file_bytes,
                    crate::registry::approx_tokens(read.observation.file_bytes as usize),
                ));
            }
            Clip::InsideLine { line, line_bytes } => {
                out.push_str(&format!(
                    "… [truncated inside line {line} of {total_lines}: that single line is \
                     {line_bytes} bytes, more than this call's {max_bytes}-byte result \
                     budget]\n"
                ));
            }
        }

        Ok(ToolOutput::ok(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ctx_with(file: &str, content: &str) -> (ToolContext, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("leveler-read-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), content).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        (
            ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval),
            dir,
        )
    }

    #[tokio::test]
    async fn reads_with_line_numbers() {
        let (ctx, dir) = ctx_with("a.txt", "one\ntwo\nthree\n").await;
        let out = ReadFileTool
            .execute(
                serde_json::json!({"path": "a.txt"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("     1\tone"));
        assert!(out.content.contains("     3\tthree"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn respects_line_range() {
        let (ctx, dir) = ctx_with("a.txt", "l1\nl2\nl3\nl4\n").await;
        let out = ReadFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "start_line": 2, "end_line": 3}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("l2"));
        assert!(out.content.contains("l3"));
        assert!(!out.content.contains("l1"));
        assert!(!out.content.contains("l4"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn range_past_the_byte_cap_still_returns_lines() {
        // Lock the bug: a line range that starts past the output cap must
        // still return content — the byte cap limits the *output*, not which
        // part of the file is reachable.
        let per_line = 58;
        let n = crate::registry::MAX_TOOL_OUTPUT / per_line + 200;
        let mut content = String::new();
        for i in 0..n {
            content.push_str(&format!("l{i:06}-{}\n", "x".repeat(per_line - 9)));
        }
        let (ctx, dir) = ctx_with("big.txt", &content).await;
        let out = ReadFileTool
            .execute(
                serde_json::json!({"path": "big.txt", "start_line": n - 5, "end_line": n}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains(&format!("l{:06}", n - 3)),
            "tail range must be readable: {}",
            &out.content[..out.content.len().min(300)]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn truncation_marker_reports_total_lines_and_paging() {
        // A truncated read must tell the model how big the file is and how to
        // page through it, not a bare "[truncated]".
        let per_line = 58;
        let n = crate::registry::MAX_TOOL_OUTPUT / per_line + 200;
        let mut content = String::new();
        for i in 0..n {
            content.push_str(&format!("l{i:06}-{}\n", "x".repeat(per_line - 9)));
        }
        let (ctx, dir) = ctx_with("big.txt", &content).await;
        let out = ReadFileTool
            .execute(
                serde_json::json!({"path": "big.txt"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content.contains(&format!("of {n} lines")),
            "marker must state the total line count: {}",
            &out.content[out.content.len().saturating_sub(300)..]
        );
        assert!(
            out.content.contains("start_line="),
            "marker must tell the model how to continue: {}",
            &out.content[out.content.len().saturating_sub(300)..]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The registry caps every result at the per-turn budget. `read_file`
    /// must page at that same budget so its own marker is the only
    /// truncation the model sees: a `start_line=N` that points past a
    /// middle the central cap elided would make the model skip lines it
    /// never saw.
    #[tokio::test]
    async fn paging_marker_points_at_the_first_unseen_line_under_the_central_cap() {
        let budget = 8 * 1024;
        let per_line = 58;
        let n = budget * 3 / per_line;
        let mut content = String::new();
        for i in 0..n {
            content.push_str(&format!("l{i:06}-{}\n", "x".repeat(per_line - 9)));
        }
        let (mut ctx, dir) = ctx_with("big.txt", &content).await;
        ctx.policy.tool_output_budget = budget;
        let out = crate::default_registry()
            .execute(
                "read_file",
                serde_json::json!({"path": "big.txt"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            !out.content.contains("elided to fit the context"),
            "the tool must page under the budget itself, not be chopped by the central cap"
        );
        let marker = out
            .content
            .rfind("continue with start_line=")
            .expect("paging marker");
        let next: usize = out.content[marker + "continue with start_line=".len()..]
            .trim_end_matches(|c: char| !c.is_ascii_digit())
            .trim_end_matches(']')
            .trim_end()
            .parse()
            .expect("start_line number");
        // Every line before the pointer was shown, contiguously, so the model
        // resumes exactly where it stopped reading.
        for line in 1..next {
            assert!(
                out.content
                    .contains(&format!("{line:>6}\tl{:06}-", line - 1)),
                "line {line} must be present before start_line={next}"
            );
        }
        assert!(
            !out.content.contains(&format!("{next:>6}\t")),
            "start_line={next} must be the first line NOT shown"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn one_very_long_line_is_clipped_before_formatting() {
        let (ctx, dir) = ctx_with(
            "long.txt",
            &"x".repeat(crate::registry::MAX_TOOL_OUTPUT * 2),
        )
        .await;
        let out = ReadFileTool
            .execute(
                serde_json::json!({"path": "long.txt"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(
            out.content.len() <= crate::registry::MAX_TOOL_OUTPUT + MARKER_RESERVE,
            "a single line must not allocate/return the whole file: {} bytes",
            out.content.len()
        );
        assert!(out.content.contains("truncated inside line 1"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn missing_file_is_model_error() {
        let (ctx, dir) = ctx_with("a.txt", "x").await;
        let out = ReadFileTool
            .execute(
                serde_json::json!({"path": "nope.txt"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("file not found") && out.content.contains("list_files"),
            "missing file should be recoverable: {}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn directory_path_tells_model_to_use_list_files() {
        let dir =
            std::env::temp_dir().join(format!("leveler-read-dir-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = ReadFileTool
            .execute(
                serde_json::json!({"path": "sub"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "directory must be a model-facing error");
        assert!(
            out.content.contains("directory") && out.content.contains("list_files"),
            "error should redirect to list_files: {}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn repeated_reads_return_the_same_content_unannotated() {
        let (ctx, dir) = ctx_with("a.txt", "one\ntwo\n").await;
        let mut first = None;
        for attempt in 1..=4 {
            let out = ReadFileTool
                .execute(
                    serde_json::json!({"path": "a.txt"}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(!out.is_error, "attempt {attempt}: {}", out.content);
            let previous = first.get_or_insert_with(|| out.content.clone());
            assert_eq!(
                *previous, out.content,
                "how often the model reads is not the reader's business"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
