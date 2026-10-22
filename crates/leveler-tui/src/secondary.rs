//! Shared chrome for Secondary Surfaces.
//!
//! A Secondary Surface is a full-viewport page opened from the main workbench
//! that owns independent content and is explicitly dismissed. It is distinct
//! from an Overlay/Picker (a small floating decision box) and from an Inline
//! Panel (a block embedded in the main workbench).
//!
//! The shell gives every Secondary Surface the same product language:
//!
//! ```text
//! ← <identity>                            <parent / global status>
//! ──────────────────────────────────────────────────────────────────
//!
//!   <padded content>
//!
//! ──────────────────────────────────────────────────────────────────
//!   <interaction hints>
//! ```
//!
//! Ownership: the shell owns the viewport split, header identity, the two
//! rules, the horizontal safe margin, the footer hint, and the content rect.
//! It owns no page logic, no business state, and no key handling — each
//! surface still builds its own body and (optionally) its own composer.
//!
//! ONE content rect bounds every slot. The nav glyph, the title, the parent
//! status, both rules, the body and the footer hints all start and end on the
//! same safe margin, so the page reads as one column of content rather than as
//! chrome that owns the terminal edge and a body that does not. The margin
//! shrinks with the terminal ([`padding_x`]), and nothing is drawn twice
//! inset — a nested component may add its own indent, but never the page's.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::state::AppState;
use crate::status_line::{StatusPhase, status_lines, status_phase};
use crate::theme::Theme;

/// The safe horizontal margin for a page at `width`, applied to EVERY slot —
/// nav glyph, title, status, rules, body and footer hints.
///
/// The margin follows the terminal so a wide screen gets breathing room while
/// a narrow one keeps its usable columns: 2 columns from 80 up, 1 in the
/// 40–79 band, and none below that, where every column is needed for the
/// content and the nav to stay readable. One function, so no page can pick a
/// different margin from its neighbour's.
pub(crate) fn padding_x(width: u16) -> u16 {
    if width >= 80 {
        crate::layout::WORKSPACE_GUTTER_X
    } else if width >= 40 {
        1
    } else {
        0
    }
}

/// One Secondary Surface's identity and interaction contract.
pub(crate) struct SecondaryPage<'a> {
    /// Left identity, e.g. `btw · 旁路线程`. Already localized.
    pub title: &'a str,
    /// Right-aligned parent / global status — the main run's own words.
    pub status: Vec<Span<'static>>,
    /// Footer interaction hint. Only bindings that really exist.
    pub hint: &'a str,
}

/// The rects a Secondary Surface draws into. EVERY slot is already inset by
/// the page's safe margin, so a page (or the shell itself) must never inset a
/// slot again.
pub(crate) struct SecondaryLayout {
    pub header: Rect,
    pub rule_top: Rect,
    pub content: Rect,
    pub composer: Rect,
    pub rule_bottom: Rect,
    pub footer: Rect,
}

/// The display columns a page's footer can use at terminal `width`.
pub(crate) fn footer_width(width: u16) -> usize {
    width.saturating_sub(2 * padding_x(width)) as usize
}

/// Split `area` into the shared Secondary Surface slots, with one footer row.
///
/// The vertical split happens first, then the safe margin is applied once, to
/// every slot — the rules included, so a rule never runs past the content it
/// separates.
pub(crate) fn layout(area: Rect, composer_rows: u16) -> SecondaryLayout {
    layout_with_footer(area, composer_rows, 1)
}

