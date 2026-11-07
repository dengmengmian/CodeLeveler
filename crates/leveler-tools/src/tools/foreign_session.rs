//! `foreign_session_history` — read the imported source session on demand.
//!
//! An imported session carries only a bounded briefing. The full transcript
//! stays in the source agent's file, and this tool is the only path to it.
//! Every call re-reads the file, so the source remains the single authority;
//! nothing is cached and nothing is written.

use async_trait::async_trait;
use leveler_execution::RiskLevel;
use leveler_foreign_session::{
    ForeignEventDetail, ForeignEventKind, ForeignEventRef, ForeignSessionError,
    ForeignSessionSummary, index, read_events, search,
};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// The tool name the import briefing tells the model to call.
pub const FOREIGN_SESSION_HISTORY_TOOL: &str = "foreign_session_history";

const OUTLINE_DEFAULT: usize = 120;
const OUTLINE_MAX: usize = 400;
const READ_DEFAULT: usize = 10;
const SEARCH_DEFAULT: usize = 10;

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Action {
    /// List event ordinals, kinds, source lines and bounded previews.
    Outline,
    /// Read the full text of a contiguous range, starting at `start`.
    Read,
    /// Case-insensitive literal search over event text.
    Search,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HistoryInput {
    action: Action,
    /// 1-based event ordinal. `outline` starts here; `read` starts here.
    #[serde(default)]
    start: Option<u32>,
    /// Maximum rows returned. Bounded per action.
    #[serde(default)]
    limit: Option<usize>,
    /// Literal text for `search`. Required for that action.
    #[serde(default)]
    query: Option<String>,
}

/// One imported session, bound at construction.
///
/// The binding is the exact summary resolved at import time. The source file
/// is re-read on every call and never trusted to be unchanged or present: a
/// deleted or truncated source fails loudly instead of serving a stale copy.
pub struct ForeignSessionHistoryTool {
    summary: ForeignSessionSummary,
}

impl ForeignSessionHistoryTool {
    pub fn new(summary: ForeignSessionSummary) -> Self {
        Self { summary }
    }

    fn identity(&self) -> String {
        let summary = &self.summary;
        format!(
            "source: {} session {}\nfile: {}\ncwd: {}",
            summary.tool.display_name(),
            summary.native_id,
            summary.source_path.display(),
            summary
                .cwd
                .as_ref()
                .map(|cwd| cwd.display().to_string())
                .unwrap_or_else(|| "(not recorded)".to_string()),
        )
    }
}

#[async_trait]
impl Tool for ForeignSessionHistoryTool {
    fn name(&self) -> &'static str {
        FOREIGN_SESSION_HISTORY_TOOL
    }

    fn description(&self) -> &'static str {
        "Read the original transcript of the agent session this conversation was \
         imported from. `outline` lists event ordinals with bounded previews; \
         `read` returns full text for a range starting at `start`; `search` \
         finds literal text and returns ordinals with context. This is read-only \
         source evidence: it does not replay anything, and a claim in it is not \
         current workspace fact."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<HistoryInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    /// The source file is an absolute path resolved at import time; the tool
    /// reads it directly and needs no workspace of its own.
    fn requires_workspace(&self) -> bool {
        false
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: HistoryInput = super::parse_input(self.name(), input)?;
        if cancellation.is_cancelled() {
            return Ok(ToolOutput::error("cancelled before reading the source"));
        }
        let summary = self.summary.clone();
        let action = input.action;
        let start = input.start.unwrap_or(1);
        let limit = input.limit;
        let query = input.query;
        let identity = self.identity();
        let result = tokio::task::spawn_blocking(move || match action {
            Action::Outline => outline(&summary, start, limit),
            Action::Read => read(&summary, start, limit),
            Action::Search => match query.as_deref() {
                Some(query) if !query.trim().is_empty() => search_source(&summary, query, limit),
                _ => Ok(ToolOutput::error("`search` requires a non-empty `query`")),
            },
        })
        .await
        .map_err(|error| ToolError::InvalidArguments {
            tool: FOREIGN_SESSION_HISTORY_TOOL.to_string(),
            message: format!("reader thread failed: {error}"),
        })?;
        let output = match result {
            Ok(output) => output,
            Err(error) => source_error(error),
        };
        let mut output = output.with_metadata(serde_json::json!({
            "source": self.summary.source_path.display().to_string(),
            "native_id": self.summary.native_id,
        }));
        output.content = format!("{identity}\n\n{}", output.content);
        Ok(output)
    }
}

