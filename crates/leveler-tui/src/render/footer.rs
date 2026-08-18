use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::screen::Screen;
use crate::state::AppState;
use crate::transcript::TranscriptItem;

use super::text::{take_display_prefix, truncate_display};

pub(crate) const COMPOSER_MAX_ROWS: usize = 8;
pub(crate) const COMPOSER_PROMPT: &str = "› ";
pub(crate) const COMPOSER_CONT: &str = "  ";

/// Cells between the popup border and the command column.
/// Selected rows draw `>` in the left pad; unselected rows keep a space.
/// That cell is reserved either way so selection cannot shift columns.
const SLASH_POPUP_PAD_X: u16 = 1;
/// Default command column. Grows to fit the longest visible name.
const SLASH_COMMAND_COL: usize = 14;
const SLASH_COL_GAP: usize = 1;
const SLASH_POPUP_MAX_WIDTH: u16 = 46;

/// Columns after the popup border and inner padding.
pub(crate) fn slash_popup_content_width(popup_width: u16) -> usize {
    popup_width
        .saturating_sub(2) // left + right border
        .saturating_sub(SLASH_POPUP_PAD_X.saturating_mul(2)) as usize
}

/// Command column and remaining description columns for `content_w`.
///
/// Command width is measured in terminal cells, never bytes. The column is
/// never shrunk below the longest visible name — a tight popup clips at the
/// content rect instead of turning `/permission` into `/permi…`.
pub(crate) fn slash_popup_columns(content_w: usize, names: &[&str]) -> (usize, usize) {
    let longest = names
        .iter()
        .map(|n| UnicodeWidthStr::width(*n))
        .max()
        .unwrap_or(0);
    let command_col = SLASH_COMMAND_COL.max(longest);
    let rest = content_w.saturating_sub(command_col);
    let desc_col = if rest > SLASH_COL_GAP {
        rest.saturating_sub(SLASH_COL_GAP)
    } else {
        0
    };
    (command_col, desc_col)
}

struct SlashPopupCells {
    name: String,
    name_pad: usize,
    desc: String,
    desc_col: usize,
}

fn slash_popup_cells(name: &str, desc: &str, content_w: usize, names: &[&str]) -> SlashPopupCells {
    let (command_col, desc_col) = slash_popup_columns(content_w, names);
    // Command stays whole. Narrow popups clip at render time, never ellipsize.
    let name_shown = name.to_string();
    let name_pad = command_col.saturating_sub(UnicodeWidthStr::width(name_shown.as_str()));
    let desc_shown = if desc_col == 0 {
        String::new()
    } else {
        truncate_display(desc, desc_col)
    };
    SlashPopupCells {
        name: name_shown,
        name_pad,
        desc: desc_shown,
        desc_col,
    }
}

/// Display-width prefix with no ellipsis. Used to clip a row to the content
/// rect; commands must not become `/permi…`.
fn clip_display(s: &str, max: usize) -> String {
    take_display_prefix(s, max).0
}

/// One slash-popup row: command (prefer full) + padded gap + truncated desc.
/// The returned string is exactly `content_w` display cells (or empty).
#[cfg(test)]
fn slash_popup_row(name: &str, desc: &str, content_w: usize, names: &[&str]) -> String {
    if content_w == 0 {
        return String::new();
    }
    let cells = slash_popup_cells(name, desc, content_w, names);
    let mut out = cells.name;
    out.push_str(&" ".repeat(cells.name_pad));
    if cells.desc_col > 0 {
        out.push(' ');
        out.push_str(&cells.desc);
    }
    let used = UnicodeWidthStr::width(out.as_str());
    if used < content_w {
        out.push_str(&" ".repeat(content_w - used));
    }
    let mut clipped = clip_display(&out, content_w);
    let painted = UnicodeWidthStr::width(clipped.as_str());
    if painted < content_w {
        clipped.push_str(&" ".repeat(content_w - painted));
    }
    clipped
}

fn slash_popup_geometry(transcript: Rect, composer: Rect, visible_rows: u16) -> (Rect, Rect, Rect) {
    let height = visible_rows.saturating_add(2);
    let width = SLASH_POPUP_MAX_WIDTH.min(transcript.width).max(4);
    let y = composer.y.saturating_sub(height).max(transcript.y);
    let popup = Rect {
        x: composer.x,
        y,
        width,
        height: height.min(composer.y.saturating_sub(transcript.y).max(1)),
    };
    let inner = Rect {
        x: popup.x.saturating_add(1),
        y: popup.y.saturating_add(1),
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };
    let pad = SLASH_POPUP_PAD_X.min(inner.width / 2);
    let content_w = slash_popup_content_width(popup.width);
    let content = Rect {
        x: inner.x.saturating_add(pad),
        y: inner.y,
        width: (content_w as u16).min(inner.width.saturating_sub(pad.saturating_mul(2))),
        height: inner.height,
    };
    (popup, inner, content)
}