/// [`layout`] with an explicit footer height.
///
/// A page whose interaction hint needs more than one row passes the row count
/// it produced; every other page keeps the default single row. The content
/// loses exactly the rows the footer gained.
pub(crate) fn layout_with_footer(
    area: Rect,
    composer_rows: u16,
    footer_rows: u16,
) -> SecondaryLayout {
    let footer_rows = footer_rows.max(1);
    let composer_rows = composer_rows.min(
        area.height
            .saturating_sub(4 + footer_rows.saturating_sub(1)),
    );
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // rule above the body
        Constraint::Min(1),    // content
        Constraint::Length(composer_rows),
        Constraint::Length(1), // rule above the footer
        Constraint::Length(footer_rows),
    ])
    .split(area);
    let pad = padding_x(area.width);
    let inset = |r: Rect| crate::layout::horizontal_inset(r, pad);
    SecondaryLayout {
        header: inset(chunks[0]),
        rule_top: inset(chunks[1]),
        content: inset(chunks[2]),
        composer: inset(chunks[3]),
        rule_bottom: inset(chunks[4]),
        footer: inset(chunks[5]),
    }
}

/// Draw the identity header, its right-aligned parent status, and the rule
/// below it — and publish the painted `← title` nav region as the page's back
/// hit.
///
/// Drawing and publishing are the same call on purpose: the nav glyph is the
/// page's only control, so a page must not be able to paint a `←` the mouse
/// cannot use. The hit is exactly the painted glyph + title (truncation
/// included), never the whole row, because the right half is the status.
///
/// Returns the painted region for callers that need the geometry; clicking it
/// runs the same one-level back as Esc.
pub(crate) fn draw_header(
    frame: &mut Frame,
    state: &mut AppState,
    l: &SecondaryLayout,
    page: &SecondaryPage,
) -> Option<Rect> {
    let theme = &state.theme;
    let back = paint_header(frame, l, page, theme);
    state.back_hit = back.map(|rect| (rect.y, rect.x, rect.x + rect.width));
    back
}

