use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use unicode_width::UnicodeWidthStr;

use crate::state::AppState;
use crate::theme::Theme;

use super::{render_list_focused, render_scrolled};
use super::text::{sanitize_terminal_line, truncate_display, wrap};

pub(super) fn render_help_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let zh = state.locale == crate::i18n::Locale::Zh;
    let page = crate::secondary::SecondaryPage {
        title: state.t().help_title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: if zh {
            "输入搜索 · ↑↓ 选择 · PageUp/Down 翻页 · Esc 返回"
        } else {
            "Type to search · ↑↓ select · PageUp/Down · Esc back"
        },
    };
    let layout = crate::secondary::layout(area, 1);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, &layout, &page, &state.theme);
    let theme = &state.theme;
    frame.render_widget(
        ratatui::widgets::Paragraph::new(sanitize_terminal_line(&format!(
            "{}{}",
            if zh { "搜索: " } else { "Search: " },
            state.help.query
        ))),
        layout.composer,
    );
    let matches = state.help.matches(state.t());
    let mut lines = Vec::new();
    let mut rows: Vec<(usize, String)> = Vec::new();
    let mut focus = 0;
    for (index, entry) in matches.iter().enumerate() {
        let selected = index == state.help.selected;
        if selected {
            focus = lines.len();
        }
        // An entry can wrap, so its hit starts at its first line and covers
        // the entry: the index is the binding's position in the result set.
        rows.push((lines.len(), index.to_string()));
        let text = sanitize_terminal_line(&format!(
            "{}{}  {}",
            if selected { "› " } else { "  " },
            entry.binding,
            entry.description
        ));
        let style = Style::default().fg(if selected {
            theme.accent.primary
        } else {
            theme.text.primary
        });
        lines.extend(
            wrap(&text, layout.content.width as usize)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, style))),
        );
    }
    if matches.is_empty() {
        lines.push(Line::from(if zh {
            "无匹配结果"
        } else {
            "No matches"
        }));
    }
    let mut hits = Vec::new();
    crate::render::render_list_rows(
        frame,
        layout.content,
        lines,
        &rows,
        focus,
        theme,
        &mut hits,
    );
    state.screen_rows = hits;
    search_cursor(frame, layout.composer, &state.help.query, zh);
}

pub(super) fn render_history_search(frame: &mut Frame, area: Rect, state: &AppState) {
    // Runtime approval/clarification always owns the foreground while pending.
    if state.overlay.is_some()
        || state.update.is_some()
        || state.surface != crate::btw::SurfaceFocus::Main
    {
        return;
    }
    let Some(search) = &state.history_search else {
        return;
    };
    let zh = state.locale == crate::i18n::Locale::Zh;
    let height = area.height.min(12);
    let panel = Rect {
        y: area.y + area.height.saturating_sub(height),
        height,
        ..area
    };
    frame.render_widget(ratatui::widgets::Clear, panel);
    let block = Block::default().borders(Borders::ALL).title(if zh {
        "输入历史 · Enter 放回草稿 · Esc 取消"
    } else {
        "Input history · Enter edit · Esc cancel"
    });
    let inner = block.inner(panel);
    frame.render_widget(block, panel);
    let [query_area, results] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    frame.render_widget(
        ratatui::widgets::Paragraph::new(sanitize_terminal_line(&format!(
            "{}{}",
            if zh { "搜索: " } else { "Search: " },
            search.query
        ))),
        query_area,
    );
    let matches = search.matches(state.composer.history());
    let mut lines = Vec::new();
    for (index, text) in matches.iter().enumerate() {
        let selected = index == search.selected;
        let preview = sanitize_terminal_line(&text.replace('\n', " ↵ "));
        lines.push(Line::from(Span::styled(
            format!("{}{}", if selected { "› " } else { "  " }, preview),
            Style::default().fg(if selected {
                state.theme.accent.primary
            } else {
                state.theme.text.primary
            }),
        )));
    }
    if matches.is_empty() {
        lines.push(Line::from(if zh {
            "无匹配历史"
        } else {
            "No matching history"
        }));
    }
    render_list_focused(frame, results, lines, search.selected, &state.theme);
    search_cursor(frame, query_area, &search.query, zh);
}

fn search_cursor(frame: &mut Frame, area: Rect, query: &str, zh: bool) {
    if area.width > 0 && area.height > 0 {
        let text = sanitize_terminal_line(&format!(
            "{}{}",
            if zh { "搜索: " } else { "Search: " },
            query
        ));
        let x = UnicodeWidthStr::width(text.as_str()).min(area.width.saturating_sub(1) as usize);
        frame.set_cursor_position((area.x + x as u16, area.y));
    }
}

