//! `/context` — the Context Inspector.
//!
//! The TUI never recomputes a token figure here: the runtime's kernel publishes
//! a [`leveler_model::ContextAccounting`] snapshot before each request, this
//! view renders it, and `Enter` is the only interaction — toggling disclosure
//! state that is presentation-only and never reaches the conversation.
//!
//! The Context Map is a drawing of the snapshot, not a second accounting. Every
//! cell is an equal share of the window the runtime declared, allotted by
//! [`crate::context_grid`]; the token figures beside it are the snapshot's own.

use std::collections::{HashMap, HashSet};

use leveler_client_protocol::{ClientCommand, CommandId};
use leveler_model::{ContextAccounting, ContextCategory, ContextPressure, TokenCountKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::context_grid::{self, GridShape};
use crate::i18n::UiText;
use crate::render::{
    pad_display, pad_line_to_width, render_list_focused, render_scrolled, truncate_display,
};
use crate::state::AppState;
use crate::theme::Theme;

#[derive(Debug, Clone, Default)]
pub struct ContextView {
    /// The latest accounting snapshot the runtime published.
    pub loaded: Option<ContextAccounting>,
    /// Query this view owns; foreign/stale `ContextLoaded` events are ignored.
    pub pending_query_id: Option<CommandId>,
    /// Disclosure state for categories that have children. Presentation only.
    pub expanded: HashSet<String>,
    /// Selected row index over the visible (flattened) rows.
    pub selected: usize,
}

/// Issue the `QueryContext` this view owns.
pub fn issue_query(state: &mut AppState) -> ClientCommand {
    let query_id = CommandId::generate();
    state.context.pending_query_id = Some(query_id.clone());
    ClientCommand::QueryContext {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
    }
}

/// One visible category row after applying disclosure state. `top` carries the
/// top-level ancestor so a nested leaf keeps its parent's map colour.
#[derive(Debug, Clone)]
struct Row {
    key: String,
    top: String,
    depth: usize,
    label: String,
    tokens: u64,
    has_children: bool,
}

fn visible_rows(view: &ContextView) -> Vec<Row> {
    let mut out = Vec::new();
    if let Some(acc) = &view.loaded {
        for cat in &acc.categories {
            flatten(cat, &cat.name, 0, &view.expanded, &mut out);
        }
    }
    out
}

fn flatten(
    cat: &ContextCategory,
    top: &str,
    depth: usize,
    expanded: &HashSet<String>,
    out: &mut Vec<Row>,
) {
    out.push(Row {
        key: cat.name.clone(),
        top: top.to_string(),
        depth,
        label: cat.label.clone(),
        tokens: cat.tokens,
        has_children: !cat.children.is_empty(),
    });
    if expanded.contains(&cat.name) {
        for child in &cat.children {
            flatten(child, top, depth + 1, expanded, out);
        }
    }
}

pub fn move_sel(view: &mut ContextView, delta: isize) {
    let n = visible_rows(view).len();
    if n == 0 {
        view.selected = 0;
        return;
    }
    let next = view.selected as isize + delta;
    view.selected = next.clamp(0, (n - 1) as isize) as usize;
}

/// `Enter` on a row with children toggles it; anything else is a no-op.
pub fn toggle_selected(view: &mut ContextView) {
    let rows = visible_rows(view);
    let Some(row) = rows.get(view.selected) else {
        return;
    };
    if row.has_children {
        if view.expanded.contains(&row.key) {
            view.expanded.remove(&row.key);
        } else {
            view.expanded.insert(row.key.clone());
        }
    }
    clamp(view);
}

pub fn clamp(view: &mut ContextView) {
    let n = visible_rows(view).len();
    if n == 0 {
        view.selected = 0;
    } else if view.selected >= n {
        view.selected = n - 1;
    }
}

/// The built screen: rendered lines plus the line the selection sits on, so
/// `render_list_focused` can keep it visible.
struct ContextScreen {
    lines: Vec<Line<'static>>,
    focus: Option<usize>,
}

/// The map colour for each top-level category, in the snapshot's own display
/// order (largest first). The biggest consumer therefore always takes the
/// primary accent; the rest follow a fixed, restrained ramp. Children inherit
/// their top-level ancestor's colour.
fn top_colors(acc: &ContextAccounting, theme: &Theme) -> HashMap<String, Color> {
    let palette = [
        theme.accent.primary,
        theme.accent.secondary,
        theme.status.info,
        theme.accent.subtle,
        theme.text.secondary,
    ];
    acc.categories
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.clone(), palette[i % palette.len()]))
        .collect()
}