/// Write `text` at `(x, y)` without crossing `right`. Wide characters that
/// would overflow the last cell are dropped, not split.
fn put_clipped(buf: &mut Buffer, x: u16, y: u16, text: &str, right: u16, style: Style) -> u16 {
    if x >= right || y >= buf.area.height {
        return x;
    }
    let room = right.saturating_sub(x) as usize;
    let clipped = clip_display(text, room);
    let written = UnicodeWidthStr::width(clipped.as_str()) as u16;
    buf.set_stringn(x, y, clipped, room, style);
    x.saturating_add(written).min(right)
}

pub(crate) fn render_slash_popup(
    frame: &mut Frame,
    transcript: Rect,
    composer: Rect,
    state: &AppState,
) {
    let matches = slash_popup_match_rows(state);
    if matches.is_empty() {
        return;
    }
    let theme = &state.theme;
    // Keep the highlighted row visible.
    const MAX: usize = 8;
    let sel = state.slash_selected.min(matches.len() - 1);
    let start = sel
        .saturating_sub(MAX - 1)
        .min(matches.len().saturating_sub(MAX));
    let end = (start + MAX).min(matches.len());
    let rows = (end - start) as u16;
    let (popup, inner, content) = slash_popup_geometry(transcript, composer, rows);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border.normal));
    let names: Vec<&str> = matches[start..end]
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    // Clear first: conversation CJK leaves 2-cell glyphs whose continuation
    // cells swallow the rest of a description (only the first ideograph
    // remains, and the transcript shows through as "权限 户端").
    frame.render_widget(Clear, popup);
    theme.paint_surface(frame, popup, theme.surface.elevated);
    frame.render_widget(block, popup);
    if inner.width == 0 || inner.height == 0 || content.width == 0 {
        return;
    }
    let content_right = content.x.saturating_add(content.width);
    let buf = frame.buffer_mut();
    for (row, (name, desc)) in matches[start..end].iter().enumerate() {
        let y = content.y.saturating_add(row as u16);
        if y >= inner.y.saturating_add(inner.height) || y >= buf.area.height {
            break;
        }
        let selected = start + row == sel;
        let cells = slash_popup_cells(name, desc, content.width as usize, &names);
        let (name_style, desc_style, pad_style) = if selected {
            (
                Style::default()
                    .fg(theme.text.primary)
                    .bg(theme.surface.selection)
                    .add_modifier(Modifier::BOLD),
                Style::default()
                    .fg(theme.text.primary)
                    .bg(theme.surface.selection),
                Style::default().bg(theme.surface.selection),
            )
        } else {
            (
                Style::default()
                    .fg(theme.accent.primary)
                    .bg(theme.surface.elevated),
                Style::default()
                    .fg(theme.text.secondary)
                    .bg(theme.surface.elevated),
                Style::default().bg(theme.surface.elevated),
            )
        };
        // Own every inner cell (pads included) before writing glyphs.
        let fill = " ".repeat(inner.width as usize);
        buf.set_stringn(inner.x, y, &fill, inner.width as usize, pad_style);
        let marker = if selected { ">" } else { " " };
        put_clipped(
            buf,
            inner.x,
            y,
            marker,
            content.x.max(inner.x.saturating_add(1)),
            pad_style,
        );
        let mut x = content.x;
        x = put_clipped(buf, x, y, &cells.name, content_right, name_style);
        x = put_clipped(
            buf,
            x,
            y,
            &" ".repeat(cells.name_pad),
            content_right,
            pad_style,
        );
        if cells.desc_col > 0 {
            x = put_clipped(buf, x, y, " ", content_right, pad_style);
            put_clipped(buf, x, y, &cells.desc, content_right, desc_style);
        }
    }
}

/// Shared match list for slash / @file popups (name, description).
fn slash_popup_match_rows(state: &AppState) -> Vec<(String, String)> {
    let files = crate::screen::visible_file_popup(state);
    if files.is_empty() {
        crate::screen::visible_slash_popup(state)
    } else {
        files
            .into_iter()
            .map(|path| (format!("@{path}"), state.t().file_mention.to_string()))
            .collect()
    }
}

