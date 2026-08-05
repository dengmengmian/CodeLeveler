use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::screen::Screen;
use crate::state::AppState;
use crate::transcript::TranscriptItem;

use super::text::{take_display_prefix, truncate_display};

pub(crate) const COMPOSER_MAX_ROWS: usize = 8;
pub(crate) const COMPOSER_PROMPT: &str = "› ";
pub(crate) const COMPOSER_CONT: &str = "  ";

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
    let height = rows + 2;
    let width = 46.min(transcript.width).max(4);
    let y = composer.y.saturating_sub(height).max(transcript.y);
    let popup = Rect {
        x: composer.x,
        y,
        width,
        height: height.min(composer.y.saturating_sub(transcript.y).max(1)),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(popup);
    let inner_w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for (row, (name, desc)) in matches[start..end].iter().enumerate() {
        let selected = start + row == sel;
        let name_w = UnicodeWidthStr::width(name.as_str());
        let desc_room = inner_w.saturating_sub(name_w + 2);
        let desc = truncate_display(desc, desc_room);
        let used = name_w + 2 + UnicodeWidthStr::width(desc.as_str());
        let pad = inner_w.saturating_sub(used);
        let (name_style, desc_style, pad_style) = if selected {
            (
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                Style::default()
                    .fg(theme.text)
                    .add_modifier(Modifier::REVERSED),
                Style::default().add_modifier(Modifier::REVERSED),
            )
        } else {
            (
                Style::default().fg(theme.accent),
                Style::default().fg(theme.muted),
                Style::default(),
            )
        };
        lines.push(Line::from(vec![
            Span::styled(name.clone(), name_style),
            Span::styled(format!("  {desc}"), desc_style),
            Span::styled(" ".repeat(pad), pad_style),
        ]));
    }
    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(lines), inner);
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
    let content_w = width.saturating_sub(4).max(4); // `│ ` … `│`
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
    let border_color = if focused { theme.accent } else { theme.border };
    let border = Style::default().fg(border_color);

    let width = width.max(8);
    // Interior width between the two `│`.
    let inner = width.saturating_sub(2);
    // Content after `│ ` (one leading space inside the frame).
    let content_w = inner.saturating_sub(1).max(1);

    let (window, caret_row) = composer_visual_rows(state, content_w);

    let mut content: Vec<Line<'static>> = Vec::with_capacity(window.len());
    let mut caret_col = COMPOSER_PROMPT.width() as u16;

    for (vi, row) in window.iter().enumerate() {
        let mut spans = Vec::new();
        let mut used = 0usize;

        let p = truncate_display(row.prompt, content_w);
        used += UnicodeWidthStr::width(p.as_str());
        spans.push(Span::styled(p, Style::default().fg(theme.accent)));

        if !row.chips.is_empty() {
            let room = content_w.saturating_sub(used);
            let c = truncate_display(&row.chips, room);
            used += UnicodeWidthStr::width(c.as_str());
            spans.push(Span::styled(c, Style::default().fg(theme.attachment)));
        }

        let room = content_w.saturating_sub(used);
        let piece = truncate_display(&row.text, room);
        let text_start = used;
        used += UnicodeWidthStr::width(piece.as_str());
        spans.push(Span::styled(piece, Style::default()));

        if row.placeholder {
            let room = content_w.saturating_sub(used);
            let hint = truncate_display(state.t().composer_placeholder, room);
            used += UnicodeWidthStr::width(hint.as_str());
            spans.push(Span::styled(hint, Style::default().fg(theme.muted)));
        }

        if let Some(ghost) = row.ghost {
            let room = content_w.saturating_sub(used);
            if room > 0 {
                let g = truncate_display(ghost, room);
                used += UnicodeWidthStr::width(g.as_str());
                spans.push(Span::styled(g, Style::default().fg(theme.muted)));
            }
        }

        if vi == caret_row
            && let Some(cc) = row.caret_col
        {
            // +2 for `│ ` frame prefix
            caret_col = (2 + text_start + cc) as u16;
        }

        // Frame: `│ ` + content + pad + `│`
        let pad = content_w.saturating_sub(used);
        let mut line_spans = vec![Span::styled("│ ", border)];
        line_spans.extend(spans);
        if pad > 0 {
            line_spans.push(Span::raw(" ".repeat(pad)));
        }
        line_spans.push(Span::styled("│", border));
        content.push(Line::from(line_spans));
    }

    if content.is_empty() {
        let pad = content_w.saturating_sub(COMPOSER_PROMPT.width());
        content.push(Line::from(vec![
            Span::styled("│ ", border),
            Span::styled(COMPOSER_PROMPT, Style::default().fg(theme.accent)),
            Span::raw(" ".repeat(pad)),
            Span::styled("│", border),
        ]));
        caret_col = 2;
    }

    // Top border
    let top = Line::from(Span::styled(format!("╭{}╮", "─".repeat(inner)), border));

    // Bottom border with the model and mode trust chip on the right.
    // Exact display width = width: `╰` + dashes/chip + `╯`.
    let trust = composer_trust_chip(state);
    let tw = UnicodeWidthStr::width(trust.as_str());
    let bottom = if tw + 4 <= inner {
        // `╰{left_dashes} {trust} {right_dashes}╯` where spaces around trust count.
        let chip = format!(" {trust} ");
        let chip_w = UnicodeWidthStr::width(chip.as_str());
        let dash_budget = inner.saturating_sub(chip_w);
        // Prefer more left dashes so the chip hugs the right corner.
        let right = 1usize.min(dash_budget);
        let left = dash_budget.saturating_sub(right);
        Line::from(vec![
            Span::styled(format!("╰{}", "─".repeat(left)), border),
            Span::styled(chip, Style::default().fg(theme.muted)),
            Span::styled(format!("{}╯", "─".repeat(right)), border),
        ])
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

/// Compact trust label on the composer bottom border:
/// `model · perm · collab` plus optional `· wf` / non-default work-mode.
///
/// Permission uses stable English product labels (`auto` / `approve` / `full`).
/// Token stats and context belong on the Footer, not here.
fn composer_trust_chip(state: &AppState) -> String {
    let perm = crate::status_line::permission_chip_label(state);
    let collab = state.collaboration.as_str();
    let mut base = format!("{} · {perm} · {collab}", state.model_label);
    if state.work_profile != "balanced" {
        let short = match state.work_profile.as_str() {
            "economy" => "eco",
            "delivery" => "del",
            other => other,
        };
        base.push_str(" · ");
        base.push_str(short);
    }
    if state.untrusted_config.is_empty() {
        return base;
    }
    // Ignored in-repo config is a standing condition, not an event, so it rides
    // the chip for as long as it holds rather than fading like a notification.
    format!("{base} · {}", state.t().untrusted_config_chip)
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
        Style::default().fg(state.theme.muted),
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
            Style::default().fg(theme.attachment),
        ))
    } else {
        let mut spans = Vec::new();
        for (i, att) in state.pending_attachments.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                format!("[{}] {}", i + 1, att.summary()),
                Style::default().fg(theme.attachment),
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
            },
        )
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
