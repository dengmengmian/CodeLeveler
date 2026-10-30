//! Conversation build: transcript → wrapped display lines
//! plus the disclosure hit rows, memoized under one cache key.
//!
//! The hit rows are built in the same pass and cached in the same entry as
//! the lines — a hit can never describe stale lines. This invariant is what
//! makes mouse disclosure clicks trustworthy; do not split the caches.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::render::{item_render, items_need_gap, sub_agent_tree_lines};
use crate::state::AppState;
use crate::transcript::TranscriptItem;

use super::ConvKey;

/// How many lines the conversation content needs at `width` (for scroll math).
pub fn conversation_line_count(state: &AppState, width: usize) -> usize {
    state.conversation_lines(width).len()
}

impl AppState {
    /// Cache-aware conversation lines: re-wraps the whole transcript only when a
    /// render input changed; otherwise returns the previously built lines (an
    /// `Rc` clone, O(1)). The empty/splash case is not cached — the splash reads
    /// repo/branch, which the transcript `version` does not track.
    pub(crate) fn conversation_lines(&self, width: usize) -> std::rc::Rc<Vec<Line<'static>>> {
        self.conversation_lines_and_hits(width).0
    }

    /// Cache-aware lines plus the disclosure hit rows built alongside them.
    /// One cache entry carries both so a hit can never describe stale lines.
    pub(crate) fn conversation_lines_and_hits(
        &self,
        width: usize,
    ) -> (
        std::rc::Rc<Vec<Line<'static>>>,
        std::rc::Rc<Vec<(usize, usize)>>,
    ) {
        let (lines, hits, _, _, _, _, _) = self.conversation_build(width);
        (lines, hits)
    }

    /// The absolute line where the last Final answer begins, for jump-to-final
    /// navigation. Reuses the same memoized build as painting, so the anchor
    /// can never describe different lines than the ones on screen.
    pub(crate) fn final_answer_anchor(&self, width: usize) -> Option<usize> {
        self.conversation_build(width).3
    }

    /// The absolute line span `[start, end)` of the transcript item at
    /// `index`, for fold anchoring. Reuses the same memoized build as painting,
    /// so a fold can never anchor against lines the reader is not looking at.
    pub fn item_span(&self, index: usize, width: usize) -> Option<(usize, usize)> {
        self.conversation_build(width).4.get(index).copied()
    }

    /// The first absolute line of the transcript item at `index`.
    pub fn item_start_line(&self, index: usize, width: usize) -> Option<usize> {
        self.item_span(index, width).map(|(start, _)| start)
    }

    /// The transcript item painted at absolute conversation line `line`, from
    /// the same memoized layout the viewport draws. A pointer gesture can only
    /// ever describe a record that is actually on screen.
    pub fn item_at_line(&self, line: usize, width: usize) -> Option<usize> {
        let (_, _, _, _, spans, _, _) = self.conversation_build(width);
        spans
            .iter()
            .enumerate()
            .find(|(_, (start, end))| (*start..*end).contains(&line))
            .map(|(index, _)| index)
    }

    /// The copy projection for the current conversation build: exact screen
    /// text plus the renderer-owned copy policy. Both come from the same build
    /// the viewport paints, so a selection can never describe a different
    /// layout than the one on screen.
    pub(crate) fn conversation_plain(
        &self,
        width: usize,
    ) -> (Vec<String>, Vec<crate::selection::PlainRow>) {
        let (lines, _, _, _, _, _, copy) = self.conversation_build(width);
        (
            lines.iter().map(crate::selection::line_to_plain).collect(),
            copy.as_ref().clone(),
        )
    }

    /// The memoized build: lines, disclosure hit rows, command rows, the
    /// last Final answer's start line, every item's line span, painted source
    /// ranges, and the copy policy aligned with the lines.
    #[allow(clippy::type_complexity)]
    pub(crate) fn conversation_build(
        &self,
        width: usize,
    ) -> (
        std::rc::Rc<Vec<Line<'static>>>,
        std::rc::Rc<Vec<(usize, usize)>>,
        std::rc::Rc<Vec<super::view::CommandHit>>,
        Option<usize>,
        std::rc::Rc<Vec<(usize, usize)>>,
        std::rc::Rc<Vec<super::view::SourceSpan>>,
        std::rc::Rc<Vec<crate::selection::PlainRow>>,
    ) {
        if crate::splash::conversation_is_empty(self) {
            let (lines, hits, commands, anchor, starts, sources, copy) =
                build_conversation(self, width);
            return (
                std::rc::Rc::new(lines),
                std::rc::Rc::new(hits),
                std::rc::Rc::new(commands),
                anchor,
                std::rc::Rc::new(starts),
                std::rc::Rc::new(sources),
                std::rc::Rc::new(copy),
            );
        }
        let key = ConvKey {
            version: self.transcript.version(),
            width,
            theme_id: self.theme.id,
            monochrome: self.theme.monochrome,
            locale: self.locale,
            tools_expanded: self.tools_expanded,
            awaiting_approval: self.approval_gated_call().cloned(),
            elapsed_secs: self.elapsed_secs,
            fold_version: self.conv.fold_version,
            focused_command: self.focused_command().cloned(),
            focused_user_shell: self.focused_user_shell_id().cloned(),
            shell_elapsed_secs: self
                .running_user_shell()
                .map(|shell| {
                    ((self.motion_clock.as_millis() as i64 - shell.started_motion_ms).max(0) / 1000)
                        as u64
                })
                .unwrap_or(0),
            question_card_open: self.question_card_open(),
        };
        if let Some((k, lines, hits, commands, anchor, starts, sources, copy)) =
            self.conv.cache.borrow().as_ref()
            && *k == key
        {
            crate::profile::add("tui.build_cache_hit_count", 1);
            return (
                lines.clone(),
                hits.clone(),
                commands.clone(),
                *anchor,
                starts.clone(),
                sources.clone(),
                copy.clone(),
            );
        }
        let started = crate::profile::start();
        let (lines, hits, commands, anchor, starts, sources, copy) =
            build_conversation(self, width);
        crate::profile::stop(started, "tui.projection_ms");
        crate::profile::add("tui.build_count", 1);
        let (lines, hits, commands, starts, sources, copy) = (
            std::rc::Rc::new(lines),
            std::rc::Rc::new(hits),
            std::rc::Rc::new(commands),
            std::rc::Rc::new(starts),
            std::rc::Rc::new(sources),
            std::rc::Rc::new(copy),
        );
        *self.conv.cache.borrow_mut() = Some((
            key,
            lines.clone(),
            hits.clone(),
            commands.clone(),
            anchor,
            starts.clone(),
            sources.clone(),
            copy.clone(),
        ));
        (lines, hits, commands, anchor, starts, sources, copy)
    }

    /// The command row under content (`abs_line`, display `col`), if any.
    pub(crate) fn command_hit_at(
        &self,
        width: usize,
        abs_line: usize,
    ) -> Option<super::view::CommandHit> {
        let (_, _, commands, _, _, _, _) = self.conversation_build(width);
        commands.iter().find(|hit| hit.line == abs_line).copied()
    }

    /// The transcript item behind the disclosure row at `abs_line`, if any.
    pub(crate) fn disclosure_item_at(&self, width: usize, abs_line: usize) -> Option<usize> {
        let (_, hits) = self.conversation_lines_and_hits(width);
        hits.iter()
            .find(|(line, _)| *line == abs_line)
            .map(|(_, item)| *item)
    }
}

#[cfg(test)]
pub fn build_conversation_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    build_conversation_lines_with_hits(state, width).0
}

/// Build the conversation plus its disclosure hit rows: for every finished,
/// non-edit tool group the FIRST emitted line is the clickable `▸/▾` header.
pub fn build_conversation_lines_with_hits(
    state: &AppState,
    width: usize,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let (lines, hits, _, _, _, _, _) = build_conversation(state, width);
    (lines, hits)
}

/// Build the conversation, its disclosure hit rows, and its command rows in
/// one pass, so no click target can describe lines other than these.
///
/// Immutable transcript items are wrapped through a per-item memo
/// ([`super::view::ItemLineCache`]): a streaming delta rebuilds only the one
/// item whose text grew, while a long finalized history is reused verbatim.
#[allow(clippy::type_complexity)]
fn build_conversation(
    state: &AppState,
    width: usize,
) -> (
    Vec<Line<'static>>,
    Vec<(usize, usize)>,
    Vec<super::view::CommandHit>,
    Option<usize>,
    Vec<(usize, usize)>,
    Vec<super::view::SourceSpan>,
    Vec<crate::selection::PlainRow>,
) {
    let theme = &state.theme;
    let t = state.t();
    let mut out: Vec<Line<'static>> = Vec::new();
    // Semantic copy policy, aligned 1:1 with `out`.
    let mut copy: Vec<crate::selection::PlainRow> = Vec::new();
    let mut hits: Vec<(usize, usize)> = Vec::new();
    let mut commands: Vec<super::view::CommandHit> = Vec::new();
    let mut sources = Vec::new();
    // Absolute line where the LAST Final answer begins. Tracked as items are
    // placed, so jump-to-final consumes the same projection painting does.
    let mut final_anchor: Option<usize> = None;
    // The first absolute line of each transcript item, for fold anchoring: a
    // fold preserves the toggled entry's position on screen by shifting the
    // viewport by the same amount the entry moved.
    let mut item_spans: Vec<(usize, usize)> = vec![(0, 0); state.transcript.items().len()];

    // Empty session: brand splash (logo + tagline) instead of a blank void.
    if crate::splash::conversation_is_empty(state) {
        let height = state
            .conv
            .rect
            .map(|(_, _, _, h)| h as usize)
            .filter(|h| *h > 0)
            .unwrap_or(24);
        let splash = crate::splash::splash_lines(state, width, height, theme, t);
        let copy = vec![
            crate::selection::PlainRow {
                skip: true,
                ..Default::default()
            };
            splash.len()
        ];
        return (
            splash,
            hits,
            commands,
            final_anchor,
            item_spans,
            sources,
            copy,
        );
    }

    let env = super::view::UnitEnv {
        width,
        theme_id: state.theme.id,
        monochrome: state.theme.monochrome,
        locale: state.locale,
        tools_expanded: state.tools_expanded,
        plan_waiting: crate::workbench::plan_waiting_for_user_projection(state),
    };
    // Take the previous per-item memo so unchanged units can be moved forward
    // instead of re-wrapped. The borrow is released before any rendering runs.
    let prev = std::mem::take(&mut *state.conv.item_cache.borrow_mut());
    let mut prev_units = prev.units.into_iter();
    let mut new_units: Vec<super::view::CachedUnit> = Vec::new();

    let items = state.transcript.items();
    let folds = plan_process_folds(items);
    let mut unit_index = 0;
    while unit_index < folds.units.len() {
        let (mut idx, independent_call) = match folds.units[unit_index] {
            ProcessUnit::Receipt(run) => {
                let fold = &folds.runs[run];
                if !out.is_empty() {
                    out.push(Line::from(""));
                    copy.push(crate::selection::PlainRow::default());
                }
                let before = out.len();
                render_process_fold(
                    state,
                    fold,
                    &mut out,
                    &mut hits,
                    &mut commands,
                    &mut sources,
                    width,
                    theme,
                    t,
                );
                // The execution receipt is evidence UI, not message body.
                copy.extend(std::iter::repeat_n(
                    crate::selection::PlainRow {
                        skip: true,
                        ..Default::default()
                    },
                    out.len() - before,
                ));
                for &(item_index, _) in &fold.members {
                    let old = item_spans[item_index];
                    item_spans[item_index] = if old == (0, 0) {
                        (before, out.len())
                    } else {
                        (old.0.min(before), old.1.max(out.len()))
                    };
                }
                unit_index += 1;
                continue;
            }
            ProcessUnit::Item(index) => (index, None),
            ProcessUnit::IndependentCalls { item, start, end } => (item, Some((start, end))),
        };
        // A derived result unit borrows its real call and preserves original identity.
        let isolated;
        let item = if let Some((start, end)) = independent_call {
            let TranscriptItem::ToolGroup(group) = &items[idx] else {
                unreachable!()
            };
            let one = crate::transcript::ToolGroupBlock {
                calls: group.calls[start..end].to_vec(),
                open: group.open,
                display: group.display,
                round: group.round,
            };
            isolated = TranscriptItem::ToolGroup(one);
            &isolated
        } else {
            &items[idx]
        };
        // Remember where this item starts: a group whose tools are all Silent
        // (ls / probe runs) renders nothing, and a separator emitted before it
        // would leave a blank gap with no content — the reader sees a hole
        // between their prompt and the answer. The gap is undone below if the
        // item turned out to be invisible.
        let gap_at =
            (idx > 0 && !out.is_empty() && items_need_gap(&items[idx - 1], item)).then(|| {
                out.push(Line::from(""));
                copy.push(crate::selection::PlainRow::default());
                out.len() - 1
            });
        let first_item = idx;
        let before_item = out.len();
        if let TranscriptItem::Assistant(block) = item
            && block.kind == crate::transcript::AssistantKind::Final
        {
            final_anchor = Some(out.len());
        }
        // Message types are distinguished by shape, not role headings:
        // the `❯` prompt band, `●` agent prose, status glyphs for tool
        // activity. The mark itself is painted by the viewport.
        //
        let cacheable = memoizable_item(item, state);
        // Observation for the streaming-frame cost: how much of a long
        // history is reused, how much is re-wrapped, and how many units are
        // re-rendered every frame because they read live state. Timed with the
        // existing opt-in profiler (`LEVELER_TUI_PROFILE=1`), so a real session
        // can show whether the live set is actually the problem before any
        // cache boundary moves.
        crate::profile::add(
            if cacheable {
                "tui.unit_memo_candidate_count"
            } else {
                "tui.unit_live_count"
            },
            1,
        );
        if cacheable {
            let reused = prev_units.next();
            let (lines, unit_hits, unit_commands, unit_copy, unit_items) = match reused {
                Some(unit)
                    if unit.env == env && unit.items.len() == 1 && &unit.items[0] == item =>
                {
                    crate::profile::add("tui.unit_reused_count", 1);
                    (unit.lines, unit.hits, unit.commands, unit.copy, unit.items)
                }
                _ => {
                    crate::profile::add("tui.unit_rewrapped_count", 1);
                    let (lines, unit_hits, unit_commands, unit_copy) =
                        render_cacheable_unit(state, item, width);
                    (
                        std::rc::Rc::new(lines),
                        unit_hits,
                        unit_commands,
                        unit_copy,
                        vec![item.clone()],
                    )
                }
            };
            for (rel, k) in &unit_hits {
                hits.push((out.len() + rel, idx + k));
            }
            for c in &unit_commands {
                commands.push(super::view::CommandHit {
                    receipt: false,
                    line: out.len() + c.line,
                    item: idx + c.item_offset,
                    call: c.call,
                    stoppable: c.stoppable,
                });
            }
            out.extend(lines.iter().cloned());
            copy.extend(unit_copy.iter().cloned());
            new_units.push(super::view::CachedUnit {
                items: unit_items,
                env,
                lines,
                hits: unit_hits,
                commands: unit_commands,
                copy: unit_copy,
            });
        } else {
            match item {
                TranscriptItem::ToolGroup(group) => {
                    // Product activity stream — not a raw tool trace:
                    // Silent (ls/list_files/probes) stay out; Normal exploration
                    // aggregates; Important edits/runs stay one bold line each.
                    // A finished, non-edit group's first line is its disclosure
                    // row — record it as a click target for this exact item.
                    if crate::activity_stream::group_has_disclosure(group) {
                        hits.push((out.len(), idx));
                    }
                    let base = out.len();
                    let mut rows = Vec::new();
                    let mut group_copy = Vec::new();
                    out.extend(crate::activity_stream::render_activity(
                        group,
                        theme,
                        width,
                        state.locale,
                        t,
                        state.elapsed_secs,
                        state.approval_gated_call(),
                        state.focused_command(),
                        &mut rows,
                        Some(&state.conv.process_folds.borrow()),
                        state.question_card_open(),
                        &mut group_copy,
                    ));
                    for row in rows {
                        commands.push(super::view::CommandHit {
                            receipt: row.disclosure,
                            line: base + row.line,
                            item: idx,
                            call: independent_call.map_or(row.call, |(start, _)| start + row.call),
                            stoppable: row.stoppable,
                        });
                    }
                    // Tool execution is evidence UI, not message body — with
                    // one exception the renderer names itself: an expanded
                    // edit's diff rows are the patch's own source lines.
                    copy.extend(std::iter::repeat_n(
                        crate::selection::PlainRow {
                            skip: true,
                            ..Default::default()
                        },
                        out.len() - base,
                    ));
                    crate::selection::apply_copy_rows(&mut copy[base..], &group_copy);
                }
                TranscriptItem::SubAgent(first) => {
                    // A run of consecutive sub-agent blocks renders as one tree
                    // (aggregate header + ├─/└─ children). Any other item breaks
                    // the run — batches split by tool calls stay separate.
                    let mut blocks = vec![first];
                    while let Some(TranscriptItem::SubAgent(next)) = items.get(idx + 1) {
                        blocks.push(next);
                        idx += 1;
                    }
                    let base = out.len();
                    out.extend(sub_agent_tree_lines(
                        &blocks,
                        theme,
                        width,
                        t,
                        state.elapsed_secs,
                    ));
                    copy.extend(std::iter::repeat_n(
                        crate::selection::PlainRow {
                            skip: true,
                            ..Default::default()
                        },
                        out.len() - base,
                    ));
                }
                TranscriptItem::UserShell(shell) => {
                    // Every user shell row is a click target: click folds its
                    // output inline (running or finished) — the same hit map
                    // every other disclosure uses, geometry stays untouched.
                    hits.push((out.len(), idx));
                    let (lines, rows) = crate::render::user_shell_lines_with_copy(
                        shell,
                        theme,
                        width,
                        t,
                        state.motion_clock,
                        state.focused_user_shell_id() == Some(&shell.id),
                    );
                    out.extend(lines);
                    copy.extend(rows);
                }
                _ => unreachable!("non-cacheable item handled above"),
            }
        }
        // The item produced nothing (an all-Silent tool group): take the
        // separator back so no hole is left behind.
        let invisible = out.len() == before_item;
        if invisible && let Some(at) = gap_at {
            out.remove(at);
            copy.remove(at);
        }
        if !invisible {
            let call = independent_call.and_then(|(start, _)| match &items[first_item] {
                TranscriptItem::ToolGroup(group) => group.calls.get(start).map(|c| c.id.clone()),
                _ => None,
            });
            sources.push(super::view::SourceSpan {
                receipt: false,
                item: first_item,
                call,
                start: before_item,
                end: out.len(),
            });
        }
        // One span per consumed item. A sub-agent run (or any future multi-item
        // unit) renders several items as one block, so they all share it.
        for span in &mut item_spans[first_item..=idx] {
            let next = (if invisible { out.len() } else { before_item }, out.len());
            *span = if *span == (0, 0) {
                next
            } else {
                (span.0.min(next.0), span.1.max(next.1))
            };
        }
        unit_index += 1;
        while matches!(folds.units.get(unit_index), Some(ProcessUnit::Item(index)) if *index <= idx)
        {
            unit_index += 1;
        }
    }

    *state.conv.item_cache.borrow_mut() = super::view::ItemLineCache { units: new_units };
    debug_assert_eq!(
        copy.len(),
        out.len(),
        "the copy policy must be aligned 1:1 with painted lines"
    );
    (out, hits, commands, final_anchor, item_spans, sources, copy)
}

