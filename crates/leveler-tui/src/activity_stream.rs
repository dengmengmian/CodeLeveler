//! Conversation activity stream: per-call tool evidence.
//!
//! Product surface, not a tool trace — but evidence, not a summary. Every
//! user-visible call owns a row that answers four questions: which tool ran, on
//! what, what came back, and what state it is in. An aggregate that replaced
//! those rows ("读取 4 个文件", "检查代码库") answered none of them.
//!
//! - **Silent** tools (ls/find probes, goal bookkeeping): hidden per-call.
//! - **User-visible**: every Normal/Important call renders a head row (`›`
//!   anchor + action + inline target) and, collapsed, one `└` result line. No
//!   whole-line tinting: only the status glyph carries a status color.
//! - `›` is the transcript's first-level execution anchor (`▌` user, `●`
//!   agent, `›` tool). It says a tool ran here — never a status, a fold state
//!   or a cursor.
//! - Consecutive calls of the SAME tool read as one RUN: the tool is named
//!   once, and each call becomes a `├─`/`└─` child carrying only what differs.
//!   A count of children would be the children said twice; a count of
//!   FAILURES rides on the head, because nothing else shows it at a glance.
//! - A finished group that is not a run keeps a clickable `▸/▾` summary as a
//!   HEADER over its rows. That fold governs each call's OUTPUT — the one
//!   thing it may hide. A lone call gets no header: its own row already says
//!   everything. Either way the group's FIRST row is the click target.
//! - Calls the reducer OBSERVED in flight together ([`ToolCallBlock::batch`])
//!   keep their own header, which is the ONLY row that claims concurrency:
//!   a run's tree says "same tool, one after another", never "at once".
//! - Consecutive same-file patches merge into one edit node whose diff is
//!   always complete; consecutive identical failures merge with a `×N` count.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::i18n::{Locale, UiText};
use crate::render::truncate_display;
use crate::theme::{Ink, Theme};
use crate::tool_cell::{tool_action_label_for, tool_summary_pub};
use crate::tool_taxonomy::{ActivityVisibility, activity_visibility};
use crate::transcript::{StopRequest, ToolCallBlock, ToolGroupBlock, ToolStatus};

/// Render a tool group for the Conversation activity stream.
///
/// Every user-visible call in the group owns a row, whatever the group's
/// state (§4). The row says which tool ran, on what, what came back, and what
/// state it is in — four questions an aggregate ("读取 4 个文件") answers none
/// of. Finished history keeps its clickable `▸/▾` summary as a HEADER over
/// those rows, not in place of them: that row governs how much of each call's
/// OUTPUT is shown, and output is the only thing a fold may hide.
#[cfg(test)]
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
    let mut rows = Vec::new();
    render_group_rows(
        group,
        theme,
        width,
        locale,
        t,
        now_elapsed_secs,
        awaiting_approval,
        None,
        &mut rows,
    )
}

/// A command call's clickable rows inside a rendered group: its head and
/// command lines toggle its output. `stoppable` says whether the call is still
/// running and can be stopped by this client — the row no longer carries a
/// permanent stop label; the stop is a contextual action on the focused row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommandRow {
    pub line: usize,
    /// Index of the call in its group's `calls`.
    pub call: usize,
    pub stoppable: bool,
}

/// [`render_group`], also reporting where each command call's rows landed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_group_rows(
    group: &ToolGroupBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    now_elapsed_secs: u64,
    awaiting_approval: Option<&leveler_client_protocol::ToolCallId>,
    focused_command: Option<&leveler_client_protocol::ToolCallId>,
    rows: &mut Vec<CommandRow>,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let index_of = |call: &ToolCallBlock| {
        group
            .calls
            .iter()
            .position(|c| std::ptr::eq(c, call))
            .unwrap_or(0)
    };
    let visible: Vec<&ToolCallBlock> = group
        .calls
        .iter()
        .filter(|c| is_conversation_visible(c))
        .collect();
    let units = plan_units(&group.calls);
    // A group header earns its row only by ADDING something. Over a lone call
    // "读取 1 个文件" is the row beneath it said twice; over a run, each run
    // already names its tool and counts its own failures; over a mixed
    // stretch, "检查代码库" is what the rows below say, in their own words,
    // with their own targets. What no single row can say — how many of them
    // failed, how many lack a permission — keeps the header alive.
    //
    // The group's FIRST row is the click target either way (see
    // `conversation::build`), so dropping the header costs no interaction.
    let runs = units.iter().any(|u| match u {
        StreamUnit::Run(_) => true,
        StreamUnit::Batch(calls) => is_uniform(calls),
        _ => false,
    });
    if group_has_disclosure(group) && visible.len() > 1 && !runs {
        let header = disclosure_presentation(&visible, group.expanded, t);
        if header_adds_information(&header) {
            out.push(crate::presentation::disclosure::header_line(
                &header, theme, width,
            ));
        }
    }
    for unit in units {
        match unit {
            StreamUnit::Single(call) if is_shell_call(call) => {
                let at = out.len();
                let (lines, stoppable) = command_unit_lines(
                    call,
                    theme,
                    width,
                    locale,
                    t,
                    group.expanded,
                    now_elapsed_secs,
                    None,
                    awaits(call, awaiting_approval),
                    focused_command,
                );
                push_command_rows(rows, at, index_of(call), stoppable, lines.len());
                out.extend(lines);
            }
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
                    true,
                ));
                push_expanded_detail(call, group.expanded, theme, width, locale, t, &mut out);
            }
            StreamUnit::Run(calls) => {
                push_run(
                    &calls,
                    None,
                    group,
                    theme,
                    width,
                    locale,
                    t,
                    now_elapsed_secs,
                    awaiting_approval,
                    &mut out,
                );
            }
            // A burst of ONE tool is a run that also happened concurrently:
            // same shape, and the parallel fact kept on its head. A burst of
            // different tools cannot lose their names, so it keeps its own.
            StreamUnit::Batch(calls) if is_uniform(&calls) => {
                let label = t.parallel_header.replace("{}", &calls.len().to_string());
                push_run(
                    &calls,
                    Some(label),
                    group,
                    theme,
                    width,
                    locale,
                    t,
                    now_elapsed_secs,
                    awaiting_approval,
                    &mut out,
                );
            }
            StreamUnit::Batch(calls) => {
                // The header's count is the number of children drawn below it,
                // taken from the same projection — never the batch's raw size.
                let running = calls.iter().any(|c| c.status == ToolStatus::Running);
                let (glyph, color) = if running {
                    ("\u{25cc} ", theme.accent.primary)
                } else {
                    ("\u{b7} ", theme.ink(Ink::Subtle))
                };
                let label = t.parallel_header.replace("{}", &calls.len().to_string());
                out.push(Line::from(vec![
                    Span::styled(glyph, Style::default().fg(color)),
                    Span::styled(
                        truncate_display(&label, width.saturating_sub(2).max(1)),
                        Style::default().fg(theme.ink(Ink::Meta)),
                    ),
                ]));
                let last = calls.len() - 1;
                for (i, call) in calls.iter().enumerate() {
                    let branch = if i == last { "\u{2514} " } else { "\u{251c} " };
                    if is_shell_call(call) {
                        let at = out.len();
                        let (lines, stoppable) = command_unit_lines(
                            call,
                            theme,
                            width,
                            locale,
                            t,
                            group.expanded,
                            now_elapsed_secs,
                            Some(branch),
                            awaits(call, awaiting_approval),
                            focused_command,
                        );
                        push_command_rows(rows, at, index_of(call), stoppable, lines.len());
                        out.extend(lines);
                        continue;
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
                        Some(branch),
                        awaits(call, awaiting_approval),
                        true,
                    ));
                    push_expanded_detail(call, group.expanded, theme, width, locale, t, &mut out);
                }
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
                    None,
                    // A settled failure is not waiting on anybody.
                    false,
                    true,
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
    // A command that ran without the network it needed lacks a permission;
    // it is counted apart from the failures.
    let needs_network = visible
        .iter()
        .filter(|c| c.status == ToolStatus::Failed && needs_network_permission(c))
        .count();
    let failed = visible
        .iter()
        .filter(|c| c.status == ToolStatus::Failed)
        .count()
        - needs_network;
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
        needs_permission_suffix: (needs_network > 0 && visible.len() > 1).then(|| {
            t.batch_needs_network
                .replace("{}", &needs_network.to_string())
        }),
        expanded,
        drill_down: false,
        duration_ms,
        first_error: (!expanded).then(|| first_error_line(visible)).flatten(),
    }
}

