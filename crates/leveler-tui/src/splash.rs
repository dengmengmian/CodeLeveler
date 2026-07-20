//! Empty-session splash: a rounded two-column welcome box.
//!
//! Shown only when Conversation has no real work yet. Left column is the plain
//! **CL** block-letter mark; right column is brand + a short getting-started
//! command list (pulled from the real slash registry). Narrow terminals fall
//! back to a single centered column.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::i18n::UiText;
use crate::state::AppState;
use crate::theme::Theme;
use crate::transcript::TranscriptItem;

/// Readable **C** + **L** block letters (not a geometric mono-glyph).
const LOGO: &[&str] = &[
    " ██████╗ ██      ",
    "██╔════╝ ██      ",
    "██║      ██      ",
    "██║      ██      ",
    "╚██████╗ ███████╗",
    " ╚═════╝ ╚══════╝",
];

/// Commands surfaced in the splash tips list (must exist in the slash registry).
const TIP_COMMANDS: &[&str] = &["/goal", "/workflow", "/model", "/help"];

/// Gap between the logo column and the text column.
const GAP: usize = 3;
/// Fixed width of the command-name column (longest name + one space).
const NAME_COL: usize = 10;
/// Maximum box width so the card stays compact on wide terminals.
const MAX_BOX_W: usize = 76;

/// Whether Conversation is still empty of real turns (only welcome/btw ignored).
pub(crate) fn conversation_is_empty(state: &AppState) -> bool {
    !state.transcript.items().iter().any(|item| {
        matches!(
            item,
            TranscriptItem::User(_)
                | TranscriptItem::Assistant(_)
                | TranscriptItem::ToolGroup(_)
                | TranscriptItem::SubAgent(_)
                | TranscriptItem::Completion(_)
                | TranscriptItem::Error(_)
                | TranscriptItem::Note(_)
                | TranscriptItem::TurnEnd(_)
                | TranscriptItem::Recap(_)
        )
    })
}

/// Splash lines for the empty Conversation viewport.
pub(crate) fn splash_lines(
    state: &AppState,
    width: usize,
    theme: &Theme,
    t: &UiText,
) -> Vec<Line<'static>> {
    let width = width.max(24);
    let accent = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(theme.muted);
    let border = Style::default().fg(theme.border);

    let logo_w = LOGO.iter().map(|r| disp_w(r)).max().unwrap_or(0);
    // Smallest box that still leaves a usable text column.
    let min_box = logo_w + GAP + 4 + 30;
    if width < min_box + 2 {
        return fallback_centered(state, width, theme, t);
    }

    let box_w = width.saturating_sub(2).min(MAX_BOX_W).max(min_box);
    let inner = box_w - 4; // borders + one padding space each side
    let right_w = inner - logo_w - GAP;
    let box_left_pad = (width - box_w) / 2;

    // ── Right-column rows (brand + tips) ────────────────────────────────────
    let version = state.version();
    let mut right: Vec<Vec<Span<'static>>> = Vec::new();
    right.push(vec![
        Span::styled("CodeLeveler".to_string(), accent),
        Span::styled(format!("  v{version}"), muted),
    ]);
    right.push(Vec::new());
    right.push(vec![Span::styled(t.splash_tips_title.to_string(), accent)]);
    right.push(vec![Span::styled(t.splash_tips_lead.to_string(), muted)]);
    right.push(Vec::new());

    let registry = crate::screen::slash_commands(t);
    for name in TIP_COMMANDS {
        let desc = registry
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, d)| *d)
            .unwrap_or("");
        let desc = truncate_w(desc, right_w.saturating_sub(NAME_COL));
        right.push(vec![
            Span::styled(pad_to(name, NAME_COL), accent),
            Span::styled(desc, muted),
        ]);
    }

    // ── Assemble the box ────────────────────────────────────────────────────
    let rows = right.len().max(LOGO.len());
    let logo_top = (rows - LOGO.len()) / 2;

    let mut out = Vec::new();
    out.push(Line::from(""));
    out.push(border_line("╭", "╮", box_w, box_left_pad, border));
    out.push(inner_blank(inner, box_left_pad, border));

    for i in 0..rows {
        let logo_cell = if i >= logo_top && i < logo_top + LOGO.len() {
            LOGO[i - logo_top]
        } else {
            ""
        };
        let right_spans = right.get(i).cloned().unwrap_or_default();
        out.push(content_row(
            logo_cell,
            logo_w,
            right_spans,
            right_w,
            box_left_pad,
            accent,
            border,
        ));
    }

    out.push(inner_blank(inner, box_left_pad, border));
    out.push(border_line("╰", "╯", box_w, box_left_pad, border));
    out.push(Line::from(""));
    out
}