// ───────────────────────────────────────────────────────────────────────────
// View-time process folds
// ───────────────────────────────────────────────────────────────────────────
//
// A provider that reasons between tool calls makes the transcript alternate
// `Thought / ToolGroup / Thought / ToolGroup`, which paints a wall of `◆`
// headers over one-row reads. The fix is PRESENTATION ONLY: a chronological run
// of ordinary work call ranges is painted as one receipt until the reader
// opens it. Visible independent results and narration delimit those ranges. Nothing is
// merged or reordered — the same semantic items stay in the transcript, and
// opening the fold restores them in their real order.
//
// A closed parent hides every child while preserving each child's own display
// selection. Opening it reveals child headers, never opens their bodies.

/// One visible unit derived from authoritative transcript items and call ranges.
#[derive(Debug, Clone, Copy)]
enum ProcessUnit {
    Item(usize),
    IndependentCalls {
        item: usize,
        start: usize,
        end: usize,
    },
    Receipt(usize),
}

#[derive(Debug)]
struct ProcessFold {
    anchor: usize,
    anchor_call: usize,
    /// Original transcript indices; a mixed group can contribute to two runs.
    members: Vec<(usize, usize)>,
}

#[derive(Debug, Default)]
struct ProcessFolds {
    /// Latest run touching each item, used only for the keyboard's latest-group target.
    owner: Vec<Option<usize>>,
    runs: Vec<ProcessFold>,
    units: Vec<ProcessUnit>,
}

/// Derive chronological receipts across group boundaries. Independent results
/// split call ranges, while reasoning and invisible bookkeeping remain transparent.
fn plan_process_folds(items: &[TranscriptItem]) -> ProcessFolds {
    let mut plan = ProcessFolds {
        owner: vec![None; items.len()],
        ..Default::default()
    };
    let mut current = None;
    for (item_index, item) in items.iter().enumerate() {
        if let TranscriptItem::ToolGroup(group) = item {
            let mut visible = false;
            for (call_index, call) in group.calls.iter().enumerate() {
                if !crate::activity_stream::is_conversation_visible(call) {
                    continue;
                }
                visible = true;
                if crate::activity_stream::is_process_fold_call(call) {
                    let run = *current.get_or_insert_with(|| {
                        let run = plan.runs.len();
                        plan.runs.push(ProcessFold {
                            anchor: item_index,
                            anchor_call: call_index,
                            members: Vec::new(),
                        });
                        plan.units.push(ProcessUnit::Receipt(run));
                        run
                    });
                    plan.runs[run].members.push((item_index, call_index));
                    plan.owner[item_index] = Some(run);
                } else {
                    current = None;
                    if let Some(ProcessUnit::IndependentCalls { item, end, .. }) =
                        plan.units.last_mut()
                        && *item == item_index
                    {
                        *end = call_index + 1;
                    } else {
                        plan.units.push(ProcessUnit::IndependentCalls {
                            item: item_index,
                            start: call_index,
                            end: call_index + 1,
                        });
                    }
                }
            }
            if !visible {
                plan.units.push(ProcessUnit::Item(item_index));
            }
        } else {
            let transparent = matches!(item, TranscriptItem::Thought(_))
                || matches!(item, TranscriptItem::Assistant(block) if block.kind != crate::transcript::AssistantKind::Final && block.text.trim().is_empty());
            if !transparent {
                current = None;
            }
            plan.units.push(ProcessUnit::Item(item_index));
        }
    }
    plan
}

/// The stable identity of a run: its anchor group's first exploration call.
fn fold_anchor_key(
    items: &[TranscriptItem],
    fold: &ProcessFold,
) -> Option<leveler_client_protocol::ToolCallId> {
    let TranscriptItem::ToolGroup(group) = items.get(fold.anchor)? else {
        return None;
    };
    group
        .calls
        .get(fold.anchor_call)
        .map(|call| call.id.clone())
}

/// Whether a view-time exploration fold is open. Presentation-only state kept
/// on the conversation view, keyed by the run's anchor call id.
impl crate::conversation::ConversationView {
    pub(crate) fn process_fold_expanded(&self, key: &leveler_client_protocol::ToolCallId) -> bool {
        self.process_folds.borrow().contains(key)
    }
}

/// Resolve the latest execution (tool group or user command) to the receipt
/// currently folding it, when it is part of a process run.
/// Mouse clicks keep their exact item, while the keyboard must open the
/// containing process run instead of toggling a hidden member.
pub(crate) fn keyboard_expand_target(state: &AppState) -> Option<usize> {
    let latest = state.transcript.last_execution_index()?;
    let plan = plan_process_folds(state.transcript.items());
    Some(
        plan.owner[latest]
            .map(|run| plan.runs[run].anchor)
            .unwrap_or(latest),
    )
}

/// The transcript items whose own exploration details Ctrl+O reveals: every
/// member group of the run the keyboard targets, or the targeted group itself.
/// The receipt is the fold; the bodies under it belong to the calls, so asking
/// for the latest command's details asks for the whole run's.
pub(crate) fn keyboard_detail_calls(state: &AppState, target: usize) -> Vec<(usize, usize)> {
    let items = state.transcript.items();
    let plan = plan_process_folds(items);
    match plan.owner.get(target).copied().flatten() {
        Some(run) => plan.runs[run].members.clone(),
        None => Vec::new(),
    }
}

/// The edit calls in one transcript item, in paint order — but only when the
/// item is an edit-ONLY node.
///
/// A group that also holds commands keeps `Ctrl+O`'s existing meaning (open
/// the process receipt). Reserving the key for the edit in such a group would
/// silently stop opening the commands beside it.
fn edit_calls_at(state: &AppState, item: usize) -> Vec<usize> {
    let Some(TranscriptItem::ToolGroup(group)) = state.transcript.items().get(item) else {
        return Vec::new();
    };
    let visible: Vec<(usize, &crate::transcript::ToolCallBlock)> = group
        .calls
        .iter()
        .enumerate()
        .filter(|(_, call)| crate::activity_stream::is_conversation_visible(call))
        .collect();
    if visible.is_empty()
        || !visible
            .iter()
            .all(|(_, call)| crate::activity_stream::is_edit_call(call))
    {
        return Vec::new();
    }
    visible
        .into_iter()
        .filter(|(_, call)| crate::activity_stream::is_edit_call(call))
        .map(|(index, _)| index)
        .collect()
}

/// Toggle the diff drawer of the edit-only node a keyboard fold targets.
///
/// An edit's drawer is per CALL (a merged node paints its first call's mode),
/// and it is not part of the process-run fold set: `Ctrl+O` on a settled edit
/// must fold the diff, not the exploration receipt above it. Returns the new
/// expanded state, or `None` when the item holds no edit.
pub(crate) fn toggle_edit_fold(state: &mut AppState, item: usize) -> Option<bool> {
    let calls = edit_calls_at(state, item);
    let first = *calls.first()?;
    let TranscriptItem::ToolGroup(group) = state.transcript.items().get(item)? else {
        return None;
    };
    let expanded = !group.calls.get(first)?.expanded;
    for call in calls {
        state.transcript.set_call_expanded(item, call, expanded);
    }
    Some(expanded)
}

/// Explicit command focus reveals its containing receipt without changing call drawers.
pub(crate) fn reveal_command_receipt(state: &mut AppState, item: usize, call: usize) {
    let plan = plan_process_folds(state.transcript.items());
    let key = plan
        .runs
        .iter()
        .find(|fold| fold.members.contains(&(item, call)))
        .and_then(|fold| fold_anchor_key(state.transcript.items(), fold));
    if let Some(key) = key
        && state.conv.process_folds.borrow_mut().insert(key)
    {
        state.conv.fold_version = state.conv.fold_version.wrapping_add(1);
        state.conv.plain.clear();
        state.conv.plain_layout.clear();
        state.conv.plain_width = 0;
    }
}

/// Toggle one derived span using the same first-call identity that painted it.
pub(crate) fn toggle_span_receipt(state: &mut AppState, item: usize, call: usize) -> Option<bool> {
    let reading = super::interaction::reading_anchor(state);
    let TranscriptItem::ToolGroup(group) = state.transcript.items().get(item)? else {
        return None;
    };
    let key = group.calls.get(call)?.id.clone();
    let expanded = {
        let mut folds = state.conv.process_folds.borrow_mut();
        if folds.remove(&key) {
            false
        } else {
            folds.insert(key);
            true
        }
    };
    state.conv.fold_version = state.conv.fold_version.wrapping_add(1);
    state.conv.plain.clear();
    state.conv.plain_layout.clear();
    state.conv.plain_width = 0;
    super::interaction::restore_reading_anchor(state, reading);
    Some(expanded)
}

/// Toggle the view-time exploration fold whose receipt is `item`, if any.
/// Returns the new expanded state, or `None` when the item is not a run's
/// anchor.
///
/// Only the run's ANCHOR (its first work group) opens or closes the
/// fold. A member row inside an open fold keeps its own fold: a
/// click there must reveal tool details, never collapse the run around it.
///
/// Kept beside the planner so the run the reader clicked is resolved by the
/// same derivation that painted it — the hit can never describe another run.
pub(crate) fn toggle_process_fold(state: &mut AppState, item: usize) -> Option<bool> {
    let items = state.transcript.items();
    let plan = plan_process_folds(items);
    let run = plan.owner.get(item).copied().flatten()?;
    let fold = plan.runs.get(run)?;
    if fold.anchor != item {
        return None;
    }
    let key = fold_anchor_key(items, fold)?;
    let expanded = {
        let mut folds = state.conv.process_folds.borrow_mut();
        if folds.remove(&key) {
            false
        } else {
            folds.insert(key);
            true
        }
    };
    state.conv.fold_version = state.conv.fold_version.wrapping_add(1);
    // The plain-text projection backs selection and URL hit-testing; the fold
    // changed the lines under it, so it must be rebuilt on next use.
    state.conv.plain.clear();
    state.conv.plain_layout.clear();
    state.conv.plain_width = 0;
    Some(expanded)
}

/// Paint one view-time process run as a unit: the aggregate receipt, then
/// the members the current fold state reveals.
#[allow(clippy::too_many_arguments)]
fn render_process_fold(
    state: &AppState,
    fold: &ProcessFold,
    out: &mut Vec<Line<'static>>,
    hits: &mut Vec<(usize, usize)>,
    commands: &mut Vec<super::view::CommandHit>,
    sources: &mut Vec<super::view::SourceSpan>,
    width: usize,
    theme: &crate::theme::Theme,
    t: &crate::i18n::UiText,
) {
    let items = state.transcript.items();
    let expanded =
        fold_anchor_key(items, fold).is_some_and(|key| state.conv.process_fold_expanded(&key));
    let members: Vec<&crate::transcript::ToolCallBlock> = fold
        .members
        .iter()
        .filter_map(|(i, c)| match items.get(*i) {
            Some(TranscriptItem::ToolGroup(group)) => group.calls.get(*c),
            _ => None,
        })
        .collect();
    // An item-only disclosure is unambiguous only when this run owns the whole group.
    let group_is_mixed = matches!(items.get(fold.anchor), Some(TranscriptItem::ToolGroup(group))
        if group.calls.iter().any(|c| crate::activity_stream::is_conversation_visible(c) && !crate::activity_stream::is_process_fold_call(c)));
    if group_is_mixed {
        commands.push(super::view::CommandHit {
            receipt: true,
            line: out.len(),
            item: fold.anchor,
            call: fold.anchor_call,
            stoppable: false,
        });
    } else {
        hits.push((out.len(), fold.anchor));
    }
    out.push(crate::activity_stream::process_fold_receipt_line(
        &members,
        theme,
        width,
        t,
        expanded,
        state.elapsed_secs,
    ));
    sources.push(super::view::SourceSpan {
        receipt: true,
        item: fold.anchor,
        call: fold_anchor_key(items, fold),
        start: out.len() - 1,
        end: out.len(),
    });
    let body_start = out.len();
    let child_width = width
        .saturating_sub(crate::activity_stream::GROUP_BODY_INDENT.len())
        .max(1);
    if expanded {
        for &(item_index, call_index) in &fold.members {
            let Some(TranscriptItem::ToolGroup(group)) = items.get(item_index) else {
                continue;
            };
            let Some(call) = group.calls.get(call_index) else {
                continue;
            };
            let source_start = out.len();
            commands.push(super::view::CommandHit {
                receipt: false,
                line: out.len(),
                item: item_index,
                call: call_index,
                stoppable: crate::activity_stream::process_call_stoppable(
                    call,
                    theme,
                    t,
                    state.elapsed_secs,
                    state.approval_gated_call(),
                ),
            });
            out.extend(crate::activity_stream::process_member_lines_focused(
                call,
                theme,
                child_width,
                state.locale,
                t,
                state.elapsed_secs,
                state.approval_gated_call(),
                state.focused_command(),
            ));
            sources.push(super::view::SourceSpan {
                receipt: false,
                item: item_index,
                call: Some(call.id.clone()),
                start: source_start,
                end: out.len(),
            });
        }
    }
    // Children step in one level under the receipt, exactly like every other
    // fold's body. Width was reserved above, so this only paints.
    for line in out.iter_mut().skip(body_start) {
        line.spans.insert(
            0,
            Span::styled(
                crate::activity_stream::GROUP_BODY_INDENT.to_string(),
                Style::default().fg(theme.ink(crate::theme::Ink::Subtle)),
            ),
        );
    }
}

