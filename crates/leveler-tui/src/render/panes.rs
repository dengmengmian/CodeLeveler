use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::state::AppState;

/// Semantic colour for a list row's status glyph, title or state message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowTone {
    Default,
    Muted,
    Accent,
    Success,
    Warning,
    Error,
    Info,
}

impl RowTone {
    fn color(self, theme: &crate::theme::Theme) -> Color {
        match self {
            RowTone::Default => theme.text.primary,
            RowTone::Muted => theme.text.muted,
            RowTone::Accent => theme.accent.primary,
            RowTone::Success => theme.status.success,
            RowTone::Warning => theme.status.warning,
            RowTone::Error => theme.status.error,
            RowTone::Info => theme.status.info,
        }
    }
}

/// One logical list object. The shared painter owns the columns and the
/// full-row states; the page owns the words and the data they came from.
#[derive(Debug, Clone)]
pub(crate) struct ListRow {
    pub title: String,
    /// One-cell status glyph and its semantic tone. `None` still reserves the
    /// status column so rows in one list share a title column.
    pub status: Option<(String, RowTone)>,
    /// Right-aligned facts (count, model, time, scope). Never part of title.
    pub meta: Option<String>,
    /// Optional second line. Aligns under the title, not under the cursor.
    pub description: Option<String>,
    /// Pre-styled description (search match highlight, diff colours). Row
    /// background is applied to every span; text colours and modifiers stay.
    pub description_line: Option<Line<'static>>,
    pub description_tone: RowTone,
    /// Let a description use as many lines as its display width needs. Help
    /// and Agents keep whole sentences; data tables stay on one line.
    pub wrap_description: bool,
    /// `Some(expanded)` reserves and paints the fold column. Lists that mix
    /// expanded and leaf rows must set `Some(false)` on leaves.
    pub fold: Option<bool>,
    pub tone: RowTone,
}

impl ListRow {
    pub(crate) fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            status: None,
            meta: None,
            description: None,
            description_line: None,
            description_tone: RowTone::Muted,
            wrap_description: false,
            fold: None,
            tone: RowTone::Default,
        }
    }

    pub(crate) fn status(mut self, glyph: impl Into<String>, tone: RowTone) -> Self {
        self.status = Some((glyph.into(), tone));
        self
    }

    pub(crate) fn meta(mut self, meta: impl Into<String>) -> Self {
        self.meta = Some(meta.into());
        self
    }

    pub(crate) fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub(crate) fn wrap_description(mut self) -> Self {
        self.wrap_description = true;
        self
    }

    pub(crate) fn description_line(mut self, line: Line<'static>) -> Self {
        self.description_line = Some(line);
        self
    }

    pub(crate) fn description_tone(mut self, tone: RowTone) -> Self {
        self.description_tone = tone;
        self
    }

    pub(crate) fn fold(mut self, expanded: bool) -> Self {
        self.fold = Some(expanded);
        self
    }

    pub(crate) fn tone(mut self, tone: RowTone) -> Self {
        self.tone = tone;
        self
    }
}

/// Focus/hover facts for one painted row. They come from the page and from
/// the shared pointer hover, never from the row's text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RowPaint {
    pub selected: bool,
    pub hovered: bool,
    /// True only for the list that currently owns the keyboard. A selected
    /// row in an unfocused list stays visible but loses the cursor and bold.
    pub focused: bool,
}

const CURSOR_COLS: usize = 2;
const FOLD_COLS: usize = 2;
const STATUS_COLS: usize = 2;
const META_GAP: usize = 2;

/// Paint facts for a row the page is about to draw. Hover is owned by the
/// shared pointer state, so every list uses the same key identity.
pub(crate) fn row_paint(state: &AppState, key: &str, selected: bool, focused: bool) -> RowPaint {
    RowPaint {
        selected,
        hovered: state
            .list_hover
            .as_ref()
            .is_some_and(|hover| hover.screen == state.active_screen && hover.key == key),
        focused,
    }
}

