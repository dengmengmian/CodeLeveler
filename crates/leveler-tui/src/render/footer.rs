use leveler_client_protocol::PlanStepStatus;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::footer_queue::queued_lines;
use crate::screen::Screen;
use crate::state::AppState;
use crate::status_line::status_line_content;
use crate::transcript::TranscriptItem;

use super::text::{take_display_prefix, truncate_display, wrap};
use super::transcript_lines::{btw_card_lines, item_render, items_need_gap};

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
    // Keep the highlighted row visible (same window as inline slash_popup_lines).
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
    let slash = crate::screen::visible_slash_popup(state);
    let files = crate::screen::visible_file_popup(state);
    if files.is_empty() {
        slash
            .into_iter()
            .map(|(name, desc)| (name.to_string(), desc.to_string()))
            .collect()
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

            let placeholder = li == 0 && first_of_logical && state.composer.is_empty();
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
            placeholder: true,
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

/// Compact trust label on the composer bottom border: model · permission only.
///
/// Permission uses stable English product labels (`auto` / `approve` / `full`).
/// Token stats and context belong on the Footer, not here.
fn composer_trust_chip(state: &AppState) -> String {
    let perm = crate::status_line::permission_chip_label(state);
    format!("{} · {perm}", state.model_label)
}

/// The slash-command completion popup as inline lines (a bordered box shown just
/// above the composer while the user types `/…`). Empty when it shouldn't show.
fn slash_popup_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let matches = slash_popup_match_rows(state);
    if matches.is_empty() {
        return Vec::new();
    }
    let theme = &state.theme;
    let border = Style::default().fg(theme.border);
    let box_w = 46.min(width).max(4);
    let inner = box_w - 2;

    // Window up to MAX rows around the highlighted entry so it stays visible.
    const MAX: usize = 8;
    let sel = state.slash_selected.min(matches.len() - 1);
    let start = sel
        .saturating_sub(MAX - 1)
        .min(matches.len().saturating_sub(MAX));
    let end = (start + MAX).min(matches.len());

    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(inner)),
        border,
    )));
    for (row, (name, desc)) in matches[start..end].iter().enumerate() {
        let selected = start + row == sel;
        // "│ name  desc … │" — highlighted row is reverse-video.
        let name_w = UnicodeWidthStr::width(name.as_str());
        let desc_room = inner.saturating_sub(1 + name_w + 2 + 1);
        let desc = truncate_display(desc, desc_room);
        let used = name_w + 2 + UnicodeWidthStr::width(desc.as_str());
        let pad = inner.saturating_sub(1 + used + 1);
        let (name_style, desc_style) = if selected {
            (
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                Style::default()
                    .fg(theme.text)
                    .add_modifier(Modifier::REVERSED),
            )
        } else {
            (
                Style::default().fg(theme.accent),
                Style::default().fg(theme.muted),
            )
        };
        let pad_style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        out.push(Line::from(vec![
            Span::styled("│ ", border),
            Span::styled(name.clone(), name_style),
            Span::styled(format!("  {desc}"), desc_style),
            Span::styled(" ".repeat(pad), pad_style),
            Span::styled(" │", border),
        ]));
    }
    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner)),
        border,
    )));
    out
}

/// Clip a line's spans to at most `max` display columns, returning the clipped
/// spans and the width actually used.
fn clip_spans(line: Line<'static>, max: usize) -> (Vec<Span<'static>>, usize) {
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    let mut used = 0usize;
    for span in line.spans {
        if used >= max {
            break;
        }
        let piece = truncate_display(&span.content, max - used);
        if piece.is_empty() {
            continue;
        }
        used += UnicodeWidthStr::width(piece.as_str());
        out.push(Span::styled(piece, span.style));
    }
    (out, used)
}

/// The open overlay as a bordered inline box (replacing the composer in the
/// footer), plus the overlay's text-input cursor as (col, row) within the box.
fn overlay_box_lines(
    state: &AppState,
    width: usize,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let Some(overlay) = &state.overlay else {
        return (Vec::new(), None);
    };
    let theme = &state.theme;
    let (title, content, cursor) = crate::overlay::content_lines(overlay, theme);
    let border = Style::default().fg(theme.accent);
    let inner_w = width.saturating_sub(4).max(1);

    let mut out: Vec<Line<'static>> = Vec::with_capacity(content.len() + 2);
    let tw = UnicodeWidthStr::width(title.as_str()).min(inner_w);
    out.push(Line::from(vec![
        Span::styled("╭─ ", border),
        Span::styled(
            truncate_display(&title, inner_w),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}╮", "─".repeat(inner_w.saturating_sub(tw + 1))),
            border,
        ),
    ]));
    for line in content {
        let (spans, used) = clip_spans(line, inner_w);
        let mut row = vec![Span::styled("│ ", border)];
        row.extend(spans);
        row.push(Span::raw(" ".repeat(inner_w.saturating_sub(used))));
        row.push(Span::styled(" │", border));
        out.push(Line::from(row));
    }
    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner_w + 2)),
        border,
    )));
    // +1 row for the top border, +2 cols for the "│ " gutter.
    (out, cursor.map(|(row, col)| (col + 2, row + 1)))
}