/// One visual row of the soft-wrapped composer (prompt + optional chips + text).
struct ComposerVisRow {
    /// Leading prompt (`› ` or continuation indent).
    prompt: &'static str,
    /// Attachment chips only on the first visual row of the buffer.
    chips: String,
    /// Soft-wrapped slice of one logical line (display-width limited).
    text: String,
    /// When set, the caret sits on this row at this display column within `text`.
    caret_col: Option<usize>,
    /// Empty-buffer placeholder (visual only).
    placeholder: bool,
    /// Slash-arg ghost after caret (visual only).
    ghost: Option<&'static str>,
}

/// Soft-wrap the composer buffer into visual rows for a content width of
/// `width` (inside the box, after `│ `). Long lines wrap instead of overflowing.
fn composer_visual_rows(state: &AppState, width: usize) -> (Vec<ComposerVisRow>, usize) {
    let inner_w = width.max(1);
    // Only show the "type a message / commands" hint on a fresh session. Once
    // real turns exist the user knows the ropes — repeating it every idle moment
    // is noise.
    let show_placeholder = crate::splash::conversation_is_empty(state);
    let chips: String = (1..=state.pending_attachments.len())
        .map(|n| format!("[图片 #{n}] "))
        .collect();
    let (crow, ccol) = state.composer.cursor_row_col_display();
    let logical = state.composer.lines();
    let mut vis: Vec<ComposerVisRow> = Vec::new();

    for (li, line_text) in logical.iter().enumerate() {
        let mut rest = *line_text;
        let mut first_of_logical = true;
        // At least one visual row per logical line (empty line still paints).
        loop {
            let prompt = if li == 0 && first_of_logical {
                COMPOSER_PROMPT
            } else {
                COMPOSER_CONT
            };
            let chips_here = if li == 0 && first_of_logical {
                chips.as_str()
            } else {
                ""
            };
            let prefix_w = UnicodeWidthStr::width(prompt) + UnicodeWidthStr::width(chips_here);
            let room = inner_w.saturating_sub(prefix_w).max(1);
            let start_byte = line_text.len() - rest.len();
            let start_col = UnicodeWidthStr::width(&line_text[..start_byte]);
            let (piece, next) = if rest.is_empty() && first_of_logical {
                (String::new(), "")
            } else if rest.is_empty() {
                break;
            } else {
                take_display_prefix(rest, room)
            };

            // Map caret onto display columns [start_col, start_col + piece_w].
            let piece_w = UnicodeWidthStr::width(piece.as_str());
            let caret = if li == crow && ccol >= start_col && ccol <= start_col + piece_w {
                // Caret exactly at the wrap boundary with more text remaining →
                // paint it at column 0 of the next visual row.
                if ccol == start_col + piece_w && !next.is_empty() && piece_w >= room {
                    None
                } else {
                    Some(ccol - start_col)
                }
            } else {
                None
            };

            let placeholder =
                show_placeholder && li == 0 && first_of_logical && state.composer.is_empty();
            let ghost = if li == crow
                && first_of_logical
                && li == 0
                && state.composer.cursor() >= state.composer.text().len()
            {
                crate::screen::slash_arg_ghost(state.composer.text(), state.t())
            } else {
                None
            };

            vis.push(ComposerVisRow {
                prompt,
                chips: chips_here.to_string(),
                text: piece,
                caret_col: caret,
                placeholder,
                ghost,
            });

            rest = next;
            first_of_logical = false;
            if rest.is_empty() {
                break;
            }
        }
    }

    if vis.is_empty() {
        vis.push(ComposerVisRow {
            prompt: COMPOSER_PROMPT,
            chips,
            text: String::new(),
            caret_col: Some(0),
            placeholder: show_placeholder,
            ghost: None,
        });
    }

    // Ensure a caret exists (empty buffer / edge cases).
    if !vis.iter().any(|r| r.caret_col.is_some())
        && let Some(last) = vis.last_mut()
    {
        last.caret_col = Some(UnicodeWidthStr::width(last.text.as_str()));
    }

    let caret_idx = vis.iter().position(|r| r.caret_col.is_some()).unwrap_or(0);
    let rows = vis.len().clamp(1, COMPOSER_MAX_ROWS);
    let scroll = vis.len().saturating_sub(rows).min(caret_idx);
    let window: Vec<ComposerVisRow> = vis.into_iter().skip(scroll).take(rows).collect();
    let caret_in_window = caret_idx.saturating_sub(scroll);
    (window, caret_in_window)
}

/// Total rows the composer box occupies (content + top/bottom borders).
pub(crate) fn composer_visible_rows(state: &AppState, width: usize) -> usize {
    let chrome = 2 + crate::layout::INPUT_INTERNAL_PADDING_X as usize;
    let content_w = width.saturating_sub(chrome).max(4);
    let content = composer_visual_rows(state, content_w)
        .0
        .len()
        .clamp(1, COMPOSER_MAX_ROWS);
    content + 2 // ╭ top + ╰ bottom
}

