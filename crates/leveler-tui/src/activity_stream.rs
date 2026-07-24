//! Conversation activity stream: per-call two-line tool units.
//!
//! Product surface, not a tool trace:
//! - **Silent** tools (ls/find probes, goal bookkeeping): hidden per-call.
//! - **User-visible**: every Normal/Important call renders its own unit —
//!   a head row (status glyph + action + inline target) and a `└` result
//!   line. No whole-line tinting: only the glyph carries a status color.
//! - Consecutive same-file patches merge into one edit node with a combined
//!   hunk stat line and folded diff rows; consecutive identical failures
//!   merge into one unit with a `×N` retry count.
//! - Expanded groups reveal output details under the unit.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::i18n::{Locale, UiText};
use crate::render::truncate_display;
use crate::theme::Theme;
use crate::tool_cell::{tool_action_label_for, tool_summary_pub};
use crate::tool_taxonomy::{ActivityVisibility, activity_visibility};
use crate::transcript::{ToolCallBlock, ToolGroupBlock, ToolStatus};

/// Render a tool group for the Conversation activity stream.
pub(crate) fn render_group(
    group: &ToolGroupBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    now_elapsed_secs: u64,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // A concurrent batch gets one quiet dim header so the user sees these
    // calls ran together rather than one after another.
    let parallel_n = group.calls.iter().filter(|c| c.parallel).count();
    if parallel_n >= 2 {
        let label = t.parallel_header.replace("{}", &parallel_n.to_string());
        out.push(Line::from(Span::styled(
            truncate_display(&label, width),
            Style::default().fg(theme.dim),
        )));
    }
    for unit in plan_units(&group.calls) {
        match unit {
            StreamUnit::Single(call) => {
                out.extend(unit_lines(
                    call,
                    theme,
                    width,
                    locale,
                    t,
                    group.expanded,
                    1,
                    None,
                    now_elapsed_secs,
                ));
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
            StreamUnit::EditMerge(calls) => {
                out.extend(edit_unit_lines(
                    &calls,
                    theme,
                    width,
                    locale,
                    t,
                    group.expanded,
                ));
            }
            StreamUnit::FailMerge(calls) => {
                let total_ms: u64 = calls.iter().filter_map(|c| c.duration_ms).sum();
                out.extend(unit_lines(
                    calls[0],
                    theme,
                    width,
                    locale,
                    t,
                    group.expanded,
                    calls.len(),
                    (total_ms >= 100).then_some(total_ms),
                    // FailMerge is a finished failure group, never live-running.
                    0,
                ));
                if group.expanded {
                    append_call_detail(calls[0], theme, width, true, locale, t, &mut out);
                }
            }
        }
    }
    out
}

/// Whether a completed/running call may appear as its own Conversation unit.
pub(crate) fn is_conversation_visible(call: &ToolCallBlock) -> bool {
    // Silent tools (exploration probes, goal bookkeeping) stay out unless they
    // failed — a failure is always user-facing.
    match activity_visibility(&call.name, &call.arguments) {
        ActivityVisibility::Silent => call.status == ToolStatus::Failed,
        ActivityVisibility::Normal | ActivityVisibility::Important => true,
    }
}

enum StreamUnit<'a> {
    Single(&'a ToolCallBlock),
    EditMerge(Vec<&'a ToolCallBlock>),
    FailMerge(Vec<&'a ToolCallBlock>),
}

fn plan_units(calls: &[ToolCallBlock]) -> Vec<StreamUnit<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < calls.len() {
        let call = &calls[i];
        if !is_conversation_visible(call) {
            i += 1;
            continue;
        }
        if mergeable_edit(call) {
            // Merge render-adjacent patches to the same file (hidden probes in
            // between do not break adjacency; a visible different tool does).
            let key = edit_merge_key(call);
            let mut group = vec![call];
            let mut j = i + 1;
            while j < calls.len() {
                let next = &calls[j];
                if !is_conversation_visible(next) {
                    j += 1;
                    continue;
                }
                if mergeable_edit(next) && edit_merge_key(next) == key {
                    group.push(next);
                    j += 1;
                } else {
                    break;
                }
            }
            out.push(StreamUnit::EditMerge(group));
            i = j;
            continue;
        }
        if call.status == ToolStatus::Failed {
            // Merge render-adjacent identical failures (same tool, same args —
            // the model retrying the exact same call). Nine lines of repeated
            // error collapse into one unit with a `×N` retry count.
            let mut group = vec![call];
            let mut j = i + 1;
            while j < calls.len() {
                let next = &calls[j];
                if !is_conversation_visible(next) {
                    j += 1;
                    continue;
                }
                if next.status == ToolStatus::Failed
                    && next.name == call.name
                    && next.arguments == call.arguments
                {
                    group.push(next);
                    j += 1;
                } else {
                    break;
                }
            }
            if group.len() > 1 {
                out.push(StreamUnit::FailMerge(group));
                i = j;
                continue;
            }
        }
        out.push(StreamUnit::Single(call));
        i += 1;
    }
    out
}

/// A non-failed edit with a real file target can merge with its neighbors.
fn mergeable_edit(call: &ToolCallBlock) -> bool {
    if call.status == ToolStatus::Failed {
        return false;
    }
    match call.name.as_str() {
        "apply_patch" => !crate::tool_cell::patch_files_key(&call.arguments).is_empty(),
        "replace" => {
            // Same-file consecutive replaces merge like patches.
            serde_json::from_str::<serde_json::Value>(&call.arguments)
                .ok()
                .and_then(|v| v.get("path")?.as_str().map(str::to_string))
                .is_some_and(|p| !p.is_empty())
        }
        _ => false,
    }
}

/// Merge identity: same touched files, same status. Different files or a
/// status change (running → ok) never merge.
fn edit_merge_key(call: &ToolCallBlock) -> (String, ToolStatus) {
    let key = if call.name == "replace" {
        serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|v| v.get("path")?.as_str().map(|p| p.to_string()))
            .unwrap_or_default()
    } else {
        crate::tool_cell::patch_files_key(&call.arguments)
    };
    (key, call.status)
}

fn is_shell_call(call: &ToolCallBlock) -> bool {
    matches!(call.name.as_str(), "run_command" | "shell_command")
}

/// One visible tool call as a two-line unit:
/// `✓ 动作  参数 · 0.4s` / `  └ 结果`.
///
/// `repeat` > 1 marks a FailMerge unit: identical consecutive failures shown
/// once with a `×N` suffix on the result row. `duration_override` lets the
/// merged unit show the summed duration instead of the first call's.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn unit_lines(
    call: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    expanded: bool,
    repeat: usize,
    duration_override: Option<u64>,
    now_elapsed_secs: u64,
) -> Vec<Line<'static>> {
    // Plan/goal guard rejections carry internal English validation text for the
    // model — show a warning glyph and a localized note instead. Other failures
    // are real errors: the result row shows the first error line.
    let guard_denial = call.status == ToolStatus::Failed
        && matches!(call.name.as_str(), "update_plan" | "update_goal");
    let (glyph, glyph_color) = if guard_denial {
        ("⚠", theme.warning)
    } else {
        status_glyph(call.status, theme)
    };
    let action = if call.name == "task" {
        t.unsupported_task_action.to_string()
    } else {
        tool_action_label_for(&call.name, locale)
    };

    // Trailing status marker: a running call shows its live elapsed time so a
    // long command (e.g. `go test`) is visibly working rather than a static
    // block; a finished call shows its final duration.
    let tail = match call.status {
        ToolStatus::Running => {
            let secs = now_elapsed_secs.saturating_sub(call.started_elapsed_secs);
            if secs > 0 {
                format!(" · {}", crate::status_line::fmt_elapsed(secs))
            } else {
                " …".to_string()
            }
        }
        _ => duration_override
            .or(call.duration_ms)
            .filter(|ms| *ms >= 100)
            .map(|ms| format!(" · {:.1}s", ms as f64 / 1000.0))
            .unwrap_or_default(),
    };

    let mut head = vec![
        Span::styled(format!("{glyph} "), Style::default().fg(glyph_color)),
        Span::styled(action.clone(), Style::default().fg(theme.tool)),
    ];

    // The head carries the one-line target inline (text color; `$` highlighted
    // for shell). Width budget reserves the tail plus a small margin.
    let mut summary = strip_inline_md(&tool_summary_pub(&call.name, &call.arguments, t));
    // A failed patch whose arguments can't be parsed falls back to a generic
    // placeholder ("补丁"); recover the real target from the error preview.
    if call.status == ToolStatus::Failed
        && call.name == "apply_patch"
        && (summary.is_empty() || summary == t.tool_label_patch)
        && let Some(file) = failed_patch_target(call.preview.as_deref())
    {
        summary = file;
    }
    if !summary.is_empty() && summary != "{}" {
        let shell = is_shell_call(call);
        let used = 2 + UnicodeWidthStr::width(action.as_str()) + 2 + usize::from(shell) * 2;
        let avail = width
            .saturating_sub(used + UnicodeWidthStr::width(tail.as_str()) + 8)
            .max(8);
        head.push(Span::raw("  "));
        if shell {
            head.push(Span::styled("$ ", Style::default().fg(theme.shell_prompt)));
        }
        head.push(Span::styled(
            truncate_display(&summary, avail),
            Style::default().fg(theme.text),
        ));
    }
    if !tail.is_empty() {
        head.push(Span::styled(tail, Style::default().fg(theme.dim)));
    }
    let mut out = vec![Line::from(head)];

    // Line 2: result summary.
    out.extend(result_lines_for(
        call,
        theme,
        width,
        t,
        expanded,
        guard_denial,
        repeat,
    ));
    out
}

