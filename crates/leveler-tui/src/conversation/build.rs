//! Conversation build: transcript → wrapped display lines
//! plus the disclosure hit rows, memoized under one cache key.
//!
//! The hit rows are built in the same pass and cached in the same entry as
//! the lines — a hit can never describe stale lines. This invariant is what
//! makes mouse disclosure clicks trustworthy; do not split the caches.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render::{item_render, items_need_gap, sub_agent_tree_lines};
use crate::state::AppState;
use crate::transcript::TranscriptItem;

use super::ConvKey;

/// How many lines the conversation content needs at `width` (for scroll math).
pub fn conversation_line_count(state: &AppState, width: usize) -> usize {
    state.conversation_lines(width).len()
}

fn wrap_simple(s: &str, width: usize) -> Vec<String> {
    crate::render::wrap(s, width)
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
        let (lines, hits, _, _, _) = self.conversation_build(width);
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

    /// The memoized build: lines, disclosure hit rows, command rows, the
    /// last Final answer's start line, and every item's line span.
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
    ) {
        if crate::splash::conversation_is_empty(self) {
            let (lines, hits, commands, anchor, starts) = build_conversation(self, width);
            return (
                std::rc::Rc::new(lines),
                std::rc::Rc::new(hits),
                std::rc::Rc::new(commands),
                anchor,
                std::rc::Rc::new(starts),
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
        };
        if let Some((k, lines, hits, commands, anchor, starts)) = self.conv.cache.borrow().as_ref()
            && *k == key
        {
            crate::profile::add("tui.build_cache_hit_count", 1);
            return (
                lines.clone(),
                hits.clone(),
                commands.clone(),
                *anchor,
                starts.clone(),
            );
        }
        let started = crate::profile::start();
        let (lines, hits, commands, anchor, starts) = build_conversation(self, width);
        crate::profile::stop(started, "tui.projection_ms");
        crate::profile::add("tui.build_count", 1);
        let (lines, hits, commands, starts) = (
            std::rc::Rc::new(lines),
            std::rc::Rc::new(hits),
            std::rc::Rc::new(commands),
            std::rc::Rc::new(starts),
        );
        *self.conv.cache.borrow_mut() = Some((
            key,
            lines.clone(),
            hits.clone(),
            commands.clone(),
            anchor,
            starts.clone(),
        ));
        (lines, hits, commands, anchor, starts)
    }

    /// The command row under content (`abs_line`, display `col`), if any.
    pub(crate) fn command_hit_at(
        &self,
        width: usize,
        abs_line: usize,
    ) -> Option<super::view::CommandHit> {
        let (_, _, commands, _, _) = self.conversation_build(width);
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
    let (lines, hits, _, _, _) = build_conversation(state, width);
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
) {
    let theme = &state.theme;
    let t = state.t();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<(usize, usize)> = Vec::new();
    let mut commands: Vec<super::view::CommandHit> = Vec::new();
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
        return (
            crate::splash::splash_lines(state, width, height, theme, t),
            hits,
            commands,
            final_anchor,
            item_spans,
        );
    }

    let env = super::view::UnitEnv {
        width,
        theme_id: state.theme.id,
        monochrome: state.theme.monochrome,
        locale: state.locale,
        tools_expanded: state.tools_expanded,
    };
    // Take the previous per-item memo so unchanged units can be moved forward
    // instead of re-wrapped. The borrow is released before any rendering runs.
    let prev = std::mem::take(&mut *state.conv.item_cache.borrow_mut());
    let mut prev_units = prev.units.into_iter();
    let mut new_units: Vec<super::view::CachedUnit> = Vec::new();

    let items = state.transcript.items();
    let folds = plan_exploration_folds(items);
    let mut idx = 0;
    while idx < items.len() {
        let item = &items[idx];
        // A view-time exploration run paints as ONE unit: the aggregate
        // receipt, plus whatever its current fold state reveals. Nothing is
        // merged — the run's items stay in the transcript and opening the fold
        // restores them in their real order.
        if let Some(run) = folds.owner[idx] {
            let fold = &folds.runs[run];
            debug_assert_eq!(fold.start, idx, "a run is entered at its start");
            if idx > 0 && items_need_gap(&items[idx - 1], item) {
                out.push(Line::from(""));
            }
            let before_item = out.len();
            render_exploration_fold(
                state,
                fold,
                &mut out,
                &mut hits,
                &mut commands,
                width,
                theme,
                t,
            );
            // The memo is positional over cacheable items: consume one stale
            // unit per cacheable item the fold covers and push a
            // never-matching placeholder, so items AFTER the fold keep their
            // cache slot instead of re-wrapping on every frame.
            for covered in &items[fold.start..fold.end] {
                if !matches!(
                    covered,
                    TranscriptItem::ToolGroup(_)
                        | TranscriptItem::SubAgent(_)
                        | TranscriptItem::UserShell(_)
                ) {
                    let _ = prev_units.next();
                    new_units.push(super::view::CachedUnit {
                        items: Vec::new(),
                        env,
                        lines: std::rc::Rc::new(Vec::new()),
                        hits: Vec::new(),
                        commands: Vec::new(),
                    });
                }
            }
            item_spans[fold.start..fold.end].fill((before_item, out.len()));
            idx = fold.end;
            continue;
        }
        // Remember where this item starts: a group whose tools are all Silent
        // (ls / probe runs) renders nothing, and a separator emitted before it
        // would leave a blank gap with no content — the reader sees a hole
        // between their prompt and the answer. The gap is undone below if the
        // item turned out to be invisible.
        let gap_at = (idx > 0 && items_need_gap(&items[idx - 1], item)).then(|| {
            out.push(Line::from(""));
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
        // `▌` user prompt, `●` agent prose, status glyphs for tool activity.
        //
        // A ToolGroup, a SubAgent run and a UserShell read live state (the turn
        // clock, approval hold, command focus), so they are rendered fresh and
        // are NOT part of the per-item memo. Everything else is memoized.
        let cacheable = !matches!(
            item,
            TranscriptItem::ToolGroup(_)
                | TranscriptItem::SubAgent(_)
                | TranscriptItem::UserShell(_)
        );
        if cacheable {
            let reused = prev_units.next();
            let (lines, unit_hits, unit_commands, unit_items) = match reused {
                Some(unit)
                    if unit.env == env && unit.items.len() == 1 && &unit.items[0] == item =>
                {
                    (unit.lines, unit.hits, unit.commands, unit.items)
                }
                _ => {
                    let (lines, unit_hits, unit_commands) =
                        render_cacheable_unit(state, item, width);
                    (
                        std::rc::Rc::new(lines),
                        unit_hits,
                        unit_commands,
                        vec![item.clone()],
                    )
                }
            };
            for (rel, k) in &unit_hits {
                hits.push((out.len() + rel, idx + k));
            }
            for c in &unit_commands {
                commands.push(super::view::CommandHit {
                    line: out.len() + c.line,
                    item: idx + c.item_offset,
                    call: c.call,
                    stoppable: c.stoppable,
                });
            }
            out.extend(lines.iter().cloned());
            new_units.push(super::view::CachedUnit {
                items: unit_items,
                env,
                lines,
                hits: unit_hits,
                commands: unit_commands,
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
                    ));
                    commands.extend(rows.into_iter().map(|row| super::view::CommandHit {
                        line: base + row.line,
                        item: idx,
                        call: row.call,
                        stoppable: row.stoppable,
                    }));
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
                    out.extend(sub_agent_tree_lines(
                        &blocks,
                        theme,
                        width,
                        t,
                        state.elapsed_secs,
                    ));
                }
                TranscriptItem::UserShell(shell) => {
                    // Every user shell row is a click target: click opens its
                    // Shell Details (running or finished) — the reducer
                    // dispatches by item type, geometry stays untouched.
                    hits.push((out.len(), idx));
                    out.extend(crate::render::user_shell_lines(
                        shell,
                        theme,
                        width,
                        t,
                        state.elapsed_secs,
                    ));
                }
                _ => unreachable!("non-cacheable item handled above"),
            }
        }
        // The item produced nothing (an all-Silent tool group): take the
        // separator back so no hole is left behind.
        if out.len() == before_item
            && let Some(at) = gap_at
        {
            out.remove(at);
        }
        // One span per consumed item. A sub-agent run (or any future multi-item
        // unit) renders several items as one block, so they all share it.
        item_spans[first_item..=idx].fill((before_item, out.len()));
        idx += 1;
    }

    *state.conv.item_cache.borrow_mut() = super::view::ItemLineCache { units: new_units };
    (out, hits, commands, final_anchor, item_spans)
}