/// Input box: rounded border, prompt inside, trust chip on the bottom-right
/// (`model · permission` only — tokens/ctx live on the Footer).
///
/// Returns (lines including top/bottom borders, cursor as col/row **inside**
/// the content area — row 0 is the first content line, not the top border).
pub(crate) fn composer_box_lines(
    state: &AppState,
    width: usize,
) -> (Vec<Line<'static>>, (u16, u16)) {
    let theme = &state.theme;
    // Accent border only when Input owns workbench focus.
    let focused = state.overlay.is_none()
        && state.active_screen == Screen::Conversation
        && state.workbench_focus == crate::state::WorkbenchFocus::Input;
    let border_color = if focused {
        theme.border.focus
    } else {
        theme.border.normal
    };
    let border = Style::default().fg(border_color);

    let width = width.max(8);
    let pad = crate::layout::INPUT_INTERNAL_PADDING_X as usize;
    // Interior width between the two `│`.
    let inner = width.saturating_sub(2);
    // Content after the left border + inner padding.
    let content_w = inner.saturating_sub(pad).max(1);

    let (window, caret_row) = composer_visual_rows(state, content_w);

    let mut content: Vec<Line<'static>> = Vec::with_capacity(window.len());
    let mut caret_col = COMPOSER_PROMPT.width() as u16;

    for (vi, row) in window.iter().enumerate() {
        let mut spans = Vec::new();
        let mut used = 0usize;

        let p = truncate_display(row.prompt, content_w);
        used += UnicodeWidthStr::width(p.as_str());
        spans.push(Span::styled(p, Style::default().fg(theme.accent.primary)));

        if !row.chips.is_empty() {
            let room = content_w.saturating_sub(used);
            let c = truncate_display(&row.chips, room);
            used += UnicodeWidthStr::width(c.as_str());
            spans.push(Span::styled(c, Style::default().fg(theme.accent.secondary)));
        }

        let room = content_w.saturating_sub(used);
        let piece = truncate_display(&row.text, room);
        let text_start = used;
        used += UnicodeWidthStr::width(piece.as_str());
        spans.push(Span::styled(
            piece,
            Style::default()
                .fg(theme.text.primary)
                .bg(theme.surface.input),
        ));

        if row.placeholder {
            let room = content_w.saturating_sub(used);
            let hint = truncate_display(state.t().composer_placeholder, room);
            used += UnicodeWidthStr::width(hint.as_str());
            spans.push(Span::styled(hint, Style::default().fg(theme.text.muted)));
        }

        if let Some(ghost) = row.ghost {
            let room = content_w.saturating_sub(used);
            if room > 0 {
                let g = truncate_display(ghost, room);
                used += UnicodeWidthStr::width(g.as_str());
                spans.push(Span::styled(g, Style::default().fg(theme.text.muted)));
            }
        }

        if vi == caret_row
            && let Some(cc) = row.caret_col
        {
            // Left border + inner padding, then the prompt/text offset.
            caret_col = (1 + pad + text_start + cc) as u16;
        }

        // Frame: `│` + inner pad + content + fill + `│`
        let fill = content_w.saturating_sub(used);
        let mut line_spans = vec![Span::styled(format!("│{}", " ".repeat(pad)), border)];
        line_spans.extend(spans);
        if fill > 0 {
            line_spans.push(Span::raw(" ".repeat(fill)));
        }
        line_spans.push(Span::styled("│", border));
        content.push(Line::from(line_spans));
    }

    if content.is_empty() {
        let fill = content_w.saturating_sub(COMPOSER_PROMPT.width());
        content.push(Line::from(vec![
            Span::styled(format!("│{}", " ".repeat(pad)), border),
            Span::styled(COMPOSER_PROMPT, Style::default().fg(theme.accent.primary)),
            Span::raw(" ".repeat(fill)),
            Span::styled("│", border),
        ]));
        caret_col = (1 + pad) as u16;
    }

    // Top border
    let top = Line::from(Span::styled(format!("╭{}╮", "─".repeat(inner)), border));

    // Bottom border with the model and mode trust chip on the right.
    // Exact display width = width: `╰` + dashes/chip + `╯`.
    let trust_budget = inner.saturating_sub(4);
    let trust = composer_trust_chip_for_width(state, trust_budget);
    let tw = UnicodeWidthStr::width(trust.as_str());
    let bottom = if tw + 4 <= inner {
        // `╰{left_dashes} {trust} {right_dashes}╯` where spaces around trust count.
        let chip = format!(" {trust} ");
        let chip_w = UnicodeWidthStr::width(chip.as_str());
        let dash_budget = inner.saturating_sub(chip_w);
        // Prefer more left dashes so the chip hugs the right corner.
        let right = 1usize.min(dash_budget);
        let left = dash_budget.saturating_sub(right);
        Line::from({
            let mut spans = vec![Span::styled(format!("╰{}", "─".repeat(left)), border)];
            spans.extend(composer_trust_spans(state, trust_budget));
            spans.push(Span::styled(format!("{}╯", "─".repeat(right)), border));
            spans
        })
    } else {
        Line::from(Span::styled(format!("╰{}╯", "─".repeat(inner)), border))
    };

    let mut lines = Vec::with_capacity(content.len() + 2);
    lines.push(top);
    lines.extend(content);
    lines.push(bottom);

    // Cursor row is +1 for the top border.
    let cx = caret_col.min((width.saturating_sub(1)) as u16);
    let cy = (1 + caret_row) as u16;
    (lines, (cx, cy))
}