/// Paint one logical row into one or two lines.
///
/// Every span carries the row background, and the trailing pad owns the rest
/// of the line, so a selection or hover is a continuous band rather than a
/// text-only highlight. Width budgets use display columns, which keeps CJK,
/// emoji and combining marks from pushing the right meta outside the pane.
pub(crate) fn list_row_lines(
    row: &ListRow,
    paint: RowPaint,
    theme: &crate::theme::Theme,
    width: usize,
) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::default()];
    }
    let focused_cursor = paint.selected && paint.focused;
    let unfocused_selected = paint.selected && !paint.focused;
    let background = if focused_cursor {
        theme.surface.selection
    } else if paint.hovered || unfocused_selected {
        theme.surface.hover
    } else {
        theme.surface.canvas
    };
    let reverse = theme.monochrome && focused_cursor;
    let bg = if theme.monochrome {
        Color::Reset
    } else {
        background
    };
    let base = |mut style: Style| {
        style = style.bg(bg);
        if reverse {
            style = style.add_modifier(Modifier::REVERSED);
        } else if unfocused_selected && theme.monochrome {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        style
    };

    let title_color = if unfocused_selected {
        theme.text.secondary
    } else {
        row.tone.color(theme)
    };
    let title_style = base(Style::default().fg(title_color)).add_modifier(if focused_cursor {
        Modifier::BOLD
    } else {
        Modifier::empty()
    });
    let cursor_style = base(Style::default().fg(if paint.focused {
        theme.accent.primary
    } else {
        theme.text.muted
    }));
    let cursor = if focused_cursor {
        "❯ "
    } else if paint.selected {
        "› "
    } else {
        "  "
    };

    let fold = row.fold.map(|expanded| if expanded { "▾ " } else { "▸ " });
    let status = row.status.as_ref().map(|(glyph, tone)| {
        let mut text = sanitize_glyph(glyph);
        let used = UnicodeWidthStr::width(text.as_str());
        if used < STATUS_COLS {
            text.push_str(&" ".repeat(STATUS_COLS - used));
        }
        (text, *tone)
    });

    let prefix_w = CURSOR_COLS
        + row.fold.map_or(0, |_| FOLD_COLS)
        + row.status.as_ref().map_or(0, |_| STATUS_COLS);
    let avail = width.saturating_sub(prefix_w);
    let meta_text = row
        .meta
        .as_deref()
        .map(crate::render::sanitize_terminal_line)
        .filter(|text| !text.is_empty());
    let (title_text, meta_text) = match meta_text {
        None => (crate::render::truncate_display(&row.title, avail), None),
        Some(meta) => {
            let meta_w = UnicodeWidthStr::width(meta.as_str());
            let max_meta = (avail / 2).max(1);
            let meta = crate::render::truncate_display(&meta, max_meta);
            let meta_w = meta_w.min(max_meta);
            let title_room = avail.saturating_sub(meta_w + META_GAP).max(1);
            (
                crate::render::truncate_display(&row.title, title_room),
                Some(meta),
            )
        }
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(cursor.to_string(), cursor_style));
    if let Some(fold) = fold {
        spans.push(Span::styled(fold.to_string(), cursor_style));
    }
    if let Some((glyph, tone)) = status {
        spans.push(Span::styled(
            glyph,
            base(Style::default().fg(tone.color(theme))),
        ));
    }
    spans.push(Span::styled(title_text.clone(), title_style));
    let mut used = prefix_w + UnicodeWidthStr::width(title_text.as_str());
    let meta = meta_text.unwrap_or_default();
    if !meta.is_empty() {
        let meta_w = UnicodeWidthStr::width(meta.as_str());
        let gap = avail.saturating_sub(UnicodeWidthStr::width(title_text.as_str()) + meta_w);
        if gap > 0 {
            spans.push(Span::styled(" ".repeat(gap), base(Style::default())));
            used += gap;
        }
        spans.push(Span::styled(
            meta.clone(),
            base(Style::default().fg(if unfocused_selected {
                theme.text.muted
            } else {
                theme.text.secondary
            })),
        ));
        used += meta_w;
    }
    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            base(Style::default()),
        ));
    }
    let mut lines = vec![Line::from(spans)];

    if let Some(description_line) = row.description_line.as_ref() {
        let mut desc_spans = vec![Span::styled(" ".repeat(prefix_w), base(Style::default()))];
        let mut used = prefix_w;
        for span in &description_line.spans {
            let mut style = span.style;
            if style.bg.is_none() {
                style = style.bg(bg);
            }
            if reverse {
                style = style.add_modifier(Modifier::REVERSED);
            } else if unfocused_selected && theme.monochrome {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            let text = span.content.as_ref();
            let room = width.saturating_sub(used);
            if room == 0 {
                break;
            }
            let text = crate::render::truncate_display(text, room);
            used += UnicodeWidthStr::width(text.as_str());
            desc_spans.push(Span::styled(text, style));
        }
        if used < width {
            desc_spans.push(Span::styled(
                " ".repeat(width - used),
                base(Style::default()),
            ));
        }
        lines.push(Line::from(desc_spans));
    } else if let Some(description) = row.description.as_deref() {
        let indent = " ".repeat(prefix_w);
        let desc_room = width.saturating_sub(prefix_w);
        let clean = crate::render::sanitize_terminal_line(description);
        let pieces = if row.wrap_description {
            crate::render::wrap(&clean, desc_room.max(1))
        } else {
            vec![crate::render::truncate_display(&clean, desc_room)]
        };
        for piece in pieces {
            let mut desc_spans = vec![Span::styled(indent.clone(), base(Style::default()))];
            let desc_used = prefix_w + UnicodeWidthStr::width(piece.as_str());
            desc_spans.push(Span::styled(
                piece,
                base(Style::default().fg(row.description_tone.color(theme))),
            ));
            if desc_used < width {
                desc_spans.push(Span::styled(
                    " ".repeat(width - desc_used),
                    base(Style::default()),
                ));
            }
            lines.push(Line::from(desc_spans));
        }
    }
    lines
}

