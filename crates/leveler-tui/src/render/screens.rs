use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use unicode_width::UnicodeWidthStr;

use crate::state::AppState;
use crate::theme::Theme;

use super::render_list_focused;
use super::text::{sanitize_terminal_line, wrap};

pub(super) fn render_help_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    if state.help.detail.is_some() {
        render_help_detail(frame, area, state);
        return;
    }
    let t = &state.t().help_page;
    let page = crate::secondary::SecondaryPage {
        title: t.title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: t.footer_list,
    };
    let layout = crate::secondary::page_layout(state, area, 1);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, state, &layout, &page);
    let theme = &state.theme;
    let category = state
        .help
        .category
        .map(|category| t.category_label(category).to_string())
        .unwrap_or_else(|| t.all_categories.to_string());
    let search = format!(
        "{}{}    [{}: {}]",
        t.search_label, state.help.query, t.category, category
    );
    frame.render_widget(
        ratatui::widgets::Paragraph::new(sanitize_terminal_line(&search))
            .style(Style::default().fg(state.theme.text.muted)),
        layout.composer,
    );
    // The cursor sits after the query text, before the category chip.
    let cursor_text = format!("{}{}", t.search_label, state.help.query);
    let cursor_text = sanitize_terminal_line(&cursor_text);
    let x = UnicodeWidthStr::width(cursor_text.as_str())
        .min(layout.composer.width.saturating_sub(1) as usize);
    frame.set_cursor_position((layout.composer.x + x as u16, layout.composer.y));
    let entries = state.help.matches(state.t());
    let mut lines = Vec::new();
    let mut hits: Vec<crate::render::RowHitSpec> = Vec::new();
    let mut focus = 0;
    let mut last_category: Option<crate::screen::SlashCategory> = None;
    let mut keys_started = false;
    for (index, entry) in entries.iter().enumerate() {
        if entry.is_key {
            if !keys_started {
                lines.push(crate::render::section_header_line(
                    t.key_bindings,
                    None,
                    None,
                    theme,
                    layout.content.width as usize,
                ));
                keys_started = true;
            }
        } else if last_category != entry.category {
            lines.push(crate::render::section_header_line(
                entry
                    .category
                    .map(|category| t.category_label(category))
                    .unwrap_or(t.all_categories),
                None,
                None,
                theme,
                layout.content.width as usize,
            ));
            last_category = entry.category;
        }
        let selected = index == state.help.selected;
        let mut title = entry.binding.clone();
        if !entry.aliases.is_empty() {
            title.push_str(&format!("  ({})", entry.aliases.join(", ")));
        }
        if state.is_busy() && entry.busy == Some(crate::screen::BusyPolicy::IdleOnly) {
            title.push_str(&format!("  · {}", t.unavailable_busy));
        }
        title.push_str("  ");
        title.push_str(&entry.description);
        let row = crate::render::ListRow::new(title);
        let key = entry.binding.clone();
        let paint = crate::render::row_paint(state, &key, selected, true);
        let row_lines =
            crate::render::list_row_lines(&row, paint, theme, layout.content.width as usize);
        if selected {
            focus = lines.len();
        }
        hits.push(crate::render::RowHitSpec {
            line: lines.len(),
            height: row_lines.len() as u16,
            key,
        });
        lines.extend(row_lines);
    }
    if entries.is_empty() {
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::FilterEmpty,
                message: t.no_matches,
                detail: None,
            },
            theme,
            layout.content.width as usize,
        ));
    }
    crate::render::render_list_rows_with_spans(
        frame,
        layout.content,
        lines,
        &hits,
        focus,
        theme,
        &mut state.screen_rows,
    );
}

fn push_help_field(lines: &mut Vec<Line<'static>>, theme: &Theme, label: &str, value: &str) {
    lines.push(Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(theme.text.muted)),
        Span::styled(
            sanitize_terminal_line(value),
            Style::default().fg(theme.text.primary),
        ),
    ]));
}