/// Render one memoized transcript item to lines, plus its disclosure hit rows
/// and command rows in item-relative coordinates (always empty for these item
/// types today, but the shape is the same as a fresh render).
#[allow(clippy::type_complexity)]
/// Whether a unit may be memoized: only when nothing it paints can change
/// without the transcript itself changing.
///
/// A CLOSED, FINISHED tool group reads frozen facts only — each call's status,
/// duration and preview. `now_elapsed_secs` reaches the head only for `Running`
/// calls, so a finished group's timing never moves, and an edit's diff is the
/// change itself. A group that is still open, still running, holding an
/// approval, or holding the command focus is painted fresh: those are exactly
/// the inputs the memo cannot see. A sub-agent run and a user shell read the
/// live turn clock, so they stay out of the memo as before.
fn memoizable_item(item: &TranscriptItem, state: &AppState) -> bool {
    match item {
        TranscriptItem::ToolGroup(group) => {
            crate::activity_stream::group_is_finished(group)
                && !group.open
                && !group.calls.iter().any(|call| {
                    state.approval_gated_call() == Some(&call.id)
                        || state.focused_command() == Some(&call.id)
                })
        }
        TranscriptItem::SubAgent(_) => false,
        // A finished shell is immutable: it reads neither the motion clock
        // nor a focus marker (only running executions can be focused). Its
        // full, auto-expanded output is wrapped once and then reused — without
        // this, every earlier large result is re-wrapped on every frame of the
        // next command and a 10 MiB run becomes quadratic.
        TranscriptItem::UserShell(shell) => shell.status.is_terminal(),
        _ => true,
    }
}

fn render_cacheable_unit(
    state: &AppState,
    item: &TranscriptItem,
    width: usize,
) -> (
    Vec<Line<'static>>,
    Vec<(usize, usize)>,
    Vec<super::view::CachedCommand>,
    Vec<crate::selection::PlainRow>,
) {
    use crate::render::transcript_lines::{
        assistant_render_with_copy, prefixed_copy_rows, push_user_turn, thought_lines_with_copy,
        user_copy_rows,
    };
    let theme = &state.theme;
    let t = state.t();
    let mut hits: Vec<(usize, usize)> = Vec::new();
    let mut commands: Vec<super::view::CachedCommand> = Vec::new();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut copy: Vec<crate::selection::PlainRow> = Vec::new();
    match item {
        TranscriptItem::User(text) => {
            push_user_turn(text, theme, width, &mut out);
            copy = user_copy_rows(text, width);
        }
        TranscriptItem::Assistant(block) => {
            // Prose is communication: always in full, never a click target.
            let (lines, rows) = assistant_render_with_copy(block, theme, width);
            out.extend(lines);
            copy = rows;
        }
        TranscriptItem::GoalRecap(_) => {
            // A goal recap's first line is its disclosure row: click expands the
            // persisted checkpoint's structured sections.
            hits.push((0, 0));
            out.extend(item_render(item, theme, width, state.tools_expanded, t));
        }
        TranscriptItem::Thought(block) => {
            // A live Thought is the reasoning arriving: it paints in place at the
            // tail while the model works. Once it settles it leaves no
            // conversation row — the transcript keeps it, the conversation does
            // not, so a finished turn reads as its work and its answer.
            if !block.done {
                let (lines, rows) = thought_lines_with_copy(block, theme, width, t);
                if !lines.is_empty() && !block.text.trim().is_empty() {
                    hits.push((0, 0));
                }
                out.extend(lines);
                copy = rows;
            }
        }
        TranscriptItem::Failure(_) => {
            hits.push((0, 0));
            out.extend(item_render(item, theme, width, state.tools_expanded, t));
        }
        TranscriptItem::Error(text) => {
            out.extend(item_render(item, theme, width, state.tools_expanded, t));
            copy = prefixed_copy_rows("✗ ", text, width);
        }
        TranscriptItem::Note(text) => {
            out.extend(item_render(item, theme, width, state.tools_expanded, t));
            copy = prefixed_copy_rows("◆ ", text, width);
        }
        TranscriptItem::UserShell(shell) => {
            // The header stays the inline fold/open target.
            hits.push((0, 0));
            let (lines, rows) = crate::render::user_shell_lines_with_copy(
                shell,
                theme,
                width,
                t,
                state.motion_clock,
                state.focused_user_shell_id() == Some(&shell.id),
            );
            out.extend(lines);
            copy = rows;
        }
        TranscriptItem::ToolGroup(group) => {
            // A SETTLED group still owns click targets: an edit's diff folds
            // from its own head row, and a historical fold must work exactly
            // like a live one. The memoized path therefore keeps the command
            // rows `render_activity` produced, instead of dropping them.
            let mut rows = Vec::new();
            let mut group_copy = Vec::new();
            let base = out.len();
            out.extend(crate::activity_stream::render_activity(
                group,
                theme,
                width,
                state.locale,
                t,
                0,
                None,
                None,
                &mut rows,
                None,
                false,
                &mut group_copy,
            ));
            for row in rows {
                commands.push(super::view::CachedCommand {
                    line: row.line,
                    item_offset: 0,
                    call: row.call,
                    stoppable: row.stoppable,
                });
            }
            copy.extend(std::iter::repeat_n(
                crate::selection::PlainRow {
                    skip: true,
                    ..Default::default()
                },
                out.len() - base,
            ));
            crate::selection::apply_copy_rows(&mut copy[base..], &group_copy);
        }
        TranscriptItem::Plan(plan) => {
            let waiting_for_user = crate::workbench::plan_waiting_for_user_projection(state);
            crate::render::transcript_lines::historical_plan_lines_with_context(
                plan,
                theme,
                width,
                &mut out,
                t,
                waiting_for_user,
            );
        }
        other => out.extend(item_render(other, theme, width, state.tools_expanded, t)),
    }
    // A surface with no semantic rows (a settled status block, a tool group,
    // a plan card) is display-only: its whole span is skipped.
    debug_assert!(
        copy.is_empty() || copy.len() == out.len(),
        "copy policy must describe every painted row"
    );
    if copy.len() != out.len() {
        copy = vec![
            crate::selection::PlainRow {
                skip: true,
                ..Default::default()
            };
            out.len()
        ];
    }
    (out, hits, commands, copy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Boot;
    use leveler_client_protocol::{MessageId, SessionId};
    use std::rc::Rc;

    fn test_state() -> AppState {
        let mut s = AppState::new(
            crate::theme::Theme::no_color(),
            Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        );
        s.size = (80, 40);
        s.conv.rect = Some((0, 2, 80, 30));
        s
    }

    /// User row + one finalized assistant turn + a live streaming message.
    fn state_with_history() -> AppState {
        let mut s = test_state();
        s.transcript.push_user("please summarize".into());
        let done = MessageId::new("done");
        s.transcript.begin_assistant(done.clone());
        s.transcript.append_assistant(
            &done,
            "## Answer\n\nA finalized `code` answer that wraps at a narrow width.",
        );
        s.transcript.finish_assistant(&done);
        let live = MessageId::new("live");
        s.transcript.begin_assistant(live.clone());
        s.transcript.append_assistant(&live, "streaming");
        s
    }

    fn first_assistant_unit(state: &AppState) -> Rc<Vec<Line<'static>>> {
        state
            .conv
            .item_cache
            .borrow()
            .units
            .iter()
            .find(|unit| matches!(unit.items.first(), Some(TranscriptItem::Assistant(_))))
            .map(|unit| unit.lines.clone())
            .expect("a finalized assistant unit is cached")
    }

    #[test]
    fn unchanged_history_is_not_rewrapped_when_the_live_message_grows() {
        let mut s = state_with_history();
        let width = 60;
        let before = build_conversation(&s, width).0;
        let prefix_before = first_assistant_unit(&s);

        // The only change between frames: the streaming tail grows.
        s.transcript
            .append_assistant(&MessageId::new("live"), " text arrives");
        let after = build_conversation(&s, width).0;
        let prefix_after = first_assistant_unit(&s);

        assert!(
            Rc::ptr_eq(&prefix_before, &prefix_after),
            "unchanged finalized history must be reused, not re-wrapped"
        );
        assert_ne!(before, after, "the new delta must still appear");
        let shared = prefix_before.len().min(prefix_after.len());
        assert_eq!(
            before[..shared],
            after[..shared],
            "prefix lines must be stable"
        );
    }

    #[test]
    fn width_and_theme_changes_invalidate_the_item_cache() {
        let s = state_with_history();
        let _ = build_conversation(&s, 60);
        let base = first_assistant_unit(&s);

        let narrow = build_conversation(&s, 40).0;
        let narrow_unit = first_assistant_unit(&s);
        assert!(
            !Rc::ptr_eq(&base, &narrow_unit),
            "a width change must re-wrap"
        );
        assert!(!narrow.is_empty());

        let mut themed = state_with_history();
        let _ = build_conversation(&themed, 60);
        let before_theme = first_assistant_unit(&themed);
        themed.theme = crate::theme::Theme::dark();
        let _ = build_conversation(&themed, 60);
        let after_theme = first_assistant_unit(&themed);
        assert!(
            !Rc::ptr_eq(&before_theme, &after_theme),
            "a theme change must re-render"
        );
    }

    /// A summarized diff keeps a fixed row budget independent of viewport
    /// height, and says how much it hides.
    /// The memo boundary for tool groups: a settled one is reusable, a running
    /// one is not (its head reads the live turn clock), and neither an approval
    /// hold nor the command focus may be memoized away.
    #[test]
    fn only_settled_groups_join_the_item_memo() {
        let mut s = test_state();
        let id = leveler_client_protocol::ToolCallId::new("t1");
        s.transcript.push_tool_started(
            id.clone(),
            "read_file".into(),
            serde_json::json!({ "path": "a.rs" }).to_string(),
            false,
            0,
            None,
            None,
        );
        let group = |s: &crate::state::AppState| match s.transcript.items().first() {
            Some(TranscriptItem::ToolGroup(g)) => g.clone(),
            other => panic!("expected a tool group, got {other:?}"),
        };
        assert!(
            !memoizable_item(&TranscriptItem::ToolGroup(group(&s)), &s),
            "a running group reads the live clock"
        );
        s.transcript
            .complete_tool(&id, true, "read".into(), 5, None);
        assert!(
            !memoizable_item(&TranscriptItem::ToolGroup(group(&s)), &s),
            "a group still taking calls is not settled"
        );
        // Sealing the run closes the group; from then on it paints frozen facts.
        s.transcript.push_note("sealed".into());
        assert!(
            memoizable_item(&TranscriptItem::ToolGroup(group(&s)), &s),
            "a settled group only paints frozen facts"
        );
    }

    #[test]
    fn a_confirmed_diff_preview_is_stable_at_any_viewport_height() {
        let mut s = test_state();
        s.transcript.push_user("edit a file".into());
        let diff = (0..40)
            .map(|i| {
                format!(
                    "@@ -{},3 +{},3 @@\n context {i}\n-old call {i}\n+new call {i}\n",
                    i * 10 + 1,
                    i * 10 + 1
                )
            })
            .collect::<Vec<_>>()
            .join("");
        let id = leveler_client_protocol::ToolCallId::new("e1");
        s.transcript.push_tool_started(
            id.clone(),
            "apply_patch".into(),
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch"
            })
            .to_string(),
            false,
            0,
            None,
            None,
        );
        s.transcript
            .complete_tool(&id, true, "patched".into(), 5, Some(diff));
        // Seal the group so the edit renders as settled history.
        s.transcript.push_note("tail".into());

        // Small viewport and tall viewport, same width.
        s.conv.rect = Some((0, 2, 80, 12));
        let short_lines = s.conversation_lines(80);
        let short = short_lines.len();
        s.conv.rect = Some((0, 2, 80, 80));
        let tall_lines = s.conversation_lines(80);
        let tall = tall_lines.len();

        assert_eq!(
            tall, short,
            "the summarized diff is independent of viewport height: {short} -> {tall}"
        );
        let plain: Vec<String> = tall_lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|sp| sp.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(
            plain.iter().any(|r| r.contains("+ new call 39")),
            "every applied row is painted: {plain:?}"
        );
        assert!(
            plain.iter().any(|r| r.contains("+ new call 20")),
            "the middle of a long diff is never folded away: {plain:?}"
        );
        assert!(
            !plain.iter().any(|r| r.contains("还有")),
            "no hidden-middle hint exists any more: {plain:?}"
        );
    }
}

/// View-time process folds: the presentation contracts the Conversation
/// IA now guarantees. These tests drive the SAME build path the UI paints.
#[cfg(test)]
mod exploration_fold_tests {
    use super::*;
    use leveler_client_protocol::{MessageId, SessionId, ToolCallId};

    fn boot() -> AppState {
        let mut s = AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
                session_id: SessionId::new("fold"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        );
        s.size = (80, 40);
        s.conv.rect = Some((0, 2, 80, 30));
        s.transcript.push_user("find the bug".into());
        s
    }

    fn tool(s: &mut AppState, id: &str, name: &str, args: serde_json::Value, step: u32, ok: bool) {
        s.transcript.push_tool_started(
            ToolCallId::new(id),
            name.into(),
            args.to_string(),
            false,
            0,
            Some(step),
            None,
        );
        let preview = "alpha\nbeta\n";
        let diff =
            (name == "apply_patch").then(|| "@@ -1,1 +1,2 @@\n context\n+added\n".to_string());
        s.transcript
            .complete_tool(&ToolCallId::new(id), ok, preview.into(), 10, diff);
    }

    fn read(s: &mut AppState, id: &str, path: &str, step: u32) {
        tool(
            s,
            id,
            "read_file",
            serde_json::json!({ "path": path }),
            step,
            true,
        );
    }

    fn search(s: &mut AppState, id: &str, pattern: &str, step: u32) {
        tool(
            s,
            id,
            "grep",
            serde_json::json!({ "pattern": pattern }),
            step,
            true,
        );
    }

    fn thought(s: &mut AppState, text: &str) {
        s.transcript.begin_thought();
        s.transcript.append_thought(text);
        s.transcript.finish_thought(Some(800));
    }

    fn narration(s: &mut AppState, text: &str) {
        let id = MessageId::new("n");
        s.transcript.begin_assistant(id.clone());
        s.transcript.append_assistant(&id, text);
        s.transcript.finish_assistant(&id);
    }