/// The header's paint, without the hit publication — the body of
/// [`draw_header`], kept separate only so the borrow of `state.theme` ends
/// before the hit is recorded.
fn paint_header(
    frame: &mut Frame,
    l: &SecondaryLayout,
    page: &SecondaryPage,
    theme: &Theme,
) -> Option<Rect> {
    if l.header.height == 0 {
        return None;
    }
    // The navigation row owns the full width: the back glyph anchors the left
    // edge, the parent status the right. Unlike the body it is not inset, so
    // the nav reads as chrome above the page rather than content inside it.
    let inner = l.header;
    if inner.width == 0 {
        draw_rule(frame, l.rule_top, theme);
        return None;
    }
    let left_text = format!("← {}", page.title);
    // A status line can carry trailing padding; measure only what is visible so
    // padding never forces the whole status off the row.
    let right_plain: String = page.status.iter().map(|s| s.content.as_ref()).collect();
    let right_w = UnicodeWidthStr::width(right_plain.trim_end());
    let right_visible = right_w > 0 && right_w <= inner.width as usize;
    // The parent status is the right anchor. It yields when the row cannot hold
    // identity + status with a gap; the identity alone tells the user where
    // they are, which is the harder fact to recover.
    let left_room = if right_visible {
        (inner.width as usize).saturating_sub(right_w + 1)
    } else {
        inner.width as usize
    };
    if left_room > 0 {
        let left = Span::styled(
            crate::render::truncate_display(&left_text, left_room),
            Style::default()
                .fg(theme.accent.secondary)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_widget(
            Paragraph::new(Line::from(left)),
            Rect {
                x: inner.x,
                y: inner.y,
                width: left_room as u16,
                height: 1,
            },
        );
    }
    if right_visible {
        frame.render_widget(
            Paragraph::new(Line::from(page.status.clone())),
            Rect {
                x: inner.x + inner.width - right_w as u16,
                y: inner.y,
                width: right_w as u16,
                height: 1,
            },
        );
    }
    draw_rule(frame, l.rule_top, theme);
    // The hit is the glyph + title as painted (truncation included), never the
    // whole row: the right half belongs to the status.
    let painted = crate::render::truncate_display(&left_text, left_room.min(inner.width as usize));
    let nav_w = UnicodeWidthStr::width(painted.as_str()) as u16;
    (nav_w > 0).then_some(Rect {
        x: inner.x,
        y: inner.y,
        width: nav_w,
        height: 1,
    })
}

/// The separator the product uses between interaction hints.
pub(crate) const HINT_SEPARATOR: &str = " · ";

/// Pack interaction hints into rows of at most `width` display columns.
///
/// A footer must never hide a binding: whole segments (`↑↓ 移动`, `A 接受`)
/// move to the next row instead of being ellipsized. Only when one segment is
/// wider than the terminal — far below the supported minimum — is it split by
/// display width, so even then no key disappears.
pub(crate) fn wrap_hint<'a>(
    segments: impl IntoIterator<Item = &'a str>,
    width: usize,
) -> Vec<String> {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut row_width = 0usize;
    for segment in segments {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let segment_width = UnicodeWidthStr::width(segment);
        let separator_width = if row.is_empty() {
            0
        } else {
            UnicodeWidthStr::width(HINT_SEPARATOR)
        };
        if !row.is_empty() && row_width + separator_width + segment_width > width {
            rows.push(std::mem::take(&mut row));
            row_width = 0;
        }
        if segment_width > width {
            let mut rest = segment;
            loop {
                let (piece, tail) = crate::render::text::take_display_prefix(rest, width);
                let piece_width = UnicodeWidthStr::width(piece.as_str());
                if tail.is_empty() {
                    row = piece;
                    row_width = piece_width;
                    break;
                }
                rows.push(piece);
                rest = tail;
            }
            continue;
        }
        if !row.is_empty() {
            row.push_str(HINT_SEPARATOR);
            row_width += UnicodeWidthStr::width(HINT_SEPARATOR);
        }
        row.push_str(segment);
        row_width += segment_width;
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// Draw the rule above the footer and the interaction hint below it.
///
/// `page.hint` may carry one row per line; the footer rect must be tall
/// enough (see [`layout_with_footer`]). Rows produced by [`wrap_hint`] already
/// fit, so they paint in full.
pub(crate) fn draw_footer(
    frame: &mut Frame,
    l: &SecondaryLayout,
    page: &SecondaryPage,
    theme: &Theme,
) {
    draw_rule(frame, l.rule_bottom, theme);
    if l.footer.height == 0 {
        return;
    }
    let inner = l.footer;
    if inner.width == 0 {
        return;
    }
    let style = Style::default().fg(theme.text.muted);
    let rows: Vec<Line> = page
        .hint
        .lines()
        .take(inner.height as usize)
        .map(|row| {
            Line::from(Span::styled(
                crate::render::truncate_display(row, inner.width as usize),
                style,
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), inner);
}

/// A full-width horizontal rule.
fn draw_rule(frame: &mut Frame, area: Rect, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(theme.border.normal),
        ))),
        area,
    );
}