fn render_help_detail(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = &state.t().help_page;
    let page = crate::secondary::SecondaryPage {
        title: state.help.detail.unwrap_or(t.title),
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: t.footer_detail,
    };
    let layout = crate::secondary::page_layout(state, area, 0);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, state, &layout, &page);
    let theme = &state.theme;
    let Some(entry) = state.help.detail_entry(state.t()) else {
        return;
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                entry.binding.clone(),
                Style::default()
                    .fg(theme.text.primary)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  [{}]",
                    entry
                        .category
                        .map(|category| t.category_label(category))
                        .unwrap_or(t.all_categories)
                ),
                Style::default().fg(theme.text.muted),
            ),
        ]),
        Line::default(),
    ];
    push_help_field(&mut lines, theme, t.usage, entry.usage);
    let aliases = if entry.aliases.is_empty() {
        t.aliases_none.to_string()
    } else {
        entry.aliases.join(", ")
    };
    push_help_field(&mut lines, theme, t.aliases, &aliases);
    push_help_field(
        &mut lines,
        theme,
        t.availability,
        if state.is_busy() && entry.busy == Some(crate::screen::BusyPolicy::IdleOnly) {
            t.unavailable_busy
        } else {
            t.available
        },
    );
    push_help_field(
        &mut lines,
        theme,
        t.category,
        entry
            .category
            .map(|category| t.category_label(category))
            .unwrap_or(t.all_categories),
    );
    push_help_field(&mut lines, theme, t.source, t.source_registry);
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!("{}: ", t.description),
        Style::default().fg(theme.text.muted),
    )));
    for line in wrap(&entry.description, layout.content.width as usize) {
        lines.push(Line::from(Span::styled(
            line,
            Style::default().fg(theme.text.primary),
        )));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        t.detail_hint,
        Style::default().fg(theme.text.muted),
    )));
    let paragraph = ratatui::widgets::Paragraph::new(lines)
        .scroll((state.help.detail_scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, layout.content);
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
    let layout = crate::secondary::page_layout(state, area, 0);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, state, &layout, &page);
    let theme = &state.theme;
    let area = layout.content;
    // Wipe the whole content rect first so a shorter file/list can't leave
    // ghosts from the previous selection or scroll position.
    theme.paint_canvas(frame, area);
    const DIFF_SPLIT: crate::layout::SplitSpec = crate::layout::SplitSpec {
        axis: crate::layout::SplitAxis::Horizontal,
        primary_pct: 32,
        primary_min: 22,
        detail_min: 40,
        min_dual_width: 78,
        min_dual_height: 8,
    };
    let split = crate::layout::split_pane(area, DIFF_SPLIT)
        .unwrap_or_else(|| crate::layout::single_pane(area, state.page_focus));
    state.page_split = Some(crate::state::PageSplit {
        screen: crate::screen::Screen::Diff,
        layout: split,
    });
    let list_area = split.primary;
    let detail_area = split.detail;

    let files = state
        .diff
        .as_ref()
        .map(|d| d.files.as_slice())
        .unwrap_or(&[]);
    let selected = state.diff_selected.min(files.len().saturating_sub(1));

    if list_area.width > 0 && list_area.height > 0 {
        let list_w = list_area.width as usize;
        let mut rows: Vec<Line> = Vec::new();
        let mut row_hits: Vec<crate::render::RowHitSpec> = Vec::new();
        rows.push(crate::render::section_header_line(
            t.diff_files_pane,
            Some(files.len()),
            None,
            theme,
            list_w,
        ));
        if files.is_empty() {
            // Loading is not empty, and a failed read is not empty either.
            let block = if let Some(error) = &state.diff_error {
                crate::render::ListStateBlock {
                    kind: crate::render::ListStateKind::Error,
                    message: t.diff_failed,
                    detail: Some(error),
                }
            } else if state.diff_pending {
                crate::render::ListStateBlock {
                    kind: crate::render::ListStateKind::Loading,
                    message: t.diff_loading,
                    detail: None,
                }
            } else {
                crate::render::ListStateBlock {
                    kind: crate::render::ListStateKind::Empty,
                    message: t.diff_empty,
                    detail: None,
                }
            };
            rows.extend(crate::render::list_state_lines(block, theme, list_w));
        }
        for (index, file) in files.iter().enumerate() {
            let row = crate::render::ListRow::new(file.path.clone())
                .meta(format!("+{} -{}", file.added, file.removed));
            let focused = !split.dual || state.page_focus == crate::layout::SplitFocus::Primary;
            let paint = crate::render::row_paint(state, &file.path, index == selected, focused);
            let row_lines = crate::render::list_row_lines(&row, paint, theme, list_w);
            for line in row_lines {
                let line_index = rows.len();
                row_hits.push(crate::render::RowHitSpec {
                    line: line_index,
                    height: 1,
                    key: file.path.clone(),
                });
                rows.push(line);
            }
        }
        let focus = row_hits
            .iter()
            .position(|row| files.get(selected).is_some_and(|file| file.path == row.key))
            .map_or(0, |index| row_hits[index].line);
        crate::render::render_list_rows_with_spans(
            frame,
            list_area,
            rows,
            &row_hits,
            focus,
            theme,
            &mut state.screen_rows,
        );
    }

    if detail_area.width > 0 && detail_area.height > 0 {
        let mut detail: Vec<Line> = Vec::new();
        if let Some(f) = files.get(selected) {
            match &f.patch {
                Some(patch) => {
                    let rows: Vec<crate::diff_view::DiffRow> =
                        crate::diff_view::parse_diff_rows(patch)
                            .into_iter()
                            .filter(|row| {
                                !matches!(row, crate::diff_view::DiffRow::FileHeader { .. })
                            })
                            .collect();
                    let painted = crate::diff_view::paint_diff_rows(
                        &rows,
                        theme,
                        detail_area.width as usize,
                        &crate::diff_view::DiffPaintOptions {
                            first_lead: "",
                            rest_lead: "",
                            line_numbers: true,
                            fill_width: true,
                            hunk_headers: true,
                            patch_copy: false,
                            language: Some(f.path.as_str()),
                        },
                        &|unchanged| match unchanged {
                            Some(gap) => t.diff_unchanged_lines.replace("{}", &gap.to_string()),
                            None => "⋯".to_string(),
                        },
                    );
                    detail.extend(painted.into_iter().map(|row| row.line));
                }
                None => detail.push(Line::from(Span::styled(
                    t.diff_patch_pending,
                    Style::default().fg(theme.text.secondary),
                ))),
            }
        }
        let max = detail.len().saturating_sub(detail_area.height as usize);
        state.page_scroll = state.page_scroll.min(max);
        crate::render::render_pane_at_offset(frame, detail_area, detail, state.page_scroll, theme);
    }
    crate::render::paint_split_divider(frame, &split, theme);
}

