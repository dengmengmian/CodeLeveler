use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::state::AppState;

/// Render a full-screen view's content with the user's scroll offset, clamped
/// so the last line stays reachable but never scrolls past the end.
pub(crate) fn render_scrolled(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    lines: Vec<Line<'static>>,
) {
    let max = lines.len().saturating_sub(area.height as usize);
    let scroll = state.screen_scroll.min(max);
    render_pane_filled(frame, area, lines, scroll);
}

/// The scroll offset that keeps line `focus` visible in an `area_h`-tall pane
/// showing `lines_len` lines: 0 until the focus passes the fold, then just enough
/// to keep it on the last visible row, clamped to the content.
pub(crate) fn list_scroll_offset(lines_len: usize, area_h: usize, focus: usize) -> usize {
    let max = lines_len.saturating_sub(area_h);
    focus.saturating_sub(area_h.saturating_sub(1)).min(max)
}

/// Render a selectable list, auto-scrolling so the `focus`-th line stays visible
/// as the selection moves with ↑↓ (the list can be taller than the pane).
pub(crate) fn render_list_focused(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    focus: usize,
) {
    let offset = list_scroll_offset(lines.len(), area.height as usize, focus);
    render_pane_filled(frame, area, lines, offset);
}

/// Paint a scrollable pane by materialising every visible row (padded to the
/// full width). Paragraph alone only writes glyphs, so shorter content can leave
/// previous frame tails if Clear is skipped or cells get out of sync; filling
/// each row forces a complete overwrite of the viewport.
fn render_pane_filled(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>, scroll: usize) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let height = area.height as usize;
    let mut visible: Vec<Line<'static>> = Vec::with_capacity(height);
    for row in 0..height {
        let line = lines
            .get(scroll + row)
            .cloned()
            .unwrap_or_else(|| Line::from(""));
        visible.push(pad_line_to_width(line, width));
    }
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Paragraph::new(visible), area);
}

/// Truncate or right-pad a line so its display width equals `width`. Every cell
/// in the row is then owned by this frame's buffer.
pub(crate) fn pad_line_to_width(line: Line<'static>, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::from("");
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        if used >= width {
            break;
        }
        let content = span.content.as_ref();
        let sw = UnicodeWidthStr::width(content);
        if used + sw <= width {
            used += sw;
            spans.push(span);
            continue;
        }
        // Span overflows remaining room — keep a width-safe prefix (no ellipsis;
        // the pane clips, it doesn't need a marker on every truncated row).
        let room = width - used;
        let mut piece = String::new();
        let mut w = 0usize;
        for ch in content.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if w + cw > room {
                break;
            }
            piece.push(ch);
            w += cw;
        }
        // If a wide char won't fit, leave the leftover as padding below.
        if !piece.is_empty() {
            spans.push(Span::styled(piece, span.style));
            used += w;
        }
        break;
    }
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    Line::from(spans)
}