/// Build the conversation "footer" (live region) for inline rendering: the not-
/// yet-committed streaming tail, the status line, and the composer box (or the
/// open overlay in its place). Returns the lines plus the cursor position
/// (col, row within the footer) — `None` hides the cursor.
pub fn conversation_footer(
    state: &AppState,
    width: usize,
    tail_start: usize,
    tail_skip: usize,
    has_history: bool,
) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let theme = &state.theme;
    let mut out: Vec<Line<'static>> = Vec::new();

    // The uncommitted tail (streaming assistant / running tools). When the first
    // tail item is a streaming assistant whose stable prefix is already in
    // scrollback (`tail_skip > 0`), its live tail continues seamlessly: skip
    // those committed lines and emit no separating blank before it.
    //
    // `/btw` side cards are collected and pinned just above the status line so
    // they never interleave mid-stream with the main answer.
    let items = state.transcript.items();
    let tail_start = tail_start.min(items.len());
    let mut btw_cards: Vec<&crate::transcript::BtwBlock> = Vec::new();
    for (idx, item) in items[tail_start..].iter().enumerate() {
        if let TranscriptItem::Btw(block) = item {
            btw_cards.push(block);
            continue;
        }
        let continues = idx == 0 && tail_skip > 0;
        // Blank separator, except when continuing a half-committed assistant
        // or when the previous item groups with this one (tool-call runs).
        let prev = (tail_start + idx).checked_sub(1).map(|i| &items[i]);
        let gap = match prev {
            Some(TranscriptItem::Btw(_)) => true,
            Some(prev) => items_need_gap(prev, item),
            None => has_history,
        };
        if (idx > 0 || has_history) && !continues && gap {
            out.push(Line::from(""));
        }
        let lines = item_render(item, theme, width.max(1), state.tools_expanded, state.t());
        if continues {
            out.extend(lines.into_iter().skip(tail_skip));
        } else {
            out.extend(lines);
        }
    }

    if state.is_busy() && !state.project_rule_sources.is_empty() {
        out.push(Line::from(""));
        out.push(Line::from(vec![
            Span::styled(
                format!("◆ {} · ", state.t().active_rules),
                Style::default().fg(theme.accent),
            ),
            Span::styled(
                state.project_rule_sources.join(", "),
                Style::default().fg(theme.muted),
            ),
        ]));
    }

    // Sticky plan panel: only while open work remains (or a failure). Fully
    // succeeded plans (including 1/1 ✓) drop away once the turn answers.
    if let Some(plan) = &state.plan
        && crate::workbench::plan_panel_should_show(plan)
    {
        out.push(Line::from(""));
        let done = plan
            .steps
            .iter()
            .filter(|step| step.status == PlanStepStatus::Done)
            .count();
        let current = plan
            .steps
            .iter()
            .position(|s| s.status == PlanStepStatus::Running)
            .map(|i| i + 1)
            .unwrap_or_else(|| done.saturating_add(1).min(plan.steps.len()));
        out.push(Line::from(Span::styled(
            format!(
                "{} · {}/{} · {} {}/{}",
                state.t().active_plan,
                done,
                plan.steps.len(),
                state.t().plan_step_of,
                current,
                plan.steps.len()
            ),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        for step in &plan.steps {
            let glyph = match step.status {
                PlanStepStatus::Done => "✓",
                PlanStepStatus::Running => "●",
                PlanStepStatus::Pending => "○",
                PlanStepStatus::Failed => "×",
                PlanStepStatus::Skipped => "–",
            };
            let style = if step.status == PlanStepStatus::Running {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.muted)
            };
            for (index, line) in wrap(
                &format!("{glyph} {}. {}", step.index + 1, step.description),
                width.max(1),
            )
            .into_iter()
            .enumerate()
            {
                let line = if index == 0 {
                    line
                } else {
                    format!("  {line}")
                };
                out.push(Line::from(Span::styled(line, style)));
            }
        }
    }

    // Live reasoning (model thinking) sits above the status line while a turn
    // is in flight — collapsed by default to a one-line header only.
    if !state.reasoning.is_empty() {
        out.push(Line::from(""));
        out.extend(reasoning_lines(state, width));
    }

    // Shift+↑/↓ turn navigation peek (composer draft is never touched).
    if state.turn_nav.is_some() {
        out.push(Line::from(""));
        out.extend(turn_nav_lines(state, width));
    }

    // Keep one blank row between transcript and composer. A live status or
    // clarification marker occupies its own row and gets breathing room on
    // both sides; idle turns do not emit extra empty status rows.
    out.push(Line::from(""));
    let status = status_line_content(state, width);
    let has_status = status.spans.iter().any(|span| !span.content.is_empty());
    if has_status {
        out.push(status);
        out.push(Line::from(""));
    }

    // An open overlay renders inline IN PLACE of the composer, so the
    // conversation (and the streaming tail above) stays visible while the user
    // decides — no alternate-screen flip. Side-question cards still sit under
    // the overlay box so the main answer stream stays clean.
    if state.overlay.is_some() {
        let base = out.len();
        let (obox, ocur) = overlay_box_lines(state, width);
        out.extend(obox);
        for block in &btw_cards {
            out.push(Line::from(""));
            out.extend(btw_card_lines(block, theme, width.max(1), state.t()));
        }
        let cursor = ocur.map(|(cx, crow)| (cx as u16, (base + crow) as u16));
        return (out, cursor);
    }

    // (Pending attachments render as inline chips inside the composer box.)

    // Slash-command completion popup, just above the composer (spec §29).
    out.extend(slash_popup_lines(state, width));

    // The composer box, tracking the cursor row within the footer.
    let (composer, (cx, crow)) = composer_box_lines(state, width);
    let cy = out.len() as u16 + crow;
    out.extend(composer);

    // Queued messages (submitted while busy) listed just below the composer.
    out.extend(queued_lines(state, width));

    // Side questions live *under* the input box (above the bottom chrome) so
    // they read as a footer note, not a second main answer mid-stream.
    for block in btw_cards {
        out.push(Line::from(""));
        out.extend(btw_card_lines(block, theme, width.max(1), state.t()));
    }

    // Trust chip lives on the composer bottom border — no second chrome strip.
    (out, Some((cx, cy)))
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

/// Live reasoning / thinking block for the current turn.
///
/// Collapsed (default): header + line count + expand hint only — no body, so
/// long chain-of-thought never floods the live footer. Expanded (Ctrl+O): full
/// text, hard-capped so a runaway stream still fits.
fn reasoning_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let theme = &state.theme;
    let text = state.reasoning.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();
    let disclosure = if state.reasoning_expanded {
        "▾"
    } else {
        "▸"
    };
    let t = state.t();
    let mut out = vec![Line::from(vec![
        Span::styled(
            format!("{disclosure} {}", t.thinking),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if total > 0 {
                t.thinking_lines.replacen("{}", &total.to_string(), 1)
            } else {
                String::new()
            },
            Style::default().fg(theme.border),
        ),
    ])];
    // Collapsed: header only — body would re-flood the footer on long CoT.
    if !state.reasoning_expanded {
        if total > 0 {
            out.push(Line::from(Span::styled(
                t.expand_thinking_tools.to_string(),
                Style::default().fg(theme.border),
            )));
        }
        return out;
    }
    const MAX_EXPANDED_LINES: usize = 24;
    let inner = width.saturating_sub(4).max(1);
    let mut shown = 0usize;
    for line in &lines {
        if shown >= MAX_EXPANDED_LINES {
            break;
        }
        for wrapped in wrap(line, inner) {
            if shown >= MAX_EXPANDED_LINES {
                break;
            }
            out.push(Line::from(Span::styled(
                format!("  {wrapped}"),
                Style::default().fg(theme.muted),
            )));
            shown += 1;
        }
    }
    if total > MAX_EXPANDED_LINES || lines.len() > shown {
        out.push(Line::from(Span::styled(
            "  … (+ more · Ctrl+O)".to_string(),
            Style::default().fg(theme.border),
        )));
    }
    out
}

/// Peek card for Shift+↑/↓ user-turn navigation (draft is never cleared).
pub(crate) fn turn_nav_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let Some(idx) = state.turn_nav else {
        return Vec::new();
    };
    let turns = user_turn_summaries(state);
    if turns.is_empty() {
        return Vec::new();
    }
    let idx = idx.min(turns.len() - 1);
    let theme = &state.theme;
    let t = state.t();
    let (n, summary) = &turns[idx];
    let head = t
        .turn_nav
        .replacen("{}", &(idx + 1).to_string(), 1)
        .replacen("{}", &turns.len().to_string(), 1);
    let preview = truncate_display(summary, width.saturating_sub(4).max(8));
    vec![
        Line::from(Span::styled(
            format!("⇧ {head}"),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("  {preview}"),
            Style::default().fg(theme.user_message),
        )),
        Line::from(Span::styled(
            format!("  · turn #{n} · Shift+↑/↓ · Esc {}", t.turn_nav_live),
            Style::default().fg(theme.border),
        )),
    ]
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
            },
        )
    }

    #[test]
    fn collapsed_reasoning_is_header_only() {
        let mut s = state();
        s.reasoning = (0..20)
            .map(|i| format!("thought line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        s.reasoning_expanded = false;
        let lines = reasoning_lines(&s, 80);
        let joined: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("思考"), "{joined}");
        assert!(joined.contains("20"), "line count: {joined}");
        assert!(
            !joined.contains("thought line"),
            "collapsed must not dump body: {joined}"
        );
        assert!(joined.contains("Ctrl+O"), "expand hint: {joined}");
    }

    #[test]
    fn expanded_reasoning_shows_body_capped() {
        let mut s = state();
        s.reasoning = (0..40)
            .map(|i| format!("thought line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        s.reasoning_expanded = true;
        let lines = reasoning_lines(&s, 80);
        let body = lines
            .iter()
            .filter(|l| l.to_string().contains("thought line"))
            .count();
        assert!(body > 0 && body <= 24, "body rows: {body}");
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
