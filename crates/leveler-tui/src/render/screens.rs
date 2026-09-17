use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use unicode_width::UnicodeWidthStr;

use crate::state::AppState;
use crate::theme::Theme;

use super::panes::{render_list_focused, render_scrolled};
use super::text::{sanitize_terminal_line, truncate_display, wrap};

pub(super) fn render_help_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let t = state.t();
    let mut lines: Vec<Line> = vec![screen_title(t.help_title, theme), Line::from("")];
    lines.push(Line::from(Span::styled(
        t.help_commands.to_string(),
        Style::default().fg(theme.text.secondary),
    )));
    for (cat, cmds) in crate::screen::slash_commands_grouped(t) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {}", crate::screen::category_label(cat, t)),
            Style::default()
                .fg(theme.text.secondary)
                .add_modifier(Modifier::BOLD),
        )));
        for (name, desc) in cmds {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {name:<14}"),
                    Style::default().fg(theme.accent.primary),
                ),
                Span::raw(desc.to_string()),
            ]));
        }
    }
    lines.push(Line::from(""));

    // The composer hint row carries only the few keys that matter in the
    // moment; this table is the complete list.
    let keys = [
        ("Enter", t.key_submit),
        ("Ctrl+J / Alt+Enter", t.key_newline),
        ("Ctrl+X Ctrl+E", t.key_editor),
        ("Shift+Tab", t.key_permission),
        ("Tab", t.key_tab_focus),
        (&format!("↑/↓ ({})", t.help_scope_input), t.key_history),
        (
            &format!("↑/↓ ({})", t.help_scope_conversation),
            t.key_scroll_messages,
        ),
        ("PageUp/PageDown", t.key_scroll_page),
        ("Ctrl+C", t.key_cancel_quit),
        ("Ctrl+M", t.key_model),
        ("Ctrl+V", t.key_paste_image),
        ("Ctrl+O", t.key_expand),
        ("Ctrl+?", t.help_title),
        ("Ctrl+D/T/S", t.key_screens),
        ("Ctrl+End / Ctrl+↓", t.key_jump),
        ("End", t.key_end),
        ("Esc", t.key_esc),
    ];
    lines.push(Line::from(Span::styled(
        t.help_keys.to_string(),
        Style::default().fg(theme.text.secondary),
    )));
    for (k, d) in keys {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {k:<20}"),
                Style::default().fg(theme.accent.primary),
            ),
            Span::raw(d.to_string()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t.help_scroll.to_string(),
        Style::default().fg(theme.text.secondary),
    )));
    render_scrolled(frame, area, state, lines);
}

pub(crate) fn screen_title(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    ))
}

pub(super) fn render_diff_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let t = state.t();
    // Wipe the whole split first so a shorter file/list can't leave ghosts
    // from the previous selection or scroll position.
    theme.paint_canvas(frame, area);
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(area);

    let files = state
        .diff
        .as_ref()
        .map(|d| d.files.as_slice())
        .unwrap_or(&[]);
    let selected = state.diff_selected.min(files.len().saturating_sub(1));

    let list_block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme.border.normal))
        .title(Span::styled(
            t.diff_files_pane,
            Style::default().fg(theme.text.secondary),
        ));
    let list_inner = list_block.inner(list_area);
    let list_w = list_inner.width as usize;

    let mut rows: Vec<Line> = Vec::new();
    if files.is_empty() {
        rows.push(Line::from(Span::styled(
            t.diff_empty,
            Style::default().fg(theme.text.secondary),
        )));
    }
    for (i, f) in files.iter().enumerate() {
        let cursor = if i == selected { "› " } else { "  " };
        // Budget path + stats so long paths never spill past the pane edge.
        let stats = format!("  +{} -{}", f.added, f.removed);
        let stats_w = UnicodeWidthStr::width(stats.as_str());
        let cursor_w = UnicodeWidthStr::width(cursor);
        let path_budget = list_w.saturating_sub(cursor_w + stats_w).max(1);
        let path = truncate_display(&f.path, path_budget);
        rows.push(Line::from(vec![
            Span::styled(
                cursor.to_string(),
                Style::default().fg(theme.accent.primary),
            ),
            Span::raw(path),
            Span::styled(
                format!("  +{}", f.added),
                Style::default().fg(theme.status.success),
            ),
            Span::styled(
                format!(" -{}", f.removed),
                Style::default().fg(theme.status.error),
            ),
        ]));
    }
    frame.render_widget(list_block, list_area);
    render_list_focused(frame, list_inner, rows, selected, theme);

    let mut detail: Vec<Line> = Vec::new();
    if let Some(f) = files.get(selected) {
        match &f.patch {
            Some(patch) => {
                let wrap_w = detail_area.width.max(1) as usize;
                for raw in patch.lines() {
                    // Tabs / control chars desync cell columns on real terminals.
                    let clean = sanitize_terminal_line(raw);
                    let color = if clean.starts_with('+') && !clean.starts_with("+++") {
                        theme.diff.added
                    } else if clean.starts_with('-') && !clean.starts_with("---") {
                        theme.diff.removed
                    } else {
                        theme.text.secondary
                    };
                    // Wrap long patch lines instead of clipping them off-screen.
                    for piece in wrap(&clean, wrap_w) {
                        detail.push(Line::from(Span::styled(piece, Style::default().fg(color))));
                    }
                }
            }
            None => detail.push(Line::from(Span::styled(
                t.diff_reload_hint,
                Style::default().fg(theme.text.secondary),
            ))),
        }
    }
    detail.push(Line::from(""));
    detail.push(Line::from(Span::styled(
        t.diff_footer_hint,
        Style::default().fg(theme.text.secondary),
    )));
    render_scrolled(frame, detail_area, state, detail);
}

fn session_status_dot(status: &str, theme: &Theme) -> (&'static str, ratatui::style::Color) {
    let s = status.to_ascii_lowercase();
    if s.contains("complet") || s.contains("verif") || s == "done" {
        ("●", theme.status.success)
    } else if s.contains("fail") || s.contains("error") {
        ("●", theme.status.error)
    } else if s.contains("interrupt") || s.contains("cancel") {
        ("●", theme.status.warning)
    } else if s.contains("run") || s.contains("active") || s.contains("busy") {
        ("●", theme.accent.primary)
    } else {
        ("○", theme.text.secondary)
    }
}

pub(super) fn render_sessions_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let t = state.t();
    let mut lines: Vec<Line> = vec![screen_title(t.screen_sessions, theme), Line::from("")];
    if state.sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            t.sessions_empty,
            Style::default().fg(theme.text.secondary),
        )));
    }
    for (i, s) in state.sessions.iter().enumerate() {
        let cursor = if i == state.sessions_selected {
            "› "
        } else {
            "  "
        };
        let (dot, color) = session_status_dot(&s.status, theme);
        let goal = truncate_display(&s.goal, 40);
        lines.push(Line::from(vec![
            Span::styled(cursor, Style::default().fg(theme.accent.primary)),
            Span::styled(format!("{dot} "), Style::default().fg(color)),
            Span::raw(format!("{goal}  ")),
            Span::styled(
                format!("[{}] ", s.status),
                Style::default().fg(theme.text.secondary),
            ),
            Span::styled(s.model.clone(), Style::default().fg(theme.text.secondary)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t.sessions_footer_hint,
        Style::default().fg(theme.text.secondary),
    )));
    // +2 for the title and blank line that precede the session rows.
    render_list_focused(frame, area, lines, state.sessions_selected + 2, theme);
}