    /// Close the trailing group. A group the model may still extend is not
    /// history yet, so a fixture that wants settled runs must close it.
    fn seal(s: &mut AppState) {
        s.transcript.push_note("sealed".into());
    }

    fn render(s: &AppState) -> Vec<String> {
        s.conversation_lines(80)
            .iter()
            .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn fixed_contract_opening_a_receipt_preserves_the_diff_reading_row() {
        let mut s = boot();
        read(&mut s, "r", "before.rs", 0);
        s.transcript.complete_tool(
            &ToolCallId::new("r"),
            true,
            (0..10)
                .map(|n| format!("read row {n}\n"))
                .collect::<String>(),
            1,
            None,
        );
        tool(
            &mut s,
            "edit",
            "apply_patch",
            serde_json::json!({"path":"changed.rs"}),
            0,
            true,
        );
        s.transcript.complete_tool(
            &ToolCallId::new("edit"),
            true,
            "applied".into(),
            1,
            Some(format!(
                "@@ -1,1 +1,81 @@\n context\n{}",
                (0..80)
                    .map(|n| format!("+change_{n}\n"))
                    .collect::<String>()
            )),
        );
        tool(
            &mut s,
            "tail",
            "run_command",
            serde_json::json!({"program":"make","args":["check"]}),
            0,
            true,
        );
        let rows = render(&s);
        let receipt = s.command_hit_at(80, position(&rows, "读取 1 次")).unwrap();
        s.transcript
            .set_call_expanded(receipt.item, receipt.call, true);
        s.conv.auto_scroll = false;
        // A summarized patch paints its ends, not its middle, so the reading row
        // has to be one this view actually paints (`change_50` waits for Ctrl+O).
        s.conv.scroll = position(&render(&s), "change_79");
        let before = render(&s)[s.conv.scroll].clone();
        assert!(
            before.contains("change_79"),
            "the reader is on the diff row: {before:?}"
        );
        toggle_span_receipt(&mut s, receipt.item, receipt.call);
        // Folding a receipt above the diff shortens it, so the same content
        // moves to a different absolute row: the guarantee is that the reader
        // stays ON that content, not that the number is frozen.
        assert!(
            render(&s).iter().any(|l| l.contains("change_79")),
            "the diff row the reader was on is still painted after the fold"
        );
    }

    #[test]
    fn fixed_contract_edit_boundaries_merge_neighboring_group_fragments() {
        let mut s = boot();
        read(&mut s, "a", "before.rs", 0);
        read(&mut s, "b", "beside.rs", 1);
        tool(
            &mut s,
            "edit",
            "apply_patch",
            serde_json::json!({"path":"changed.rs"}),
            1,
            true,
        );
        tool(
            &mut s,
            "c",
            "run_command",
            serde_json::json!({"program":"make","args":["check"]}),
            1,
            false,
        );
        tool(
            &mut s,
            "d",
            "run_command",
            serde_json::json!({"program":"make","args":["test"]}),
            2,
            true,
        );
        let rows = render(&s);
        assert_eq!(receipts(&rows).len(), 2, "{rows:#?}");
        assert!(receipts(&rows)[0].contains("读取 2 次"));
        assert!(receipts(&rows)[1].contains("2 个命令"));
        assert!(rows.iter().any(|r| r.contains("+ added")));
        assert_eq!(
            s.transcript
                .items()
                .iter()
                .filter(|item| matches!(item, TranscriptItem::ToolGroup(_)))
                .count(),
            3
        );
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let opened = render(&s);
        assert!(position(&opened, "before.rs") < position(&opened, "beside.rs"));
        assert!(position(&opened, "beside.rs") < position(&opened, "+ added"));
        assert!(!opened.iter().any(|r| r.contains("make check")));
        let child = s
            .command_hit_at(80, position(&opened, "beside.rs"))
            .unwrap();
        s.transcript.toggle_call_at(child.item, child.call);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let key = crate::action::Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('o'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        crate::reducer::reduce(&mut s, key.clone());
        let latest = render(&s);
        assert!(latest.iter().any(|r| r.contains("make check")));
        assert!(latest.iter().any(|r| r.contains("make test")));
        assert!(!latest.iter().any(|r| r.contains("beside.rs")));
        crate::reducer::reduce(&mut s, key);
        let Some(TranscriptItem::ToolGroup(group)) = s.transcript.items().get(child.item) else {
            panic!("original group")
        };
        assert!(
            group.calls[child.call].expanded,
            "latest-phase keyboard fold cannot rewrite previous-phase child choice"
        );
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        assert!(render(&s).iter().any(|r| r.contains("alpha")));
    }

    #[test]
    fn fixed_contract_invisible_work_does_not_split_visible_stages() {
        let mut s = boot();
        read(&mut s, "r1", "before.rs", 0);
        tool(
            &mut s,
            "silent",
            "list_files",
            serde_json::json!({"path":"."}),
            1,
            true,
        );
        read(&mut s, "r2", "after.rs", 2);
        let rows = render(&s);
        assert_eq!(receipts(&rows).len(), 1, "{rows:#?}");
        assert!(receipts(&rows)[0].contains("读取 2 次"));
        assert!(s.transcript.items().iter().any(|it| matches!(it, TranscriptItem::ToolGroup(g) if g.calls.iter().any(|c| c.id.as_str() == "silent"))));
    }

    #[test]
    fn fixed_contract_whitespace_progress_is_transparent_until_text_arrives() {
        let mut s = boot();
        read(&mut s, "r1", "before.rs", 0);
        let id = MessageId::new("empty-progress");
        s.transcript.begin_assistant(id.clone());
        s.transcript.append_assistant(&id, "  ");
        s.transcript.finish_assistant(&id);
        read(&mut s, "r2", "after.rs", 1);
        assert_eq!(receipts(&render(&s)).len(), 1);
        s.transcript
            .append_assistant(&id, "A meaningful discovery.");
        let rows = render(&s);
        assert_eq!(receipts(&rows).len(), 2);
        assert!(rows.iter().any(|r| r.contains("A meaningful discovery.")));
    }

    #[test]
    fn fixed_contract_background_terminal_overrides_an_old_stop_request() {
        let mut s = boot();
        s.transcript.push_tool_started(
            ToolCallId::new("bg"),
            "run_command".into(),
            serde_json::json!({"program":"make","args":["ci"],"background":true}).to_string(),
            false,
            0,
            Some(0),
            None,
        );
        s.transcript
            .set_tool_stop(&ToolCallId::new("bg"), crate::transcript::StopRequest::Sent);
        s.transcript.complete_tool(
            &ToolCallId::new("bg"),
            true,
            "task_id: bg-task\nstatus: running".into(),
            10,
            None,
        );
        assert!(render(&s).iter().any(|r| r.contains("正在停止")));
        s.transcript
            .note_background_exit("bg-task", false, true, None, 200);
        let rows = render(&s);
        assert!(rows.iter().any(|r| r.contains("已停止")), "{rows:#?}");
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("正在停止") || r.contains("正在执行")),
            "{rows:#?}"
        );
    }

    #[test]
    fn fixed_contract_mixed_group_receipts_are_independent_and_diff_stays_full() {
        let mut s = boot();
        read(&mut s, "r", "before.rs", 0);
        tool(
            &mut s,
            "edit",
            "apply_patch",
            serde_json::json!({"path":"changed.rs"}),
            0,
            true,
        );
        let diff = format!(
            "@@ -1,1 +1,41 @@\n context\n{}",
            (0..40)
                .map(|n| format!("+change_{n}\n"))
                .collect::<String>()
        );
        s.transcript.complete_tool(
            &ToolCallId::new("edit"),
            true,
            "applied".into(),
            1,
            Some(diff),
        );
        tool(
            &mut s,
            "failed",
            "run_command",
            serde_json::json!({"program":"cargo","args":["test"]}),
            0,
            false,
        );
        // These calls really share one semantic group, with the edit as its visible boundary.
        assert_eq!(
            s.transcript
                .items()
                .iter()
                .filter(|it| matches!(it, TranscriptItem::ToolGroup(_)))
                .count(),
            1
        );
        let rows = render(&s);
        assert_eq!(receipts(&rows).len(), 2);
        assert!(rows.iter().any(|r| r.contains("change_39")));
        assert!(
            !rows.iter().any(|r| r.contains("before.rs")
                || r.contains("cargo test")
                || r.contains("alpha"))
        );
        let receipt = s.command_hit_at(80, position(&rows, "读取 1 次")).unwrap();
        assert!(receipt.receipt);
        toggle_span_receipt(&mut s, receipt.item, receipt.call);
        let rows = render(&s);
        assert!(rows.iter().any(|r| r.contains("before.rs")));
        assert!(!rows.iter().any(|r| r.contains("cargo test")));
        let child = s.command_hit_at(80, position(&rows, "before.rs")).unwrap();
        assert!(!child.receipt);
        s.transcript.toggle_call_at(child.item, child.call);
        assert!(render(&s).iter().any(|r| r.contains("alpha")));
        toggle_span_receipt(&mut s, receipt.item, receipt.call);
        assert!(!render(&s).iter().any(|r| r.contains("alpha")));
        toggle_span_receipt(&mut s, receipt.item, receipt.call);
        assert!(render(&s).iter().any(|r| r.contains("alpha")));
        let rows = render(&s);
        assert!(position(&rows, "before.rs") < position(&rows, "change_0"));
        assert!(position(&rows, "change_39") < position(&rows, "1 个命令"));
    }

    #[test]
    fn fixed_contract_live_receipt_prioritizes_status_at_narrow_widths() {
        let mut s = boot();
        read(&mut s, "r", "before.rs", 0);
        search(&mut s, "g", "pattern", 0);
        s.transcript.push_tool_started(
            ToolCallId::new("live"),
            "run_command".into(),
            serde_json::json!({"program":"make","args":["ci"]}).to_string(),
            false,
            0,
            Some(0),
            None,
        );
        for width in [24, 40] {
            let rows: Vec<_> = s
                .conversation_lines(width)
                .iter()
                .map(crate::selection::line_to_plain)
                .collect();
            assert!(
                rows.iter().any(|r| r.contains("正在执行 1 项")),
                "{rows:#?}"
            );
        }
        s.transcript.set_tool_stop(
            &ToolCallId::new("live"),
            crate::transcript::StopRequest::Sent,
        );
        for width in [24, 40] {
            let rows: Vec<_> = s
                .conversation_lines(width)
                .iter()
                .map(crate::selection::line_to_plain)
                .collect();
            assert!(rows.iter().any(|r| r.contains("正在停止")), "{rows:#?}");
        }
    }