fn session_status_dot(status: &str) -> (&'static str, crate::render::RowTone) {
    let s = status.to_ascii_lowercase();
    if s.contains("fail") || s.contains("error") {
        ("●", crate::render::RowTone::Error)
    } else if s.contains("interrupt") || s.contains("cancel") {
        ("●", crate::render::RowTone::Warning)
    } else if s.contains("run") || s.contains("active") || s.contains("busy") {
        ("●", crate::render::RowTone::Info)
    } else {
        ("○", crate::render::RowTone::Muted)
    }
}

pub(super) fn render_sessions_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let page = crate::secondary::SecondaryPage {
        title: t.screen_sessions,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: t.sessions_footer_hint,
    };
    let layout = crate::secondary::page_layout(state, area, 0);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, state, &layout, &page);
    let theme = &state.theme;
    let width = layout.content.width as usize;

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        t.sessions_filter_label
            .replacen("{}", state.sessions_filter.label(t), 1),
        Style::default().fg(theme.text.secondary),
    )));
    if let Some(error) = &state.sessions_error {
        let kind = if state.sessions.is_empty() {
            crate::render::ListStateKind::Error
        } else {
            crate::render::ListStateKind::Stale
        };
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind,
                message: error,
                detail: None,
            },
            theme,
            width,
        ));
    } else if state.sessions_loading {
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::Loading,
                message: t.sessions_loading,
                detail: None,
            },
            theme,
            width,
        ));
    } else if state.sessions.is_empty() {
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind: if state.all_sessions.is_empty() {
                    crate::render::ListStateKind::Empty
                } else {
                    crate::render::ListStateKind::FilterEmpty
                },
                message: if state.all_sessions.is_empty() {
                    t.sessions_empty
                } else {
                    t.sessions_empty_filter
                },
                detail: None,
            },
            theme,
            width,
        ));
    }
    let mut hits: Vec<crate::render::RowHitSpec> = Vec::new();
    for (i, s) in state.sessions.iter().enumerate() {
        // The wire status stays typed data; only the rendering is localized.
        // An unknown status is shown verbatim rather than guessed.
        let status = if s.status == "completed" {
            match s.declaration {
                Some(leveler_client_protocol::UiTaskDeclaration::Answered) => {
                    t.sessions_status_answered
                }
                Some(leveler_client_protocol::UiTaskDeclaration::Completed) => {
                    t.sessions_status_declared
                }
                None => t.sessions_status_ended,
            }
        } else if s.status == "active" {
            t.sessions_status_active
        } else {
            &s.status
        };
        let mut title = s.title.clone().unwrap_or_else(|| s.goal.clone());
        if s.archived {
            title.push_str(&format!(" [{}]", t.sessions_filter_archived));
        }
        let (dot, tone) = session_status_dot(&s.status);
        let description = if s.model.is_empty() {
            s.updated_at.clone()
        } else {
            format!("{} · {}", s.model, s.updated_at)
        };
        let row = crate::render::ListRow::new(title)
            .status(dot, tone)
            .meta(status)
            .description(description);
        let key = s.id.to_string();
        let paint = crate::render::row_paint(state, &key, i == state.sessions_selected, true);
        let row_lines = crate::render::list_row_lines(&row, paint, theme, width);
        hits.push(crate::render::RowHitSpec {
            line: lines.len(),
            height: row_lines.len() as u16,
            key,
        });
        lines.extend(row_lines);
    }
    let focus = hits
        .get(state.sessions_selected)
        .map(|hit| hit.line)
        .unwrap_or(0);
    crate::render::render_list_rows_with_spans(
        frame,
        layout.content,
        lines,
        &hits,
        focus,
        theme,
        &mut state.screen_rows,
    );
}