fn outline(
    summary: &ForeignSessionSummary,
    start: u32,
    limit: Option<usize>,
) -> Result<ToolOutput, ForeignSessionError> {
    let index = index(summary)?;
    let limit = limit.unwrap_or(OUTLINE_DEFAULT).clamp(1, OUTLINE_MAX);
    let start = start.max(1);
    let mut out = format!(
        "{} meaningful events ({} records, {} malformed lines, {} compactions{})\n",
        index.events.len(),
        index.stats.records,
        index.stats.malformed_lines,
        index.stats.compactions,
        if index.truncated {
            ", index truncated at the per-call cap"
        } else {
            ""
        },
    );
    let rows: Vec<&ForeignEventRef> = index
        .events
        .iter()
        .filter(|event| event.index >= start)
        .take(limit)
        .collect();
    if rows.is_empty() {
        out.push_str(&format!("no events at or after #{start}\n"));
    }
    for event in rows {
        let sidechain = if event.sidechain { " sidechain" } else { "" };
        out.push_str(&format!(
            "#{} [{}]{} L{} {} — {}\n",
            event.index,
            kind_str(event.kind),
            sidechain,
            event.line,
            event.timestamp.as_deref().unwrap_or("-"),
            event.preview,
        ));
    }
    let last = index.events.last().map(|event| event.index).unwrap_or(0);
    out.push_str(&format!(
        "range shown: #{start}.. (last ordinals available; last is #{last})\n"
    ));
    Ok(ToolOutput::ok(out))
}

fn read(
    summary: &ForeignSessionSummary,
    start: u32,
    limit: Option<usize>,
) -> Result<ToolOutput, ForeignSessionError> {
    let limit = limit.unwrap_or(READ_DEFAULT).clamp(1, 50);
    let details = read_events(summary, start.max(1), limit)?;
    if details.is_empty() {
        return Ok(ToolOutput::ok("no events in that range"));
    }
    let mut out = String::new();
    for detail in &details {
        out.push_str(&format_detail(detail));
    }
    Ok(ToolOutput::ok(out))
}

fn search_source(
    summary: &ForeignSessionSummary,
    query: &str,
    limit: Option<usize>,
) -> Result<ToolOutput, ForeignSessionError> {
    let limit = limit.unwrap_or(SEARCH_DEFAULT).clamp(1, 20);
    let hits = search(summary, query, limit)?;
    if hits.is_empty() {
        return Ok(ToolOutput::ok(format!("no event contains {query:?}")));
    }
    let mut out = format!("{} hit(s) for {:?}:\n", hits.len(), query);
    for hit in hits {
        out.push_str(&format!(
            "#{} [{}] L{} — {}\n",
            hit.event.index,
            kind_str(hit.event.kind),
            hit.event.line,
            hit.snippet,
        ));
    }
    out.push_str("Use `read` with an ordinal to see the full event.\n");
    Ok(ToolOutput::ok(out))
}

fn format_detail(detail: &ForeignEventDetail) -> String {
    let mut header = format!(
        "=== #{} [{}] L{} {}",
        detail.index,
        kind_str(detail.kind),
        detail.line,
        detail.timestamp.as_deref().unwrap_or("-"),
    );
    if detail.sidechain {
        header.push_str(" sidechain");
    }
    if detail.is_error {
        header.push_str(" error");
    }
    if let Some(name) = detail.tool_name.as_deref() {
        header.push_str(&format!(" tool={name}"));
    }
    if let Some(call_id) = detail.call_id.as_deref() {
        header.push_str(&format!(" call={call_id}"));
    }
    header.push_str(" ===\n");
    header.push_str(&detail.text);
    if detail.truncated {
        header.push_str("\n[truncated at the per-event limit]");
    }
    header.push_str("\n\n");
    header
}

