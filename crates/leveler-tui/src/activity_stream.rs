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
    // A group earns its rows while it is happening — which call is still
    // running, what a command printed — and stops earning them the moment it
    // finishes: every finished work group (single calls included) folds to
    // its clickable ▸ disclosure row; a click (or Ctrl+O for the latest)
    // reopens it.
    //
    // Edits are the exception: a diff is not a step toward the result, it IS
    // the result, and folding it would erase the only record of what changed.
    let visible: Vec<&ToolCallBlock> = group
        .calls
        .iter()
        .filter(|c| is_conversation_visible(c))
        .collect();
    let disclosable = group_has_disclosure(group);
    if disclosable && !group.expanded {
        return crate::presentation::disclosure::collapsed_lines(
            &disclosure_presentation(&visible, group.expanded, t),
            theme,
            width,
        );
    }
    if disclosable && group.expanded {
        // The same row, open — clicking it again folds the group back.
        out.push(crate::presentation::disclosure::header_line(
            &disclosure_presentation(&visible, group.expanded, t),
            theme,
            width,
        ));
    }
    // Replace-in-place: while the group is still working, ordinary calls
    // that already finished give up their rows to the live one — current work
    // earns screen space, settled work compacts to one truthful line and
    // folds fully (clickable) when the group finishes. Failures keep their
    // rows (an error is why you are reading this), and edits keep their diff.
    let live_group = group.open || !group_is_finished(group);
    if live_group {
        let settled: Vec<&ToolCallBlock> = visible
            .iter()
            .copied()
            .filter(|c| c.status == ToolStatus::Ok && !is_edit_call(c))
            .collect();
        if !settled.is_empty() {
            let label = disclosure_presentation(&settled, false, t).label;
            out.push(Line::from(vec![
                Span::styled("\u{2713} ", Style::default().fg(theme.status.success)),
                Span::styled(
                    truncate_display(&label, width.saturating_sub(2).max(1)),
                    Style::default().fg(theme.text.muted),
                ),
            ]));
        }
    }
    // A concurrent batch gets one quiet dim header so the user sees these
    // calls ran together rather than one after another.
    let parallel_n = if live_group {
        // Live, the header answers "how many are running NOW" — settled
        // members are already counted by the compact ✓ line above.
        group
            .calls
            .iter()
            .filter(|c| c.parallel && c.status == ToolStatus::Running)
            .count()
    } else {
        group.calls.iter().filter(|c| c.parallel).count()
    };
    if parallel_n >= 2 {
        let label = t.parallel_header.replace("{}", &parallel_n.to_string());
        out.push(Line::from(Span::styled(
            truncate_display(&label, width),
            Style::default().fg(theme.text.muted),
        )));
    }
    const LIVE_RUNNING_ROWS: usize = 3;
    let mut running_shown = 0usize;
    let mut running_hidden = 0usize;
    for unit in plan_units(&group.calls) {
        match unit {
            StreamUnit::Single(call) => {
                if live_group && call.status == ToolStatus::Ok && !is_edit_call(call) {
                    // Already represented by the compact settled line above.
                    continue;
                }
                if live_group && call.status == ToolStatus::Running {
                    // Bounded live area: a 17-wide parallel burst must not
                    // claim 17 rows. First LIVE_RUNNING_ROWS running members
                    // (deterministic call order) get rows; the rest are one
                    // truthful count line.
                    running_shown += 1;
                    if running_shown > LIVE_RUNNING_ROWS {
                        running_hidden += 1;
                        continue;
                    }
                }
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
    if running_hidden > 0 {
        out.push(Line::from(Span::styled(
            truncate_display(
                &t.parallel_more_running
                    .replace("{}", &running_hidden.to_string()),
                width,
            ),
            Style::default().fg(theme.text.muted),
        )));
    }
    out
}

/// Presentation class of one call for the disclosure contract, derived from
/// the tool taxonomy — NOT a second tool-name registry. A tool the taxonomy
/// has never heard of (MCP / future extension) is ordinary finished work and
/// falls back to the generic "Ran N tools" label instead of losing its
/// disclosure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisclosureClass {
    /// Shell commands — "Ran N shell commands".
    Shell,
    /// File/symbol reads — "Read N files".
    Read,
    /// Searches and LSP lookups — "Searched codebase".
    Search,
    /// Any other user-visible finished work, including unknown tools.
    Work,
    /// Keeps its own presentation, never folded into a disclosure:
    /// plan/goal bookkeeping, user interaction, edits, and the
    /// unsupported-delegation warning.
    Excluded,
}

fn disclosure_class(name: &str) -> DisclosureClass {
    use crate::tool_taxonomy::ToolKind;
    let Some(entry) = crate::tool_taxonomy::lookup(name) else {
        return DisclosureClass::Work;
    };
    match entry.kind {
        // Task management shares the Execute kind but is not a shell command.
        ToolKind::Execute if matches!(name, "run_command" | "shell_command") => {
            DisclosureClass::Shell
        }
        ToolKind::Execute => DisclosureClass::Work,
        ToolKind::Read => DisclosureClass::Read,
        // A symbol read is a read; the rest of the LSP kind is code lookup.
        ToolKind::Lsp if name == "read_symbol" => DisclosureClass::Read,
        ToolKind::Search | ToolKind::ListDir | ToolKind::Lsp => DisclosureClass::Search,
        ToolKind::Edit | ToolKind::Write => DisclosureClass::Excluded,
        ToolKind::Plan | ToolKind::Goal | ToolKind::AskUser => DisclosureClass::Excluded,
        // The unsupported-delegation warning has bespoke rendering that a
        // generic fold would hide.
        ToolKind::Other if name == "task" => DisclosureClass::Excluded,
        _ => DisclosureClass::Work,
    }
}

/// Whether this group renders a clickable `▸/▾` disclosure header as its
/// first line: finished, non-edit, and every visible call is ordinary work.
/// Bookkeeping/interaction calls (a denied plan update, a permission request)
/// keep their own presentation instead of being folded into "ran N tools".
pub(crate) fn group_has_disclosure(group: &ToolGroupBlock) -> bool {
    // `open` is the group's own truth about whether the burst is still in
    // flight. Members can ALL be momentarily settled while the model is
    // still streaming the next call's arguments — projecting such a group
    // as completed history paints active work as done (the real
    // `▸ 并行执行了 7 个工具`-while-running regression). History begins when
    // the group closes, not when its current members happen to be settled.
    if group.open || !group_is_finished(group) || group_has_edits(group) {
        return false;
    }
    let visible: Vec<&ToolCallBlock> = group
        .calls
        .iter()
        .filter(|c| is_conversation_visible(c))
        .collect();
    !visible.is_empty()
        && visible
            .iter()
            .all(|c| disclosure_class(&c.name) != DisclosureClass::Excluded)
}

fn group_is_finished(group: &ToolGroupBlock) -> bool {
    !group.calls.is_empty() && group.calls.iter().all(|c| c.status != ToolStatus::Running)
}

/// Edits render as a diff, which stays visible whatever the group's state.
/// (`write_file` is kept by name so an out-of-taxonomy writer can never be
/// folded away as generic work.)
fn group_has_edits(group: &ToolGroupBlock) -> bool {
    group.calls.iter().any(is_edit_call)
}

/// Edits are the exception everywhere: a diff is not a step toward the
/// result, it IS the result. (`write_file` kept by name so an out-of-taxonomy
/// writer can never be folded away as generic work.)
fn is_edit_call(c: &ToolCallBlock) -> bool {
    use crate::tool_taxonomy::ToolKind;
    c.name == "write_file"
        || matches!(
            crate::tool_taxonomy::lookup(&c.name).map(|e| e.kind),
            Some(ToolKind::Edit | ToolKind::Write)
        )
}
/// Adapter: an Agent ToolGroup's visible calls → the shared disclosure
/// presentation. All tool-specific judgement happens here (semantic label,
/// which failure names itself, when a duration is authoritative); the
/// renderer in `presentation::disclosure` sees only the finished model.
fn disclosure_presentation(
    visible: &[&ToolCallBlock],
    expanded: bool,
    t: &UiText,
) -> crate::presentation::disclosure::DisclosurePresentation {
    let failed = visible
        .iter()
        .filter(|c| c.status == ToolStatus::Failed)
        .count();
    // Only a single call has an authoritative duration (the runtime supplied
    // it). Summing children fakes wall time for parallel batches — four 5s
    // reads did not take 20s — so a multi-tool disclosure shows none.
    let duration_ms = match visible {
        [only] => only.duration_ms,
        _ => None,
    };
    crate::presentation::disclosure::DisclosurePresentation {
        label: disclosure_label(visible, failed, t),
        failed,
        failed_suffix: (failed > 0 && visible.len() > 1)
            .then(|| t.batch_failed.replace("{}", &failed.to_string())),
        expanded,
        duration_ms,
        first_error: (!expanded).then(|| first_error_line(visible)).flatten(),
    }
}

/// The semantic summary for a finished group: what KIND of work it was, in
/// user language, with correct singular/plural — never a generic "N tools"
/// when the batch had one shape.
fn disclosure_label(visible: &[&ToolCallBlock], failed: usize, t: &UiText) -> String {
    use DisclosureClass::*;
    let n = visible.len();
    let class = disclosure_class(&visible[0].name);
    let uniform = visible.iter().all(|c| disclosure_class(&c.name) == class);
    // A single failure names itself: a failed shell command is "Shell command
    // failed", a failed generic tool is "Tool failed" (never a success-shaped
    // "Ran 1 tool"). Read/search keep their semantic label — the ✗ glyph and
    // error line carry the failure.
    if failed > 0 && n == 1 {
        match class {
            Shell => return t.disclosure_failed.to_string(),
            Work => return t.disclosure_failed_tool.to_string(),
            _ => {}
        }
    }
    let parallel = visible.iter().filter(|c| c.parallel).count() >= 2;
    if parallel && !uniform {
        return t.disclosure_parallel.replace("{}", &n.to_string());
    }
    match (uniform, class, n) {
        (true, Shell, 1) => t.disclosure_shell_one.to_string(),
        (true, Shell, _) => t.disclosure_shell_many.replace("{}", &n.to_string()),
        (true, Read, 1) => t.disclosure_read_one.to_string(),
        (true, Read, _) => t.disclosure_read_many.replace("{}", &n.to_string()),
        (true, Search, _) => t.disclosure_search.to_string(),
        (true, Work, 1) => t.disclosure_tools_one.to_string(),
        _ => t.disclosure_tools_many.replace("{}", &n.to_string()),
    }
}

/// The first meaningful error line of a failed call, for the collapsed row.
fn first_error_line(visible: &[&ToolCallBlock]) -> Option<String> {
    visible
        .iter()
        .find(|c| c.status == ToolStatus::Failed)
        .and_then(|c| c.preview.as_deref())
        .and_then(|p| {
            p.lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_string)
        })
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
        ("⚠", theme.status.warning)
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
        Span::styled(action.clone(), Style::default().fg(theme.accent.secondary)),
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
        let shell = is_shell_call(call)
            && crate::tool_cell::summary_is_command_line(&call.name, &call.arguments);
        let used = 2 + UnicodeWidthStr::width(action.as_str()) + 2 + usize::from(shell) * 2;
        let avail = width
            .saturating_sub(used + UnicodeWidthStr::width(tail.as_str()) + 8)
            .max(8);
        head.push(Span::raw("  "));
        if shell {
            head.push(Span::styled(
                "$ ",
                Style::default().fg(theme.accent.secondary),
            ));
        }
        head.push(Span::styled(
            truncate_display(&summary, avail),
            Style::default().fg(theme.text.primary),
        ));
    }
    if !tail.is_empty() {
        head.push(Span::styled(tail, Style::default().fg(theme.text.muted)));
    }
    // Collapsed, a finished success is one row: the size of what came back
    // rides on the head instead of claiming a row of its own. Failures keep
    // their second row — the error text is why you are reading this at all.
    let fold_result = !expanded && call.status == ToolStatus::Ok;
    if fold_result {
        let n = content_line_count(call);
        if n > 0 {
            let (pre, post) = split_placeholder(t.tool_output_lines);
            head.push(Span::styled(
                format!(" · {pre}{n}{post}"),
                Style::default().fg(theme.text.muted),
            ));
        }
        return vec![Line::from(head)];
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
        ToolStatus::Running => ("◌", theme.accent.primary),
        ToolStatus::Ok => ("✓", theme.status.success),
        ToolStatus::Failed => ("✗", theme.status.error),
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
            Span::styled("  └ ", Style::default().fg(theme.text.secondary)),
            Span::styled(
                truncate_display(&note, width.saturating_sub(4 + retry_w).max(1)),
                Style::default().fg(theme.text.secondary),
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
                    Style::default().fg(theme.text.muted),
                ));
            }
        }
        if repeat > 1 {
            spans.push(Span::styled(
                format!(" ×{repeat}"),
                Style::default().fg(theme.text.muted),
            ));
        }
        return vec![Line::from(spans)];
    }
    if call.status == ToolStatus::Running {
        return Vec::new();
    }
    let n = content_line_count(call);
    if n == 0 {
        return Vec::new();
    }
    let (pre, post) = split_placeholder(t.tool_output_lines);
    // Collapsed, a success is worth one fact: how much came back. The first
    // content line is `package main` or a title comment often enough that
    // spending a whole row on it, for every read, forever, is not worth it —
    // Ctrl+O still has it. Shell output was already count-only here.
    let first = if is_shell_call(call) || !expanded {
        None
    } else {
        first_content_line(call)
    };
    let mut spans = vec![Span::styled(
        "  └ ",
        Style::default().fg(theme.text.secondary),
    )];
    if let Some(first) = first {
        let count_w =
            UnicodeWidthStr::width(pre) + n.to_string().len() + UnicodeWidthStr::width(post);
        let avail = width.saturating_sub(4 + count_w + 3 + 2).max(8);
        spans.push(Span::styled(
            truncate_display(&first, avail),
            Style::default().fg(theme.text.secondary),
        ));
        spans.push(Span::styled(
            " · ".to_string(),
            Style::default().fg(theme.text.muted),
        ));
    }
    spans.push(Span::styled(
        pre.to_string(),
        Style::default().fg(theme.text.secondary),
    ));
    spans.push(Span::styled(
        n.to_string(),
        Style::default().fg(theme.text.muted),
    ));
    spans.push(Span::styled(
        post.to_string(),
        Style::default().fg(theme.text.secondary),
    ));
    if call_timed_out(call) {
        spans.push(Span::styled(
            t.result_timeout.to_string(),
            Style::default().fg(theme.text.muted),
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
        Span::styled(action.clone(), Style::default().fg(theme.accent.secondary)),
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
            Style::default().fg(theme.text.primary),
        ));
    }
    if !tail.is_empty() {
        head.push(Span::styled(tail, Style::default().fg(theme.text.muted)));
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
        Span::styled("  └ ", Style::default().fg(theme.text.secondary)),
        Span::styled(pre.to_string(), Style::default().fg(theme.text.secondary)),
        Span::styled(edits.to_string(), Style::default().fg(theme.text.muted)),
        Span::styled(post.to_string(), Style::default().fg(theme.text.secondary)),
        Span::styled(" · ".to_string(), Style::default().fg(theme.text.secondary)),
        Span::styled(
            format!("+{added} −{removed}"),
            Style::default().fg(theme.text.muted),
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

    fn open_group(calls: Vec<ToolCallBlock>) -> ToolGroupBlock {
        ToolGroupBlock {
            calls,
            open: true,
            expanded: false,
        }
    }

    /// THE regression: an OPEN group whose members are all momentarily
    /// settled (the model is still streaming the next parallel call) must
    /// NOT render as completed history. History begins when the group
    /// closes, not when its current members happen to be settled.
    #[test]
    fn an_open_group_with_all_members_settled_is_not_history() {
        let mut calls: Vec<ToolCallBlock> = (0..7)
            .map(|i| {
                call(
                    "read_file",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                    ToolStatus::Ok,
                )
            })
            .collect();
        for c in &mut calls {
            c.parallel = true;
        }
        let g = open_group(calls);
        assert!(
            !group_has_disclosure(&g),
            "open group is never a disclosure"
        );
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            !lines.iter().any(|l| l.starts_with('▸')),
            "no history-style row while the burst is in flight: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains('✓') && l.contains('7')),
            "settled-so-far stays truthfully visible: {lines:?}"
        );
        // …and the same group, closed, becomes exactly one history row —
        // one presentation role at a time, never both.
        let mut closed = g.clone();
        closed.open = false;
        assert!(group_has_disclosure(&closed));
        let lines = render_group_text(&closed, 100, Locale::Zh);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with('▸'), "{lines:?}");
    }

    /// The invariant is class-agnostic: Read, Search, Command, and
    /// generic/unknown (MCP-style) groups all obey the same rule — an OPEN
    /// group is never history, whatever its members' momentary statuses.
    #[test]
    fn every_group_class_obeys_the_open_is_not_history_invariant() {
        let cases: [(&str, &str); 4] = [
            ("read_file", r#"{"path":"a.rs"}"#),
            ("grep", r#"{"pattern":"BrowserSession"}"#),
            ("run_command", r#"{"program":"cargo","args":["test"]}"#),
            ("mcp__demo__inspect", r#"{"target":"repo"}"#),
        ];
        for (name, args) in cases {
            let g = open_group(vec![
                call(name, args, ToolStatus::Ok),
                call(name, args, ToolStatus::Ok),
            ]);
            assert!(
                !group_has_disclosure(&g),
                "{name}: open group must not be history"
            );
            let lines = render_group_text(&g, 100, Locale::Zh);
            assert!(
                !lines.iter().any(|l| l.starts_with('▸')),
                "{name}: no history row while open: {lines:?}"
            );
            let mut closed = g.clone();
            closed.open = false;
            assert!(
                group_has_disclosure(&closed),
                "{name}: closed settled group IS history"
            );
        }
    }

    /// A mixed parallel burst (read + search + command) is one bounded live
    /// area, not one block per tool kind.
    #[test]
    fn a_mixed_parallel_burst_is_one_bounded_live_area() {
        let mut calls = vec![
            call("read_file", r#"{"path":"runtime.rs"}"#, ToolStatus::Running),
            call("grep", r#"{"pattern":"owner"}"#, ToolStatus::Running),
            call(
                "run_command",
                r#"{"program":"cargo","args":["check"]}"#,
                ToolStatus::Running,
            ),
            call("read_file", r#"{"path":"driver.rs"}"#, ToolStatus::Ok),
        ];
        for c in &mut calls {
            c.parallel = true;
        }
        let g = open_group(calls);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            !lines.iter().any(|l| l.starts_with('▸')),
            "not history: {lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains('◌')).count(),
            3,
            "each running member (≤ bound) keeps one row: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains('✓')),
            "the settled member is counted, not shown running: {lines:?}"
        );
    }

    /// Partial completion keeps the group live at every step until the LAST
    /// member settles — completing the first member must not collapse it.
    #[test]
    fn a_parallel_group_stays_live_until_the_last_member_settles() {
        let mk = |statuses: [ToolStatus; 3]| {
            let mut calls: Vec<ToolCallBlock> = statuses
                .iter()
                .enumerate()
                .map(|(i, st)| call("read_file", &format!(r#"{{"path":"f{i}.rs"}}"#), *st))
                .collect();
            for c in &mut calls {
                c.parallel = true;
            }
            open_group(calls)
        };
        use ToolStatus::{Ok as O, Running as R};
        for (label, g) in [
            ("all running", mk([R, R, R])),
            ("one done", mk([O, R, R])),
            ("two done", mk([O, O, R])),
        ] {
            let lines = render_group_text(&g, 100, Locale::Zh);
            assert!(
                lines.iter().any(|l| l.contains('◌')),
                "{label}: live members keep live rows: {lines:?}"
            );
            assert!(
                !lines.iter().any(|l| l.starts_with('▸')),
                "{label}: not history yet: {lines:?}"
            );
        }
    }

    /// §6 bounded live area: a wide burst gets a fixed number of running
    /// rows plus one truthful hidden count — never one row per member.
    #[test]
    fn a_wide_parallel_burst_is_bounded_with_an_honest_hidden_count() {
        let mut calls: Vec<ToolCallBlock> = (0..7)
            .map(|i| {
                call(
                    "read_file",
                    &format!(r#"{{"path":"file{i}.rs"}}"#),
                    ToolStatus::Running,
                )
            })
            .collect();
        for c in &mut calls {
            c.parallel = true;
        }
        let g = open_group(calls);
        let lines = render_group_text(&g, 100, Locale::Zh);
        let live = lines.iter().filter(|l| l.contains('◌')).count();
        assert_eq!(live, 3, "bounded running rows: {lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("还有 4 个")),
            "hidden members are counted, not dropped: {lines:?}"
        );
    }

    /// §11 serial replace-in-place: while a group is open, settled ordinary
    /// calls compact to ONE line and only the live call keeps a full row.
    #[test]
    fn an_open_group_compacts_settled_calls_and_keeps_one_live_row() {
        let g = group(vec![
            call(
                "read_file",
                r#"{"path":"internal/bot/service.go"}"#,
                ToolStatus::Ok,
            ),
            call(
                "read_file",
                r#"{"path":"internal/model/bot.go"}"#,
                ToolStatus::Ok,
            ),
            call("grep", r#"{"pattern":"TokenPlain"}"#, ToolStatus::Running),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 2, "settled summary + live row: {lines:?}");
        assert!(
            lines[0].contains("2"),
            "counts the settled calls: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("service.go")),
            "settled calls gave up their rows: {lines:?}"
        );
        assert!(
            lines[1].contains('◌'),
            "the live call keeps its row: {lines:?}"
        );
        assert!(lines[1].contains("TokenPlain"), "{lines:?}");
    }

    /// §21: a failure keeps its row while the group is still working — an
    /// error must never be compacted away as ordinary settled work.
    #[test]
    fn a_failed_call_keeps_its_row_while_the_group_is_open() {
        let mut failed = call(
            "run_command",
            r#"{"args":["test","./..."]}"#,
            ToolStatus::Failed,
        );
        failed.preview = Some("run_command is missing `program`".into());
        let g = group(vec![
            call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            failed,
            call("grep", r#"{"pattern":"x"}"#, ToolStatus::Running),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines.iter().any(|l| l.contains('✗')),
            "failure stays prominent: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains('◌')), "{lines:?}");
    }

    /// §33 parallel: a bounded current group, updated in place — finishing
    /// one member moves it into the settled line, live rows shrink.
    #[test]
    fn a_parallel_group_updates_in_place_as_members_finish() {
        let mut a = call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok);
        let mut b = call("read_file", r#"{"path":"b.rs"}"#, ToolStatus::Running);
        let mut c = call("grep", r#"{"pattern":"x"}"#, ToolStatus::Running);
        for x in [&mut a, &mut b, &mut c] {
            x.parallel = true;
        }
        let g = group(vec![a, b, c]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        let live = lines.iter().filter(|l| l.contains('◌')).count();
        assert_eq!(live, 2, "two still running: {lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains("a.rs")),
            "finished member settled compactly: {lines:?}"
        );
    }

    /// §12/§14: once every call finishes, the whole group folds to its
    /// clickable disclosure row — the settled line and live rows all vanish.
    #[test]
    fn a_finished_group_folds_and_drops_the_live_rendering() {
        let g = group(vec![
            call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            call("read_file", r#"{"path":"b.rs"}"#, ToolStatus::Ok),
            call("grep", r#"{"pattern":"x"}"#, ToolStatus::Ok),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 1, "one disclosure row: {lines:?}");
        assert!(lines[0].starts_with('▸'), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains('◌')), "{lines:?}");
    }

    /// An invalid run_command (no program) must not be prettified as `$ …` —
    /// the runtime refused it, and the row has to say so, not fabricate a
    /// command line out of the arguments.
    #[test]
    fn a_failed_program_less_run_command_shows_no_shell_prompt() {
        let mut c = call(
            "run_command",
            r#"{"args":["test","./...","-count=1"]}"#,
            ToolStatus::Failed,
        );
        c.preview = Some("run_command is missing `program`".into());
        let g = group(vec![c]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            !lines.iter().any(|l| l.contains('$')),
            "no shell prompt for an invalid call: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("test ./...")),
            "must not fabricate a command: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("program")),
            "names the missing field: {lines:?}"
        );
        assert!(lines[0].contains('✗'), "failure stays visible: {lines:?}");
    }

    /// A long multi-line shell script stays a single compact running row —
    /// current work earns focus, not screen area.
    #[test]
    fn a_long_multi_line_script_renders_one_compact_running_row() {
        let script = "echo start\ncurl -X POST http://127.0.0.1:8090/api/v1/bots/1/permissions \\\n  -H 'Content-Type: application/json' \\\n  -d '{\\\"scope\\\":\\\"repo\\\"}'\ntail -5 out.log";
        let args = serde_json::json!({ "cmd": script }).to_string();
        let g = group(vec![call("shell_command", &args, ToolStatus::Running)]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert_eq!(lines.len(), 1, "one row while running: {lines:?}");
        assert!(lines[0].contains('◌'), "{lines:?}");
    }

    #[test]
    fn unknown_tool_folds_to_a_generic_disclosure() {
        // MCP-style / future tool the taxonomy has never heard of.
        let g = group(vec![call(
            "mcp__demo__inspect",
            r#"{"target":"repo"}"#,
            ToolStatus::Ok,
        )]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with('▸') && lines[0].contains("执行了 1 个工具"),
            "unknown tools fall back to the generic label: {lines:?}"
        );
        let mut open = group(vec![call(
            "mcp__demo__inspect",
            r#"{"target":"repo"}"#,
            ToolStatus::Ok,
        )]);
        open.expanded = true;
        let lines = render_group_text(&open, 80, Locale::Zh);
        assert!(lines[0].starts_with('▾'), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains("{\"target\"")),
            "expanded detail must not dump raw JSON args: {lines:?}"
        );
    }

    #[test]
    fn known_work_tools_outside_shell_read_search_fold_too() {
        // WebSearch kind → generic work disclosure.
        let g = group(vec![call("web_search", r#"{"query":"x"}"#, ToolStatus::Ok)]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert!(
            lines[0].starts_with('▸') && lines[0].contains("执行了 1 个工具"),
            "web tools are ordinary finished work: {lines:?}"
        );
        // LSP kind reads as search work.
        let g = group(vec![call(
            "diagnostics",
            r#"{"path":"a.rs"}"#,
            ToolStatus::Ok,
        )]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert!(
            lines[0].starts_with('▸') && lines[0].contains("搜索了代码库"),
            "LSP lookups read as codebase search: {lines:?}"
        );
    }

    #[test]
    fn interaction_and_delegation_groups_never_fold() {
        // request_permissions is an interaction, task is the unsupported-
        // delegation warning — both keep their own presentation.
        for (name, args) in [
            ("request_permissions", r#"{"scope":"full"}"#),
            ("task", r#"{"description":"do x"}"#),
        ] {
            let g = group(vec![call(name, args, ToolStatus::Ok)]);
            assert!(
                !group_has_disclosure(&g),
                "{name} must not fold into a generic disclosure"
            );
        }
    }

    #[test]
    fn mixed_work_group_folds_to_a_generic_count() {
        let g = group(vec![
            call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            call("grep", r#"{"pattern":"x"}"#, ToolStatus::Ok),
            call(
                "run_command",
                r#"{"program":"cargo","args":["test"]}"#,
                ToolStatus::Ok,
            ),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines[0].starts_with('▸') && lines[0].contains("执行了 3 个工具"),
            "a mixed sequential batch gets the generic count: {lines:?}"
        );
    }

    #[test]
    fn failed_unknown_tool_names_the_failure() {
        let mut c = call(
            "mcp__demo__inspect",
            r#"{"target":"x"}"#,
            ToolStatus::Failed,
        );
        c.preview = Some("connection refused".into());
        let g = group(vec![c]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines[0].starts_with('▸')
                && lines[0].contains('✗')
                && lines[0].contains("工具执行失败"),
            "an unknown failed tool must not read like success: {lines:?}"
        );
        assert!(
            lines[1].contains("connection refused"),
            "first error line rides along: {lines:?}"
        );
    }

    #[test]
    fn group_duration_is_shown_for_a_single_call_only() {
        // Single call: the runtime-supplied duration is authoritative.
        let mut c = call(
            "run_command",
            r#"{"program":"cargo","args":["test"]}"#,
            ToolStatus::Ok,
        );
        c.duration_ms = Some(1250);
        let g = group(vec![c]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(lines[0].contains("1.2s"), "{lines:?}");

        // Multi-call group: summing child durations fakes wall time (four
        // parallel 5s reads did NOT take 20s) — show no group duration.
        let calls: Vec<ToolCallBlock> = (0..4)
            .map(|i| {
                let mut c = parallel_call("read_file", &format!(r#"{{"path":"f{i}.rs"}}"#));
                c.duration_ms = Some(5000);
                c
            })
            .collect();
        let g = group(calls);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            !lines[0].contains("20.0s") && !lines[0].contains(" s") && !lines[0].contains("5.0s"),
            "no derived duration on a multi-tool disclosure: {lines:?}"
        );

        // Missing duration: nothing, not 0s.
        let mut c = call(
            "run_command",
            r#"{"program":"cargo","args":["build"]}"#,
            ToolStatus::Ok,
        );
        c.duration_ms = None;
        let g = group(vec![c]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(!lines[0].contains("0.0s"), "{lines:?}");
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

    fn running(mut c: ToolCallBlock) -> ToolCallBlock {
        c.status = ToolStatus::Running;
        c.duration_ms = None;
        c
    }

    #[test]
    fn a_finished_parallel_batch_collapses_to_one_row() {
        // Eight parallel reads cost sixteen rows while they were interesting
        // and sixteen rows forever after. Once they are done the answer below
        // carries their result; the batch only needs to say it happened.
        let g = group(
            (0..8)
                .map(|i| parallel_call("read_file", &format!(r#"{{"path":"f{i}.rs"}}"#)))
                .collect(),
        );
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(
            lines.len(),
            1,
            "a finished batch should be one row, got:\n{}",
            lines.join("\n")
        );
        assert!(
            lines[0].contains('8'),
            "row must say how many: {:?}",
            lines[0]
        );
    }

    #[test]
    fn a_running_parallel_batch_stays_open() {
        // While it runs, which call is STILL GOING is exactly what you want —
        // and only that: settled members compact to one line (frozen model:
        // current work earns screen space, completed work gets out of the way).
        let mut calls: Vec<ToolCallBlock> = (0..4)
            .map(|i| parallel_call("read_file", &format!(r#"{{"path":"f{i}.rs"}}"#)))
            .collect();
        calls[2] = running(calls[2].clone());
        let g = group(calls);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines.iter().any(|l| l.contains('◌') && l.contains("f2.rs")),
            "the live call keeps its row:\n{}",
            lines.join("\n")
        );
        assert!(
            lines.iter().any(|l| l.contains('✓') && l.contains('3')),
            "settled members compact to one counted line:\n{}",
            lines.join("\n")
        );
        assert!(
            !lines.iter().any(|l| l.contains("f0.rs")),
            "settled members gave up their rows:\n{}",
            lines.join("\n")
        );
    }

    #[test]
    fn a_batch_with_failures_reports_them_on_its_row() {
        // Collapsing must not hide that something broke — but eight rows of
        // error output is how a screen full of failures becomes unreadable.
        let mut calls: Vec<ToolCallBlock> = (0..6)
            .map(|i| parallel_call("read_file", &format!(r#"{{"path":"f{i}.rs"}}"#)))
            .collect();
        calls[1].status = ToolStatus::Failed;
        calls[4].status = ToolStatus::Failed;
        let g = group(calls);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(
            lines.len(),
            2,
            "disclosure row plus first error:\n{}",
            lines.join("\n")
        );
        assert!(
            lines[0].contains('2') && lines[0].contains("失败") && lines[0].contains('✗'),
            "the row must name the failures: {:?}",
            lines[0]
        );
        assert!(
            lines[0].starts_with('▸'),
            "collapsed disclosure: {:?}",
            lines[0]
        );
    }

    #[test]
    fn a_finished_single_tool_drops_its_preview_row() {
        let g = group(vec![call(
            "read_file",
            r#"{"path":"go.mod"}"#,
            ToolStatus::Ok,
        )]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(
            lines.len(),
            1,
            "a finished tool is one row:\n{}",
            lines.join("\n")
        );
    }

    #[test]
    fn a_finished_edit_keeps_its_diff() {
        // Edits are the exception. A read is a step on the way to an answer and
        // its result lands in that answer; a diff IS the result — collapsing it
        // would hide the only record of what changed.
        let g = group(vec![
            patch_call("a.rs", "old", "new"),
            patch_call("b.rs", "old", "new"),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines.iter().any(|l| l.contains("new")),
            "the diff body must survive:\n{}",
            lines.join("\n")
        );
    }

    #[test]
    fn ctrl_o_still_opens_a_collapsed_group() {
        // Collapsing is the default, not a wall: the detail is one key away.
        let mut g = group(
            (0..4)
                .map(|i| parallel_call("read_file", &format!(r#"{{"path":"f{i}.rs"}}"#)))
                .collect(),
        );
        g.expanded = true;
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines.len() > 1,
            "expanding must show the calls:\n{}",
            lines.join("\n")
        );
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
            lines[0].starts_with('▸') && lines[0].contains('3'),
            "a finished parallel batch folds to its disclosure: {lines:?}"
        );
        let mut open = group(vec![
            parallel_call("read_file", r#"{"path":"a.rs"}"#),
            parallel_call("grep", r#"{"pattern":"x"}"#),
            parallel_call("read_file", r#"{"path":"b.rs"}"#),
        ]);
        open.expanded = true;
        let lines = render_group_text(&open, 100, Locale::Zh);
        assert!(
            lines.iter().any(|l| l.contains("并行执行 3 个工具")),
            "expanded parallel batch keeps its concurrency header: {lines:?}"
        );
    }

    #[test]
    fn parallel_header_is_localized() {
        let mut g = group(vec![
            parallel_call("read_file", r#"{"path":"a.rs"}"#),
            parallel_call("grep", r#"{"pattern":"x"}"#),
        ]);
        g.expanded = true;
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
    fn exploration_calls_collapse_into_one_row() {
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
        assert_eq!(lines.len(), 1, "a finished batch is one row: {lines:?}");
        assert!(lines[0].contains('4'), "{lines:?}");

        // And each call comes back intact when asked for.
        let mut open = g;
        open.expanded = true;
        let opened = render_group_text(&open, 80, Locale::Zh);
        assert_eq!(
            opened.iter().filter(|l| l.contains("读取文件")).count(),
            2,
            "{opened:?}"
        );
    }

    #[test]
    fn single_read_renders_one_row() {
        // New contract: a finished single work tool folds to its disclosure.
        let g = group(vec![call(
            "read_file",
            r#"{"path":"src/auth.go"}"#,
            ToolStatus::Ok,
        )]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with('▸') && lines[0].contains("读取了 1 个文件"),
            "collapsed disclosure with the singular read label: {lines:?}"
        );
    }

    #[test]
    fn ok_read_result_reports_its_size_on_one_row() {
        let mut c = call("read_file", r#"{"path":"README.md"}"#, ToolStatus::Ok);
        c.preview = Some("     1\t# GitCode AI 中间件服务\n     2\t\n     3\tbody".into());
        let mut g = group(vec![c]);
        g.expanded = true;
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines[0].starts_with('▾'),
            "expanded disclosure header: {lines:?}"
        );
        let joined = lines.join("\n");
        assert!(joined.contains("3 行"), "size must survive: {joined}");
        let summary = lines
            .iter()
            .find(|l| l.contains("3 行"))
            .expect("summary row exists");
        assert!(
            !summary.contains('\t'),
            "line-number gutter must not leak into the summary row: {summary}"
        );
    }

    #[test]
    fn expanding_a_read_brings_back_its_first_line() {
        // The preview is folded away, not thrown away.
        let mut c = call("read_file", r#"{"path":"README.md"}"#, ToolStatus::Ok);
        c.preview = Some("     1\t# GitCode AI 中间件服务\n     2\t\n     3\tbody".into());
        let mut g = group(vec![c]);
        g.expanded = true;
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines.iter().any(|l| l.contains("# GitCode AI 中间件服务")),
            "Ctrl+O must show what was read: {lines:?}"
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
        assert_eq!(lines.len(), 1, "collapsed to one disclosure row: {lines:?}");
        assert!(
            lines[0].starts_with('▸') && lines[0].contains("执行了 1 个命令"),
            "shell disclosure label: {lines:?}"
        );
        assert!(
            !lines[0].contains("unused import"),
            "no output dump on the collapsed row: {lines:?}"
        );
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
        assert!(
            !lines.is_empty() && lines[0].starts_with('▸') && lines[0].contains('✗'),
            "a silent failure still surfaces, marked failed: {lines:?}"
        );
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
        assert_eq!(lines.len(), 2, "disclosure / error rows: {lines:?}");
        assert!(
            lines[0].starts_with('▸') && lines[0].contains('✗') && lines[0].contains("失败"),
            "collapsed failure names itself: {lines:?}"
        );
        assert!(
            lines[1].starts_with("  └ ") && lines[1].contains("error: no such command"),
            "result row carries the first error line: {lines:?}"
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
        let mut g = group(vec![call("shell_command", &args, ToolStatus::Ok)]);
        g.expanded = true;
        let lines = render_group_text(&g, 100, Locale::Zh);
        let head = lines
            .iter()
            .find(|l| l.contains("执行命令"))
            .expect("command row exists");
        assert!(
            head.contains("$ ") && head.contains("cargo test --workspace"),
            "command row carries the body with a shell prompt: {lines:?}"
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
        let lines: Vec<String> = render_group(
            &g,
            &Theme::no_color(),
            100,
            Locale::Zh,
            Locale::Zh.text(),
            45,
        )
        .into_iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
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