pub(super) fn render_foreign_sessions_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let title = match state.foreign_sessions.tool {
        Some(leveler_client_protocol::UiForeignTool::Codex) => t.screen_resume_codex,
        _ => t.screen_resume_claude,
    };
    let page = crate::secondary::SecondaryPage {
        title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: t.foreign_sessions_footer_hint,
    };
    let layout = crate::secondary::page_layout(state, area, 0);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, state, &layout, &page);
    let theme = &state.theme;
    let width = layout.content.width.max(1) as usize;

    let mut lines: Vec<Line> = Vec::new();
    if state.foreign_sessions.sessions.is_empty() {
        // Loading, a failed scan and a real empty result are three different
        // claims; only the last one may say "nothing found".
        let block = if let Some(error) = &state.foreign_sessions.error {
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::Error,
                message: error,
                detail: None,
            }
        } else if state.foreign_sessions.loading {
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::Loading,
                message: t.foreign_sessions_loading,
                detail: None,
            }
        } else {
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::Empty,
                message: t.foreign_sessions_empty,
                detail: None,
            }
        };
        lines.extend(crate::render::list_state_lines(block, theme, width));
    }
    let mut hits: Vec<crate::render::RowHitSpec> = Vec::new();
    for (index, session) in state.foreign_sessions.sessions.iter().enumerate() {
        // The source id stays on the row: it is the identity `/resume-* <id>`
        // takes, and the thing the person must not confuse with this native
        // session's own id.
        let mut meta = format!("{} · {}", session.native_id, session.updated_at);
        if let Some(cwd) = &session.cwd {
            meta.push_str(" · ");
            meta.push_str(&leveler_core::path::display_path(std::path::Path::new(cwd)));
        }
        if let Some(branch) = &session.branch {
            meta.push_str(" · ⎇ ");
            meta.push_str(branch);
        }
        let row = crate::render::ListRow::new(&session.title).description(meta);
        let key = session.native_id.clone();
        let paint =
            crate::render::row_paint(state, &key, index == state.foreign_sessions.selected, true);
        let row_lines = crate::render::list_row_lines(&row, paint, theme, width);
        hits.push(crate::render::RowHitSpec {
            line: lines.len(),
            height: row_lines.len() as u16,
            key,
        });
        lines.extend(row_lines);
    }
    let focus = hits
        .get(state.foreign_sessions.selected)
        .map(|hit| hit.line)
        .unwrap_or(0);
    crate::render::render_list_rows_with_spans(
        frame,
        layout.content,
        lines,
        &hits,
        focus,
        theme,
        &mut state.screen_rows,
    );
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
                text.contains("❯ Esc"),
                "selected row missing at {width}x{height}: {text}"
            );
        }
        state.help.query = "ctrl+j".into();
        state.help.selected = 0;
        assert!(help_text(&mut state, 28, 10).contains("Ctrl+J"));
        state.help.query = "missing-binding".into();
        assert!(
            help_text(&mut state, 28, 10).contains("No matching commands"),
            "the empty result names itself"
        );
    }
}