/// A status glyph is one safe cell. Control characters never reach the buffer.
fn sanitize_glyph(glyph: &str) -> String {
    let glyph = crate::render::sanitize_terminal_line(glyph);
    let mut out = String::new();
    for ch in glyph.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w == 0 {
            continue;
        }
        if w + UnicodeWidthStr::width(out.as_str()) > STATUS_COLS {
            break;
        }
        out.push(ch);
    }
    if out.is_empty() { " ".to_string() } else { out }
}

/// The one grouping line every sub-page uses: `label · N ────────`.
pub(crate) fn section_header_line(
    label: &str,
    count: Option<usize>,
    collapsed: Option<bool>,
    theme: &crate::theme::Theme,
    width: usize,
) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let fold = collapsed.map(|collapsed| if collapsed { "▸ " } else { "▾ " });
    let mut text = fold.unwrap_or("").to_string();
    text.push_str(&crate::render::sanitize_terminal_line(label));
    if let Some(count) = count {
        text.push_str(" · ");
        text.push_str(&count.to_string());
    }
    let text = crate::render::truncate_display(&text, width);
    let used = UnicodeWidthStr::width(text.as_str());
    let mut spans = vec![Span::styled(
        text,
        Style::default()
            .fg(theme.text.secondary)
            .add_modifier(Modifier::BOLD),
    )];
    if used < width {
        spans.push(Span::styled(
            format!(" {}", "─".repeat(width - used - 1)),
            Style::default().fg(theme.border.subtle),
        ));
    }
    Line::from(spans)
}

/// One label/value fact in a Document or Detail page.
///
/// The layout is display-column based, never a fixed count of ASCII spaces:
/// CJK labels, long values and unknown-width glyphs all measure the same way
/// they paint. A value that does not fit moves to its own wrapped rows.
#[derive(Debug, Clone)]
pub(crate) struct KeyValueRow {
    pub label: String,
    pub value: String,
    pub tone: RowTone,
}

impl KeyValueRow {
    pub(crate) fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            tone: RowTone::Default,
        }
    }

    pub(crate) fn tone(mut self, tone: RowTone) -> Self {
        self.tone = tone;
        self
    }
}

