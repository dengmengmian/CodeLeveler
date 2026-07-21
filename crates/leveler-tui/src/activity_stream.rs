//! Conversation activity stream: filter Internal Trace and aggregate exploration.
//!
//! Product surface, not a tool trace:
//! - **Internal Trace** (grep/search/read_file/list/symbol): hidden per-call;
//!   consecutive successes collapse into one Activity Summary.
//! - **User-visible**: Important edits/runs and Failed tools stay one line each.
//! - Expanded groups can reveal trace details under the summary.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::i18n::{Locale, UiText};
use crate::render::truncate_display;
use crate::theme::Theme;
use crate::tool_cell::{tool_action_label_for, tool_summary_pub};
use crate::tool_taxonomy::{ActivityVisibility, ToolKind, activity_visibility};
use crate::transcript::{ToolCallBlock, ToolGroupBlock, ToolStatus};

/// Render a tool group for the Conversation activity stream.
pub(crate) fn render_group(
    group: &ToolGroupBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // A concurrent read-only batch shows as one "parallel" header so the user
    // sees these calls ran together rather than one after another.
    let parallel_n = group.calls.iter().filter(|c| c.parallel).count();
    if parallel_n >= 2 {
        let label = match locale {
            Locale::Zh => format!("⇉ {parallel_n} 个工具并发执行"),
            Locale::En => format!("⇉ {parallel_n} tools in parallel"),
        };
        out.push(Line::from(Span::styled(
            truncate_display(&label, width),
            Style::default().fg(theme.muted),
        )));
    }
    let units = plan_units(&group.calls);
    for unit in units {
        match unit {
            StreamUnit::Single(call) => {
                // Failed tools always keep a one-line summary on the activity
                // row (status + first error line). Multi-line dumps only when
                // the group is expanded — never flood on collapse.
                out.push(single_activity_line(call, theme, width, locale, t));
                if !group.expanded {
                    continue;
                }
                if activity_visibility(&call.name, &call.arguments) == ActivityVisibility::Silent
                    && call.status != ToolStatus::Failed
                {
                    continue;
                }
                append_call_detail(call, theme, width, true, locale, t, &mut out);
            }
            StreamUnit::Aggregate {
                reads,
                searches,
                symbols,
                other,
                samples,
            } => {
                out.push(aggregate_line(
                    theme, width, t, reads, searches, symbols, other,
                ));
                // Details: only when the group is expanded (user-requested).
                if group.expanded {
                    for call in samples {
                        out.push(detail_trace_line(call, theme, width, locale));
                    }
                }
            }
        }
    }
    out
}

/// Whether a completed/running call may appear as its own Conversation line.
pub(crate) fn is_conversation_visible(call: &ToolCallBlock) -> bool {
    // Internal Trace successes never appear as per-path lines (only in aggregates).
    if is_internal_trace(call) && call.status == ToolStatus::Ok {
        return false;
    }
    // Running Trace is noise while the model thinks.
    if is_internal_trace(call) && call.status == ToolStatus::Running {
        return false;
    }
    match activity_visibility(&call.name, &call.arguments) {
        ActivityVisibility::Silent => call.status == ToolStatus::Failed,
        ActivityVisibility::Normal | ActivityVisibility::Important => true,
    }
}

#[derive(Debug)]
enum StreamUnit<'a> {
    Single(&'a ToolCallBlock),
    Aggregate {
        reads: usize,
        searches: usize,
        symbols: usize,
        other: usize,
        samples: Vec<&'a ToolCallBlock>,
    },
}