/// Recover the target file of a failed patch from its error preview
/// (`failed to apply hunk to <file>: …`).
fn failed_patch_target(preview: Option<&str>) -> Option<String> {
    let first = preview?.lines().map(str::trim).find(|l| !l.is_empty())?;
    let rest = first.strip_prefix("failed to apply hunk to ")?;
    let file = rest.split(':').next()?.trim();
    if file.is_empty() {
        None
    } else {
        Some(file.to_string())
    }
}

fn status_glyph(status: ToolStatus, theme: &Theme) -> (&'static str, ratatui::style::Color) {
    match status {
        ToolStatus::Running => ("◌", theme.accent),
        ToolStatus::Ok => ("✓", theme.success),
        ToolStatus::Failed => ("✗", theme.error),
    }
}

/// The `└ …` result row: first error line for failures (with a fold hint for
/// the hidden rest and a `×N` retry count for merged repeats), a first-content
/// preview plus a quiet output-line count for successes.
#[allow(clippy::too_many_arguments)]
fn result_lines_for(
    call: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    t: &UiText,
    expanded: bool,
    guard_denial: bool,
    repeat: usize,
) -> Vec<Line<'static>> {
    if call.status == ToolStatus::Failed {
        let note = if guard_denial {
            Some(crate::tool_cell::guard_denial_note(&call.name, t).to_string())
        } else {
            failed_one_line_summary(call, t)
        };
        let Some(note) = note else {
            return Vec::new();
        };
        let retry_w = if repeat > 1 {
            UnicodeWidthStr::width(format!(" ×{repeat}").as_str())
        } else {
            0
        };
        let mut spans = vec![
            Span::styled("  └ ", Style::default().fg(theme.muted)),
            Span::styled(
                truncate_display(&note, width.saturating_sub(4 + retry_w).max(1)),
                Style::default().fg(theme.muted),
            ),
        ];
        if !expanded && !guard_denial {
            let more = preview_line_count(call).saturating_sub(1);
            if more > 0 {
                spans.push(Span::styled(
                    format!(
                        " {}",
                        t.fold_more_lines_short.replace("{}", &more.to_string())
                    ),
                    Style::default().fg(theme.dim),
                ));
            }
        }
        if repeat > 1 {
            spans.push(Span::styled(
                format!(" ×{repeat}"),
                Style::default().fg(theme.dim),
            ));
        }
        return vec![Line::from(spans)];
    }
    if call.status == ToolStatus::Running {
        return Vec::new();
    }
    // Ok: lead with the first content line so a successful read shows WHAT was
    // read, not just how much; the quiet count keeps the unit honest. Shell
    // output stays count-only (its first line is usually noise).
    let n = content_line_count(call);
    if n == 0 {
        return Vec::new();
    }
    let (pre, post) = split_placeholder(t.tool_output_lines);
    let first = if is_shell_call(call) {
        None
    } else {
        first_content_line(call)
    };
    let mut spans = vec![Span::styled("  └ ", Style::default().fg(theme.muted))];
    if let Some(first) = first {
        let count_w =
            UnicodeWidthStr::width(pre) + n.to_string().len() + UnicodeWidthStr::width(post);
        let avail = width.saturating_sub(4 + count_w + 3 + 2).max(8);
        spans.push(Span::styled(
            truncate_display(&first, avail),
            Style::default().fg(theme.muted),
        ));
        spans.push(Span::styled(
            " · ".to_string(),
            Style::default().fg(theme.dim),
        ));
    }
    spans.push(Span::styled(
        pre.to_string(),
        Style::default().fg(theme.muted),
    ));
    spans.push(Span::styled(n.to_string(), Style::default().fg(theme.dim)));
    spans.push(Span::styled(
        post.to_string(),
        Style::default().fg(theme.muted),
    ));
    if call_timed_out(call) {
        spans.push(Span::styled(
            t.result_timeout.to_string(),
            Style::default().fg(theme.dim),
        ));
    }
    vec![Line::from(spans)]
}

