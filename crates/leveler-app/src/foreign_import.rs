//! Building the bounded briefing for a session imported from another agent.
//!
//! The briefing is context, not a transcript copy: it carries the source
//! identity, the quoted task, the state the source ended in, and a pointer to
//! the read-only history tool for everything else. The source file stays the
//! single authority for the original history.

use std::path::Path;

use leveler_client_protocol::{UiForeignSession, UiForeignTool};
use leveler_foreign_session::{
    ForeignEventKind, ForeignSessionDigest, ForeignSessionSummary, ForeignTool,
};

/// Map a wire tool to the reader adapter.
pub(crate) fn adapter_tool(tool: UiForeignTool) -> ForeignTool {
    match tool {
        UiForeignTool::ClaudeCode => ForeignTool::ClaudeCode,
        UiForeignTool::Codex => ForeignTool::Codex,
    }
}

/// Map a reader tool back to its wire value.
pub(crate) fn ui_tool(tool: ForeignTool) -> UiForeignTool {
    match tool {
        ForeignTool::ClaudeCode => UiForeignTool::ClaudeCode,
        ForeignTool::Codex => UiForeignTool::Codex,
    }
}

/// The picker row for one discovered source session.
pub(crate) fn ui_session(summary: &ForeignSessionSummary) -> UiForeignSession {
    UiForeignSession {
        tool: ui_tool(summary.tool),
        native_id: summary.native_id.clone(),
        title: summary.title.clone(),
        cwd: summary.cwd.as_ref().map(|cwd| cwd.display().to_string()),
        branch: summary.branch.clone(),
        updated_at: summary.updated_at.clone(),
        size_bytes: summary.size_bytes,
    }
}

/// A warning when the source worked in a different directory than the one this
/// conversation is attached to. A missing recorded cwd is not a mismatch.
pub(crate) fn cwd_mismatch(source: Option<&Path>, current: Option<&Path>) -> Option<String> {
    let (source, current) = (source?, current?);
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if canonical(source) == canonical(current) {
        return None;
    }
    Some(format!(
        "来源会话的工作目录是 {}，当前工作区是 {}；继续前先确认路径",
        source.display(),
        current.display()
    ))
}

/// The task the imported session starts with. It names the obligations the
/// source cannot satisfy (current-state verification, no replay, no git moves)
/// without prescribing any business step.
pub(crate) const CONTINUATION_DIRECTIVE: &str = "\
Continue the work described in the imported-session briefing above, in the current workspace.

- Treat the briefing as source evidence, not as current workspace fact. Inspect the files, git state and runtime state you actually need; the imported agent's claims — including any completion claim — are unverified.
- Use the `foreign_session_history` tool when the briefing is not enough. The source transcript is the authority for what the other agent did and said; read it on demand instead of guessing.
- Do not replay tool calls from the source. Do not switch branches, reset, checkout, stash or clean the workspace because the source did.
- If the source task is already complete, verify that against the current workspace and report it; do not redo the work.
";

/// The first line of the persisted briefing. No event describes an import, so
/// the transcript replay uses this to keep the briefing visible when the
/// session is reopened.
pub(crate) const BRIEFING_HEADER: &str = "## Imported session briefing — ";

