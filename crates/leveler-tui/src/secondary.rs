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
//! full-width rules, the horizontal content padding, the footer hint, and the
//! content rect. It owns no page logic, no business state, and no key handling
//! — each surface still builds its own body and (optionally) its own composer.
//!
//! The navigation header is the one element that owns the terminal edge: the
//! back glyph sits at column 0 and only the content (and the footer hints) are
//! inset, so the page hierarchy reads nav → body → nested detail.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::state::AppState;
use crate::status_line::{StatusPhase, status_lines, status_phase};
use crate::theme::Theme;

/// Horizontal breathing room for Secondary Surface content and footer hints.
/// The rules and the navigation header touch the edge; the body does not.
pub(crate) const PADDING_X: u16 = 2;

/// One Secondary Surface's identity and interaction contract.
pub(crate) struct SecondaryPage<'a> {
    /// Left identity, e.g. `btw · 旁路线程`. Already localized.
    pub title: &'a str,
    /// Right-aligned parent / global status — the main run's own words.
    pub status: Vec<Span<'static>>,
    /// Footer interaction hint. Only bindings that really exist.
    pub hint: &'a str,
}

/// The rects a Secondary Surface draws into. `content` and `composer` are
/// already horizontally padded; `header` / `footer` are full width but their
/// text is drawn with the same padding.
pub(crate) struct SecondaryLayout {
    pub header: Rect,
    pub rule_top: Rect,
    pub content: Rect,
    pub composer: Rect,
    pub rule_bottom: Rect,
    pub footer: Rect,
}

/// Split `area` into the shared Secondary Surface slots.
pub(crate) fn layout(area: Rect, composer_rows: u16) -> SecondaryLayout {
    let composer_rows = composer_rows.min(area.height.saturating_sub(4));
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // rule above the body
        Constraint::Min(1),    // content
        Constraint::Length(composer_rows),
        Constraint::Length(1), // rule above the footer
        Constraint::Length(1), // footer hints
    ])
    .split(area);
    SecondaryLayout {
        header: chunks[0],
        rule_top: chunks[1],
        content: crate::layout::horizontal_inset(chunks[2], PADDING_X),
        composer: crate::layout::horizontal_inset(chunks[3], PADDING_X),
        rule_bottom: chunks[4],
        footer: chunks[5],
    }
}

/// Draw the identity header, its right-aligned parent status, and the rule
/// below it.
pub(crate) fn draw_header(
    frame: &mut Frame,
    l: &SecondaryLayout,
    page: &SecondaryPage,
    theme: &Theme,
) {
    if l.header.height == 0 {
        return;
    }
    // The navigation row owns the full width: the back glyph anchors the left
    // edge, the parent status the right. Unlike the body it is not inset, so
    // the nav reads as chrome above the page rather than content inside it.
    let inner = l.header;
    if inner.width == 0 {
        draw_rule(frame, l.rule_top, theme);
        return;
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
}

/// Draw the rule above the footer and the interaction hint below it.
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
    let inner = crate::layout::horizontal_inset(l.footer, PADDING_X);
    if inner.width == 0 {
        return;
    }
    let hint = crate::render::truncate_display(page.hint, inner.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(theme.text.muted),
        ))),
        inner,
    );
}

/// Chrome for a view that has not adopted the shell yet: the app header strip,
/// the runtime status strip, and the body in between. Keeps the pre-shell frame
/// so a surface is not forced into the Secondary Surface language before it
/// fits (split panes, QR decisions, live execution).
pub(crate) fn legacy_frame(frame: &mut Frame, area: Rect, state: &AppState) -> Rect {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(crate::status_line::header_line(state, area.width as usize)),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(crate::status_line::status_line_content(
            state,
            area.width as usize,
        )),
        chunks[2],
    );
    chunks[1]
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
    {
        return line.spans;
    }
    if state.status == leveler_client_protocol::RuntimeStatus::Error {
        return vec![Span::styled(
            format!("✗ {}", state.t().final_failed),
            Style::default().fg(state.theme.status.error),
        )];
    }
    if let Some(block) = state.transcript.last_turn_end() {
        // The compact verdict only: the transcript divider's rule decoration is
        // not header material. Same wording mapping as the divider itself.
        let (label, color) = crate::render::turn_end_marker(block, &state.theme, state.t());
        let mut text = label;
        if block.elapsed_secs > 0 {
            text.push_str(&format!(
                " · {}",
                crate::status_line::fmt_elapsed(block.elapsed_secs)
            ));
        }
        return vec![Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
    }
    vec![Span::styled(
        state.t().btw_main_idle,
        Style::default().fg(state.theme.text.muted),
    )]
}