/// One `│ … │` content row: logo cell + gap + right spans, both padded so the
/// right border aligns.
fn content_row(
    logo_cell: &str,
    logo_w: usize,
    right_spans: Vec<Span<'static>>,
    right_w: usize,
    left_pad: usize,
    logo_style: Style,
    border: Style,
) -> Line<'static> {
    let mut spans = Vec::new();
    if left_pad > 0 {
        spans.push(Span::raw(" ".repeat(left_pad)));
    }
    spans.push(Span::styled("│ ".to_string(), border));

    spans.push(Span::styled(logo_cell.to_string(), logo_style));
    let logo_pad = logo_w.saturating_sub(disp_w(logo_cell)) + GAP;
    spans.push(Span::raw(" ".repeat(logo_pad)));

    let mut used = 0usize;
    for s in right_spans {
        used += disp_w(s.content.as_ref());
        spans.push(s);
    }
    if right_w > used {
        spans.push(Span::raw(" ".repeat(right_w - used)));
    }

    spans.push(Span::styled(" │".to_string(), border));
    Line::from(spans)
}

fn inner_blank(inner: usize, left_pad: usize, border: Style) -> Line<'static> {
    let mut spans = Vec::new();
    if left_pad > 0 {
        spans.push(Span::raw(" ".repeat(left_pad)));
    }
    spans.push(Span::styled("│ ".to_string(), border));
    spans.push(Span::raw(" ".repeat(inner)));
    spans.push(Span::styled(" │".to_string(), border));
    Line::from(spans)
}

fn border_line(
    left: &str,
    right: &str,
    box_w: usize,
    left_pad: usize,
    border: Style,
) -> Line<'static> {
    let mut spans = Vec::new();
    if left_pad > 0 {
        spans.push(Span::raw(" ".repeat(left_pad)));
    }
    let bar = format!("{left}{}{right}", "─".repeat(box_w.saturating_sub(2)));
    spans.push(Span::styled(bar, border));
    Line::from(spans)
}

/// Single centered column for terminals too narrow for the two-column box.
fn fallback_centered(
    state: &AppState,
    width: usize,
    theme: &Theme,
    t: &UiText,
) -> Vec<Line<'static>> {
    let accent = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(theme.muted);

    let mut out = Vec::new();
    out.push(Line::from(""));
    for row in LOGO {
        out.push(center_line(row, width, accent));
    }
    out.push(Line::from(""));
    out.push(center_line(
        &format!("CodeLeveler  v{}", state.version()),
        width,
        accent,
    ));
    out.push(center_line(t.splash_tips_title, width, muted));
    out.push(Line::from(""));
    out.push(center_line(t.splash_hint, width, muted));
    out.push(Line::from(""));
    out
}

fn center_line(s: &str, width: usize, style: Style) -> Line<'static> {
    let w = disp_w(s);
    let pad = width.saturating_sub(w) / 2;
    let mut spans = Vec::new();
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(Span::styled(s.to_string(), style));
    Line::from(spans)
}

fn disp_w(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Pad `s` on the right with spaces to reach `target` display columns.
fn pad_to(s: &str, target: usize) -> String {
    let w = disp_w(s);
    if w >= target {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(target - w))
    }
}

/// Truncate `s` to at most `max` display columns, adding `…` when clipped.
fn truncate_w(s: &str, max: usize) -> String {
    if disp_w(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthStr::width(ch.to_string().as_str());
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Boot;
    use leveler_client_protocol::SessionId;

    fn empty_state() -> AppState {
        AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: true,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
            },
        )
    }

    fn joined(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|sp| sp.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn splash_shows_brand_logo_and_commands() {
        let s = empty_state();
        let lines = splash_lines(&s, 90, &s.theme, s.t());
        let text = joined(&lines);
        assert!(text.contains("CodeLeveler"), "{text}");
        assert!(text.contains('█'), "logo cells: {text}");
        assert!(text.contains("██║      ██"), "CL letters missing: {text}");
        // Two-column box borders + command tips.
        assert!(
            text.contains('╭') && text.contains('╰'),
            "box border: {text}"
        );
        assert!(text.contains("/goal"), "tips list missing: {text}");
        assert!(conversation_is_empty(&s));
    }

    #[test]
    fn box_rows_are_width_aligned() {
        let s = empty_state();
        let width = 90;
        let lines = splash_lines(&s, width, &s.theme, s.t());
        // Every bordered row must share one right-edge column so `│` lines up.
        let edges: Vec<usize> = lines
            .iter()
            .filter_map(|l| {
                let plain: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
                let trimmed = plain.trim_end();
                if trimmed.ends_with('│') || trimmed.ends_with('╮') || trimmed.ends_with('╯')
                {
                    Some(disp_w(trimmed))
                } else {
                    None
                }
            })
            .collect();
        assert!(
            edges.len() >= 5,
            "expected several bordered rows: {edges:?}"
        );
        assert!(
            edges.iter().all(|w| *w == edges[0]),
            "right borders misaligned: {edges:?}"
        );
    }

    #[test]
    fn narrow_terminal_falls_back_to_centered() {
        let s = empty_state();
        let lines = splash_lines(&s, 40, &s.theme, s.t());
        let text = joined(&lines);
        assert!(text.contains("CodeLeveler"), "{text}");
        // No box borders in the fallback.
        assert!(!text.contains('╭'), "narrow should not draw a box: {text}");
    }

    #[test]
    fn non_empty_conversation_is_not_splash() {
        let mut s = empty_state();
        s.transcript.push_user("hello".into());
        assert!(!conversation_is_empty(&s));
    }
}