/// Compact trust label on the composer bottom border — the runtime state chip.
#[cfg(test)]
fn composer_trust_chip(state: &AppState) -> String {
    crate::status_line::runtime_status_chip(state, usize::MAX)
}

fn composer_trust_chip_for_width(state: &AppState, max_width: usize) -> String {
    crate::status_line::runtime_status_chip(state, max_width)
}

/// Same contents as [`composer_trust_chip`], with model vs mode hierarchy.
fn composer_trust_spans(state: &AppState, max_width: usize) -> Vec<Span<'static>> {
    let theme = &state.theme;
    let secondary = Style::default().fg(theme.text.secondary);
    let muted = Style::default().fg(theme.text.muted);
    let chip = composer_trust_chip_for_width(state, max_width);
    let mut spans = Vec::new();
    spans.push(Span::styled(" ", secondary));
    for (i, part) in chip.split(" · ").enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", muted));
        }
        let style = if i == 0 { secondary } else { muted };
        spans.push(Span::styled(part.to_string(), style));
    }
    spans.push(Span::styled(" ", secondary));
    spans
}

/// The single hint row under the composer, or nothing.
///
/// Two moments earn it: a turn in flight (the interrupt is the only key that
/// matters, and an interrupt nobody can find is not one) and an untouched
/// composer (the openers). Once the user starts typing, the row goes away —
/// hints competing with a half-written sentence are noise.
pub(crate) fn key_hint_line(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let t = state.t();
    let hints = if state.is_busy() {
        format!("Esc {} · Ctrl+C {}", t.hint_interrupt, t.hint_cancel)
    } else if state.composer.is_empty() && state.turn_nav.is_none() {
        format!(
            "/ {} · @ {} · Shift+Tab {} · Ctrl+? {}",
            t.hint_commands, t.hint_files, t.hint_permission, t.hint_shortcuts
        )
    } else {
        return Vec::new();
    };
    vec![Line::from(Span::styled(
        truncate_display(&hints, width.max(1)),
        Style::default().fg(state.theme.text.muted),
    ))]
}

/// The attachments strip above the composer (spec §40): compact when many.
pub(crate) fn render_attachments(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let theme = &state.theme;
    let n = state.pending_attachments.len();
    let line = if n > 2 {
        Line::from(Span::styled(
            format!("{n} 个附件 · 空输入框按 Backspace 删除末项"),
            Style::default().fg(theme.accent.secondary),
        ))
    } else {
        let mut spans = Vec::new();
        for (i, att) in state.pending_attachments.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                format!("[{}] {}", i + 1, att.summary()),
                Style::default().fg(theme.accent.secondary),
            ));
        }
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), area);
}

pub(crate) fn render_composer(frame: &mut Frame, area: Rect, state: &AppState) {
    let composer_focused = state.overlay.is_none() && state.active_screen == Screen::Conversation;
    let (lines, (cx, cy)) = composer_box_lines(state, area.width as usize);
    // Clip to the allocated area height (box may want more rows than remain).
    let shown: Vec<Line> = lines.into_iter().take(area.height as usize).collect();
    frame.render_widget(Paragraph::new(shown), area);

    if composer_focused {
        let x = area.x + cx;
        let y = area.y + cy;
        if x < area.x + area.width && y < area.y + area.height {
            frame.set_cursor_position(Position::new(x, y));
        }
    }
}

/// `(ordinal_1based, preview_text)` for every user message in the transcript.
pub(crate) fn user_turn_summaries(state: &AppState) -> Vec<(usize, String)> {
    state
        .transcript
        .items()
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::User(text) => {
                let one = text.lines().next().unwrap_or(text).trim();
                if one.is_empty() {
                    None
                } else {
                    Some(one.to_string())
                }
            }
            _ => None,
        })
        .enumerate()
        .map(|(i, s)| (i + 1, s))
        .collect()
}

