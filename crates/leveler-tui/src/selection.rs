//! Conversation text selection + clipboard copy.
//!
//! Mouse drag inside the Conversation viewport builds a lightweight selection
//! (display-column based). On release, the selected plain text is copied to
//! the clipboard through terminal-aware routes with an evidence-bearing receipt.

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

/// How one painted conversation row maps to copyable text.
///
/// The row's plain text stays aligned with the screen, so selection columns
/// and URL hit-testing keep working unchanged. Everything here is produced by
/// the renderer that painted the row — never inferred from the text:
///
/// * `indent` — leading display columns that are decoration (marker gutter,
///   code gutter, rail).
/// * `skip` — the row has no authored text (block header, status, padding,
///   divider) and must not copy.
/// * `joiner` — `Some` means this row is a soft wrap of the previous logical
///   line; copy that joiner instead of a newline. `Some("")` is a break with
///   no source character (CJK/overlong code).
/// * `prefix` — the semantic marker the renderer replaced with a visual glyph
///   (heading `#`, list `- `/`1. `, quote `> `). Emitted only when the
///   selection starts at the marker.
/// * `break_line` — a `skip` row that still ENDS the previous copied line.
///   A diff separator or hunk header paints chrome between two runs of real
///   code, and those runs are separate source lines: skipping the row without
///   a break would glue hunk A's last line onto hunk B's first one.
/// * `suffix` — trailing display columns that are decoration (a duration, a
///   fold hint). A selection that runs into them copies none of those columns.
/// * `source` — the row's real text when the paint truncated it. Used only
///   when the selection covers the whole content region; a drag through the
///   middle still copies the painted cells.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlainRow {
    pub indent: u16,
    pub skip: bool,
    pub joiner: Option<Box<str>>,
    pub prefix: Option<Box<str>>,
    pub break_line: bool,
    pub suffix: u16,
    pub source: Option<Box<str>>,
}

/// One painted row a renderer declares copyable, addressed relative to the
/// lines that same renderer returned.
///
/// The activity stream paints a tool group as one block of lines, so a copy
/// policy that describes only its diff body has to name the rows it owns. The
/// renderer that painted them is the only thing that knows the gutter width,
/// the wrap points and the marker columns — nothing here is inferred from the
/// painted text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopyRow {
    pub row: usize,
    pub policy: PlainRow,
}

impl CopyRow {
    pub(crate) fn new(row: usize, policy: PlainRow) -> Self {
        Self { row, policy }
    }
}

/// Apply renderer-declared copy rows to the span that was painted as
/// display-only. `copy` starts at the block's first line, exactly like the
/// indices the renderer reported.
///
/// An index past the end is ignored: the block shrank between painting and
/// here, and a stale override must never rewrite a different row's policy.
pub(crate) fn apply_copy_rows(copy: &mut [PlainRow], rows: &[CopyRow]) {
    for row in rows {
        if let Some(slot) = copy.get_mut(row.row) {
            *slot = row.policy.clone();
        }
    }
}

