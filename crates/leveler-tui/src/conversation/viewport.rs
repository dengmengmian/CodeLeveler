//! The Conversation viewport component: paints the visible window of the
//! built lines into its rect, publishes the authoritative rect for geometry,
//! and draws the scroll-to-bottom affordance. Scroll math comes from
//! `geometry`; line content from `build` — this file only paints.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::state::AppState;
use crate::transcript::TranscriptItem;

/// Animate the rail of every running Thought visible in this window.
///
/// `all` is the projection this frame painted and `item_spans` its per-item
/// line spans, so a Thought's rows come from the same build that produced the
/// window. The sweep is computed over the Thought's WHOLE rail and each
/// covered row is then painted only if the window shows it — the highlight is
/// a function of the rail, never of the viewport, so scrolling cannot move it
/// along the rail.
fn paint_live_reasoning_rail(
    state: &AppState,
    all: &[Line<'static>],
    item_spans: &[(usize, usize)],
    scroll: usize,
    lines: &mut [Line<'static>],
) {
    if !state.live_animation {
        return;
    }
    let window_end = scroll + lines.len();
    for (index, item) in state.transcript.items().iter().enumerate() {
        let TranscriptItem::Thought(block) = item else {
            continue;
        };
        // Only a RUNNING Thought animates: a completed or interrupted segment
        // has stopped, and a collapsed one paints no body to sweep.
        if block.done || block.display.is_collapsed() {
            continue;
        }
        let Some(&(start, end)) = item_spans.get(index) else {
            continue;
        };
        if end <= scroll || start >= window_end || end > all.len() {
            continue;
        }
        let mut rail_rows = crate::render::reasoning_rail_rows(&all[start..end]);
        for row in &mut rail_rows {
            *row += start;
        }
        crate::render::paint_reasoning_rail(
            lines,
            scroll,
            &rail_rows,
            &state.theme,
            state.motion_clock,
        );
    }
}

pub fn render(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let theme = &state.theme;
    let content = crate::conversation::geometry::content_rect(area);
    let width = content.width as usize;
    let height = content.height as usize;
    if height == 0 || width == 0 {
        state.conv.rect = None;
        state.conv.scroll_bottom_rect = None;
        return;
    }

    // Publish before building lines so the empty-state card can size to this
    // viewport (splash is not cached on transcript version).
    state.conv.rect = Some((content.x, content.y, content.width, content.height));

    let (all, _hits, _commands, _anchor, item_spans) = state.conversation_build(width);
    // Plain text only backs mouse selection / clipboard. Rebuild it from this
    // frame's lines while a selection is live; otherwise clear it (the mouse-down
    // path calls `ensure_conversation_plain`, which rebuilds against current
    // content on demand) so idle frames skip an O(lines) clone per repaint.
    if state.conv.selection.is_active() {
        state.conv.plain = all.iter().map(crate::selection::line_to_plain).collect();
        state.conv.plain_width = width;
    } else if !state.conv.plain.is_empty() {
        state.conv.plain.clear();
        state.conv.plain_width = 0;
    }

    let total = all.len();
    let max_scroll = crate::conversation::geometry::max_scroll(total, height);
    // Empty-state card is read top-down (brand → commands). Following the
    // live edge would hide the product name on a short terminal.
    let follow = state.conv.auto_scroll && !crate::splash::conversation_is_empty(state);
    let scroll =
        crate::conversation::geometry::effective_scroll(state.conv.scroll, follow, total, height);

    // Only the visible window is cloned + highlighted; the rest stays in the Rc.
    let mut lines: Vec<Line> = all
        .iter()
        .enumerate()
        .skip(scroll)
        .take(height)
        .map(|(_, line)| line.clone())
        .collect();

    // A running Thought's rail animates on THIS frame's window only. The
    // projection above is untouched, so history cannot re-wrap, no cache entry
    // is invalidated, and the highlight is style-only: line count, wrap and the
    // scroll anchor are exactly what the projection produced.
    paint_live_reasoning_rail(state, &all, &item_spans, scroll, &mut lines);

    // Selection is an explicit user act and owns the ink on the rows it
    // covers, so it is applied after the decorative rail highlight.
    for (offset, line) in lines.iter_mut().enumerate() {
        *line = crate::selection::apply_selection_highlight(
            std::mem::replace(line, Line::from("")),
            scroll + offset,
            &state.conv.selection,
            theme,
        );
    }

    // Pad ABOVE, not below: a short conversation should sit against the
    // composer the way terminal output does, instead of pinning to the top of
    // the viewport and leaving a band of dead space between the last line and
    // the input box. Once the transcript is taller than the viewport this pad
    // is empty and scrolling behaves exactly as before.
    //
    // An empty session is the exception. It has no output to grow upward from —
    // it has a title card, and a title card pressed against the input box under
    // a screenful of void reads as a bug rather than a welcome, so centre it.
    if lines.len() < height {
        let above = crate::conversation::geometry::painted_top_padding(state, lines.len(), height);
        let mut padded = vec![Line::from(""); above];
        padded.append(&mut lines);
        padded.resize(height, Line::from(""));
        lines = padded;
    }

    frame.render_widget(
        Paragraph::new(lines).style(
            Style::default()
                .bg(theme.surface.canvas)
                .fg(theme.text.primary),
        ),
        content,
    );

    // Scroll-to-bottom affordance: only when pinned away from live edge.
    // Hide while selecting/copying so the badge cannot cover or steal mouse
    // hits on the text the user is trying to select (was centered on the last
    // row and blocked mid-line copy).
    if max_scroll > 0 && scroll < max_scroll && !state.conv.selection.is_active() {
        let below = max_scroll - scroll;
        let n = state.conv.unread.max(below);
        // The count is CONTENT LINES below the viewport, not messages. Say so:
        // `↓36` read as "36 new messages" when it was really "36 rows".
        let hint = if n > 1 {
            format!(
                " {} ",
                state.t().conv_scroll_below.replace("{}", &n.to_string())
            )
        } else {
            " ↓ ".to_string()
        };
        let hint_w = (hint.chars().count() as u16).max(1).min(area.width);
        // Bottom-right, not center — less likely to sit on prose mid-line.
        let x = area.x.saturating_add(area.width.saturating_sub(hint_w));
        let y = area.y.saturating_add(area.height.saturating_sub(1));
        let btn = Rect {
            x,
            y,
            width: hint_w,
            height: 1,
        };
        state.conv.scroll_bottom_rect = Some((btn.x, btn.y, btn.width, btn.height));
        frame.render_widget(
            Paragraph::new(Span::styled(
                hint,
                Style::default()
                    .fg(theme.accent.primary)
                    .bg(theme.surface.elevated)
                    .add_modifier(Modifier::BOLD),
            )),
            btn,
        );
    } else {
        state.conv.scroll_bottom_rect = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Boot;
    use crate::theme::Theme;
    use leveler_client_protocol::{RuntimeStatus, SessionId};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 60,
        height: 20,
    };

    fn test_state() -> AppState {
        let mut state = AppState::new(
            Theme::dark(),
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
        state.size = (AREA.width, AREA.height);
        state.conv.auto_scroll = true;
        state
    }

    /// A busy turn with one running Thought whose body wraps onto more rows
    /// than the highlight is tall, so a sweep has somewhere to travel.
    fn state_with_live_thought() -> AppState {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.transcript.push_user("先推理再回答".into());
        state.transcript.begin_thought();
        state
            .transcript
            .append_thought(&"正在逐条核对 provider catalog 与 pricing 的差异。".repeat(6));
        state
    }

    fn paint(state: &mut AppState, motion_ms: u64) -> Buffer {
        state.motion_clock = std::time::Duration::from_millis(motion_ms);
        let mut terminal = Terminal::new(TestBackend::new(AREA.width, AREA.height)).unwrap();
        terminal.draw(|frame| render(frame, AREA, state)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// A live Thought preceded by more history than the viewport shows, so the
    /// window can be scrolled while the whole Thought stays on screen.
    fn state_with_scrollable_history() -> AppState {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.conv.auto_scroll = false;
        for index in 0..20 {
            state.transcript.push_user(format!("历史提问 {index}"));
        }
        state.transcript.begin_thought();
        state
            .transcript
            .append_thought(&"正在逐条核对 provider catalog 与 pricing 的差异。".repeat(6));
        state
    }

    /// Every rail row's `│` cell in paint order, as `(row, ink)`.
    fn rail_inks(buffer: &Buffer) -> Vec<(u16, Color)> {
        let mut out = Vec::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "│" {
                    out.push((y, cell.fg));
                    break;
                }
            }
        }
        out
    }

    fn accented_rows(buffer: &Buffer, accent: Color) -> Vec<u16> {
        rail_inks(buffer)
            .into_iter()
            .filter(|(_, fg)| *fg == accent)
            .map(|(row, _)| row)
            .collect()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A running Thought's rail sweeps: the highlight is somewhere near the top
    /// at one phase and further down at the next, and it wears the live accent
    /// while the rest of the rail stays muted.
    #[test]
    fn a_running_thoughts_rail_highlight_sweeps_and_wears_the_live_accent() {
        let mut state = state_with_live_thought();
        let (accent, muted) = (state.theme.accent.secondary, state.theme.text.muted);

        let top = accented_rows(&paint(&mut state, 0), accent);
        let mid = accented_rows(&paint(&mut state, 750), accent);

        assert_eq!(
            top.len(),
            crate::render::REASONING_RAIL_SEGMENT_ROWS,
            "the highlight is one segment tall"
        );
        assert_eq!(top[1], top[0] + 1, "the segment is contiguous");
        assert_ne!(top, mid, "a later frame moved the highlight");
        assert!(
            top[0] < mid[0],
            "the sweep runs top to bottom: {top:?} {mid:?}"
        );

        let inks = rail_inks(&paint(&mut state, 0));
        assert!(
            inks.iter().filter(|(_, fg)| *fg == muted).count() > 0,
            "the rail body keeps its muted ink: {inks:?}"
        );
        assert!(
            inks.iter().all(|(_, fg)| *fg == accent || *fg == muted),
            "only the rail is recoloured: {inks:?}"
        );
    }

    /// A completed Thought has stopped: its expanded body paints a plain muted
    /// rail at every phase.
    #[test]
    fn a_completed_thought_has_no_animated_rail() {
        let mut state = state_with_live_thought();
        state.transcript.finish_thought(Some(2800));
        let index = state.transcript.items().len() - 1;
        assert!(
            state
                .transcript
                .set_item_display(index, crate::fold::DisplayMode::Expanded)
        );
        let (accent, muted) = (state.theme.accent.secondary, state.theme.text.muted);

        for ms in [0, 400, 750, 1499] {
            let inks = rail_inks(&paint(&mut state, ms));
            assert!(!inks.is_empty(), "the expanded body is painted at {ms}ms");
            assert!(
                inks.iter().all(|(_, fg)| *fg == muted),
                "a settled rail never animates (at {ms}ms): {inks:?}"
            );
            assert!(accented_rows(&paint(&mut state, ms), accent).is_empty());
        }
    }

    /// A collapsed Thought paints no body at all, so it owns no rail to sweep.
    #[test]
    fn a_collapsed_thought_paints_no_rail() {
        let mut state = state_with_live_thought();
        let index = state.transcript.items().len() - 1;
        assert!(
            state
                .transcript
                .set_item_display(index, crate::fold::DisplayMode::Collapsed)
        );
        for ms in [0, 750] {
            assert!(
                rail_inks(&paint(&mut state, ms)).is_empty(),
                "no rail without a body (at {ms}ms)"
            );
        }
    }

    /// Reduced motion degrades the rail to a static muted one while the live
    /// state stays visible in the header accent — the fact survives, the
    /// motion does not.
    #[test]
    fn reduced_motion_leaves_a_static_live_rail() {
        let mut state = state_with_live_thought();
        state.live_animation = false;
        let (accent, muted) = (state.theme.accent.secondary, state.theme.text.muted);

        let first = rail_inks(&paint(&mut state, 0));
        let later = rail_inks(&paint(&mut state, 1200));
        assert_eq!(first, later, "no motion is painted");
        assert!(!first.is_empty());
        assert!(
            first.iter().all(|(_, fg)| *fg == muted),
            "static rail: {first:?}"
        );

        let header = paint(&mut state, 0);
        let bullet = (0..header.area.height)
            .flat_map(|y| (0..header.area.width).map(move |x| (x, y)))
            .find(|&(x, y)| header[(x, y)].symbol() == "◆")
            .map(|(x, y)| header[(x, y)].fg);
        assert_eq!(
            bullet,
            Some(accent),
            "the running Thought is still marked live"
        );
    }

    /// No animation frame may move a line, re-wrap a row, or shift the anchor:    /// every phase paints the same text over the same projection, and the
    /// projection cache is not invalidated by a repaint.
    #[test]
    fn an_animation_frame_changes_no_line_and_no_scroll() {
        let mut state = state_with_live_thought();
        let width = crate::conversation::geometry::content_rect(AREA).width as usize;
        let height = crate::conversation::geometry::content_rect(AREA).height as usize;

        let (projection, _, _, _, _) = state.conversation_build(width);
        let total = projection.len();
        let widths: Vec<usize> = projection.iter().map(Line::width).collect();
        let max_scroll = crate::conversation::geometry::max_scroll(total, height);
        let scroll_before = state.conv.scroll;

        let mut baseline: Option<String> = None;
        for ms in [0, 400, 750, 1200, 1499] {
            let painted = paint(&mut state, ms);
            let text = buffer_text(&painted);
            match &baseline {
                Some(previous) => assert_eq!(&text, previous, "text moved at {ms}ms"),
                None => baseline = Some(text),
            }

            let (again, _, _, _, _) = state.conversation_build(width);
            assert!(
                std::rc::Rc::ptr_eq(&projection, &again),
                "an animation frame must not invalidate the projection at {ms}ms"
            );
            assert_eq!(again.iter().map(Line::width).collect::<Vec<_>>(), widths);
            assert_eq!(
                crate::conversation::geometry::effective_scroll(
                    state.conv.scroll,
                    state.conv.auto_scroll,
                    again.len(),
                    height
                ),
                max_scroll,
                "follow-tail anchor moved at {ms}ms"
            );
            assert_eq!(
                state.conv.scroll, scroll_before,
                "the renderer wrote a scroll"
            );
        }
    }

    /// The sweep is defined on the Thought's rail, not on the window: scrolling
    /// the same Thought one row up moves the highlight's screen row with it
    /// instead of moving the highlight along the rail.
    #[test]
    fn the_highlight_keeps_its_rail_position_when_the_window_scrolls() {
        let mut state = state_with_scrollable_history();
        let accent = state.theme.accent.secondary;
        let content = crate::conversation::geometry::content_rect(AREA);
        let width = content.width as usize;
        let height = content.height as usize;
        let item = state.transcript.items().len() - 1;
        let start = state
            .item_start_line(item, width)
            .expect("the Thought is placed");
        let total = crate::conversation::build::conversation_line_count(&state, width);
        let max_scroll = crate::conversation::geometry::max_scroll(total, height);
        assert!(
            start > max_scroll,
            "history scrolls above the Thought: {start}"
        );

        // Where the accent sits within the visible rail, top row = 0.
        let rail_positions = |state: &mut AppState, scroll: usize| -> Vec<usize> {
            state.conv.scroll = scroll;
            rail_inks(&paint(state, 750))
                .iter()
                .enumerate()
                .filter(|(_, (_, ink))| *ink == accent)
                .map(|(position, _)| position)
                .collect()
        };

        let lower = rail_positions(&mut state, max_scroll);
        let upper = rail_positions(&mut state, max_scroll - 1);
        assert_eq!(lower.len(), crate::render::REASONING_RAIL_SEGMENT_ROWS);
        assert_eq!(lower, upper, "the highlight did not move along the rail");
    }
}