#[cfg(test)]
mod p1_tests {
    use super::*;
    use crate::state::Boot;
    use crate::theme::Theme;
    use leveler_client_protocol::SessionId;

    fn state() -> AppState {
        AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("sess-abcdef12"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        )
    }

    #[test]
    fn trust_chip_is_model_work_permission_session() {
        let mut s = state();
        s.model_label = "deepseek/v4".into();
        s.work_profile = "balanced".into();
        s.mode_label = "Assisted".into();
        s.collaboration = "chat".into();
        assert_eq!(composer_trust_chip(&s), "v4 · balanced · auto · chat");
        s.work_profile = "delivery".into();
        s.mode_label = "RequestApproval".into();
        s.collaboration = "plan".into();
        assert_eq!(composer_trust_chip(&s), "v4 · delivery · ask · plan");
    }

    /// The stderr startup notice is swallowed by the alternate screen, so the
    /// composer border is where a TUI user learns their repository ships config
    /// that is being ignored. It must persist for the whole session, not fade.
    #[test]
    fn trust_chip_flags_ignored_in_repo_config() {
        let mut s = state();
        assert!(
            !composer_trust_chip(&s).contains('⚠'),
            "clean repo shows no warning"
        );

        s.untrusted_config = vec![".leveler/hooks.yaml".to_string()];
        let chip = composer_trust_chip(&s);
        assert!(chip.contains('⚠'), "{chip}");
        assert!(
            chip.contains(&s.model_label) && chip.contains("assisted") || chip.contains('·'),
            "the model/permission chip must survive: {chip}"
        );
    }

    #[test]
    fn user_turn_summaries_lists_only_user_messages() {
        let mut s = state();
        s.transcript.push_user("first".into());
        s.transcript.push_note("system note".into());
        s.transcript.push_user("second ask".into());
        let turns = user_turn_summaries(&s);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].1, "first");
        assert_eq!(turns[1].1, "second ask");
    }
}