pub fn render_context_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let t = state.t();

    // Shared Secondary Surface chrome: identity + parent status on top, hints
    // at the bottom, padded content in between. This page owns only its body.
    let page = crate::secondary::SecondaryPage {
        title: t.context_title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: t.context_footer_hint,
    };
    let layout = crate::secondary::layout(area, 0);
    crate::secondary::draw_header(frame, &layout, &page, theme);
    crate::secondary::draw_footer(frame, &layout, &page, theme);
    let body = layout.content;

    let Some(acc) = state.context.loaded.as_ref() else {
        let msg = if state.context.pending_query_id.is_some() {
            t.context_loading
        } else {
            t.context_empty
        };
        let lines = vec![Line::from(Span::styled(
            msg,
            Style::default().fg(theme.text.secondary),
        ))];
        render_scrolled(frame, body, state, lines);
        return;
    };

    let screen = context_screen(acc, &state.context, body.width as usize, theme, t);
    match screen.focus {
        Some(focus) => render_list_focused(frame, body, screen.lines, focus, theme),
        None => render_scrolled(frame, body, state, screen.lines),
    }
}

/// Build the inspector body. `width` is the pane's full column count; the map
/// and the breakdown share it side by side when there is room, and stack when
/// there is not.
fn context_screen(
    acc: &ContextAccounting,
    view: &ContextView,
    width: usize,
    theme: &Theme,
    t: &UiText,
) -> ContextScreen {
    let dim = Style::default().fg(theme.text.muted);
    let secondary = Style::default().fg(theme.text.secondary);
    let window = acc.context_window_tokens.filter(|w| *w > 0);

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Header: the model, the real numbers, and the runtime-derived pressure.
    let used = fmt_tokens(acc.used_tokens);
    let mut head = vec![Span::styled(
        acc.model.to_string(),
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    )];
    match window {
        Some(w) => {
            let pct = acc.used_tokens as f64 * 100.0 / w as f64;
            head.push(Span::styled(
                format!(" · {used} / {} · {pct:.1}%", fmt_tokens(w)),
                Style::default().fg(theme.text.primary),
            ));
        }
        None => {
            head.push(Span::styled(
                format!(" · {used} {}", t.context_estimated),
                Style::default().fg(theme.text.primary),
            ));
        }
    }
    head.push(Span::styled(" · ", secondary));
    head.push(pressure_span(acc.pressure, t, theme));
    lines.push(Line::from(head));

    if let Some(free) = acc.free_tokens {
        lines.push(Line::from(Span::styled(
            format!("{} {}", t.context_free, fmt_tokens(free)),
            dim,
        )));
    }
    lines.push(Line::from(""));

    let colors = top_colors(acc, theme);
    let rows = visible_rows(view);

    // Decide the map's geometry before building the breakdown: when the two
    // share a row the breakdown is narrower, and a row laid out for the full
    // pane would have its token columns clipped away.
    const GAP: usize = 3;
    let shape = window.map(|_| GridShape::for_width(width));
    let left_w = shape.map(|s| s.cols * 2 - 1).unwrap_or(0);
    let side_by_side = shape.is_some() && left_w + GAP + 34 <= width;
    let right_w = if side_by_side {
        width.saturating_sub(left_w + GAP)
    } else {
        width
    };

    // The map needs a window: a grid drawn against an unknown total would be a
    // fabricated percentage. Without one, the breakdown stands alone.
    let grid = match (window, shape) {
        (Some(w), Some(shape)) => {
            let slices = context_grid::slices_from(acc);
            let cells = context_grid::draw(&slices, shape, w, acc.compact_at_tokens);
            Some(grid_lines(&cells, shape, &colors, theme))
        }
        _ => None,
    };

    let breakdown = breakdown_lines(acc, view, &rows, &colors, window, right_w, theme, t);

    let header = lines.len();
    let (body, focus) = compose_body(grid, breakdown, side_by_side, left_w, header, view.selected);
    lines.extend(body);

    lines.push(Line::from(""));
    if let Some(at) = acc.compact_at_tokens {
        let pct = window
            .map(|w| format!(" · {:.1}%", at as f64 * 100.0 / w as f64))
            .unwrap_or_default();
        lines.push(Line::from(Span::styled(
            format!("◈ {} {}{pct}", t.context_compact_at, fmt_tokens(at)),
            secondary,
        )));
    }
    lines.push(Line::from(Span::styled(
        accounting_kind(acc.token_count_kind, t),
        dim,
    )));
    lines.push(Line::from(Span::styled(compaction_line(acc, t), dim)));
    lines.push(Line::from(Span::styled(t.context_hint.to_string(), dim)));

    let lines = lines
        .into_iter()
        .map(|line| pad_line_to_width(line, width))
        .collect();
    ContextScreen { lines, focus }
}