fn plan_units(calls: &[ToolCallBlock]) -> Vec<StreamUnit<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < calls.len() {
        let call = &calls[i];
        // Batch consecutive successful Internal Trace tools into one summary.
        if is_trace_success(call) {
            let mut reads = 0usize;
            let mut searches = 0usize;
            let mut symbols = 0usize;
            let mut other = 0usize;
            let mut samples = Vec::new();
            while i < calls.len() && is_trace_success(&calls[i]) {
                tally(
                    &calls[i],
                    &mut reads,
                    &mut searches,
                    &mut symbols,
                    &mut other,
                );
                if samples.len() < 8 {
                    samples.push(&calls[i]);
                }
                i += 1;
            }
            out.push(StreamUnit::Aggregate {
                reads,
                searches,
                symbols,
                other,
                samples,
            });
            continue;
        }
        if !is_conversation_visible(call) {
            i += 1;
            continue;
        }
        out.push(StreamUnit::Single(call));
        i += 1;
    }
    out
}

fn is_trace_success(call: &ToolCallBlock) -> bool {
    // Only real exploration tools enter the summary batch — demoted shell probes
    // (`ls`/`find`) stay fully Silent and never produce a summary line alone.
    call.status == ToolStatus::Ok && is_summary_exploration(call)
}

/// Tools whose successful runs collapse into an Activity Summary line.
fn is_summary_exploration(call: &ToolCallBlock) -> bool {
    match crate::tool_taxonomy::lookup(&call.name).map(|e| e.kind) {
        Some(
            ToolKind::Read
            | ToolKind::Search
            | ToolKind::Lsp
            | ToolKind::ListDir
            | ToolKind::WebSearch
            | ToolKind::Media,
        ) => true,
        _ => matches!(
            call.name.as_str(),
            "read_file"
                | "list_files"
                | "grep"
                | "repository_search"
                | "find_symbol"
                | "read_symbol"
                | "find_references"
                | "git_status"
                | "git_diff"
                | "web_search"
                | "view_image"
        ),
    }
}

/// Internal Trace: exploration tools + demoted Silent probes — not user decisions.
fn is_internal_trace(call: &ToolCallBlock) -> bool {
    is_summary_exploration(call)
        || activity_visibility(&call.name, &call.arguments) == ActivityVisibility::Silent
}

fn tally(
    call: &ToolCallBlock,
    reads: &mut usize,
    searches: &mut usize,
    symbols: &mut usize,
    other: &mut usize,
) {
    match crate::tool_taxonomy::lookup(&call.name).map(|e| e.kind) {
        Some(ToolKind::Read) => *reads += 1,
        Some(ToolKind::Search | ToolKind::WebSearch) => *searches += 1,
        Some(ToolKind::Lsp) => *symbols += 1,
        _ if call.name == "read_file" => *reads += 1,
        _ if matches!(
            call.name.as_str(),
            "grep" | "repository_search" | "web_search" | "git_diff" | "git_status"
        ) =>
        {
            *searches += 1
        }
        _ if matches!(
            call.name.as_str(),
            "find_symbol" | "read_symbol" | "find_references"
        ) =>
        {
            *symbols += 1
        }
        _ => *other += 1,
    }
}

fn single_activity_line(
    call: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
) -> Line<'static> {
    let (glyph, color) = match call.status {
        ToolStatus::Running => ("⟳", theme.accent),
        ToolStatus::Ok => ("✓", theme.success),
        ToolStatus::Failed => ("✗", theme.error),
    };
    let action = if call.name == "task" {
        t.unsupported_task_action.to_string()
    } else {
        tool_action_label_for(&call.name, locale)
    };
    let summary = strip_inline_md(&tool_summary_pub(&call.name, &call.arguments));
    let mut body = if summary.is_empty() || summary == "{}" {
        action
    } else {
        format!("{action}  {summary}")
    };
    // Collapsed failure: keep the error visible on the same line.
    if call.status == ToolStatus::Failed
        && let Some(err) = failed_one_line_summary(call, t)
    {
        body.push_str(" — ");
        body.push_str(&err);
    }
    let dur = call
        .duration_ms
        .filter(|ms| *ms >= 100)
        .map(|ms| format!("  {:.1}s", ms as f64 / 1000.0))
        .unwrap_or_default();
    let text = format!("{glyph} {body}{dur}");
    let style = if activity_visibility(&call.name, &call.arguments) == ActivityVisibility::Important
    {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color)
    };
    Line::from(Span::styled(truncate_display(&text, width), style))
}