    #[test]
    fn fixed_contract_continuous_rounds_and_thoughts_share_one_collapsed_receipt() {
        let mut s = boot();
        read(&mut s, "r1", "before.rs", 0);
        thought(&mut s, "private reasoning");
        read(&mut s, "r2", "after.rs", 1);
        seal(&mut s);
        let rows = render(&s);
        assert_eq!(receipts(&rows).len(), 1);
        assert!(receipts(&rows)[0].contains("读取 2 次"));
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let rows = render(&s);
        assert!(rows.iter().any(|r| r.contains("before.rs")));
        assert!(rows.iter().any(|r| r.contains("after.rs")));
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("思考") || r.contains("private reasoning"))
        );
    }

    #[test]
    fn fixed_contract_single_live_tool_is_collapsed_and_the_live_thought_paints() {
        let mut s = boot();
        s.transcript.push_tool_started(
            ToolCallId::new("live"),
            "read_file".into(),
            serde_json::json!({"path":"hidden.rs"}).to_string(),
            false,
            0,
            Some(0),
            None,
        );
        s.transcript.append_thought("hidden live reasoning");
        let rows = render(&s);
        assert_eq!(receipts(&rows).len(), 1, "{rows:#?}");
        assert!(
            rows.iter().any(|r| r.contains("hidden live reasoning")),
            "a live Thought paints its body beside the receipt: {rows:#?}"
        );
        assert!(!rows.iter().any(|r| r.contains("hidden.rs")));
        assert!(rows.iter().any(|r| r.contains("正在执行")));
    }

    #[test]
    fn fixed_contract_prose_has_no_role_bullet() {
        let mut s = boot();
        narration(&mut s, "A meaningful discovery and next step.");
        let rows = render(&s);
        assert!(
            rows.iter()
                .any(|r| r == "A meaningful discovery and next step."),
            "{rows:#?}"
        );
    }

    #[test]
    fn settled_model_rounds_share_a_receipt_without_narration() {
        let mut s = boot();
        read(&mut s, "r1", "before.rs", 0);
        thought(&mut s, "private reasoning");
        read(&mut s, "r2", "after.rs", 1);
        seal(&mut s);
        let rows = render(&s);
        assert_eq!(receipts(&rows).len(), 1);
        assert!(receipts(&rows)[0].contains("读取 2 次"));
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let opened = render(&s);
        assert!(position(&opened, "before.rs") < position(&opened, "after.rs"));
        assert!(!opened.iter().any(|r| r.contains("思考")));
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        assert_eq!(render(&s), rows);
    }

    #[test]
    fn legacy_round_identity_never_invents_a_visible_boundary() {
        for (first, second, expected) in [
            (None, None, 1),
            (None, Some(1), 1),
            (Some(0), None, 1),
            (Some(0), Some(0), 1),
        ] {
            let mut s = boot();
            read(&mut s, "r1", "first-a.rs", 0);
            read(&mut s, "r2", "first-b.rs", 0);
            thought(&mut s, "next reasoning");
            read(&mut s, "r3", "next-a.rs", 1);
            read(&mut s, "r4", "next-b.rs", 1);
            seal(&mut s);
            // Model-step absence is historical data, not a guessed boundary.
            let mut rounds = [first, second].into_iter();
            for item in s.transcript.items_mut() {
                if let TranscriptItem::ToolGroup(group) = item {
                    group.round = rounds.next().unwrap();
                }
            }
            let lines = render(&s);
            assert_eq!(
                receipts(&lines).len(),
                expected,
                "{first:?}/{second:?}: {lines:#?}"
            );
        }
    }

    #[test]
    fn settled_process_collapses_commands_reads_and_failures_in_one_round() {
        let mut s = boot();
        read(&mut s, "r1", "before.rs", 0);
        thought(&mut s, "inspect the result");
        tool(
            &mut s,
            "c1",
            "run_command",
            serde_json::json!({"program":"cargo", "args":["test"], "description":"Run regression tests"}),
            0,
            false,
        );
        read(&mut s, "r2", "after.rs", 0);
        narration(&mut s, "The test failed; investigate next.");

        let collapsed = render(&s);
        let summaries = receipts(&collapsed);
        assert_eq!(summaries.len(), 1, "one process receipt: {collapsed:#?}");
        assert!(summaries[0].contains("读取 2 次"), "{collapsed:#?}");
        assert!(summaries[0].contains("1 个命令"), "{collapsed:#?}");
        assert!(summaries[0].contains("1 个失败"), "{collapsed:#?}");
        assert!(
            !collapsed.iter().any(|r| r.contains("before.rs")
                || r.contains("cargo test")
                || r.contains("alpha"))
        );
        assert!(collapsed.iter().any(|r| r.contains("The test failed")));

        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let open = render(&s);
        assert!(position(&open, "before.rs") < position(&open, "Run regression tests"));
        assert!(position(&open, "Run regression tests") < position(&open, "after.rs"));
        assert!(
            !open.iter().any(|r| r.contains("$ cargo test")),
            "opening the parent only reveals headers: {open:#?}"
        );
        let hit = s
            .command_hit_at(80, position(&open, "Run regression tests"))
            .expect("command retains its own click target");
        s.transcript.toggle_call_at(hit.item, hit.call);
        assert!(render(&s).iter().any(|r| r.contains("$ cargo test")));
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        assert!(!render(&s).iter().any(|r| r.contains("$ cargo test")));
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        assert!(
            render(&s).iter().any(|r| r.contains("$ cargo test")),
            "the child display choice survives closing its parent"
        );
    }

    #[test]
    fn a_settled_single_command_keeps_its_description_inside_a_receipt() {
        let mut s = boot();
        tool(
            &mut s,
            "c1",
            "run_command",
            serde_json::json!({"program":"git","args":["status"],"description":"Check working tree"}),
            0,
            true,
        );
        narration(&mut s, "Checked.");
        let rows = render(&s);
        assert!(receipts(&rows)[0].contains("1 个命令"));
        assert!(!rows.iter().any(|r| r.contains("Check working tree")));
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let rows = render(&s);
        let hit = s
            .command_hit_at(80, position(&rows, "Check working tree"))
            .unwrap();
        s.transcript.toggle_call_at(hit.item, hit.call);
        assert!(render(&s).iter().any(|r| r.contains("$ git status")));
    }

    #[test]
    fn failed_exploration_stays_in_the_receipt_and_is_reachable() {
        let mut s = boot();
        read(&mut s, "r1", "a.rs", 0);
        tool(
            &mut s,
            "r2",
            "read_file",
            serde_json::json!({"path":"missing.rs"}),
            0,
            false,
        );
        narration(&mut s, "One file could not be read.");
        let lines = render(&s);
        assert_eq!(receipts(&lines).len(), 1, "{lines:#?}");
        assert!(receipts(&lines)[0].contains("1 个失败"), "{lines:#?}");
        assert!(!lines.iter().any(|r| r.contains("missing.rs")));
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let lines = render(&s);
        assert!(lines.iter().any(|r| r.contains("missing.rs")));
        assert!(
            s.command_hit_at(80, position(&lines, "missing.rs"))
                .is_some()
        );
    }

    #[test]
    fn a_settled_background_wait_hides_its_wire_receipt_until_opened() {
        let mut s = boot();
        tool(
            &mut s,
            "w1",
            "wait_task",
            serde_json::json!({"task_id":"bg-2"}),
            0,
            false,
        );
        s.transcript.complete_tool(
            &ToolCallId::new("w1"),
            false,
            "task_id: bg-2\nstatus: exited\nnext_cursor: 11647\nexit_code: Some(2)".into(),
            211_300,
            None,
        );
        narration(&mut s, "Investigate the failed task.");
        let lines = render(&s);
        assert!(
            !lines.iter().any(|r| r.contains("next_cursor")
                || r.contains("Some(2)")
                || r.contains("完成 0 项")),
            "wire fields stay inside the drawer: {lines:#?}"
        );
        let target = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, target);
        let rows = render(&s);
        let hit = s
            .command_hit_at(80, position(&rows, "等待后台任务"))
            .unwrap();
        s.transcript.toggle_call_at(hit.item, hit.call);
        assert!(
            render(&s).iter().any(|r| r.contains("next_cursor")),
            "the original evidence stays inspectable"
        );
    }

    #[test]
    fn a_process_receipt_counts_the_detached_process_outcome() {
        let mut s = boot();
        tool(
            &mut s,
            "bg",
            "run_command",
            serde_json::json!({"program":"make", "args":["ci"], "background":true}),
            0,
            true,
        );
        s.transcript.complete_tool(
            &ToolCallId::new("bg"),
            true,
            "task_id: bg-2\nstatus: running".into(),
            40,
            None,
        );
        read(&mut s, "r1", "README.md", 0);
        s.transcript
            .note_background_exit("bg-2", false, false, Some(2), 34_900);
        narration(&mut s, "CI failed.");
        let lines = render(&s);
        assert_eq!(receipts(&lines).len(), 1, "{lines:#?}");
        assert!(
            receipts(&lines)[0].contains("1 个失败"),
            "spawn success cannot hide process failure: {lines:#?}"
        );
        s.transcript
            .note_background_exit("bg-2", false, true, None, 34_900);
        assert!(
            !render(&s).iter().any(|r| r.contains("个失败")),
            "a stopped process is not a failure"
        );
    }

    #[test]
    fn live_work_and_full_diffs_never_disappear_inside_process_receipts() {
        let mut s = boot();
        read(&mut s, "r1", "before.rs", 0);
        read(&mut s, "r2", "config.rs", 0);
        s.transcript.push_tool_started(
            ToolCallId::new("live"),
            "run_command".into(),
            serde_json::json!({"program":"make", "args":["ci"]}).to_string(),
            false,
            0,
            Some(0),
            None,
        );
        let lines = render(&s);
        assert!(
            lines.iter().any(|r| r.contains("正在执行")),
            "running work stays visible: {lines:#?}"
        );
        s.transcript
            .complete_tool(&ToolCallId::new("live"), true, "ok".into(), 100, None);
        tool(
            &mut s,
            "edit",
            "apply_patch",
            serde_json::json!({"path":"changed.rs"}),
            3,
            true,
        );
        let diff = format!(
            "@@ -1,1 +1,81 @@\n context\n{}",
            (0..80)
                .map(|n| format!("+change_{n}\n"))
                .collect::<String>()
        );
        s.transcript.complete_tool(
            &ToolCallId::new("edit"),
            true,
            "applied".into(),
            1,
            Some(diff),
        );
        read(&mut s, "r3", "after.rs", 4);
        read(&mut s, "r4", "next.rs", 4);
        narration(&mut s, "Done.");
        for width in [28, 80, 120] {
            let lines: Vec<String> = s
                .conversation_lines(width)
                .iter()
                .map(crate::selection::line_to_plain)
                .collect();
            assert_eq!(
                receipts(&lines).len(),
                2,
                "the edit separates the receipts at width {width}: {lines:#?}"
            );
            assert!(lines.iter().any(|r| r.contains("change_0")));
            assert!(lines.iter().any(|r| r.contains("change_79")));
            let hit = s
                .command_hit_at(width, position(&lines, "编辑文件"))
                .expect("the edit head is its own fold target");
            assert!(
                !hit.receipt,
                "the edit is never absorbed into a process receipt at width {width}"
            );
        }
    }

    #[test]
    fn live_thought_paints_then_leaves_no_history_row_when_it_settles() {
        let mut s = boot();
        thought(&mut s, "first reasoning");
        s.transcript.append_thought("live reasoning continues");
        let rows = render(&s);
        assert!(
            rows.iter().any(|r| r.contains("live reasoning continues")),
            "a live Thought paints its body: {rows:#?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("思考")),
            "and its own header: {rows:#?}"
        );
        assert_eq!(s.conversation_lines_and_hits(80).1.len(), 1);
        s.transcript.finish_thought(Some(900));
        assert_eq!(s.transcript.items().len(), 3);
        for item in 1..3 {
            let (start, end) = s.item_span(item, 80).unwrap();
            assert_eq!(start, end);
        }
        s.transcript.append_thought("next reasoning");
        let rows = render(&s);
        assert!(
            !rows.iter().any(|r| r.contains("live reasoning continues")),
            "a settled Thought leaves no row: {rows:#?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("next reasoning")),
            "the next live Thought paints: {rows:#?}"
        );
        assert!(s.transcript.items().iter().any(
            |it| matches!(it, TranscriptItem::Thought(b) if !b.done && b.text == "next reasoning")
        ));
    }

    /// Ctrl+O opens tool details while retained reasoning remains invisible.
    #[test]
    fn ctrl_o_never_reveals_standalone_thought_headers() {
        let mut s = boot();
        read(&mut s, "r", "README.md", 0);
        thought(&mut s, "retained reasoning");
        s.transcript
            .set_item_display(2, crate::fold::DisplayMode::Expanded);
        let key = crate::action::Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('o'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        crate::reducer::reduce(&mut s, key);
        assert!(
            !render(&s)
                .iter()
                .any(|r| r.contains("思考") || r.contains("retained reasoning"))
        );
        assert!(render(&s).iter().any(|r| r.contains("README.md")));
        let TranscriptItem::Thought(block) = &s.transcript.items()[2] else {
            panic!("retained")
        };
        assert_eq!(block.text, "retained reasoning");
        assert_eq!(block.duration_ms, Some(800));
    }

    /// A reasoning-only turn gives Ctrl+O no conversation target.
    #[test]
    fn ctrl_o_has_no_reasoning_fallback_without_tools() {
        let mut s = boot();
        thought(&mut s, "retained reasoning");
        s.transcript
            .set_item_display(1, crate::fold::DisplayMode::Expanded);
        let key = crate::action::Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('o'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        crate::reducer::reduce(&mut s, key);
        assert!(
            !render(&s)
                .iter()
                .any(|r| r.contains("思考") || r.contains("retained reasoning"))
        );
        assert!(s.conversation_lines_and_hits(80).1.is_empty());
        let TranscriptItem::Thought(block) = &s.transcript.items()[1] else {
            panic!("retained")
        };
        assert_eq!(block.text, "retained reasoning");
        assert_eq!(block.duration_ms, Some(800));
    }

    #[test]
    fn hidden_thoughts_preserve_mouse_tool_details() {
        let mut s = boot();
        thought(&mut s, "before read");
        read(&mut s, "r", "README.md", 0);
        thought(&mut s, "before answer");
        narration(&mut s, "answer");
        let group = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, group);
        assert!(!s.tools_expanded, "mouse opens only its target");
        assert_eq!(
            render(&s)
                .iter()
                .filter(|line| line.contains("思考 ·"))
                .count(),
            0
        );
        assert!(!render(&s).iter().any(|line| line.contains("before read")));
        assert!(!render(&s).iter().any(|line| line.contains("before answer")));
    }

    fn receipts(lines: &[String]) -> Vec<&String> {
        lines
            .iter()
            .filter(|l| {
                (l.starts_with('\u{25b8}') || l.starts_with('\u{25be}')) && !l.contains("编辑文件")
            })
            .collect()
    }

    /// The index of the run's anchor: its first work group.
    fn first_group_index(s: &AppState) -> usize {
        s.transcript
            .items()
            .iter()
            .position(|it| matches!(it, TranscriptItem::ToolGroup(_)))
            .expect("a tool group")
    }

    /// Historical reasoning display choices cannot override the status-only contract.
    #[test]
    fn a_legacy_thought_display_choice_cannot_reveal_its_body() {
        let mut s = boot();
        thought(&mut s, "retained reasoning");
        s.transcript
            .set_item_display(1, crate::fold::DisplayMode::Expanded);
        crate::conversation::interaction::toggle_fold(&mut s, 1);
        assert!(
            !render(&s)
                .iter()
                .any(|r| r.contains("思考") || r.contains("retained reasoning"))
        );
        assert!(s.conversation_lines_and_hits(80).1.is_empty());
        let TranscriptItem::Thought(block) = &s.transcript.items()[1] else {
            panic!("retained")
        };
        assert_eq!(block.text, "retained reasoning");
        assert_eq!(block.duration_ms, Some(800));
    }

    #[test]
    fn ctrl_o_opens_the_receipt_containing_the_latest_tool_group() {
        let mut s = boot();
        read(&mut s, "r1", "old-a.rs", 0);
        read(&mut s, "r2", "old-b.rs", 0);
        seal(&mut s);
        narration(&mut s, "Next step");
        read(&mut s, "r3", "latest-c.rs", 2);
        read(&mut s, "r4", "latest-d.rs", 2);
        seal(&mut s);
        let collapsed = render(&s);
        assert!(
            !collapsed
                .iter()
                .any(|l| l.contains("latest-c.rs") || l.contains("latest-d.rs"))
        );
        let ctrl_o = crate::action::Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('o'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        crate::reducer::reduce(&mut s, ctrl_o.clone());
        let open = render(&s);
        assert!(open.iter().any(|l| l.contains("latest-c.rs")), "{open:?}");
        assert!(open.iter().any(|l| l.contains("latest-d.rs")), "{open:?}");
        assert!(
            !open
                .iter()
                .any(|l| l.contains("old-a.rs") || l.contains("old-b.rs")),
            "{open:?}"
        );
        assert_eq!(
            receipts(&open)
                .iter()
                .filter(|l| l.starts_with('▾'))
                .count(),
            1
        );
        crate::reducer::reduce(&mut s, ctrl_o);
        assert_eq!(render(&s), collapsed, "the same receipt closes again");
    }

    fn position(lines: &[String], needle: &str) -> usize {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not found in {lines:#?}"))
    }

    /// THOUGHT-IA-2/3/5/6 and TOOL-IA-4: alternating Thoughts and exploration
    /// groups paint ONE aggregate receipt, the Thoughts stay out of the label,
    /// and neither a Thought header nor a member target is on screen.
    #[test]
    fn a_collapsed_run_is_one_receipt_and_hides_its_thoughts() {
        let mut s = boot();
        thought(&mut s, "checking pricing");
        read(&mut s, "r1", "README.md", 0);
        thought(&mut s, "now config");
        read(&mut s, "r2", "config.rs", 0);
        search(&mut s, "g1", "missing model", 0);
        thought(&mut s, "almost done");

        let lines = render(&s);
        let found = receipts(&lines);
        assert_eq!(found.len(), 1, "one aggregate receipt: {lines:#?}");
        assert_eq!(found[0].trim_end(), "\u{25b8} 读取 2 次 \u{b7} 搜索 1 次");
        assert!(
            !lines.iter().any(|l| l.contains("思考 ·")),
            "no Thought header while folded: {lines:#?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("README.md") || l.contains("config.rs")),
            "member targets are hidden while folded: {lines:#?}"
        );
    }

    /// Opening the receipt restores tool chronology while reasoning stays hidden.
    #[test]
    fn expanding_a_run_restores_tool_chronology_without_thought_rows() {
        let mut s = boot();
        thought(&mut s, "first thought");
        read(&mut s, "r1", "a.rs", 0);
        thought(&mut s, "second thought");
        read(&mut s, "r2", "b.rs", 1);
        search(&mut s, "g", "missing model", 1);
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let rows = render(&s);
        assert!(position(&rows, "a.rs") < position(&rows, "b.rs"));
        assert!(position(&rows, "b.rs") < position(&rows, "missing model"));
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("思考") || r.contains("thought"))
        );
        assert_eq!(
            s.transcript
                .items()
                .iter()
                .filter(|it| matches!(it, TranscriptItem::Thought(_)))
                .count(),
            2
        );
    }

    #[test]
    fn nested_exploration_detail_keeps_long_output_reachable() {
        let mut s = boot();
        read(&mut s, "r1", "long.rs", 0);
        read(&mut s, "r2", "b.rs", 0);
        seal(&mut s);
        let body = (0..60)
            .map(|i| format!("unique line {i}\n"))
            .collect::<String>();
        s.transcript
            .complete_tool(&ToolCallId::new("r1"), true, body, 10, None);
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let rows = render(&s);
        let hit = s.command_hit_at(80, position(&rows, "long.rs")).unwrap();
        s.transcript.toggle_call_at(hit.item, hit.call);
        let rows = render(&s);
        assert!(rows.iter().any(|r| r.contains("unique line 0")));
        assert!(rows.iter().any(|r| r.contains("unique line 30")));
        assert!(rows.iter().any(|r| r.contains("unique line 59")));
        assert!(!rows.iter().any(|r| r.contains("beta")));
    }

    #[test]
    fn nested_exploration_opens_only_requested_call_and_parent_hides_children() {
        let mut s = boot();
        thought(&mut s, "reasoning body");
        read(&mut s, "r1", "a.rs", 0);
        read(&mut s, "r2", "b.rs", 0);
        seal(&mut s);
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let rows = render(&s);
        assert!(
            !rows.iter().any(|r| r.contains("alpha")),
            "headers only: {rows:#?}"
        );
        let row = position(&rows, "a.rs");
        let hit = s
            .command_hit_at(80, row)
            .expect("each child has its own call hit");
        s.transcript.toggle_call_at(hit.item, hit.call);
        let rows = render(&s);
        assert_eq!(rows.iter().filter(|r| r.contains("alpha")).count(), 1);
        assert_eq!(rows.iter().filter(|r| r.contains("beta")).count(), 1);
        // Historical reasoning display choices cannot reveal it in the conversation.
        crate::conversation::interaction::toggle_fold(&mut s, 1);
        let opened = render(&s);
        assert_eq!(
            opened
                .iter()
                .filter(|r| r.contains("reasoning body"))
                .count(),
            0,
            "the Thought remains status only: {opened:#?}"
        );
        // Folding the receipt hides tools without exposing retained reasoning.
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let rows = render(&s);
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("a.rs") || r.contains("alpha")),
            "the closed receipt hides its tool rows: {rows:#?}"
        );
        assert_eq!(
            rows.iter().filter(|r| r.contains("reasoning body")).count(),
            0,
            "a pinned Thought remains status only: {rows:#?}"
        );
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let rows = render(&s);
        assert_eq!(rows.iter().filter(|r| r.contains("beta")).count(), 1);
        s.transcript.toggle_call_at(hit.item, hit.call);
        assert!(!render(&s).iter().any(|r| r.contains("beta")));
    }

    /// Even a legacy expanded reasoning choice stays outside the conversation.
    #[test]
    fn an_opened_legacy_thought_stays_hidden_when_the_parent_folds() {
        let mut s = boot();
        thought(&mut s, "retained reasoning");
        read(&mut s, "r", "after.rs", 0);
        s.transcript
            .set_item_display(1, crate::fold::DisplayMode::Expanded);
        let key = crate::action::Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('o'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        crate::reducer::reduce(&mut s, key);
        assert!(
            !render(&s)
                .iter()
                .any(|r| r.contains("思考") || r.contains("retained reasoning"))
        );
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        assert!(!render(&s).iter().any(|r| r.contains("retained reasoning")));
        let TranscriptItem::Thought(block) = &s.transcript.items()[1] else {
            panic!("retained")
        };
        assert_eq!(block.text, "retained reasoning");
        assert_eq!(block.duration_ms, Some(800));
    }

    /// A settled command joins the process receipt; its chronology is restored
    /// only when the reader opens that receipt.
    #[test]
    fn a_settled_command_joins_the_run() {
        let mut s = boot();
        thought(&mut s, "t1");
        read(&mut s, "r1", "a.rs", 0);
        thought(&mut s, "t2");
        read(&mut s, "r2", "b.rs", 0);
        tool(
            &mut s,
            "c1",
            "run_command",
            serde_json::json!({ "program": "cargo", "args": ["test"] }),
            0,
            true,
        );
        read(&mut s, "r3", "c.rs", 0);
        search(&mut s, "g1", "x", 0);
        seal(&mut s);

        let lines = render(&s);
        assert_eq!(receipts(&lines).len(), 1, "one process receipt: {lines:#?}");
        assert!(!lines.iter().any(|l| l.contains("cargo test")));
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let lines = render(&s);
        let run = position(&lines, "cargo test");
        assert!(
            position(&lines, "b.rs") < run && run < position(&lines, "c.rs"),
            "the command retains its chronological place: {lines:#?}"
        );
    }

    /// TOOL-IA-7: a confirmed Edit breaks the fold — a diff is a result, not
    /// exploration.
    #[test]
    fn an_edit_breaks_the_run() {
        let mut s = boot();
        thought(&mut s, "t1");
        read(&mut s, "r1", "a.rs", 0);
        thought(&mut s, "t2");
        read(&mut s, "r2", "b.rs", 0);
        thought(&mut s, "before edit");
        tool(
            &mut s,
            "e1",
            "apply_patch",
            serde_json::json!({ "path": "a.rs" }),
            0,
            true,
        );
        thought(&mut s, "after edit");
        read(&mut s, "r3", "c.rs", 0);
        search(&mut s, "g1", "x", 0);
        seal(&mut s);

        let lines = render(&s);
        assert_eq!(
            receipts(&lines).len(),
            2,
            "the Edit split the run: {lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("+ added")),
            "the diff stays: {lines:#?}"
        );
    }

    /// TOOL-IA-8: narration (assistant prose) breaks the fold and stays whole.
    #[test]
    fn narration_breaks_the_run_and_stays_visible() {
        let mut s = boot();
        thought(&mut s, "t1");
        read(&mut s, "r1", "a.rs", 0);
        thought(&mut s, "t2");
        read(&mut s, "r2", "b.rs", 0);
        narration(&mut s, "the workspace is clean");
        read(&mut s, "r3", "c.rs", 0);
        search(&mut s, "g1", "x", 0);
        seal(&mut s);

        let lines = render(&s);
        assert_eq!(
            receipts(&lines).len(),
            2,
            "narration split the run: {lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("the workspace is clean")),
            "narration is never folded: {lines:#?}"
        );
    }

    /// TOOL-IA-9: a failed exploration call is a breaker, so its target can
    /// never be hidden inside an aggregate receipt.
    #[test]
    fn a_failed_exploration_target_is_collapsed_but_reachable() {
        let mut s = boot();
        read(&mut s, "r1", "a.rs", 0);
        tool(
            &mut s,
            "f",
            "read_file",
            serde_json::json!({"path":"missing.rs"}),
            1,
            false,
        );
        let rows = render(&s);
        assert_eq!(receipts(&rows).len(), 1);
        assert!(receipts(&rows)[0].contains("1 个失败"));
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("missing.rs") || r.contains("alpha"))
        );
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        assert!(render(&s).iter().any(|r| r.contains("missing.rs")));
    }

    /// TOOL-IA-3-like: a lone exploration call keeps its own target row instead
    /// of a one-item receipt.
    #[test]
    fn a_lone_exploration_call_keeps_its_target_behind_a_receipt() {
        let mut s = boot();
        read(&mut s, "r1", "only.rs", 0);
        let rows = render(&s);
        assert_eq!(receipts(&rows).len(), 1);
        assert!(!rows.iter().any(|r| r.contains("only.rs")));
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        assert!(render(&s).iter().any(|r| r.contains("only.rs")));
    }

    /// SEMANTIC-IA-2 / toggling: opening and closing the fold is reversible and
    /// never reorders or loses a semantic item.
    #[test]
    fn toggling_the_receipt_is_reversible() {
        let mut s = boot();
        thought(&mut s, "t1");
        read(&mut s, "r1", "a.rs", 0);
        thought(&mut s, "t2");
        read(&mut s, "r2", "b.rs", 0);
        seal(&mut s);
        let before = render(&s);
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let open = render(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let after = render(&s);
        assert_eq!(before, after, "closing restores the collapsed frame");
        assert!(open.len() > before.len(), "opening revealed detail");
    }
}

