use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use unicode_width::UnicodeWidthStr;

use leveler_client_protocol::CheckState;

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
        Style::default().fg(theme.muted),
    )));
    for (name, desc) in crate::screen::slash_commands(t) {
        lines.push(Line::from(vec![
            Span::styled(format!("  {name:<12}"), Style::default().fg(theme.accent)),
            Span::raw(desc.to_string()),
        ]));
    }
    lines.push(Line::from(""));

    // Sticky footer never lists these — Help / Ctrl+? is the learning surface.
    let keys = [
        ("Enter", t.key_submit),
        ("Ctrl+J / Alt+Enter", t.key_newline),
        ("Tab", "Input ↔ Conversation focus"),
        ("↑/↓ (Input)", "command history"),
        ("↑/↓ (Conversation)", "scroll messages"),
        ("PageUp/PageDown", "scroll Conversation"),
        ("Ctrl+C", t.key_cancel_quit),
        ("Ctrl+M", t.key_model),
        ("Ctrl+Q", "Queue"),
        ("Ctrl+O", t.key_expand),
        ("Ctrl+?", t.help_title),
        ("Ctrl+P/D/R/T/S/G", t.key_screens),
        ("Ctrl+End / Ctrl+↓", t.key_jump),
        ("End", t.key_end),
        ("Esc", t.key_esc),
    ];
    lines.push(Line::from(Span::styled(
        t.help_keys.to_string(),
        Style::default().fg(theme.muted),
    )));
    for (k, d) in keys {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<20}"), Style::default().fg(theme.accent)),
            Span::raw(d.to_string()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t.help_scroll.to_string(),
        Style::default().fg(theme.muted),
    )));
    render_scrolled(frame, area, state, lines);
}

pub(crate) fn screen_title(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ))
}

pub(super) fn render_verification_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let mut lines: Vec<Line> = vec![screen_title("验证结果", theme), Line::from("")];
    match &state.verification {
        Some(v) if !v.checks.is_empty() => {
            for check in &v.checks {
                let (glyph, color) = match check.status {
                    CheckState::Passed => ("✓", theme.success),
                    CheckState::Failed => ("✗", theme.error),
                    CheckState::Skipped => ("○", theme.muted),
                    CheckState::Running => ("●", theme.accent),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{glyph} "), Style::default().fg(color)),
                    Span::raw(check.name.clone()),
                ]));
                if let Some(evidence) = &check.evidence {
                    for line in wrap(evidence, area.width.saturating_sub(4).max(1) as usize)
                        .into_iter()
                        .take(6)
                    {
                        lines.push(Line::from(Span::styled(
                            format!("    {line}"),
                            Style::default().fg(theme.muted),
                        )));
                    }
                }
            }
            if let Some(passed) = v.passed {
                lines.push(Line::from(""));
                let (t, c) = if passed {
                    ("验证通过", theme.success)
                } else {
                    ("验证失败", theme.error)
                };
                lines.push(Line::from(Span::styled(t, Style::default().fg(c))));
            }
        }
        _ => lines.push(Line::from(Span::styled(
            "暂无验证结果",
            Style::default().fg(theme.muted),
        ))),
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑↓/PgUp/PgDn 滚动 · Esc 返回",
        Style::default().fg(theme.muted),
    )));
    render_scrolled(frame, area, state, lines);
}

