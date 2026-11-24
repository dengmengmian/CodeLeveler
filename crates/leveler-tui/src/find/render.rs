//! The `/find` page: one Secondary Surface with a query row, a Search/Turns
//! switch, the search's real scope, and the results.
//!
//! Everything painted here comes from [`crate::find`]'s corpus or
//! [`crate::find::turn_entries`]; the page itself owns no content and never
//! consults the runtime. Row geometry is published through the same
//! `state.screen_rows` contract every other list page uses, so a click selects
//! exactly the row under the pointer.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::{FindHit, FindTab, turn_entries};
use crate::i18n::UiText;
use crate::render::truncate_display;
use crate::state::AppState;

pub fn render_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let page = crate::secondary::SecondaryPage {
        title: t.find.title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: t.find.hint,
    };
    let layout = crate::secondary::page_layout(state, area, 1);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, state, &layout, &page);
    let theme = &state.theme;

    render_query(frame, layout.composer, state, t);

    let content = layout.content;
    if content.width == 0 || content.height == 0 {
        return;
    }
    let [tabs_area, scope_area, body_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(content);

    render_tabs(frame, tabs_area, state, t);
    let scope = crate::find::scope_line(state, t);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_display(&scope, scope_area.width as usize),
            Style::default().fg(theme.text.muted),
        ))),
        scope_area,
    );
    if body_area.width == 0 || body_area.height == 0 {
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<crate::render::RowHitSpec> = Vec::new();
    let mut focus = 0usize;
    if let Some(notice) = &state.find.notice {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            Style::default().fg(theme.status.warning),
        )));
    }

    match state.find.tab {
        FindTab::Search => {
            render_search_body(state, body_area, t, &mut lines, &mut hits, &mut focus)
        }
        FindTab::Turns => render_turns_body(state, t, body_area, &mut lines, &mut hits, &mut focus),
    }

    crate::render::render_list_rows_with_spans(
        frame,
        body_area,
        lines,
        &hits,
        focus,
        theme,
        &mut state.screen_rows,
    );
}

fn render_query(frame: &mut Frame, area: Rect, state: &AppState, t: &UiText) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = &state.theme;
    let counter = if state.find.tab == FindTab::Search && !state.find.hits.is_empty() {
        format!(
            "{}/{}",
            state.find.selected_search.min(state.find.hits.len() - 1) + 1,
            state.find.hits.len()
        )
    } else {
        String::new()
    };
    let counter_width = UnicodeWidthStr::width(counter.as_str()) as u16;
    let counter_room = counter_width.min(area.width);
    let query_area = Rect {
        width: area.width.saturating_sub(counter_room.saturating_add(1)),
        ..area
    };
    let query = state.find.query.as_str();
    let empty = query.is_empty();
    let shown = if empty { t.find.placeholder } else { query };
    let style = if empty {
        Style::default().fg(theme.text.muted)
    } else {
        Style::default().fg(theme.text.primary)
    };
    let prefix = Span::styled(
        t.find.query_prefix.to_string(),
        Style::default().fg(theme.text.secondary),
    );
    let body = Span::styled(shown.to_string(), style);
    frame.render_widget(Paragraph::new(Line::from(vec![prefix, body])), query_area);
    if !counter.is_empty() && query_area.width < area.width {
        let start = area.x + query_area.width + 1;
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                counter,
                Style::default().fg(theme.text.meta),
            ))),
            Rect {
                x: start,
                width: counter_room,
                ..area
            },
        );
    }
    // The composer's caret sits at the end of the query, clamped to the row.
    let used = UnicodeWidthStr::width(
        format!("{}{}", t.find.query_prefix, if empty { "" } else { query }).as_str(),
    );
    let x = area.x + (used.min(query_area.width.saturating_sub(1) as usize)) as u16;
    frame.set_cursor_position((x, area.y));
}