#[cfg(test)]
mod semantic_copy_tests {
    use super::*;
    use crate::selection::{TextPos, TextSelection, extract_selected_text_with_layout};
    use crate::state::Boot;
    use leveler_client_protocol::{MessageId, SessionId};

    fn dark_state() -> AppState {
        let mut s = AppState::new(
            crate::theme::Theme::dark(),
            Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        );
        s.size = (80, 40);
        s.conv.rect = Some((0, 2, 80, 30));
        s
    }

    fn copy_rows(state: &AppState, width: usize, start: usize, end: usize) -> String {
        let (plain, layout) = state.conversation_plain(width);
        assert!(end > start, "empty selection");
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: start, col: 0 });
        sel.extend(TextPos {
            row: end - 1,
            col: 10_000,
        });
        sel.finish();
        extract_selected_text_with_layout(&plain, &layout, &sel)
    }

    fn copy_item(state: &AppState, width: usize, item: usize) -> String {
        let (start, end) = state.item_span(item, width).expect("item span");
        copy_rows(state, width, start, end)
    }

    fn answer(state: &mut AppState, text: &str) {
        let id = MessageId::new(format!("a{}", state.transcript.items().len()));
        state.transcript.begin_assistant(id.clone());
        state.transcript.append_assistant(&id, text);
        state.transcript.finish_assistant(&id);
    }

    #[test]
    fn a_prompt_copies_its_source_and_never_the_painted_marker() {
        let mut s = dark_state();
        // The source itself contains the marker glyph: it must survive.
        s.transcript.push_user("第一行 ❯ 是原文\n\n第二段".into());
        let copied = copy_item(&s, 80, 0);
        assert_eq!(copied, "第一行 ❯ 是原文\n\n第二段");
        // The band padding and the painted marker are not in the item text.
        assert!(!copied.starts_with('❯'));
    }

    /// A prompt that wraps inside its card copies exactly its source at every
    /// width: the card gutter and its padding never reach the clipboard.
    #[test]
    fn a_long_wrapped_prompt_copies_its_source_without_card_padding() {
        let mut s = dark_state();
        let source = (0..30)
            .map(|i| format!("word{i:02} "))
            .collect::<String>()
            .trim_end()
            .to_string();
        s.transcript.push_user(source.clone());
        for width in [80usize, 60, 48] {
            assert_eq!(copy_item(&s, width, 0), source, "width {width}");
        }
    }

    #[test]
    fn markdown_copies_block_structure_without_visual_glyphs() {
        let mut s = dark_state();
        answer(
            &mut s,
            "## 标题\n\n正文 **加粗** 文本。\n\n- 第一项\n- 第二项\n\n> 引用\n",
        );
        let copied = copy_item(&s, 80, 0);
        assert_eq!(
            copied,
            "## 标题\n\n正文 加粗 文本。\n\n- 第一项\n- 第二项\n\n> 引用"
        );
        assert!(!copied.contains('▎'), "{copied:?}");
        assert!(!copied.contains('•'), "{copied:?}");
        assert!(!copied.contains('▌'), "{copied:?}");
        assert!(!copied.contains("**"), "{copied:?}");
    }

    #[test]
    fn a_code_block_copies_pure_code_without_header_or_fence() {
        let mut s = dark_state();
        answer(
            &mut s,
            "```sql\nSELECT id, name\nFROM users      -- 注释\nWHERE id = 1\nORDER BY name\nLIMIT 10;\n```",
        );
        let copied = copy_item(&s, 80, 0);
        assert_eq!(
            copied,
            "SELECT id, name\nFROM users      -- 注释\nWHERE id = 1\nORDER BY name\nLIMIT 10;"
        );
        assert!(!copied.contains("sql"), "{copied:?}");
        assert!(!copied.contains("```"), "{copied:?}");
    }

    #[test]
    fn wrapped_code_rejoins_to_the_source_line() {
        let mut s = dark_state();
        let long = "let value = call_something(alpha, beta, gamma, delta, epsilon, zeta);";
        answer(
            &mut s,
            &format!("```rust\nfn main() {{\n    {long}\n}}\n```"),
        );
        for width in [80usize, 48, 32] {
            let copied = copy_item(&s, width, 0);
            assert_eq!(
                copied,
                format!("fn main() {{\n    {long}\n}}"),
                "width {width}"
            );
        }
    }

    #[test]
    fn json_and_shell_code_keep_indentation_and_comments() {
        for (lang, body) in [
            (
                "json",
                "{\n  \"name\": \"leveler\",\n  \"values\": [1, 2, 3]\n}",
            ),
            (
                "sh",
                "# keep this comment\nmkdir -p out\n  echo \"  indented\"\nexit 0",
            ),
        ] {
            let mut s = dark_state();
            let mut md = String::from("```");
            md.push_str(lang);
            md.push('\n');
            md.push_str(body);
            md.push_str("\n```");
            answer(&mut s, &md);
            let copied = copy_item(&s, 80, 0);
            assert_eq!(copied, body, "{lang}");
        }
    }

    #[test]
    fn reasoning_copies_its_body_without_header_or_rail() {
        let mut s = dark_state();
        s.transcript.begin_thought();
        s.transcript.append_thought("先检查输入。\n再核对输出。");
        let index = s.transcript.items().len() - 1;
        s.transcript
            .set_item_display(index, crate::fold::DisplayMode::Expanded);
        let copied = copy_item(&s, 80, index);
        assert_eq!(copied, "先检查输入。\n再核对输出。");
        assert!(!copied.contains('◆'), "{copied:?}");
        assert!(!copied.contains('│'), "{copied:?}");
    }

    #[test]
    fn tool_rows_never_enter_a_message_copy() {
        let mut s = dark_state();
        s.transcript.push_user("查一下".into());
        let id = leveler_client_protocol::ToolCallId::new("t1");
        s.transcript.push_tool_started(
            id.clone(),
            "read_file".into(),
            serde_json::json!({ "path": "a.rs" }).to_string(),
            false,
            0,
            None,
            None,
        );
        s.transcript
            .complete_tool(&id, true, "read".into(), 5, None);
        // Seal the run so the group renders as settled history.
        s.transcript.push_note("sealed".into());
        answer(&mut s, "答案在正文里。");
        let total = s.conversation_plain(80).0.len();
        let copied = copy_rows(&s, 80, 0, total);
        assert!(copied.contains("查一下"), "{copied:?}");
        assert!(copied.contains("答案在正文里。"), "{copied:?}");
        assert!(!copied.contains("读取"), "{copied:?}");
        assert!(!copied.contains('›'), "{copied:?}");
        assert!(!copied.contains('▸'), "{copied:?}");
        assert!(!copied.contains("0.0s"), "{copied:?}");
    }

    #[test]
    fn prompt_leading_indentation_and_internal_spaces_survive() {
        let mut s = dark_state();
        s.transcript.push_user("  前导空格\n正常行 a  b   c".into());
        assert_eq!(copy_item(&s, 60, 0), "  前导空格\n正常行 a  b   c");
    }

    #[test]
    fn unicode_emoji_and_source_glyphs_survive_copy() {
        let mut s = dark_state();
        s.transcript
            .push_user("中文 😀 émoji — 保留\n第二行：❯".into());
        answer(
            &mut s,
            "```text\n│ 这是原文里的竖线\n• 这是原文里的圆点\n# 这是原文里的井号\n```\n\n结束 😀",
        );
        let user = copy_item(&s, 60, 0);
        assert_eq!(user, "中文 😀 émoji — 保留\n第二行：❯");
        let answer = copy_item(&s, 60, 1);
        assert_eq!(
            answer,
            "│ 这是原文里的竖线\n• 这是原文里的圆点\n# 这是原文里的井号\n\n结束 😀"
        );
    }

    #[test]
    fn a_cross_block_drag_keeps_each_blocks_real_text() {
        let mut s = dark_state();
        s.transcript.push_user("第一块".into());
        answer(
            &mut s,
            "回答一。\n\n- 列表项\n\n```sh\necho ok\nprintf done\n```",
        );
        let all = copy_rows(&s, 60, 0, s.conversation_plain(60).0.len());
        assert!(all.starts_with("第一块"), "{all:?}");
        assert!(
            all.contains("回答一。\n\n- 列表项\n\necho ok\nprintf done"),
            "{all:?}"
        );
    }

    /// Narration and the final answer are different semantic roles but the
    /// same authored prose: both copy in full, without their painted bullets.
    #[test]
    fn narration_and_the_final_answer_both_copy_their_text() {
        let mut s = dark_state();
        s.transcript.push_user("question".into());
        let id = MessageId::new("narration");
        s.transcript.begin_assistant(id.clone());
        s.transcript
            .append_assistant(&id, "Narration: reading the files.");
        s.transcript.finish_assistant(&id);
        if let TranscriptItem::Assistant(block) = &mut s.transcript.items_mut()[1] {
            block.kind = crate::transcript::AssistantKind::Progress;
        }
        answer(&mut s, "Final: the answer.");
        let all = copy_rows(&s, 80, 0, s.conversation_plain(80).0.len());
        assert!(all.contains("Narration: reading the files."), "{all:?}");
        assert!(all.contains("Final: the answer."), "{all:?}");
        assert!(!all.contains('●'), "the painted bullet never copies");
    }

    /// A runtime note is a host fact, not a user prompt: it copies its text
    /// with no user marker and no host glyph.
    #[test]
    fn a_runtime_note_never_becomes_a_user_prompt_in_a_copy() {
        let mut s = dark_state();
        s.transcript.push_user("real ask".into());
        s.transcript.push_note("runtime injected fact".into());
        answer(&mut s, "done");
        let all = copy_rows(&s, 80, 0, s.conversation_plain(80).0.len());
        assert!(all.starts_with("real ask"), "{all:?}");
        assert!(all.contains("runtime injected fact"), "{all:?}");
        assert!(!all.contains('❯'), "the user marker is paint-only");
        assert!(!all.contains('◆'), "the note glyph is paint-only");
    }

    fn push_shell(state: &mut AppState, command: &str, output: &str) -> usize {
        let id = leveler_core::UserShellId::new("ush-copy");
        state.transcript.push_user_shell_started(
            id.clone(),
            command.to_string(),
            "/repo".into(),
            0,
        );
        state
            .transcript
            .append_user_shell_output(&id, output, false);
        state.transcript.complete_user_shell(
            &id,
            Some(0),
            120,
            crate::transcript::UserShellStatus::Success,
        );
        state.transcript.user_shell_index(&id).expect("shell block")
    }

    /// Selecting a shell record copies the command, then the output's own
    /// text. The status header, the rail and the `$` prompt marker are
    /// display chrome; a real `│`, `$` or `▏` inside the output survives.
    #[test]
    fn a_shell_record_copies_content_without_rail_prompt_or_status() {
        let mut s = dark_state();
        let index = push_shell(
            &mut s,
            "printf 'x'",
            "│ name │ value │\n$ literal\n  indented\n▏ sql\n",
        );
        let copied = copy_item(&s, 80, index);
        assert_eq!(
            copied,
            "printf 'x'\n│ name │ value │\n$ literal\n  indented\n▏ sql"
        );
        assert!(!copied.contains("Ran (user)"), "{copied:?}");
        assert!(!copied.contains("│ printf"), "the rail is not copied");
    }

    /// The SQL case that started this: a `▏` in the code is source text and
    /// survives, while any renderer-owned gutter is still stripped.
    #[test]
    fn a_sql_code_block_keeps_a_real_bar_glyph_and_strips_the_visual_gutter() {
        let mut s = dark_state();
        answer(
            &mut s,
            "```sql\n▏ SELECT s.id,\n       s.scope,\n  FROM repo_review_skill s;\n```",
        );
        let copied = copy_item(&s, 80, 0);
        assert_eq!(
            copied,
            "▏ SELECT s.id,\n       s.scope,\n  FROM repo_review_skill s;"
        );
    }

    /// A diff's `+`, `-` and `@@` are payload, not decoration.
    #[test]
    fn a_diff_copy_keeps_add_remove_and_hunk_markers() {
        let mut s = dark_state();
        answer(&mut s, "```diff\n@@ -1 +1 @@\n-old line\n+new line\n```");
        let copied = copy_item(&s, 80, 0);
        assert_eq!(copied, "@@ -1 +1 @@\n-old line\n+new line");
    }

    /// The complete-capture pointer is metadata: painted, never copied.
    #[test]
    fn the_full_output_pointer_is_painted_but_never_copied() {
        let mut s = dark_state();
        let id = leveler_core::UserShellId::new("ush-full");
        s.transcript
            .push_user_shell_started(id.clone(), "seq 1 1000000".into(), "/repo".into(), 0);
        s.transcript
            .append_user_shell_output(&id, "TAIL-ONLY\n", false);
        s.transcript
            .set_user_shell_full_output(&id, Some("/state/capture".into()));
        s.transcript.mark_user_shell_output_truncated(&id);
        s.transcript.complete_user_shell(
            &id,
            Some(0),
            10,
            crate::transcript::UserShellStatus::Success,
        );
        let index = s.transcript.user_shell_index(&id).expect("shell");
        let (plain, _) = s.conversation_plain(80);
        let painted: String = plain.join("\n");
        assert!(
            painted.contains("完整输出可用"),
            "the open affordance is visible: {painted:?}"
        );
        assert!(
            !painted.contains("/state/capture"),
            "the capture path is internal and must not leak into the UI: {painted:?}"
        );
        let copied = copy_item(&s, 80, index);
        assert_eq!(copied, "seq 1 1000000\nTAIL-ONLY");
        assert!(
            !copied.contains("完整输出"),
            "metadata never enters a selection copy"
        );
    }

    /// A virtual output block never lays out its full result in the
    /// conversation, even when a running block was pinned expanded.
    #[test]
    fn a_capture_backed_result_stays_a_bounded_record() {
        let mut s = dark_state();
        let id = leveler_core::UserShellId::new("ush-virtual");
        s.transcript
            .push_user_shell_started(id.clone(), "big".into(), "/repo".into(), 0);
        let body: String = (0..20_000).map(|n| format!("line-{n:05}\n")).collect();
        s.transcript.append_user_shell_output(&id, &body, false);
        s.transcript
            .set_user_shell_full_output(&id, Some("/state/capture".into()));
        s.transcript.mark_user_shell_output_truncated(&id);
        s.transcript.complete_user_shell(
            &id,
            Some(0),
            10,
            crate::transcript::UserShellStatus::Success,
        );
        let index = s.transcript.user_shell_index(&id).expect("shell");
        s.transcript.expand_item_display(index);
        let lines = s.conversation_lines(80);
        assert!(
            lines.len() < 100,
            "20k output lines must not be laid out inline: {} rows",
            lines.len()
        );
        let painted: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<Vec<_>>()
            .join("");
        assert!(painted.contains("完整输出可用"));
    }

    /// A screen wrap of one terminal line is one source line: copy rejoins it
    /// with no invented newline.
    #[test]
    fn wrapped_shell_output_copies_one_logical_line() {
        let mut s = dark_state();
        let long = "abcdefghij".repeat(20);
        let index = push_shell(&mut s, "run", &format!("{long}\n"));
        for width in [80usize, 40, 24] {
            let copied = copy_item(&s, width, index);
            assert_eq!(copied, format!("run\n{long}"), "width {width}");
        }
    }

    /// A long finished execution never buries the chat: the record paints a
    /// bounded window at the live edge and counts the rows above it. The rows
    /// are not folded — scrolling reaches every one of them.
    #[test]
    fn a_long_shell_result_is_a_bounded_window_instead_of_a_fold() {
        let mut s = dark_state();
        let output: String = (0..100).map(|i| format!("line-{i:03}\n")).collect();
        let index = push_shell(&mut s, "seq 0 99", &output);
        let (plain, _) = s.conversation_plain(80);
        assert!(
            !plain.iter().any(|line| line.contains("line-000")),
            "the window does not lay out the whole head"
        );
        assert!(
            plain.iter().any(|line| line.contains("line-099")),
            "the tail is painted"
        );
        assert!(
            plain.iter().any(|line| line.contains("上方还有 88 行")),
            "what is above the window is counted, never hidden silently"
        );
        assert!(
            !plain.iter().any(|line| line.contains("已折叠")),
            "there is no fold in this record"
        );
        assert!(
            plain.len() < 60,
            "100 output rows must not lay out 100 conversation rows: {}",
            plain.len()
        );

        // Scrolling reaches the first row and keeps it on screen.
        let width = crate::conversation::geometry::content_width(&s);
        let room = crate::render::user_shell_output_room(width);
        let total = crate::shell_render::output_rows(&output, &[], room).len();
        let max = crate::shell_render::output_max_scroll(total);
        assert!(max > 0);
        s.transcript.scroll_user_shell_output(
            &leveler_core::UserShellId::new("ush-copy"),
            -(max as isize),
            max,
        );
        let (plain, _) = s.conversation_plain(80);
        assert!(
            plain.iter().any(|line| line.contains("line-000")),
            "the head is reachable by scrolling"
        );
        assert!(
            plain.iter().any(|line| line.contains("下方还有")),
            "the rows below the window are counted"
        );
        let _ = index;
    }

    /// A user shell has no fold: writing one is refused instead of pretending
    /// the record collapsed.
    #[test]
    fn a_user_shell_rejects_a_fold_write() {
        let mut s = dark_state();
        let index = push_shell(&mut s, "seq 0 2", "0\n1\n2\n");
        assert!(
            !s.transcript
                .set_item_display(index, crate::fold::DisplayMode::Collapsed),
            "a scroll window is not a fold"
        );
    }
}