/// Whether a group header says anything its own rows cannot.
///
/// The label never does: every row names its tool, its target and its result
/// in the same user language the label is written in. An aggregate does —
/// "2 failed" over eight rows is a fact about the stretch, not about any row
/// in it, and the same goes for calls that lack a permission rather than
/// having failed on their own. (`duration_ms` is authoritative only for a lone
/// call, which never reaches this header, and `first_error` belongs to the
/// collapsed form this surface does not render.)
fn header_adds_information(p: &crate::presentation::disclosure::DisclosurePresentation) -> bool {
    p.failed > 0 || p.needs_permission_suffix.is_some()
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
        .find(|c| c.status == ToolStatus::Failed && !needs_network_permission(c))
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

/// The transcript's first-level execution anchor: `▌` user, `●` agent, `›`
/// tool. It says a tool ran here — never a status, a fold state or a cursor.
pub(crate) const TOOL_ANCHOR: &str = "\u{203a}";

/// The glyph a finished exploration used to wear. It is no longer drawn: the
/// anchor opens the row, and this marked a success the same way a blank did.
const EXPLORED: &str = "\u{b7}";

enum StreamUnit<'a> {
    Single(&'a ToolCallBlock),
    /// Two or more consecutive calls of the SAME tool: one head naming the
    /// tool, a `├─`/`└─` child per call. Purely how the stretch READS — the
    /// calls themselves are untouched, and a run never spans a group boundary
    /// (prose, a user message or a different KIND of work already closed it).
    Run(Vec<&'a ToolCallBlock>),
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
    merge_runs(out)
}

/// Collapse consecutive singles of the SAME tool into one run.
///
/// This runs over the planned units, never over the calls: a batch, an edit
/// merge and a failure merge have already claimed their members and keep their
/// own shape. Commands are left alone too — their row carries a stop action and
/// an output fold whose columns are reported back to the hit-test.
fn merge_runs(units: Vec<StreamUnit<'_>>) -> Vec<StreamUnit<'_>> {
    let mut out: Vec<StreamUnit<'_>> = Vec::with_capacity(units.len());
    for unit in units {
        let StreamUnit::Single(call) = unit else {
            out.push(unit);
            continue;
        };
        if is_shell_call(call) {
            out.push(StreamUnit::Single(call));
            continue;
        }
        match out.last_mut() {
            Some(StreamUnit::Run(run)) if run[0].name == call.name => run.push(call),
            Some(StreamUnit::Single(prev)) if prev.name == call.name && !is_shell_call(prev) => {
                let prev = *prev;
                out.pop();
                out.push(StreamUnit::Run(vec![prev, call]));
            }
            _ => out.push(StreamUnit::Single(call)),
        }
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
    // False for a run child, whose head already named the tool.
    show_action: bool,
) -> Vec<Line<'static>> {
    // Plan/goal guard rejections carry internal English validation text for the
    // model — show a warning glyph and a localized note instead. Other failures
    // are real errors: the result row shows the first error line.
    let guard_denial = call.status == ToolStatus::Failed
        && matches!(call.name.as_str(), "update_plan" | "update_goal");
    let unanswered = unanswered_question_note(call, t).is_some();
    let (glyph, glyph_color) = if guard_denial || awaiting_approval || unanswered {
        ("⚠", theme.status.warning)
    } else {
        status_glyph(call, theme)
    };
    let action = if call.name == "task" {
        t.unsupported_task_action.to_string()
    } else {
        crate::tool_cell::tool_call_label(&call.name, &call.arguments, locale)
    };

    // Trailing status marker: a running call shows its live elapsed time so a
    // long command (e.g. `go test`) is visibly working rather than a static
    // block; a finished call shows its final duration.
    let tail = match call.status {
        // Held for approval: a clock would say the command is taking a while,
        // when it has not started. Say what it is actually waiting for.
        ToolStatus::Running if awaiting_approval => format!(" · {}", t.approval_pending),
        ToolStatus::Running => {
            let secs = call.running_secs(now_elapsed_secs);
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
    // Row opener: a child of a tree wears its branch, everything else wears the
    // execution anchor. `›` says "a tool ran here" and nothing else — not a
    // status, not a fold state, not a cursor.
    head.push(Span::styled(
        branch
            .map(str::to_string)
            .unwrap_or_else(|| format!("{TOOL_ANCHOR} ")),
        Style::default().fg(theme.ink(Ink::Subtle)),
    ));
    // The muted `·` was a bullet, not a fact: it marked a successful read the
    // same way a blank would. The anchor opens the row now, so only glyphs
    // that SAY something (✗ ◌ ⊘ ⚠ ✓-on-a-result) still earn their column.
    if glyph != EXPLORED {
        head.push(Span::styled(
            format!("{glyph} "),
            Style::default().fg(glyph_color),
        ));
    }

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
    let has_summary = !summary.is_empty() && summary != "{}";
    // A visible tool row must say WHAT ran. A run child may omit its action
    // when the run head already carries the tool's own label AND the target
    // differs; anything else — a more specific action, or no target at all —
    // would leave elapsed time and a line count standing alone.
    let render_action =
        show_action || !has_summary || action != tool_action_label_for(&call.name, locale);
    if render_action {
        head.push(Span::styled(
            action.clone(),
            Style::default().fg(body_ink(call.status, theme)),
        ));
    }
    if has_summary {
        let shell = is_shell_call(call)
            && crate::tool_cell::summary_is_command_line(&call.name, &call.arguments);
        let used: usize = head
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum::<usize>()
            + usize::from(render_action) * 2
            + usize::from(shell) * 2;
        let avail = width
            .saturating_sub(used + UnicodeWidthStr::width(tail.as_str()) + 8)
            .max(8);
        // The label needs a gap before its target; a branch or anchor already
        // ended in one.
        if render_action {
            head.push(Span::raw("  "));
        }
        if shell {
            head.push(Span::styled(
                "$ ",
                Style::default().fg(theme.ink(Ink::Meta)),
            ));
        }
        head.push(Span::styled(
            truncate_display(&summary, avail),
            Style::default().fg(body_ink(call.status, theme)),
        ));
    }
    if !tail.is_empty() {
        head.push(Span::styled(
            tail,
            Style::default().fg(theme.ink(Ink::Meta)),
        ));
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
            let at_least = if preview_truncated(call) { "+" } else { "" };
            head.push(Span::styled(
                format!(" · {pre}{n}{at_least}{post}"),
                Style::default().fg(theme.ink(Ink::Meta)),
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
            &child_rail(branch),
        ));
    }
    out
}

/// Whether every call in a stretch ran the same tool, which is what lets one
/// head speak for all of them.
fn is_uniform(calls: &[&ToolCallBlock]) -> bool {
    calls
        .iter()
        .all(|c| c.name == calls[0].name && !is_shell_call(c))
}

/// One run: a head naming the tool once (plus whatever else is true of the
/// stretch — it ran in parallel, some of it failed) and a `├─`/`└─` child per
/// call carrying only what differs.
#[allow(clippy::too_many_arguments)]
fn push_run(
    calls: &[&ToolCallBlock],
    // A fact about the stretch itself, already localized ("6 tasks in
    // parallel"). Never a count of the children — they are right there.
    stretch: Option<String>,
    group: &ToolGroupBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    now_elapsed_secs: u64,
    awaiting_approval: Option<&leveler_client_protocol::ToolCallId>,
    out: &mut Vec<Line<'static>>,
) {
    // A run is active while any of its calls is; once all are settled the head
    // recedes with them. The children carry the per-call state either way.
    let run_ink = if calls.iter().any(|c| c.status == ToolStatus::Running) {
        theme.ink(Ink::Active)
    } else {
        theme.ink(Ink::Settled)
    };
    let mut head = vec![
        Span::styled(
            format!("{TOOL_ANCHOR} "),
            Style::default().fg(theme.ink(Ink::Subtle)),
        ),
        Span::styled(
            tool_action_label_for(&calls[0].name, locale),
            Style::default().fg(run_ink),
        ),
    ];
    if let Some(stretch) = stretch {
        head.push(Span::styled(
            format!(" \u{b7} {stretch}"),
            Style::default().fg(theme.ink(Ink::Meta)),
        ));
    }
    // A count of children is what the children already show; a count of
    // FAILURES is not. It rides on the head so a run that went wrong says so
    // on the row that names it.
    head.extend(run_attention_suffix(calls, theme, t));
    out.push(Line::from(head));
    let last = calls.len() - 1;
    for (i, call) in calls.iter().enumerate() {
        let branch = if i == last {
            "  \u{2514}\u{2500} "
        } else {
            "  \u{251c}\u{2500} "
        };
        out.extend(run_child_lines(
            call,
            theme,
            width,
            locale,
            t,
            group.expanded,
            now_elapsed_secs,
            branch,
            awaits(call, awaiting_approval),
        ));
        let body = out.len();
        push_expanded_detail(call, group.expanded, theme, width, locale, t, out);
        indent_onto_rail(&mut out[body..], &child_rail(Some(branch)), theme);
    }
}

/// The rail a tree child's continuation rows ride under: still inside the
/// tree while siblings follow, clear of it once this is the last child.
fn child_rail(branch: Option<&str>) -> String {
    let Some(branch) = branch else {
        return String::new();
    };
    let width = UnicodeWidthStr::width(branch);
    if !branch.trim_start().starts_with('\u{251c}') {
        return " ".repeat(width);
    }
    let indent = branch.len() - branch.trim_start().len();
    let rail = format!("{}\u{2502}", " ".repeat(indent));
    let pad = width.saturating_sub(UnicodeWidthStr::width(rail.as_str()));
    format!("{rail}{}", " ".repeat(pad))
}

/// Indent already-built lines onto a tree rail, so a child's body reads as
/// belonging to that child instead of as another branch.
fn indent_onto_rail(lines: &mut [Line<'static>], rail: &str, theme: &Theme) {
    if rail.is_empty() {
        return;
    }
    for line in lines {
        line.spans.insert(
            0,
            Span::styled(
                rail.to_string(),
                Style::default().fg(theme.ink(Ink::Subtle)),
            ),
        );
    }
}

/// What went wrong inside a run, on the row that names it: failures, and
/// calls that lack a permission rather than having failed on their own. Empty
/// for a clean run — a run that worked needs no adjective.
fn run_attention_suffix(calls: &[&ToolCallBlock], theme: &Theme, t: &UiText) -> Vec<Span<'static>> {
    let needs_network = calls
        .iter()
        .filter(|c| c.status == ToolStatus::Failed && needs_network_permission(c))
        .count();
    let failed = calls
        .iter()
        .filter(|c| c.status == ToolStatus::Failed)
        .count()
        - needs_network;
    let mut out = Vec::new();
    if failed > 0 {
        out.push(Span::styled(
            format!(
                " \u{b7} \u{2717} {}",
                t.batch_failed.replace("{}", &failed.to_string())
            ),
            Style::default().fg(theme.status.warning),
        ));
    }
    if needs_network > 0 {
        out.push(Span::styled(
            format!(
                " \u{b7} \u{26a0} {}",
                t.batch_needs_network
                    .replace("{}", &needs_network.to_string())
            ),
            Style::default().fg(theme.status.warning),
        ));
    }
    out
}

/// One child of a tool run: the branch, then only what differs from its
/// siblings — the target, how it ended, how much came back.
#[allow(clippy::too_many_arguments)]
fn run_child_lines(
    call: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    expanded: bool,
    now_elapsed_secs: u64,
    branch: &str,
    awaiting_approval: bool,
) -> Vec<Line<'static>> {
    unit_lines(
        call,
        theme,
        width,
        locale,
        t,
        expanded,
        1,
        None,
        now_elapsed_secs,
        Some(branch),
        awaiting_approval,
        false,
    )
}

/// Record a command unit's head and command lines as click targets.
fn push_command_rows(
    rows: &mut Vec<CommandRow>,
    at: usize,
    call: usize,
    stoppable: bool,
    lines: usize,
) {
    rows.push(CommandRow {
        line: at,
        call,
        stoppable,
    });
    if lines > 1 {
        rows.push(CommandRow {
            line: at + 1,
            call,
            stoppable,
        });
    }
}

/// Most output rows an expanded command shows; the rest is named, not drawn.
const COMMAND_OUTPUT_ROWS: usize = 20;

/// Output rows a RUNNING command shows under its row: enough to see it move.
pub(crate) const LIVE_TAIL_ROWS: usize = 6;

/// Diff rows a settled edit keeps on screen before the rest is counted.
pub(crate) const DIFF_PREVIEW_ROWS: usize = 24;

/// Rows a resolved clarification's answer may take in the transcript before
/// the rest is folded. A multi-question answer is one row per question, and
/// the whole set is the record of what the user decided.
const ANSWER_SUMMARY_ROWS: usize = 6;

/// A command's lifecycle, as its row states it: a status glyph and the facts
/// that follow the command — how long, how it ended, how much it printed.
/// Every fact comes from the runtime or this client's own stop request, never
/// from reading the output: stderr on a success is still a success.
struct CommandHead {
    glyph: &'static str,
    glyph_color: ratatui::style::Color,
    /// Rendered after the command as ` · a · b`, in order.
    facts: Vec<String>,
    stoppable: bool,
}

fn command_head(
    call: &ToolCallBlock,
    theme: &Theme,
    t: &UiText,
    now_elapsed_secs: u64,
    awaiting_approval: bool,
) -> CommandHead {
    let duration = call.duration_ms.map(|ms| {
        if ms < 60_000 {
            format!("{:.1}s", ms as f64 / 1000.0)
        } else {
            crate::status_line::fmt_elapsed(ms / 1000)
        }
    });
    let head = |glyph, glyph_color, facts: Vec<Option<String>>, stoppable| CommandHead {
        glyph,
        glyph_color,
        facts: facts.into_iter().flatten().collect(),
        stoppable,
    };
    match call.status {
        // Announced, not authorised: a clock would say it is taking a while
        // when it has not started.
        ToolStatus::Running if awaiting_approval => head(
            "\u{26a0}",
            theme.status.warning,
            vec![Some(t.approval_pending.to_string())],
            false,
        ),
        ToolStatus::Running => match call.stop {
            StopRequest::Sent => head(
                "\u{25cc}",
                theme.accent.primary,
                vec![Some(t.command_stopping.to_string())],
                false,
            ),
            StopRequest::Uncertain => head(
                "?",
                theme.status.warning,
                vec![Some(t.command_stop_unknown.to_string())],
                true,
            ),
            StopRequest::None => head(
                "\u{25cc}",
                theme.accent.primary,
                vec![Some(crate::status_line::fmt_elapsed(
                    call.running_secs(now_elapsed_secs),
                ))],
                true,
            ),
        },
        // A backgrounded command was STARTED, not finished: the call returns
        // the moment the process detaches. `↗` is the mark the activity strip
        // already gives a background process; its detail lives there.
        ToolStatus::Ok if started_in_background(call) => head(
            "\u{2197}",
            theme.accent.primary,
            vec![Some(t.command_backgrounded.to_string())],
            false,
        ),
        ToolStatus::Ok => head(
            "\u{2713}",
            theme.status.success,
            vec![duration, command_output_count(call, t)],
            false,
        ),
        ToolStatus::Failed if needs_network_permission(call) => head(
            "\u{26a0}",
            theme.status.warning,
            vec![Some(t.command_needs_network.to_string())],
            false,
        ),
        ToolStatus::Failed if call_timed_out(call) => head(
            "\u{2717}",
            theme.status.error,
            vec![
                duration,
                Some(t.result_timeout.trim_start_matches(" · ").to_string()),
            ],
            false,
        ),
        ToolStatus::Failed => head(
            "\u{2717}",
            theme.status.error,
            vec![
                duration,
                call.exit_code
                    .filter(|code| *code != 0)
                    .map(|code| format!("exit {code}")),
            ],
            false,
        ),
        ToolStatus::Cancelled => head(
            "\u{2298}",
            theme.text.muted,
            vec![Some(t.command_stopped.to_string()), duration],
            false,
        ),
        ToolStatus::Unknown => head(
            "?",
            theme.status.warning,
            vec![Some(t.command_unknown.to_string())],
            false,
        ),
    }
}

/// `137 行` for a finished command's output: counted, never poured into the
/// transcript. `+` marks a count taken from a preview the runtime capped.
fn command_output_count(call: &ToolCallBlock, t: &UiText) -> Option<String> {
    let streamed = call.output.lines().filter(|l| !l.trim().is_empty()).count();
    let previewed = preview_body_lines(call).len();
    let n = streamed.max(previewed);
    if n == 0 {
        return None;
    }
    let capped = streamed < previewed && preview_truncated(call) || call.output_truncated;
    let (pre, post) = split_placeholder(t.tool_output_lines);
    Some(format!("{pre}{n}{}{post}", if capped { "+" } else { "" }))
}

/// One command call: ONE summary row — opener, status glyph, the command,
/// then its facts — and, when its row is opened or its group expanded, its
/// output. A failure keeps one quiet line naming what broke. Returns the lines
/// and whether the call is stoppable right now.
///
/// The command is the flexible cell and the facts are fixed: a long command is
/// cut before its duration or exit code is. The head carries NO permanent stop
/// label: stopping is a contextual action on the focused row, whose opener
/// becomes the selection marker the activity strip uses.
#[allow(clippy::too_many_arguments)]
fn command_unit_lines(
    call: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    group_expanded: bool,
    now_elapsed_secs: u64,
    branch: Option<&str>,
    awaiting_approval: bool,
    focused_command: Option<&leveler_client_protocol::ToolCallId>,
) -> (Vec<Line<'static>>, bool) {
    let head = command_head(call, theme, t, now_elapsed_secs, awaiting_approval);
    let subtle = Style::default().fg(theme.ink(Ink::Subtle));
    let meta = Style::default().fg(theme.ink(Ink::Meta));
    let body = Style::default().fg(body_ink(call.status, theme));
    let opener = if focused_command == Some(&call.id) {
        "\u{2192} ".to_string()
    } else {
        branch
            .map(str::to_string)
            .unwrap_or_else(|| format!("{TOOL_ANCHOR} "))
    };
    let prompt = crate::tool_cell::summary_is_command_line(&call.name, &call.arguments);
    let mut command = strip_inline_md(&tool_summary_pub(&call.name, &call.arguments, t));
    // A shell call the runtime refused can carry no renderable command line.
    // Duration and exit code would then be the whole row, so name the tool:
    // every visible row must say what ran.
    if command.trim().is_empty() {
        command = crate::tool_cell::tool_action_label_for(&call.name, locale);
    }
    let facts: String = head.facts.iter().map(|f| format!(" \u{b7} {f}")).collect();

    let mut spans = vec![
        Span::styled(opener, subtle),
        Span::styled(
            format!("{} ", head.glyph),
            Style::default().fg(head.glyph_color),
        ),
    ];
    if prompt {
        spans.push(Span::styled("$ ", meta));
    }
    let used: usize = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let room = width.saturating_sub(used + UnicodeWidthStr::width(facts.as_str()));
    spans.push(Span::styled(truncate_display(&command, room.max(1)), body));
    if !facts.is_empty() {
        spans.push(Span::styled(facts, meta));
    }
    let mut out = vec![clip_line(spans, width)];

    // Continuation rows ride the tree's rail when this call is a child.
    let rail = match branch {
        Some(b) if b.trim_start().starts_with('\u{251c}') => child_rail(Some(b)),
        Some(b) => " ".repeat(UnicodeWidthStr::width(b)),
        None => String::new(),
    };
    let body_indent = format!("{rail}    ");
    let open = call.expanded || group_expanded;
    if open {
        // Live output while it streamed; a call seen only finished (history,
        // replay) has just the runtime's preview.
        let logical: Vec<&str> = if !call.output.is_empty() {
            call.output.lines().collect()
        } else {
            preview_body_lines(call)
        };
        let hidden = logical.len().saturating_sub(COMMAND_OUTPUT_ROWS);
        if hidden > 0 || call.output_truncated {
            out.push(clip_line(
                vec![Span::styled(
                    format!(
                        "{body_indent}{}",
                        t.command_output_hidden
                            .replace("{}", &hidden.max(1).to_string())
                    ),
                    meta,
                )],
                width,
            ));
        }
        let avail = width
            .saturating_sub(UnicodeWidthStr::width(body_indent.as_str()))
            .max(1);
        for line in &logical[hidden..] {
            out.push(clip_line(
                vec![Span::styled(
                    format!("{body_indent}{}", truncate_display(line, avail)),
                    Style::default().fg(theme.ink(Ink::Settled)),
                )],
                width,
            ));
        }
    } else if call.status == ToolStatus::Running
        && crate::tool_taxonomy::result_lifetime(&call.name)
            == crate::tool_taxonomy::ResultLifetime::Transient
    {
        // Live: the last few lines it printed, so the user sees it move. The
        // tail is PROCESS — it leaves the moment the call settles (the branch
        // above no longer matches), and the full output stays one Enter away.
        let logical: Vec<&str> = call
            .output
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let hidden = logical.len().saturating_sub(LIVE_TAIL_ROWS);
        let stem = format!("{rail}  \u{2514} ");
        let indent = " ".repeat(UnicodeWidthStr::width(stem.as_str()));
        let room = width
            .saturating_sub(UnicodeWidthStr::width(stem.as_str()))
            .max(1);
        let mut first = true;
        let mut lead = || {
            let lead = if first { stem.clone() } else { indent.clone() };
            first = false;
            lead
        };
        if hidden > 0 {
            out.push(clip_line(
                vec![
                    Span::styled(lead(), subtle),
                    Span::styled(
                        format!(
                            "\u{2026} {}",
                            t.command_output_hidden.replace("{}", &hidden.to_string())
                        ),
                        meta,
                    ),
                ],
                width,
            ));
        }
        for line in &logical[hidden..] {
            out.push(clip_line(
                vec![
                    Span::styled(lead(), subtle),
                    Span::styled(
                        truncate_display(line, room),
                        Style::default().fg(theme.ink(Ink::Settled)),
                    ),
                ],
                width,
            ));
        }
    } else if call.status == ToolStatus::Failed
        && !call_timed_out(call)
        && let Some(note) = failed_one_line_summary(call, t)
    {
        let stem = format!("{rail}  \u{2514} ");
        let room = width.saturating_sub(UnicodeWidthStr::width(stem.as_str()));
        out.push(clip_line(
            vec![
                Span::styled(stem, subtle),
                Span::styled(
                    truncate_display(&note, room.max(1)),
                    Style::default().fg(theme.ink(Ink::Settled)),
                ),
            ],
            width,
        ));
    }
    (out, head.stoppable)
}

/// Cut a row at `width` display cells, span by span, so no cell is written
/// past the edge whatever a narrow terminal leaves for the fixed parts.
fn clip_line(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut used = 0;
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        let w = UnicodeWidthStr::width(span.content.as_ref());
        if used + w <= width {
            used += w;
            out.push(span);
            continue;
        }
        let mut cut = String::new();
        for ch in span.content.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cw > width {
                break;
            }
            used += cw;
            cut.push(ch);
        }
        if !cut.is_empty() {
            out.push(Span::styled(cut, span.style));
        }
        break;
    }
    Line::from(out)
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

/// What happened to a question the user did not answer, in their words — the
/// result otherwise carries the runtime's note to the model
/// (`leveler_agent` `handle_clarification`). `None` for a real answer.
fn unanswered_question_note<'a>(call: &ToolCallBlock, t: &'a UiText) -> Option<&'a str> {
    if !is_user_decision_call(call) || call.status != ToolStatus::Ok {
        return None;
    }
    let preview = call.preview.as_deref()?.trim_start();
    if preview.starts_with("The user did not respond in time") {
        Some(t.question_timed_out)
    } else if preview.starts_with("No user is available to answer") {
        Some(t.question_unattended)
    } else if preview.starts_with("The user saw this question and chose to skip it") {
        Some(t.question_skipped)
    } else {
        None
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

/// The ink for a call's own words — its action label and the target or command
/// it ran on.
///
/// Brightness states the row's LIFECYCLE, not its tool category: work still in
/// flight keeps the primary ink, while a settled trace recedes to the secondary
/// ink so a long session's accumulated reads, searches and commands never
/// compete with the agent's current prose. A failure or an unresolved call
/// keeps the primary ink — it is still asking for attention, and its status
/// glyph carries the colour. The muted ink stays reserved for chrome (anchor,
/// rail, elapsed, counts), so metadata reads quieter than the row it belongs to
/// at either tier. This is a real foreground token, never `Modifier::DIM`.
fn body_ink(status: ToolStatus, theme: &Theme) -> ratatui::style::Color {
    match status {
        ToolStatus::Running | ToolStatus::Failed | ToolStatus::Unknown => theme.ink(Ink::Active),
        ToolStatus::Ok | ToolStatus::Cancelled => theme.ink(Ink::Settled),
    }
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
        ToolStatus::Cancelled => ("\u{2298}", theme.text.muted),
        ToolStatus::Unknown => ("?", theme.status.warning),
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
#[allow(clippy::too_many_arguments)]
fn result_lines_for(
    call: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    t: &UiText,
    expanded: bool,
    guard_denial: bool,
    repeat: usize,
    // The tree rail a child of a run/batch continues under, so its result row
    // reads as belonging to that child instead of as another branch.
    rail: &str,
) -> Vec<Line<'static>> {
    let stem = format!("{rail}  \u{2514} ");
    let stem_w = UnicodeWidthStr::width(stem.as_str());
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
            Span::styled(stem.clone(), Style::default().fg(theme.ink(Ink::Subtle))),
            Span::styled(
                truncate_display(&note, width.saturating_sub(stem_w + retry_w).max(1)),
                Style::default().fg(theme.ink(Ink::Settled)),
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
                    Style::default().fg(theme.ink(Ink::Meta)),
                ));
            }
        }
        if repeat > 1 {
            spans.push(Span::styled(
                format!(" ×{repeat}"),
                Style::default().fg(theme.ink(Ink::Meta)),
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
    // how long their answer was and not what it said. A multi-question answer
    // arrives as one labelled line per question, so it keeps its rows instead
    // of collapsing to whichever answer happened to be first.
    if is_user_decision_call(call) {
        let answer = unanswered_question_note(call, t)
            .unwrap_or_else(|| call.preview.as_deref().unwrap_or("").trim());
        let indent = " ".repeat(stem_w);
        let rows: Vec<&str> = answer
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        let mut out: Vec<Line<'static>> = Vec::new();
        for (i, line) in rows.iter().take(ANSWER_SUMMARY_ROWS).enumerate() {
            let lead = if i == 0 { stem.clone() } else { indent.clone() };
            out.push(Line::from(vec![
                Span::styled(lead, Style::default().fg(theme.ink(Ink::Subtle))),
                Span::styled(
                    truncate_display(line, width.saturating_sub(stem_w + 1).max(8)),
                    Style::default().fg(theme.ink(Ink::Prose)),
                ),
            ]));
        }
        if rows.len() > ANSWER_SUMMARY_ROWS {
            out.push(Line::from(vec![
                Span::styled(indent, Style::default().fg(theme.ink(Ink::Subtle))),
                Span::styled(
                    t.fold_more_lines_short
                        .replace("{}", &(rows.len() - ANSWER_SUMMARY_ROWS).to_string()),
                    Style::default().fg(theme.ink(Ink::Meta)),
                ),
            ]));
        }
        if out.is_empty() {
            out.push(Line::from(Span::styled(
                stem,
                Style::default().fg(theme.ink(Ink::Subtle)),
            )));
        }
        return out;
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
        stem.clone(),
        Style::default().fg(theme.ink(Ink::Subtle)),
    )];
    if let Some(first) = first {
        let count_w = UnicodeWidthStr::width(pre)
            + n.to_string().len()
            + usize::from(preview_truncated(call))
            + UnicodeWidthStr::width(post);
        let avail = width.saturating_sub(4 + count_w + 3 + 2).max(8);
        spans.push(Span::styled(
            truncate_display(&first, avail),
            Style::default().fg(theme.ink(Ink::Settled)),
        ));
        spans.push(Span::styled(
            " · ".to_string(),
            Style::default().fg(theme.ink(Ink::Meta)),
        ));
    }
    spans.push(Span::styled(
        pre.to_string(),
        Style::default().fg(theme.ink(Ink::Meta)),
    ));
    spans.push(Span::styled(
        if preview_truncated(call) {
            format!("{n}+")
        } else {
            n.to_string()
        },
        Style::default().fg(theme.ink(Ink::Meta)),
    ));
    spans.push(Span::styled(
        post.to_string(),
        Style::default().fg(theme.ink(Ink::Meta)),
    ));
    if call_timed_out(call) {
        spans.push(Span::styled(
            t.result_timeout.to_string(),
            Style::default().fg(theme.ink(Ink::Meta)),
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
    // The group was opened: show the whole diff instead of its preview.
    expanded: bool,
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
        Span::styled(
            action.clone(),
            Style::default().fg(body_ink(first.status, theme)),
        ),
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
            Style::default().fg(body_ink(first.status, theme)),
        ));
    }
    if !tail.is_empty() {
        head.push(Span::styled(
            tail,
            Style::default().fg(theme.ink(Ink::Meta)),
        ));
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
        Span::styled("  └ ", Style::default().fg(theme.ink(Ink::Subtle))),
        Span::styled(pre.to_string(), Style::default().fg(theme.ink(Ink::Meta))),
        Span::styled(edits.to_string(), Style::default().fg(theme.ink(Ink::Meta))),
        Span::styled(post.to_string(), Style::default().fg(theme.ink(Ink::Meta))),
        Span::styled(" · ".to_string(), Style::default().fg(theme.ink(Ink::Meta))),
        Span::styled(
            format!("+{added} −{removed}"),
            Style::default().fg(theme.ink(Ink::Meta)),
        ),
    ]));

    // An edit is a durable result: its diff stays after the call settles.
    // A large one is a PREVIEW — the first rows and a count of the rest — not
    // a fold; opening the group shows every row.
    let start = out.len();
    crate::tool_cell::merged_diff_rows(calls, theme, width, &mut out);
    let hidden = (out.len() - start).saturating_sub(DIFF_PREVIEW_ROWS);
    if !expanded && hidden > 0 {
        out.truncate(start + DIFF_PREVIEW_ROWS);
        out.push(clip_line(
            vec![
                Span::styled("    ", Style::default().fg(theme.ink(Ink::Subtle))),
                Span::styled(
                    t.fold_more_lines.replace("{}", &hidden.to_string()),
                    Style::default().fg(theme.ink(Ink::Meta)),
                ),
            ],
            width,
        ));
    }
    out
}