// ───────────────────────────────────────────────────────────────────────────
// View-time exploration folds
// ───────────────────────────────────────────────────────────────────────────
//
// A provider that reasons between tool calls makes the transcript alternate
// `Thought / ToolGroup / Thought / ToolGroup`, which paints a wall of `◆`
// headers over one-row reads. The fix is PRESENTATION ONLY: a chronological run
// of finished collapsed Thoughts and settled non-destructive exploration groups
// is painted as ONE aggregate receipt until the reader opens it. Nothing is
// merged or reordered — the same semantic items stay in the transcript, and
// opening the fold restores them in their real order.

/// One item inside a view-time exploration run.
#[derive(Debug)]
struct FoldSlot {
    item: usize,
    /// Hidden while the fold is collapsed. A participant is a finished
    /// collapsed Thought or a settled exploration group; a transparent slot is
    /// one the reader already opened and must keep seeing.
    participant: bool,
}

/// One derived exploration run. `end` is exclusive.
#[derive(Debug)]
struct ExplorationFold {
    /// The run's first exploration group — the toggle target and the run's
    /// stable identity. Its first call's id keys the view-time fold set.
    anchor: usize,
    start: usize,
    end: usize,
    slots: Vec<FoldSlot>,
    /// Every exploration call the receipt counts, as (item index, call index).
    members: Vec<(usize, usize)>,
}