/// First non-empty content line of an Ok preview, with read_file's
/// line-number gutter (`   12\tfoo`) stripped.
fn first_content_line(call: &ToolCallBlock) -> Option<String> {
    let preview = call.preview.as_deref()?.trim();
    let line = preview.lines().find(|l| !l.trim().is_empty())?;
    let stripped = strip_line_gutter(line).trim();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// Strip a leading `<digits>\t` gutter that read_file adds to every row.
fn strip_line_gutter(line: &str) -> &str {
    let trimmed = line.trim_start();
    let digits = trimmed.len()
        - trimmed
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    if digits > 0 && trimmed[digits..].starts_with('\t') {
        trimmed[digits + 1..].trim_start()
    } else {
        trimmed
    }
}

/// Merged same-file edit node: one head (glyph + action + inline files), one
/// hunk-stats line, then the combined diff rows (folded unless the group is
/// expanded).
fn edit_unit_lines(
    calls: &[&ToolCallBlock],
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    expanded: bool,
) -> Vec<Line<'static>> {
    let Some(first) = calls.first() else {
        return Vec::new();
    };
    let (glyph, glyph_color) = status_glyph(first.status, theme);
    // Prefer apply_patch presentation even when the merge mixes replace calls.
    let action = tool_action_label_for("apply_patch", locale);
    let tail = match first.status {
        ToolStatus::Running => " …".to_string(),
        _ => {
            let total_ms: u64 = calls.iter().filter_map(|c| c.duration_ms).sum();
            if total_ms >= 100 {
                format!(" · {:.1}s", total_ms as f64 / 1000.0)
            } else {
                String::new()
            }
        }
    };
    let mut head = vec![
        Span::styled(format!("{glyph} "), Style::default().fg(glyph_color)),
        Span::styled(action.clone(), Style::default().fg(theme.tool)),
    ];
    // The touched file(s) ride inline on the head row.
    let files = {
        let key = edit_merge_key(first).0;
        if key.is_empty() {
            crate::tool_cell::patch_files_key(&first.arguments).replace('\u{1}', ", ")
        } else {
            key.replace('\u{1}', ", ")
        }
    };
    if !files.is_empty() {
        let used = 2 + UnicodeWidthStr::width(action.as_str()) + 2;
        let avail = width
            .saturating_sub(used + UnicodeWidthStr::width(tail.as_str()) + 8)
            .max(8);
        head.push(Span::raw("  "));
        head.push(Span::styled(
            truncate_display(&files, avail),
            Style::default().fg(theme.text),
        ));
    }
    if !tail.is_empty() {
        head.push(Span::styled(tail, Style::default().fg(theme.dim)));
    }
    let mut out = vec![Line::from(head)];

    // Line 2: `└ N 处修改 · +A −R`.
    let mut hunks = 0usize;
    let mut added = 0usize;
    let mut removed = 0usize;
    for call in calls {
        let stats = if call.name == "replace" {
            crate::tool_cell::replace_patch_from_arguments(&call.arguments)
                .map(|p| crate::tool_cell::patch_stats_from_text(&p))
                .unwrap_or_default()
        } else {
            crate::tool_cell::patch_stats(&call.arguments)
        };
        hunks += stats.hunks;
        added += stats.added;
        removed += stats.removed;
    }
    let edits = if hunks == 0 { calls.len() } else { hunks };
    let (pre, post) = split_placeholder(t.edit_merge_summary);
    out.push(Line::from(vec![
        Span::styled("  └ ", Style::default().fg(theme.muted)),
        Span::styled(pre.to_string(), Style::default().fg(theme.muted)),
        Span::styled(edits.to_string(), Style::default().fg(theme.dim)),
        Span::styled(post.to_string(), Style::default().fg(theme.muted)),
        Span::styled(" · ".to_string(), Style::default().fg(theme.muted)),
        Span::styled(
            format!("+{added} −{removed}"),
            Style::default().fg(theme.dim),
        ),
    ]));

    crate::tool_cell::merged_diff_rows(calls, theme, width, expanded, t, &mut out);
    out
}