/// Paint aligned label/value rows for a Document or Detail page.
///
/// The label column is the widest label capped so at least a dozen value
/// columns remain. A label wider than the cap takes its own row; a long value
/// wraps under the value column with the label's width as its indent.
pub(crate) fn key_value_lines(
    rows: &[KeyValueRow],
    width: usize,
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    if width == 0 || rows.is_empty() {
        return Vec::new();
    }
    let max_label = rows
        .iter()
        .map(|row| UnicodeWidthStr::width(row.label.as_str()))
        .max()
        .unwrap_or(0);
    let label_col = max_label.min(width.saturating_sub(12)).min(24);
    let value_indent = label_col.saturating_add(2);
    let value_width = width.saturating_sub(value_indent).max(1);
    let mut out = Vec::new();
    for row in rows {
        let label = crate::render::text::sanitize_terminal_line(&row.label);
        let value = crate::render::text::sanitize_terminal_line(&row.value);
        let label_width = UnicodeWidthStr::width(label.as_str());
        let tone = Style::default().fg(row.tone.color(theme));
        let label_style = Style::default().fg(theme.text.muted);
        let wrapped = crate::render::text::wrap(&value, value_width);
        if label_width <= label_col {
            let lead = format!("{}{}", crate::render::pad_display(&label, label_col), "  ");
            let mut first = true;
            for row in wrapped {
                let prefix = if first {
                    lead.clone()
                } else {
                    " ".repeat(value_indent)
                };
                out.push(Line::from(vec![
                    Span::styled(prefix, label_style),
                    Span::styled(row, tone),
                ]));
                first = false;
            }
            if first {
                out.push(Line::from(Span::styled(lead, label_style)));
            }
        } else {
            out.push(Line::from(Span::styled(label, label_style)));
            for row in wrapped {
                out.push(Line::from(vec![Span::raw("  "), Span::styled(row, tone)]));
            }
        }
    }
    out
}

/// A list state that is not a list row. Pages must choose the kind from typed
/// state; the shared painter only gives it one visual grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListStateKind {
    Loading,
    Empty,
    FilterEmpty,
    Error,
    Partial,
    Stale,
}

impl ListStateKind {
    fn glyph(self) -> &'static str {
        match self {
            ListStateKind::Loading => "…",
            ListStateKind::Empty => "—",
            ListStateKind::FilterEmpty => "∅",
            ListStateKind::Error => "×",
            ListStateKind::Partial => "!",
            ListStateKind::Stale => "~",
        }
    }

    fn tone(self) -> RowTone {
        match self {
            ListStateKind::Loading | ListStateKind::Empty | ListStateKind::FilterEmpty => {
                RowTone::Muted
            }
            ListStateKind::Error => RowTone::Error,
            ListStateKind::Partial | ListStateKind::Stale => RowTone::Warning,
        }
    }
}

pub(crate) struct ListStateBlock<'a> {
    pub kind: ListStateKind,
    pub message: &'a str,
    pub detail: Option<&'a str>,
}

pub(crate) fn list_state_lines(
    block: ListStateBlock<'_>,
    theme: &crate::theme::Theme,
    width: usize,
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let color = block.kind.tone().color(theme);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("{} ", block.kind.glyph()),
            Style::default().fg(color),
        ),
        Span::styled(
            crate::render::sanitize_terminal_line(block.message),
            Style::default().fg(color),
        ),
    ])];
    if let Some(detail) = block.detail.filter(|detail| !detail.trim().is_empty()) {
        for row in crate::render::wrap(
            &crate::render::sanitize_terminal_line(detail),
            width.saturating_sub(2).max(1),
        ) {
            lines.push(Line::from(Span::styled(
                format!("  {row}"),
                Style::default().fg(theme.text.secondary),
            )));
        }
    }
    lines
}

/// Publish the full logical-object rect for every painted row. One object may
/// own several painted lines (description, wrapped help text), and a click on
/// any of them must select that one object.
pub(crate) struct RowHitSpec {
    pub line: usize,
    pub height: u16,
    pub key: String,
}

