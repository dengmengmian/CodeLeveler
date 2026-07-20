//! Conversation text selection + clipboard copy.
//!
//! Mouse drag inside the Conversation viewport builds a lightweight selection
//! (display-column based). On release, the selected plain text is copied to
//! the system clipboard via `arboard`.

use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::Theme;

/// Display-cell position inside the full Conversation content (not viewport).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextPos {
    /// Absolute content line index (0-based).
    pub row: usize,
    /// Display column within the line (0-based).
    pub col: usize,
}

/// Active or completed selection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextSelection {
    pub anchor: Option<TextPos>,
    pub focus: Option<TextPos>,
    /// True while the primary button is held and dragging.
    pub dragging: bool,
}

impl TextSelection {
    pub fn clear(&mut self) {
        self.anchor = None;
        self.focus = None;
        self.dragging = false;
    }

    pub fn is_empty(&self) -> bool {
        match (self.anchor, self.focus) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }

    /// True when a selection is in progress or still painted after copy.
    pub fn is_active(&self) -> bool {
        self.dragging || !self.is_empty()
    }

    /// Normalized (start, end) with start ≤ end in document order.
    pub fn range(&self) -> Option<(TextPos, TextPos)> {
        let a = self.anchor?;
        let b = self.focus?;
        if (a.row, a.col) <= (b.row, b.col) {
            Some((a, b))
        } else {
            Some((b, a))
        }
    }

    pub fn begin(&mut self, pos: TextPos) {
        self.anchor = Some(pos);
        self.focus = Some(pos);
        self.dragging = true;
    }

    pub fn extend(&mut self, pos: TextPos) {
        if self.anchor.is_some() {
            self.focus = Some(pos);
            self.dragging = true;
        }
    }

    pub fn finish(&mut self) {
        self.dragging = false;
        if self.is_empty() {
            self.clear();
        }
    }
}

/// Extract plain text for a selection from absolute content lines.
pub fn extract_selected_text(plain_lines: &[String], sel: &TextSelection) -> String {
    let Some((start, end)) = sel.range() else {
        return String::new();
    };
    if plain_lines.is_empty() {
        return String::new();
    }
    let last = plain_lines.len() - 1;
    let start_row = start.row.min(last);
    let end_row = end.row.min(last);
    let mut out = String::new();
    for row in start_row..=end_row {
        let line = plain_lines.get(row).map(String::as_str).unwrap_or("");
        let from = if row == start_row { start.col } else { 0 };
        let to = if row == end_row {
            end.col
        } else {
            UnicodeWidthStr::width(line)
        };
        let slice = slice_display_cols(line, from, to);
        if row > start_row {
            out.push('\n');
        }
        out.push_str(&slice);
    }
    out
}

/// Slice `s` by display columns `[from, to)`.
pub fn slice_display_cols(s: &str, from: usize, to: usize) -> String {
    if from >= to {
        return String::new();
    }
    let mut out = String::new();
    let mut col = 0usize;
    for g in s.graphemes(true) {
        let gw = g
            .chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum::<usize>()
            .max(1);
        let next = col + gw;
        if next > from && col < to {
            out.push_str(g);
        }
        col = next;
        if col >= to {
            break;
        }
    }
    out
}

/// Apply a soft reverse highlight to selected cells on one viewport line.
///
/// Unselected spans keep their original styles so the line does not "jump"
/// (flattening everything to muted + bold was shifting visual width/color).
/// Selected cells only change **background** (no bold) for the same reason.
///
/// `abs_row` is the absolute content row index for this line.
pub fn apply_selection_highlight(
    line: Line<'static>,
    abs_row: usize,
    sel: &TextSelection,
    theme: &Theme,
) -> Line<'static> {
    let Some((start, end)) = sel.range() else {
        return line;
    };
    if abs_row < start.row || abs_row > end.row {
        return line;
    }
    let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    if plain.is_empty() {
        return line;
    }
    let line_w = UnicodeWidthStr::width(plain.as_str());
    let from = if abs_row == start.row { start.col } else { 0 };
    let to = if abs_row == end.row { end.col } else { line_w };
    if from >= to {
        return line;
    }

    let sel_bg = theme.border;
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    for span in line.spans {
        let text = span.content.as_ref();
        if text.is_empty() {
            continue;
        }
        let span_w = UnicodeWidthStr::width(text);
        let span_start = col;
        let span_end = col + span_w;

        // Entirely outside selection → keep as-is.
        if span_end <= from || span_start >= to {
            col = span_end;
            out.push(span);
            continue;
        }
        let style = span.style;

        // Classify every grapheme into before/mid/after by its own cell span, so
        // a full-width glyph straddling a selection boundary lands in exactly one
        // bucket (never duplicated, never dropped). Rule matches the overlap rule
        // in `slice_display_cols` so highlight and copied text stay consistent.
        let (mut before, mut mid, mut after) = (String::new(), String::new(), String::new());
        for g in text.graphemes(true) {
            let gw = g
                .chars()
                .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                .sum::<usize>()
                .max(1);
            let gstart = col;
            col += gw;
            if col <= from {
                before.push_str(g);
            } else if gstart < to {
                mid.push_str(g);
            } else {
                after.push_str(g);
            }
        }

        if !before.is_empty() {
            out.push(Span::styled(before, style));
        }
        if !mid.is_empty() {
            // Background only: keep fg from original style so glyphs don't reflow.
            let mut mid_style = style.bg(sel_bg);
            // Ensure selected text stays readable if fg was default/dark.
            if mid_style.fg.is_none() {
                mid_style = mid_style.fg(theme.text);
            }
            out.push(Span::styled(mid, mid_style));
        }
        if !after.is_empty() {
            out.push(Span::styled(after, style));
        }
    }
    if out.is_empty() {
        Line::from("")
    } else {
        Line::from(out)
    }
}

