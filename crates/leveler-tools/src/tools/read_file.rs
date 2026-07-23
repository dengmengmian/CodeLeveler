//! `read_file` — read a workspace file with line numbers (spec §18.3).

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// Maximum bytes of file content returned per call; ranges page through the rest.
const MAX_BYTES: usize = 256 * 1024;
/// Maximum file size read into memory; larger files are refused with guidance.
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

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
         `list_files` instead — `read_file` does not list directories. Absolute \
         paths outside the workspace are rejected unless under a configured \
         readonly root (`--readonly-root`). Returns content with 1-based line \
         numbers; optional inclusive line range."
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
        let path = context.workspace.resolve_read(&input.path)?;

        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput::error(crate::recoverable::missing_file(
                    &input.path,
                )));
            }
            Err(e) => return Err(ToolError::Io(format!("stat {}: {e}", input.path))),
        };
        if meta.is_dir() {
            return Ok(ToolOutput::error(crate::recoverable::path_is_directory(
                &input.path,
            )));
        }
        if meta.len() > MAX_FILE_BYTES {
            return Ok(ToolOutput::error(format!(
                "file too large to read: `{}` is {} bytes (limit {} MB). Use `grep` \
                 to locate the relevant part, or `run_command` with sed/head/tail \
                 to slice it.",
                input.path,
                meta.len(),
                MAX_FILE_BYTES / (1024 * 1024)
            )));
        }

        // Detect wasteful repeated reads of the same unchanged range (spec §28).
        // This is deliberately a nudge, not a denial: edit tools can require a
        // fresh read to recover, and a read operation that returned content did
        // not fail.
        let range_key = format!(
            "{}:{}-{}",
            input.path,
            input.start_line.unwrap_or(0),
            input.end_line.unwrap_or(0)
        );
        let file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput::error(crate::recoverable::missing_file(
                    &input.path,
                )));
            }
            Err(e) => return Err(ToolError::Io(format!("read {}: {e}", input.path))),
        };

        let start = input.start_line.unwrap_or(1).max(1);
        let end = input.end_line.unwrap_or(usize::MAX);
        let mut out = String::new();
        let mut reader = tokio::io::BufReader::new(file);
        let mut line = Vec::new();
        let mut fingerprint = leveler_context::ContentFingerprint::default();
        let mut scanned_for_binary = 0usize;
        let mut binary = false;
        let mut total_lines = 0usize;
        let mut first_shown = None;
        let mut last_shown = 0usize;
        let mut clipped = false;
        let mut clipped_inside_line = false;

        // Stream the complete file once: this keeps narrow ranges O(line size)
        // in memory while still producing the full-file fingerprint needed by
        // stale-write protection and the total line count used in paging copy.
        loop {
            use tokio::io::AsyncBufReadExt;
            line.clear();
            let read = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return Ok(ToolOutput::error("read_file cancelled"));
                }
                read = reader.read_until(b'\n', &mut line) => read,
            }
            .map_err(|e| ToolError::Io(format!("read {}: {e}", input.path)))?;
            if read == 0 {
                break;
            }
            fingerprint.update(&line);
            if scanned_for_binary < 8192 {
                let scan_len = (8192 - scanned_for_binary).min(line.len());
                binary |= line[..scan_len].contains(&0);
                scanned_for_binary += scan_len;
            }
            total_lines += 1;
            let lineno = total_lines;
            if clipped || lineno < start || lineno > end {
                continue;
            }

            let prefix = format!("{lineno:>6}\t");
            if out.len() + prefix.len() + 1 > MAX_BYTES {
                clipped = true;
                continue;
            }
            let mut content_end = line.as_slice();
            if content_end.ends_with(b"\n") {
                content_end = &content_end[..content_end.len() - 1];
            }
            if content_end.ends_with(b"\r") {
                content_end = &content_end[..content_end.len() - 1];
            }
            let rendered = String::from_utf8_lossy(content_end);
            let remaining = MAX_BYTES - out.len() - prefix.len() - 1;
            if rendered.len() > remaining && !out.is_empty() {
                clipped = true;
                continue;
            }
            let shown = crate::registry::floor_boundary(&rendered, rendered.len().min(remaining));
            first_shown.get_or_insert(lineno);
            last_shown = lineno;
            out.push_str(&prefix);
            out.push_str(&rendered[..shown]);
            out.push('\n');
            if shown < rendered.len() {
                clipped = true;
                clipped_inside_line = true;
            }
        }

        if binary {
            return Ok(ToolOutput::error(format!(
                "refusing to read binary file: {}",
                input.path
            )));
        }

        let fingerprint = fingerprint.finish();
        let repeated = context
            .read_guard
            .tripped_fingerprint(&range_key, fingerprint);
        context
            .file_state
            .record_fingerprint(&input.path, fingerprint);

        if repeated {
            out.insert_str(
                0,
                "[note: this unchanged range was read multiple times; returning it again so recovery is not blocked]\n",
            );
        }
        if clipped {
            if clipped_inside_line {
                out.push_str(&format!(
                    "… [truncated within line {last_shown} of {total_lines}; the file is {} \
                     bytes / ~{} tokens — use grep or run_command to inspect that long line]\n",
                    meta.len(),
                    crate::registry::approx_tokens(meta.len() as usize),
                ));
            } else {
                out.push_str(&format!(
                    "… [truncated: lines {}–{} of {total_lines} lines shown ({} bytes \
                     / ~{} tokens total); continue with start_line={}]\n",
                    first_shown.unwrap_or(start),
                    last_shown,
                    meta.len(),
                    crate::registry::approx_tokens(meta.len() as usize),
                    last_shown + 1
                ));
            }
        }
        if first_shown.is_none() && !clipped {
            out.push_str(&format!(
                "(no lines in the requested range; the file has {total_lines} lines)\n"
            ));
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
        // Lock the bug: a line range that starts past MAX_BYTES must still
        // return content — the byte cap limits the *output*, not which part
        // of the file is reachable.
        let per_line = 58;
        let n = MAX_BYTES / per_line + 200;
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
        let n = MAX_BYTES / per_line + 200;
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

    #[tokio::test]
    async fn one_very_long_line_is_clipped_before_formatting() {
        let (ctx, dir) = ctx_with("long.txt", &"x".repeat(MAX_BYTES * 2)).await;
        let out = ReadFileTool
            .execute(
                serde_json::json!({"path": "long.txt"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(
            out.content.len() <= MAX_BYTES + 512,
            "a single line must not allocate/return the whole file: {} bytes",
            out.content.len()
        );
        assert!(out.content.contains("truncated"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn oversized_file_is_a_recoverable_error() {
        // Files past the in-memory limit are refused with guidance instead of
        // being read whole (memory) or silently clipped.
        let content = "y".repeat(MAX_FILE_BYTES as usize + 1);
        let (ctx, dir) = ctx_with("huge.bin.log", &content).await;
        let out = ReadFileTool
            .execute(
                serde_json::json!({"path": "huge.bin.log"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "oversized file must be a model-facing error");
        assert!(
            out.content.contains("grep") || out.content.contains("run_command"),
            "must steer to a tool that can slice it: {}",
            out.content
        );
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
    async fn repeated_read_still_returns_the_requested_content() {
        let (ctx, dir) = ctx_with("a.txt", "one\ntwo\n").await;
        for attempt in 1..=4 {
            let out = ReadFileTool
                .execute(
                    serde_json::json!({"path": "a.txt"}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(
                !out.is_error,
                "attempt {attempt} must remain recoverable: {}",
                out.content
            );
            assert!(out.content.contains("one"));
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