/// The applied-diff default a reader actually meets: an edit opens with its
/// complete diff, folds to one stat row on request, and never has its rows
/// trimmed by a line-count heuristic.
#[cfg(test)]
mod diff_default_tests {
    use super::*;
    use crate::state::{AppState, Boot};
    use leveler_client_protocol::{SessionId, ToolCallId, UiCommandStop};

    fn state() -> AppState {
        let mut s = AppState::new(
            crate::theme::Theme::no_color(),
            Boot {
                session_id: SessionId::new("diff"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        );
        s.size = (80, 40);
        s.conv.rect = Some((0, 2, 80, 30));
        s.transcript.push_user("change the file".into());
        s
    }

    /// The canonical diff the runtime reports for a whole-file write.
    fn whole_file_diff(path: &str, before: &str, after: &str) -> String {
        let hunks = leveler_tools::tools::applied_diff::whole_file_hunks(before, after);
        leveler_tools::tools::applied_diff::unified_diff(path, &hunks).expect("a real applied diff")
    }

    fn write(s: &mut AppState, id: &str, path: &str, before: &str, after: &str) -> String {
        s.transcript.push_tool_started(
            ToolCallId::new(id),
            "write_file".into(),
            serde_json::json!({ "path": path, "content": after }).to_string(),
            false,
            0,
            Some(0),
            None,
        );
        let diff = whole_file_diff(path, before, after);
        s.transcript.complete_tool(
            &ToolCallId::new(id),
            true,
            format!("Created {path}"),
            3,
            Some(diff.clone()),
        );
        diff
    }

    fn render(s: &AppState, width: usize) -> Vec<String> {
        s.conversation_lines(width)
            .iter()
            .map(crate::selection::line_to_plain)
            .collect()
    }

    fn content(lines: usize) -> String {
        (1..=lines)
            .map(|i| format!("    expectLine{i}()\n"))
            .collect()
    }

    /// A created file opens its whole body. The old render cut every change
    /// run to its first and last three rows, so a 103-line file showed six.
    #[test]
    fn a_created_file_opens_every_line_at_any_size() {
        for (path, lines) in [
            ("backend/internal/httpapi/cookies_test.go", 103usize),
            ("internal/httpapi/health.go", 34usize),
        ] {
            let mut s = state();
            write(&mut s, "w", path, "", &content(lines));
            s.transcript.push_note("tail".into());
            let rows = render(&s, 80);
            for i in 1..=lines {
                assert!(
                    rows.iter().any(|r| r.contains(&format!("expectLine{i}()"))),
                    "{path}: line {i} of {lines} is missing"
                );
            }
            assert!(
                rows.iter().any(|r| r.contains(&format!("{lines:>4} │ +"))),
                "{path}: the last new-file line is numbered {lines}: {rows:#?}"
            );
            assert!(
                !rows.iter().any(|r| r.contains("还有")),
                "{path}: no elision hint exists any more: {rows:#?}"
            );

            // The head names the file and keeps its status glyph.
            assert!(
                rows.iter().any(|r| r.contains(path) && r.contains('✓')),
                "{path}: head keeps glyph + file: {rows:#?}"
            );
        }
    }

    /// The collapsed row is the ONE row the reader asked for: file + `+A/-R`.
    #[test]
    fn a_folded_created_file_is_one_row_with_its_stats() {
        let mut s = state();
        write(
            &mut s,
            "w",
            "backend/internal/httpapi/cookies_test.go",
            "",
            &content(103),
        );
        s.transcript.push_note("tail".into());
        let expanded = s.conversation_lines(80).len();
        let head = s
            .command_hit_at(80, 2)
            .expect("the edit head is a fold target");
        s.transcript.toggle_call_at(head.item, head.call);
        let rows = render(&s, 80);
        assert_eq!(
            rows.iter()
                .filter(|r| r.contains("cookies_test.go"))
                .count(),
            1,
            "{rows:#?}"
        );
        let head_row = rows
            .iter()
            .find(|r| r.contains("cookies_test.go"))
            .expect("the folded head");
        assert!(head_row.contains("+103/-0"), "{head_row}");
        assert!(!head_row.contains("expectLine"), "{head_row}");
        assert!(
            s.conversation_lines(80).len() < expanded - 100,
            "folding a 103-line diff removes its rows"
        );

        // And one more toggle restores every row.
        s.transcript.toggle_call_at(head.item, head.call);
        let rows = render(&s, 80);
        assert!(
            rows.iter().any(|r| r.contains("expectLine103()")),
            "{rows:#?}"
        );
    }

    /// A removal is just as complete: every `-` row, numbered from the old
    /// file, with `+0/-N` stats.
    #[test]
    fn a_removed_file_paints_every_removal() {
        let mut s = state();
        write(&mut s, "d", "src/dead.go", &content(40), "");
        s.transcript.push_note("tail".into());
        let rows = render(&s, 80);
        for i in 1..=40 {
            assert!(
                rows.iter()
                    .any(|r| r.contains(&format!("-     expectLine{i}()"))),
                "removal {i} is missing: {rows:#?}"
            );
        }
        let head = rows
            .iter()
            .position(|r| r.contains("dead.go"))
            .expect("the head names the file");
        let hit = s.command_hit_at(80, head).expect("fold target");
        s.transcript.toggle_call_at(hit.item, hit.call);
        let folded = render(&s, 80);
        assert!(
            folded
                .iter()
                .any(|r| r.contains("dead.go") && r.contains("+0/-40")),
            "{folded:#?}"
        );
    }

    /// A successful write whose content did not change reports no diff. The
    /// request must not be re-drawn as if it had landed.
    #[test]
    fn a_no_change_write_is_not_painted_as_a_change() {
        let mut s = state();
        s.transcript.push_tool_started(
            ToolCallId::new("n"),
            "write_file".into(),
            serde_json::json!({
                "path": "src/same.rs",
                "content": "fn same() {}\n"
            })
            .to_string(),
            false,
            0,
            Some(0),
            None,
        );
        s.transcript
            .complete_tool(&ToolCallId::new("n"), true, "Replaced".into(), 2, None);
        s.transcript.push_note("tail".into());
        let rows = render(&s, 80);
        assert!(
            rows.iter().any(|r| r.contains("无可用 diff")),
            "absence is stated: {rows:#?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("+ fn same")),
            "the request is not the result: {rows:#?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("+0/-0")),
            "nothing changed, so there are no counts: {rows:#?}"
        );
        assert!(
            s.command_hit_at(80, rows.iter().position(|r| r.contains("same.rs")).unwrap())
                .is_none(),
            "an empty node has nothing to fold"
        );
    }

    /// A cancelled edit reports no diff. It is neither a success nor a change.
    #[test]
    fn a_cancelled_edit_shows_no_fabricated_diff() {
        let mut s = state();
        s.transcript.push_tool_started(
            ToolCallId::new("c"),
            "write_file".into(),
            serde_json::json!({
                "path": "src/stopped.rs",
                "content": "fn never() {}\n"
            })
            .to_string(),
            false,
            0,
            Some(0),
            None,
        );
        s.transcript.complete_command(
            &ToolCallId::new("c"),
            false,
            "cancelled".into(),
            2,
            None,
            None,
            Some(UiCommandStop::Confirmed),
        );
        s.transcript.push_note("tail".into());
        let rows = render(&s, 80);
        assert!(
            rows.iter().any(|r| r.contains("⊘")),
            "a cancelled edit keeps the cancelled glyph: {rows:#?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("never()")),
            "a request that never ran is never painted as code: {rows:#?}"
        );
    }

    /// Replay paints from the same transcript facts: a finished session's
    /// scrollback keeps the whole diff, not a preview.
    #[test]
    fn a_replayed_transcript_still_paints_the_whole_diff() {
        let mut s = state();
        write(&mut s, "w", "src/replayed.rs", "", &content(50));
        s.transcript.push_note("tail".into());
        let item = s
            .transcript
            .items()
            .iter()
            .find(|item| matches!(item, TranscriptItem::ToolGroup(_)))
            .expect("the edit group");
        let lines = crate::render::transcript_lines::item_render(
            item,
            &crate::theme::Theme::no_color(),
            80,
            false,
            crate::i18n::Locale::Zh.text(),
        );
        let rows: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
            .collect();
        for i in [1usize, 25, 50] {
            assert!(
                rows.iter().any(|r| r.contains(&format!("expectLine{i}()"))),
                "replay keeps line {i}: {rows:#?}"
            );
        }
    }

    /// A width change re-wraps every row; the top of the viewport must stay on
    /// the same transcript row, not on a different one that happens to share
    /// its line number.
    #[test]
    fn a_reflow_keeps_the_reader_on_the_same_row() {
        let mut s = state();
        write(&mut s, "w", "src/long.rs", "", &content(60));
        s.transcript.push_note("tail".into());
        s.conv.auto_scroll = false;
        s.conv.scroll = 12;
        let anchor =
            crate::conversation::interaction::reading_anchor(&s).expect("a reading anchor");
        let before = crate::selection::line_to_plain(&s.conversation_lines(80)[12]);
        // The viewport re-wraps at 40 columns, then restores the anchor.
        s.conv.rect = Some((0, 2, 40, 30));
        crate::conversation::interaction::restore_reading_anchor(&mut s, Some(anchor));
        let after = crate::selection::line_to_plain(&s.conversation_lines(40)[s.conv.scroll]);
        assert_eq!(
            after.trim_end(),
            before.trim_end(),
            "the reader stays on the same content row"
        );
    }
}

/// Dragging over an edit's diff copies the patch's own code.
///
/// The painted gutter, the `+`/`-` direction marker and the leading indent are
/// decoration the renderer owns; a selection crosses them without taking them.
/// The complete unified diff is a different product entry (`/copy diff`), so a
/// selected row must never smuggle patch metadata into source text.
#[cfg(test)]
mod diff_copy_tests {
    use crate::selection::{TextPos, TextSelection, extract_selected_text_with_layout};
    use crate::state::{AppState, Boot};
    use leveler_client_protocol::{SessionId, ToolCallId};

    fn state() -> AppState {
        let mut s = AppState::new(
            crate::theme::Theme::no_color(),
            Boot {
                session_id: SessionId::new("diff-copy"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        );
        s.size = (80, 40);
        s.conv.rect = Some((0, 2, 80, 30));
        s.transcript.push_user("change the file".into());
        s
    }

    fn whole_file_diff(path: &str, before: &str, after: &str) -> String {
        let hunks = leveler_tools::tools::applied_diff::whole_file_hunks(before, after);
        leveler_tools::tools::applied_diff::unified_diff(path, &hunks).expect("a real applied diff")
    }

    fn edit(s: &mut AppState, id: &str, path: &str, before: &str, after: &str) -> String {
        s.transcript.push_tool_started(
            ToolCallId::new(id),
            "write_file".into(),
            serde_json::json!({ "path": path, "content": after }).to_string(),
            false,
            0,
            Some(0),
            None,
        );
        let diff = whole_file_diff(path, before, after);
        s.transcript.complete_tool(
            &ToolCallId::new(id),
            true,
            format!("Created {path}"),
            3,
            Some(diff.clone()),
        );
        diff
    }

    fn edit_without_diff(s: &mut AppState, id: &str, path: &str, content: &str) {
        s.transcript.push_tool_started(
            ToolCallId::new(id),
            "write_file".into(),
            serde_json::json!({ "path": path, "content": content }).to_string(),
            false,
            0,
            Some(0),
            None,
        );
        s.transcript
            .complete_tool(&ToolCallId::new(id), true, format!("Wrote {path}"), 3, None);
    }

    fn copy_rows(state: &AppState, width: usize, start: usize, end: usize) -> String {
        let (plain, layout) = state.conversation_plain(width);
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: start, col: 0 });
        sel.extend(TextPos {
            row: end.saturating_sub(1),
            col: 10_000,
        });
        sel.finish();
        extract_selected_text_with_layout(&plain, &layout, &sel)
    }

    fn copy_item(state: &AppState, width: usize, item: usize) -> String {
        let (start, end) = state.item_span(item, width).expect("item span");
        copy_rows(state, width, start, end)
    }

    fn rows(state: &AppState, width: usize) -> Vec<String> {
        state
            .conversation_lines(width)
            .iter()
            .map(crate::selection::line_to_plain)
            .collect()
    }

    const SRC: &str = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";

    #[test]
    fn an_expanded_diff_copies_the_patch_source_not_the_gutter() {
        let mut s = state();
        edit(&mut s, "e1", "src/main.rs", "", SRC);
        let item = s.transcript.items().len() - 1;
        let painted = rows(&s, 80);
        assert!(
            painted.iter().any(|r| r.contains("│ +")),
            "the diff is expanded by default: {painted:#?}"
        );
        let copied = copy_item(&s, 80, item);
        assert_eq!(copied, SRC.trim_end(), "copied: {copied:?}");
        assert!(!copied.contains('│'), "no gutter: {copied:?}");
        assert!(!copied.contains("+ "), "no direction marker: {copied:?}");
        assert!(
            !copied.contains("写入文件") && !copied.contains("main.rs"),
            "the node's head row is chrome, not source: {copied:?}"
        );
    }

    #[test]
    fn a_modified_hunk_copies_removals_and_additions_as_source_lines() {
        let mut s = state();
        edit(
            &mut s,
            "e1",
            "src/main.rs",
            "fn main() {\n    let a = 1;\n}\n",
            "fn main() {\n    let a = 2;\n}\n",
        );
        let item = s.transcript.items().len() - 1;
        let copied = copy_item(&s, 80, item);
        // A unified hunk interleaves old and new side; the selection yields the
        // lines exactly as the patch spells them, minus the marker columns.
        assert_eq!(
            copied, "fn main() {\n    let a = 1;\n    let a = 2;\n}",
            "{copied:?}"
        );
        assert!(!copied.contains('│'), "{copied:?}");
        assert!(!copied.contains("- "), "{copied:?}");
        assert!(!copied.contains("+ "), "{copied:?}");
    }

    #[test]
    fn a_soft_wrapped_diff_line_rejoins_without_inserting_characters() {
        let long = "    let value = call_something(alpha, beta, gamma, delta, epsilon, zeta) + another_call();";
        let mut s = state();
        edit(&mut s, "e1", "src/main.rs", "", &format!("{long}\n"));
        let item = s.transcript.items().len() - 1;
        for width in [80usize, 48, 32, 24] {
            let copied = copy_item(&s, width, item);
            assert_eq!(copied, long, "width {width} lost or gained characters");
        }
    }

    #[test]
    fn a_cross_hunk_selection_keeps_the_lines_in_order_and_separate() {
        let before: String = (0..40).map(|i| format!("line{i}\n")).collect();
        let mut after = before.clone();
        after = after.replace("line2\n", "line2 changed\n");
        after = after.replace("line30\n", "line30 changed\n");
        let mut s = state();
        let diff = edit(&mut s, "e1", "src/main.rs", &before, &after);
        assert_eq!(diff.matches("@@").count(), 2, "two hunks: {diff}");
        let item = s.transcript.items().len() - 1;
        let copied = copy_item(&s, 80, item);
        let lines: Vec<&str> = copied.lines().collect();
        let first = lines
            .iter()
            .position(|l| l.trim() == "line2 changed")
            .expect("first hunk copied");
        let second = lines
            .iter()
            .position(|l| l.trim() == "line30 changed")
            .expect("second hunk copied");
        assert!(second > first, "hunks keep their own order: {copied:?}");
        // The separator between hunks ends the previous line: two unrelated
        // source lines are never glued into one.
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("line2 changedline30 changed")),
            "hunks merged: {copied:?}"
        );
        assert!(!copied.contains('⋯'), "the separator is chrome: {copied:?}");
        assert!(
            !copied.contains("@@"),
            "hunk headers are chrome: {copied:?}"
        );
    }