fn render_tabs(frame: &mut Frame, area: Rect, state: &AppState, t: &UiText) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = &state.theme;
    let active = Style::default()
        .fg(theme.accent.primary)
        .add_modifier(Modifier::BOLD);
    let idle = Style::default().fg(theme.text.muted);
    let search = if state.find.tab == FindTab::Search {
        active
    } else {
        idle
    };
    let turns = if state.find.tab == FindTab::Turns {
        active
    } else {
        idle
    };
    let line = Line::from(vec![
        Span::styled(format!(" {} ", t.find.tab_search), search),
        Span::styled("│", Style::default().fg(theme.text.subtle)),
        Span::styled(format!(" {} ", t.find.tab_turns), turns),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_search_body(
    state: &AppState,
    body: Rect,
    t: &UiText,
    lines: &mut Vec<Line<'static>>,
    hits: &mut Vec<crate::render::RowHitSpec>,
    focus: &mut usize,
) {
    let theme = &state.theme;
    let scanning = state.find.searching || state.find.artifacts_done < state.find.artifacts_total;
    if state.find.query.trim().is_empty() {
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::Empty,
                message: t.find.no_query,
                detail: None,
            },
            theme,
            body.width as usize,
        ));
        return;
    }
    if state.find.hits.is_empty() {
        let (kind, message) = if scanning {
            (crate::render::ListStateKind::Loading, t.find.searching)
        } else {
            (crate::render::ListStateKind::FilterEmpty, t.find.no_results)
        };
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind,
                message,
                detail: None,
            },
            theme,
            body.width as usize,
        ));
        return;
    }
    let body_width = body.width as usize;
    for (index, hit) in state.find.hits.iter().enumerate() {
        let selected = index == state.find.selected_search;
        if selected {
            *focus = lines.len();
        }
        let mut title = String::new();
        if hit.turn > 0 {
            title.push_str(&format!("T{:02} · ", hit.turn));
        }
        title.push_str(hit.source.label(t));
        let mut meta = String::new();
        if let Some(artifact) = &hit.artifact {
            meta = t
                .find
                .artifact_line
                .replace("{}", &artifact.line.to_string());
        }
        let row = crate::render::ListRow::new(title)
            .meta(meta)
            .description_line(hit_body(hit, theme));
        let key = index.to_string();
        let paint = crate::render::row_paint(state, &key, selected, true);
        let row_lines = crate::render::list_row_lines(&row, paint, theme, body_width);
        hits.push(crate::render::RowHitSpec {
            line: lines.len(),
            height: row_lines.len() as u16,
            key,
        });
        lines.extend(row_lines);
    }
    if state.find.capped {
        lines.push(Line::from(Span::styled(
            truncate_display(
                &t.find
                    .capped
                    .replace("{}", &state.find.hits.len().to_string()),
                body_width,
            ),
            Style::default().fg(theme.text.muted),
        )));
    }
}

fn hit_body(hit: &FindHit, theme: &crate::theme::Theme) -> Line<'static> {
    let body_style = Style::default().fg(theme.text.secondary);
    let highlight = Style::default()
        .bg(theme.surface.selection)
        .fg(theme.accent.primary)
        .add_modifier(Modifier::BOLD);
    let before = &hit.snippet[..hit.match_start.min(hit.snippet.len())];
    let match_end = hit.match_end.min(hit.snippet.len());
    let mid = &hit.snippet[hit.match_start.min(match_end)..match_end];
    let after = &hit.snippet[match_end..];
    let mut spans = Vec::new();
    if !before.is_empty() {
        spans.push(Span::styled(before.to_string(), body_style));
    }
    if !mid.is_empty() {
        spans.push(Span::styled(mid.to_string(), body_style.patch(highlight)));
    }
    if !after.is_empty() {
        spans.push(Span::styled(after.to_string(), body_style));
    }
    Line::from(spans)
}

