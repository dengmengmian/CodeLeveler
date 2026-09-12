//! Conversation activity stream: per-call tool evidence.
//!
//! Product surface, not a tool trace — but evidence, not a summary. Every
//! user-visible call owns a row that answers four questions: which tool ran, on
//! what, what came back, and what state it is in. An aggregate that replaced
//! those rows ("读取 4 个文件", "检查代码库") answered none of them.
//!
//! - **Silent** tools (ls/find probes, goal bookkeeping): hidden per-call.
//! - **User-visible**: every Normal/Important call renders a head row (status
//!   glyph + action + inline target) and, collapsed, one `└` result line. No
//!   whole-line tinting: only the glyph carries a status color.
//! - A finished group keeps a clickable `▸/▾` summary as a HEADER over its
//!   rows. That fold governs each call's OUTPUT — the one thing it may hide.
//!   A lone call gets no header: its own row already says everything.
//! - Calls the reducer OBSERVED in flight together ([`ToolCallBlock::batch`])
//!   render as a `├`/`└` tree under one header. Nothing else does: adjacency
//!   and timing never imply concurrency.
//! - Consecutive same-file patches merge into one edit node whose diff is
//!   always complete; consecutive identical failures merge with a `×N` count.

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
///
/// Every user-visible call in the group owns a row, whatever the group's
/// state (§4). The row says which tool ran, on what, what came back, and what
/// state it is in — four questions an aggregate ("读取 4 个文件") answers none
/// of. Finished history keeps its clickable `▸/▾` summary as a HEADER over
/// those rows, not in place of them: that row governs how much of each call's
/// OUTPUT is shown, and output is the only thing a fold may hide.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_group(
    group: &ToolGroupBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    now_elapsed_secs: u64,
    // The call an open approval overlay is holding, when one is open. That
    // call has been announced but NOT authorised, so it must not wear the
    // running mark (§11): a `rm -rf` nobody has agreed to is not work in
    // progress. Identity comes from the request's own `call_id` — never from
    // "the latest running call", which would be a guess.
    awaiting_approval: Option<&leveler_client_protocol::ToolCallId>,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let visible: Vec<&ToolCallBlock> = group
        .calls
        .iter()
        .filter(|c| is_conversation_visible(c))
        .collect();
    // The summary header earns its row only when it summarizes something: over
    // a lone call, "读取 1 个文件" is the row beneath it said twice. A
    // single-call group's own head row is then the click target.
    if group_has_disclosure(group) && visible.len() > 1 {
        out.push(crate::presentation::disclosure::header_line(
            &disclosure_presentation(&visible, group.expanded, t),
            theme,
            width,
        ));
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
                    None,
                    awaits(call, awaiting_approval),
                ));
                push_expanded_detail(call, group.expanded, theme, width, locale, t, &mut out);
            }
            StreamUnit::Batch(calls) => {
                // The header's count is the number of children drawn below it,
                // taken from the same projection — never the batch's raw size.
                let running = calls.iter().any(|c| c.status == ToolStatus::Running);
                let (glyph, color) = if running {
                    ("\u{25cc} ", theme.accent.primary)
                } else {
                    ("\u{b7} ", theme.text.muted)
                };
                let label = t.parallel_header.replace("{}", &calls.len().to_string());
                out.push(Line::from(vec![
                    Span::styled(glyph, Style::default().fg(color)),
                    Span::styled(
                        truncate_display(&label, width.saturating_sub(2).max(1)),
                        Style::default().fg(theme.text.muted),
                    ),
                ]));
                let last = calls.len() - 1;
                for (i, call) in calls.iter().enumerate() {
                    let branch = if i == last { "\u{2514} " } else { "\u{251c} " };
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
                        Some(branch),
                        awaits(call, awaiting_approval),
                    ));
                    push_expanded_detail(call, group.expanded, theme, width, locale, t, &mut out);
                }
            }
            StreamUnit::EditMerge(calls) => {
                out.extend(edit_unit_lines(&calls, theme, width, locale, t));
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
                    None,
                    // A settled failure is not waiting on anybody.
                    false,
                ));
                push_expanded_detail(calls[0], group.expanded, theme, width, locale, t, &mut out);
            }
        }
    }
    out
}

/// Whether this exact call is the one an open approval is holding.
fn awaits(
    call: &ToolCallBlock,
    awaiting_approval: Option<&leveler_client_protocol::ToolCallId>,
) -> bool {
    call.status == ToolStatus::Running && awaiting_approval == Some(&call.id)
}