pub(super) fn render_diff_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    // The shared shell, with the split inside its body: the page's identity
    // and its way back are the header, the hint is the footer, and the two
    // panes keep their own titles inside the content rect.
    let t = state.t();
    // A diff is always "of something": the branch and repo are this page's own
    // summary, and they belong on the header rather than in a second strip the
    // rest of the pages do not have.
    let page = crate::secondary::SecondaryPage {
        title: t.screen_diff,
        status: crate::status_line::header_line(state, area.width as usize).spans,
        hint: t.diff_footer_hint,
    };
    let layout = crate::secondary::layout(area, 0);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, &layout, &page, &state.theme);
    let theme = &state.theme;
    let area = layout.content;
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
    let mut row_hits: Vec<(usize, String)> = Vec::new();
    if files.is_empty() {
        // Loading is not empty, and a failed read is not empty either. Saying
        // "无改动" while a diff of thousands of files is still being computed —
        // or when git could not be read at all — is a false claim about the
        // workspace.
        if let Some(error) = &state.diff_error {
            rows.push(Line::from(Span::styled(
                t.diff_failed,
                Style::default().fg(theme.status.error),
            )));
            for piece in wrap(error, list_w.max(1)) {
                rows.push(Line::from(Span::styled(
                    piece,
                    Style::default().fg(theme.text.secondary),
                )));
            }
        } else {
            let text = if state.diff_pending {
                t.diff_loading
            } else {
                t.diff_empty
            };
            rows.push(Line::from(Span::styled(
                text,
                Style::default().fg(theme.text.secondary),
            )));
        }
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
        row_hits.push((rows.len() - 1, i.to_string()));
    }
    frame.render_widget(list_block, list_area);
    let mut hits = Vec::new();
    crate::render::render_list_rows(
        frame,
        list_inner,
        rows,
        &row_hits,
        selected,
        theme,
        &mut hits,
    );
    state.screen_rows = hits;

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
    // The binding hints are the shell's footer now; the patch pane ends on its
    // own content rather than repeating them.
    render_scrolled(frame, detail_area, state, detail);
}

fn session_status_dot(status: &str, theme: &Theme) -> (&'static str, ratatui::style::Color) {
    let s = status.to_ascii_lowercase();
    if s.contains("fail") || s.contains("error") {
        ("●", theme.status.error)
    } else if s.contains("interrupt") || s.contains("cancel") {
        ("●", theme.status.warning)
    } else if s.contains("run") || s.contains("active") || s.contains("busy") {
        ("●", theme.accent.primary)
    } else {
        ("○", theme.text.secondary)
    }
}

pub(super) fn render_sessions_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let page = crate::secondary::SecondaryPage {
        title: t.screen_sessions,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: t.sessions_footer_hint,
    };
    let layout = crate::secondary::layout(area, 0);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, &layout, &page, &state.theme);
    let theme = &state.theme;

    let mut lines: Vec<Line> = Vec::new();
    if state.sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            t.sessions_empty,
            Style::default().fg(theme.text.secondary),
        )));
    }
    let mut rows: Vec<(usize, String)> = Vec::new();
    for (i, s) in state.sessions.iter().enumerate() {
        // The row's key is its index: this page's cursor is positional, and a
        // click selects the row that was painted, not a remembered one.
        rows.push((lines.len(), i.to_string()));
        let cursor = if i == state.sessions_selected {
            "› "
        } else {
            "  "
        };
        let (dot, color) = session_status_dot(&s.status, theme);
        let status = if s.status == "completed" {
            match s.declaration {
                Some(leveler_client_protocol::UiTaskDeclaration::Answered) => "已回答",
                Some(leveler_client_protocol::UiTaskDeclaration::Completed) => "声明完成",
                None => "已结束",
            }
        } else {
            &s.status
        };
        let goal = truncate_display(&s.goal, 40);
        lines.push(Line::from(vec![
            Span::styled(cursor, Style::default().fg(theme.accent.primary)),
            Span::styled(format!("{dot} "), Style::default().fg(color)),
            Span::raw(format!("{goal}  ")),
            Span::styled(
                format!("[{status}] "),
                Style::default().fg(theme.text.secondary),
            ),
            Span::styled(s.model.clone(), Style::default().fg(theme.text.secondary)),
        ]));
    }
    let mut hits = Vec::new();
    crate::render::render_list_rows(
        frame,
        layout.content,
        lines,
        &rows,
        state.sessions_selected,
        theme,
        &mut hits,
    );
    state.screen_rows = hits;
}

#[cfg(test)]
mod search_tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn state() -> AppState {
        AppState::new(
            Theme::no_color(),
            crate::state::Boot {
                session_id: leveler_client_protocol::SessionId::new("search-test"),
                user: "u".into(),
                version: "test".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::En,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        )
    }
    fn help_text(state: &mut AppState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_help_screen(frame, frame.area(), state))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }
    #[test]
    fn history_search_never_covers_an_overlay_or_another_surface() {
        let mut state = state();
        state.history_search = Some(crate::history_search::HistorySearch::default());
        state.overlay = Some(crate::overlay::Overlay::ModelPicker(Box::new(
            crate::overlay::SelectionModel::new("picker", Vec::new(), false),
        )));
        for surface in [
            crate::btw::SurfaceFocus::Main,
            crate::btw::SurfaceFocus::Btw,
        ] {
            state.surface = surface;
            let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        ratatui::widgets::Paragraph::new("foreground owner"),
                        frame.area(),
                    );
                    render_history_search(frame, frame.area(), &state);
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("foreground owner"));
            assert!(!text.contains("Input history"));
            state.overlay = None;
        }
    }
    #[test]
    fn help_selection_remains_visible_after_narrow_resize() {
        let mut state = state();
        state.help.selected = state.help.matches(state.t()).len() - 1;
        for (width, height) in [(80, 24), (28, 10), (18, 7)] {
            let text = help_text(&mut state, width, height);
            assert!(
                text.contains("› Esc"),
                "selected row missing at {width}x{height}: {text}"
            );
        }
        state.help.query = "ctrl+j".into();
        state.help.selected = 0;
        assert!(help_text(&mut state, 28, 10).contains("Ctrl+J"));
        state.help.query = "missing-binding".into();
        assert!(help_text(&mut state, 28, 10).contains("No matches"));
    }
}