/// The main run's live status as spans, for a Secondary Surface header.
///
/// Busy / awaiting-user come straight from the shared status strip; an idle
/// surface shows the last turn's own terminal marker, so "Completed" / "Failed"
/// are the runtime's words, never a guess. Distinct from a side thread's own
/// response state.
pub(crate) fn main_status_spans(state: &AppState, width: usize) -> Vec<Span<'static>> {
    if (state.is_busy() || status_phase(state) == StatusPhase::AwaitingUser)
        && let Some(line) = status_lines(state, width).into_iter().next()
        && !line.spans.is_empty()
    {
        return line.spans;
    }
    // An interrupting card lives on Main, and the strip is deliberately silent
    // there because the card is the wait itself. This surface has no card, so
    // it is the one that has to name the wait — otherwise a side thread would
    // report the main task as idle while the runtime is blocked on the user.
    if status_phase(state) == StatusPhase::AwaitingUser
        && let Some(hint) = state
            .overlay
            .as_ref()
            .and_then(|overlay| overlay.status_hint(state.t()))
    {
        return vec![Span::styled(
            hint,
            Style::default().fg(state.theme.status.warning),
        )];
    }
    if state.status == leveler_client_protocol::RuntimeStatus::Error {
        return vec![Span::styled(
            format!("✗ {}", state.t().final_failed),
            Style::default().fg(state.theme.status.error),
        )];
    }
    if let Some(block) = state.transcript.last_turn_end() {
        // Same wording as the transcript receipt: the status names the turn's
        // own end state, so the header reads it too.
        let (label, color) = crate::render::turn_end_marker(block, &state.theme, state.t());
        let verdict = !label.is_empty();
        let elapsed = if block.elapsed_secs > 0 {
            crate::status_line::fmt_elapsed(block.elapsed_secs)
        } else {
            String::new()
        };
        let text = match (verdict, elapsed.is_empty()) {
            (false, true) => String::new(),
            (false, false) => elapsed,
            (true, true) => label,
            (true, false) => format!("{label} · {elapsed}"),
        };
        if !text.is_empty() {
            let mut style = Style::default().fg(color);
            if verdict {
                style = style.add_modifier(Modifier::BOLD);
            }
            return vec![Span::styled(text, style)];
        }
    }
    vec![Span::styled(
        state.t().btw_main_idle,
        Style::default().fg(state.theme.text.muted),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVIEW_HINTS: [&str; 6] = [
        "↑↓ 移动",
        "空格 选择",
        "Enter 查看",
        "A 接受",
        "R 拒绝",
        "Esc 返回",
    ];

    fn width_of(row: &str) -> usize {
        UnicodeWidthStr::width(row)
    }

    /// Whole actions move to the next row; none is cut or dropped.
    #[test]
    fn hint_rows_move_whole_actions_to_the_next_row() {
        let rows = wrap_hint(REVIEW_HINTS, 24);
        assert!(
            rows.len() > 1,
            "24 columns cannot hold the full hint: {rows:?}"
        );
        for row in &rows {
            assert!(width_of(row) <= 24, "row too wide: {row:?}");
        }
        for action in REVIEW_HINTS {
            assert!(
                rows.iter().any(|row| row.contains(action)),
                "{action:?} was dropped: {rows:?}"
            );
        }
    }

    /// CJK labels are measured in cells, not characters.
    #[test]
    fn hint_rows_measure_display_width() {
        let rows = wrap_hint(REVIEW_HINTS, 20);
        for row in &rows {
            assert!(width_of(row) <= 20, "row too wide: {row:?}");
        }
        let joined = rows.join(" · ");
        for action in REVIEW_HINTS {
            assert!(joined.contains(action), "{action:?} was dropped: {joined}");
        }
    }

    /// Below the width of a single action, splitting keeps every glyph rather
    /// than replacing the tail with an ellipsis.
    #[test]
    fn an_oversized_hint_action_is_split_not_dropped() {
        let rows = wrap_hint(["Esc 返回", "A 接受"], 4);
        for row in &rows {
            assert!(width_of(row) <= 4, "row too wide: {row:?}");
        }
        let joined: String = rows.concat();
        for glyph in ['返', '回', '接', '受'] {
            assert!(joined.contains(glyph), "{glyph:?} was dropped: {rows:?}");
        }
    }

    /// A taller footer takes its rows from the content, not from the rules.
    #[test]
    fn a_two_row_footer_shrinks_the_body_by_one_row() {
        let one = layout(ratatui::layout::Rect::new(0, 0, 48, 20), 0);
        let two = layout_with_footer(ratatui::layout::Rect::new(0, 0, 48, 20), 0, 2);
        assert_eq!(one.footer.height, 1);
        assert_eq!(two.footer.height, 2);
        assert_eq!(two.content.height + 1, one.content.height);
        assert_eq!(two.rule_bottom.y + 1, two.footer.y);
    }
}