/// Split a "{} …" i18n template around its placeholder.
fn split_placeholder(template: &str) -> (&str, &str) {
    template.split_once("{}").unwrap_or((template, ""))
}

/// Output line count for an Ok result row, skipping shell metadata rows.
fn content_line_count(call: &ToolCallBlock) -> usize {
    let Some(preview) = call
        .preview
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    else {
        return 0;
    };
    if is_shell_call(call) {
        preview
            .lines()
            .filter(|l| {
                !l.starts_with("exit: ")
                    && *l != "[timed out]"
                    && !(l.starts_with("--- ") && l.ends_with(" ---"))
            })
            .count()
    } else {
        preview.lines().count()
    }
}

fn preview_line_count(call: &ToolCallBlock) -> usize {
    call.preview
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| p.lines().count())
        .unwrap_or(0)
}

fn call_timed_out(call: &ToolCallBlock) -> bool {
    is_shell_call(call)
        && call
            .preview
            .as_deref()
            .is_some_and(|p| p.lines().any(|l| l == "[timed out]"))
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
    render_group(group, &Theme::no_color(), width, locale, locale.text(), 0)
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
            started_elapsed_secs: 0,
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

    fn patch_call(file: &str, old: &str, new: &str) -> ToolCallBlock {
        call(
            "apply_patch",
            &serde_json::json!({
                "patch": format!(
                    "*** Begin Patch\n*** Update File: {file}\n@@\n-{old}\n+{new}\n*** End Patch"
                )
            })
            .to_string(),
            ToolStatus::Ok,
        )
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
            lines.iter().any(|l| l.contains("并行执行 3 个工具")),
            "a ≥2-call parallel batch must show a concurrency header: {lines:?}"
        );
    }

    #[test]
    fn parallel_header_is_localized() {
        let g = group(vec![
            parallel_call("read_file", r#"{"path":"a.rs"}"#),
            parallel_call("grep", r#"{"pattern":"x"}"#),
        ]);
        let lines = render_group_text(&g, 100, Locale::En);
        assert!(
            lines.iter().any(|l| l.contains("2 tools in parallel")),
            "{lines:?}"
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
            !lines.iter().any(|l| l.contains("并行执行")),
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
        // list_files is Silent — successful probes never reach Conversation.
        assert!(lines.is_empty(), "{lines:?}");
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
        assert!(lines.iter().any(|l| l.contains("目标收尾")), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("受阻") && l.contains("缺 API key")),
            "{lines:?}"
        );
    }

    #[test]
    fn exploration_calls_render_as_individual_units() {
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
        // No aggregation: each call is its own three-line unit.
        assert_eq!(
            lines.iter().filter(|l| l.contains("读取文件")).count(),
            2,
            "{lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains("搜索代码")).count(),
            2,
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("PROJECT_RULES.md"))
                && lines.iter().any(|l| l.contains("Makefile")),
            "each unit shows its own target: {lines:?}"
        );
    }

    #[test]
    fn single_read_renders_a_two_line_unit() {
        let g = group(vec![call(
            "read_file",
            r#"{"path":"src/auth.go"}"#,
            ToolStatus::Ok,
        )]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].starts_with('✓')
                && lines[0].contains("读取文件")
                && lines[0].contains("auth.go"),
            "head carries glyph + action + inline target: {lines:?}"
        );
        assert!(lines[1].starts_with("  └ "), "{lines:?}");
    }

    #[test]
    fn ok_read_result_shows_first_content_line_and_count() {
        let mut c = call("read_file", r#"{"path":"README.md"}"#, ToolStatus::Ok);
        c.preview = Some("     1\t# GitCode AI 中间件服务\n     2\t\n     3\tbody".into());
        let g = group(vec![c]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[1].contains("# GitCode AI 中间件服务") && lines[1].contains("3 行"),
            "first content line (gutter stripped) + line count: {lines:?}"
        );
        assert!(
            !lines[1].contains("1\t"),
            "line-number gutter must be stripped: {lines:?}"
        );
    }

    #[test]
    fn ok_shell_result_stays_count_only() {
        let mut c = call(
            "run_command",
            r#"{"program":"cargo","args":["test"]}"#,
            ToolStatus::Ok,
        );
        c.preview = Some("warning: unused import\nexit: 0".into());
        let g = group(vec![c]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines[1].starts_with("  └ ") && lines[1].contains("1 行"),
            "shell result keeps the quiet count, no first-line dump: {lines:?}"
        );
        assert!(!lines[1].contains("unused import"), "{lines:?}");
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
        assert!(lines.iter().any(|l| l.contains("编辑文件")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("web.go")), "{lines:?}");
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
    fn collapsed_failed_tool_shows_first_error_and_a_fold_hint() {
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
        assert_eq!(lines.len(), 2, "head / result rows: {lines:?}");
        assert!(lines[0].starts_with('✗'), "{lines:?}");
        assert!(
            lines[1].starts_with("  └ ") && lines[1].contains("error: no such command"),
            "result row carries the first error line: {lines:?}"
        );
        assert!(
            lines[1].contains("+2 行") && lines[1].contains("Ctrl+O"),
            "the hidden rest gets a fold hint: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("long help dump line 2")),
            "must not dump the log while collapsed: {lines:?}"
        );
    }

    #[test]
    fn guard_denied_plan_reads_as_warning_not_error() {
        let mut c = call(
            "update_plan",
            r#"{"explanation":"现在开始第3步"}"#,
            ToolStatus::Failed,
        );
        c.preview = Some(
            "— plan step \"创建项目结构\" cannot be completed while step 1 is in_progress".into(),
        );
        let g = group(vec![c]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(lines[0].starts_with('⚠'), "{lines:?}");
        let joined = lines.join("\n");
        assert!(joined.contains("计划未更新"), "{joined}");
        assert!(
            !joined.contains("plan step") && !joined.contains("cannot"),
            "internal guard text must not leak: {joined}"
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
            lines.len() > 2,
            "expanded should reveal detail under the unit: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("extra context line")),
            "{lines:?}"
        );
    }

    #[test]
    fn shell_command_renders_dollar_prompt_and_hides_json_and_cd() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/example".into());
        let args =
            format!(r#"{{"cmd":"cd {home}/Develop/app/codeleveler && cargo test --workspace"}}"#);
        let g = group(vec![call("shell_command", &args, ToolStatus::Ok)]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("执行命令")
                && lines[0].contains("$ ")
                && lines[0].contains("cargo test --workspace"),
            "head carries the command body with a shell prompt: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains('{') || l.contains("\"cmd\"")),
            "must not leak JSON args: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains(&home) || l.contains("Develop/app")),
            "must not leak absolute cwd: {lines:?}"
        );
    }

    #[test]
    fn running_tool_uses_the_running_glyph() {
        let g = group(vec![call(
            "run_command",
            r#"{"program":"cargo","args":["build"]}"#,
            ToolStatus::Running,
        )]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(lines[0].starts_with('◌'), "{lines:?}");
        assert!(
            lines[0].contains("$ ") && lines[0].contains("cargo build"),
            "{lines:?}"
        );
    }

    #[test]
    fn running_command_shows_live_elapsed_time() {
        // A long command must show its live elapsed so it reads as "working",
        // not a static block (the reported blank-during-command issue).
        let g = group(vec![call(
            "run_command",
            r#"{"program":"go","args":["test","./..."]}"#,
            ToolStatus::Running,
        )]);
        // Turn is 45s in; the command started at elapsed 0 → 45s of runtime.
        let lines: Vec<String> =
            render_group(&g, &Theme::no_color(), 100, Locale::Zh, Locale::Zh.text(), 45)
                .into_iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                .collect();
        assert!(
            lines[0].contains("45s"),
            "running command must show live elapsed: {lines:?}"
        );
    }

    #[test]
    fn consecutive_same_file_patches_merge_into_one_edit_node() {
        let g = group(vec![
            patch_call("src/a.rs", "old1", "new1"),
            patch_call("src/a.rs", "old2", "new2"),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(
            lines.iter().filter(|l| l.contains("编辑文件")).count(),
            1,
            "one merged node, one head: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("2 处修改") && l.contains("+2 −2")),
            "combined hunk stats: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("new1") && l.contains('+'))
                && lines.iter().any(|l| l.contains("old2") && l.contains('-')),
            "both patches' diff rows: {lines:?}"
        );
    }

    #[test]
    fn different_file_patches_do_not_merge() {
        let g = group(vec![
            patch_call("src/a.rs", "old1", "new1"),
            patch_call("src/b.rs", "old2", "new2"),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(
            lines.iter().filter(|l| l.contains("编辑文件")).count(),
            2,
            "different files stay separate nodes: {lines:?}"
        );
    }

    #[test]
    fn non_adjacent_same_file_patches_do_not_merge() {
        let g = group(vec![
            patch_call("src/a.rs", "old1", "new1"),
            call("grep", r#"{"pattern":"x"}"#, ToolStatus::Ok),
            patch_call("src/a.rs", "old2", "new2"),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(
            lines.iter().filter(|l| l.contains("编辑文件")).count(),
            2,
            "a visible different tool breaks the merge: {lines:?}"
        );
    }

    #[test]
    fn single_patch_shows_stats_and_folded_diff_rows() {
        let g = group(vec![patch_call("src/a.rs", "old", "new")]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines[0].starts_with('✓')
                && lines[0].contains("编辑文件")
                && lines[0].contains("src/a.rs"),
            "head carries glyph + action + inline file: {lines:?}"
        );
        assert!(
            lines[1].starts_with("  └ ")
                && lines[1].contains("1 处修改")
                && lines[1].contains("+1 −1"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("new") && l.contains('+'))
                && lines.iter().any(|l| l.contains("old") && l.contains('-')),
            "diff rows visible by default: {lines:?}"
        );
    }

    #[test]
    fn failed_patch_recovers_target_file_from_error_preview() {
        // Unparseable patch args leave the summary at the generic placeholder;
        // the error preview still names the file — show that instead.
        let mut c = call("apply_patch", "{}", ToolStatus::Failed);
        c.preview =
            Some("failed to apply hunk to README.md: could not find context line `Archite".into());
        let g = group(vec![c]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines[0].contains("编辑文件") && lines[0].contains("README.md"),
            "head names the failed patch's target file: {lines:?}"
        );
        assert!(!lines[0].contains("补丁"), "{lines:?}");
        assert!(
            lines[1].starts_with("  └ ") && lines[1].contains("could not find context line"),
            "{lines:?}"
        );
    }

    #[test]
    fn consecutive_identical_failures_merge_with_retry_count() {
        let args = r#"{"patch":"*** Begin Patch\n*** Update File: README.md\n*** End Patch"}"#;
        let failed = || {
            let mut c = call("apply_patch", args, ToolStatus::Failed);
            c.preview = Some("invalid patch: line 1: bad hunk".into());
            c
        };
        let g = group(vec![failed(), failed(), failed()]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(
            lines.iter().filter(|l| l.contains("编辑文件")).count(),
            1,
            "identical retries collapse into one unit: {lines:?}"
        );
        assert!(
            lines[1].contains("invalid patch") && lines[1].contains("×3"),
            "result row carries the error and the retry count: {lines:?}"
        );
    }

    #[test]
    fn distinct_failures_do_not_merge() {
        let mut c1 = call("apply_patch", r#"{"patch":"a"}"#, ToolStatus::Failed);
        c1.preview = Some("invalid patch: a".into());
        let mut c2 = call("apply_patch", r#"{"patch":"b"}"#, ToolStatus::Failed);
        c2.preview = Some("invalid patch: b".into());
        let g = group(vec![c1, c2]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(
            lines.iter().filter(|l| l.contains("编辑文件")).count(),
            2,
            "different arguments stay separate units: {lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains('×')), "{lines:?}");
    }

    #[test]
    fn expanded_group_reveals_output_details() {
        let mut g = group(vec![
            call("grep", r#"{"pattern":"dist"}"#, ToolStatus::Ok),
            call("grep", r#"{"pattern":"build"}"#, ToolStatus::Ok),
        ]);
        g.expanded = true;
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert!(lines.len() >= 2, "units + detail lines: {lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("dist") || l.contains("搜索")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("ok")),
            "expanded detail shows the output body: {lines:?}"
        );
    }
}