/// Compose the map and the breakdown. `side_by_side` is decided by the caller
/// (it needs the breakdown's width before the rows are laid out). Returns the
/// body lines and the index (into the returned lines) the selection lands on.
fn compose_body(
    grid: Option<Vec<Line<'static>>>,
    breakdown: Vec<Line<'static>>,
    side_by_side: bool,
    left_w: usize,
    header: usize,
    selected: usize,
) -> (Vec<Line<'static>>, Option<usize>) {
    const GAP: usize = 3;

    if side_by_side {
        let grid = grid.expect("side-by-side needs a map");
        let body_len = grid.len().max(breakdown.len());
        let mut body = Vec::with_capacity(body_len);
        for i in 0..body_len {
            let mut spans: Vec<Span<'static>> = Vec::new();
            match grid.get(i) {
                Some(g) => {
                    spans.extend(g.spans.clone());
                    let pad = left_w.saturating_sub(line_width(g)) + GAP;
                    spans.push(Span::raw(" ".repeat(pad)));
                }
                None if !breakdown.is_empty() => {
                    spans.push(Span::raw(" ".repeat(left_w + GAP)));
                }
                None => {}
            }
            if let Some(b) = breakdown.get(i) {
                spans.extend(b.spans.clone());
            }
            body.push(Line::from(spans));
        }
        // Heading sits at breakdown row 0, so the selected category is at
        // `1 + selected`.
        return (body, Some(header + 1 + selected));
    }

    let mut body = Vec::new();
    let mut start = 0usize;
    if let Some(grid) = grid {
        start = grid.len();
        body.extend(grid);
        body.push(Line::from(""));
        start += 1;
    }
    // Heading at breakdown row 0, so the selected category is at `start + 1`.
    let focus = header + start + 1 + selected;
    body.extend(breakdown);
    (body, Some(focus))
}

