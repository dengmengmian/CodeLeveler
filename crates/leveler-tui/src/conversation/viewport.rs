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

pub fn render(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let theme = &state.theme;
    let width = area.width as usize;
    let height = area.height as usize;
    if height == 0 || width == 0 {
        state.conv.rect = None;
        state.conv.scroll_bottom_rect = None;
        return;
    }

    let all = state.conversation_lines(width);
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
    let scroll = crate::conversation::geometry::effective_scroll(
        state.conv.scroll,
        state.conv.auto_scroll,
        total,
        height,
    );

    // Only the visible window is cloned + highlighted; the rest stays in the Rc.
    let mut lines: Vec<Line> = all
        .iter()
        .enumerate()
        .skip(scroll)
        .take(height)
        .map(|(abs_row, line)| {
            crate::selection::apply_selection_highlight(
                line.clone(),
                abs_row,
                &state.conv.selection,
                theme,
            )
        })
        .collect();

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
        let pad = crate::conversation::geometry::top_padding(lines.len(), height);
        let above = if crate::splash::conversation_is_empty(state) {
            pad / 2
        } else {
            pad
        };
        let mut padded = vec![Line::from(""); above];
        padded.append(&mut lines);
        padded.resize(height, Line::from(""));
        lines = padded;
    }

    frame.render_widget(Paragraph::new(lines), area);

    // Mouse hit-testing for next events.
    state.conv.rect = Some((area.x, area.y, area.width, area.height));

    // Scroll-to-bottom affordance: only when pinned away from live edge.
    // Hide while selecting/copying so the badge cannot cover or steal mouse
    // hits on the text the user is trying to select (was centered on the last
    // row and blocked mid-line copy).
    if max_scroll > 0 && scroll < max_scroll && !state.conv.selection.is_active() {
        let below = max_scroll - scroll;
        let n = state.conv.unread.max(below);
        let hint = if n > 1 {
            format!(" ▼{n} ")
        } else {
            " ▼ ".to_string()
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
                    .fg(theme.accent)
                    .bg(theme.code_bg)
                    .add_modifier(Modifier::BOLD),
            )),
            btn,
        );
    } else {
        state.conv.scroll_bottom_rect = None;
    }
}
