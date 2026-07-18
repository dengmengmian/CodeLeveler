//! `read_file` — read a workspace file with line numbers (spec §18.3).

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// Maximum bytes read before truncating (large files go to artifacts later).
const MAX_BYTES: usize = 256 * 1024;

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
        _cancellation: CancellationToken,
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
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput::error(crate::recoverable::missing_file(
                    &input.path,
                )));
            }
            Err(e) => return Err(ToolError::Io(format!("read {}: {e}", input.path))),
        };
        let repeated = context.read_guard.tripped(&range_key, &bytes);

        // Reject binary content: a NUL byte in the first chunk is a strong signal.
        let scan = &bytes[..bytes.len().min(8192)];
        if scan.contains(&0) {
            return Ok(ToolOutput::error(format!(
                "refusing to read binary file: {}",
                input.path
            )));
        }

        // Remember the file as the model now sees it, so a later apply_patch can
        // tell whether something else rewrote it in between.
        context.file_state.record(&input.path, &bytes);

        let truncated = bytes.len() > MAX_BYTES;
        let slice = &bytes[..bytes.len().min(MAX_BYTES)];
        let text = String::from_utf8_lossy(slice);

        let start = input.start_line.unwrap_or(1).max(1);
        let end = input.end_line.unwrap_or(usize::MAX);

        let mut out = String::new();
        if repeated {
            out.push_str(
                "[note: this unchanged range was read multiple times; returning it again so recovery is not blocked]\n",
            );
        }
        for (i, line) in text.lines().enumerate() {
            let lineno = i + 1;
            if lineno < start {
                continue;
            }
            if lineno > end {
                break;
            }
            out.push_str(&format!("{lineno:>6}\t{line}\n"));
        }
        if truncated {
            out.push_str("… [truncated: file exceeds read limit]\n");
        }
        if out.is_empty() {
            out.push_str("(no lines in the requested range)\n");
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