pub(crate) fn render_list_rows_with_spans(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    rows: &[RowHitSpec],
    focus: usize,
    theme: &crate::theme::Theme,
    hits: &mut Vec<(Rect, String)>,
) {
    let offset = list_scroll_offset(lines.len(), area.height as usize, focus);
    if area.width > 0 {
        for row in rows {
            if row.line < offset {
                continue;
            }
            let y = area.y + (row.line - offset) as u16;
            if y >= area.y.saturating_add(area.height) {
                continue;
            }
            let height = row
                .height
                .max(1)
                .min(area.y.saturating_add(area.height).saturating_sub(y));
            if height == 0 {
                continue;
            }
            hits.push((
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height,
                },
                row.key.clone(),
            ));
        }
    }
    render_pane_filled(frame, area, lines, offset, theme);
}

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
    render_pane_filled(frame, area, lines, scroll, &state.theme);
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
    theme: &crate::theme::Theme,
) {
    let offset = list_scroll_offset(lines.len(), area.height as usize, focus);
    render_pane_filled(frame, area, lines, offset, theme);
}

/// Render a selectable list and publish one hit per painted row.
///
/// The hit is the row's whole line, and its row number comes from the SAME
/// scroll offset the paint used — so a click can never land on a different row
/// than the one under the pointer, after a scroll, a resize or a data update.
/// A row scrolled out of the pane owns no hit at all.
///
/// `rows` is (line index within `lines`, the page's own row key). Pages whose
/// cursor is positional pass the index as the key; pages with stable row
/// identities pass the identity.
pub(crate) fn render_list_rows(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    rows: &[(usize, String)],
    focus: usize,
    theme: &crate::theme::Theme,
    hits: &mut Vec<(Rect, String)>,
) {
    let offset = list_scroll_offset(lines.len(), area.height as usize, focus);
    render_rows_at_offset(frame, area, lines, rows, offset, theme, hits);
}

/// The same list painter under a scroll offset the PAGE owns.
///
/// A page that is one scrolling document (its cursor is only one of the things
/// on it) cannot let the cursor pick the offset: the user's own scroll and a
/// section reveal must both be able to move the viewport. The hits come from
/// this offset, so a click still lands on the row under the pointer.
pub(crate) fn render_rows_at_offset(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    rows: &[(usize, String)],
    offset: usize,
    theme: &crate::theme::Theme,
    hits: &mut Vec<(Rect, String)>,
) {
    if area.width > 0 {
        for (line, key) in rows {
            if *line < offset {
                continue;
            }
            let y = area.y + (*line - offset) as u16;
            if y >= area.y.saturating_add(area.height) {
                continue;
            }
            hits.push((
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                },
                key.clone(),
            ));
        }
    }
    render_pane_filled(frame, area, lines, offset, theme);
}

/// Paint a scrollable pane by materialising every visible row (padded to the
/// full width). Paragraph alone only writes glyphs, so shorter content can leave
/// previous frame tails if Clear is skipped or cells get out of sync; filling
/// each row forces a complete overwrite of the viewport.
fn render_pane_filled(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    scroll: usize,
    theme: &crate::theme::Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let height = area.height as usize;
    let overflow = lines.len() > height;
    // The thumb sits in the column just outside the text when the card still
    // has a gutter there. On a pane that already touches the frame edge it
    // takes the last column, and the text yields that column.
    let bar_inside = overflow && area.x + area.width >= frame.area().width && area.width > 4;
    let text_width = if bar_inside {
        area.width.saturating_sub(1)
    } else {
        area.width
    };
    let width = text_width as usize;
    let mut visible: Vec<Line<'static>> = Vec::with_capacity(height);
    for row in 0..height {
        let line = lines
            .get(scroll + row)
            .cloned()
            .unwrap_or_else(|| Line::from(""));
        visible.push(pad_line_to_width(line, width));
    }
    let text_area = Rect {
        width: text_width,
        ..area
    };
    theme.paint_canvas(frame, text_area);
    frame.render_widget(Paragraph::new(visible), text_area);
    if overflow {
        paint_scrollbar(frame, area, lines.len(), scroll, theme);
    }
}

/// Paint a page-owned detail at its own offset. The offset is clamped here,
/// so a content shrink cannot leave the pane blank past the end.
pub(crate) fn render_pane_at_offset(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    offset: usize,
    theme: &crate::theme::Theme,
) {
    let max = lines.len().saturating_sub(area.height as usize);
    render_pane_filled(frame, area, lines, offset.min(max), theme);
}