#[derive(Debug, Default)]
struct ExplorationFolds {
    /// For each transcript item, the run index it belongs to.
    owner: Vec<Option<usize>>,
    runs: Vec<ExplorationFold>,
}

/// How one transcript item behaves inside an exploration run.
enum FoldClass {
    /// A settled exploration group that the folded receipt speaks for.
    ParticipantGroup,
    /// A finished, default-collapsed Thought the receipt folds over.
    ParticipantThought,
    /// A member the reader already opened: kept visible, never hidden.
    Transparent,
    /// Anything else ends the run.
    Break,
}

fn classify_fold_item(item: &TranscriptItem) -> FoldClass {
    match item {
        TranscriptItem::Thought(block) if block.done => {
            if block.display.is_collapsed() {
                FoldClass::ParticipantThought
            } else {
                // A Thought the reader opened keeps its body visible. It does
                // not split the run: the fold still speaks for its groups.
                FoldClass::Transparent
            }
        }
        // A live Thought is the tail itself; a run never folds around it.
        TranscriptItem::Thought(_) => FoldClass::Break,
        TranscriptItem::ToolGroup(group)
            if crate::activity_stream::group_is_exploration_fold_member(group) =>
        {
            // The run's fold state is the fold SET, not the group's own
            // drawer: a member that was opened before the fold formed is still
            // claimed by the receipt, and its drawer is restored when the
            // receipt is opened.
            FoldClass::ParticipantGroup
        }
        _ => FoldClass::Break,
    }
}

/// Derive every view-time exploration run in one pass.
///
/// A run folds only when it holds at least one exploration group and counts at
/// least two exploration calls: a lone read keeps its own target row, and a
/// run of only Thoughts has an empty label and nothing to compact. When a
/// candidate run does not fold, the whole candidate is skipped — no sub-run of
/// it can have more members, so none can fold either.
fn plan_exploration_folds(items: &[TranscriptItem]) -> ExplorationFolds {
    let mut plan = ExplorationFolds {
        owner: vec![None; items.len()],
        runs: Vec::new(),
    };
    let mut i = 0;
    while i < items.len() {
        let mut slots: Vec<FoldSlot> = Vec::new();
        let mut members: Vec<(usize, usize)> = Vec::new();
        let mut anchor: Option<usize> = None;
        let mut j = i;
        while j < items.len() {
            match classify_fold_item(&items[j]) {
                FoldClass::ParticipantGroup => {
                    if let Some(TranscriptItem::ToolGroup(group)) = items.get(j) {
                        for (ci, call) in group.calls.iter().enumerate() {
                            if crate::activity_stream::is_exploration_fold_call(call) {
                                members.push((j, ci));
                            }
                        }
                    }
                    anchor.get_or_insert(j);
                    slots.push(FoldSlot {
                        item: j,
                        participant: true,
                    });
                }
                FoldClass::ParticipantThought => slots.push(FoldSlot {
                    item: j,
                    participant: true,
                }),
                FoldClass::Transparent => slots.push(FoldSlot {
                    item: j,
                    participant: false,
                }),
                FoldClass::Break => break,
            }
            j += 1;
        }
        match anchor {
            Some(anchor) if members.len() >= 2 && j > i => {
                let run = plan.runs.len();
                for k in i..j {
                    plan.owner[k] = Some(run);
                }
                plan.runs.push(ExplorationFold {
                    anchor,
                    start: i,
                    end: j,
                    slots,
                    members,
                });
                i = j;
            }
            _ => i = if j > i { j } else { i + 1 },
        }
    }
    plan
}

