//! Conversation activity stream: per-call tool evidence.
//!
//! Product surface, not a tool trace — but evidence, not a summary. Every
//! user-visible call owns a row that answers four questions: which tool ran, on
//! what, what came back, and what state it is in.
//!
//! **One execution-presentation contract, three shapes:**
//!
//! - **Search / Read / List — compact receipt.** A stretch of two or more
//!   successful exploration calls collapses to ONE line that counts by kind
//!   (`› 读取 7 个文件 · 搜索 2 次`). A burst of reads is not a decision log:
//!   the files add nothing a reader acts on, and a row each is the
//!   execution-log waterfall this contract removes. A LONE exploration call
//!   keeps its evidence row (aggregating one call loses the target for no
//!   compactness), and a stretch containing a FAILURE falls back to rows so the
//!   failing target stays inspectable.
//! - **Run / Shell / Test — one logical execution.** A run keeps its command
//!   and its outcome (`› 执行命令 · 完成 1 项` / `└─ ✓ $ cargo test · 8.2s`).
//!   The started/completed/waiting lifecycle is NEVER written into history as
//!   separate rows: it lives in the single live activity while the command runs
//!   and freezes to the receipt on completion.
//! - **Edit — inline diff.** A diff IS the result, so it is never folded into
//!   a count; consecutive same-file patches merge into one node.
//!
//! Mixed stretches that are not exploration keep the per-call rows below their
//! disclosure header.
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
//! - A group that is not a run keeps a clickable `▸/▾` summary as a HEADER
//!   over its rows. That fold governs each call's OUTPUT — the one thing it
//!   may hide. The header exists from the group's FIRST second (`⋮ 正在执行 ·
//!   N 个命令`) and closing only rewrites it (`▸ … · 全部成功`), so a close
//!   never inserts a row or shifts the children under it. A lone non-shell
//!   call gets no header: it may still become a run, whose own head then owns
//!   the stretch. Either way the group's FIRST row is the click target.
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

/// Render a tool group for the Conversation activity stream (test-only
/// convenience wrapper over [`render_group_rows`]).
///
/// Every user-visible call in the group owns a row, whatever the group's
/// state. The row says which tool ran, on what, what came back, and what state
/// it is in. A finished, non-edit group keeps a clickable `▸/▾` summary over
/// those rows, and that row governs how much of each call's OUTPUT is shown.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_group(
    group: &ToolGroupBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    now_elapsed_secs: u64,
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
    // Execution-presentation contract, shape 1: Search / Read / List is ONE
    // view-time fold. The collapsed group paints the aggregate receipt; an
    // expanded group paints the receipt and then every real member row, so the
    // detail is never lost — only folded. Runs and edits keep their own shape
    // below and break an exploration stretch.
    let compact_exploration = group_is_compact_exploration(group);
    let units = plan_units(&group.calls);
    if compact_exploration {
        // The receipt is the group's own summary row: a second header above it
        // would say the same thing twice. It IS the disclosure row, and its
        // children step in one level like every other fold's rows.
        let live = group.open || !group_is_finished(group);
        let members = exploration_members(group);
        out.push(exploration_receipt_line(
            &members,
            theme,
            width,
            t,
            live,
            group.expanded(),
        ));
        if group.expanded() {
            out.extend(expanded_exploration_body(
                &members,
                theme,
                width,
                locale,
                t,
                now_elapsed_secs,
                awaiting_approval,
            ));
        }
        return out;
    }
    // An execution round whose calls are all one tool IS a run: it reads as a
    // tool-named head (`› 执行命令 · 完成 3 项`) with the calls as its tree,
    // instead of a stage sentence above flat rows. The round identity makes
    // the group exactly one model response, so this is its natural form.
    if group.round.is_some()
        && group_is_disclosable(group)
        && !visible.is_empty()
        && visible.iter().all(|c| c.name == visible[0].name)
        && !is_lone_exploration(&visible)
    {
        push_run(
            &visible,
            None,
            group,
            theme,
            width,
            locale,
            t,
            now_elapsed_secs,
            awaiting_approval,
            true,
            rows,
            &index_of,
            focused_command,
            &mut out,
        );
        return out;
    }
    // A group header earns its row by ADDING what no child can. A `Run`/`Batch`
    // unit — or an exploration receipt, which is itself a summary row — paints
    // its own head, so a second disclosure above it would be two parents for
    // one stretch. Every other group is a STAGE, and its parent row states the
    // aggregate — how many, whether any failed, and the whole stretch's
    // duration — including when everything succeeded: the rows below are then
    // children of a stated outcome instead of orphans.
    //
    // Ownership is STRUCTURAL, not historical: the parent row exists from the
    // moment the stage does, and `open` only decides its CONTENT (running verb
    // vs outcome). Gating the row itself on `open` — or on the call count —
    // made closing the group insert a row and shift every child, the layout
    // jump this rule exists to remove. A lone non-shell call is still eligible
    // to become a `Run`, whose head would then own the stretch, so the stage row
    // waits for it; a lone shell command never merges into a run.
    //
    // A group's FIRST row is the click target either way (see
    // `conversation::build`), so this adds a parent without moving the target.
    let unit_owns_head = units.iter().any(|u| {
        matches!(
            u,
            StreamUnit::Run(_) | StreamUnit::Batch(_) | StreamUnit::ExploreRun(_)
        )
    });
    // Live unless the group is CLOSED and every call has settled. The group's
    // own `open` flag alone is not enough: a closed group can still hold a call
    // in flight, and a settled-but-open one is still being written to.
    let live = group.open || !group_is_finished(group);
    // A round whose calls are NOT all one tool does not fit the single-run
    // form, but it is still one execution node: it drops the tool name and
    // counts the operations it ran, instead of a stage sentence. Edit rounds
    // are excluded — a diff is the result and keeps its own shape.
    let mixed_round = group.round.is_some()
        && !visible.is_empty()
        && !group_has_edits(group)
        && group_is_disclosable(group)
        && !is_lone_exploration(&visible);
    let stage_worthy = !mixed_round
        && group_is_disclosable(group)
        && !unit_owns_head
        && (visible.len() > 1 || visible.first().is_some_and(|c| is_shell_call(c)));
    let header = if mixed_round {
        let mut p = disclosure_presentation(&visible, group.expanded(), live, t);
        p.label = round_mixed_head(&visible, live, t);
        p.ok_suffix = None;
        Some(p)
    } else {
        stage_worthy.then(|| disclosure_presentation(&visible, group.expanded(), live, t))
    };
    if let Some(header) = &header {
        out.push(crate::presentation::disclosure::header_line(
            header, theme, width,
        ));
    }
    // The children are clipped to the room they will occupy BEFORE the indent
    // is added below, so nesting can never push a row past the right gutter.
    let width = if header.is_some() {
        width.saturating_sub(GROUP_BODY_INDENT.len())
    } else {
        width
    };
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
                    group.expanded(),
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
                    group.expanded(),
                    1,
                    None,
                    now_elapsed_secs,
                    None,
                    awaits(call, awaiting_approval),
                    true,
                    is_exploration_call(call),
                ));
                push_expanded_detail(call, group.expanded(), theme, width, locale, t, &mut out);
            }
            StreamUnit::ExploreRun(members) => {
                // A stretch of exploration inside a larger round: the collapsed
                // group paints the aggregate, an expanded one keeps the
                // aggregate header and then every real member row under it.
                out.push(exploration_receipt_line(
                    &members,
                    theme,
                    width,
                    t,
                    live,
                    group.expanded(),
                ));
                if group.expanded() {
                    out.extend(expanded_exploration_body(
                        &members,
                        theme,
                        width,
                        locale,
                        t,
                        now_elapsed_secs,
                        awaiting_approval,
                    ));
                }
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
                    false,
                    rows,
                    &index_of,
                    focused_command,
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
                    false,
                    rows,
                    &index_of,
                    focused_command,
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
                    ("\u{203a} ", theme.ink(Ink::Subtle))
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
                            group.expanded(),
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
                        group.expanded(),
                        1,
                        None,
                        now_elapsed_secs,
                        Some(branch),
                        awaits(call, awaiting_approval),
                        true,
                        false,
                    ));
                    push_expanded_detail(call, group.expanded(), theme, width, locale, t, &mut out);
                }
            }
            StreamUnit::EditMerge(calls) => {
                // A confirmed edit's diff IS the result: every hunk and every
                // changed line is painted, whatever the group's fold. Folding
                // may hide a tool's OUTPUT; it may never hide the change the
                // user was shown.
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
                    group.expanded(),
                    calls.len(),
                    (total_ms >= 100).then_some(total_ms),
                    // FailMerge is a finished failure group, never live-running.
                    0,
                    None,
                    // A settled failure is not waiting on anybody.
                    false,
                    true,
                    false,
                ));
                push_expanded_detail(
                    calls[0],
                    group.expanded(),
                    theme,
                    width,
                    locale,
                    t,
                    &mut out,
                );
            }
        }
    }
    // Ownership is stated by position: when the stage row exists, every row
    // under it steps in one level. Width was already reserved above, so this
    // only paints.
    if header.is_some() {
        for line in out.iter_mut().skip(1) {
            line.spans.insert(
                0,
                Span::styled(
                    GROUP_BODY_INDENT.to_string(),
                    Style::default().fg(theme.ink(Ink::Subtle)),
                ),
            );
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

/// The member rows an expanded exploration fold reveals, indented one level
/// under the aggregate header.
///
/// This is what makes the fold VIEW-TIME only: the group still holds every real
/// call, and opening it paints each one with its own target and detail. Members
/// are never shell commands ([`folds_into_exploration`] admits only Search /
/// Read / List), so none of them reports a command row.
#[allow(clippy::too_many_arguments)]
fn expanded_exploration_body(
    members: &[&ToolCallBlock],
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    now_elapsed_secs: u64,
    awaiting_approval: Option<&leveler_client_protocol::ToolCallId>,
) -> Vec<Line<'static>> {
    let body_width = width.saturating_sub(GROUP_BODY_INDENT.len());
    let mut body = Vec::new();
    for call in members {
        body.extend(unit_lines(
            call,
            theme,
            body_width,
            locale,
            t,
            true,
            1,
            None,
            now_elapsed_secs,
            None,
            awaits(call, awaiting_approval),
            true,
            true,
        ));
        append_call_detail(call, theme, body_width, true, locale, t, &mut body);
    }
    for line in body.iter_mut() {
        line.spans.insert(
            0,
            Span::styled(
                GROUP_BODY_INDENT.to_string(),
                Style::default().fg(theme.ink(Ink::Subtle)),
            ),
        );
    }
    body
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
    //
    // A compact exploration receipt IS a fold: its collapsed row is the
    // aggregate, and expanding reveals every real member call it counted.
    !group.open && group_is_finished(group) && group_is_disclosable(group)
}

/// Whether these calls are exploration (Search/Read/List) — the classes a
/// compact receipt may speak for.
fn is_exploration_class(name: &str) -> bool {
    matches!(
        disclosure_class(name),
        DisclosureClass::Read | DisclosureClass::Search
    )
}

/// Whether this call belongs to the exploration class the receipt speaks for
/// (Search / Read / List and the LSP lookups that read like them).
fn is_exploration_call(call: &ToolCallBlock) -> bool {
    is_exploration_class(&call.name)
}

/// Whether this call can join a view-time exploration fold inside a MIXED
/// group. A failed call never does: a receipt that swallowed the failing target
/// would hide the one row a reader needs. A call the reducer OBSERVED in a
/// concurrent batch never does either — a burst has a stronger claim to its own
/// tree (which names the failures), so a stretch may not steal its members. A
/// call still in flight never does: its row IS the live activity while it runs,
/// and only a settled stretch is history worth folding.
fn folds_into_exploration(call: &ToolCallBlock) -> bool {
    call.status == ToolStatus::Ok && call.batch.is_none() && is_exploration_call(call)
}

/// The visible calls an exploration receipt speaks for, in order. A whole-group
/// receipt (`group_is_compact_exploration`) has already proved every visible
/// member is eligible, so this is the receipt's own view of the stretch.
fn exploration_members(group: &ToolGroupBlock) -> Vec<&ToolCallBlock> {
    group
        .calls
        .iter()
        .filter(|c| is_conversation_visible(c) && is_exploration_call(c))
        .collect()
}

/// Whether one call is eligible to be counted by a view-time exploration fold:
/// visible, non-destructive, and exploration. The fold planner uses this to
/// collect the members a receipt speaks for without knowing any tool name.
pub(crate) fn is_exploration_fold_call(call: &ToolCallBlock) -> bool {
    is_conversation_visible(call) && is_exploration_call(call)
}

/// Whether this group is a settled, non-destructive EXPLORATION participant in
/// a view-time fold.
///
/// Stricter than [`group_is_compact_exploration`]: a single call still
/// participates (a finished Thought beside it already gives the fold a reason
/// to exist), but the group must be closed and every visible call must have
/// SUCCEEDED. A failure, an edit, a command or an unknown tool is a breaker,
/// so a fold can never hide a target the reader needs.
pub(crate) fn group_is_exploration_fold_member(group: &ToolGroupBlock) -> bool {
    if group.open || !group_is_finished(group) {
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
            .all(|c| is_exploration_call(c) && c.status == ToolStatus::Ok)
}

/// The exploration calls one fold participant contributes, in order.
pub(crate) fn group_exploration_fold_calls(group: &ToolGroupBlock) -> Vec<&ToolCallBlock> {
    group
        .calls
        .iter()
        .filter(|c| is_exploration_fold_call(c))
        .collect()
}

/// The aggregate receipt row of a view-time exploration fold that spans
/// several groups (and the finished Thoughts between them). `members` are the
/// real exploration calls the fold speaks for, in chronology; Thoughts never
/// count toward the label.
pub(crate) fn exploration_fold_receipt_line(
    members: &[&ToolCallBlock],
    theme: &Theme,
    width: usize,
    t: &UiText,
    expanded: bool,
) -> Line<'static> {
    exploration_receipt_line(members, theme, width, t, false, expanded)
}

/// One group's member rows inside an OPEN fold, indented one level under the
/// aggregate receipt. No receipt of its own: the fold already stated it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exploration_fold_member_lines(
    calls: &[&ToolCallBlock],
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
    now_elapsed_secs: u64,
    awaiting_approval: Option<&leveler_client_protocol::ToolCallId>,
) -> Vec<Line<'static>> {
    expanded_exploration_body(
        calls,
        theme,
        width,
        locale,
        t,
        now_elapsed_secs,
        awaiting_approval,
    )
}