fn kind_str(kind: ForeignEventKind) -> &'static str {
    match kind {
        ForeignEventKind::User => "user",
        ForeignEventKind::Assistant => "assistant",
        ForeignEventKind::ToolCall => "tool_call",
        ForeignEventKind::ToolResult => "tool_result",
        ForeignEventKind::System => "system",
    }
}

fn source_error(error: ForeignSessionError) -> ToolOutput {
    ToolOutput::error(format!(
        "could not read the imported source session: {error}"
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use leveler_foreign_session::{ForeignTool, find};

    use super::*;

    const ID: &str = "11111111-1111-4111-8111-111111111111";

    fn summary(root: &Path) -> ForeignSessionSummary {
        let dir = root.join("projects").join("-tmp-work-proj");
        fs::create_dir_all(&dir).unwrap();
        let lines = [
            format!(
                r##"{{"type":"user","sessionId":"{ID}","cwd":"/tmp/work/proj","message":{{"role":"user","content":"Fix the failing build"}}}}"##
            ),
            format!(
                r#"{{"type":"assistant","sessionId":"{ID}","cwd":"/tmp/work/proj","message":{{"role":"assistant","content":[{{"type":"text","text":"I will inspect it."}},{{"type":"tool_use","id":"toolu_1","name":"Bash","input":{{"command":"cargo test"}}}}]}}}}"#
            ),
            format!(
                r#"{{"type":"user","sessionId":"{ID}","cwd":"/tmp/work/proj","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":"error: test failed"}}]}}}}"#
            ),
        ];
        fs::write(dir.join(format!("{ID}.jsonl")), lines.join("\n") + "\n").unwrap();
        find(ForeignTool::ClaudeCode, root, ID).unwrap()
    }

    async fn run(tool: &ForeignSessionHistoryTool, input: serde_json::Value) -> ToolOutput {
        let dir = tempfile::tempdir().unwrap();
        let ws = leveler_execution::Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        tool.execute(input, ctx, CancellationToken::new())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn outline_read_and_search_serve_the_source_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ForeignSessionHistoryTool::new(summary(dir.path()));

        let outline = run(&tool, serde_json::json!({"action": "outline"})).await;
        assert!(!outline.is_error, "{}", outline.content);
        assert!(
            outline.content.contains("Claude Code session"),
            "{}",
            outline.content
        );
        assert!(outline.content.contains("#1 [user]"), "{}", outline.content);
        assert!(
            outline.content.contains("tool_result"),
            "{}",
            outline.content
        );

        let read = run(
            &tool,
            serde_json::json!({"action": "read", "start": 4, "limit": 1}),
        )
        .await;
        assert!(!read.is_error, "{}", read.content);
        assert!(
            read.content.contains("error: test failed"),
            "{}",
            read.content
        );
        assert!(
            !read.content.contains("Fix the failing build"),
            "{}",
            read.content
        );

        let search = run(
            &tool,
            serde_json::json!({"action": "search", "query": "FAILING"}),
        )
        .await;
        assert!(!search.is_error, "{}", search.content);
        assert!(search.content.contains("#1"), "{}", search.content);
    }

    #[tokio::test]
    async fn missing_source_and_bad_arguments_are_visible_failures() {
        let dir = tempfile::tempdir().unwrap();
        let source = summary(dir.path());
        let tool = ForeignSessionHistoryTool::new(source.clone());
        fs::remove_file(&source.source_path).unwrap();

        let missing = run(&tool, serde_json::json!({"action": "outline"})).await;
        assert!(missing.is_error);
        assert!(
            missing.content.contains("could not read"),
            "{}",
            missing.content
        );
        assert!(
            missing
                .content
                .contains(&source.source_path.display().to_string()),
            "the failure must name the exact source"
        );

        let tool = ForeignSessionHistoryTool::new(summary(dir.path()));
        let no_query = run(&tool, serde_json::json!({"action": "search"})).await;
        assert!(no_query.is_error);
        assert!(no_query.content.contains("requires a non-empty `query`"));
    }
}