/// The stable identity of a run: its anchor group's first exploration call.
fn fold_anchor_key(
    items: &[TranscriptItem],
    fold: &ExplorationFold,
) -> Option<leveler_client_protocol::ToolCallId> {
    let TranscriptItem::ToolGroup(group) = items.get(fold.anchor)? else {
        return None;
    };
    crate::activity_stream::group_exploration_fold_calls(group)
        .first()
        .map(|call| call.id.clone())
}

/// Whether a view-time exploration fold is open. Presentation-only state kept
/// on the conversation view, keyed by the run's anchor call id.
impl crate::conversation::ConversationView {
    pub(crate) fn exploration_fold_expanded(
        &self,
        key: &leveler_client_protocol::ToolCallId,
    ) -> bool {
        self.exploration_folds.borrow().contains(key)
    }
}

/// Toggle the view-time exploration fold whose receipt is `item`, if any.
/// Returns the new expanded state, or `None` when the item is not a run's
/// anchor.
///
/// Only the run's ANCHOR (its first exploration group) opens or closes the
/// fold. A Thought or member row inside an open fold keeps its own fold: a
/// click there must reveal reasoning, never collapse the run around it.
///
/// Kept beside the planner so the run the reader clicked is resolved by the
/// same derivation that painted it — the hit can never describe another run.
pub(crate) fn toggle_exploration_fold(state: &mut AppState, item: usize) -> Option<bool> {
    let items = state.transcript.items();
    let plan = plan_exploration_folds(items);
    let run = plan.owner.get(item).copied().flatten()?;
    let fold = plan.runs.get(run)?;
    if fold.anchor != item {
        return None;
    }
    let key = fold_anchor_key(items, fold)?;
    let expanded = {
        let mut folds = state.conv.exploration_folds.borrow_mut();
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
    state.conv.plain_width = 0;
    Some(expanded)
}

/// Paint one view-time exploration run as a unit: the aggregate receipt, then
/// the members the current fold state reveals.
#[allow(clippy::too_many_arguments)]
fn render_exploration_fold(
    state: &AppState,
    fold: &ExplorationFold,
    out: &mut Vec<Line<'static>>,
    hits: &mut Vec<(usize, usize)>,
    commands: &mut Vec<super::view::CommandHit>,
    width: usize,
    theme: &crate::theme::Theme,
    t: &crate::i18n::UiText,
) {
    let items = state.transcript.items();
    let expanded =
        fold_anchor_key(items, fold).is_some_and(|key| state.conv.exploration_fold_expanded(&key));
    let members: Vec<&crate::transcript::ToolCallBlock> = fold
        .members
        .iter()
        .filter_map(|(i, c)| match items.get(*i) {
            Some(TranscriptItem::ToolGroup(group)) => group.calls.get(*c),
            _ => None,
        })
        .collect();
    // The receipt IS the fold's disclosure row: its click toggles the run.
    hits.push((out.len(), fold.anchor));
    out.push(crate::activity_stream::exploration_fold_receipt_line(
        &members, theme, width, t, expanded,
    ));
    let body_start = out.len();
    let child_width = width
        .saturating_sub(crate::activity_stream::GROUP_BODY_INDENT.len())
        .max(1);
    for slot in &fold.slots {
        // A collapsed fold shows nothing of its participants: the receipt
        // already speaks for them. Transparent slots stay visible either way.
        if !expanded && slot.participant {
            continue;
        }
        match items.get(slot.item) {
            Some(TranscriptItem::Thought(block)) => {
                hits.push((out.len(), slot.item));
                out.extend(crate::render::thought_lines(block, theme, child_width, t));
            }
            Some(TranscriptItem::ToolGroup(group)) if slot.participant => {
                let calls = crate::activity_stream::group_exploration_fold_calls(group);
                out.extend(crate::activity_stream::exploration_fold_member_lines(
                    &calls,
                    theme,
                    child_width,
                    state.locale,
                    t,
                    state.elapsed_secs,
                    state.approval_gated_call(),
                ));
            }
            Some(TranscriptItem::ToolGroup(group)) => {
                // A member the reader already opened keeps its own drawer.
                if crate::activity_stream::group_has_disclosure(group) {
                    hits.push((out.len(), slot.item));
                }
                let base = out.len();
                let mut rows = Vec::new();
                out.extend(crate::activity_stream::render_activity(
                    group,
                    theme,
                    child_width,
                    state.locale,
                    t,
                    state.elapsed_secs,
                    state.approval_gated_call(),
                    state.focused_command(),
                    &mut rows,
                ));
                commands.extend(rows.into_iter().map(|row| super::view::CommandHit {
                    line: base + row.line,
                    item: slot.item,
                    call: row.call,
                    stoppable: row.stoppable,
                }));
            }
            _ => {}
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
fn render_cacheable_unit(
    state: &AppState,
    item: &TranscriptItem,
    width: usize,
) -> (
    Vec<Line<'static>>,
    Vec<(usize, usize)>,
    Vec<super::view::CachedCommand>,
) {
    let theme = &state.theme;
    let t = state.t();
    let mut hits: Vec<(usize, usize)> = Vec::new();
    let mut out: Vec<Line<'static>> = Vec::new();
    match item {
        TranscriptItem::User(text) => {
            // A solid heading bar + bold body marks the user's turn clearly
            // apart from the assistant's `●` bullet and normal-weight prose.
            let bar = Style::default()
                .fg(theme.text.primary)
                .add_modifier(Modifier::BOLD);
            let body = Style::default()
                .fg(theme.text.primary)
                .add_modifier(Modifier::BOLD);
            for line in wrap_simple(text, width.saturating_sub(2).max(1)) {
                out.push(Line::from(vec![
                    Span::styled("▌ ", bar),
                    Span::styled(line, body),
                ]));
            }
        }
        TranscriptItem::Assistant(block) => {
            // Prose is communication: always in full, never a click target.
            out.extend(crate::render::assistant_render(block, theme, width));
        }
        TranscriptItem::GoalRecap(_) | TranscriptItem::MemoryList(_) => {
            // A goal recap's first line is its disclosure row: click expands the
            // persisted checkpoint's structured sections. A memory listing uses
            // the same interaction for its details.
            hits.push((0, 0));
            out.extend(item_render(item, theme, width, state.tools_expanded, t));
        }
        TranscriptItem::Thought(block) => {
            // A Thought's header is its disclosure row, exactly like a tool
            // group's first row: click folds or opens the reasoning body.
            // A provider that returned no text owns nothing to reveal, so its
            // header stays a plain row rather than a fold that does nothing.
            if !block.text.is_empty() {
                hits.push((0, 0));
            }
            out.extend(item_render(item, theme, width, state.tools_expanded, t));
        }
        other => out.extend(item_render(other, theme, width, state.tools_expanded, t)),
    }
    (out, hits, Vec::new())
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

    #[test]
    fn expanding_a_memory_list_rebuilds_that_item() {
        let mut s = test_state();
        s.transcript.push_user("hi".into());
        s.transcript
            .push_memory_list("summary line".into(), "detail line".into());
        let collapsed = build_conversation(&s, 60).0;
        {
            let items = s.transcript.items_mut();
            let last = items.last_mut().expect("memory item");
            if let TranscriptItem::MemoryList(block) = last {
                block.expanded = true;
            }
        }
        let expanded = build_conversation(&s, 60).0;
        assert!(
            expanded.len() > collapsed.len(),
            "expanding must add the detail line"
        );
    }

    /// A height-only resize can no longer truncate a confirmed diff. The Full
    /// Diff contract paints every change row at any viewport, so the same
    /// width projects the same number of lines whether the viewport is short
    /// or tall — and the last change row is always present.
    #[test]
    fn a_confirmed_diff_is_complete_at_any_viewport_height() {
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
            "a confirmed diff is never viewport-truncated: {short} -> {tall}"
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
            "the last change row is painted: {plain:?}"
        );
        assert!(
            !plain.iter().any(|r| r.contains("还有")),
            "no preview marker replaced a hunk"
        );
    }
}

/// View-time exploration folds: the presentation contracts the Conversation
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

    fn receipts(lines: &[String]) -> Vec<&String> {
        lines
            .iter()
            .filter(|l| l.starts_with('\u{25b8}') || l.starts_with('\u{25be}'))
            .collect()
    }

    /// The index of the run's anchor: its first exploration group.
    fn first_group_index(s: &AppState) -> usize {
        s.transcript
            .items()
            .iter()
            .position(|it| matches!(it, TranscriptItem::ToolGroup(_)))
            .expect("a tool group")
    }

    /// THOUGHT-IA-4/8: a click on a Thought row inside an OPEN fold opens its
    /// reasoning body; it never collapses the run around it.
    #[test]
    fn clicking_a_thought_inside_an_open_fold_opens_its_body() {
        let mut s = boot();
        thought(&mut s, "first");
        read(&mut s, "r1", "a.rs", 0);
        thought(&mut s, "open this body");
        read(&mut s, "r2", "b.rs", 1);
        seal(&mut s);
        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        assert!(render(&s).iter().any(|l| l.starts_with('\u{25be}')));

        // The second Thought is item 3 (user=0, t=1, group=2, t=3).
        crate::conversation::interaction::toggle_fold(&mut s, 3);
        let after = render(&s);
        assert!(
            after.iter().any(|l| l.starts_with('\u{25be}')),
            "the fold stays open: {after:#?}"
        );
        assert!(
            after.iter().any(|l| l.contains("open this body")),
            "the reasoning body opened: {after:#?}"
        );
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
        read(&mut s, "r2", "config.rs", 1);
        search(&mut s, "g1", "missing model", 2);
        thought(&mut s, "almost done");

        let lines = render(&s);
        let found = receipts(&lines);
        assert_eq!(found.len(), 1, "one aggregate receipt: {lines:#?}");
        assert_eq!(
            found[0].trim_end(),
            "\u{25b8} 读取 2 个文件 \u{b7} 搜索 1 次"
        );
        assert!(
            !lines.iter().any(|l| l.contains("已思考")),
            "no Thought header while folded: {lines:#?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("README.md") || l.contains("config.rs")),
            "member targets are hidden while folded: {lines:#?}"
        );
    }

    /// THOUGHT-IA-7 / SEMANTIC-IA-3 / TOOL-IA-5: opening the receipt restores
    /// the REAL chronology — each Thought and each member row in its original
    /// order. Nothing was merged away.
    #[test]
    fn expanding_a_run_restores_the_real_chronology() {
        let mut s = boot();
        thought(&mut s, "first thought");
        read(&mut s, "r1", "a.rs", 0);
        thought(&mut s, "second thought");
        read(&mut s, "r2", "b.rs", 1);
        search(&mut s, "g1", "missing model", 2);
        seal(&mut s);

        let collapsed = render(&s);
        assert!(collapsed.iter().any(|l| l.contains("读取 2 个文件")));

        let anchor = first_group_index(&s);
        crate::conversation::interaction::toggle_fold(&mut s, anchor);
        let open = render(&s);
        assert!(open.len() > collapsed.len(), "the fold added rows");
        assert_eq!(
            open.iter().filter(|l| l.starts_with('\u{25be}')).count(),
            1,
            "the receipt stays and opens: {open:#?}"
        );
        let receipt = position(&open, "\u{25be}");
        let thoughts: Vec<usize> = open
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains("已思考"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(thoughts.len(), 2, "both Thoughts are restored: {open:#?}");
        let (t1, t2) = (thoughts[0], thoughts[1]);
        let a = position(&open, "a.rs");
        let b = position(&open, "b.rs");
        let g = position(&open, "missing model");
        assert!(
            receipt < t1 && t1 < a && a < t2 && t2 < b && b < g,
            "chronology restored: {open:#?}"
        );
    }

    /// THOUGHT-IA-8: a Thought the reader opened keeps its body and is never
    /// absorbed back into the fold.
    #[test]
    fn an_opened_thought_stays_visible_inside_the_fold() {
        let mut s = boot();
        thought(&mut s, "first");
        read(&mut s, "r1", "a.rs", 0);
        thought(&mut s, "keep me visible");
        read(&mut s, "r2", "b.rs", 1);
        seal(&mut s);
        // User opens the second Thought (item 3: user, t, read, t).
        s.transcript
            .set_item_display(3, crate::fold::DisplayMode::Expanded);

        let lines = render(&s);
        assert!(
            lines.iter().any(|l| l.contains("keep me visible")),
            "the opened reasoning body stays on screen: {lines:#?}"
        );
        assert_eq!(receipts(&lines).len(), 1, "the fold still forms");
    }

    /// TOOL-IA-6: a Run is its own block and breaks the exploration fold.
    #[test]
    fn a_command_breaks_the_run() {
        let mut s = boot();
        thought(&mut s, "t1");
        read(&mut s, "r1", "a.rs", 0);
        thought(&mut s, "t2");
        read(&mut s, "r2", "b.rs", 1);
        tool(
            &mut s,
            "c1",
            "run_command",
            serde_json::json!({ "program": "cargo", "args": ["test"] }),
            2,
            true,
        );
        read(&mut s, "r3", "c.rs", 3);
        search(&mut s, "g1", "x", 4);
        seal(&mut s);

        let lines = render(&s);
        assert_eq!(
            receipts(&lines).len(),
            2,
            "the Run split the run: {lines:#?}"
        );
        let run = position(&lines, "cargo test");
        let first = position(&lines, "\u{25b8}");
        let second = lines
            .iter()
            .rposition(|l| l.starts_with('\u{25b8}'))
            .expect("second receipt");
        assert!(
            first < run && run < second,
            "the Run sits between: {lines:#?}"
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
        read(&mut s, "r2", "b.rs", 1);
        tool(
            &mut s,
            "e1",
            "apply_patch",
            serde_json::json!({ "path": "a.rs" }),
            2,
            true,
        );
        read(&mut s, "r3", "c.rs", 3);
        search(&mut s, "g1", "x", 4);
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
        read(&mut s, "r2", "b.rs", 1);
        narration(&mut s, "the workspace is clean");
        read(&mut s, "r3", "c.rs", 2);
        search(&mut s, "g1", "x", 3);
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
    fn a_failed_exploration_target_is_never_folded_away() {
        let mut s = boot();
        thought(&mut s, "t1");
        read(&mut s, "r1", "a.rs", 0);
        thought(&mut s, "t2");
        read(&mut s, "r2", "b.rs", 1);
        tool(
            &mut s,
            "f1",
            "read_file",
            serde_json::json!({ "path": "missing.rs" }),
            2,
            false,
        );

        let lines = render(&s);
        assert!(
            lines.iter().any(|l| l.contains("missing.rs")),
            "the failing target is visible: {lines:#?}"
        );
        // The group holding the failure is not an exploration participant.
        assert!(
            lines.iter().any(|l| l.contains("\u{2717}")),
            "the failure glyph is present: {lines:#?}"
        );
    }

    /// TOOL-IA-3-like: a lone exploration call keeps its own target row instead
    /// of a one-item receipt.
    #[test]
    fn a_lone_exploration_call_keeps_its_own_target() {
        let mut s = boot();
        read(&mut s, "r1", "only.rs", 0);
        let lines = render(&s);
        assert!(
            receipts(&lines).is_empty(),
            "no receipt for one call: {lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("only.rs")),
            "the lone target is visible: {lines:#?}"
        );
    }

    /// SEMANTIC-IA-2 / toggling: opening and closing the fold is reversible and
    /// never reorders or loses a semantic item.
    #[test]
    fn toggling_the_receipt_is_reversible() {
        let mut s = boot();
        thought(&mut s, "t1");
        read(&mut s, "r1", "a.rs", 0);
        thought(&mut s, "t2");
        read(&mut s, "r2", "b.rs", 1);
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
