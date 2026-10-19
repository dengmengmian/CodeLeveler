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

/// Animate the left rail and status glyph of every running local command in
/// this window. Style-only over the already-built lines: the record's height,
/// wrapping and hit rows never change, and the animation is defined by elapsed
/// time so a burst of output cannot speed it up.
fn paint_live_shell_rail(
    state: &AppState,
    item_spans: &[(usize, usize)],
    scroll: usize,
    lines: &mut [Line<'static>],
) {
    if !state.live_animation {
        return;
    }
    let level =
        crate::motion::breath_level(state.motion_clock, crate::shell_render::SHELL_BREATH_PERIOD);
    let color = state.theme.shell.breathe(level);
    let spinner = crate::status_line::spinner_frame((state.motion_clock.as_millis() / 120) as u64);
    let window_end = scroll + lines.len();
    for (index, item) in state.transcript.items().iter().enumerate() {
        let TranscriptItem::UserShell(shell) = item else {
            continue;
        };
        if shell.status != crate::transcript::UserShellStatus::Running {
            continue;
        }
        let Some(&(start, end)) = item_spans.get(index) else {
            continue;
        };
        if end <= scroll || start >= window_end {
            continue;
        }
        for row in start..end.min(scroll + lines.len()) {
            let Some(offset) = row.checked_sub(scroll) else {
                continue;
            };
            let Some(line) = lines.get_mut(offset) else {
                continue;
            };
            if let Some(rail) = line.spans.first_mut() {
                rail.style = rail.style.fg(color);
            }
            // The header's second span is the status glyph.
            if row == start
                && let Some(glyph) = line.spans.get_mut(1)
            {
                glyph.content = format!("{spinner} ").into();
                glyph.style = glyph.style.fg(color);
            }
        }
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

    let (all, _hits, _commands, _anchor, item_spans, _sources) = state.conversation_build(width);
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
    paint_live_shell_rail(state, &item_spans, scroll, &mut lines);

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

    // Real transcripts grow downward from the viewport top. The empty-session
    // splash alone is centered; hit-testing consumes this same geometry policy.
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

    /// Live reasoning paints in the conversation; a settled one leaves no row.
    #[test]
    fn live_reasoning_paints_its_rows_and_rail() {
        let mut state = state_with_live_thought();
        assert!(state.transcript.is_thinking());
        for ms in [0, 750, 1499] {
            let buffer = paint(&mut state, ms);
            assert!(
                !rail_inks(&buffer).is_empty(),
                "live reasoning paints its own rail"
            );
            assert!(buffer_text(&buffer).contains("◆"));
        }
    }

    /// Historical display flags cannot create a reasoning body or header.
    #[test]
    fn a_completed_thought_has_no_animated_rail() {
        let mut state = state_with_live_thought();
        state.transcript.finish_thought(Some(2800));
        let index = state.transcript.items().len() - 1;
        state
            .transcript
            .set_item_display(index, crate::fold::DisplayMode::Expanded);
        for ms in [0, 400, 750, 1499] {
            let buffer = paint(&mut state, ms);
            assert!(
                rail_inks(&buffer).is_empty(),
                "reasoning remains hidden after display toggles"
            );
            assert!(!buffer_text(&buffer).contains("◆"));
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

    /// Animation settings change the rail's motion, not the rows themselves.
    #[test]
    fn reduced_motion_keeps_the_painted_reasoning_still() {
        let mut state = state_with_live_thought();
        state.live_animation = false;
        let first = paint(&mut state, 0);
        let later = paint(&mut state, 1200);
        assert!(!rail_inks(&first).is_empty());
        assert_eq!(
            buffer_text(&first),
            buffer_text(&later),
            "hidden reasoning has no animation"
        );
        assert!(
            state.transcript.is_thinking(),
            "runtime reasoning fact is preserved"
        );
    }

    /// No animation frame may move a line, re-wrap a row, or shift the anchor:    /// every phase paints the same text over the same projection, and the
    /// projection cache is not invalidated by a repaint.
    #[test]
    fn an_animation_frame_changes_no_line_and_no_scroll() {
        let mut state = state_with_live_thought();
        let width = crate::conversation::geometry::content_rect(AREA).width as usize;
        let height = crate::conversation::geometry::content_rect(AREA).height as usize;

        let (projection, _, _, _, _, _) = state.conversation_build(width);
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

            let (again, _, _, _, _, _) = state.conversation_build(width);
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

    /// Hidden reasoning never changes the geometry of a pinned history viewport.
    #[test]
    fn hidden_reasoning_preserves_pinned_history_scroll() {
        let mut state = state_with_scrollable_history();
        let thought = state.transcript.items().len() - 1;
        state
            .transcript
            .set_item_display(thought, crate::fold::DisplayMode::Expanded);
        let content = crate::conversation::geometry::content_rect(AREA);
        let total =
            crate::conversation::build::conversation_line_count(&state, content.width as usize);
        let max_scroll = crate::conversation::geometry::max_scroll(total, content.height as usize);
        assert!(max_scroll > 0, "fixture must be scrollable");
        for scroll in [max_scroll, max_scroll - 1] {
            state.conv.scroll = scroll;
            let before = paint(&mut state, 0);
            let later = paint(&mut state, 750);
            assert!(!rail_inks(&later).is_empty());
            assert_eq!(buffer_text(&before), buffer_text(&later));
            assert_eq!(
                state.conv.scroll, scroll,
                "hidden reasoning never changes pinned scroll"
            );
        }
    }
}