fn render_turns_body(
    state: &AppState,
    t: &UiText,
    body: Rect,
    lines: &mut Vec<Line<'static>>,
    hits: &mut Vec<crate::render::RowHitSpec>,
    focus: &mut usize,
) {
    let theme = &state.theme;
    let turns = turn_entries(state);
    if turns.is_empty() {
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::Empty,
                message: t.find.no_turns,
                detail: None,
            },
            theme,
            body.width as usize,
        ));
        return;
    }
    let body_width = body.width as usize;
    for (index, turn) in turns.iter().enumerate() {
        let selected = index == state.find.selected_turn;
        if selected {
            *focus = lines.len();
        }
        let status = if turn.running {
            t.find.turn_running.to_string()
        } else if let Some(status) = turn.status {
            crate::find::turn_status_label(status, t)
        } else {
            t.find.turn_no_status.to_string()
        };
        let meta = if let Some(secs) = turn.elapsed_secs {
            format!("{} · {}", status, crate::find::format_elapsed(secs))
        } else {
            status
        };
        let preview = turn.preview.replace('\n', " ");
        let row =
            crate::render::ListRow::new(t.find.turn_title.replace("{}", &turn.ordinal.to_string()))
                .meta(meta)
                .description(preview);
        let key = index.to_string();
        let paint = crate::render::row_paint(state, &key, selected, true);
        let row_lines = crate::render::list_row_lines(&row, paint, theme, body_width);
        hits.push(crate::render::RowHitSpec {
            line: lines.len(),
            height: row_lines.len() as u16,
            key,
        });
        lines.extend(row_lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Boot;
    use leveler_client_protocol::SessionId;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn state() -> AppState {
        let mut state = AppState::new(
            crate::theme::Theme::dark(),
            Boot {
                session_id: SessionId::new("find-render"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        );
        state.size = (80, 24);
        state
            .transcript
            .push_user("Implement the refund workflow".into());
        state.transcript.push_note("refund approved".into());
        state
    }

    fn screen(state: &mut AppState, cols: u16, rows: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        terminal
            .draw(|frame| render_screen(frame, frame.area(), state))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for row in buffer.content().chunks(cols as usize) {
            for cell in row {
                out.push_str(cell.symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_page_paints_query_tabs_and_matching_rows_without_panicking() {
        let mut state = state();
        state.active_screen = crate::screen::Screen::Find;
        state.find.query = "refund".into();
        state.find.generation = 1;
        state.find.corpus = Some(std::sync::Arc::new(crate::find::build_corpus(&state)));
        let (hits, capped) =
            crate::find::search_corpus(state.find.corpus.as_ref().unwrap(), "refund");
        crate::find::apply_transcript_results(&mut state, 1, hits, capped);
        let text = screen(&mut state, 80, 24);
        // A wide glyph occupies two buffer cells; the test backend leaves the
        // second as a space. Compare without them, and keep the ASCII rows
        // exact.
        let compact: String = text.chars().filter(|ch| *ch != ' ').collect();
        assert!(compact.contains("会话查找"), "{text}");
        assert!(compact.contains("搜索"), "{text}");
        assert!(compact.contains("回合"), "{text}");
        assert!(compact.contains("范围"), "{text}");
        assert!(text.contains("refund"), "{text}");
    }

    #[test]
    fn narrow_and_short_terminals_do_not_panic_or_overflow() {
        let mut state = state();
        state.active_screen = crate::screen::Screen::Find;
        state.find.query = "退款审批流程与很长的关键词".into();
        state.find.tab = FindTab::Turns;
        for (cols, rows) in [(30u16, 8u16), (40, 12), (120, 40)] {
            state.size = (cols, rows);
            let text = screen(&mut state, cols, rows);
            // One row per terminal row, and nothing painted outside the
            // canvas. (Wide glyphs make a character count wider than the
            // terminal; the buffer itself is exactly `cols` cells wide.)
            assert_eq!(text.lines().count(), rows as usize, "{cols}x{rows}");
            assert!(
                !text.contains('\u{FFFD}'),
                "{cols}x{rows} painted a replacement glyph"
            );
        }
    }
}