/// The per-call output body an expanded group reveals. Silent bookkeeping that
/// succeeded has no body worth a row.
#[allow(clippy::too_many_arguments)]
fn push_expanded_detail(
    call: &ToolCallBlock,
    expanded: bool,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    out: &mut Vec<Line<'static>>,
) {
    if !expanded {
        return;
    }
    if activity_visibility(&call.name, &call.arguments) == ActivityVisibility::Silent
        && call.status != ToolStatus::Failed
    {
        return;
    }
    append_call_detail(call, theme, width, true, locale, t, out);
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
    if !uniform && visible.iter().all(|c| is_exploratory(c)) {
        return t.disclosure_explore.to_string();
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
    /// Two or more render-adjacent calls the reducer OBSERVED in flight
    /// together (same [`ToolCallBlock::batch`]). Adjacency is required so a
    /// serial call that ran between two members of one burst keeps its place
    /// in the transcript instead of being reordered under a tree.
    Batch(Vec<&'a ToolCallBlock>),
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
        if let Some(id) = call.batch {
            // Render-adjacent members of the SAME observed burst.
            let mut members = vec![call];
            let mut j = i + 1;
            while j < calls.len() {
                let next = &calls[j];
                if !is_conversation_visible(next) {
                    j += 1;
                    continue;
                }
                if next.batch == Some(id) {
                    members.push(next);
                    j += 1;
                } else {
                    break;
                }
            }
            // A burst with one visible member is not a tree: it is one call.
            if members.len() > 1 {
                out.push(StreamUnit::Batch(members));
                i = j;
                continue;
            }
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

/// The files one edit touched, preferring what execution reported.
///
/// `applied_diff` is the only source that knows where a change LANDED; the
/// arguments only say what was asked for. Reading the request first is how a
/// `write_file` (whose argument is a path plus a body, not a patch) ended up
/// with no target and no diff at all.
fn edit_target_files(call: &ToolCallBlock) -> String {
    if let Some(diff) = call
        .applied_diff
        .as_deref()
        .filter(|d| !d.trim().is_empty())
    {
        let files = crate::tool_cell::patch_touched_files_pub(diff);
        if !files.is_empty() {
            return files.join("\u{1}");
        }
    }
    if let Some(path) = serde_json::from_str::<serde_json::Value>(&call.arguments)
        .ok()
        .and_then(|v| v.get("path")?.as_str().map(str::to_string))
        .filter(|p| !p.is_empty())
    {
        return path;
    }
    crate::tool_cell::patch_files_key(&call.arguments)
}

/// A non-failed edit with a real file target renders as an edit node.
///
/// A failed edit is excluded on purpose: it has no applied diff, so the only
/// patch text available is the REQUEST, and painting that as a change would
/// show the user code that is not in their tree.
fn mergeable_edit(call: &ToolCallBlock) -> bool {
    if call.status == ToolStatus::Failed {
        return false;
    }
    is_edit_call(call) && !edit_target_files(call).is_empty()
}

/// Merge identity: same tool, same touched files, same status. A different
/// tool never merges — one node carries one action label, and calling a
/// `write_file` "编辑文件" because an `apply_patch` came first is a lie about
/// which tool ran.
fn edit_merge_key(call: &ToolCallBlock) -> (String, String, ToolStatus) {
    (call.name.clone(), edit_target_files(call), call.status)
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
    // `├ ` / `└ ` when this row is a child of a batch header, else `None`.
    branch: Option<&str>,
    // True when an open approval is holding this call: announced, not
    // authorised, and therefore not running.
    awaiting_approval: bool,
) -> Vec<Line<'static>> {
    // Plan/goal guard rejections carry internal English validation text for the
    // model — show a warning glyph and a localized note instead. Other failures
    // are real errors: the result row shows the first error line.
    let guard_denial = call.status == ToolStatus::Failed
        && matches!(call.name.as_str(), "update_plan" | "update_goal");
    let (glyph, glyph_color) = if guard_denial || awaiting_approval {
        ("⚠", theme.status.warning)
    } else {
        status_glyph(call, theme)
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
        // Held for approval: a clock would say the command is taking a while,
        // when it has not started. Say what it is actually waiting for.
        ToolStatus::Running if awaiting_approval => format!(" · {}", t.approval_pending),
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

    let mut head = Vec::new();
    if let Some(branch) = branch {
        head.push(Span::styled(
            branch.to_string(),
            Style::default().fg(theme.text.muted),
        ));
    }
    head.push(Span::styled(
        format!("{glyph} "),
        Style::default().fg(glyph_color),
    ));
    head.push(Span::styled(
        action.clone(),
        Style::default().fg(theme.accent.secondary),
    ));
    let branch_w = branch.map(UnicodeWidthStr::width).unwrap_or(0);

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
        let used =
            branch_w + 2 + UnicodeWidthStr::width(action.as_str()) + 2 + usize::from(shell) * 2;
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
    //
    // An interaction is the other exception. Its result is a decision the user
    // made, not output produced by a tool, so "· 1 行" is the one thing about
    // it that does not matter. The answer always gets its row.
    let fold_result = !expanded && call.status == ToolStatus::Ok && !is_user_decision_call(call);
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

    // Line 2: the result summary — but only while it is the ONLY account of
    // the result. An expanded group prints the whole output body below this
    // unit, and emitting both put two `└` rows on screen saying the same
    // thing. A merged failure keeps its row either way: the `×N` retry count
    // lives nowhere else.
    if !expanded || repeat > 1 {
        out.extend(result_lines_for(
            call,
            theme,
            width,
            t,
            expanded,
            guard_denial,
            repeat,
        ));
    }
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

/// Whether this call's result is a decision the USER made rather than output a
/// tool produced. Read from the taxonomy kind, not a second tool-name table.
fn is_user_decision_call(call: &ToolCallBlock) -> bool {
    matches!(
        crate::tool_taxonomy::lookup(&call.name).map(|e| e.kind),
        Some(crate::tool_taxonomy::ToolKind::AskUser)
    )
}

/// Whether this call is the agent LOOKING AROUND — a read, a search, a symbol
/// lookup. Derived from the taxonomy, never a second tool-name registry.
fn is_exploratory(call: &ToolCallBlock) -> bool {
    crate::tool_taxonomy::activity_class(&call.name) == crate::tool_taxonomy::ActivityClass::Explore
}

/// The glyph for one call, weighted by what its success actually PROVES.
///
/// `ToolStatus::Ok` is a runtime fact: the tool ran. It is not a product
/// result. A read that succeeded proves a file was read — the task may be no
/// closer to done — while an edit that succeeded changed the user's tree and a
/// passing command proved something. Spending the same green ✓ on both makes
/// the mark meaningless, so exploration gets a quiet dot and the success mark
/// stays scarce.
fn status_glyph(call: &ToolCallBlock, theme: &Theme) -> (&'static str, ratatui::style::Color) {
    match call.status {
        ToolStatus::Running => ("\u{25cc}", theme.accent.primary),
        ToolStatus::Failed => ("\u{2717}", theme.status.error),
        // A goal update that reports `blocked` is a call that RAN and an
        // outcome that did not. Ok is the runtime fact; ✓ would be a claim the
        // row's own text contradicts. Read from the taxonomy, not a second
        // tool-name table.
        ToolStatus::Ok
            if call.name == "update_goal"
                && crate::tool_taxonomy::update_goal_is_blocked(&call.arguments) =>
        {
            ("\u{26a0}", theme.status.warning)
        }
        ToolStatus::Ok if is_exploratory(call) => ("\u{b7}", theme.text.muted),
        ToolStatus::Ok => ("\u{2713}", theme.status.success),
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
    // An interaction's result is the user's own words. Print them, not a line
    // count: "· 1 行" over a decision the user was asked to make tells them
    // how long their answer was and not what it said.
    if is_user_decision_call(call) {
        let answer = call.preview.as_deref().unwrap_or("").trim();
        return vec![Line::from(vec![
            Span::styled("  └ ", Style::default().fg(theme.text.secondary)),
            Span::styled(
                truncate_display(answer, width.saturating_sub(4).max(8)),
                Style::default().fg(theme.text.primary),
            ),
        ])];
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
/// hunk-stats line, then the combined diff rows — always complete (§6).
fn edit_unit_lines(
    calls: &[&ToolCallBlock],
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
) -> Vec<Line<'static>> {
    let Some(first) = calls.first() else {
        return Vec::new();
    };
    let (glyph, glyph_color) = status_glyph(first, theme);
    // The node's own tool names it: every member shares it (merge identity).
    let action = tool_action_label_for(&first.name, locale);
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
    let files = edit_target_files(first).replace('\u{1}', ", ");
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

    // Line 2: `└ N 处修改 · +A −R`, counted over the SAME patch text the diff
    // rows below are drawn from. Counting the request instead put a `+3 −0`
    // header above two changed lines.
    let mut hunks = 0usize;
    let mut added = 0usize;
    let mut removed = 0usize;
    for call in calls {
        let stats = crate::tool_cell::edit_patch_for(call)
            .map(|p| crate::tool_cell::patch_stats_from_text(&p))
            .unwrap_or_default();
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

    crate::tool_cell::merged_diff_rows(calls, theme, width, &mut out);
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

/// Tool activity is the SECOND level of the conversation: the user's prompt
/// and the agent's prose own the content baseline, and what the agent DID to
/// answer sits one level inside them (its own details one level deeper still).
/// The block is rendered against the narrower inner width so the indent can
/// never push a row past the right gutter.
pub(crate) const ACTIVITY_INDENT: &str = "  ";

/// A tool group placed at the conversation's activity level.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_activity(
    group: &ToolGroupBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    now_elapsed_secs: u64,
    awaiting_approval: Option<&leveler_client_protocol::ToolCallId>,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(ACTIVITY_INDENT.len());
    render_group(
        group,
        theme,
        inner,
        locale,
        t,
        now_elapsed_secs,
        awaiting_approval,
    )
    .into_iter()
    .map(|line| {
        let mut spans = Vec::with_capacity(line.spans.len() + 1);
        spans.push(Span::raw(ACTIVITY_INDENT));
        spans.extend(line.spans);
        Line::from(spans)
    })
    .collect()
}

/// Plain-text lines for tests (no styling).
#[cfg(test)]
pub(crate) fn render_group_text(
    group: &ToolGroupBlock,
    width: usize,
    locale: Locale,
) -> Vec<String> {
    render_group(
        group,
        &Theme::no_color(),
        width,
        locale,
        locale.text(),
        0,
        None,
    )
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

    /// Plain-text lines with an approval held open over one call.
    fn render_group_awaiting(group: &ToolGroupBlock, awaiting: Option<&ToolCallId>) -> Vec<String> {
        render_group(
            group,
            &Theme::no_color(),
            100,
            Locale::Zh,
            Locale::Zh.text(),
            0,
            awaiting,
        )
        .into_iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|sp| sp.content.as_ref())
                .collect::<String>()
        })
        .collect()
    }

    /// Found by replaying a real blocked session: `update_goal(blocked)` is a
    /// call that RAN, so its status is Ok, and the row rendered
    /// `✓ 目标收尾 受阻：…` — the success mark on the announcement that the
    /// work could not be done. Same mistake exploration used to make.
    #[test]
    fn a_goal_that_reports_blocked_does_not_wear_the_success_mark() {
        let theme = Theme::no_color();
        let blocked = call(
            "update_goal",
            r#"{"status":"blocked","summary":"conflicts with zero_test.go"}"#,
            ToolStatus::Ok,
        );
        let (glyph, _) = status_glyph(&blocked, &theme);
        assert_ne!(glyph, "\u{2713}", "a blocked goal is not a success");

        let complete = call(
            "update_goal",
            r#"{"status":"complete","summary":"done"}"#,
            ToolStatus::Ok,
        );
        let (glyph, _) = status_glyph(&complete, &theme);
        assert_eq!(glyph, "\u{2713}", "a completed goal still earns it");
    }

    fn call(name: &str, args: &str, status: ToolStatus) -> ToolCallBlock {
        ToolCallBlock {
            id: ToolCallId::new(format!("{name}-{}", args.len())),
            name: name.into(),
            arguments: args.into(),
            status,
            preview: Some("ok".into()),
            duration_ms: Some(5),
            parallel: false,
            batch: None,
            started_elapsed_secs: 0,
            applied_diff: None,
        }
    }

    /// Live MEMBER rows: a `◌` row that names a tool, never the batch header
    /// (`◌ 并行处理 N 项`) or a live aggregate above them.
    fn live_member_rows(lines: &[String]) -> usize {
        lines
            .iter()
            .filter(|l| l.contains('\u{25cc}') && !l.contains("正在") && !l.contains("并行"))
            .count()
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

    // ── Activity weight: what a success actually proves ─────────────────────

    /// R1: exploration in flight reads as in flight, never as a result.
    #[test]
    fn running_reads_show_progress_not_success() {
        let calls: Vec<ToolCallBlock> = (0..4)
            .map(|i| {
                call(
                    "read_file",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                    ToolStatus::Running,
                )
            })
            .collect();
        let lines = render_group_text(&open_group(calls), 100, Locale::Zh);
        let text = lines.join("\n");
        assert!(text.contains('\u{25cc}'), "{text}");
        assert!(!text.contains('\u{2713}'), "nothing succeeded yet: {text}");
    }

    /// R2: settled inside a still-open burst keeps its own evidence row. The
    /// aggregate that used to REPLACE these rows ("· 已读取 4 个文件") answered
    /// none of the four questions a tool row owes the reader: which file, and
    /// what came back.
    #[test]
    fn settled_reads_in_an_open_burst_keep_their_own_rows() {
        let calls: Vec<ToolCallBlock> = (0..4)
            .map(|i| {
                call(
                    "read_file",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                    ToolStatus::Ok,
                )
            })
            .collect();
        let lines = render_group_text(&open_group(calls), 100, Locale::Zh);
        let text = lines.join("\n");
        for i in 0..4 {
            assert!(text.contains(&format!("f{i}.rs")), "{text}");
        }
        assert!(
            !text.contains("已读取 4 个文件"),
            "an aggregate must not stand in for the rows: {text}"
        );
        assert!(
            !text.contains('\u{2713}'),
            "reading is still not a result: {text}"
        );
        assert!(
            !text.contains('\u{25b8}'),
            "an open burst is not history yet: {text}"
        );
    }

    /// R3: closed exploration is history, and history keeps its evidence. The
    /// clickable summary row survives as a HEADER over the per-call rows — it
    /// used to be all that was left of them.
    #[test]
    fn closed_reads_keep_a_row_each_under_their_summary() {
        let calls: Vec<ToolCallBlock> = (0..4)
            .map(|i| {
                call(
                    "read_file",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                    ToolStatus::Ok,
                )
            })
            .collect();
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        assert_eq!(
            lines[0].trim_end(),
            "\u{25b8} 读取 4 个文件",
            "the summary stays the click target: {lines:?}"
        );
        let text = lines.join("\n");
        for i in 0..4 {
            assert!(
                text.contains(&format!("f{i}.rs")),
                "history lost file f{i}.rs: {lines:?}"
            );
        }
        assert_eq!(lines.len(), 5, "one header, four evidence rows: {lines:?}");
    }

    /// Each evidence row answers what ran, on what, and what came back.
    #[test]
    fn an_evidence_row_names_the_tool_the_target_and_the_result() {
        let mut c = call("grep", r#"{"pattern":"WorkerQueueGroups"}"#, ToolStatus::Ok);
        c.preview = Some((0..13).map(|i| format!("hit {i}\n")).collect());
        let text = render_group_text(&group(vec![c]), 100, Locale::Zh).join("\n");
        assert!(text.contains("搜索代码"), "the tool: {text}");
        assert!(text.contains("WorkerQueueGroups"), "the target: {text}");
        assert!(text.contains("13"), "the result size: {text}");
    }

    /// R4: a failure is a failure whatever its weight — it keeps the ✗ and
    /// the reason.
    #[test]
    fn a_failed_read_still_reads_as_a_failure() {
        let mut c = call("read_file", r#"{"path":"missing.rs"}"#, ToolStatus::Failed);
        c.preview = Some("no such file".into());
        let lines = render_group_text(&group(vec![c]), 100, Locale::Zh);
        let text = lines.join("\n");
        assert!(text.contains('\u{2717}'), "{text}");
        assert!(text.contains("no such file"), "{text}");
    }

    /// R5: searching is exploration too.
    #[test]
    fn a_settled_search_is_not_marked_as_a_result() {
        let lines = render_group_text(
            &open_group(vec![call("grep", r#"{"pattern":"owner"}"#, ToolStatus::Ok)]),
            100,
            Locale::Zh,
        );
        let text = lines.join("\n");
        assert!(text.contains("搜索代码"), "{text}");
        assert!(
            text.contains("owner"),
            "the pattern it searched for: {text}"
        );
        assert!(
            text.contains('\u{b7}'),
            "quiet dot, not a result mark: {text}"
        );
        assert!(!text.contains('\u{2713}'), "{text}");
    }

    // ── §6: only an edit that LANDED may be presented as a change ──────────

    /// A failed edit must not present its request as a result. The patch text
    /// in `arguments` is what the model asked for; `applied_diff` is what
    /// execution proved. A failure has no applied diff, and rendering the
    /// request in its place showed the user code that is not in their tree —
    /// with a hunk count and a `+N −M` stat line to vouch for it.
    #[test]
    fn a_failed_edit_shows_neither_a_diff_nor_applied_stats() {
        let mut c = call(
            "apply_patch",
            r#"{"patch":"*** Begin Patch\n*** Update File: src/worker.rs\n@@ -41,2 +41,3 @@\n sem chan\n+lightSem chan\n*** End Patch"}"#,
            ToolStatus::Failed,
        );
        c.preview = Some("failed to apply hunk to src/worker.rs: context not found".into());
        let text = render_group_text(&group(vec![c]), 100, Locale::Zh).join("\n");
        assert!(text.contains('\u{2717}'), "a failed edit keeps ✗: {text}");
        assert!(
            text.contains("src/worker.rs"),
            "a failed edit still names its target: {text}"
        );
        assert!(
            !text.contains("lightSem"),
            "a patch that did not apply must not be shown as code: {text}"
        );
        assert!(
            !text.contains("+1") && !text.contains("处修改"),
            "nothing was changed, so no change stats: {text}"
        );
        assert!(
            text.contains("context not found"),
            "the reason it failed is the row's job: {text}"
        );
    }

    /// The same call, applied: the diff and the stats are exactly what a
    /// success owes the reader.
    #[test]
    fn an_applied_edit_shows_the_diff_execution_reported() {
        let mut c = call("apply_patch", r#"{"patch":"ignored"}"#, ToolStatus::Ok);
        c.applied_diff = Some(
            "--- a/src/worker.rs\n+++ b/src/worker.rs\n@@ -41,2 +41,3 @@\n sem chan\n+lightSem chan\n"
                .into(),
        );
        let text = render_group_text(&group(vec![c]), 100, Locale::Zh).join("\n");
        assert!(text.contains("lightSem chan"), "{text}");
        assert!(text.contains("处修改"), "{text}");
        assert!(text.contains("+1"), "{text}");
    }

    /// A new file is an edit: its contents are the change, and a `write_file`
    /// used to render as one collapsed row with the diff nowhere on screen.
    #[test]
    fn a_created_file_shows_its_contents_inline() {
        let mut c = call(
            "write_file",
            r#"{"path":"src/light.rs","content":"fn light() {}\n"}"#,
            ToolStatus::Ok,
        );
        c.applied_diff = Some(
            "--- a/src/light.rs\n+++ b/src/light.rs\n@@ -0,0 +1,2 @@\n+fn light() {}\n+// new\n"
                .into(),
        );
        let text = render_group_text(&group(vec![c]), 100, Locale::Zh).join("\n");
        assert!(text.contains("src/light.rs"), "{text}");
        assert!(
            text.contains("fn light() {}"),
            "created content missing: {text}"
        );
        assert!(text.contains("// new"), "created content truncated: {text}");
    }

    /// The stat line and the rows below it must read from the same source.
    /// Counting the REQUEST while rendering the APPLIED diff let a `+3 −0`
    /// header sit above two changed lines.
    #[test]
    fn edit_stats_count_the_applied_diff_not_the_request() {
        let mut c = call(
            "apply_patch",
            r#"{"patch":"*** Begin Patch\n*** Update File: src/w.rs\n@@ -1,1 +1,4 @@\n keep\n+a\n+b\n+c\n*** End Patch"}"#,
            ToolStatus::Ok,
        );
        // Execution located one of the three requested lines.
        c.applied_diff =
            Some("--- a/src/w.rs\n+++ b/src/w.rs\n@@ -1,1 +1,2 @@\n keep\n+a\n".into());
        let text = render_group_text(&group(vec![c]), 100, Locale::Zh).join("\n");
        assert!(
            text.contains("+1"),
            "stats must match the applied diff: {text}"
        );
        assert!(
            !text.contains("+3"),
            "the request is not the result: {text}"
        );
    }

    /// R6: an edit changed the user's tree. That IS the result, and the one
    /// glyph helper must not weight it down with the reads around it.
    #[test]
    fn an_edit_keeps_the_success_mark() {
        let lines = render_group_text(
            &group(vec![call(
                "apply_patch",
                r#"{"patch":"*** Begin Patch\n*** Update File: a.rs\n-a\n+b\n*** End Patch"}"#,
                ToolStatus::Ok,
            )]),
            100,
            Locale::Zh,
        );
        let text = lines.join("\n");
        assert!(text.contains('\u{2713}'), "an edit is a result: {text}");
    }

    /// R7: hidden probes stay hidden and never inflate a count the user reads.
    #[test]
    fn silent_probes_contribute_neither_rows_nor_counts() {
        let mut calls: Vec<ToolCallBlock> = (0..3)
            .map(|i| {
                call(
                    "read_file",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                    ToolStatus::Ok,
                )
            })
            .collect();
        calls.extend((0..2).map(|i| {
            call(
                "list_files",
                &format!(r#"{{"path":"d{i}"}}"#),
                ToolStatus::Ok,
            )
        }));
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        let text = lines.join("\n");
        assert!(text.contains("读取 3 个文件"), "{text}");
        assert!(!text.contains("d0") && !text.contains("d1"), "{text}");
        assert_eq!(lines.len(), 4, "one header, three visible rows: {lines:?}");
    }

    // ── §5: a real batch renders as a tree, and nothing else does ───────────

    fn batched(name: &str, args: &str, status: ToolStatus, batch: Option<u32>) -> ToolCallBlock {
        let mut c = call(name, args, status);
        c.parallel = true;
        c.batch = batch;
        c
    }

    /// P1: three calls the reducer observed in flight together get one header
    /// and one child row each, with the tree markers saying so.
    #[test]
    fn one_observed_batch_renders_as_a_tree() {
        let calls = vec![
            batched("grep", r#"{"pattern":"a"}"#, ToolStatus::Ok, Some(7)),
            batched("grep", r#"{"pattern":"b"}"#, ToolStatus::Ok, Some(7)),
            batched("read_file", r#"{"path":"c.rs"}"#, ToolStatus::Ok, Some(7)),
        ];
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        let text = lines.join("\n");
        assert!(text.contains("并行"), "the batch names itself: {text}");
        assert_eq!(
            lines.iter().filter(|l| l.contains('\u{251c}')).count(),
            2,
            "two ├ children: {lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains('\u{2514}')).count(),
            1,
            "one └ last child: {lines:?}"
        );
        assert!(text.contains("c.rs"), "children keep their targets: {text}");
    }

    /// P2: the batch header's count is the number of children under it. "3 in
    /// parallel" over two rows is a lie the reader can see.
    #[test]
    fn the_batch_header_counts_the_children_it_has() {
        let mut calls = vec![
            batched("grep", r#"{"pattern":"a"}"#, ToolStatus::Ok, Some(1)),
            batched("grep", r#"{"pattern":"b"}"#, ToolStatus::Ok, Some(1)),
        ];
        // A silent probe in the same burst renders no row, so it is not a child.
        calls.push(batched(
            "list_files",
            r#"{"path":"d"}"#,
            ToolStatus::Ok,
            Some(1),
        ));
        let text = render_group_text(&group(calls), 100, Locale::Zh).join("\n");
        assert!(text.contains('2'), "{text}");
        assert!(
            !text.contains('3'),
            "invisible calls are not children: {text}"
        );
    }

    /// P3: without a batch, two eligible calls are two rows and no tree — the
    /// regression this replaced inferred a batch from the `parallel` flag
    /// alone, so two sequential rounds read as "2 in parallel".
    #[test]
    fn unbatched_calls_never_render_as_parallel() {
        let calls = vec![
            batched("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok, None),
            batched("read_file", r#"{"path":"b.rs"}"#, ToolStatus::Ok, None),
        ];
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        let text = lines.join("\n");
        assert!(!text.contains("并行"), "no batch, no claim: {text}");
        assert!(
            !text.contains('\u{251c}') && !text.contains('\u{2514}'),
            "{text}"
        );
    }

    /// P4: a batch of one is not a batch. One member left visible renders as
    /// an ordinary row.
    #[test]
    fn a_batch_with_one_visible_member_is_a_plain_row() {
        let calls = vec![
            batched("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok, Some(3)),
            batched("list_files", r#"{"path":"d"}"#, ToolStatus::Ok, Some(3)),
        ];
        let text = render_group_text(&group(calls), 100, Locale::Zh).join("\n");
        assert!(!text.contains("并行"), "{text}");
        assert!(text.contains("a.rs"), "{text}");
    }

    /// P5: each child keeps its OWN status. A batch where one call failed must
    /// not present three successes, nor three failures.
    #[test]
    fn each_child_keeps_its_own_status() {
        let mut bad = batched("grep", r#"{"pattern":"b"}"#, ToolStatus::Failed, Some(9));
        bad.preview = Some("regex parse error".into());
        let calls = vec![
            batched("grep", r#"{"pattern":"a"}"#, ToolStatus::Ok, Some(9)),
            bad,
            batched("grep", r#"{"pattern":"c"}"#, ToolStatus::Running, Some(9)),
        ];
        let text = render_group_text(&open_group(calls), 100, Locale::Zh).join("\n");
        assert!(text.contains('\u{2717}'), "the failure shows: {text}");
        assert!(text.contains("regex parse error"), "{text}");
        assert!(text.contains('\u{25cc}'), "the running one shows: {text}");
    }

    /// P6: two distinct bursts in one group stay two trees. Welding them would
    /// claim six calls ran at once.
    #[test]
    fn two_bursts_in_one_group_stay_two_trees() {
        let mut calls: Vec<ToolCallBlock> = (0..2)
            .map(|i| {
                batched(
                    "grep",
                    &format!(r#"{{"pattern":"a{i}"}}"#),
                    ToolStatus::Ok,
                    Some(1),
                )
            })
            .collect();
        calls.extend((0..2).map(|i| {
            batched(
                "grep",
                &format!(r#"{{"pattern":"b{i}"}}"#),
                ToolStatus::Ok,
                Some(2),
            )
        }));
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        assert_eq!(
            lines.iter().filter(|l| l.contains("并行")).count(),
            2,
            "one header per burst: {lines:?}"
        );
    }

    /// P7: a wide burst shows every member. The old renderer capped the live
    /// area at three rows and summarized the rest, so a 17-wide batch hid
    /// fourteen of the objects it was working on.
    #[test]
    fn a_wide_live_burst_shows_every_member() {
        let calls: Vec<ToolCallBlock> = (0..17)
            .map(|i| {
                batched(
                    "read_file",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                    ToolStatus::Running,
                    Some(4),
                )
            })
            .collect();
        let text = render_group_text(&open_group(calls), 100, Locale::Zh).join("\n");
        for i in 0..17 {
            assert!(
                text.contains(&format!("f{i}.rs")),
                "member f{i} hidden: {text}"
            );
        }
    }

    // ── §11: a call held for approval is not a call that is running ─────────

    /// A1: while the human is deciding, the row says so. It used to paint `◌`
    /// — the same mark a command actually executing wears — so a `rm -rf` that
    /// had not been authorised and might never be read as work in progress.
    #[test]
    fn a_call_awaiting_approval_does_not_read_as_running() {
        let c = call(
            "run_command",
            r#"{"program":"rm","args":["-rf","stale"]}"#,
            ToolStatus::Running,
        );
        let id = c.id.clone();
        let lines = render_group_awaiting(&open_group(vec![c]), Some(&id));
        let text = lines.join("\n");
        assert!(text.contains('\u{26a0}'), "waiting marker: {text}");
        assert!(!text.contains('\u{25cc}'), "not the running marker: {text}");
        assert!(text.contains("等待批准"), "{text}");
        assert!(
            text.contains("rm -rf stale"),
            "and it still names the command: {text}"
        );
        assert!(!text.contains('\u{2713}'), "nothing succeeded: {text}");
    }

    /// A2: only the call the approval is about. Another call genuinely running
    /// beside it keeps its running mark.
    #[test]
    fn only_the_call_under_approval_is_marked_waiting() {
        let gated = call(
            "run_command",
            r#"{"program":"rm","args":["-rf","stale"]}"#,
            ToolStatus::Running,
        );
        let other = call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Running);
        let id = gated.id.clone();
        let lines = render_group_awaiting(&open_group(vec![gated, other]), Some(&id));
        let gated_row = lines.iter().find(|l| l.contains("rm")).expect("gated row");
        let other_row = lines
            .iter()
            .find(|l| l.contains("a.rs"))
            .expect("other row");
        assert!(gated_row.contains('\u{26a0}'), "{gated_row}");
        assert!(other_row.contains('\u{25cc}'), "{other_row}");
    }

    /// A3: with no approval open, nothing is marked waiting.
    #[test]
    fn without_an_open_approval_a_running_call_is_running() {
        let c = call(
            "run_command",
            r#"{"program":"rm","args":["-rf","stale"]}"#,
            ToolStatus::Running,
        );
        let text = render_group_awaiting(&open_group(vec![c]), None).join("\n");
        assert!(text.contains('\u{25cc}'), "{text}");
        assert!(!text.contains("等待批准"), "{text}");
    }

    /// §10.7: an answered question is frozen into history — the question it
    /// asked and the answer the user gave, on the record.
    #[test]
    fn an_answered_clarification_keeps_the_question_and_the_answer() {
        let mut c = call(
            "request_user_input",
            r#"{"question":"light_group 未配置时该怎么办?","options":["回退到主组","启动失败"]}"#,
            ToolStatus::Ok,
        );
        c.preview = Some("回退到主组".into());
        let text = render_group_text(&group(vec![c]), 100, Locale::Zh).join("\n");
        assert!(
            text.contains("询问"),
            "the row names the interaction: {text}"
        );
        assert!(
            text.contains("light_group"),
            "the question stays on the record: {text}"
        );
        assert!(
            text.contains("回退到主组"),
            "the answer stays on the record: {text}"
        );
    }

    /// R11: a tool the taxonomy has never heard of is not silently demoted to
    /// exploration — it may well have changed something.
    #[test]
    fn an_unknown_tool_keeps_the_success_mark() {
        let lines = render_group_text(
            &open_group(vec![call(
                "mcp__notion__create_page",
                r#"{"title":"x"}"#,
                ToolStatus::Ok,
            )]),
            100,
            Locale::Zh,
        );
        let text = lines.join("\n");
        assert!(text.contains('\u{2713}'), "forward compatible: {text}");
    }

    /// R12: the aggregate copy and its glyph must survive a narrow terminal.
    #[test]
    fn a_narrow_terminal_truncates_instead_of_panicking() {
        let calls: Vec<ToolCallBlock> = (0..12)
            .map(|i| {
                call(
                    "read_file",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                    ToolStatus::Ok,
                )
            })
            .collect();
        for width in [1usize, 3, 8, 20] {
            let lines = render_group_text(&open_group(calls.clone()), width, Locale::Zh);
            assert!(!lines.is_empty(), "width {width}");
        }
    }

    /// CASE B: a wide read burst gets one header that counts it and one child
    /// row per file, each naming the file. Every member is accounted for on
    /// screen, not in a summary.
    #[test]
    fn a_wide_read_burst_shows_one_header_and_a_row_per_file() {
        let calls: Vec<ToolCallBlock> = (0..7)
            .map(|i| {
                batched(
                    "read_file",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                    ToolStatus::Running,
                    Some(9),
                )
            })
            .collect();
        let lines = render_group_text(&open_group(calls), 100, Locale::Zh);
        assert!(
            lines[0].contains('7'),
            "the header counts the batch: {lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains(".rs")).count(),
            7,
            "every member is on screen: {lines:?}"
        );
        assert!(
            lines.iter().skip(1).all(|l| l.contains("读取文件")),
            "each child names its tool in user language: {lines:?}"
        );
    }

    /// CASE C: hidden probes must not inflate a conversation-facing count.
    /// "5 in parallel" over three visible rows is a lie the user can see.
    #[test]
    fn silent_members_never_inflate_the_batch_count() {
        let mut calls: Vec<ToolCallBlock> = (0..3)
            .map(|i| {
                batched(
                    "read_file",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                    ToolStatus::Running,
                    Some(1),
                )
            })
            .collect();
        calls.extend((0..2).map(|i| {
            batched(
                "list_files",
                &format!(r#"{{"path":"d{i}"}}"#),
                ToolStatus::Running,
                Some(1),
            )
        }));
        let lines = render_group_text(&open_group(calls), 100, Locale::Zh);
        assert!(lines[0].contains('3'), "{lines:?}");
        assert!(
            !lines[0].contains('5'),
            "silent probes are not children: {lines:?}"
        );
    }

    /// CASE F: a tool the taxonomy has never heard of (MCP / future
    /// extension) still folds, but in product language — the conversation
    /// never says "tools".
    #[test]
    fn an_unknown_tool_folds_as_an_operation_not_a_tool() {
        let g = group(vec![
            call("mcp__notion__search", r#"{"q":"a"}"#, ToolStatus::Ok),
            call("mcp__notion__fetch", r#"{"q":"b"}"#, ToolStatus::Ok),
        ]);
        assert!(group_has_disclosure(&g));
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(lines[0].contains("2 项操作"), "{lines:?}");
        assert!(!lines[0].contains("工具"), "{lines:?}");
        let lines = render_group_text(&g, 100, Locale::En);
        assert!(!lines[0].to_lowercase().contains("tool"), "{lines:?}");
    }

    /// CASE E: an edit keeps its own result visible, but it is no longer able
    /// to hold a neighbouring read or shell group open — each finished
    /// activity folds on its own.
    #[test]
    fn an_edit_group_does_not_keep_its_neighbours_expanded() {
        let reads = group(vec![
            call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            call("read_file", r#"{"path":"b.rs"}"#, ToolStatus::Ok),
        ]);
        let edit = group(vec![call(
            "apply_patch",
            r#"{"patch":"*** Begin Patch\n*** Update File: a.rs\n*** End Patch"}"#,
            ToolStatus::Ok,
        )]);
        let shell = group(vec![call(
            "run_command",
            r#"{"command":"cargo test"}"#,
            ToolStatus::Ok,
        )]);
        assert!(group_has_disclosure(&reads), "reads fold on their own");
        assert!(!group_has_disclosure(&edit), "the diff IS the result");
        assert!(group_has_disclosure(&shell), "commands fold on their own");
    }

    /// The conversation has three levels: prompt / prose at column 0, what
    /// the agent DID one level in, and the details of that one level deeper.
    #[test]
    fn tool_activity_renders_one_level_inside_the_narrative() {
        let g = group(vec![call(
            "read_file",
            r#"{"path":"a.rs"}"#,
            ToolStatus::Ok,
        )]);
        let indented = render_activity(
            &g,
            &Theme::no_color(),
            100,
            Locale::Zh,
            Locale::Zh.text(),
            0,
            None,
        );
        let text: Vec<String> = indented
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(
            text.iter().all(|l| l.starts_with("  ")),
            "activity sits inside the narrative: {text:?}"
        );
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
        assert_eq!(lines.len(), 7, "seven settled calls, seven rows: {lines:?}");
        // …and the same group, closed, gains its clickable summary header —
        // one presentation role at a time, never both.
        let mut closed = g.clone();
        closed.open = false;
        assert!(group_has_disclosure(&closed));
        let lines = render_group_text(&closed, 100, Locale::Zh);
        assert_eq!(lines.len(), 8, "header plus the same seven rows: {lines:?}");
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
            live_member_rows(&lines),
            3,
            "each running member (≤ bound) keeps one row: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains('\u{b7}')),
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

    /// §4/§5: a wide burst shows every member. The bound this replaced gave
    /// three rows and counted the other four, so the reader could not see
    /// which files were being read.
    #[test]
    fn a_wide_live_burst_counts_nothing_because_it_hides_nothing() {
        let calls: Vec<ToolCallBlock> = (0..7)
            .map(|i| {
                batched(
                    "read_file",
                    &format!(r#"{{"path":"file{i}.rs"}}"#),
                    ToolStatus::Running,
                    Some(8),
                )
            })
            .collect();
        let lines = render_group_text(&open_group(calls), 100, Locale::Zh);
        assert_eq!(live_member_rows(&lines), 7, "every member shows: {lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains("还有")),
            "nothing is hidden, so nothing is counted: {lines:?}"
        );
    }

    /// While a group is open, every call keeps its own row: the two settled
    /// reads still say which files they read, and the live search says what it
    /// is searching for. The compaction this replaced printed "· 已读取 2 个
    /// 文件" and threw both filenames away.
    #[test]
    fn an_open_group_keeps_a_row_per_call_settled_or_live() {
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
        assert_eq!(lines.len(), 3, "a row per call: {lines:?}");
        assert!(lines[0].contains("service.go"), "{lines:?}");
        assert!(lines[1].contains("bot.go"), "{lines:?}");
        assert!(
            lines[2].contains('◌') && lines[2].contains("TokenPlain"),
            "the live call is marked live: {lines:?}"
        );
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

    /// A batch updates in place: a member that finished keeps its row and
    /// changes its glyph. It does not vanish into a count — which file came
    /// back is exactly what the reader is here for.
    #[test]
    fn batch_members_change_glyph_in_place_as_they_finish() {
        let g = group(vec![
            batched("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok, Some(2)),
            batched(
                "read_file",
                r#"{"path":"b.rs"}"#,
                ToolStatus::Running,
                Some(2),
            ),
            batched("grep", r#"{"pattern":"x"}"#, ToolStatus::Running, Some(2)),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(live_member_rows(&lines), 2, "two still running: {lines:?}");
        let settled = lines
            .iter()
            .find(|l| l.contains("a.rs"))
            .expect("the finished member keeps its row");
        assert!(settled.contains('\u{b7}'), "settled exploration: {settled}");
    }

    /// Once every call finishes, the group is history: the live `◌` rows are
    /// gone and the clickable summary heads the evidence that remains.
    #[test]
    fn a_finished_group_folds_and_drops_the_live_rendering() {
        let g = group(vec![
            call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            call("read_file", r#"{"path":"b.rs"}"#, ToolStatus::Ok),
            call("grep", r#"{"pattern":"x"}"#, ToolStatus::Ok),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(lines[0].starts_with('▸'), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains('◌')),
            "nothing is running any more: {lines:?}"
        );
        assert_eq!(lines.len(), 4, "header plus a row per call: {lines:?}");
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
    fn an_unknown_tool_still_gets_a_readable_row() {
        // MCP-style / future tool the taxonomy has never heard of.
        let g = group(vec![call(
            "mcp__demo__inspect",
            r#"{"target":"repo"}"#,
            ToolStatus::Ok,
        )]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("demo/inspect"), "{lines:?}");
        let mut open = group(vec![call(
            "mcp__demo__inspect",
            r#"{"target":"repo"}"#,
            ToolStatus::Ok,
        )]);
        open.expanded = true;
        let lines = render_group_text(&open, 80, Locale::Zh);
        assert!(
            !lines.iter().any(|l| l.contains("{\"target\"")),
            "expanded detail must not dump raw JSON args: {lines:?}"
        );
    }

    /// The summary label classifies work outside shell/read/search too. Tested
    /// with two calls because a lone call needs no summary — its own row says
    /// everything the summary would.
    #[test]
    fn known_work_tools_outside_shell_read_search_get_a_summary_too() {
        // WebSearch kind → generic work.
        let g = group(vec![
            call("web_search", r#"{"query":"x"}"#, ToolStatus::Ok),
            call("web_search", r#"{"query":"y"}"#, ToolStatus::Ok),
        ]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert!(
            lines[0].starts_with('▸') && lines[0].contains("完成 2 项操作"),
            "web tools are ordinary finished work: {lines:?}"
        );
        // LSP kind reads as search work.
        let g = group(vec![
            call("diagnostics", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            call("diagnostics", r#"{"path":"b.rs"}"#, ToolStatus::Ok),
        ]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert!(
            lines[0].starts_with('▸') && lines[0].contains("搜索代码库"),
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
            lines[0].starts_with('▸') && lines[0].contains("完成 3 项操作"),
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
            lines[0].starts_with('✗'),
            "an unknown failed tool must not read like success: {lines:?}"
        );
        assert!(
            lines[1].contains("connection refused"),
            "the reason rides along: {lines:?}"
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

    #[test]
    fn a_finished_batch_keeps_a_row_per_member_under_its_tree_header() {
        let g = group(
            (0..8)
                .map(|i| {
                    batched(
                        "read_file",
                        &format!(r#"{{"path":"f{i}.rs"}}"#),
                        ToolStatus::Ok,
                        Some(5),
                    )
                })
                .collect(),
        );
        let lines = render_group_text(&g, 100, Locale::Zh);
        let text = lines.join("\n");
        assert!(
            lines.iter().any(|l| l.contains("并行") && l.contains('8')),
            "the batch header counts its members: {lines:?}"
        );
        for i in 0..8 {
            assert!(
                text.contains(&format!("f{i}.rs")),
                "member f{i} lost: {text}"
            );
        }
    }

    #[test]
    fn a_running_batch_shows_the_live_member_and_keeps_the_settled_ones() {
        let mut calls: Vec<ToolCallBlock> = (0..4)
            .map(|i| {
                batched(
                    "read_file",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                    ToolStatus::Ok,
                    Some(6),
                )
            })
            .collect();
        calls[2].status = ToolStatus::Running;
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        let text = lines.join("\n");
        assert!(
            lines.iter().any(|l| l.contains('◌') && l.contains("f2.rs")),
            "the live call is marked live: {text}"
        );
        for i in [0usize, 1, 3] {
            assert!(
                text.contains(&format!("f{i}.rs")),
                "member f{i} lost: {text}"
            );
        }
        assert!(
            !lines.iter().any(|l| l.contains('\u{2713}')),
            "reading is still not a result: {text}"
        );
    }

    #[test]
    fn a_batch_with_failures_names_them_on_its_header_and_keeps_each_row() {
        let mut calls: Vec<ToolCallBlock> = (0..6)
            .map(|i| parallel_call("read_file", &format!(r#"{{"path":"f{i}.rs"}}"#)))
            .collect();
        for i in [1usize, 4] {
            calls[i].status = ToolStatus::Failed;
            calls[i].preview = Some(format!("no such file f{i}.rs"));
        }
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        assert!(
            lines[0].starts_with('▸')
                && lines[0].contains('2')
                && lines[0].contains("失败")
                && lines[0].contains('✗'),
            "the header must name the failures: {:?}",
            lines[0]
        );
        let text = lines.join("\n");
        for i in 0..6 {
            assert!(
                text.contains(&format!("f{i}.rs")),
                "row f{i} missing: {text}"
            );
        }
        assert_eq!(
            lines.iter().filter(|l| l.starts_with('✗')).count(),
            2,
            "each failure marks its own row: {lines:?}"
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
    fn an_observed_batch_gets_a_concurrency_header_under_the_group_summary() {
        let g = group(vec![
            batched("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok, Some(1)),
            batched("grep", r#"{"pattern":"x"}"#, ToolStatus::Ok, Some(1)),
            batched("read_file", r#"{"path":"b.rs"}"#, ToolStatus::Ok, Some(1)),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines[0].starts_with('▸') && lines[0].contains("检查代码库"),
            "reads and searches together are one exploration: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("并行处理 3 项")),
            "the batch says it ran concurrently: {lines:?}"
        );
    }

    #[test]
    fn the_batch_header_is_localized() {
        let g = group(vec![
            batched("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok, Some(1)),
            batched("grep", r#"{"pattern":"x"}"#, ToolStatus::Ok, Some(1)),
        ]);
        let lines = render_group_text(&g, 100, Locale::En);
        assert!(
            lines.iter().any(|l| l.contains("2 tasks in parallel")),
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
            !lines.iter().any(|l| l.contains("并行处理")),
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

    /// Reads and searches together are ONE exploration for the purpose of the
    /// summary label, and four separate pieces of evidence below it.
    #[test]
    fn exploration_calls_share_one_summary_and_keep_four_rows() {
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
        assert!(
            lines[0].contains("检查代码库"),
            "the summary heads them: {lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains("读取文件")).count(),
            2,
            "both reads keep a row without expanding: {lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains("搜索代码")).count(),
            2,
            "both searches keep a row without expanding: {lines:?}"
        );
    }

    /// A lone finished call is one row, and that row is the evidence: the
    /// summary header it used to wear said "读取 1 个文件" and dropped the
    /// filename, which is the only part worth reading.
    #[test]
    fn a_single_read_is_one_row_naming_its_file() {
        let g = group(vec![call(
            "read_file",
            r#"{"path":"src/auth.go"}"#,
            ToolStatus::Ok,
        )]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("读取文件"), "{lines:?}");
        assert!(lines[0].contains("src/auth.go"), "{lines:?}");
        assert!(
            !lines[0].contains("读取 1 个文件"),
            "no summary over a single row: {lines:?}"
        );
    }

    #[test]
    fn ok_read_result_reports_its_size_on_one_row() {
        let mut c = call("read_file", r#"{"path":"README.md"}"#, ToolStatus::Ok);
        c.preview = Some("     1\t# GitCode AI 中间件服务\n     2\t\n     3\tbody".into());
        let g = group(vec![c]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        let joined = lines.join("\n");
        assert!(joined.contains("README.md"), "the target: {joined}");
        assert!(joined.contains("3 行"), "the result size: {joined}");
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
        assert_eq!(lines.len(), 1, "one call, one row: {lines:?}");
        assert!(
            lines[0].starts_with('✓') && lines[0].contains("cargo test"),
            "the row names the command it ran: {lines:?}"
        );
        assert!(
            !lines[0].contains("unused import"),
            "OUTPUT stays folded until asked for: {lines:?}"
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
            !lines.is_empty() && lines[0].starts_with('✗'),
            "a silent failure still surfaces, marked failed: {lines:?}"
        );
        assert!(
            lines[0].contains("missing"),
            "and names its target: {lines:?}"
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
        assert_eq!(lines.len(), 2, "one call: its row and its error: {lines:?}");
        assert!(
            lines[0].starts_with('✗') && lines[0].contains("cargo test"),
            "the row names the failure and the command: {lines:?}"
        );
        assert!(
            lines[1].starts_with("  └ ") && lines[1].contains("error: no such command"),
            "result row carries the first error line: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("long help dump line 2")),
            "OUTPUT is what a fold may hide: {lines:?}"
        );
    }

    /// A search that failed for a real reason must say the real reason.
    ///
    /// The UI used to key its "guard denial" substitution off the tool NAME —
    /// grep, find_files, list_files, git_status — and print
    /// "已跳过：重复的检查无需再次执行" over whatever actually went wrong. That
    /// guard no longer exists in the runtime, so the sentence was invented: a
    /// bad regex read as a skipped duplicate check.
    #[test]
    fn a_search_that_really_failed_reports_its_real_error() {
        for name in ["grep", "find_files", "list_files", "git_status"] {
            let mut c = call(name, r#"{"pattern":"[unclosed"}"#, ToolStatus::Failed);
            c.preview = Some("regex parse error: unclosed character class".into());
            let mut g = group(vec![c]);
            g.expanded = true;
            let joined = render_group_text(&g, 100, Locale::Zh).join("\n");
            assert!(
                joined.contains("regex parse error"),
                "{name} must report what went wrong: {joined}"
            );
            assert!(
                !joined.contains("重复的检查"),
                "{name} must not be given an invented reason: {joined}"
            );
        }
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
            None,
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