/// The one-cell separator painted between two panes. It is never a hit target;
/// pages publish only the two content rects.
pub(crate) fn paint_split_divider(
    frame: &mut Frame,
    layout: &crate::layout::SplitLayout,
    theme: &crate::theme::Theme,
) {
    if !layout.dual || layout.divider.width == 0 || layout.divider.height == 0 {
        return;
    }
    let (symbol, style) = match layout.axis {
        crate::layout::SplitAxis::Horizontal => ("│", Style::default().fg(theme.border.normal)),
        crate::layout::SplitAxis::Vertical => ("─", Style::default().fg(theme.border.normal)),
    };
    let len = match layout.axis {
        crate::layout::SplitAxis::Horizontal => layout.divider.height,
        crate::layout::SplitAxis::Vertical => layout.divider.width,
    };
    // A one-column Rect clips a single long Paragraph to its first row. The
    // vertical divider therefore needs one line per row; the horizontal one
    // is already one line across its width.
    let lines: Vec<Line<'static>> = match layout.axis {
        crate::layout::SplitAxis::Horizontal => (0..len)
            .map(|_| Line::from(Span::styled(symbol, style)))
            .collect(),
        crate::layout::SplitAxis::Vertical => {
            vec![Line::from(Span::styled(symbol.repeat(len as usize), style))]
        }
    };
    frame.render_widget(Paragraph::new(lines), layout.divider);
}

