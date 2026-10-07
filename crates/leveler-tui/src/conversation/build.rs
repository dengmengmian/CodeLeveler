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
        let (lines, hits, _, _) = self.conversation_build(width);
        (lines, hits)
    }

    /// The absolute line where the last Final answer begins, for jump-to-final
    /// navigation. Reuses the same memoized build as painting, so the anchor
    /// can never describe different lines than the ones on screen.
    pub(crate) fn final_answer_anchor(&self, width: usize) -> Option<usize> {
        self.conversation_build(width).3
    }

    /// The memoized build: lines, disclosure hit rows, command rows, and the
    /// last Final answer's start line.
    #[allow(clippy::type_complexity)]
    pub(crate) fn conversation_build(
        &self,
        width: usize,
    ) -> (
        std::rc::Rc<Vec<Line<'static>>>,
        std::rc::Rc<Vec<(usize, usize)>>,
        std::rc::Rc<Vec<super::view::CommandHit>>,
        Option<usize>,
    ) {
        if crate::splash::conversation_is_empty(self) {
            let (lines, hits, commands, anchor) = build_conversation(self, width);
            return (
                std::rc::Rc::new(lines),
                std::rc::Rc::new(hits),
                std::rc::Rc::new(commands),
                anchor,
            );
        }
        let diff_preview_rows = crate::activity_stream::diff_preview_rows_for_viewport(
            super::geometry::viewport_height(self),
        );
        let key = ConvKey {
            version: self.transcript.version(),
            width,
            theme_id: self.theme.id,
            monochrome: self.theme.monochrome,
            locale: self.locale,
            tools_expanded: self.tools_expanded,
            awaiting_approval: self.approval_gated_call().cloned(),
            elapsed_secs: self.elapsed_secs,
            diff_preview_rows,
            focused_command: self.focused_command().cloned(),
        };
        if let Some((k, lines, hits, commands, anchor)) = self.conv.cache.borrow().as_ref()
            && *k == key
        {
            crate::profile::add("tui.build_cache_hit_count", 1);
            return (lines.clone(), hits.clone(), commands.clone(), *anchor);
        }
        let started = crate::profile::start();
        let (lines, hits, commands, anchor) = build_conversation(self, width);
        crate::profile::stop(started, "tui.projection_ms");
        crate::profile::add("tui.build_count", 1);
        let (lines, hits, commands) = (
            std::rc::Rc::new(lines),
            std::rc::Rc::new(hits),
            std::rc::Rc::new(commands),
        );
        *self.conv.cache.borrow_mut() =
            Some((key, lines.clone(), hits.clone(), commands.clone(), anchor));
        (lines, hits, commands, anchor)
    }

    /// The command row under content (`abs_line`, display `col`), if any.
    pub(crate) fn command_hit_at(
        &self,
        width: usize,
        abs_line: usize,
    ) -> Option<super::view::CommandHit> {
        let (_, _, commands, _) = self.conversation_build(width);
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
    let (lines, hits, _, _) = build_conversation(state, width);
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
) {
    let theme = &state.theme;
    let t = state.t();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<(usize, usize)> = Vec::new();
    let mut commands: Vec<super::view::CommandHit> = Vec::new();
    // Absolute line where the LAST Final answer begins. Tracked as items are
    // placed, so jump-to-final consumes the same projection painting does.
    let mut final_anchor: Option<usize> = None;
    let diff_preview_rows = crate::activity_stream::diff_preview_rows_for_viewport(
        super::geometry::viewport_height(state),
    );

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
    let mut idx = 0;
    while idx < items.len() {
        let item = &items[idx];
        // Remember where this item starts: a group whose tools are all Silent
        // (ls / probe runs) renders nothing, and a separator emitted before it
        // would leave a blank gap with no content — the reader sees a hole
        // between their prompt and the answer. The gap is undone below if the
        // item turned out to be invisible.
        let gap_at = (idx > 0 && items_need_gap(&items[idx - 1], item)).then(|| {
            out.push(Line::from(""));
            out.len() - 1
        });
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
                        diff_preview_rows,
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
        idx += 1;
    }

    *state.conv.item_cache.borrow_mut() = super::view::ItemLineCache { units: new_units };
    (out, hits, commands, final_anchor)
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

    /// A height-only resize (same width) must re-project the diff preview: the
    /// budget is derived from the viewport, so the cache key has to notice a
    /// changed height or it keeps painting the previous budget's truncation.
    #[test]
    fn a_height_only_resize_reprojects_the_diff_preview() {
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

        // Small viewport: the preview budget is small.
        s.conv.rect = Some((0, 2, 80, 12));
        let short = s.conversation_lines(80).len();
        // Same width, tall viewport: the budget grows and the fold marker goes.
        s.conv.rect = Some((0, 2, 80, 80));
        let tall = s.conversation_lines(80).len();

        assert!(
            tall > short,
            "a height-only resize must re-project the diff preview: {short} -> {tall}"
        );
    }
}