/// Whether this call is a directory listing (counted separately in the
/// receipt: "列出 2 个目录" is a different fact from "读取 2 个文件").
fn is_list_call(name: &str) -> bool {
    matches!(
        crate::tool_taxonomy::lookup(name).map(|e| e.kind),
        Some(crate::tool_taxonomy::ToolKind::ListDir)
    )
}

/// The bare verb a lone exploration row wears: the same vocabulary the
/// aggregate receipt counts in, so `› 读取 a.rs` and `▸ 读取 2 个文件` read as
/// one family instead of a sentence beside a tool-catalogue entry.
///
/// Mirrors [`exploration_receipt_label`]'s kind split exactly — List, then
/// Read, then Search — so single and aggregate can never disagree about what a
/// call is.
fn exploration_verb(name: &str, t: &UiText) -> &'static str {
    if is_list_call(name) {
        t.explore_verb_list
    } else if disclosure_class(name) == DisclosureClass::Read {
        t.explore_verb_read
    } else {
        t.explore_verb_search
    }
}

/// Whether a group's whole visible activity is ONE exploration call.
///
/// A lone Read/Search/List is not a stretch, so it is painted as its own direct
/// row (`› 读取 a.rs · 12 行`) instead of under an aggregate parent: a receipt
/// for one item states `完成 1 项` over the only row there is.
fn is_lone_exploration(visible: &[&ToolCallBlock]) -> bool {
    matches!(visible, [only] if is_exploration_call(only))
}

/// A stretch of two or more SUCCESSFUL exploration calls: the compact-receipt
/// shape. Every visible call must be eligible — a single call keeps its
/// evidence row (aggregating it loses the target for no compactness), and a
/// failure falls back to rows so the failing target stays inspectable.
///
/// This is the whole-group form, so it is deliberately laxer than
/// [`folds_into_exploration`]: a batch of exploration calls that ALL succeeded
/// is one receipt, and its concurrency detail is what expanding restores.
fn group_is_compact_exploration(group: &ToolGroupBlock) -> bool {
    let visible: Vec<&ToolCallBlock> = group
        .calls
        .iter()
        .filter(|c| is_conversation_visible(c))
        .collect();
    visible.len() >= 2
        && visible
            .iter()
            .all(|c| is_exploration_call(c) && c.status != ToolStatus::Failed)
}

/// The receipt's text: counts by KIND, so a mixed read/search stretch still
/// says what it did instead of one vague verb. `live` picks the progress verb,
/// so a running stretch never claims a past-tense outcome.
fn exploration_receipt_label(visible: &[&ToolCallBlock], t: &UiText, live: bool) -> String {
    let lists = visible.iter().filter(|c| is_list_call(&c.name)).count();
    let reads = visible
        .iter()
        .filter(|c| !is_list_call(&c.name) && disclosure_class(&c.name) == DisclosureClass::Read)
        .count();
    let searches = visible
        .iter()
        .filter(|c| !is_list_call(&c.name) && disclosure_class(&c.name) == DisclosureClass::Search)
        .count();
    let (read, search, list) = if live {
        (
            t.receipt_running_read,
            t.receipt_running_search,
            t.receipt_running_list,
        )
    } else {
        (t.receipt_read, t.receipt_search, t.receipt_list)
    };
    let mut parts = Vec::new();
    if reads > 0 {
        parts.push(read.replace("{}", &reads.to_string()));
    }
    if searches > 0 {
        parts.push(search.replace("{}", &searches.to_string()));
    }
    if lists > 0 {
        parts.push(list.replace("{}", &lists.to_string()));
    }
    parts.join(" \u{b7} ")
}

/// The one-line compact receipt. Exploration is not a result, so the settled
/// form wears the conversation's own fold glyph (`\u{25b8}` folded / `\u{25be}`
/// open) — the same vocabulary every other clickable disclosure row uses. The
/// live form wears the running `\u{25cc}` and is not a fold target yet.
fn exploration_receipt_line(
    members: &[&ToolCallBlock],
    theme: &Theme,
    width: usize,
    t: &UiText,
    live: bool,
    expanded: bool,
) -> Line<'static> {
    let label = exploration_receipt_label(members, t, live);
    let (glyph, color, tone) = if live {
        ("\u{25cc} ", theme.accent.primary, theme.ink(Ink::Meta))
    } else if expanded {
        ("\u{25be} ", theme.text.muted, theme.ink(Ink::Settled))
    } else {
        ("\u{25b8} ", theme.text.muted, theme.ink(Ink::Settled))
    };
    Line::from(vec![
        Span::styled(glyph, Style::default().fg(color)),
        Span::styled(
            truncate_display(&label, width.saturating_sub(2).max(1)),
            Style::default().fg(tone),
        ),
    ])
}