/// Extract plain text for a selection from absolute content lines, honoring
/// each row's [`PlainRow`] layout. Rows without an entry are ordinary text.
pub fn extract_selected_text_with_layout(
    plain_lines: &[String],
    layout: &[PlainRow],
    sel: &TextSelection,
) -> String {
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
    let mut wrote_row = false;
    for row in start_row..=end_row {
        let row_layout = layout.get(row).cloned().unwrap_or_default();
        // A display-only row copies no text. It only breaks the preceding
        // copied line when the renderer said the row sits between two of them.
        if row_layout.skip {
            if row_layout.break_line && wrote_row {
                out.push('\n');
            }
            continue;
        }
        let line = plain_lines.get(row).map(String::as_str).unwrap_or("");
        let from_raw = if row == start_row { start.col } else { 0 };
        let indent = usize::from(row_layout.indent);
        let suffix = usize::from(row_layout.suffix);
        let line_width = UnicodeWidthStr::width(line);
        // Trailing decoration stays on screen and out of the clipboard.
        let content_end = line_width.saturating_sub(suffix);
        let from = from_raw.max(indent);
        let to_raw = if row == end_row { end.col } else { line_width };
        let to = to_raw.min(content_end);
        // The whole content region is selected: a truncated row then copies
        // the source the renderer named, not the ellipsis it painted.
        let covers_content = from_raw <= indent && to_raw >= content_end && content_end > indent;
        let use_source = covers_content && row_layout.source.is_some();
        // The selection ends inside this row's decoration: it reaches no text.
        if !use_source && to < from {
            continue;
        }
        if wrote_row {
            match row_layout.joiner.as_deref() {
                // A soft wrap: the renderer broke the line here, the source
                // had only this whitespace (often none, for CJK/code).
                Some(joiner) => out.push_str(joiner),
                None => out.push('\n'),
            }
        }
        if from_raw <= indent
            && let Some(prefix) = row_layout.prefix.as_deref()
        {
            out.push_str(prefix);
        }
        if let Some(source) = row_layout.source.as_deref().filter(|_| use_source) {
            out.push_str(source);
        } else if to > from {
            out.push_str(&slice_display_cols(line, from, to));
        }
        wrote_row = true;
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

    let sel_bg = theme.surface.selection;
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
            // Explicit pair: never inherit a fg that collides with selection.
            let mid_style = style.bg(sel_bg).fg(theme.text.primary);
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

/// Copy selected text; forwarding is not proof that the outer clipboard received it.
pub fn copy_to_clipboard(text: &str) -> Result<crate::clipboard::CopyReceipt, String> {
    crate::clipboard::copy(text)
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
        assert_eq!(
            extract_selected_text_with_layout(&lines, &[], &sel),
            "world"
        );
    }

    #[test]
    fn extract_multi_line() {
        let lines = vec!["aaa".into(), "bbb".into(), "ccc".into()];
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 1 });
        sel.extend(TextPos { row: 2, col: 2 });
        assert_eq!(
            extract_selected_text_with_layout(&lines, &[], &sel),
            "aa\nbbb\ncc"
        );
    }

    #[test]
    fn reversed_drag_normalizes() {
        let lines = vec!["abcdef".into()];
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 5 });
        sel.extend(TextPos { row: 0, col: 1 });
        assert_eq!(extract_selected_text_with_layout(&lines, &[], &sel), "bcde");
    }

    /// A prompt's marker gutter is decoration: selecting the whole band must
    /// not copy its columns, and its vertical padding rows are not text.
    #[test]
    fn copying_a_prompt_drops_its_gutter_and_padding_rows() {
        let lines = vec![
            "".to_string(),
            "  alpha".to_string(),
            "  ".to_string(),
            "  beta".to_string(),
            "".to_string(),
        ];
        let layout = vec![
            PlainRow {
                indent: 0,
                skip: true,
                ..Default::default()
            },
            PlainRow {
                indent: 2,
                skip: false,
                ..Default::default()
            },
            PlainRow {
                indent: 2,
                skip: false,
                ..Default::default()
            },
            PlainRow {
                indent: 2,
                skip: false,
                ..Default::default()
            },
            PlainRow {
                indent: 0,
                skip: true,
                ..Default::default()
            },
        ];
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 0 });
        sel.extend(TextPos { row: 4, col: 8 });
        sel.finish();
        assert_eq!(
            extract_selected_text_with_layout(&lines, &layout, &sel),
            "alpha\n\nbeta"
        );
    }

    /// The gutter is also dropped when a drag starts inside it.
    #[test]
    fn a_drag_starting_in_the_gutter_copies_only_body_text() {
        let lines = vec!["  hello".to_string()];
        let layout = vec![PlainRow {
            indent: 2,
            skip: false,
            ..Default::default()
        }];
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 0 });
        sel.extend(TextPos { row: 0, col: 7 });
        sel.finish();
        assert_eq!(
            extract_selected_text_with_layout(&lines, &layout, &sel),
            "hello"
        );
    }

    /// A soft wrap is one logical line: copy the renderer's joiner, never a
    /// newline. A word wrap keeps the dropped space; a CJK/code break adds
    /// nothing.
    #[test]
    fn a_soft_wrap_rejoins_without_a_newline() {
        let lines = vec![
            "hello".to_string(),
            "world".to_string(),
            "下一行".to_string(),
        ];
        let layout = vec![
            PlainRow::default(),
            PlainRow {
                joiner: Some(" ".into()),
                ..Default::default()
            },
            PlainRow::default(),
        ];
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 0 });
        sel.extend(TextPos { row: 2, col: 6 });
        sel.finish();
        assert_eq!(
            extract_selected_text_with_layout(&lines, &layout, &sel),
            "hello world\n下一行"
        );
    }

    /// A list/heading/quote marker the renderer replaced with a glyph is
    /// restored only when the selection starts at the marker.
    #[test]
    fn a_semantic_prefix_is_restored_at_the_marker() {
        let lines = vec!["• alpha".to_string(), "• beta".to_string()];
        let layout = vec![
            PlainRow {
                indent: 2,
                prefix: Some("- ".into()),
                ..Default::default()
            },
            PlainRow {
                indent: 2,
                prefix: Some("- ".into()),
                ..Default::default()
            },
        ];
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 0 });
        sel.extend(TextPos { row: 1, col: 6 });
        sel.finish();
        assert_eq!(
            extract_selected_text_with_layout(&lines, &layout, &sel),
            "- alpha\n- beta"
        );

        // Starting inside the body does not inject a marker.
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 4 });
        sel.extend(TextPos { row: 0, col: 7 });
        sel.finish();
        assert_eq!(
            extract_selected_text_with_layout(&lines, &layout, &sel),
            "pha"
        );
    }

    /// A selection that ends inside the decoration contributes no line.
    #[test]
    fn a_selection_ending_in_the_gutter_adds_nothing() {
        let lines = vec!["  hello".to_string(), "  world".to_string()];
        let layout = vec![
            PlainRow {
                indent: 2,
                ..Default::default()
            },
            PlainRow {
                indent: 2,
                ..Default::default()
            },
        ];
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 0 });
        sel.extend(TextPos { row: 0, col: 1 });
        sel.finish();
        assert_eq!(extract_selected_text_with_layout(&lines, &layout, &sel), "");
    }

    /// A fold hint painted on the same row is decoration. Selecting the row
    /// copies the words and stops before the hint.
    #[test]
    fn a_trailing_suffix_is_decoration() {
        let lines = vec!["验收范围不完整 (+2 行)".to_string()];
        let hint = unicode_width::UnicodeWidthStr::width(" (+2 行)");
        let layout = vec![PlainRow {
            suffix: hint as u16,
            ..Default::default()
        }];
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 0 });
        sel.extend(TextPos {
            row: 0,
            col: 10_000,
        });
        sel.finish();
        assert_eq!(
            extract_selected_text_with_layout(&lines, &layout, &sel),
            "验收范围不完整"
        );
    }

    /// Covering a truncated row copies the source the renderer named. A drag
    /// that stops inside the painted cells copies only those cells.
    #[test]
    fn a_covered_truncated_row_copies_its_source() {
        let lines = vec!["目标未提交  在本地把 DevOrder…".to_string()];
        let full = "目标未提交  在本地把 DevOrder 全套服务起来";
        let layout = vec![PlainRow {
            source: Some(full.into()),
            ..Default::default()
        }];
        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 0 });
        sel.extend(TextPos {
            row: 0,
            col: 10_000,
        });
        sel.finish();
        assert_eq!(
            extract_selected_text_with_layout(&lines, &layout, &sel),
            full
        );

        let mut sel = TextSelection::default();
        sel.begin(TextPos { row: 0, col: 0 });
        sel.extend(TextPos { row: 0, col: 10 });
        sel.finish();
        assert_eq!(
            extract_selected_text_with_layout(&lines, &layout, &sel),
            "目标未提交"
        );
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
    fn named_themes_select_with_the_readable_pair() {
        for theme in [Theme::dark(), Theme::light(), Theme::high_contrast()] {
            let line = Line::from(Span::styled(
                "selected",
                Style::default().fg(theme.text.muted),
            ));
            let mut sel = TextSelection::default();
            sel.begin(TextPos { row: 0, col: 0 });
            sel.extend(TextPos { row: 0, col: 8 });
            let out = apply_selection_highlight(line, 0, &sel, &theme);
            assert!(
                out.spans.iter().any(|s| {
                    s.content.as_ref() == "selected"
                        && s.style.bg == Some(theme.surface.selection)
                        && s.style.fg == Some(theme.text.primary)
                }),
                "{:?} selection pair: {out:?}",
                theme.id
            );
        }
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