/// Split a "{} …" i18n template around its placeholder.
fn split_placeholder(template: &str) -> (&str, &str) {
    template.split_once("{}").unwrap_or((template, ""))
}

/// The runtime's tag on a command that failed reaching the network inside a
/// network-blocked sandbox (`leveler_tools::recoverable::NETWORK_PERMISSION_REQUIRED`).
/// It opens a note addressed to the model; the row states it in the user's words.
pub(crate) const NETWORK_PERMISSION_REQUIRED: &str = "[network permission required]";

/// Whether this call detached a process instead of running one to completion.
///
/// The request is the truth: `background: true` returns as soon as the process
/// is spawned. A refused background request fails, so a successful one always
/// means "started".
fn started_in_background(call: &ToolCallBlock) -> bool {
    is_shell_call(call)
        && serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|v| v.get("background")?.as_bool())
            .unwrap_or(false)
}

fn needs_network_permission(call: &ToolCallBlock) -> bool {
    is_shell_call(call)
        && call
            .preview
            .as_deref()
            .is_some_and(|p| p.trim_start().starts_with(NETWORK_PERMISSION_REQUIRED))
}

/// A finished command's output as its preview carries it, without the
/// runtime's metadata rows (`exit: N`, stream headers) or its note to the model.
fn preview_body_lines(call: &ToolCallBlock) -> Vec<&str> {
    call.preview
        .as_deref()
        .unwrap_or("")
        .lines()
        .filter(|l| {
            let metadata = l.starts_with("exit: ")
                || (l.starts_with("--- ") && l.ends_with(" ---"))
                || l.starts_with(NETWORK_PERMISSION_REQUIRED);
            !l.trim().is_empty() && !metadata
        })
        .collect()
}