#[cfg(test)]
mod slash_popup_layout_tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn content_width_subtracts_border_and_padding() {
        assert_eq!(slash_popup_content_width(80), 76);
        assert_eq!(slash_popup_content_width(46), 42);
    }

    #[test]
    fn descriptions_share_one_column() {
        let names = ["/model", "/permission", "/work-mode"];
        let content_w = 40;
        let (command_col, _) = slash_popup_columns(content_w, &names);
        assert_eq!(command_col, SLASH_COMMAND_COL);
        let start = command_col + SLASH_COL_GAP;
        for name in names {
            let row = slash_popup_row(name, "标签", content_w, &names);
            let prefix_w: usize = row
                .chars()
                .scan(0usize, |acc, ch| {
                    if *acc >= start {
                        return None;
                    }
                    *acc += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    Some(*acc)
                })
                .last()
                .unwrap_or(0);
            assert!(
                prefix_w >= start,
                "{name} description should start at col {start}: {row:?}"
            );
            let desc_start = row
                .chars()
                .scan(0usize, |acc, ch| {
                    let at = *acc;
                    *acc += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    Some((at, ch))
                })
                .find(|(at, _)| *at == start)
                .map(|(_, ch)| ch);
            assert_eq!(desc_start, Some('标'), "{name} row={row:?}");
        }
    }

    #[test]
    fn permission_command_is_never_truncated() {
        let names = ["/model", "/permission", "/goal"];
        let content_w = slash_popup_content_width(46);
        let row = slash_popup_row(
            "/permission",
            "权限审批流程以及更多说明文字",
            content_w,
            &names,
        );
        assert!(
            row.contains("/permission"),
            "command must stay intact: {row}"
        );
        assert!(!row.contains("/permissi…"), "{row}");
    }

    #[test]
    fn long_description_is_truncated_with_ellipsis() {
        let names = ["/model"];
        let content_w = slash_popup_content_width(46);
        let row = slash_popup_row(
            "/model",
            "一个非常长的描述文本xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            content_w,
            &names,
        );
        assert!(row.contains("/model"), "{row}");
        assert!(row.contains('…'), "description must ellipsize: {row:?}");
        assert_eq!(UnicodeWidthStr::width(row.as_str()), content_w);
    }

    #[test]
    fn chinese_truncate_is_display_width_safe() {
        let names = ["/model"];
        let row = slash_popup_row(
            "/model",
            "切换使用的 AI 模型以及 provider 配置管理",
            24,
            &names,
        );
        assert!(row.contains("/model"), "{row}");
        assert!(row.contains('…') || UnicodeWidthStr::width(row.as_str()) <= 24);
        assert_eq!(UnicodeWidthStr::width(row.as_str()), 24);
        assert!(row.is_char_boundary(row.len()));
    }

    #[test]
    fn narrow_popup_does_not_panic() {
        let names = ["/permission"];
        for w in 0u16..=10 {
            let content_w = slash_popup_content_width(w);
            let row = slash_popup_row("/permission", "权限审批流程", content_w, &names);
            assert!(
                UnicodeWidthStr::width(row.as_str()) <= content_w,
                "w={w} overflow: {row:?}"
            );
        }
    }

    #[test]
    fn cjk_labels_use_terminal_cell_width() {
        for (text, cells) in [("权限审批", 8), ("切换模型", 8), ("工作模式", 8)] {
            assert_eq!(
                UnicodeWidthStr::width(text),
                cells,
                "{text} must be {cells} cells, not bytes={} chars={}",
                text.len(),
                text.chars().count()
            );
            assert_ne!(
                UnicodeWidthStr::width(text),
                text.len(),
                "{text}: String::len is bytes, not cells"
            );
        }
    }

    #[test]
    fn descriptions_align_across_varied_command_lengths() {
        let names = ["/model", "/permission", "/work-mode", "/collab"];
        let content_w = 40;
        let (command_col, _) = slash_popup_columns(content_w, &names);
        let start = command_col + SLASH_COL_GAP;
        let mut desc_starts = Vec::new();
        for name in names {
            let row = slash_popup_row(name, "对齐", content_w, &names);
            let ch = row
                .chars()
                .scan(0usize, |acc, ch| {
                    let at = *acc;
                    *acc += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    Some((at, ch))
                })
                .find(|(at, _)| *at == start)
                .map(|(_, ch)| ch);
            assert_eq!(ch, Some('对'), "{name} row={row:?}");
            desc_starts.push(start);
        }
        assert!(desc_starts.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn command_is_never_ellipsized() {
        let names = ["/permission"];
        for content_w in [0, 1, 2, 5, 10, 14, 42] {
            let cells = slash_popup_cells("/permission", "权限审批流程", content_w, &names);
            assert!(
                !cells.name.contains('…'),
                "command must not ellipsize at content_w={content_w}: {:?}",
                cells.name
            );
            if content_w >= UnicodeWidthStr::width("/permission") {
                assert_eq!(cells.name, "/permission");
            }
        }
    }

    #[test]
    fn long_description_does_not_overflow_available_cells() {
        let names = ["/btw"];
        let content_w = 22;
        let row = slash_popup_row(
            "/btw",
            "一个非常长的描述文本xxxxxxxxxxxxxxxx",
            content_w,
            &names,
        );
        assert!(row.contains("/btw"), "{row}");
        assert!(row.contains('…'), "must ellipsize: {row:?}");
        assert_eq!(UnicodeWidthStr::width(row.as_str()), content_w);
        assert!(row.is_char_boundary(row.len()));
        assert!(
            !row.contains('\u{FFFD}'),
            "must not split a UTF-8 codepoint: {row:?}"
        );
    }

    #[test]
    fn narrow_widths_do_not_panic_or_underflow() {
        let names = ["/permission", "/work-mode"];
        for w in [0u16, 1, 2, 5, 10] {
            let content_w = slash_popup_content_width(w);
            let (command_col, desc_col) = slash_popup_columns(content_w, &names);
            let _ = command_col.saturating_add(desc_col);
            let row = slash_popup_row("/permission", "权限审批", content_w, &names);
            let painted = UnicodeWidthStr::width(row.as_str());
            assert!(
                painted <= content_w,
                "w={w} content_w={content_w} painted={painted} row={row:?}"
            );
        }
    }

    fn popup_state() -> crate::state::AppState {
        use crate::state::Boot;
        use crate::theme::Theme;
        use leveler_client_protocol::SessionId;
        crate::state::AppState::new(
            Theme::dark(),
            Boot {
                session_id: SessionId::new("sess-slash"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        )
    }

    fn paint_slash_over_cjk(
        selected: usize,
    ) -> (
        ratatui::buffer::Buffer,
        ratatui::layout::Rect,
        ratatui::layout::Rect,
    ) {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::text::Line;
        use ratatui::widgets::Paragraph;

        let mut state = popup_state();
        state.composer.replace("/");
        state.slash_selected = selected;
        let transcript = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 16,
        };
        let composer = Rect {
            x: 2,
            y: 18,
            width: 76,
            height: 4,
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let stain = "客户端报告询问到这里";
                let line = stain.repeat(8);
                let lines = vec![Line::from(line); frame.area().height as usize];
                frame.render_widget(Paragraph::new(lines), frame.area());
                render_slash_popup(frame, transcript, composer, &state);
            })
            .unwrap();
        let matches = slash_popup_match_rows(&state);
        let rows = matches.len().min(8) as u16;
        let (popup, _inner, content) = slash_popup_geometry(transcript, composer, rows);
        (terminal.backend().buffer().clone(), popup, content)
    }

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16, x0: u16, x1: u16) -> String {
        let mut out = String::new();
        let mut x = x0;
        while x < x1 {
            let cell = buf.cell((x, y));
            let sym = cell.map(|c| c.symbol()).unwrap_or("");
            if !sym.is_empty() {
                out.push_str(sym);
            }
            x = x.saturating_add(1);
        }
        out
    }

    #[test]
    fn render_does_not_write_past_content_right() {
        let (buf, popup, content) = paint_slash_over_cjk(0);
        let inner_right = popup.x + popup.width - 1;
        for y in content.y..content.y + content.height {
            let last = content.x + content.width;
            for x in last..popup.x + popup.width {
                if x >= inner_right {
                    continue;
                }
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or("");
                assert!(
                    sym.trim().is_empty() || x == inner_right.saturating_sub(1),
                    "row {y} col {x} past content.right={} is {sym:?} (popup {:?})",
                    last,
                    popup
                );
            }
        }
    }

    #[test]
    fn selection_keeps_command_and_stays_inside_content() {
        use crate::theme::Theme;
        let theme = Theme::dark();
        let (buf, _popup, content) = paint_slash_over_cjk(3);
        let mut found = None;
        for y in content.y..content.y + content.height {
            let text = row_text(&buf, y, content.x, content.x + content.width);
            if text.contains("/btw") {
                found = Some((y, text));
                break;
            }
        }
        let (y, text) = found.expect("selected /btw row");
        assert!(
            text.contains("/btw") && !text.contains("/bt…"),
            "command must stay whole: {text:?}"
        );
        let mut saw_selection = false;
        for x in content.x..content.x + content.width {
            if buf
                .cell((x, y))
                .is_some_and(|c| c.bg == theme.surface.selection)
            {
                saw_selection = true;
            }
        }
        assert!(
            saw_selection,
            "selected row must paint selection background"
        );
        let last_glyph_x = (content.x..content.x + content.width)
            .rev()
            .find(|x| {
                buf.cell((*x, y))
                    .is_some_and(|c| !c.symbol().trim().is_empty())
            })
            .unwrap_or(content.x);
        assert!(
            last_glyph_x < content.x + content.width,
            "last glyph {last_glyph_x} exceeds content.right"
        );
    }

    #[test]
    fn popup_overwrites_cjk_underlay_inside_content() {
        let (buf, popup, content) = paint_slash_over_cjk(0);
        let mut saw_full_model = false;
        for y in content.y..content.y + content.height {
            let text = row_text(&buf, y, popup.x, popup.x + popup.width);
            assert!(
                !text.contains('客')
                    && !text.contains('户')
                    && !text.contains('端')
                    && !text.contains('报')
                    && !text.contains('询'),
                "conversation CJK bled into popup row {y}: {text:?}"
            );
            if text.contains("/model") {
                assert!(
                    text.contains('切')
                        && text.contains('换')
                        && text.contains('模')
                        && text.contains('型'),
                    "full description must render, not just the first CJK: {text:?}"
                );
                saw_full_model = true;
            }
        }
        assert!(saw_full_model, "expected /model row");
    }

    #[test]
    fn rendered_descriptions_share_one_start_column() {
        let (buf, _popup, content) = paint_slash_over_cjk(0);
        let mut desc_x = None;
        for (name, first) in [("/model", '切'), ("/permission", '权'), ("/goal", '设')] {
            let mut found = None;
            for y in content.y..content.y + content.height {
                let text = row_text(&buf, y, content.x, content.x + content.width);
                if text.contains(name) {
                    found = Some(y);
                    break;
                }
            }
            let y = found.unwrap_or_else(|| panic!("missing {name}"));
            let text = row_text(&buf, y, content.x, content.x + content.width);
            assert!(
                text.contains(first),
                "{name} missing leading {first}: {text:?}"
            );
            let x = (content.x..content.x + content.width)
                .find(|x| {
                    buf.cell((*x, y))
                        .is_some_and(|c| c.symbol().starts_with(first))
                })
                .unwrap_or_else(|| panic!("{name} missing {first}"));
            match desc_x {
                None => desc_x = Some(x),
                Some(prev) => assert_eq!(x, prev, "{name} description x={x} prev={prev}"),
            }
        }
    }
}