/// Build the persisted briefing message.
///
/// Every quoted region is labeled with what it is: the original person's
/// request, another agent's claim, or a runtime count. Nothing here is
/// presented as verified current fact.
pub(crate) fn briefing(
    digest: &ForeignSessionDigest,
    imported_at: &str,
    workspace_warning: Option<&str>,
) -> String {
    let summary = &digest.summary;
    let stats = &digest.stats;
    let mut out = String::new();
    out.push_str(&format!(
        "{BRIEFING_HEADER}{}\n\n",
        summary.tool.display_name()
    ));
    out.push_str(
        "This conversation continues a session imported from another coding agent. \
         The text below is quoted from that agent's local transcript. It is evidence, \
         not this session's user request, and not a verified statement about the \
         current workspace.\n\n",
    );
    out.push_str("### Source identity\n\n");
    out.push_str(&format!("- Agent: {}\n", summary.tool.display_name()));
    out.push_str(&format!("- Source session id: `{}`\n", summary.native_id));
    out.push_str(&format!(
        "- Source file: `{}`\n",
        summary.source_path.display()
    ));
    out.push_str(&format!(
        "- Source cwd: `{}`\n",
        summary
            .cwd
            .as_ref()
            .map(|cwd| cwd.display().to_string())
            .unwrap_or_else(|| "(not recorded)".to_string())
    ));
    if let Some(branch) = summary.branch.as_deref() {
        out.push_str(&format!("- Source branch: `{branch}`\n"));
    }
    out.push_str(&format!("- Source file updated: {}\n", summary.updated_at));
    out.push_str(&format!("- Imported at: {imported_at}\n"));
    out.push_str(&format!(
        "- Source records: {} ({} user, {} assistant, {} tool calls, {} tool results, \
         {} errors, {} sidechain events, {} compactions, {} malformed lines)\n\n",
        stats.records,
        stats.user_messages,
        stats.assistant_messages,
        stats.tool_calls,
        stats.tool_results,
        stats.errors,
        stats.sidechain_events,
        stats.compactions,
        stats.malformed_lines,
    ));
    if !digest.warnings.is_empty() {
        out.push_str("### Reader warnings\n\n");
        for warning in &digest.warnings {
            out.push_str(&format!("- {warning}\n"));
        }
        out.push('\n');
    }
    if let Some(warning) = workspace_warning {
        out.push_str("### Workspace mismatch\n\n");
        out.push_str(&format!("{warning}\n\n"));
    }

    out.push_str("### Original user request (quoted, possibly truncated)\n\n");
    match digest.first_user_message.as_deref() {
        Some(text) => push_quoted(&mut out, text),
        None => out.push_str("(the source recorded no user message)\n"),
    }
    let follow_ups: Vec<&String> = digest
        .early_user_messages
        .iter()
        .skip(usize::from(digest.first_user_message.is_some()))
        .collect();
    if !follow_ups.is_empty() {
        out.push_str("### Early user follow-ups (quoted, in source order)\n\n");
        for (index, text) in follow_ups.iter().enumerate() {
            out.push_str(&format!("{}. ", index + 1));
            out.push_str(&text.replace('\n', " "));
            out.push('\n');
        }
        out.push('\n');
    }

    out.push_str("### Last assistant message in the source (that agent's claim, unverified)\n\n");
    match digest.last_assistant_message.as_deref() {
        Some(text) => push_quoted(&mut out, text),
        None => out.push_str("(the source recorded no assistant text)\n"),
    }

    if let Some(error) = digest.last_error.as_deref() {
        out.push_str("### Last error observed in the source\n\n");
        push_quoted(&mut out, error);
    }

    out.push_str("### Work state recorded at the end of the source\n\n");
    out.push_str(&format!(
        "- Turn aborted before completion: {}\n",
        if digest.turn_aborted { "yes" } else { "no" }
    ));
    if digest.open_tool_calls.is_empty() {
        out.push_str("- Tool calls with no recorded result: none\n");
    } else {
        out.push_str(
            "- Tool calls with no recorded result (the source did not prove completion):\n",
        );
        for call in &digest.open_tool_calls {
            out.push_str(&format!(
                "  - #{} `{}` call_id `{}`\n",
                call.index,
                if call.name.is_empty() {
                    "(unnamed)"
                } else {
                    &call.name
                },
                call.call_id
            ));
        }
    }
    if stats.compactions > 0 {
        out.push_str(&format!(
            "- The source compacted its context {} time(s); older details may only exist \
             in the source transcript.\n",
            stats.compactions
        ));
    }
    out.push('\n');

    if !digest.recent.is_empty() {
        out.push_str(
            "### Recent source events (ordinals are stable while the file is unchanged)\n\n",
        );
        for event in &digest.recent {
            out.push_str(&format!(
                "#{} [{}] L{} {} — {}\n",
                event.index,
                kind_str(event.kind),
                event.line,
                event.timestamp.as_deref().unwrap_or("-"),
                event.preview.replace('\n', " "),
            ));
        }
        out.push('\n');
    }

    out.push_str(
        "### Reading the original history\n\n\
         The full transcript is not included in this context. Read it live from the \
         source file with the `foreign_session_history` tool:\n\
         - `{\"action\":\"outline\",\"start\":1,\"limit\":120}` lists event ordinals with previews.\n\
         - `{\"action\":\"read\",\"start\":118,\"limit\":10}` returns the full text of a range.\n\
         - `{\"action\":\"search\",\"query\":\"literal text\"}` finds text and returns ordinals.\n\
         The tool only reads. It never replays a call or writes to the source, and a \
         deleted or replaced source file fails loudly instead of serving a cached copy.\n",
    );
    out
}