/// Render the map as rows of glyphs, each row joined by one space.
fn grid_lines(
    cells: &[context_grid::GridCell],
    shape: GridShape,
    colors: &HashMap<String, Color>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let free_style = Style::default().fg(theme.text.disabled);
    let marker_style = Style::default().fg(theme.text.secondary);
    let mut lines = Vec::with_capacity(shape.rows);
    for row in cells.chunks(shape.cols) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            let (glyph, style) = if cell.fold_boundary {
                ("◈", marker_style)
            } else if cell.free {
                ("○", free_style)
            } else {
                (
                    "●",
                    Style::default().fg(*colors.get(&cell.key).unwrap_or(&theme.text.primary)),
                )
            };
            spans.push(Span::styled(glyph, style));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn breakdown_lines(
    acc: &ContextAccounting,
    view: &ContextView,
    rows: &[Row],
    colors: &HashMap<String, Color>,
    window: Option<u64>,
    width: usize,
    theme: &Theme,
    t: &UiText,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(Line::from(Span::styled(
        t.context_breakdown.to_string(),
        Style::default().fg(theme.text.secondary),
    )));
    for (i, row) in rows.iter().enumerate() {
        let color = *colors.get(&row.top).unwrap_or(&theme.text.primary);
        let expanded = view.expanded.contains(&row.key);
        lines.push(row_line(
            row,
            color,
            expanded,
            window,
            i == view.selected,
            width,
            theme,
        ));
    }
    if let Some(free) = acc.free_tokens {
        lines.push(free_line(free, window, theme, t));
    }
    lines
}

fn row_line(
    row: &Row,
    color: Color,
    expanded: bool,
    window: Option<u64>,
    selected: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let indent = "  ".repeat(row.depth);
    let mark = if selected { ">" } else { " " };
    // Disclosure glyph reflects the row's own expansion state, never selection:
    // a selected leaf has nothing to expand and a collapsed parent must not
    // read as open just because the cursor is on it.
    let arrow = if row.has_children {
        if expanded { "▾" } else { "▸" }
    } else {
        " "
    };
    let prefix_w = 1 + indent.len() + 2;
    let label_col = width.saturating_sub(prefix_w + 2 + 8 + 2 + 6).clamp(6, 40);
    let label = truncate_display(&row.label, label_col);
    let label = pad_display(&label, label_col);
    let (tokens, pct) = token_pct(row.tokens, window);

    let style = if selected {
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text.primary)
    };
    let swatch = if row.depth == 0 { "● " } else { "  " };
    let mut spans = vec![
        Span::styled(mark.to_string(), style),
        Span::styled(format!("{indent}{arrow} "), style),
        Span::styled(swatch.to_string(), Style::default().fg(color)),
        Span::styled(label, style),
        Span::styled(
            format!("{tokens:>8}"),
            Style::default().fg(theme.text.secondary),
        ),
    ];
    if !pct.is_empty() {
        spans.push(Span::styled(
            format!("  {pct:>6}"),
            Style::default().fg(theme.text.secondary),
        ));
    }
    Line::from(spans)
}

fn free_line(free: u64, window: Option<u64>, theme: &Theme, t: &UiText) -> Line<'static> {
    let (tokens, pct) = token_pct(free, window);
    let mut spans = vec![
        Span::raw(" "),
        Span::raw("  "),
        Span::styled("○ ", Style::default().fg(theme.text.disabled)),
        Span::styled(
            format!("{} ", t.context_free),
            Style::default().fg(theme.text.muted),
        ),
        Span::styled(
            format!("{tokens:>8}"),
            Style::default().fg(theme.text.muted),
        ),
    ];
    if !pct.is_empty() {
        spans.push(Span::styled(
            format!("  {pct:>6}"),
            Style::default().fg(theme.text.muted),
        ));
    }
    Line::from(spans)
}

/// `(compact token text, percent-of-window text)`. The percent is empty when
/// the window is unknown — a share of an unknown total is not a fact.
fn token_pct(tokens: u64, window: Option<u64>) -> (String, String) {
    let pct = match window {
        Some(w) if w > 0 => format!("{:.1}%", tokens as f64 * 100.0 / w as f64),
        _ => String::new(),
    };
    (fmt_tokens(tokens), pct)
}

/// Context magnitudes, formatted once: `999`, `1.2k`, `51.8k`, `1.2M`.
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        trim_zero(n as f64 / 1_000_000.0, "M")
    } else if n >= 1_000 {
        trim_zero(n as f64 / 1_000.0, "k")
    } else {
        n.to_string()
    }
}

fn trim_zero(v: f64, suffix: &str) -> String {
    let s = format!("{v:.1}");
    let s = s.strip_suffix(".0").unwrap_or(&s);
    format!("{s}{suffix}")
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

fn pressure_span(pressure: ContextPressure, t: &UiText, theme: &Theme) -> Span<'static> {
    let (label, color) = match pressure {
        ContextPressure::Normal => (t.context_pressure_normal, theme.text.muted),
        ContextPressure::Warning => (t.context_pressure_warning, theme.status.warning),
        ContextPressure::Critical => (t.context_pressure_critical, theme.status.warning),
    };
    Span::styled(label.to_string(), Style::default().fg(color))
}