/// Copy text to the system clipboard. Returns true on success.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|e| format!("clipboard set failed: {e}"))
}

/// Flatten a ratatui Line to plain text (for selection extraction).
pub fn line_to_plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};

    #[test]
    fn extract_single_line_range() {
        let lines = vec!["hello world".into()];
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 6 });
        sel.extend(TextPos { row: 0, col: 11 });
        sel.finish();
        assert_eq!(extract_selected_text(&lines, &sel), "world");
    }

    #[test]
    fn extract_multi_line() {
        let lines = vec!["aaa".into(), "bbb".into(), "ccc".into()];
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 1 });
        sel.extend(TextPos { row: 2, col: 2 });
        assert_eq!(extract_selected_text(&lines, &sel), "aa\nbbb\ncc");
    }

    #[test]
    fn reversed_drag_normalizes() {
        let lines = vec!["abcdef".into()];
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 5 });
        sel.extend(TextPos { row: 0, col: 1 });
        assert_eq!(extract_selected_text(&lines, &sel), "bcde");
    }

    #[test]
    fn highlight_preserves_unselected_styles_and_width() {
        let theme = Theme::no_color();
        let line = Line::from(vec![
            Span::styled("hello ", Style::default().fg(Color::Yellow)),
            Span::styled("world", Style::default().fg(Color::Cyan)),
        ]);
        let mut sel = TextSelection::default();
        // Select only "wor" inside the second span (cols 6..9).
        sel.begin(TextPos { row: 0, col: 6 });
        sel.extend(TextPos { row: 0, col: 9 });
        let out = apply_selection_highlight(line, 0, &sel, &theme);
        let plain: String = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(plain, "hello world");
        // First span (unselected) must keep yellow.
        assert_eq!(out.spans[0].style.fg, Some(Color::Yellow));
        assert_eq!(out.spans[0].content.as_ref(), "hello ");
        // Selected mid keeps cyan fg (not forced muted).
        assert!(
            out.spans
                .iter()
                .any(|s| s.content.as_ref() == "wor" && s.style.bg.is_some()),
            "selected mid must have bg: {out:?}"
        );
        // No bold on selection (was a source of terminal reflow).
        assert!(
            out.spans.iter().all(|s| !s
                .style
                .add_modifier
                .intersects(ratatui::style::Modifier::BOLD)),
            "selection must not add BOLD: {out:?}"
        );
    }

    #[test]
    fn highlight_no_duplicate_when_boundary_splits_wide_glyph() {
        // Drag end lands in the middle of the trailing full-width "？" (cols 26..28).
        // The glyph must not be emitted twice (once highlighted, once not).
        let theme = Theme::no_color();
        let text = "你好！有什么需要帮忙的吗？";
        let line = Line::from(Span::styled(text.to_string(), Style::default()));
        let full_w = UnicodeWidthStr::width(text);
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 0 });
        // Odd column inside the last wide glyph.
        sel.extend(TextPos {
            row: 0,
            col: full_w - 1,
        });
        let out = apply_selection_highlight(line, 0, &sel, &theme);
        let plain: String = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(plain, text, "highlighted line must not duplicate the glyph");
        assert_eq!(UnicodeWidthStr::width(plain.as_str()), full_w);
    }

    #[test]
    fn is_active_while_dragging_or_painted() {
        let mut sel = TextSelection::default();
        assert!(!sel.is_active());
        sel.begin(TextPos { row: 0, col: 0 });
        assert!(sel.is_active());
        sel.extend(TextPos { row: 0, col: 3 });
        sel.finish();
        assert!(sel.is_active());
        sel.clear();
        assert!(!sel.is_active());
    }
}