/// First non-empty preview line for a failed tool (honest one-line error).
fn failed_one_line_summary(call: &ToolCallBlock, t: &UiText) -> Option<String> {
    // Unknown `task` tool: show the actionable spawn_agent hint, not raw JSON.
    if call.name == "task" {
        let preview = call.preview.as_deref().unwrap_or("");
        if preview.contains("unknown tool") || preview.contains("spawn_agent") {
            return Some(t.unsupported_task_hint.to_string());
        }
    }
    let preview = call.preview.as_deref()?.trim();
    if preview.is_empty() {
        return None;
    }
    let first = preview
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(preview);
    Some(truncate_display(first, 72))
}

fn detail_trace_line(
    call: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
) -> Line<'static> {
    let action = tool_action_label_for(&call.name, locale);
    let summary = strip_inline_md(&tool_summary_pub(&call.name, &call.arguments));
    let body = if summary.is_empty() || summary == "{}" {
        action
    } else {
        format!("{action}  {summary}")
    };
    Line::from(Span::styled(
        truncate_display(&format!("  · {body}"), width),
        Style::default().fg(theme.muted),
    ))
}

fn aggregate_line(
    theme: &Theme,
    width: usize,
    t: &UiText,
    reads: usize,
    searches: usize,
    symbols: usize,
    other: usize,
) -> Line<'static> {
    let total = reads + searches + symbols + other;
    // Search-heavy bursts → "found N related locations".
    let body = if searches > 0 && searches >= reads && searches >= symbols {
        t.activity_found_locations
            .replace("{}", &total.max(searches).to_string())
    } else if total == 0 {
        t.activity_explored.to_string()
    } else {
        let mut parts = Vec::new();
        if reads > 0 {
            parts.push(t.activity_reads.replace("{}", &reads.to_string()));
        }
        if searches > 0 {
            parts.push(t.activity_searches.replace("{}", &searches.to_string()));
        }
        if symbols > 0 {
            parts.push(t.activity_symbols.replace("{}", &symbols.to_string()));
        }
        if other > 0 {
            parts.push(other.to_string());
        }
        format!("{}（{}）", t.activity_explored, parts.join(" · "))
    };
    let text = format!("✓ {body}");
    Line::from(Span::styled(
        truncate_display(&text, width),
        Style::default().fg(theme.success),
    ))
}

fn strip_inline_md(s: &str) -> String {
    s.replace("**", "").replace('`', "")
}

fn append_call_detail(
    call: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    expanded: bool,
    locale: Locale,
    t: &UiText,
    out: &mut Vec<Line<'static>>,
) {
    if matches!(call.name.as_str(), "run_command" | "shell_command")
        && call.status == ToolStatus::Ok
        && expanded
    {
        let mut lines = crate::tool_result::result_lines(call, theme, width, true, locale, t);
        if !lines.is_empty() {
            lines.remove(0);
        }
        out.extend(lines);
        return;
    }
    let mut detail = Vec::new();
    crate::tool_cell::tool_lines(
        call,
        theme,
        width.saturating_sub(2).max(1),
        expanded,
        t,
        &mut detail,
    );
    out.extend(detail.into_iter().skip(1));
}