/// The runtime caps a result's preview and marks the cut with a trailing "…",
/// so a count taken from it is a lower bound.
fn preview_truncated(call: &ToolCallBlock) -> bool {
    call.preview
        .as_deref()
        .is_some_and(|p| p.trim_end().ends_with('\u{2026}'))
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
    if needs_network_permission(call) {
        return Some(t.command_needs_network_note.to_string());
    }
    // Only where the head already states the exit code (`exit_code` arrived
    // with protocol 1.10); an older row keeps its `exit: N` line.
    if is_shell_call(call)
        && call.exit_code.is_some()
        && let Some(line) = call.preview.as_deref().and_then(shell_failure_line)
    {
        return Some(truncate_display(&line, 72));
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

/// The line that says what went wrong in a failed command's preview: the
/// first line that reports a failure (`error…`, `FAIL`, `✗`, `panic`), else the
/// first thing it printed — never the runtime's own `exit: N` / stream-header
/// rows, which the head already states. Color escapes are removed.
fn shell_failure_line(preview: &str) -> Option<String> {
    let content: Vec<String> = preview
        .lines()
        .map(|l| strip_color(l).trim().to_string())
        .filter(|l| {
            let metadata = l.starts_with("exit: ")
                || l == "[timed out]"
                || (l.starts_with("--- ") && l.ends_with(" ---"));
            !l.is_empty() && !metadata
        })
        .collect();
    let reports_failure = |l: &str| {
        let lower = l.to_lowercase();
        lower.starts_with("error")
            || lower.contains("panic")
            || l.contains('\u{2717}')
            || counts_a_failure(&lower)
    };
    content
        .iter()
        .find(|l| reports_failure(l))
        .or_else(|| content.first())
        .cloned()
}

/// Whether a lowercased line says something FAILED, as opposed to counting
/// zero of them. Every test runner's passing summary contains the word —
/// "11 passed; 0 failed", "# fail 0" — and reading one as the reason a command
/// failed put a green result line under a red head.
fn counts_a_failure(lower: &str) -> bool {
    let number_before = |at: usize| {
        lower[..at]
            .trim_end()
            .rsplit(|c: char| !c.is_ascii_digit())
            .next()
            .filter(|token| !token.is_empty())
            .map(|token| token.chars().any(|c| c != '0'))
    };
    let number_after = |at: usize| {
        lower[at..]
            .trim_start_matches(|c: char| c.is_ascii_alphabetic() || matches!(c, ':' | '=' | ' '))
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .filter(|token| !token.is_empty())
            .map(|token| token.chars().any(|c| c != '0'))
    };
    let mut from = 0;
    while let Some(offset) = lower[from..].find("fail") {
        let at = from + offset;
        // A count on either side decides; a bare "FAILED" with neither is the
        // verdict itself.
        let nonzero = number_before(at)
            .or_else(|| number_after(at))
            .unwrap_or(true);
        if nonzero {
            return true;
        }
        from = at + "fail".len();
    }
    false
}

/// A line without its SGR color escapes (`ESC [ … m`).
fn strip_color(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
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
    focused_command: Option<&leveler_client_protocol::ToolCallId>,
    rows: &mut Vec<CommandRow>,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(ACTIVITY_INDENT.len());
    let mut group_rows = Vec::new();
    let lines = render_group_rows(
        group,
        theme,
        inner,
        locale,
        t,
        now_elapsed_secs,
        awaiting_approval,
        focused_command,
        &mut group_rows,
    );
    // The indent shifts every line, so the reported line offsets stay in the
    // conversation's own coordinate space.
    rows.extend(group_rows);
    lines
        .into_iter()
        .map(|line| {
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(Span::raw(ACTIVITY_INDENT));
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// The Sub-agent Detail's activity body: the delegated child's own tool calls,
/// grouped and summarized with the SAME taxonomy, visibility rules, disclosure
/// wording and status glyphs as the Conversation activity stream.
///
/// A child reports a reduced shape over `SubAgentActivity` (tool name, an
/// arguments preview, a result preview, an error bit), so the calls are adapted
/// into neutral [`ToolCallBlock`]s here and then run through the module's own
/// [`disclosure_label`]. The only deliberate difference from Conversation is
/// the fold: a finished run of the same tool reads as one settled line
/// ("读取 6 个文件") instead of a per-call row, because a child's steps are a
/// transient summary, not the evidence surface the parent transcript is.
///
/// Returned lines are relative to the caller's body indent.
pub(crate) fn child_activity_lines(
    calls: &[crate::multi_agent::ChildActivityCall],
    theme: &Theme,
    locale: Locale,
    t: &UiText,
    child_running: bool,
    width: usize,
) -> Vec<Line<'static>> {
    if calls.is_empty() {
        return vec![Line::from(Span::styled(
            t.activity_no_activity.to_string(),
            Style::default().fg(theme.text.muted),
        ))];
    }
    let mut blocks: Vec<ToolCallBlock> = calls
        .iter()
        .enumerate()
        .map(|(i, c)| ToolCallBlock {
            id: leveler_client_protocol::ToolCallId::new(format!("child-{i}")),
            name: c.tool.clone(),
            arguments: c.arguments.clone(),
            status: c.status,
            preview: c.preview.clone(),
            duration_ms: None,
            parallel: false,
            batch: None,
            started_elapsed_secs: 0,
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            expanded: false,
            stop: StopRequest::None,
            applied_diff: None,
        })
        .collect::<Vec<_>>();
    // Only the newest call can still be in flight. A call the child moved past
    // finished — marking it running would show work the child had already left
    // (the same untruth the step list used to carry). The last call's own
    // event decides it.
    if let Some(last_running) = blocks.iter().rposition(|b| b.status == ToolStatus::Running) {
        for b in blocks.iter_mut().take(last_running) {
            if b.status == ToolStatus::Running {
                b.status = ToolStatus::Ok;
            }
        }
    }
    // Silent probes stay out unless they failed — the same rule the
    // Conversation stream applies, so the detail cannot surface noise the
    // parent never would.
    let visible: Vec<&ToolCallBlock> = blocks
        .iter()
        .filter(|c| is_conversation_visible(c))
        .collect();
    if visible.is_empty() {
        return vec![Line::from(Span::styled(
            t.activity_no_activity.to_string(),
            Style::default().fg(theme.text.muted),
        ))];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < visible.len() {
        let mut j = i + 1;
        while j < visible.len() && visible[j].name == visible[i].name {
            j += 1;
        }
        child_activity_group_lines(
            &visible[i..j],
            theme,
            locale,
            t,
            child_running,
            width,
            &mut out,
        );
        i = j;
    }
    out
}

/// One run of same-tool calls, relative to the body indent. A run with a live
/// call names its tool and shows that call's target; a settled run folds to one
/// semantic line with the count.
fn child_activity_group_lines(
    group: &[&ToolCallBlock],
    theme: &Theme,
    locale: Locale,
    t: &UiText,
    child_running: bool,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let live = group
        .iter()
        .find(|c| c.status == ToolStatus::Running)
        .copied();
    if let Some(current) = live {
        // A live call is not a settled count: the row must name what it is
        // doing now, with its target. `●` says it is in flight; a child that
        // already settled while this call was open never got its terminal, so
        // it reads `·` instead — never a check.
        let (glyph, glyph_style) = if child_running {
            ("● ", Style::default().fg(theme.accent.primary))
        } else {
            ("· ", Style::default().fg(theme.text.muted))
        };
        let action = crate::tool_cell::tool_call_label(&current.name, &current.arguments, locale);
        out.push(Line::from(vec![
            Span::styled(glyph, glyph_style),
            Span::styled(
                truncate_display(&action, width.saturating_sub(2).max(4)),
                Style::default().fg(theme.ink(Ink::Active)),
            ),
        ]));
        let summary = strip_inline_md(&tool_summary_pub(&current.name, &current.arguments, t));
        if !summary.is_empty() && summary != "{}" {
            let avail = width.saturating_sub(ACTIVITY_INDENT.len()).max(4);
            out.push(Line::from(vec![
                Span::raw(ACTIVITY_INDENT),
                Span::styled(
                    truncate_display(&summary, avail),
                    Style::default().fg(theme.text.secondary),
                ),
            ]));
        }
        return;
    }
    let failed = group
        .iter()
        .filter(|c| c.status == ToolStatus::Failed)
        .count();
    let (glyph, color) = if failed > 0 {
        ("✗", theme.status.error)
    } else {
        ("✓", theme.status.success)
    };
    let label = disclosure_label(group, failed, t);
    out.push(Line::from(vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::styled(
            truncate_display(&label, width.saturating_sub(2).max(4)),
            Style::default().fg(theme.ink(Ink::Settled)),
        ),
    ]));
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
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            expanded: false,
            stop: Default::default(),
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

    /// The ink of the span carrying `needle` — the row's own target/command
    /// text, as opposed to its muted chrome or its status glyph.
    fn body_fg(lines: &[Line<'static>], needle: &str) -> Option<ratatui::style::Color> {
        lines.iter().find_map(|line| {
            line.spans
                .iter()
                .find(|s| s.content.contains(needle))
                .and_then(|s| s.style.fg)
        })
    }

    fn styled_group(calls: Vec<ToolCallBlock>, theme: &Theme) -> Vec<Line<'static>> {
        render_group(
            &group(calls),
            theme,
            100,
            Locale::Zh,
            Locale::Zh.text(),
            3,
            None,
        )
    }

    // ── Brightness hierarchy: lifecycle, not tool category ──────────────────

    /// A call in flight wears the live ink; the same call once settled recedes
    /// to the settled ink, so accumulated history cannot out-shine the agent's
    /// current prose — and neither ever wears prose's own ink. The status glyph and any semantic colour are
    /// separate channels and stay untouched.
    #[test]
    fn a_settled_tool_recedes_and_a_running_one_does_not() {
        let theme = Theme::dark();
        let running = styled_group(
            vec![call(
                "read_file",
                r#"{"path":"target.rs"}"#,
                ToolStatus::Running,
            )],
            &theme,
        );
        assert_eq!(
            body_fg(&running, "target.rs"),
            Some(theme.ink(Ink::Active)),
            "work in flight wears the live ink: {running:?}"
        );
        let settled = styled_group(
            vec![call("read_file", r#"{"path":"target.rs"}"#, ToolStatus::Ok)],
            &theme,
        );
        assert_eq!(
            body_fg(&settled, "target.rs"),
            Some(theme.ink(Ink::Settled)),
            "finished history recedes: {settled:?}"
        );
    }

    /// The hierarchy never buries a failure: a failed call keeps the live ink
    /// and its error glyph, so "what broke" stays findable among settled rows.
    #[test]
    fn failure_keeps_the_live_ink_and_the_error_glyph() {
        let theme = Theme::dark();
        let lines = styled_group(
            vec![call(
                "read_file",
                r#"{"path":"broken.rs"}"#,
                ToolStatus::Failed,
            )],
            &theme,
        );
        assert_eq!(body_fg(&lines, "broken.rs"), Some(theme.ink(Ink::Active)));
        assert!(
            lines.iter().any(|l| l
                .spans
                .iter()
                .any(|s| s.content.contains('\u{2717}') && s.style.fg == Some(theme.status.error))),
            "a failure keeps its error mark: {lines:?}"
        );
    }

    /// A finished command recedes on its command line exactly like any other
    /// tool body; the `$` marker and the duration stay quiet chrome.
    #[test]
    fn a_settled_command_line_recedes() {
        let theme = Theme::dark();
        let lines = styled_group(
            vec![call(
                "run_command",
                r#"{"program":"codeleveler-marker"}"#,
                ToolStatus::Ok,
            )],
            &theme,
        );
        assert_eq!(
            body_fg(&lines, "codeleveler-marker"),
            Some(theme.ink(Ink::Settled))
        );
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
            "\u{203a} 读取文件",
            "the run head names the tool and stays the click target: {lines:?}"
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
        assert!(!text.contains("d0") && !text.contains("d1"), "{text}");
        assert_eq!(
            lines.len(),
            4,
            "one run head, three visible rows: {lines:?}"
        );
        assert!(
            lines[3].starts_with("  └─"),
            "the probes are not children of the run: {lines:?}"
        );
    }

    // ── Tool Run: a stretch of the same tool is ONE segment ─────────────────
    //
    // Consecutive calls of the same tool used to print the tool's name once per
    // row under a header that counted them ("▸ 读取 6 个文件" over six rows each
    // starting "读取文件"). The count says what the rows below already show and
    // the label says it six more times. A run states the tool once and lets its
    // children carry only what differs: the target, and how it ended.

    fn reads(n: usize) -> Vec<ToolCallBlock> {
        (0..n)
            .map(|i| {
                call(
                    "read_file",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                    ToolStatus::Ok,
                )
            })
            .collect()
    }

    /// One call is not a run: its own row already names the tool and the file,
    /// and a head above it would be that row said twice.
    #[test]
    fn a_lone_call_is_not_a_tool_run() {
        let lines = render_group_text(&group(reads(1)), 100, Locale::Zh);
        assert_eq!(lines.len(), 1, "one call, one row: {lines:?}");
        assert!(lines[0].contains("读取文件"), "{lines:?}");
        assert!(lines[0].contains("f0.rs"), "{lines:?}");
        assert!(!lines[0].contains('\u{251c}'), "no tree: {lines:?}");
        assert!(!lines[0].contains('\u{2514}'), "no tree: {lines:?}");
    }

    /// `›` opens every tool row: `▌` user, `●` agent, `›` tool. The muted `·`
    /// bullet that used to sit there marked a successful read the same way a
    /// blank did, and read as a piece of punctuation rather than as the one
    /// place the transcript says "a tool ran here".
    #[test]
    fn a_tool_row_opens_with_the_execution_anchor() {
        let lines = render_group_text(&group(reads(1)), 100, Locale::Zh);
        assert!(
            lines[0].starts_with("\u{203a} 读取文件"),
            "the anchor opens the row: {lines:?}"
        );
        assert!(
            !lines[0].starts_with('\u{b7}'),
            "the old bullet is gone: {lines:?}"
        );
        // …and the metadata separator is NOT the bullet: it stays.
        assert!(
            lines[0].contains("\u{b7} 1 行"),
            "the result still rides on the row: {lines:?}"
        );
    }

    /// Two is already a run: one head naming the tool, children under it.
    #[test]
    fn two_consecutive_calls_form_a_tool_run() {
        let lines = render_group_text(&group(reads(2)), 100, Locale::Zh);
        assert_eq!(lines.len(), 3, "a head and two children: {lines:?}");
        assert!(
            lines[0].starts_with("\u{203a} 读取文件"),
            "the head wears the anchor and names the tool: {lines:?}"
        );
        assert!(
            !lines[0].contains("个文件"),
            "the head must not count what the rows below show: {lines:?}"
        );
        assert!(
            lines[1].contains("\u{251c}\u{2500} ") && lines[1].contains("f0.rs"),
            "{lines:?}"
        );
        assert!(
            lines[2].contains("\u{2514}\u{2500} ") && lines[2].contains("f1.rs"),
            "{lines:?}"
        );
        assert!(
            !lines[1].contains("读取文件") && !lines[2].contains("读取文件"),
            "a child must not repeat the head's label: {lines:?}"
        );
    }

    /// A long run stays complete: no truncation policy rides along with this.
    #[test]
    fn a_long_tool_run_shows_every_child() {
        let lines = render_group_text(&group(reads(15)), 100, Locale::Zh);
        assert_eq!(lines.len(), 16, "a head and fifteen children: {lines:?}");
        let text = lines.join("\n");
        for i in 0..15 {
            assert!(text.contains(&format!("f{i}.rs")), "lost f{i}.rs: {text}");
        }
        assert!(!text.contains("个文件"), "no count header: {text}");
        assert!(!text.contains('\u{2026}'), "nothing elided: {text}");
    }

    /// The run is how the burst reads WHILE it runs, not a shape applied to
    /// finished history: two settled reads inside an open group are a run too.
    #[test]
    fn a_tool_run_forms_while_the_burst_is_still_open() {
        let lines = render_group_text(&open_group(reads(2)), 100, Locale::Zh);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains("读取文件"), "{lines:?}");
        assert!(
            !lines[0].contains('\u{25b8}'),
            "an open burst is not history yet: {lines:?}"
        );
        assert!(lines[2].contains("\u{2514}\u{2500} "), "{lines:?}");
    }

    // ── A group header earns its row only by ADDING something ──────────────
    //
    // Over a mixed stretch the header used to say "检查代码库" above rows that
    // already read "读取文件 README.md" and "搜索代码 TaskStatus". That is the
    // rows summarized, not information. What it can still say — how many of
    // them failed, how many lack a permission — no single row can.

    /// A: an ordinary mixed group is its rows, in order, and nothing else.
    #[test]
    fn a_clean_mixed_group_drops_its_summary_header() {
        let g = group(vec![
            call("read_file", r#"{"path":"README.md"}"#, ToolStatus::Ok),
            call("grep", r#"{"pattern":"TaskStatus"}"#, ToolStatus::Ok),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 2, "one row per call, no header: {lines:?}");
        assert!(
            lines[0].starts_with("\u{203a} 读取文件") && lines[0].contains("README.md"),
            "{lines:?}"
        );
        assert!(
            lines[1].starts_with("\u{203a} 搜索代码") && lines[1].contains("TaskStatus"),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("检查代码库") || l.contains('\u{25b8}')),
            "the summary said what the rows say: {lines:?}"
        );
    }

    /// B: three different tools stay three linear rows — a mixed stretch is
    /// never welded into one run under a generic label.
    #[test]
    fn three_different_tools_stay_three_rows() {
        let g = group(vec![
            call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            call("grep", r#"{"pattern":"x"}"#, ToolStatus::Ok),
            call("diagnostics", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines.iter().all(|l| l.starts_with(TOOL_ANCHOR)),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains('\u{251c}') || l.contains('\u{2514}')),
            "different tools are not one run: {lines:?}"
        );
    }

    /// C: a failure inside a mixed group keeps the header, because "2 failed"
    /// is the one thing no single row can say — and every child still carries
    /// its own mark and its own reason.
    #[test]
    fn a_mixed_group_with_failures_keeps_the_count_and_the_reasons() {
        let mut bad_grep = call("grep", r#"{"pattern":"x"}"#, ToolStatus::Failed);
        bad_grep.preview = Some("invalid regex".into());
        let mut bad_lsp = call("diagnostics", r#"{"path":"b.rs"}"#, ToolStatus::Failed);
        bad_lsp.preview = Some("no language server".into());
        let g = group(vec![
            call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            bad_grep,
            bad_lsp,
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines[0].contains('2') && lines[0].contains("失败") && lines[0].contains('\u{2717}'),
            "the aggregate is the header's whole reason to exist: {lines:?}"
        );
        let text = lines.join("\n");
        assert!(
            text.contains("invalid regex") && text.contains("no language server"),
            "every reason survives: {lines:?}"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("\u{203a} \u{2717}"))
                .count(),
            2,
            "and every failed row keeps its own mark: {lines:?}"
        );
    }

    /// The boundary this round draws: a group that already renders a RUN says
    /// its failures on that run's own head and on each failed row. It gets no
    /// second, group-wide count on top — that would be the same failures
    /// counted twice, in two scopes, one row apart.
    #[test]
    fn a_group_that_holds_a_run_counts_its_failures_where_they_happened() {
        let mut bad_read = call("read_file", r#"{"path":"missing.rs"}"#, ToolStatus::Failed);
        bad_read.preview = Some("no such file".into());
        let mut bad_grep = call("grep", r#"{"pattern":"x"}"#, ToolStatus::Failed);
        bad_grep.preview = Some("invalid regex".into());
        let g = group(vec![
            call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            bad_read,
            bad_grep,
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines[0].starts_with("\u{203a} 读取文件") && lines[0].contains("失败"),
            "the run head counts the run's own failures: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("检查代码库")),
            "and no group-wide summary sits above it: {lines:?}"
        );
        let text = lines.join("\n");
        assert!(
            text.contains("no such file") && text.contains("invalid regex"),
            "both reasons survive: {lines:?}"
        );
    }

    /// D: when everything failed, the group says so and so does every row.
    #[test]
    fn an_all_failed_mixed_group_still_reads_as_failed() {
        let mut a = call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Failed);
        a.preview = Some("no such file".into());
        let mut b = call("grep", r#"{"pattern":"x"}"#, ToolStatus::Failed);
        b.preview = Some("invalid regex".into());
        let lines = render_group_text(&group(vec![a, b]), 100, Locale::Zh);
        assert!(
            lines[0].contains('\u{2717}') && lines[0].contains('2'),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains('\u{2713}')),
            "nothing here succeeded: {lines:?}"
        );
    }

    /// E: a mixed group in flight never had a header (history begins when the
    /// burst closes), so hiding the clean one cannot cost it running truth.
    #[test]
    fn a_running_mixed_group_keeps_its_live_rows() {
        let g = open_group(vec![
            call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            call("grep", r#"{"pattern":"x"}"#, ToolStatus::Running),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[1].contains('\u{25cc}'),
            "the live call is live: {lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains('\u{25b8}')), "{lines:?}");
    }

    /// F: a call that lacks a permission did not fail on its own — that is a
    /// group-level fact too, and it keeps the header for the same reason.
    #[test]
    fn a_missing_permission_keeps_the_header() {
        let mut blocked = call(
            "run_command",
            r#"{"program":"curl","args":["https://example.com"]}"#,
            ToolStatus::Failed,
        );
        blocked.preview = Some(format!("{NETWORK_PERMISSION_REQUIRED} curl"));
        let g = group(vec![
            call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            blocked,
        ]);
        let lines = render_group_text(&g, 120, Locale::Zh);
        assert!(
            lines[0].contains('\u{26a0}') && lines[0].contains('1'),
            "the permission gap is named above the rows: {lines:?}"
        );
    }

    /// Only CONSECUTIVE same-tool calls merge. A different tool ends the run,
    /// even inside one group — reads and searches are two segments, in order.
    #[test]
    fn a_different_tool_ends_the_run() {
        let mut calls = reads(2);
        calls.extend(
            (0..2).map(|i| call("grep", &format!(r#"{{"pattern":"p{i}"}}"#), ToolStatus::Ok)),
        );
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        let heads: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.contains('\u{251c}') && !l.contains('\u{2514}'))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(heads.len(), 2, "two runs, two heads: {lines:?}");
        assert!(lines[heads[0]].contains("读取文件"), "{lines:?}");
        assert!(lines[heads[1]].contains("搜索代码"), "{lines:?}");
        assert!(
            lines[heads[1] - 1].contains("f1.rs"),
            "the read run closes before the search run opens: {lines:?}"
        );
    }

    /// A burst the model issued in parallel is the COMMON case for reads, and
    /// it was still printing the tool's name on every child under a count of
    /// files. Concurrency is a fact and stays on the head; the repetition is
    /// not, and goes.
    #[test]
    fn a_batch_of_one_tool_reads_as_a_run_that_kept_its_parallel_fact() {
        let calls: Vec<ToolCallBlock> = (0..6)
            .map(|i| {
                batched(
                    "read_file",
                    &format!(r#"{{"path":"f{i}.rs"}}"#),
                    ToolStatus::Ok,
                    Some(1),
                )
            })
            .collect();
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        assert_eq!(lines.len(), 7, "one head, six children: {lines:?}");
        assert!(
            lines[0].starts_with("\u{203a} 读取文件") && lines[0].contains("并行"),
            "the head names the tool once and keeps the concurrency: {lines:?}"
        );
        assert!(
            !lines[0].contains("个文件"),
            "the count of files is what the children show: {lines:?}"
        );
        assert!(
            lines[1].starts_with("  \u{251c}\u{2500} f0.rs")
                && lines[6].starts_with("  \u{2514}\u{2500} f5.rs"),
            "children carry only their target: {lines:?}"
        );
        assert!(
            !lines[1].contains("读取文件"),
            "a child must not repeat the tool: {lines:?}"
        );
    }

    /// No visible unit head may render as opener plus metadata only.
    ///
    /// A unit head wears the `›` anchor (or a `├─`/`└─` branch inside a run)
    /// and must carry a span in a body ink — the ink reserved for a call's own
    /// words. Elapsed time, line counts and status glyphs use chrome inks, so
    /// a row wearing only those says nothing about what ran. Result rows
    /// (`  └ N 行`) ride under such a head and are exempt: their account is
    /// the head itself.
    fn assert_no_metadata_only_rows(group: &ToolGroupBlock, theme: &Theme) {
        let lines = render_group(group, theme, 100, Locale::Zh, Locale::Zh.text(), 0, None);
        for line in &lines {
            let raw: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let trimmed = raw.trim_start();
            let is_unit_head = trimmed.starts_with(TOOL_ANCHOR)
                || trimmed.starts_with("\u{251c}\u{2500}")
                || trimmed.starts_with("\u{2514}\u{2500}");
            if !is_unit_head {
                continue;
            }
            let primary = line.spans.iter().any(|s| {
                !s.content.trim().is_empty()
                    && matches!(
                        s.style.fg,
                        Some(c) if c == theme.ink(Ink::Active)
                            || c == theme.ink(Ink::Settled)
                            || c == theme.ink(Ink::Prose)
                    )
            });
            assert!(
                primary,
                "a visible tool head carries only metadata: {raw:?}"
            );
        }
    }

    /// Found in a real dogfood: a browser run printed `› 浏览网页` and then
    /// two children carrying only `· 7.8s · 4 行` and `· 66+ 行`. The run head
    /// named the tool once, the child had no target, and the child suppressed
    /// its own action label — so nothing on the row said what actually ran.
    /// The child must name its action, and the browser's per-call action is
    /// more specific than the tool label.
    #[test]
    fn a_browser_run_child_names_the_action_it_took() {
        let navigate = call(
            "browser_tab",
            r#"{"action":"navigate","url":"http://localhost:3000"}"#,
            ToolStatus::Ok,
        );
        let snapshot = call("browser_tab", r#"{"action":"snapshot"}"#, ToolStatus::Ok);
        let g = group(vec![navigate, snapshot]);
        assert_no_metadata_only_rows(&g, &Theme::dark());
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines[0].starts_with("\u{203a} 浏览网页"),
            "the run head still names the tool once: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("打开页面") && l.contains("http://localhost:3000")),
            "the navigate child names its action and target: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("读取页面")),
            "the snapshot child names its action: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.trim_start().starts_with('\u{b7}')),
            "no child is left as bare metadata: {lines:?}"
        );
    }

    /// The invariant is not about the browser: any run whose child has no
    /// renderable target must still name what ran instead of showing elapsed
    /// time alone.
    #[test]
    fn a_run_child_without_a_target_still_names_its_action() {
        let a = call("mystery_tool", r#"{"opaque":1}"#, ToolStatus::Ok);
        let b = call("mystery_tool", r#"{"opaque":2}"#, ToolStatus::Ok);
        let g = group(vec![a, b]);
        assert_no_metadata_only_rows(&g, &Theme::dark());
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines.iter().any(|l| l.contains("mystery_tool")),
            "a child with no target falls back to its action label: {lines:?}"
        );
    }

    /// The invariant covers every shape a tool row can take: runs, batches,
    /// edits and single calls across the taxonomy and MCP pseudo-tools.
    #[test]
    fn every_kind_of_tool_row_keeps_a_primary() {
        let theme = Theme::dark();
        let cases: &[(&str, &str)] = &[
            ("read_file", r#"{"path":"src/main.rs"}"#),
            ("grep", r#"{"pattern":"foo"}"#),
            ("find_files", r#"{"pattern":"*.rs"}"#),
            ("apply_patch", r#"{"path":"a.rs"}"#),
            ("write_file", r#"{"path":"a.rs","content":"x"}"#),
            ("run_command", r#"{"program":"cargo","args":["test"]}"#),
            ("run_command", r#"{"program":""}"#),
            ("shell_command", r#"{"cmd":""}"#),
            ("browser_tab", r#"{"action":"navigate","url":"http://x"}"#),
            ("browser_tab", r#"{"action":"snapshot"}"#),
            ("browser_act", r#"{"action":"click","ref":"e1"}"#),
            ("browser_inspect", r#"{"what":"console"}"#),
            ("web_fetch", r#"{"url":"http://x"}"#),
            ("mcp__server__thing", r#"{"arg":1}"#),
            ("mystery_tool", r#"{"opaque":1}"#),
        ];
        for (name, args) in cases {
            let g = group(vec![
                call(name, args, ToolStatus::Ok),
                call(name, args, ToolStatus::Ok),
            ]);
            assert_no_metadata_only_rows(&g, &theme);
        }
    }

    /// Opening a run prints each child's output under THAT child. Found by
    /// clicking a real run open: the body started at the tree's own column,
    /// so `└ 1 //! alpha module` read as another branch of the run.
    #[test]
    fn an_opened_run_keeps_its_output_inside_the_tree() {
        let mut calls = reads(2);
        for c in &mut calls {
            c.preview = Some("//! a module\npub fn a() {}\n".into());
        }
        let mut g = group(calls);
        g.expanded = true;
        let lines = render_group_text(&g, 100, Locale::Zh);
        let body: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("//! a module"))
            .collect();
        assert_eq!(body.len(), 2, "each child prints its own output: {lines:?}");
        assert!(
            body[0].starts_with("  \u{2502}"),
            "a middle child's output stays inside the tree: {body:?}"
        );
        assert!(
            body[1].starts_with("     "),
            "the last child's output clears the tree: {body:?}"
        );
    }

    /// A batch of DIFFERENT tools keeps naming them: one label over rows that
    /// ran different tools would be a lie about which tool ran on what.
    #[test]
    fn a_mixed_batch_still_names_every_tool() {
        let calls = vec![
            batched("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok, Some(1)),
            batched("grep", r#"{"pattern":"x"}"#, ToolStatus::Ok, Some(1)),
        ];
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        let text = lines.join("\n");
        assert!(text.contains("并行"), "{lines:?}");
        assert!(
            text.contains("读取文件") && text.contains("搜索代码"),
            "{lines:?}"
        );
    }

    /// Grouping is presentation. A child that failed still says so, with its
    /// mark and its reason — a run must never swallow a failure.
    #[test]
    fn a_failed_child_keeps_its_mark_inside_a_run() {
        let mut calls = reads(2);
        let mut bad = call("read_file", r#"{"path":"missing.rs"}"#, ToolStatus::Failed);
        bad.preview = Some("no such file".into());
        calls.push(bad);
        let text = render_group_text(&group(calls), 100, Locale::Zh).join("\n");
        assert!(text.contains("missing.rs"), "{text}");
        assert!(
            text.contains('\u{2717}'),
            "the failure keeps its mark: {text}"
        );
        assert!(text.contains("no such file"), "and its reason: {text}");
        assert!(text.contains("f0.rs") && text.contains("f1.rs"), "{text}");
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
            lines[0].starts_with(&format!("{TOOL_ANCHOR} 读取文件")),
            "consecutive reads are a run, which claims no concurrency: {lines:?}"
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
        let gated_row = lines
            .iter()
            .find(|l| l.contains("$ rm -rf stale"))
            .expect("gated row");
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

    /// A question nobody answered is not an answer: no ✓, and the row says
    /// what happened in the user's words instead of the note to the model.
    #[test]
    fn an_unanswered_clarification_says_so() {
        for (preview, note) in [
            (
                "The user did not respond in time — this is NOT a user reply. Continue only if the task can proceed without the answer; otherwise state explicitly what is missing.",
                "未回复（已超时）",
            ),
            (
                "No user is available to answer in this unattended run — this is NOT a user reply.",
                "无人回答（非交互运行）",
            ),
            (
                "The user saw this question and chose to skip it; proceed using your best judgment.",
                "已跳过",
            ),
        ] {
            let mut c = call(
                "request_user_input",
                r#"{"question":"置顶公告数量是否设上限？","options":["无上限","3 条"]}"#,
                ToolStatus::Ok,
            );
            c.preview = Some(preview.into());
            let text = render_group_text(&group(vec![c]), 120, Locale::Zh).join("\n");
            assert!(!text.contains('\u{2713}'), "{text}");
            assert!(text.contains('\u{26a0}'), "{text}");
            assert!(text.contains(note), "{text}");
            assert!(!text.contains("NOT a user reply"), "{text}");
            assert!(!text.contains("chose to skip"), "{text}");
        }
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
            lines[0].contains("读取文件"),
            "the head names the tool in user language: {lines:?}"
        );
        assert!(
            lines.iter().skip(1).all(|l| !l.contains("读取文件")),
            "and no child repeats it: {lines:?}"
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
        // A failure is what earns the header its row; the LABEL on it is what
        // this test is about.
        let mut failed = call("mcp__notion__fetch", r#"{"q":"b"}"#, ToolStatus::Failed);
        failed.preview = Some("connection refused".into());
        let g = group(vec![
            call("mcp__notion__search", r#"{"q":"a"}"#, ToolStatus::Ok),
            failed,
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
            None,
            &mut Vec::new(),
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
        assert_eq!(
            lines.len(),
            8,
            "seven settled calls, seven rows under one head: {lines:?}"
        );
        // …and the same group, closed, gains its clickable summary header —
        // one presentation role at a time, never both.
        let mut closed = g.clone();
        closed.open = false;
        assert!(group_has_disclosure(&closed));
        let lines = render_group_text(&closed, 100, Locale::Zh);
        assert_eq!(
            lines.len(),
            8,
            "the run head plus the same seven rows: {lines:?}"
        );
        assert!(lines[0].starts_with(TOOL_ANCHOR), "{lines:?}");
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
        assert_eq!(lines.len(), 4, "a row per call: {lines:?}");
        assert!(lines[1].contains("service.go"), "{lines:?}");
        assert!(lines[2].contains("bot.go"), "{lines:?}");
        assert!(
            lines[3].contains('◌') && lines[3].contains("TokenPlain"),
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

    /// A folded batch whose commands all need the network is not a batch that
    /// failed: it says what they need, and never shows the model-facing tag.
    #[test]
    fn a_folded_batch_of_network_denied_commands_is_not_headed_as_failed() {
        let denied = |url: &str| {
            let mut c = call(
                "run_command",
                &format!(r#"{{"program":"curl","args":["{url}"]}}"#),
                ToolStatus::Failed,
            );
            c.preview = Some(
                "[network permission required] blocked…\n\nexit: 6\n--- stderr ---\ncurl: (6) Could not resolve host".into(),
            );
            c.exit_code = Some(6);
            c
        };
        let g = group(vec![
            denied("https://a.example"),
            denied("https://b.example"),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(lines[0].starts_with('▸'), "{lines:?}");
        assert!(
            !lines[0].contains('✗') && !lines[0].contains("失败"),
            "{lines:?}"
        );
        assert!(
            lines[0].contains('⚠') && lines[0].contains("2 个需要网络权限"),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("[network permission required]")),
            "{lines:?}"
        );
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
        assert!(lines[0].starts_with(TOOL_ANCHOR), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains('◌')),
            "nothing is running any more: {lines:?}"
        );
        assert_eq!(
            lines.len(),
            4,
            "the reads' run head plus a row per call: {lines:?}"
        );
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

    /// A long multi-line shell script stays one compact running row — the
    /// status and the command on one line — current work earns focus, not
    /// screen area.
    #[test]
    fn a_long_multi_line_script_renders_one_compact_running_row() {
        let script = "echo start\ncurl -X POST http://127.0.0.1:8090/api/v1/bots/1/permissions \\\n  -H 'Content-Type: application/json' \\\n  -d '{\\\"scope\\\":\\\"repo\\\"}'\ntail -5 out.log";
        let args = serde_json::json!({ "cmd": script }).to_string();
        let g = group(vec![call("shell_command", &args, ToolStatus::Running)]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert_eq!(lines.len(), 1, "one row: {lines:?}");
        assert!(lines[0].contains("◌ $ echo start"), "{lines:?}");
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
        // WebSearch kind → generic work. Two DIFFERENT tools, so the group
        // is not one run and still wears the summary header.
        let mut failed = call(
            "web_fetch",
            r#"{"url":"https://example.com"}"#,
            ToolStatus::Failed,
        );
        failed.preview = Some("connection refused".into());
        let g = group(vec![
            call("web_search", r#"{"query":"x"}"#, ToolStatus::Ok),
            failed,
        ]);
        let lines = render_group_text(&g, 80, Locale::Zh);
        assert!(
            lines[0].starts_with('▸') && lines[0].contains("完成 2 项操作"),
            "web tools are ordinary finished work: {lines:?}"
        );
        // LSP kind reads as search work.
        let mut failed = call(
            "find_references",
            r#"{"symbol":"Runtime"}"#,
            ToolStatus::Failed,
        );
        failed.preview = Some("no language server".into());
        let g = group(vec![
            call("diagnostics", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            failed,
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
        let mut failed = call(
            "run_command",
            r#"{"program":"cargo","args":["test"]}"#,
            ToolStatus::Failed,
        );
        failed.preview = Some("error: no such command".into());
        let g = group(vec![
            call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            call("grep", r#"{"pattern":"x"}"#, ToolStatus::Ok),
            failed,
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
            lines[0].starts_with(&format!("{TOOL_ANCHOR} ✗")),
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
            lines[0].starts_with(TOOL_ANCHOR)
                && lines[0].contains('2')
                && lines[0].contains("失败")
                && lines[0].contains('✗'),
            "the run head must name the failures: {:?}",
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
            lines.iter().filter(|l| l.contains("├─ ✗")).count(),
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
    fn an_observed_batch_keeps_its_concurrency_header_of_its_own() {
        let g = group(vec![
            batched("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok, Some(1)),
            batched("grep", r#"{"pattern":"x"}"#, ToolStatus::Ok, Some(1)),
            batched("read_file", r#"{"path":"b.rs"}"#, ToolStatus::Ok, Some(1)),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines[0].contains("并行处理 3 项"),
            "the batch says it ran concurrently, with nothing above it: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("检查代码库")),
            "a clean stretch needs no second summary: {lines:?}"
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
            !lines.iter().any(|l| l.contains("检查代码库")),
            "four rows already say what the summary would: {lines:?}"
        );
        // …and when one of them fails, the stretch is named and counted.
        let mut with_failure = g.clone();
        with_failure.calls[1].status = ToolStatus::Failed;
        with_failure.calls[1].preview = Some("invalid regex".into());
        let failed_lines = render_group_text(&with_failure, 80, Locale::Zh);
        assert!(
            failed_lines[0].contains("检查代码库") && failed_lines[0].contains("失败"),
            "reads and searches together are one exploration: {failed_lines:?}"
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
        c.preview = Some("     1\t# 示例服务\n     2\t\n     3\tbody".into());
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

    /// The runtime's preview is capped (it ends in "…" when cut), so its line
    /// count is only a lower bound. Dogfood: a 130-line result read "91 行".
    #[test]
    fn a_truncated_preview_reports_its_size_as_a_lower_bound() {
        let mut c = call("grep", r#"{"pattern":"soak","path":"."}"#, ToolStatus::Ok);
        let body: Vec<String> = (1..=91).map(|i| format!("soak tick {i}")).collect();
        c.preview = Some(format!("{}…", body.join("\n")));
        let joined = render_group_text(&group(vec![c]), 120, Locale::Zh).join("\n");
        assert!(joined.contains("91+ 行"), "{joined}");

        let mut whole = call("grep", r#"{"pattern":"soak","path":"."}"#, ToolStatus::Ok);
        whole.preview = Some("a\nb\nc".into());
        let joined = render_group_text(&group(vec![whole]), 120, Locale::Zh).join("\n");
        assert!(
            joined.contains("3 行") && !joined.contains("3+ 行"),
            "{joined}"
        );
    }

    /// Runtime scheduling of a background task is not a Conversation event:
    /// `wait_task`/`get_task` manage one user-visible task (footer + detail)
    /// without claiming a row of their own, however often the runtime polls.
    #[test]
    fn background_task_polling_never_claims_a_conversation_row() {
        let wait = call(
            "wait_task",
            r#"{"task_id":"bg-1","timeout_seconds":120}"#,
            ToolStatus::Ok,
        );
        assert!(
            render_group_text(&group(vec![wait]), 120, Locale::Zh).is_empty(),
            "a wait that only reports the task is still running is not activity"
        );

        // Repeated waits against several tasks still add nothing: the
        // timeline has no per-invocation entity to accumulate.
        let polls: Vec<ToolCallBlock> = [
            ("wait_task", "bg-1"),
            ("wait_task", "bg-1"),
            ("wait_task", "bg-2"),
            ("get_task", "bg-1"),
            ("get_task", "bg-2"),
        ]
        .iter()
        .enumerate()
        .map(|(i, (name, task))| {
            let mut c = call(
                name,
                &format!(r#"{{"task_id":"{task}","timeout_seconds":120}}"#),
                ToolStatus::Ok,
            );
            c.id = ToolCallId::new(format!("p{i}"));
            c.duration_ms = Some(120_000);
            c.preview = Some("task_id: bg\nstatus: running".into());
            c
        })
        .collect();
        assert!(
            render_group_text(&group(polls), 120, Locale::Zh).is_empty(),
            "repeated polling of several background tasks adds no rows"
        );
    }

    /// The poll is hidden, but the failure it reports is not: a background task
    /// that exited non-zero must still reach the user.
    #[test]
    fn a_failed_background_wait_still_reports_the_task_failure() {
        let mut wait = call(
            "wait_task",
            r#"{"task_id":"bg-1","timeout_seconds":120}"#,
            ToolStatus::Failed,
        );
        wait.preview = Some("task_id: bg-1\nstatus: exited\nexit_code: 1".into());
        let rows = render_group_text(&group(vec![wait]), 120, Locale::Zh);
        assert!(!rows.is_empty(), "a failed wait must not vanish: {rows:?}");
        assert!(rows[0].contains("等待后台任务"), "{rows:?}");
        assert!(rows[0].contains('\u{2717}'), "{rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("bg-1")),
            "the task failure detail is still reachable: {rows:?}"
        );
    }

    /// The background policy never reaches a foreground tool.
    #[test]
    fn foreground_tools_remain_in_the_conversation() {
        let grep = call("grep", r#"{"pattern":"foo","path":"."}"#, ToolStatus::Ok);
        assert!(!render_group_text(&group(vec![grep]), 120, Locale::Zh).is_empty());

        let mut run = call(
            "run_command",
            r#"{"program":"make","args":["status"]}"#,
            ToolStatus::Ok,
        );
        run.duration_ms = Some(200);
        let rows = render_group_text(&group(vec![run]), 120, Locale::Zh);
        assert!(rows.iter().any(|r| r.contains("make status")), "{rows:?}");
    }

    #[test]
    fn expanding_a_read_brings_back_its_first_line() {
        // The preview is folded away, not thrown away.
        let mut c = call("read_file", r#"{"path":"README.md"}"#, ToolStatus::Ok);
        c.preview = Some("     1\t# 示例服务\n     2\t\n     3\tbody".into());
        let mut g = group(vec![c]);
        g.expanded = true;
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(
            lines.iter().any(|l| l.contains("# 示例服务")),
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
        assert_eq!(lines.len(), 1, "one row: {lines:?}");
        assert!(
            lines[0].starts_with(&format!("{TOOL_ANCHOR} ✓ $ cargo test")),
            "the row names the command it ran: {lines:?}"
        );
        assert!(
            lines[0].ends_with("1 行"),
            "how much it printed is counted on the row: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("unused import")),
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
            lines
                .first()
                .is_some_and(|l| l.starts_with(&format!("{TOOL_ANCHOR} ✗"))),
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
            lines[0].starts_with(&format!("{TOOL_ANCHOR} ✗ $ cargo test")),
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

    /// A folded batch names what went wrong in its failed command, not the
    /// runtime's exit row.
    #[test]
    fn a_folded_batch_error_line_skips_the_exit_row() {
        let mut failed = call(
            "run_command",
            r#"{"program":"cargo","args":["test"]}"#,
            ToolStatus::Failed,
        );
        failed.preview = Some("exit: 101\n--- stderr ---\nerror: test failed".into());
        failed.exit_code = Some(101);
        let ok = call(
            "run_command",
            r#"{"program":"cargo","args":["build"]}"#,
            ToolStatus::Ok,
        );
        let g = group(vec![failed, ok]);
        let lines = render_group_text(&g, 120, Locale::Zh);
        assert!(lines[0].starts_with('▸'), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("└ error: test failed")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("└ exit: 101")),
            "{lines:?}"
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
        assert!(
            lines[0].starts_with(&format!("{TOOL_ANCHOR} ⚠")),
            "{lines:?}"
        );
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
            .find(|l| l.contains("$ "))
            .expect("command row exists");
        assert!(
            head.contains("cargo test --workspace"),
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
        assert!(
            lines[0].starts_with(&format!("{TOOL_ANCHOR} ◌ $ cargo build")),
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
        let text = lines.join("\n");
        assert!(
            text.contains("invalid patch: a") && text.contains("invalid patch: b"),
            "different arguments stay separate rows: {lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains('✗')).count(),
            3,
            "two failed children, and the run head that counts them: {lines:?}"
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

/// One `run_command` call is one summary row: the command itself, then what
/// the runtime said about it. The row used to be a "执行命令 · 已完成" head
/// over a `$ command` line — the same call named twice, two rows per command.
#[cfg(test)]
mod compact_command_tests {
    use super::*;
    use crate::theme::Ink;
    use leveler_client_protocol::ToolCallId;
    use unicode_width::UnicodeWidthStr;

    fn cmd(args: &str, status: ToolStatus) -> ToolCallBlock {
        ToolCallBlock {
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            expanded: false,
            stop: Default::default(),
            id: ToolCallId::new("c1"),
            name: "run_command".into(),
            arguments: args.into(),
            status,
            preview: None,
            duration_ms: None,
            parallel: false,
            batch: None,
            started_elapsed_secs: 0,
            applied_diff: None,
        }
    }

    const SED: &str = r#"{"program":"sed","args":["-n","897,978p","docs/foo.md"]}"#;

    fn lines_at(call: ToolCallBlock, theme: &Theme, width: usize, now: u64) -> Vec<Line<'static>> {
        render_group(
            &ToolGroupBlock {
                calls: vec![call],
                open: false,
                expanded: false,
            },
            theme,
            width,
            Locale::Zh,
            Locale::Zh.text(),
            now,
            None,
        )
    }

    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn rows(call: ToolCallBlock) -> Vec<String> {
        text(&lines_at(call, &Theme::no_color(), 100, 0))
    }

    fn fg_of(lines: &[Line<'static>], needle: &str) -> Option<ratatui::style::Color> {
        lines.iter().find_map(|l| {
            l.spans
                .iter()
                .find(|s| s.content.contains(needle))
                .and_then(|s| s.style.fg)
        })
    }

    #[test]
    fn a_finished_command_is_one_row_that_leads_with_the_command() {
        let mut c = cmd(SED, ToolStatus::Ok);
        c.duration_ms = Some(200);
        let rows = rows(c);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            rows[0],
            "\u{203a} \u{2713} $ sed -n 897,978p docs/foo.md · 0.2s"
        );
        assert!(!rows[0].contains("执行命令") && !rows[0].contains("已完成"));
    }

    #[test]
    fn a_chained_shell_line_is_still_one_row() {
        let mut c = cmd(
            r#"{"cmd":"echo === && grep -rn TODO src | head && find . -name '*.rs'"}"#,
            ToolStatus::Ok,
        );
        c.name = "shell_command".into();
        c.duration_ms = Some(300);
        let rows = rows(c);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].contains("$ echo === && grep"), "{rows:?}");
    }

    #[test]
    fn a_running_command_is_one_row_with_its_live_elapsed() {
        let mut c = cmd(
            r#"{"program":"go","args":["build","./cmd/..."]}"#,
            ToolStatus::Running,
        );
        c.started_elapsed_secs = 10;
        let rows = text(&lines_at(c, &Theme::no_color(), 100, 16));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0].starts_with("\u{203a} \u{25cc} $ go build ./cmd/..."),
            "{rows:?}"
        );
        assert!(rows[0].ends_with(" · 6s"), "{rows:?}");
    }

    #[test]
    fn a_failed_command_keeps_duration_and_exit_on_its_row() {
        let mut c = cmd(
            r#"{"program":"cargo","args":["test","-p","leveler-tui"]}"#,
            ToolStatus::Failed,
        );
        c.duration_ms = Some(11_400);
        c.exit_code = Some(101);
        c.preview = Some("exit: 101\ntest activity_stream::tests::x ... FAILED".into());
        let rows = rows(c);
        assert!(
            rows.len() <= 2,
            "one row plus an optional error line: {rows:?}"
        );
        assert_eq!(
            rows[0],
            "\u{203a} \u{2717} $ cargo test -p leveler-tui · 11.4s · exit 101"
        );
        assert!(!rows.iter().any(|r| r.contains("执行命令")));
    }

    #[test]
    fn a_large_output_is_counted_on_the_row_not_poured_into_it() {
        let mut c = cmd(
            r#"{"program":"rg","args":["TODO","crates/"]}"#,
            ToolStatus::Ok,
        );
        c.duration_ms = Some(400);
        c.preview = Some((0..137).map(|i| format!("hit {i}\n")).collect());
        let rows = rows(c);
        assert_eq!(
            rows,
            vec!["\u{203a} \u{2713} $ rg TODO crates/ · 0.4s · 137 行"]
        );
    }

    #[test]
    fn stderr_on_a_success_does_not_make_it_a_failure() {
        let mut c = cmd(SED, ToolStatus::Ok);
        c.duration_ms = Some(200);
        c.preview = Some("--- stderr ---\nwarning: unused variable".into());
        let rows = rows(c);
        assert!(
            rows[0].contains('\u{2713}') && !rows[0].contains('\u{2717}'),
            "{rows:?}"
        );
    }

    #[test]
    fn every_terminal_state_is_one_row_that_says_which() {
        let t = Locale::Zh.text();
        let mut timed = cmd(r#"{"program":"cargo","args":["test"]}"#, ToolStatus::Failed);
        timed.preview = Some("[timed out]".into());
        let mut stopped = cmd(
            r#"{"program":"npm","args":["run","dev"]}"#,
            ToolStatus::Cancelled,
        );
        stopped.duration_ms = Some(32_600);
        let unknown = cmd(r#"{"program":"./deploy.sh"}"#, ToolStatus::Unknown);
        let mut background = cmd(
            r#"{"program":"pnpm","args":["dev"],"background":true}"#,
            ToolStatus::Ok,
        );
        background.duration_ms = Some(40);
        for (call, glyph, word) in [
            (timed, "\u{2717}", t.result_timeout.trim()),
            (stopped, "\u{2298}", t.command_stopped),
            (unknown, "?", t.command_unknown),
            (background, "\u{2197}", t.command_backgrounded),
        ] {
            let rows = rows(call);
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert!(
                rows[0].contains(&format!("{glyph} $ ")),
                "{glyph}: {rows:?}"
            );
            assert!(rows[0].contains(word), "{word}: {rows:?}");
        }
    }

    #[test]
    fn a_command_held_for_approval_does_not_read_as_running() {
        let c = cmd(
            r#"{"program":"curl","args":["https://example.com"]}"#,
            ToolStatus::Running,
        );
        let id = c.id.clone();
        let rows: Vec<String> = text(&render_group(
            &ToolGroupBlock {
                calls: vec![c],
                open: true,
                expanded: false,
            },
            &Theme::no_color(),
            100,
            Locale::Zh,
            Locale::Zh.text(),
            5,
            Some(&id),
        ));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0].contains("\u{26a0} $ curl https://example.com"),
            "{rows:?}"
        );
        assert!(
            rows[0].contains(Locale::Zh.text().approval_pending),
            "{rows:?}"
        );
    }

    /// A long command is cut, never its outcome: the duration and exit code
    /// are fixed cells at the end of the row.
    #[test]
    fn a_long_command_is_cut_before_its_metadata_is() {
        let long = r#"{"program":"cargo","args":["test","--package","leveler-tui","--test","integration_suite_with_a_very_long_name","--","--nocapture"]}"#;
        let mut c = cmd(long, ToolStatus::Failed);
        c.duration_ms = Some(8_200);
        c.exit_code = Some(101);
        for width in [40usize, 60, 80] {
            let rows = text(&lines_at(c.clone(), &Theme::no_color(), width, 0));
            let head = &rows[0];
            assert!(head.ends_with(" · 8.2s · exit 101"), "{width}: {head:?}");
            assert!(head.contains('\u{2026}'), "{width}: {head:?}");
            assert!(
                UnicodeWidthStr::width(head.as_str()) <= width,
                "{width}: {head:?}"
            );
        }
    }

    #[test]
    fn a_cjk_command_fits_its_width() {
        let mut c = cmd(
            r#"{"program":"echo","args":["构建产物已经上传到对象存储并完成校验"]}"#,
            ToolStatus::Ok,
        );
        c.duration_ms = Some(1_000);
        for width in [20usize, 30, 44] {
            let rows = text(&lines_at(c.clone(), &Theme::no_color(), width, 0));
            assert!(
                UnicodeWidthStr::width(rows[0].as_str()) <= width,
                "{width}: {rows:?}"
            );
        }
    }

    #[test]
    fn a_very_narrow_terminal_does_not_panic() {
        let mut c = cmd(SED, ToolStatus::Failed);
        c.exit_code = Some(2);
        c.duration_ms = Some(900);
        for width in 0..12 {
            let _ = lines_at(c.clone(), &Theme::no_color(), width, 0);
        }
    }

    // ── Live expanded, settled compact ──────────────────────────────────────

    fn with_output(mut c: ToolCallBlock, n: usize) -> ToolCallBlock {
        c.output = (1..=n).map(|i| format!("test case_{i} ... ok\n")).collect();
        c
    }

    /// A command in flight shows what it is printing: the row, then the last
    /// few output lines under it, with the rest counted, never poured in.
    #[test]
    fn a_running_command_shows_a_bounded_tail_of_its_live_output() {
        let c = with_output(cmd(SED, ToolStatus::Running), 30);
        let rows = text(&lines_at(c, &Theme::no_color(), 100, 4));
        assert!(
            rows[0].starts_with("\u{203a} \u{25cc} $ sed -n"),
            "{rows:?}"
        );
        assert_eq!(rows.len(), 1 + 1 + LIVE_TAIL_ROWS, "{rows:?}");
        assert!(
            rows[1].contains('\u{2026}'),
            "hidden lines are counted: {rows:?}"
        );
        assert!(
            rows.last().unwrap().ends_with("test case_30 ... ok"),
            "{rows:?}"
        );
        assert!(!rows.iter().any(|r| r.contains("case_1 ")), "{rows:?}");
    }

    #[test]
    fn a_short_live_output_is_shown_whole() {
        let c = with_output(cmd(SED, ToolStatus::Running), 2);
        let rows = text(&lines_at(c, &Theme::no_color(), 100, 4));
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(rows[1].ends_with("test case_1 ... ok"), "{rows:?}");
    }

    /// Settling is one transition: the tail leaves and the row drops from the
    /// live ink to the settled one, in the same logical block.
    #[test]
    fn settling_collapses_the_tail_and_the_ink_together() {
        let theme = Theme::dark();
        let running = with_output(cmd(SED, ToolStatus::Running), 12);
        let live = lines_at(running.clone(), &theme, 100, 4);
        assert!(live.len() > 1);
        assert_eq!(fg_of(&live, "sed -n"), Some(theme.ink(Ink::Active)));

        let mut done = running;
        done.status = ToolStatus::Ok;
        done.duration_ms = Some(4_100);
        let settled = lines_at(done, &theme, 100, 4);
        assert_eq!(settled.len(), 1, "{:?}", text(&settled));
        assert_eq!(fg_of(&settled, "sed -n"), Some(theme.ink(Ink::Settled)));
        assert!(text(&settled)[0].contains("12 行"), "{:?}", text(&settled));
    }

    /// Every settled outcome leaves at most the row and one error line.
    #[test]
    fn every_settled_outcome_drops_the_live_tail() {
        for status in [
            ToolStatus::Failed,
            ToolStatus::Cancelled,
            ToolStatus::Unknown,
        ] {
            let mut c = with_output(cmd(SED, ToolStatus::Running), 12);
            c.status = status;
            c.exit_code = Some(1);
            c.preview = Some("exit: 1\nerror: boom".into());
            let rows = rows(c);
            assert!(rows.len() <= 2, "{status:?}: {rows:?}");
            assert!(
                !rows.iter().any(|r| r.contains("case_")),
                "{status:?}: {rows:?}"
            );
        }
    }

    /// Opened on purpose (Enter / click), a settled command shows its output.
    #[test]
    fn an_opened_settled_command_shows_its_output() {
        let mut c = with_output(cmd(SED, ToolStatus::Ok), 3);
        c.expanded = true;
        let rows = rows(c);
        assert!(
            rows.iter().any(|r| r.ends_with("test case_3 ... ok")),
            "{rows:?}"
        );
    }

    #[test]
    fn a_live_tail_in_a_narrow_cjk_terminal_fits() {
        let mut c = cmd(SED, ToolStatus::Running);
        c.output = "编译产物已经上传到对象存储并完成校验\n".repeat(9);
        for width in [0usize, 6, 18, 30] {
            for row in text(&lines_at(c.clone(), &Theme::no_color(), width, 4)) {
                assert!(
                    UnicodeWidthStr::width(row.as_str()) <= width,
                    "{width}: {row:?}"
                );
            }
        }
    }

    // ── Durable results persist, bounded ────────────────────────────────────

    fn edit(lines: usize) -> ToolCallBlock {
        let mut patch = String::from("--- a/src/theme.rs\n+++ b/src/theme.rs\n@@ -1,1 +1,{n} @@\n");
        patch = patch.replace("{n}", &lines.to_string());
        for i in 0..lines {
            patch.push_str(&format!("+line {i}\n"));
        }
        let mut c = cmd(r#"{"path":"src/theme.rs"}"#, ToolStatus::Ok);
        c.name = "apply_patch".into();
        c.applied_diff = Some(patch);
        c.duration_ms = Some(30);
        c
    }

    #[test]
    fn a_settled_edit_keeps_its_diff_on_screen() {
        let rows = rows(edit(3));
        assert!(rows.iter().any(|r| r.contains("+ line 2")), "{rows:?}");
    }

    /// A large diff is a preview, not a fold: its first rows stay, the rest is
    /// counted, and opening the group shows all of it.
    #[test]
    fn a_large_diff_is_truncated_not_collapsed() {
        let rows = rows(edit(200));
        assert!(rows.iter().any(|r| r.contains("+ line 0")), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("+ line 199")), "{rows:?}");
        assert!(rows.len() <= 2 + DIFF_PREVIEW_ROWS + 1, "{}", rows.len());
        assert!(rows.last().unwrap().contains("还有"), "{:?}", rows.last());

        let opened = text(&render_group(
            &ToolGroupBlock {
                calls: vec![edit(200)],
                open: false,
                expanded: true,
            },
            &Theme::no_color(),
            100,
            Locale::Zh,
            Locale::Zh.text(),
            0,
            None,
        ));
        assert!(
            opened.iter().any(|r| r.contains("+ line 199")),
            "{}",
            opened.len()
        );
    }

    // ── Luminance roles ─────────────────────────────────────────────────────

    #[test]
    fn a_command_row_speaks_in_distinct_roles() {
        let theme = Theme::dark();
        let mut c = cmd(SED, ToolStatus::Ok);
        c.duration_ms = Some(200);
        let lines = lines_at(c, &theme, 100, 0);
        assert_eq!(fg_of(&lines, "sed -n"), Some(theme.ink(Ink::Settled)));
        assert_eq!(fg_of(&lines, "0.2s"), Some(theme.ink(Ink::Meta)));
        assert_eq!(fg_of(&lines, "\u{203a}"), Some(theme.ink(Ink::Subtle)));
        assert_eq!(fg_of(&lines, "\u{2713}"), Some(theme.status.success));
    }

    /// The same call reads as live while it runs and drops to the settled ink
    /// the moment it finishes — lifecycle, not tool type, sets the weight.
    #[test]
    fn a_command_steps_down_from_active_to_settled_when_it_finishes() {
        let theme = Theme::dark();
        let running = lines_at(cmd(SED, ToolStatus::Running), &theme, 100, 3);
        assert_eq!(fg_of(&running, "sed -n"), Some(theme.ink(Ink::Active)));
        let mut done = cmd(SED, ToolStatus::Ok);
        done.duration_ms = Some(200);
        let done = lines_at(done, &theme, 100, 3);
        assert_eq!(fg_of(&done, "sed -n"), Some(theme.ink(Ink::Settled)));
    }

    /// Every tool row shares the ladder: a settled read recedes like a
    /// settled command, its metadata and anchor recede further.
    #[test]
    fn other_tool_rows_share_the_same_roles() {
        let theme = Theme::dark();
        let mut read = cmd(r#"{"path":"README.md"}"#, ToolStatus::Ok);
        read.name = "read_file".into();
        read.duration_ms = Some(200);
        read.preview = Some("a\nb\n".into());
        let lines = lines_at(read, &theme, 100, 0);
        assert_eq!(fg_of(&lines, "README.md"), Some(theme.ink(Ink::Settled)));
        assert_eq!(fg_of(&lines, "0.2s"), Some(theme.ink(Ink::Meta)));
        assert_eq!(fg_of(&lines, "\u{203a}"), Some(theme.ink(Ink::Subtle)));
    }
}