    #[test]
    fn a_collapsed_edit_copies_no_source_text() {
        let mut s = state();
        edit(&mut s, "e1", "src/main.rs", "", SRC);
        let item = s.transcript.items().len() - 1;
        s.transcript.set_call_expanded(item, 0, false);
        let painted = rows(&s, 80);
        assert!(
            !painted.iter().any(|r| r.contains("│ +")),
            "folded: {painted:#?}"
        );
        let copied = copy_item(&s, 80, item);
        assert_eq!(copied, "", "a folded node carries no code: {copied:?}");
    }

    #[test]
    fn an_edit_without_an_applied_diff_copies_no_invented_code() {
        let mut s = state();
        // A successful write that reported no patch: the request must never be
        // painted (or copied) as an applied change.
        edit_without_diff(&mut s, "e1", "src/main.rs", "fn not_a_change() {}\n");
        let item = s.transcript.items().len() - 1;
        let painted = rows(&s, 80);
        assert!(
            painted
                .iter()
                .any(|r| r.contains(crate::i18n::Locale::Zh.text().diff_unavailable)),
            "the absence is stated: {painted:#?}"
        );
        let copied = copy_item(&s, 80, item);
        assert_eq!(copied, "", "{copied:?}");
    }

    #[test]
    fn created_modified_and_deleted_file_patches_copy_their_own_text() {
        // created
        let mut s = state();
        edit(
            &mut s,
            "e1",
            "src/new.rs",
            "",
            "pub fn a() {}\npub fn b() {}\n",
        );
        let item = s.transcript.items().len() - 1;
        assert_eq!(copy_item(&s, 80, item), "pub fn a() {}\npub fn b() {}");

        // deleted
        let mut s = state();
        edit(
            &mut s,
            "e1",
            "src/old.rs",
            "pub fn a() {}\npub fn b() {}\n",
            "",
        );
        let item = s.transcript.items().len() - 1;
        assert_eq!(copy_item(&s, 80, item), "pub fn a() {}\npub fn b() {}");
    }

    #[test]
    fn two_edits_of_one_file_copy_only_the_node_they_belong_to() {
        let mut s = state();
        edit(&mut s, "e1", "src/one.rs", "", "first_file\n");
        s.transcript.push_note("sealed".into());
        edit(&mut s, "e2", "src/two.rs", "", "second_file\n");
        let second = s.transcript.items().len() - 1;
        let copied = copy_item(&s, 80, second);
        assert_eq!(copied, "second_file");
        assert!(!copied.contains("first_file"), "{copied:?}");
    }

    /// A drag that starts inside the gutter still copies whole code: the
    /// gutter columns are decoration, and a selection that begins in them
    /// reaches the first code cell.
    #[test]
    fn a_drag_that_starts_in_the_gutter_copies_code_from_its_first_cell() {
        const SRC2: &str = "alpha beta\ngamma delta\n";
        let mut s = state();
        edit(&mut s, "e1", "src/g.rs", "", SRC2);
        let item = s.transcript.items().len() - 1;
        let (start, _) = s.item_span(item, 80).expect("span");
        let (plain, layout) = s.conversation_plain(80);
        // The head row is `start`; the code rows follow it.
        let first_code = start + 1;
        let second_code = start + 2;
        assert!(
            plain[first_code].contains("alpha beta"),
            "{:?}",
            plain[first_code]
        );
        let mut sel = TextSelection::default();
        // Column 2 is inside the `    ` lead + gutter decoration.
        sel.begin(TextPos {
            row: first_code,
            col: 2,
        });
        // The end column is a SCREEN column, so it must reach past the same
        // decoration on the last row to cover any code there.
        sel.extend(TextPos {
            row: second_code,
            col: usize::from(layout[second_code].indent) + 5,
        });
        sel.finish();
        let copied = extract_selected_text_with_layout(&plain, &layout, &sel);
        assert_eq!(copied, "alpha beta\ngamma", "{copied:?}");
    }

    /// A drag inside one code line copies exactly the cells it covers, with no
    /// decoration on either side.
    #[test]
    fn a_partial_selection_inside_a_diff_row_copies_only_those_cells() {
        const SRC2: &str = "alpha beta\ngamma delta\n";
        let mut s = state();
        edit(&mut s, "e1", "src/g.rs", "", SRC2);
        let item = s.transcript.items().len() - 1;
        let (start, _) = s.item_span(item, 80).expect("span");
        let (plain, layout) = s.conversation_plain(80);
        let row = start + 1;
        let indent = usize::from(layout[row].indent);
        assert_eq!(indent, 10, "the painted gutter width");
        let mut sel = TextSelection::default();
        sel.begin(TextPos {
            row,
            col: indent + 6,
        });
        sel.extend(TextPos {
            row,
            col: indent + 10,
        });
        sel.finish();
        let copied = extract_selected_text_with_layout(&plain, &layout, &sel);
        assert_eq!(copied, "beta", "{copied:?}");
    }

    /// A tabbed diff line copies the expanded columns it paints.
    #[test]
    fn a_tabbed_diff_line_copies_the_expanded_columns_it_paints() {
        let mut s = state();
        edit(&mut s, "e1", "src/tabs.rs", "", "\tlet a = 1;\n");
        let item = s.transcript.items().len() - 1;
        let copied = copy_item(&s, 80, item);
        assert_eq!(copied, "    let a = 1;");
        assert!(!copied.contains('\t'), "{copied:?}");
    }

    /// The archived plan block carries the user's explicit pause, so an ended
    /// turn does not look like an unexplained unfinished plan.
    #[test]
    fn an_archived_plan_stopped_by_the_user_says_so() {
        use leveler_client_protocol::{PlanStepStatus, UiExecutionTask, UiPlan, UiPlanStep};
        let mut s = state();
        s.execution_goal = Some(UiExecutionTask {
            goal_id: "g1".into(),
            objective: "阶段 1 完成后停下，等待我确认再执行阶段 2。".into(),
            running: false,
            user_paused: true,
        });
        s.transcript
            .push_user("阶段 1 完成后停下，等待我确认再执行阶段 2。".into());
        s.transcript.push_plan(UiPlan {
            steps: vec![
                UiPlanStep {
                    index: 0,
                    description: "阶段 1：实现接口".into(),
                    status: PlanStepStatus::Done,
                },
                UiPlanStep {
                    index: 1,
                    description: "阶段 2：写测试".into(),
                    status: PlanStepStatus::Pending,
                },
            ],
            state: Default::default(),
        });
        let conversation = |s: &AppState| {
            crate::conversation::build::build_conversation_lines(s, 100)
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let text = conversation(&s);
        assert!(text.contains("1/2"), "{text}");
        assert!(text.contains("等待用户确认"), "{text}");
        assert!(!text.contains("最后记录"), "{text}");

        // Another goal's history is not this plan's authorization: the pause
        // is projected per goal, so an unpaused goal leaves the plan neutral.
        s.execution_goal = Some(UiExecutionTask {
            goal_id: "g2".into(),
            objective: "实现新的数据导出功能，一次完成。".into(),
            running: false,
            user_paused: false,
        });
        let text = conversation(&s);
        assert!(text.contains("1/2"), "{text}");
        assert!(!text.contains("等待用户确认"), "{text}");
    }
}