/// Whether a disclosure may speak for this group's calls at all: not an edit
/// stretch (a diff IS the result), and every visible call is ordinary work
/// rather than bookkeeping/interaction that keeps its own shape. Says nothing
/// about `open`, the call count or the outcome — those change while a group
/// runs, and the parent row's EXISTENCE must not depend on them.
fn group_is_disclosable(group: &ToolGroupBlock) -> bool {
    if group_has_edits(group) {
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
///
/// `running` is the group's own truth, not the members' momentary statuses: a
/// stage whose calls have all settled while the model streams the next one is
/// still running, and must not wear an outcome it has not reached.
fn disclosure_presentation(
    visible: &[&ToolCallBlock],
    expanded: bool,
    running: bool,
    t: &UiText,
) -> crate::presentation::disclosure::DisclosurePresentation {
    if running {
        // Live: what the stage is doing and how much of it exists so far.
        // No failure count, no success claim, no final duration — each would
        // be a verdict the burst has not earned yet.
        return crate::presentation::disclosure::DisclosurePresentation {
            label: disclosure_running_label(visible, t),
            failed: 0,
            failed_suffix: None,
            needs_permission_suffix: None,
            ok_suffix: None,
            expanded,
            running: true,
            drill_down: false,
            duration_ms: None,
            first_error: None,
        };
    }
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
    // it). A multi-call stretch derives one — see `group_duration_ms`.
    let duration_ms = match visible {
        [only] => only.duration_ms,
        _ => group_duration_ms(visible),
    };
    // "All ok" is a claim about EVERY visible call, so only calls that
    // actually succeeded may earn it. Zero failures is not the same fact: a
    // cancelled or unknown call fails at nothing, and a stage reading `全部成功`
    // over `⊘ 已停止` contradicts itself in the same breath.
    let all_ok = visible.iter().all(|c| c.status == ToolStatus::Ok);
    crate::presentation::disclosure::DisclosurePresentation {
        label: disclosure_label(visible, failed, t),
        failed,
        failed_suffix: (failed > 0 && visible.len() > 1)
            .then(|| t.batch_failed.replace("{}", &failed.to_string())),
        needs_permission_suffix: (needs_network > 0 && visible.len() > 1).then(|| {
            t.batch_needs_network
                .replace("{}", &needs_network.to_string())
        }),
        ok_suffix: (all_ok && visible.len() > 1).then(|| t.batch_all_ok.to_string()),
        expanded,
        running: false,
        drill_down: false,
        duration_ms,
        first_error: (!expanded).then(|| first_error_line(visible)).flatten(),
    }
}

/// The live form of [`disclosure_label`]: the same KIND-of-work judgement, in
/// the present tense, carrying only the count observed so far. It mirrors the
/// finished label's shape so closing a group rewrites the row in place instead
/// of replacing it with a differently shaped one.
/// A mixed execution round's head. The round ran several tools, so there is
/// no single tool name to show; it states how many operations it ran instead.
/// `running` is the group's own truth, not the members' momentary statuses —
/// the same flag that decides the row's glyph — so a live head says what is
/// running and a settled one states the count, never both at once.
fn round_mixed_head(visible: &[&ToolCallBlock], running: bool, t: &UiText) -> String {
    let count = visible.len().to_string();
    if running {
        t.round_mixed_running.replace("{}", &count)
    } else {
        t.round_mixed_done.replace("{}", &count)
    }
}

fn disclosure_running_label(visible: &[&ToolCallBlock], t: &UiText) -> String {
    use DisclosureClass::*;
    let n = visible.len();
    let class = disclosure_class(&visible[0].name);
    let uniform = visible.iter().all(|c| disclosure_class(&c.name) == class);
    if !uniform && visible.iter().all(|c| is_exploratory(c)) {
        return t.disclosure_running_explore.to_string();
    }
    match (uniform, class, n) {
        (true, Shell, 1) => t.disclosure_running_shell_one.to_string(),
        (true, Shell, _) => t
            .disclosure_running_shell_many
            .replace("{}", &n.to_string()),
        (true, Read, 1) => t.disclosure_running_read_one.to_string(),
        (true, Read, _) => t.disclosure_running_read_many.replace("{}", &n.to_string()),
        (true, Search, _) => t.disclosure_running_search.to_string(),
        (true, Work, 1) => t.disclosure_running_work_one.to_string(),
        _ => t.disclosure_running_work_many.replace("{}", &n.to_string()),
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

/// The wall-clock duration of a finished multi-call group, DERIVED from the
/// runtime's per-call durations instead of summed blind.
///
/// A call the reducer OBSERVED overlapping another (a shared `batch`) ran
/// concurrently, so its burst costs its LONGEST member, not the sum: four 5s
/// reads did not take 20s. Calls with no batch demonstrably ran one after the
/// other, so they add up. `None` unless every visible call reported a
/// duration — a partial sum would read as a complete one.
fn group_duration_ms(visible: &[&ToolCallBlock]) -> Option<u64> {
    if visible.is_empty() || visible.iter().any(|c| c.duration_ms.is_none()) {
        return None;
    }
    let mut total = 0u64;
    // First-seen burst order, each folded to its longest member.
    let mut bursts: Vec<u32> = Vec::new();
    let mut burst_max: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    for call in visible {
        let ms = call.duration_ms.unwrap_or(0);
        match call.batch {
            Some(id) => {
                if !bursts.contains(&id) {
                    bursts.push(id);
                }
                let slot = burst_max.entry(id).or_insert(0);
                *slot = (*slot).max(ms);
            }
            None => total = total.saturating_add(ms),
        }
    }
    for id in bursts {
        total = total.saturating_add(burst_max.get(&id).copied().unwrap_or(0));
    }
    Some(total)
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
    /// Two or more consecutive non-destructive exploration calls (Search /
    /// Read / List, and the LSP lookups that read like them). A VIEW-TIME fold:
    /// the calls stay in the group untouched — the collapsed group paints one
    /// aggregate receipt for them, and expanding restores every member row.
    /// A shell run or a diff never joins one, and a failure breaks it, so a
    /// failing target can never be folded out of sight.
    ExploreRun(Vec<&'a ToolCallBlock>),
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
        if is_exploration_call(call) {
            // The stretch is every contiguous VISIBLE exploration call, whatever
            // its outcome: one failure un-folds the whole stretch, so the
            // aggregate can name it and the failing target keeps its own row.
            // A stretch that is clean throughout is ONE view-time fold — its
            // members stay real calls, only painted as a receipt until the
            // reader expands the group.
            let mut members = vec![call];
            let mut j = i + 1;
            while j < calls.len() {
                let next = &calls[j];
                if !is_conversation_visible(next) {
                    j += 1;
                    continue;
                }
                if is_exploration_call(next) {
                    members.push(next);
                    j += 1;
                } else {
                    break;
                }
            }
            if members.len() > 1 && members.iter().all(|c| folds_into_exploration(c)) {
                out.push(StreamUnit::ExploreRun(members));
                i = j;
                continue;
            }
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
    // True for a lone exploration call: its action wears the receipt's bare
    // verb (读取/搜索/列出) rather than the tool's noun label, so a single Read
    // reads in the same vocabulary as the aggregate receipt above it.
    exploration_row: bool,
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
    let action = if exploration_row {
        exploration_verb(&call.name, t).to_string()
    } else if call.name == "task" {
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
    let render_action = exploration_row
        || show_action
        || !has_summary
        || action != tool_action_label_for(&call.name, locale);
    if render_action {
        let action_ink = if call.status == ToolStatus::Running && !awaiting_approval {
            theme.accent.secondary
        } else {
            body_ink(call.status, theme)
        };
        head.push(Span::styled(
            action.clone(),
            Style::default().fg(action_ink),
        ));
    }
    if has_summary {
        let shell = is_shell_call(call)
            && crate::tool_cell::summary_is_command_line(&call.name, &call.arguments);
        let gap = if exploration_row { 1 } else { 2 };
        let used: usize = head
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum::<usize>()
            + usize::from(render_action) * gap
            + usize::from(shell) * 2;
        let avail = width
            .saturating_sub(used + UnicodeWidthStr::width(tail.as_str()) + 8)
            .max(8);
        // The label needs a gap before its target; a branch or anchor already
        // ended in one. A bare verb is one word, so it needs one space.
        if render_action {
            head.push(Span::raw(if exploration_row { " " } else { "  " }));
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
    // True when this run IS an execution round's head: it then states how many
    // of its calls completed, so the round reads as one lightweight node
    // instead of a stage sentence.
    round: bool,
    rows: &mut Vec<CommandRow>,
    index_of: &dyn Fn(&ToolCallBlock) -> usize,
    focused_command: Option<&leveler_client_protocol::ToolCallId>,
    out: &mut Vec<Line<'static>>,
) {
    // A run is active while any of its calls is; once all are settled the head
    // recedes with them. The children carry the per-call state either way.
    let run_ink = if calls.iter().any(|c| c.status == ToolStatus::Running) {
        theme.accent.secondary
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
    if round {
        let running = calls.iter().any(|c| c.status == ToolStatus::Running);
        let count = if running {
            t.round_running.replace("{}", &calls.len().to_string())
        } else {
            let ok = calls.iter().filter(|c| c.status == ToolStatus::Ok).count();
            t.round_done.replace("{}", &ok.to_string())
        };
        head.push(Span::styled(
            format!(" \u{b7} {count}"),
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
        // A round run covers shell calls too, so a shell child is rendered by the
        // shell renderer that owns its stop action, expansion and output rows
        // instead of the generic child row (which would drop all three).
        if is_shell_call(call) {
            let at = out.len();
            let (lines, stoppable) = command_unit_lines(
                call,
                theme,
                width,
                locale,
                t,
                group.expanded(),
                now_elapsed_secs,
                Some(branch),
                awaits(call, awaiting_approval),
                focused_command,
            );
            push_command_rows(rows, at, index_of(call), stoppable, lines.len());
            out.extend(lines);
            continue;
        }
        out.extend(run_child_lines(
            call,
            theme,
            width,
            locale,
            t,
            group.expanded(),
            now_elapsed_secs,
            branch,
            awaits(call, awaiting_approval),
        ));
        let body = out.len();
        push_expanded_detail(call, group.expanded(), theme, width, locale, t, out);
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
        // A run child rides under a head that already named the tool; it keeps
        // the tool's own label and never borrows the exploration verb.
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

/// Rows of an expanded command's OPENING that stay visible beside its tail.
///
/// A compiler report states the diagnostic and its `--> file:line` at the top
/// and the summary at the bottom, so a window that keeps only one end makes
/// the other unreachable: `前 N 行未显示` hid the very line the user needed.
const COMMAND_OUTPUT_HEAD_ROWS: usize = 6;

/// Output rows a RUNNING command shows under its row: enough to see it move.
pub(crate) const LIVE_TAIL_ROWS: usize = 6;

/// Keep short commands on one stable row. With second-granularity runtime
/// timestamps, one second is the first evidence that a call is long-lived
/// enough for its live tail to be useful instead of a one-frame layout jump.
const LIVE_TAIL_DELAY_SECS: u64 = 1;

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
/// Output line count for a call's result and whether that count is a lower
/// bound (the runtime capped the copy it sent). ONE computation, so the head
/// fact and the collapsed fold hint cannot disagree.
fn output_line_count(call: &ToolCallBlock) -> (usize, bool) {
    let streamed = call.output.lines().filter(|l| !l.trim().is_empty()).count();
    let previewed = preview_body_lines(call).len();
    let n = streamed.max(previewed);
    let capped = streamed < previewed && preview_truncated(call) || call.output_truncated;
    (n, capped)
}

fn command_output_count(call: &ToolCallBlock, t: &UiText) -> Option<String> {
    let (n, capped) = output_line_count(call);
    if n == 0 {
        return None;
    }
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
        let avail = width
            .saturating_sub(UnicodeWidthStr::width(body_indent.as_str()))
            .max(1);
        let body_line = |line: &str| {
            clip_line(
                vec![Span::styled(
                    format!("{body_indent}{}", truncate_display(line, avail)),
                    Style::default().fg(theme.ink(Ink::Settled)),
                )],
                width,
            )
        };
        let marker_line = |text: String| {
            clip_line(
                vec![Span::styled(format!("{body_indent}{text}"), meta)],
                width,
            )
        };
        let tail_start = logical.len().saturating_sub(COMMAND_OUTPUT_ROWS);
        if call.output_truncated {
            // This client keeps a byte TAIL, not the whole stream: the head is
            // gone at the source, so only the surviving tail can be drawn and
            // how much was dropped is not knowable here. Saying so beats
            // drawing the tail as if it were the whole result.
            out.push(marker_line(t.command_output_head_dropped.to_string()));
            for line in &logical[tail_start..] {
                out.push(body_line(line));
            }
        } else if logical.len() > COMMAND_OUTPUT_HEAD_ROWS + COMMAND_OUTPUT_ROWS {
            // Head AND tail, with the omitted middle named rather than silently
            // dropped: both ends of a long result carry decisions.
            let omitted = logical.len() - COMMAND_OUTPUT_HEAD_ROWS - COMMAND_OUTPUT_ROWS;
            for line in &logical[..COMMAND_OUTPUT_HEAD_ROWS] {
                out.push(body_line(line));
            }
            out.push(marker_line(
                t.command_output_omitted.replace("{}", &omitted.to_string()),
            ));
            for line in &logical[tail_start..] {
                out.push(body_line(line));
            }
        } else {
            for line in &logical {
                out.push(body_line(line));
            }
        }
    } else if call.status == ToolStatus::Running
        && crate::tool_taxonomy::result_lifetime(&call.name)
            == crate::tool_taxonomy::ResultLifetime::Transient
        && call.running_secs(now_elapsed_secs) >= LIVE_TAIL_DELAY_SECS
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
        // The one-row summary is a SUMMARY: when the command printed more than
        // it, the row must say so. Without the count a 60-line compiler report
        // reads as a one-line result.
        let (total, capped) = output_line_count(call);
        let hidden = total.saturating_sub(1);
        let hint = if hidden > 0 {
            let n = format!("{hidden}{}", if capped { "+" } else { "" });
            format!(" {}", t.fold_more_lines_short.replace("{}", &n))
        } else {
            String::new()
        };
        let room = width.saturating_sub(
            UnicodeWidthStr::width(stem.as_str()) + UnicodeWidthStr::width(hint.as_str()),
        );
        out.push(clip_line(
            vec![
                Span::styled(stem, subtle),
                Span::styled(
                    truncate_display(&note, room.max(1)),
                    Style::default().fg(theme.ink(Ink::Settled)),
                ),
                Span::styled(hint, meta),
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
/// A confirmed edit node: head, stats, and the COMPLETE canonical diff.
///
/// Unlike every other tool, an edit's diff is not output to be folded away —
/// it is the change the user is being shown, and it is painted in full. There
/// is no preview budget, no `… +N lines` substitution, and no diffstat-only
/// form: a fold may hide a run's stdout, never a line of the patch.
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

    // An edit is a durable result: its diff stays after the call settles, and
    // it is never abbreviated. Every hunk, every changed line and every
    // canonical context line is painted here.
    crate::tool_cell::merged_diff_rows(calls, theme, width, &mut out);
    out
}

/// Split a "{} …" i18n template around its placeholder.
fn split_placeholder(template: &str) -> (&str, &str) {
    template.split_once("{}").unwrap_or((template, ""))
}

/// The runtime's tag on a command that failed reaching the network inside a
/// network-blocked sandbox (`leveler_tools::recoverable::NETWORK_PERMISSION_REQUIRED`).
/// It opens a note addressed to the model; the row states it in the user's words.
///
/// Kept as a literal (not a re-export) because `leveler-tools` is only a
/// dev-dependency here; the unit test `the_network_permission_tag_is_the_one_
/// the_runtime_writes` is what keeps it from drifting again.
pub(crate) const NETWORK_PERMISSION_REQUIRED: &str = "[execution policy] Network access was denied";

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

/// Both shapes the runtime writes for a timeout start with this.
const TIMEOUT_TAG: &str = "[timed out after ";

/// A row the runtime wrote about HOW a command ran — its exit status, its
/// stream headers, a timeout, or one of the notes it addresses to the model —
/// as opposed to anything the command itself printed.
///
/// These rows are the runtime's account of the EXECUTION, never evidence about
/// the command's result: they are neither a failure reason nor output lines.
/// Every prefix below is written by `leveler-tools` (the body assembled in
/// `tools::command_execution`, plus `recoverable::sandbox_write_denied` and
/// `recoverable::network_permission_required`); a command that prints one of
/// them verbatim loses that line, which is the price of a text protocol the
/// runtime already owns.
///
/// `[permission refused] …` is deliberately NOT here: that tag carries the
/// tool's own structured refusal reason, so it IS the failure reason.
fn is_runtime_note(line: &str) -> bool {
    const TAGS: [&str; 3] = ["[execution policy] ", "[mutation rejected] ", "[note] "];
    line.starts_with("exit: ")
        || (line.starts_with("--- ") && line.ends_with(" ---"))
        || is_timeout_note(line)
        || TAGS.iter().any(|tag| line.starts_with(tag))
}

/// The runtime's timeout row: the limit that fired (`[timed out after 120s]`),
/// or the bare `[timed out]` for a preview that carries no limit.
fn is_timeout_note(line: &str) -> bool {
    line == "[timed out]" || (line.starts_with(TIMEOUT_TAG) && line.ends_with(']'))
}

/// A finished command's output as its preview carries it, without the
/// runtime's notes about how it ran.
fn preview_body_lines(call: &ToolCallBlock) -> Vec<&str> {
    call.preview
        .as_deref()
        .unwrap_or("")
        .lines()
        .filter(|l| !l.trim().is_empty() && !is_runtime_note(l))
        .collect()
}

/// The runtime caps a result's preview and marks the cut with a trailing "…",
/// so a count taken from it is a lower bound.
fn preview_truncated(call: &ToolCallBlock) -> bool {
    call.preview
        .as_deref()
        .is_some_and(|p| p.trim_end().ends_with('\u{2026}'))
}

/// Output line count for an Ok result row. A command's count is the lines it
/// printed — the same lines [`preview_body_lines`] hands the expanded body.
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
        preview_body_lines(call).len()
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
            .is_some_and(|p| p.lines().any(|l| is_timeout_note(l.trim())))
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
    // A command states its own reason, or it has none. `exit_code` arrived with
    // protocol 1.10, so where the head already states the exit code, the result
    // row must not fall back to a runtime row (`exit: N`, a stream header, a
    // policy note) — those say how the command ran, never why it failed. A row
    // from before 1.10 keeps its `exit: N` line here.
    if is_shell_call(call) && call.exit_code.is_some() {
        return call
            .preview
            .as_deref()
            .and_then(shell_failure_line)
            .map(|line| truncate_display(&line, 72));
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
/// first thing it printed — never a [`is_runtime_note`] row, which says how the
/// command ran rather than why it failed. Color escapes are removed.
fn shell_failure_line(preview: &str) -> Option<String> {
    let content: Vec<String> = preview
        .lines()
        .map(|l| strip_color(l).trim().to_string())
        .filter(|l| !l.is_empty() && !is_runtime_note(l))
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

/// Tool activity is the SECOND level of the conversation for a fold's
/// CHILDREN: the user's prompt, the agent's prose and every entry's own summary
/// row (a Thought's `◆`, a tool group's `›`/`▸`) own the content baseline, and
/// what a fold reveals sits one level inside. The group's summary row itself is
/// painted at the baseline — `›` is the conversation's first-level execution
/// anchor, exactly like `▌` for the user and `●` for the agent — so a tool can
/// never read as a child of the Thought above it.
pub(crate) const ACTIVITY_INDENT: &str = "  ";

/// The THIRD level: a group's rows sit one step in from the group's own parent
/// row, so a stage reads as `assistant text → group summary → the calls it
/// made`. Applied whenever that parent row is drawn — running (`⋮ 正在执行`)
/// or closed (`▸ …`) — so the children are in the same column for the group's
/// whole life; only a lone non-shell call, which owns no parent row, keeps its
/// current position. Two columns, matching [`ACTIVITY_INDENT`]: the hierarchy
/// is stated by position, never by a box.
pub(crate) const GROUP_BODY_INDENT: &str = "  ";

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
    let mut group_rows = Vec::new();
    let lines = render_group_rows(
        group,
        theme,
        width,
        locale,
        t,
        now_elapsed_secs,
        awaiting_approval,
        focused_command,
        &mut group_rows,
    );
    rows.extend(group_rows);
    lines
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
            display: crate::fold::DisplayMode::Collapsed,
            round: None,
        }
    }

    fn open_group(calls: Vec<ToolCallBlock>) -> ToolGroupBlock {
        ToolGroupBlock {
            calls,
            open: true,
            display: crate::fold::DisplayMode::Collapsed,
            round: None,
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

    #[test]
    fn a_running_tool_accents_only_its_action_not_its_target() {
        let theme = Theme::dark();
        let lines = styled_group(
            vec![call(
                "read_file",
                r#"{"path":"target.rs"}"#,
                ToolStatus::Running,
            )],
            &theme,
        );
        let action = exploration_verb("read_file", Locale::Zh.text());
        let action_fg = lines.iter().find_map(|line| {
            line.spans
                .iter()
                .find(|span| span.content == action)
                .and_then(|span| span.style.fg)
        });

        assert_eq!(action_fg, Some(theme.accent.secondary));
        assert_eq!(body_fg(&lines, "target.rs"), Some(theme.ink(Ink::Active)));
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

    /// R2: a live exploration burst is ONE running receipt. The per-call rows
    /// are exactly the execution-log waterfall this contract removes; the
    /// receipt says what the burst is doing and how much of it there is.
    #[test]
    fn live_read_burst_is_one_running_receipt() {
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
        assert_eq!(lines.len(), 1, "one live receipt: {lines:?}");
        assert_eq!(
            lines[0].trim_end(),
            "\u{25cc} 正在读取 · 4 个文件",
            "{lines:?}"
        );
        assert!(
            !lines[0].contains("f0.rs"),
            "the files are not re-listed: {lines:?}"
        );
        assert!(
            !lines[0].contains('\u{2713}'),
            "reading is still not a result: {lines:?}"
        );
        assert!(
            !lines[0].contains('\u{25b8}'),
            "an open burst is not history yet: {lines:?}"
        );
    }

    /// R3: closed exploration is history, and history is a compact receipt that
    /// is ALSO the group's fold row (`\u{25b8}`). The per-call rows are not gone —
    /// they are folded behind the receipt; expanding restores every member row
    /// naming its own file.
    #[test]
    fn closed_reads_are_one_compact_receipt() {
        let lines = render_group_text(&group(reads(4)), 100, Locale::Zh);
        assert_eq!(lines.len(), 1, "one line, no children: {lines:?}");
        assert_eq!(lines[0].trim_end(), "\u{25b8} 读取 4 个文件", "{lines:?}");
        assert!(
            !lines[0].contains("f0.rs"),
            "the files are not re-listed: {lines:?}"
        );
        assert!(
            group_has_disclosure(&group(reads(4))),
            "the collapsed receipt is a click target"
        );
    }

    /// TOOL-GROUP-3: expanding the receipt restores EVERY real member row, so
    /// the fold is view-time only — nothing was merged away, and the target the
    /// receipt counted is still inspectable.
    #[test]
    fn expanding_a_compact_receipt_restores_every_member_row() {
        let mut opened = group(reads(4));
        opened.display = crate::fold::DisplayMode::Expanded;
        let lines = render_group_text(&opened, 100, Locale::Zh);
        assert_eq!(
            lines[0].trim_end(),
            "\u{25be} 读取 4 个文件",
            "the aggregate header stays: {lines:?}"
        );
        let text = lines.join("\n");
        for i in 0..4 {
            assert!(text.contains(&format!("f{i}.rs")), "member lost: {text}");
        }
    }

    /// Each evidence row answers what ran, on what, and what came back.
    #[test]
    fn an_evidence_row_names_the_tool_the_target_and_the_result() {
        let mut c = call("grep", r#"{"pattern":"WorkerQueueGroups"}"#, ToolStatus::Ok);
        c.preview = Some((0..13).map(|i| format!("hit {i}\n")).collect());
        let text = render_group_text(&group(vec![c]), 100, Locale::Zh).join("\n");
        assert!(
            text.contains(Locale::Zh.text().explore_verb_search),
            "the tool: {text}"
        );
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
        assert!(
            text.contains(Locale::Zh.text().explore_verb_search),
            "{text}"
        );
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
            .map(|i| work(&format!(r#"{{"path":"p{i}.rs"}}"#), ToolStatus::Ok))
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

    // Run/tree/batch mechanics are about the SHAPE. A uniform exploration
    // stretch is a compact receipt now, so a mechanics fixture must use a
    // NON-exploration tool — otherwise the test would be measuring the receipt
    // it is not about.
    const WORK: &str = "custom_probe";
    const WORK_B: &str = "custom_scan";
    const WORK_C: &str = "custom_check";

    fn work(args: &str, status: ToolStatus) -> ToolCallBlock {
        call(WORK, args, status)
    }

    fn work_b(args: &str, status: ToolStatus) -> ToolCallBlock {
        call(WORK_B, args, status)
    }

    fn work_c(args: &str, status: ToolStatus) -> ToolCallBlock {
        call(WORK_C, args, status)
    }

    /// `n` consecutive calls of one non-exploration tool, each with a
    /// renderable target — the run fixture.
    fn works(n: usize) -> Vec<ToolCallBlock> {
        (0..n)
            .map(|i| work(&format!(r#"{{"path":"p{i}.rs"}}"#), ToolStatus::Ok))
            .collect()
    }

    fn work_batched(args: &str, status: ToolStatus, batch: Option<u32>) -> ToolCallBlock {
        batched(WORK, args, status, batch)
    }

    fn work_b_batched(args: &str, status: ToolStatus, batch: Option<u32>) -> ToolCallBlock {
        batched(WORK_B, args, status, batch)
    }

    fn work_parallel_call(args: &str) -> ToolCallBlock {
        parallel_call(WORK, args)
    }

    /// One call is not a run: its own row already names the tool and the file,
    /// and a head above it would be that row said twice.
    #[test]
    fn a_lone_call_is_not_a_tool_run() {
        let lines = render_group_text(&group(reads(1)), 100, Locale::Zh);
        assert_eq!(lines.len(), 1, "one call, one row: {lines:?}");
        assert!(
            lines[0].starts_with("\u{203a} 读取 f0.rs"),
            "the row names the bare verb and the file: {lines:?}"
        );
        assert!(
            !lines[0].contains("读取文件"),
            "not the tool noun: {lines:?}"
        );
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
            lines[0].starts_with("\u{203a} 读取 f0.rs"),
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
        let lines = render_group_text(&group(works(2)), 100, Locale::Zh);
        assert_eq!(lines.len(), 3, "a head and two children: {lines:?}");
        assert!(
            lines[0].starts_with("\u{203a} custom_probe"),
            "the head wears the anchor and names the tool: {lines:?}"
        );
        assert!(
            !lines[0].contains("p0.rs"),
            "the head must not list what the rows below show: {lines:?}"
        );
        assert!(
            lines[1].contains("\u{251c}\u{2500} ") && lines[1].contains("p0.rs"),
            "{lines:?}"
        );
        assert!(
            lines[2].contains("\u{2514}\u{2500} ") && lines[2].contains("p1.rs"),
            "{lines:?}"
        );
        assert!(
            !lines[1].contains("custom_probe") && !lines[2].contains("custom_probe"),
            "a child must not repeat the head's label: {lines:?}"
        );
    }

    /// A long run stays complete: no truncation policy rides along with this.
    #[test]
    fn a_long_tool_run_shows_every_child() {
        let lines = render_group_text(&group(works(15)), 100, Locale::Zh);
        assert_eq!(lines.len(), 16, "a head and fifteen children: {lines:?}");
        let text = lines.join("\n");
        for i in 0..15 {
            assert!(text.contains(&format!("p{i}.rs")), "lost p{i}.rs: {text}");
        }
        assert!(!text.contains('\u{2026}'), "nothing elided: {text}");
    }

    /// The run is how a non-exploration burst reads WHILE it runs, not a shape
    /// applied to finished history: two settled calls inside an open group are
    /// a run too. (Exploration is a receipt at every stage now.)
    #[test]
    fn a_tool_run_forms_while_the_burst_is_still_open() {
        let lines = render_group_text(&open_group(works(2)), 100, Locale::Zh);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains("custom_probe"), "{lines:?}");
        assert!(
            !lines[0].contains('\u{25b8}'),
            "an open burst is not history yet: {lines:?}"
        );
        assert!(lines[2].contains("\u{2514}\u{2500} "), "{lines:?}");
    }

    // ── A group header is the STAGE's parent row ─────────────────────────
    //
    // A finished multi-call group reads as `assistant text → group summary →
    // the calls it made`. The parent states the aggregate a child cannot: how
    // many ran, whether any failed, and the whole stretch's duration. The
    // children step in one level so the ownership is visible, not only
    // structural. A `Run`/`Batch` child paints its own head and is left alone.

    /// A: an ordinary mixed group keeps its parent, its outcome and its rows.
    #[test]
    fn a_clean_mixed_group_keeps_a_parent_header_above_its_rows() {
        let g = group(vec![
            work(r#"{"path":"README.md"}"#, ToolStatus::Ok),
            work_b(r#"{"pattern":"TaskStatus"}"#, ToolStatus::Ok),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 3, "one parent, one row per call: {lines:?}");
        assert!(
            lines[0].starts_with('\u{25b8}')
                && lines[0].contains("完成 2 项操作")
                && lines[0].contains("全部成功"),
            "the parent states the outcome: {lines:?}"
        );
        assert!(
            lines[1].starts_with("  \u{203a} ")
                && lines[1].contains("custom_probe")
                && lines[1].contains("README.md"),
            "the first call is a nested child: {lines:?}"
        );
        assert!(
            lines[2].starts_with("  \u{203a} ")
                && lines[2].contains("custom_scan")
                && lines[2].contains("TaskStatus"),
            "the second call is a nested child: {lines:?}"
        );
    }

    /// B: three different tools stay three linear rows — a mixed stretch is
    /// never welded into one run under a generic label.
    #[test]
    fn three_different_tools_stay_three_rows() {
        let g = group(vec![
            work(r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            work_b(r#"{"pattern":"x"}"#, ToolStatus::Ok),
            work_c(r#"{"path":"a.rs"}"#, ToolStatus::Ok),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 4, "parent plus three rows: {lines:?}");
        assert!(
            lines[1..].iter().all(|l| l.starts_with("  \u{203a} ")),
            "every child is nested, one row each: {lines:?}"
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
                .filter(|l| l.trim_start().starts_with("\u{203a} \u{2717}"))
                .count(),
            2,
            "and every failed row keeps its own mark: {lines:?}"
        );
    }

    /// D: "全部成功" is a claim about every row, so only rows that actually
    /// SUCCEEDED may earn it. Counting failures is not enough — a cancelled or
    /// unknown call fails at nothing, and the parent would then contradict the
    /// `⊘ 已停止` row sitting right under it in the same group.
    #[test]
    fn a_stage_only_claims_all_ok_when_every_visible_call_succeeded() {
        let read = || work(r#"{"path":"a.rs"}"#, ToolStatus::Ok);
        let grep = |status| work_b(r#"{"pattern":"x"}"#, status);

        // A: the control — two successes do claim it.
        let all_ok = render_group_text(&group(vec![read(), grep(ToolStatus::Ok)]), 100, Locale::Zh);
        assert!(all_ok[0].contains("全部成功"), "{all_ok:?}");

        // B/C: a settled call that did not succeed is not a success, and the
        // parent must not announce an outcome it never observed.
        for (label, status) in [
            ("cancelled", ToolStatus::Cancelled),
            ("unknown", ToolStatus::Unknown),
        ] {
            let lines = render_group_text(&group(vec![read(), grep(status)]), 100, Locale::Zh);
            assert_eq!(
                lines.iter().filter(|l| l.contains(TOOL_ANCHOR)).count(),
                2,
                "the parent keeps one row per call: {lines:?}"
            );
            assert!(
                !lines[0].contains("全部成功"),
                "{label} is not a success: {lines:?}"
            );
        }

        // Two non-successes are no more a success than one.
        let lines = render_group_text(
            &group(vec![
                shell(r#"{"program":"a"}"#, ToolStatus::Cancelled, 5),
                shell(r#"{"program":"b"}"#, ToolStatus::Cancelled, 5),
            ]),
            100,
            Locale::Zh,
        );
        assert!(!lines[0].contains("全部成功"), "{lines:?}");
    }

    /// The stage's duration is a fact no child row can state: serial calls add
    /// up, so `0.2s + 2.7s` is the `2.9s` the parent shows.
    #[test]
    fn a_group_header_sums_the_wall_clock_of_serial_calls() {
        let mut first = call(
            "run_command",
            r#"{"program":"cargo","args":["test"]}"#,
            ToolStatus::Ok,
        );
        first.duration_ms = Some(200);
        let mut second = call(
            "run_command",
            r#"{"program":"cargo","args":["build"]}"#,
            ToolStatus::Ok,
        );
        second.duration_ms = Some(2_700);
        let lines = render_group_text(&group(vec![first, second]), 120, Locale::Zh);
        assert!(lines[0].starts_with('\u{25b8}'), "{lines:?}");
        assert!(lines[0].contains("执行了 2 个命令"), "{lines:?}");
        assert!(lines[0].contains("全部成功"), "{lines:?}");
        assert!(
            lines[0].contains("2.9s"),
            "the stage's whole wall clock: {lines:?}"
        );
    }

    /// A failure keeps its count on the parent and its reason under its own
    /// nested row — the fold never hides why the stage failed.
    #[test]
    fn a_failed_stage_nests_its_error_under_the_failed_row() {
        let mut bad = call(
            "run_command",
            r#"{"program":"cargo","args":["bogus"]}"#,
            ToolStatus::Failed,
        );
        bad.preview = Some("error: no such command: `bogus`\nhelp dump".into());
        let good = call(
            "run_command",
            r#"{"program":"cargo","args":["test"]}"#,
            ToolStatus::Ok,
        );
        let lines = render_group_text(&group(vec![good, bad]), 120, Locale::Zh);
        assert!(
            lines[0].contains('\u{2717}') && lines[0].contains("1 个失败"),
            "the parent counts the failure: {lines:?}"
        );
        assert!(
            lines[1].starts_with("  \u{203a} \u{2713} $ cargo test"),
            "the success is a nested child: {lines:?}"
        );
        assert!(
            lines[2].starts_with("  \u{203a} \u{2717} $ cargo bogus"),
            "the failure is a nested child: {lines:?}"
        );
        assert!(
            lines[3].starts_with("    \u{2514} ") && lines[3].contains("no such command"),
            "the reason hangs one level under the failed row: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("help dump")),
            "OUTPUT stays folded; only the reason shows by default: {lines:?}"
        );
    }

    // ── Layout stability across a group's life ──────────────────────────────
    //
    // A group's parent row is STRUCTURAL: it exists from the first visible call
    // and closing the group only REWRITES its text. These tests compare the
    // snapshots of one group at each life stage by GEOMETRY — parent index,
    // child column, row count — so a close that inserts a row or re-indents a
    // child fails here even when the strings look reasonable.

    /// The 0-based index of the group's first child row.
    fn first_child_row(lines: &[String]) -> usize {
        lines
            .iter()
            .position(|l| l.trim_start().starts_with(TOOL_ANCHOR))
            .expect("group has a child row")
    }

    /// The starting COLUMN of the group's first child row — the group-body
    /// indent made concrete.
    fn first_child_column(lines: &[String]) -> usize {
        lines[first_child_row(lines)]
            .chars()
            .take_while(|c| *c == ' ')
            .count()
    }

    fn shell(args: &str, status: ToolStatus, ms: u64) -> ToolCallBlock {
        let mut c = call("run_command", args, status);
        c.duration_ms = Some(ms);
        c
    }

    /// Section 13: the SAME group at t0 (one call), t1 (two calls) and t2
    /// (closed). The parent is row 0 and the first child sits at the same
    /// column in all three; growth only appends, close only rewrites.
    #[test]
    fn a_tool_group_lifecycle_never_moves_its_first_child() {
        let check = || {
            shell(
                r#"{"program":"cargo","args":["check"]}"#,
                ToolStatus::Running,
                200,
            )
        };
        let test = || {
            shell(
                r#"{"program":"cargo","args":["test"]}"#,
                ToolStatus::Running,
                200,
            )
        };
        let settled = |c: ToolCallBlock| {
            let mut c = c;
            c.status = ToolStatus::Ok;
            c
        };

        let t0 = render_group_text(&open_group(vec![check()]), 100, Locale::Zh);
        let t1 = render_group_text(&open_group(vec![check(), test()]), 100, Locale::Zh);
        let t2 = render_group_text(
            &group(vec![settled(check()), settled(test())]),
            100,
            Locale::Zh,
        );

        for (name, lines) in [("t0", &t0), ("t1", &t1), ("t2", &t2)] {
            assert_eq!(first_child_row(lines), 1, "{name}: parent leads: {lines:?}");
            assert_eq!(
                first_child_column(lines),
                GROUP_BODY_INDENT.len(),
                "{name}: child indent never changes: {lines:?}"
            );
        }
        assert_eq!(t2.len(), t1.len(), "close adds no row: {t1:?} vs {t2:?}");
        assert_eq!(t1.len(), t0.len() + 1, "growth only appends: {t0:?}");
    }

    /// Section 12A/D: an open multi-call group HAS a parent row, and that row
    /// speaks in the running tense — no outcome it has not reached.
    #[test]
    fn an_open_group_parent_states_running_never_an_outcome() {
        let g = open_group(vec![
            shell(
                r#"{"program":"cargo","args":["check"]}"#,
                ToolStatus::Running,
                100,
            ),
            shell(
                r#"{"program":"cargo","args":["test"]}"#,
                ToolStatus::Running,
                100,
            ),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert!(lines[0].starts_with('\u{22ee}'), "running glyph: {lines:?}");
        assert!(lines[0].contains("正在执行"), "{lines:?}");
        assert!(lines[0].contains("2 个命令"), "{lines:?}");
        assert!(!lines[0].contains("全部成功"), "no success yet: {lines:?}");
        assert!(!lines[0].contains("失败"), "no verdict yet: {lines:?}");
        assert!(
            !lines[0].contains("执行了"),
            "not the past tense: {lines:?}"
        );
        assert!(!lines[0].contains("s"), "no final duration: {lines:?}");
    }

    /// Section 12E: 1 → 2 → 3 calls. The parent's count follows the burst and
    /// the hierarchy never changes.
    #[test]
    fn a_growing_stage_updates_its_count_without_touching_the_hierarchy() {
        let shell_at = |n: usize| {
            let mut c = call(
                "run_command",
                &format!(r#"{{"program":"cargo","args":["run{n}"]}}"#),
                ToolStatus::Running,
            );
            c.duration_ms = Some(100);
            c
        };
        let s1 = render_group_text(&open_group(vec![shell_at(1)]), 100, Locale::Zh);
        let s2 = render_group_text(&open_group(vec![shell_at(1), shell_at(2)]), 100, Locale::Zh);
        let s3 = render_group_text(
            &open_group(vec![shell_at(1), shell_at(2), shell_at(3)]),
            100,
            Locale::Zh,
        );
        assert!(
            s1[0].contains("1 个命令") && s2[0].contains("2 个命令") && s3[0].contains("3 个命令"),
            "{s1:?} {s2:?} {s3:?}"
        );
        for lines in [&s1, &s2, &s3] {
            assert_eq!(first_child_row(lines), 1, "{lines:?}");
            assert_eq!(
                first_child_column(lines),
                GROUP_BODY_INDENT.len(),
                "{lines:?}"
            );
        }
        assert_eq!(s1.len() + 1, s2.len());
        assert_eq!(s2.len() + 1, s3.len());
    }

    /// Section 12C/F: closing a clean stage rewrites the parent in place — same
    /// rows, same child column — and the run's FIRST second already had it.
    #[test]
    fn closing_a_clean_stage_rewrites_its_parent_in_place() {
        let check = shell(
            r#"{"program":"cargo","args":["check"]}"#,
            ToolStatus::Ok,
            200,
        );
        let test = shell(
            r#"{"program":"cargo","args":["test"]}"#,
            ToolStatus::Ok,
            200,
        );
        let open = render_group_text(
            &open_group(vec![check.clone(), test.clone()]),
            100,
            Locale::Zh,
        );
        let closed = render_group_text(&group(vec![check, test]), 100, Locale::Zh);

        assert_eq!(
            open.len(),
            closed.len(),
            "no row is inserted: {open:?} vs {closed:?}"
        );
        assert_eq!(
            first_child_column(&open),
            first_child_column(&closed),
            "the child never moves: {open:?} vs {closed:?}"
        );
        assert!(
            open[0].starts_with('\u{22ee}') && open[0].contains("正在执行"),
            "{open:?}"
        );
        assert!(
            closed[0].starts_with('\u{25b8}')
                && closed[0].contains("执行了 2 个命令")
                && closed[0].contains("全部成功")
                && closed[0].contains("0.4s"),
            "the same row now states the outcome: {closed:?}"
        );
    }

    /// Section 12G: a failure changes only the parent's verdict; the rows and
    /// the reason stay exactly where they were.
    #[test]
    fn a_failing_stage_keeps_its_layout_and_gains_only_the_verdict() {
        let mut bad = shell(
            r#"{"program":"cargo","args":["bogus"]}"#,
            ToolStatus::Failed,
            200,
        );
        bad.preview = Some("error: no such command: `bogus`\nhelp dump".into());
        let good = shell(
            r#"{"program":"cargo","args":["test"]}"#,
            ToolStatus::Ok,
            200,
        );
        // The same two calls, one group still open (the failing call's verdict
        // is already known, the other is still running) and one closed.
        let mut running = good.clone();
        running.status = ToolStatus::Running;
        let open = render_group_text(&open_group(vec![running, bad.clone()]), 120, Locale::Zh);
        let closed = render_group_text(&group(vec![good, bad]), 120, Locale::Zh);

        assert_eq!(open.len(), closed.len(), "{open:?} vs {closed:?}");
        assert_eq!(
            first_child_column(&open),
            first_child_column(&closed),
            "{open:?} vs {closed:?}"
        );
        assert!(
            open[0].contains("正在执行") && !open[0].contains("失败"),
            "{open:?}"
        );
        assert!(
            closed[0].contains('\u{2717}')
                && closed[0].contains("1 个失败")
                && closed[0].contains("0.4s"),
            "{closed:?}"
        );
        assert!(
            closed.iter().any(|l| l.contains("no such command")),
            "the reason survives the close: {closed:?}"
        );
    }

    /// Section 12H: sub-100ms calls — the case the flicker was worst in — still
    /// produce exactly one parent and one row per call.
    #[test]
    fn a_fast_stage_stays_one_parent_over_its_rows() {
        let fast = |n: usize, ms: u64| {
            let mut c = call(
                "run_command",
                &format!(r#"{{"program":"echo","args":["c{n}"]}}"#),
                ToolStatus::Ok,
            );
            c.duration_ms = Some(ms);
            c
        };
        let g = group(vec![fast(1, 0), fast(2, 100), fast(3, 200)]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 4, "one parent plus three rows: {lines:?}");
        assert_eq!(first_child_row(&lines), 1, "{lines:?}");
        assert!(lines[0].contains("执行了 3 个命令"), "{lines:?}");
    }

    /// Section 12I: the fixed child indent must not push a row past the gutter.
    #[test]
    fn a_stage_fits_its_narrow_terminal() {
        let check = shell(
            r#"{"program":"cargo","args":["check"]}"#,
            ToolStatus::Ok,
            400,
        );
        let test = shell(
            r#"{"program":"cargo","args":["test","-p","leveler-tui"]}"#,
            ToolStatus::Ok,
            400,
        );
        for width in [50usize, 60] {
            let open = render_group_text(
                &open_group(vec![check.clone(), test.clone()]),
                width,
                Locale::Zh,
            );
            let closed =
                render_group_text(&group(vec![check.clone(), test.clone()]), width, Locale::Zh);
            for lines in [&open, &closed] {
                for l in lines {
                    assert!(
                        UnicodeWidthStr::width(l.as_str()) <= width,
                        "{width}: {l:?}"
                    );
                }
                assert_eq!(
                    first_child_column(lines),
                    GROUP_BODY_INDENT.len(),
                    "{width}: {lines:?}"
                );
            }
        }
    }

    /// A burst the reducer observed overlapping costs its LONGEST member, not
    /// the sum: four 5s reads did not take 20s.
    #[test]
    fn group_duration_folds_a_parallel_burst_to_its_longest_member() {
        let mut a = batched("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok, Some(7));
        a.duration_ms = Some(5_000);
        let mut b = batched("read_file", r#"{"path":"b.rs"}"#, ToolStatus::Ok, Some(7));
        b.duration_ms = Some(5_000);
        let mut c = call(
            "run_command",
            r#"{"program":"cargo","args":["test"]}"#,
            ToolStatus::Ok,
        );
        c.duration_ms = Some(1_500);
        let refs: Vec<&ToolCallBlock> = vec![&a, &b, &c];
        assert_eq!(
            group_duration_ms(&refs),
            Some(6_500),
            "one 5s burst (not 10s) plus the 1.5s serial call"
        );
    }

    /// A partial total is not a total: one unreported call makes the stage's
    /// duration unknown rather than understated.
    #[test]
    fn group_duration_is_unknown_until_every_call_reports_one() {
        let a = call("read_file", r#"{"path":"a.rs"}"#, ToolStatus::Ok);
        let mut b = call("grep", r#"{"pattern":"x"}"#, ToolStatus::Running);
        b.duration_ms = None;
        assert_eq!(group_duration_ms(&[&a, &b]), None);
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

    /// E: a mixed group in flight wears its LIVE header (`⋮`, never `▸`), so
    /// the parent row exists while the rows beneath it are already settling.
    #[test]
    fn a_running_mixed_group_keeps_its_live_rows() {
        let g = open_group(vec![
            work(r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            work_b(r#"{"pattern":"x"}"#, ToolStatus::Running),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(lines.len(), 3, "a live parent plus two rows: {lines:?}");
        assert!(
            lines[0].starts_with('\u{22ee}'),
            "in flight is not history: {lines:?}"
        );
        assert!(
            lines[2].contains('\u{25cc}'),
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
        let mut calls = works(2);
        calls.extend((0..2).map(|i| work_b(&format!(r#"{{"path":"q{i}.rs"}}"#), ToolStatus::Ok)));
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        let heads: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.contains('\u{251c}') && !l.contains('\u{2514}'))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(heads.len(), 2, "two runs, two heads: {lines:?}");
        assert!(lines[heads[0]].contains("custom_probe"), "{lines:?}");
        assert!(lines[heads[1]].contains("custom_scan"), "{lines:?}");
        assert!(
            lines[heads[1] - 1].contains("p1.rs"),
            "the first run closes before the second opens: {lines:?}"
        );
    }

    /// A burst the model issued in parallel is the COMMON case for reads, and
    /// it was still printing the tool's name on every child under a count of
    /// files. Concurrency is a fact and stays on the head; the repetition is
    /// not, and goes.
    #[test]
    fn a_batch_of_one_tool_reads_as_a_run_that_kept_its_parallel_fact() {
        let calls: Vec<ToolCallBlock> = (0..6)
            .map(|i| work_batched(&format!(r#"{{"path":"p{i}.rs"}}"#), ToolStatus::Ok, Some(1)))
            .collect();
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        assert_eq!(lines.len(), 7, "one head, six children: {lines:?}");
        assert!(
            lines[0].starts_with("\u{203a} custom_probe") && lines[0].contains("并行"),
            "the head names the tool once and keeps the concurrency: {lines:?}"
        );
        assert!(
            !lines[0].contains("t0"),
            "the head must not list what the children show: {lines:?}"
        );
        assert!(
            lines[1].starts_with("  \u{251c}\u{2500} \u{2713} p0.rs")
                && lines[6].starts_with("  \u{2514}\u{2500} \u{2713} p5.rs"),
            "children carry only their target: {lines:?}"
        );
        assert!(
            !lines[1].contains("custom_probe"),
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
        let mut calls = works(2);
        for c in &mut calls {
            c.preview = Some("//! a module\npub fn a() {}\n".into());
        }
        let mut g = group(calls);
        g.display = crate::fold::DisplayMode::Expanded;
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
            work_batched(r#"{"path":"a.rs"}"#, ToolStatus::Ok, Some(1)),
            work_b_batched(r#"{"pattern":"x"}"#, ToolStatus::Ok, Some(1)),
        ];
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        let text = lines.join("\n");
        assert!(text.contains("并行"), "{lines:?}");
        assert!(
            text.contains("custom_probe") && text.contains("custom_scan"),
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
            work_b_batched(r#"{"path":"a.rs"}"#, ToolStatus::Ok, Some(7)),
            work_b_batched(r#"{"path":"b.rs"}"#, ToolStatus::Ok, Some(7)),
            work_batched(r#"{"path":"c.rs"}"#, ToolStatus::Ok, Some(7)),
        ];
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        let text = lines.join("\n");
        assert!(text.contains("并行"), "the batch names itself: {text}");
        assert!(
            lines[0].starts_with(TOOL_ANCHOR),
            "a settled batch keeps the same execution anchor as every tool row: {lines:?}"
        );
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
            work_batched(r#"{"path":"a.rs"}"#, ToolStatus::Ok, None),
            work_batched(r#"{"path":"b.rs"}"#, ToolStatus::Ok, None),
        ];
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        let text = lines.join("\n");
        assert!(!text.contains("并行"), "no batch, no claim: {text}");
        assert!(
            lines[0].starts_with(&format!("{TOOL_ANCHOR} custom_probe")),
            "consecutive calls are a run, which claims no concurrency: {lines:?}"
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
            .map(|i| work_b_batched(&format!(r#"{{"path":"a{i}.rs"}}"#), ToolStatus::Ok, Some(1)))
            .collect();
        calls.extend(
            (0..2).map(|i| {
                work_b_batched(&format!(r#"{{"path":"b{i}.rs"}}"#), ToolStatus::Ok, Some(2))
            }),
        );
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
                work_batched(
                    &format!(r#"{{"path":"p{i}.rs"}}"#),
                    ToolStatus::Running,
                    Some(4),
                )
            })
            .collect();
        let text = render_group_text(&open_group(calls), 100, Locale::Zh).join("\n");
        for i in 0..17 {
            assert!(
                text.contains(&format!("p{i}.rs")),
                "member p{i} hidden: {text}"
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

    /// CASE B: a wide live read burst is ONE running receipt. It counts the
    /// burst and names no file — a per-file row is the waterfall, not evidence.
    #[test]
    fn a_wide_live_read_burst_is_one_running_receipt() {
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
        assert_eq!(lines.len(), 1, "one receipt: {lines:?}");
        assert_eq!(
            lines[0].trim_end(),
            "\u{25cc} 正在读取 · 7 个文件",
            "{lines:?}"
        );
        assert!(
            !lines[0].contains(".rs"),
            "every member is accounted for by the count, not a row: {lines:?}"
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
            work(r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            work(r#"{"path":"b.rs"}"#, ToolStatus::Ok),
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

    /// The transcript's first-level anchors all sit at the same column: `▌` for
    /// the user, `●` for the agent, `◆` for a Thought and `›` for a tool group.
    /// A tool is a SIBLING of the Thought above it, never its child; only a
    /// fold's children step in one level.
    #[test]
    fn tool_activity_renders_at_the_transcript_baseline() {
        let g = group(vec![call(
            "read_file",
            r#"{"path":"a.rs"}"#,
            ToolStatus::Ok,
        )]);
        let lines = render_activity(
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
        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(
            text.iter().all(|l| l.starts_with(TOOL_ANCHOR)),
            "tool rows sit at the transcript baseline: {text:?}"
        );
    }

    /// THE regression: an OPEN group whose members are all momentarily
    /// settled (the model is still streaming the next parallel call) must
    /// NOT render as completed history. History begins when the group
    /// closes, not when its current members happen to be settled.
    #[test]
    fn an_open_group_with_all_members_settled_is_not_history() {
        let mut calls: Vec<ToolCallBlock> = (0..7)
            .map(|i| work(&format!(r#"{{"path":"p{i}.rs"}}"#), ToolStatus::Ok))
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
            (WORK, r#"{"path":"a.rs"}"#),
            (WORK_B, r#"{"pattern":"BrowserSession"}"#),
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
                work_batched(
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
            work(r#"{"path":"internal/bot/service.go"}"#, ToolStatus::Ok),
            work(r#"{"path":"internal/model/bot.go"}"#, ToolStatus::Ok),
            work_b(r#"{"pattern":"TokenPlain"}"#, ToolStatus::Running),
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
            work_batched(r#"{"path":"a.rs"}"#, ToolStatus::Ok, Some(2)),
            work_batched(r#"{"path":"b.rs"}"#, ToolStatus::Running, Some(2)),
            work_b_batched(r#"{"pattern":"x"}"#, ToolStatus::Running, Some(2)),
        ]);
        let lines = render_group_text(&g, 100, Locale::Zh);
        assert_eq!(live_member_rows(&lines), 2, "two still running: {lines:?}");
        let settled = lines
            .iter()
            .find(|l| l.contains("a.rs"))
            .expect("the finished member keeps its row");
        assert!(settled.contains('\u{2713}'), "settled work: {settled}");
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
            c.preview = Some(format!(
                "{NETWORK_PERMISSION_REQUIRED} blocked…\n\nexit: 6\n--- stderr ---\ncurl: (6) Could not resolve host"
            ));
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
                .any(|l| l.contains(NETWORK_PERMISSION_REQUIRED)),
            "{lines:?}"
        );
    }

    /// Once every call finishes, the group is history: the live `◌` rows are
    /// gone and the clickable summary heads the evidence that remains.
    #[test]
    fn a_finished_group_folds_and_drops_the_live_rendering() {
        let g = group(vec![
            work(r#"{"path":"a.rs"}"#, ToolStatus::Ok),
            work(r#"{"path":"b.rs"}"#, ToolStatus::Ok),
            work_b(r#"{"path":"x"}"#, ToolStatus::Ok),
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
        assert_eq!(
            lines.len(),
            2,
            "a live stage row plus one command row: {lines:?}"
        );
        assert!(lines[0].starts_with('\u{22ee}'), "{lines:?}");
        assert!(lines[1].contains("◌ $ echo start"), "{lines:?}");
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
        open.display = crate::fold::DisplayMode::Expanded;
        let lines = render_group_text(&open, 80, Locale::Zh);
        assert!(
            !lines.iter().any(|l| l.contains("{\"target\"")),
            "expanded detail must not dump raw JSON args: {lines:?}"
        );
    }

    /// The summary label classifies work outside shell/read/search too. Tested
    /// with two calls because a lone non-shell call owns no summary row — the
    /// run or its own row says everything the summary would.
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
    fn a_mixed_group_folds_its_exploration_stretch_only() {
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
            lines[0].starts_with('▸') && lines[0].contains("读取 1 个文件 · 搜索 1 次"),
            "the exploration stretch is one receipt: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("cargo test")),
            "the run keeps its own row instead of joining the receipt: {lines:?}"
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
                .map(|i| work_batched(&format!(r#"{{"path":"p{i}.rs"}}"#), ToolStatus::Ok, Some(5)))
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
                text.contains(&format!("p{i}.rs")),
                "member p{i} lost: {text}"
            );
        }
    }

    #[test]
    fn a_running_batch_shows_the_live_member_and_keeps_the_settled_ones() {
        let mut calls: Vec<ToolCallBlock> = (0..4)
            .map(|i| work_batched(&format!(r#"{{"path":"p{i}.rs"}}"#), ToolStatus::Ok, Some(6)))
            .collect();
        calls[2].status = ToolStatus::Running;
        let lines = render_group_text(&group(calls), 100, Locale::Zh);
        let text = lines.join("\n");
        assert!(
            lines
                .iter()
                .any(|l| l.contains('\u{25cc}') && l.contains("p2.rs")),
            "the live call is marked live: {text}"
        );
        for i in [0usize, 1, 3] {
            assert!(
                text.contains(&format!("p{i}.rs")),
                "member p{i} lost: {text}"
            );
        }
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
                .map(|i| work_parallel_call(&format!(r#"{{"path":"p{i}.rs"}}"#)))
                .collect(),
        );
        g.display = crate::fold::DisplayMode::Expanded;
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
            work_batched(r#"{"path":"a.rs"}"#, ToolStatus::Ok, Some(1)),
            work_b_batched(r#"{"pattern":"x"}"#, ToolStatus::Ok, Some(1)),
            work_batched(r#"{"path":"b.rs"}"#, ToolStatus::Ok, Some(1)),
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
            work_batched(r#"{"path":"a.rs"}"#, ToolStatus::Ok, Some(1)),
            work_b_batched(r#"{"pattern":"x"}"#, ToolStatus::Ok, Some(1)),
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

    /// Reads and searches together are ONE exploration receipt that counts by
    /// KIND (`2 files, 2 searches`) — the reader still learns what the stretch
    /// did, without a row per call. A failure is NOT folded: the failing target
    /// must stay inspectable, so the stretch keeps its rows.
    #[test]
    fn exploration_burst_is_one_receipt_counting_by_kind() {
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
        assert_eq!(lines.len(), 1, "one receipt: {lines:?}");
        assert_eq!(
            lines[0].trim_end(),
            "\u{25b8} 读取 2 个文件 \u{b7} 搜索 2 次",
            "{lines:?}"
        );
        // A failure keeps the stretch's rows so the failing target is visible.
        let mut with_failure = g.clone();
        with_failure.calls[1].status = ToolStatus::Failed;
        with_failure.calls[1].preview = Some("invalid regex".into());
        let failed_lines = render_group_text(&with_failure, 80, Locale::Zh);
        let failed_text = failed_lines.join("\n");
        assert!(failed_text.contains("invalid regex"), "{failed_lines:?}");
        assert!(
            failed_text.contains("dist"),
            "the failing target stays on screen: {failed_lines:?}"
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
        assert!(
            lines[0].starts_with("\u{203a} 读取 src/auth.go"),
            "the row is the evidence: {lines:?}"
        );
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
        g.display = crate::fold::DisplayMode::Expanded;
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
        assert_eq!(
            lines.len(),
            2,
            "a stage row plus the one command row: {lines:?}"
        );
        assert!(
            lines[1].starts_with(&format!("  {TOOL_ANCHOR} ✓ $ cargo test")),
            "the row names the command it ran: {lines:?}"
        );
        assert!(
            lines[1].ends_with("1 行"),
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
        assert!(!g.expanded());
        let lines = render_group_text(&g, 120, Locale::Zh);
        assert_eq!(
            lines.len(),
            3,
            "a stage row, the call's row and its error: {lines:?}"
        );
        assert!(
            lines[1].starts_with(&format!("  {TOOL_ANCHOR} ✗ $ cargo test")),
            "the row names the failure and the command: {lines:?}"
        );
        assert!(
            lines[2].starts_with("    └ ") && lines[2].contains("error: no such command"),
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
            g.display = crate::fold::DisplayMode::Expanded;
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
        g.display = crate::fold::DisplayMode::Expanded;
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
        g.display = crate::fold::DisplayMode::Expanded;
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
            lines[1].starts_with(&format!("  {TOOL_ANCHOR} ◌ $ cargo build")),
            "{lines:?}"
        );
        assert!(lines[0].starts_with('\u{22ee}'), "{lines:?}");
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
            lines[1].contains("45s"),
            "running command must show live elapsed: {lines:?}"
        );
        assert!(lines[0].starts_with('\u{22ee}'), "{lines:?}");
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
            work_b(r#"{"pattern":"dist"}"#, ToolStatus::Ok),
            work_b(r#"{"pattern":"build"}"#, ToolStatus::Ok),
        ]);
        g.display = crate::fold::DisplayMode::Expanded;
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
                display: crate::fold::DisplayMode::Collapsed,
                round: None,
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

    /// Whether this row is a group's stage parent (`⋮` running / `▸` closed)
    /// rather than a command's own row.
    fn is_stage_parent(line: &str) -> bool {
        line.starts_with('\u{22ee}') || line.starts_with('\u{25b8}')
    }

    /// The command's own rows as its unit renderer produced them: the stage
    /// parent row a lone-command group now leads with, and the group-body
    /// indent the parent applies to its children, are stripped here. These
    /// tests pin the COMMAND row's shape; the parent row and the child indent
    /// have their own lifecycle test.
    fn body(lines: Vec<String>) -> Vec<String> {
        let lines = if lines.first().is_some_and(|l| is_stage_parent(l)) {
            lines.into_iter().skip(1).collect::<Vec<_>>()
        } else {
            lines
        };
        lines
            .into_iter()
            .map(|l| l.strip_prefix(GROUP_BODY_INDENT).unwrap_or(&l).to_string())
            .collect()
    }

    fn body_lines(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
        if text(&lines).first().is_some_and(|l| is_stage_parent(l)) {
            lines.into_iter().skip(1).collect()
        } else {
            lines
        }
    }

    fn rows(call: ToolCallBlock) -> Vec<String> {
        body(text(&lines_at(call, &Theme::no_color(), 100, 0)))
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
        let rows = body(text(&lines_at(c, &Theme::no_color(), 100, 16)));
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

    /// RUN-IA-3/5: opening a command reveals the COMPLETE captured output, not
    /// a bounded tail. Collapsed is the default; expanded is the reader's ask.
    #[test]
    fn opening_a_command_reveals_its_full_output() {
        let mut c = cmd(r#"{"program":"cargo","args":["test"]}"#, ToolStatus::Ok);
        c.duration_ms = Some(8200);
        let full: String = (0..85).map(|i| format!("line {i}\n")).collect();
        c.preview = Some(full.clone());
        c.output = full;
        c.expanded = true;
        let lines = lines_at(c, &Theme::no_color(), 100, 0);
        let text = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("$ cargo test"), "{text}");
        assert!(
            text.contains("line 84"),
            "the whole output is present: {text}"
        );
        assert!(
            !text.contains("\u{8fd8}\u{6709}"),
            "an opened command is never tail-truncated: {text}"
        );
    }

    /// RUN-IA-4: a failed command names its failure while collapsed. The
    /// reader never has to open the node to notice the red.
    #[test]
    fn a_collapsed_failed_command_names_the_failure() {
        let mut c = cmd(
            r#"{"program":"npm","args":["-s","run","test"]}"#,
            ToolStatus::Failed,
        );
        c.duration_ms = Some(6200);
        c.exit_code = Some(1);
        c.preview = Some("exit: 1\nportal-layout-contract.test.ts \u{b7} 1 failed\n".into());
        let rows = rows(c);
        assert!(
            rows.iter().any(|r| r.contains("failed")),
            "the failure is visible without expanding: {rows:?}"
        );
        assert!(
            rows[0].contains("\u{2717}"),
            "the row carries the failure glyph: {rows:?}"
        );
    }

    // ── HOW a command ran is never WHY it failed ────────────────────────────

    /// The runtime's own rows say how a command ran; the command's output says
    /// what happened. A failure reason may only come from the second.
    #[test]
    fn runtime_rows_are_neither_output_nor_a_failure_reason() {
        for row in [
            "exit: 1",
            "exit: signal",
            "--- stdout ---",
            "--- stderr ---",
            "[timed out after 120s]",
            "[timed out]",
            "[execution policy] Filesystem writes were confined to the granted paths.",
            "[execution policy] Network access was denied for this command.",
            "[mutation rejected] command exceeded the remaining file budget (modified 3)",
            "[note] a workspace mutation baseline was unavailable; file changes made by this \
             command were not tracked and cannot be rolled back.",
        ] {
            assert!(is_runtime_note(row), "{row}");
        }
        // A refusal is the tool's own structured reason, so it IS one.
        assert!(!is_runtime_note(
            "[permission refused] refused a command argument pointing at a credential-bearing file"
        ));
        // A locator points at output; it is not a note about how the command
        // ran, so it keeps its row in the expanded body.
        assert!(!is_runtime_note("[stderr full output: /tmp/artifact-7]"));
        assert!(!is_runtime_note("test case_1 ... ok"));
        assert!(!is_runtime_note("--- not a stream header"));
    }

    /// The dogfooded case: `git grep` matched nothing, so it exited 1 with an
    /// empty stderr while the preview carried the sandbox's write-confinement
    /// note. That note says HOW the command ran; the row read it aloud as WHY
    /// it failed.
    #[test]
    fn a_policy_note_is_not_shown_as_the_failure_reason() {
        let mut c = cmd(
            r#"{"program":"git","args":["grep","ZZZ_NOT_FOUND_ZZZ_9f3a"]}"#,
            ToolStatus::Failed,
        );
        c.duration_ms = Some(200);
        c.exit_code = Some(1);
        c.preview = Some(
            "exit: 1\n\n[execution policy] Filesystem writes were confined to the granted paths.\n"
                .to_string(),
        );
        let rows = rows(c);
        assert_eq!(
            rows,
            vec!["\u{203a} \u{2717} $ git grep ZZZ_NOT_FOUND_ZZZ_9f3a · 0.2s · exit 1"],
            "{rows:?}"
        );
    }

    /// stderr is the reason. Dropping the runtime's notes must not drop it.
    #[test]
    fn a_real_stderr_line_still_wins_over_the_runtime_note() {
        let mut c = cmd(
            r#"{"program":"cargo-nextest","args":["--version"]}"#,
            ToolStatus::Failed,
        );
        c.duration_ms = Some(100);
        c.exit_code = Some(127);
        c.preview = Some(
            "exit: 127\n--- stderr ---\nsh: cargo-nextest: command not found\n\n\
             [execution policy] Filesystem writes were confined to the granted paths.\n"
                .to_string(),
        );
        let rows = rows(c);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(
            rows[1], "  \u{2514} sh: cargo-nextest: command not found",
            "{rows:?}"
        );
    }

    /// A rejected mutation is a note on the execution too, not the command's
    /// own error.
    #[test]
    fn a_mutation_note_is_not_shown_as_the_failure_reason() {
        let mut c = cmd(SED, ToolStatus::Failed);
        c.duration_ms = Some(20);
        c.exit_code = Some(0);
        c.preview = Some(
            "exit: 0\n--- stdout ---\nok\n\n[mutation rejected] command exceeded the remaining \
             file budget (modified 3)\n"
                .to_string(),
        );
        let rows = rows(c);
        assert!(
            !rows.iter().any(|r| r.contains("[mutation rejected]")),
            "{rows:?}"
        );
    }

    /// `false`: exit 1, nothing printed, nothing broken. There is no reason to
    /// name, so the row names none instead of inventing one.
    #[test]
    fn a_failure_with_no_reported_reason_gets_no_reason_row() {
        let mut c = cmd(r#"{"program":"false"}"#, ToolStatus::Failed);
        c.exit_code = Some(1);
        c.preview = Some(
            "exit: 1\n\n[execution policy] Filesystem writes were confined to the granted paths.\n"
                .to_string(),
        );
        let rows = rows(c);
        assert_eq!(rows, vec!["\u{203a} \u{2717} $ false · exit 1"], "{rows:?}");
    }

    /// The runtime writes the limit that fired, and a timeout is stated as one
    /// instead of its own note becoming the reason.
    #[test]
    fn a_real_timeout_note_states_the_timeout_instead_of_becoming_the_reason() {
        let mut c = cmd(r#"{"program":"sleep","args":["300"]}"#, ToolStatus::Failed);
        c.duration_ms = Some(120_000);
        c.preview =
            Some("[timed out after 120s]\nexit: signal\n--- stdout ---\nstarting\n".to_string());
        let rows = rows(c);
        assert_eq!(
            rows,
            vec!["\u{203a} \u{2717} $ sleep 300 · 2m 00s · timeout"],
            "{rows:?}"
        );
    }

    /// A successful command is untouched by the filtering, but the note is not
    /// output: the row counts what the command printed.
    #[test]
    fn a_policy_note_does_not_count_as_output() {
        let mut c = cmd(r#"{"program":"printf","args":["smoke-a"]}"#, ToolStatus::Ok);
        c.duration_ms = Some(100);
        c.exit_code = Some(0);
        c.preview = Some(
            "exit: 0\n--- stdout ---\nsmoke-a\n\n[execution policy] Filesystem writes were \
             confined to the granted paths.\n"
                .to_string(),
        );
        let rows = rows(c.clone());
        assert_eq!(
            rows,
            vec!["\u{203a} \u{2713} $ printf smoke-a · 0.1s · 1 \u{884c}"],
            "{rows:?}"
        );
        // A batch child renders through the unit path instead; its count comes
        // from the same lines the expanded body shows.
        assert_eq!(content_line_count(&c), 1);
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
        let rows: Vec<String> = body(text(&render_group(
            &ToolGroupBlock {
                calls: vec![c],
                open: true,
                display: crate::fold::DisplayMode::Collapsed,
                round: None,
            },
            &Theme::no_color(),
            100,
            Locale::Zh,
            Locale::Zh.text(),
            5,
            Some(&id),
        )));
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
            let rows = body(text(&lines_at(c.clone(), &Theme::no_color(), width, 0)));
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
            let rows = body(text(&lines_at(c.clone(), &Theme::no_color(), width, 0)));
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

    /// A command that starts and settles inside the first paint interval must
    /// not briefly grow the transcript with output rows. That one-frame
    /// expansion followed by the settled single row makes the bottom-aligned
    /// conversation visibly jump.
    #[test]
    fn a_new_running_command_keeps_one_stable_row() {
        let c = with_output(cmd(SED, ToolStatus::Running), 2);
        let rows = body(text(&lines_at(c, &Theme::no_color(), 100, 0)));
        assert_eq!(rows.len(), 1, "{rows:?}");
    }

    /// A command in flight shows what it is printing: the row, then the last
    /// few output lines under it, with the rest counted, never poured in.
    #[test]
    fn a_running_command_shows_a_bounded_tail_of_its_live_output() {
        let c = with_output(cmd(SED, ToolStatus::Running), 30);
        let rows = body(text(&lines_at(c, &Theme::no_color(), 100, 4)));
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
        let rows = body(text(&lines_at(c, &Theme::no_color(), 100, 4)));
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(rows[1].ends_with("test case_1 ... ok"), "{rows:?}");
    }

    /// Settling is one transition: the tail leaves and the row drops from the
    /// live ink to the settled one, in the same logical block.
    #[test]
    fn settling_collapses_the_tail_and_the_ink_together() {
        let theme = Theme::dark();
        let running = with_output(cmd(SED, ToolStatus::Running), 12);
        let live = body_lines(lines_at(running.clone(), &theme, 100, 4));
        assert!(live.len() > 1);
        assert_eq!(fg_of(&live, "sed -n"), Some(theme.ink(Ink::Active)));

        let mut done = running;
        done.status = ToolStatus::Ok;
        done.duration_ms = Some(4_100);
        let settled = body_lines(lines_at(done, &theme, 100, 4));
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
            for row in body(text(&lines_at(c.clone(), &Theme::no_color(), width, 4))) {
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

    /// The Full Diff contract: a confirmed edit's diff is NEVER abbreviated.
    /// Every change row is painted, whatever the group's fold, and no
    /// `… +N lines` marker ever replaces a hunk.
    #[test]
    fn a_large_diff_is_painted_in_full() {
        let rows = rows(edit(200));
        assert!(rows.iter().any(|r| r.contains("+ line 0")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("+ line 199")), "{rows:?}");
        assert!(
            !rows.last().unwrap().contains("还有"),
            "no preview marker: {:?}",
            rows.last()
        );

        // The same complete diff, whatever the group's own fold.
        let opened = text(&render_group(
            &ToolGroupBlock {
                calls: vec![edit(200)],
                open: false,
                display: crate::fold::DisplayMode::Expanded,
                round: None,
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
        let lines = body_lines(lines_at(c, &theme, 100, 0));
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

    // ── Full Diff contract ───────────────────────────────────────────────

    /// DIFF-3 / DIFF-4: every changed line of a confirmed edit is painted at
    /// every width. There is no preview row cap and no hunk is dropped.
    #[test]
    fn every_changed_line_is_painted_at_any_width() {
        for width in [24usize, 40, 80, 200] {
            let rows = body(text(&lines_at(edit(40), &Theme::no_color(), width, 0)));
            for i in 0..40 {
                assert!(
                    rows.iter().any(|r| r.contains(&format!("+ line {i}"))),
                    "width {width} dropped change line {i}"
                );
            }
        }
    }

    /// DIFF-6: no diffstat-only substitution. The collapsed edit node paints
    /// the complete patch, not a count of the lines it hid.
    #[test]
    fn a_collapsed_edit_never_replaces_the_diff_with_a_count() {
        let rows = body(text(&lines_at(edit(120), &Theme::no_color(), 100, 0)));
        assert!(rows.iter().any(|r| r.contains("+ line 119")), "{rows:?}");
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("还有") || r.contains("more lines")),
            "a preview marker replaced diff content: {rows:?}"
        );
    }

    /// DIFF-7: an edit is never an exploration participant, so an eager
    /// exploration fold can never swallow its diff.
    #[test]
    fn an_edit_never_joins_an_exploration_fold() {
        let edit = edit(3);
        assert!(is_edit_call(&edit));
        assert!(!is_exploration_call(&edit));
        assert!(!folds_into_exploration(&edit));
    }
}