/// A one-column thumb. Drawn on the pane's right edge when `total` is taller
/// than the pane, so a long card shows that it scrolls and where you are.
pub(crate) fn paint_scrollbar(
    frame: &mut Frame,
    area: Rect,
    total: usize,
    offset: usize,
    theme: &crate::theme::Theme,
) {
    let height = area.height as usize;
    if area.height == 0 || area.width == 0 || total <= height {
        return;
    }
    let inside = area.x + area.width >= frame.area().width;
    let x = if inside {
        if area.width <= 4 {
            return;
        }
        area.x + area.width - 1
    } else {
        area.x + area.width
    };
    if x >= frame.area().width {
        return;
    }
    let thumb = (height * height / total).max(1).min(height);
    let travel = height.saturating_sub(thumb);
    let max_scroll = total.saturating_sub(height);
    let top = if max_scroll == 0 {
        0
    } else {
        offset.min(max_scroll) * travel / max_scroll
    };
    let track = Style::default().fg(theme.text.disabled);
    let thumb_style = Style::default().fg(theme.text.secondary);
    for row in 0..height {
        let on_thumb = row >= top && row < top + thumb;
        let cell = Rect {
            x,
            y: area.y + row as u16,
            width: 1,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                if on_thumb { "┃" } else { "│" },
                if on_thumb { thumb_style } else { track },
            ))),
            cell,
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn a_tall_pane_paints_a_thumb_and_keeps_the_text() {
        let theme = crate::theme::Theme::no_color();
        let lines: Vec<Line<'static>> = (0..20).map(|i| Line::from(format!("row {i}"))).collect();
        let backend = TestBackend::new(20, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_pane_filled(frame, frame.area(), lines, 0, &theme))
            .unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "r");
        let thumb = (0..5).any(|y| buf.cell((19, y)).unwrap().symbol() == "┃");
        let track = (0..5).any(|y| buf.cell((19, y)).unwrap().symbol() == "│");
        assert!(thumb && track, "the overflow has no scrollbar");
    }

    fn row_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn key_values_align_cjk_labels_and_wrap_long_values() {
        let theme = crate::theme::Theme::dark();
        let rows = vec![
            KeyValueRow::new("状态", "执行中"),
            KeyValueRow::new("耗时", "42s"),
            KeyValueRow::new(
                "长路径",
                "src/一个非常长的模块路径/with-a-long-ascii-tail.rs:1234",
            ),
        ];
        let lines = key_value_lines(&rows, 32, &theme);
        let text = row_text(&lines);
        assert!(text.contains("状态"), "{text}");
        assert!(text.contains("执行中"), "{text}");
        assert!(text.contains("长路径"), "{text}");
        assert!(
            lines.iter().all(|line| {
                UnicodeWidthStr::width(
                    line.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                        .as_str(),
                ) <= 32
            }),
            "a key/value row exceeded the width: {text}"
        );
        let value_columns: Vec<usize> = lines
            .iter()
            .filter(|line| line.spans.len() >= 2)
            .filter_map(|line| {
                line.spans
                    .first()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            })
            .collect();
        assert!(
            value_columns.iter().all(|width| *width == 8),
            "every wrapped value should start at the shared column: {value_columns:?}"
        );
    }

    #[test]
    fn a_selected_row_owns_a_full_width_band() {
        let theme = crate::theme::Theme::dark();
        let row = ListRow::new("修复恢复机制")
            .status("●", RowTone::Info)
            .meta("运行中 · 2m")
            .description("deepseek-v4.1 · main · 12 轮");
        let lines = list_row_lines(
            &row,
            RowPaint {
                selected: true,
                hovered: false,
                focused: true,
            },
            &theme,
            30,
        );
        assert_eq!(lines.len(), 2);
        let first = &lines[0];
        let last = first.spans.last().unwrap();
        assert_eq!(last.style.bg, Some(theme.surface.selection));
        assert_eq!(first.spans[0].content.as_ref(), "❯ ");
        assert_eq!(
            lines[1].spans.last().unwrap().style.bg,
            Some(theme.surface.selection)
        );
        assert!(row_text(&lines).contains("deepseek-v4.1"));
    }

    #[test]
    fn hover_and_unfocused_selection_are_quieter_than_keyboard_focus() {
        let theme = crate::theme::Theme::dark();
        let row = ListRow::new("row");
        let hover = list_row_lines(
            &row,
            RowPaint {
                selected: false,
                hovered: true,
                focused: false,
            },
            &theme,
            20,
        );
        assert_eq!(
            hover[0].spans.last().unwrap().style.bg,
            Some(theme.surface.hover)
        );
        let unfocused = list_row_lines(
            &row,
            RowPaint {
                selected: true,
                hovered: false,
                focused: false,
            },
            &theme,
            20,
        );
        assert_eq!(unfocused[0].spans[0].content.as_ref(), "› ");
        assert_eq!(
            unfocused[0].spans.last().unwrap().style.bg,
            Some(theme.surface.hover)
        );
    }

    #[test]
    fn no_color_keeps_selection_visible_without_a_background() {
        let theme = crate::theme::Theme::no_color();
        let lines = list_row_lines(
            &ListRow::new("row"),
            RowPaint {
                selected: true,
                hovered: false,
                focused: true,
            },
            &theme,
            12,
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::REVERSED)),
            "monochrome selection must invert"
        );
    }

    #[test]
    fn cjk_and_long_meta_never_cross_the_row_width() {
        let theme = crate::theme::Theme::dark();
        let row = ListRow::new("这是一个很长很长的任务名称").meta("provider/model · main · 123 轮");
        for width in [10, 20, 33, 40] {
            let lines = list_row_lines(&row, RowPaint::default(), &theme, width);
            for line in lines {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                assert!(
                    UnicodeWidthStr::width(text.as_str()) <= width,
                    "width {width}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn section_headers_and_state_kinds_are_shared_and_distinct() {
        let theme = crate::theme::Theme::dark();
        let header = section_header_line("已连接", Some(3), Some(false), &theme, 24);
        let text: String = header.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("▾ 已连接 · 3"));
        assert_eq!(UnicodeWidthStr::width(text.as_str()), 24);
        let kinds = [
            ListStateKind::Loading,
            ListStateKind::Empty,
            ListStateKind::FilterEmpty,
            ListStateKind::Error,
            ListStateKind::Partial,
            ListStateKind::Stale,
        ];
        let glyphs: Vec<_> = kinds.iter().map(|kind| kind.glyph()).collect();
        assert_eq!(
            glyphs.len(),
            glyphs
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
        );
    }

    #[test]
    fn a_two_line_object_publishes_one_hit_for_both_lines() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let theme = crate::theme::Theme::dark();
        let lines = list_row_lines(
            &ListRow::new("title").description("description"),
            RowPaint::default(),
            &theme,
            20,
        );
        let rows = vec![RowHitSpec {
            line: 0,
            height: 2,
            key: "row-1".into(),
        }];
        let mut hits = Vec::new();
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_list_rows_with_spans(frame, frame.area(), lines, &rows, 0, &theme, &mut hits)
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0.height, 2);
        assert_eq!(hits[0].1, "row-1");
    }
}