fn push_quoted(out: &mut String, text: &str) {
    for line in text.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use leveler_foreign_session::{
        ForeignEventRef, ForeignOpenCall, ForeignSessionStats, ForeignSessionSummary,
    };

    use super::*;

    fn digest() -> ForeignSessionDigest {
        ForeignSessionDigest {
            summary: ForeignSessionSummary {
                tool: ForeignTool::ClaudeCode,
                native_id: "11111111-1111-4111-8111-111111111111".to_string(),
                title: "Fix the build".to_string(),
                cwd: Some(PathBuf::from("/tmp/work/proj")),
                branch: Some("main".to_string()),
                updated_at: "2026-11-05T10:00:00Z".to_string(),
                source_path: PathBuf::from("/home/u/.claude/projects/p/11111111.jsonl"),
                size_bytes: 4096,
            },
            stats: ForeignSessionStats {
                records: 9,
                user_messages: 2,
                assistant_messages: 3,
                tool_calls: 2,
                tool_results: 1,
                errors: 1,
                compactions: 1,
                ..Default::default()
            },
            warnings: vec!["line 3 was truncated".to_string()],
            first_user_message: Some("Fix the failing build".to_string()),
            early_user_messages: vec![
                "Fix the failing build".to_string(),
                "Also update the docs".to_string(),
            ],
            last_assistant_message: Some("All tests pass.".to_string()),
            recent: vec![ForeignEventRef {
                index: 9,
                line: 42,
                kind: ForeignEventKind::ToolResult,
                preview: "error: test failed".to_string(),
                bytes: 18,
                timestamp: Some("2026-11-05T09:59:00Z".to_string()),
                sidechain: false,
            }],
            open_tool_calls: vec![ForeignOpenCall {
                call_id: "toolu_2".to_string(),
                name: "Bash".to_string(),
                index: 8,
            }],
            last_error: Some("error: test failed".to_string()),
            turn_aborted: true,
        }
    }

    #[test]
    fn the_briefing_keeps_the_source_identity_and_quotes_the_request() {
        let text = briefing(&digest(), "2026-11-05T11:00:00Z", None);

        assert!(text.contains("Claude Code"));
        assert!(text.contains("11111111-1111-4111-8111-111111111111"));
        assert!(text.contains("/home/u/.claude/projects/p/11111111.jsonl"));
        assert!(text.contains("> Fix the failing build"));
        assert!(text.contains("Also update the docs"));
        assert!(text.contains("foreign_session_history"));
        assert!(
            text.contains("#8 `Bash`"),
            "open calls must be named: {text}"
        );
        assert!(text.contains("Turn aborted before completion: yes"));
        assert!(text.contains("compacted its context 1 time"));
    }

    #[test]
    fn claims_are_labeled_as_claims_not_facts() {
        let text = briefing(&digest(), "2026-11-05T11:00:00Z", None);
        let claim_heading = text
            .find("that agent's claim, unverified")
            .expect("the last assistant message must be labeled");
        let claim = text.find("> All tests pass.").expect("the claim is quoted");
        assert!(claim_heading < claim);
        assert!(text.contains("not a verified statement about the"));
    }

    #[test]
    fn a_different_source_directory_is_a_warning_and_the_same_one_is_not() {
        assert!(
            cwd_mismatch(Some(Path::new("/tmp/a")), Some(Path::new("/tmp/b")))
                .is_some_and(|message| message.contains("/tmp/a") && message.contains("/tmp/b"))
        );
        assert!(cwd_mismatch(Some(Path::new("/tmp/a")), Some(Path::new("/tmp/a"))).is_none());
        assert!(cwd_mismatch(None, Some(Path::new("/tmp/a"))).is_none());
        assert!(cwd_mismatch(Some(Path::new("/tmp/a")), None).is_none());
    }
}