fn accounting_kind(kind: TokenCountKind, t: &UiText) -> String {
    match kind {
        TokenCountKind::Exact => "Exact tokens".to_string(),
        TokenCountKind::Estimated => t.context_estimated.to_string(),
        TokenCountKind::Mixed => "Mixed accounting · total exact · breakdown estimated".to_string(),
    }
}

fn compaction_line(acc: &ContextAccounting, t: &UiText) -> String {
    match &acc.last_compaction {
        Some(record) => {
            let before = fmt_tokens(record.before_tokens);
            let after = fmt_tokens(record.after_tokens);
            let pct = record.reclaimed_ratio() * 100.0;
            format!(
                "{} {} → {} (-{pct:.1}%)",
                t.context_last_compact, before, after
            )
        }
        None => format!("{} {}", t.context_last_compact, t.context_no_compaction),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_model::{CompactionRecord, ModelRef};

    fn cat(
        name: &str,
        label: &str,
        tokens: u64,
        children: Vec<ContextCategory>,
    ) -> ContextCategory {
        ContextCategory {
            name: name.to_string(),
            label: label.to_string(),
            tokens,
            calls: 0,
            children,
        }
    }

    /// A realistic snapshot: Messages (with a tool-result drill-down) is the
    /// largest consumer, then tool definitions, then system. Free is the
    /// remainder of the declared window.
    fn snapshot() -> ContextAccounting {
        let tool_results = cat(
            "tool_results",
            "Tool results",
            18_600,
            vec![
                cat("read_file", "read_file", 10_700, vec![]),
                cat("run_command", "run_command", 6_200, vec![]),
                cat("other", "other", 1_700, vec![]),
            ],
        );
        let messages = cat(
            "messages",
            "Messages",
            51_800,
            vec![
                cat("user", "User", 5_800, vec![]),
                cat("assistant", "Assistant", 17_100, vec![]),
                cat("tool_calls", "Tool calls", 7_300, vec![]),
                tool_results,
                cat("compaction_summary", "Compaction summary", 3_000, vec![]),
            ],
        );
        let categories = vec![
            messages,
            cat("tool_definitions", "Tool definitions", 6_100, vec![]),
            cat("system", "System", 7_200, vec![]),
        ];
        let used: u64 = categories.iter().map(|c| c.tokens).sum();
        ContextAccounting {
            model: ModelRef::new("deepseek", "deepseek-chat"),
            context_window_tokens: Some(128_000),
            compact_at_tokens: Some(64_000),
            used_tokens: used,
            free_tokens: Some(128_000 - used),
            token_count_kind: TokenCountKind::Estimated,
            pressure: ContextPressure::Warning,
            categories,
            last_compaction: Some(CompactionRecord {
                before_tokens: 142_600,
                after_tokens: 53_400,
            }),
        }
    }

    fn render(acc: &ContextAccounting, view: &ContextView, width: usize) -> Vec<Line<'static>> {
        context_screen(
            acc,
            view,
            width,
            &Theme::no_color(),
            crate::Locale::En.text(),
        )
        .lines
    }

    fn glyph_count(lines: &[Line<'_>], glyph: &str) -> usize {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.content.as_ref() == glyph)
            .count()
    }

    #[test]
    fn lines_never_exceed_the_pane_width() {
        let acc = snapshot();
        let mut view = ContextView {
            loaded: Some(acc.clone()),
            ..ContextView::default()
        };
        view.expanded.insert("messages".to_string());
        for width in [120usize, 80, 60, 40, 20] {
            for line in render(&acc, &view, width) {
                let w: usize = line
                    .spans
                    .iter()
                    .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                    .sum();
                assert!(w <= width, "width={width} line={w}: {line:?}");
            }
        }
    }

    #[test]
    fn the_map_has_one_cell_per_share() {
        let acc = snapshot();
        let view = ContextView {
            loaded: Some(acc.clone()),
            ..ContextView::default()
        };
        let shape = GridShape::for_width(120);
        let lines = render(&acc, &view, 120);
        let used_cells = glyph_count(&lines, "●");
        let free_cells = glyph_count(&lines, "○");
        let markers = glyph_count(&lines, "◈");
        // Swatches are painted as `● ` / `○ ` (glyph plus space), so an exact
        // glyph match counts only the map's own cells.
        assert_eq!(
            used_cells + free_cells + markers,
            shape.cells(),
            "map cells must equal the shape's cell count"
        );
    }

    #[test]
    fn unknown_window_draws_no_map_and_no_percentage() {
        let mut acc = snapshot();
        acc.context_window_tokens = None;
        acc.compact_at_tokens = None;
        acc.free_tokens = None;
        let view = ContextView {
            loaded: Some(acc.clone()),
            ..ContextView::default()
        };
        let lines = render(&acc, &view, 80);
        assert_eq!(glyph_count(&lines, "○"), 0, "no map without a window");
        assert_eq!(glyph_count(&lines, "◈"), 0);
        let header: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !header.contains('%'),
            "a share of an unknown window is not a fact: {header}"
        );
    }

    #[test]
    fn zero_values_do_not_panic() {
        let acc = ContextAccounting {
            model: ModelRef::new("m", "m"),
            context_window_tokens: Some(0),
            compact_at_tokens: None,
            used_tokens: 0,
            free_tokens: Some(0),
            token_count_kind: TokenCountKind::Estimated,
            pressure: ContextPressure::Normal,
            categories: Vec::new(),
            last_compaction: None,
        };
        let view = ContextView {
            loaded: Some(acc.clone()),
            ..ContextView::default()
        };
        let _ = render(&acc, &view, 80);
    }

    #[test]
    fn disclosure_adds_the_child_rows() {
        let acc = snapshot();
        let collapsed = ContextView {
            loaded: Some(acc.clone()),
            ..ContextView::default()
        };
        let mut expanded = collapsed.clone();
        expanded.expanded.insert("messages".to_string());
        let flat = |v: &ContextView| {
            visible_rows(v)
                .iter()
                .map(|r| r.key.clone())
                .collect::<Vec<_>>()
        };
        assert!(!flat(&collapsed).contains(&"tool_results".to_string()));
        assert!(flat(&expanded).contains(&"tool_results".to_string()));
        // The parent's token total is unchanged by disclosure.
        let before = render(&acc, &collapsed, 100);
        let after = render(&acc, &expanded, 100);
        assert!(after.len() > before.len());
    }

    #[test]
    fn enter_toggles_and_selection_clamps() {
        let acc = snapshot();
        let mut view = ContextView {
            loaded: Some(acc),
            ..ContextView::default()
        };
        assert_eq!(visible_rows(&view).len(), 3, "three top-level categories");
        view.selected = 0;
        toggle_selected(&mut view);
        assert!(view.expanded.contains("messages"));
        assert!(visible_rows(&view).len() > 3, "children became visible");
        toggle_selected(&mut view);
        assert!(!view.expanded.contains("messages"));
        move_sel(&mut view, -5);
        assert_eq!(view.selected, 0);
        move_sel(&mut view, 99);
        assert_eq!(view.selected, 2);
    }

    #[test]
    fn compaction_line_reports_reclaimed_share() {
        let acc = snapshot();
        let line = compaction_line(&acc, crate::Locale::En.text());
        assert!(line.contains("142.6k"), "{line}");
        assert!(line.contains("53.4k"), "{line}");
        assert!(line.contains("62.5%") || line.contains("62.6%"), "{line}");
    }

    #[test]
    fn tokens_format_compactly() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1k");
        assert_eq!(fmt_tokens(1_200), "1.2k");
        assert_eq!(fmt_tokens(51_842), "51.8k");
        assert_eq!(fmt_tokens(1_200_000), "1.2M");
    }
}