/// Plain-text lines for tests (no styling).
#[cfg(test)]
pub(crate) fn render_group_text(
    group: &ToolGroupBlock,
    width: usize,
    locale: Locale,
) -> Vec<String> {
    render_group(group, &Theme::no_color(), width, locale, locale.text())
        .into_iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::ToolCallId;

    fn call(name: &str, args: &str, status: ToolStatus) -> ToolCallBlock {
        ToolCallBlock {
            id: ToolCallId::new(format!("{name}-{}", args.len())),
            name: name.into(),
            arguments: args.into(),
            status,
            preview: Some("ok".into()),
            duration_ms: Some(5),
            parallel: false,
        }
    }

    fn group(calls: Vec<ToolCallBlock>) -> ToolGroupBlock {
        ToolGroupBlock {
            calls,
            open: false,
            expanded: false,
        }
    }

    fn parallel_call(name: &str, args: &str) -> ToolCallBlock {
        let mut c = call(name, args, ToolStatus::Ok);
        c.parallel = true;
        c
    }

    #[test]
    fn parallel_batch_gets_a_concurrency_header() {
        let g = group(vec![
            parallel_call("read_file", r#"{"path":"a.rs"}"#),
            parallel_call("grep", r#"{"pattern":"x"}"#),
            parallel_call("read_file", r#"{"path":"b.rs"}"#),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines.iter().any(|l| l.contains("并发")),
            "a ≥2-call parallel batch must show a concurrency header: {lines:?}"
        );
    }

    #[test]
    fn a_single_parallel_call_gets_no_concurrency_header() {
        // One parallel-safe call is not a batch; no header (needs ≥2).
        let g = group(vec![
            parallel_call("read_file", r#"{"path":"a.rs"}"#),
            call("apply_patch", "{}", ToolStatus::Ok),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            !lines.iter().any(|l| l.contains("并发")),
            "one parallel call is not a batch: {lines:?}"
        );
    }

    #[test]
    fn silent_list_files_hidden_from_conversation() {
        let g = group(vec![
            call("list_files", r#"{"path":"."}"#, ToolStatus::Ok),
            call("list_files", r#"{"path":"cmd"}"#, ToolStatus::Ok),
        ]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        // list_files is Silent/Trace — still summarized as exploration, not path dump.
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("PROJECT_RULES") || l == "✓ ."),
            "{lines:?}"
        );
    }

    #[test]
    fn silent_shell_ls_hidden() {
        let g = group(vec![call(
            "run_command",
            r#"{"program":"ls","args":["-la"]}"#,
            ToolStatus::Ok,
        )]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert!(lines.is_empty(), "ls probe must be silent: {lines:?}");
    }

    #[test]
    fn update_goal_success_hidden_from_conversation() {
        let g = group(vec![call(
            "update_goal",
            r#"{"status":"complete","summary":"用户询问\"这是什么项目\"。通过阅读 README.md 给出了项目介绍"}"#,
            ToolStatus::Ok,
        )]);
        let lines = render_group_text(&g, 120, Locale::Zh);
        assert!(
            lines.is_empty(),
            "successful update_goal is bookkeeping, not a product row: {lines:?}"
        );
    }

    #[test]
    fn update_goal_blocked_stays_visible() {
        // blocked resolves with tool ok=true; still user-facing (stuck).
        let g = group(vec![call(
            "update_goal",
            r#"{"status":"blocked","summary":"缺 API key，无法继续"}"#,
            ToolStatus::Ok,
        )]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("目标收尾") && lines[0].contains("受阻"),
            "{lines:?}"
        );
    }

    #[test]
    fn consecutive_reads_and_greps_aggregate_without_paths() {
        let g = group(vec![
            call(
                "read_file",
                r#"{"path":"PROJECT_RULES.md"}"#,
                ToolStatus::Ok,
            ),
            call("grep", r#"{"pattern":"dist"}"#, ToolStatus::Ok),
            call("read_file", r#"{"path":"Makefile"}"#, ToolStatus::Ok),
            call(
                "grep",
                r#"{"pattern":"build","path":"cmd"}"#,
                ToolStatus::Ok,
            ),
        ]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("PROJECT_RULES.md") || l.contains("Makefile")),
            "paths must not list out: {lines:?}"
        );
        assert!(
            lines[0].contains("找到") || lines[0].contains("检查") || lines[0].contains("搜索"),
            "{lines:?}"
        );
    }

    #[test]
    fn single_read_is_summary_not_path_line() {
        let g = group(vec![call(
            "read_file",
            r#"{"path":"src/auth.go"}"#,
            ToolStatus::Ok,
        )]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            !lines[0].contains("auth.go") || lines[0].contains("检查") || lines[0].contains("读取"),
            "prefer summary over path dump: {lines:?}"
        );
        // Must not look like a raw path-only activity line.
        assert!(!lines[0].ends_with("auth.go"), "{lines:?}");
    }

    #[test]
    fn important_edit_always_shown() {
        let g = group(vec![
            call("list_files", r#"{"path":"."}"#, ToolStatus::Ok),
            call(
                "apply_patch",
                r#"{"patch":"*** Begin Patch\n*** Update File: internal/admin/web/web.go\n*** End Patch"}"#,
                ToolStatus::Ok,
            ),
        ]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("编辑") && l.contains("web.go")),
            "{lines:?}"
        );
    }

    #[test]
    fn failed_silent_tool_still_surfaces() {
        let g = group(vec![call(
            "list_files",
            r#"{"path":"missing"}"#,
            ToolStatus::Failed,
        )]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert!(!lines.is_empty() && lines[0].starts_with('✗'), "{lines:?}");
    }

    #[test]
    fn collapsed_failed_tool_keeps_one_line_error_summary() {
        let mut c = call(
            "run_command",
            r#"{"program":"cargo","args":["test"]}"#,
            ToolStatus::Failed,
        );
        c.preview =
            Some("error: no such command\nlong help dump line 2\nlong help dump line 3".into());
        let g = group(vec![c]);
        assert!(!g.expanded);
        let lines = render_group_text(&g, 120, Locale::Zh);
        assert_eq!(
            lines.len(),
            1,
            "collapsed failed tool must not dump multi-line body: {lines:?}"
        );
        assert!(lines[0].starts_with('✗'), "{lines:?}");
        assert!(
            lines[0].contains("error: no such command"),
            "one-line error summary: {lines:?}"
        );
        assert!(
            !lines[0].contains("long help dump line 2"),
            "must not include rest of dump on the summary line: {lines:?}"
        );
    }

    #[test]
    fn expanded_failed_tool_can_show_detail_lines() {
        let mut c = call(
            "run_command",
            r#"{"program":"cargo","args":["test"]}"#,
            ToolStatus::Failed,
        );
        c.preview = Some("error: boom\nextra context line".into());
        let mut g = group(vec![c]);
        g.expanded = true;
        let lines = render_group_text(&g, 120, Locale::Zh);
        assert!(
            lines.len() >= 2,
            "expanded should reveal detail under the summary: {lines:?}"
        );
    }

    #[test]
    fn mix_of_exploration_and_run_keeps_run_separate() {
        let g = group(vec![
            call("read_file", r#"{"path":"a.go"}"#, ToolStatus::Ok),
            call("read_file", r#"{"path":"b.go"}"#, ToolStatus::Ok),
            call(
                "run_command",
                r#"{"program":"cargo","args":["test"]}"#,
                ToolStatus::Ok,
            ),
        ]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("检查") || lines[0].contains("读取") || lines[0].contains("找到"),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("执行") && lines[1].contains("cargo"),
            "{lines:?}"
        );
    }

    #[test]
    fn shell_command_activity_hides_json_and_cd_prefix() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/example".into());
        let args =
            format!(r#"{{"cmd":"cd {home}/Develop/app/codeleveler && cargo test --workspace"}}"#);
        let g = group(vec![call("shell_command", &args, ToolStatus::Ok)]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("cargo test --workspace"),
            "expected command body: {lines:?}"
        );
        assert!(
            !lines[0].contains("cmd") && !lines[0].contains('{'),
            "must not leak JSON args: {lines:?}"
        );
        assert!(
            !lines[0].contains(&home) && !lines[0].contains("Develop/app"),
            "must not leak absolute cwd: {lines:?}"
        );
    }

    #[test]
    fn expanded_group_reveals_trace_details() {
        let mut g = group(vec![
            call("grep", r#"{"pattern":"dist"}"#, ToolStatus::Ok),
            call("grep", r#"{"pattern":"build"}"#, ToolStatus::Ok),
        ]);
        g.expanded = true;
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert!(lines.len() >= 2, "summary + detail lines: {lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("dist") || l.contains("搜索")),
            "{lines:?}"
        );
    }
}