pub(super) fn render_diff_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    // Wipe the whole split first so a shorter file/list can't leave ghosts
    // from the previous selection or scroll position.
    frame.render_widget(ratatui::widgets::Clear, area);
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
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" 文件 ", Style::default().fg(theme.muted)));
    let list_inner = list_block.inner(list_area);
    let list_w = list_inner.width as usize;

    let mut rows: Vec<Line> = Vec::new();
    if files.is_empty() {
        rows.push(Line::from(Span::styled(
            "无改动",
            Style::default().fg(theme.muted),
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
            Span::styled(cursor.to_string(), Style::default().fg(theme.accent)),
            Span::raw(path),
            Span::styled(
                format!("  +{}", f.added),
                Style::default().fg(theme.success),
            ),
            Span::styled(format!(" -{}", f.removed), Style::default().fg(theme.error)),
        ]));
    }
    frame.render_widget(list_block, list_area);
    render_list_focused(frame, list_inner, rows, selected);

    let mut detail: Vec<Line> = Vec::new();
    if let Some(f) = files.get(selected) {
        match &f.patch {
            Some(patch) => {
                let wrap_w = detail_area.width.max(1) as usize;
                for raw in patch.lines() {
                    // Tabs / control chars desync cell columns on real terminals.
                    let clean = sanitize_terminal_line(raw);
                    let color = if clean.starts_with('+') && !clean.starts_with("+++") {
                        theme.diff_add
                    } else if clean.starts_with('-') && !clean.starts_with("---") {
                        theme.diff_remove
                    } else {
                        theme.muted
                    };
                    // Wrap long patch lines instead of clipping them off-screen.
                    for piece in wrap(&clean, wrap_w) {
                        detail.push(Line::from(Span::styled(piece, Style::default().fg(color))));
                    }
                }
            }
            None => detail.push(Line::from(Span::styled(
                "Ctrl+D 刷新以加载补丁",
                Style::default().fg(theme.muted),
            ))),
        }
    }
    detail.push(Line::from(""));
    detail.push(Line::from(Span::styled(
        "↑↓ 选择文件 · PgUp/PgDn 滚动 · Esc 返回",
        Style::default().fg(theme.muted),
    )));
    render_scrolled(frame, detail_area, state, detail);
}

fn session_status_dot(status: &str, theme: &Theme) -> (&'static str, ratatui::style::Color) {
    let s = status.to_ascii_lowercase();
    if s.contains("complet") || s.contains("verif") || s == "done" {
        ("●", theme.success)
    } else if s.contains("fail") || s.contains("error") {
        ("●", theme.error)
    } else if s.contains("interrupt") || s.contains("cancel") {
        ("●", theme.warning)
    } else if s.contains("run") || s.contains("active") || s.contains("busy") {
        ("●", theme.accent)
    } else {
        ("○", theme.muted)
    }
}

pub(super) fn render_sessions_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let mut lines: Vec<Line> = vec![screen_title("会话", theme), Line::from("")];
    if state.sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            "暂无会话",
            Style::default().fg(theme.muted),
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
            Span::styled(cursor, Style::default().fg(theme.accent)),
            Span::styled(format!("{dot} "), Style::default().fg(color)),
            Span::raw(format!("{goal}  ")),
            Span::styled(format!("[{}] ", s.status), Style::default().fg(theme.muted)),
            Span::styled(s.model.clone(), Style::default().fg(theme.muted)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑↓ 选择 · Enter 打开 · d 删除 · Esc 返回",
        Style::default().fg(theme.muted),
    )));
    // +2 for the title and blank line that precede the session rows.
    render_list_focused(frame, area, lines, state.sessions_selected + 2);
}

pub(super) fn render_context_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let mut lines: Vec<Line> = vec![screen_title("上下文", theme), Line::from("")];
    if state.context_tokens == 0 && state.context_files.is_empty() {
        lines.push(Line::from(Span::styled(
            "暂无上下文信息（/workflow 编排工作流运行后生成）",
            Style::default().fg(theme.muted),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("估算 token：{}", state.context_tokens),
            Style::default().fg(theme.text),
        )));
        lines.push(Line::from(Span::styled(
            format!("候选文件（{}）", state.context_files.len()),
            Style::default().fg(theme.muted),
        )));
        for f in state.context_files.iter().take(40) {
            lines.push(Line::from(Span::raw(format!("  {f}"))));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑↓/PgUp/PgDn 滚动 · Esc 返回",
        Style::default().fg(theme.muted),
    )));
    render_scrolled(frame, area, state, lines);
}
