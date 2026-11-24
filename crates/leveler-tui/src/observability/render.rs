//! Full-screen `/trace` renderer. Conversation geometry is not reused.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::render::render_scrolled;
use crate::state::AppState;
use crate::status_line::fmt_tokens_compact;

use super::model::TraceTab;

pub fn render_trace_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    // The shared shell: the page's identity, its status and its way back live
    // in the header, the tabs open the body, and the hint is the footer — the
    // same chrome every other independent page draws.
    let t = state.t();
    let page = crate::secondary::SecondaryPage {
        title: t.trace_title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: t.trace_hint,
    };
    let layout = crate::secondary::page_layout(state, area, 0);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, state, &layout, &page);
    let theme = &state.theme;
    let area = layout.content;
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(tab_line(state));
    lines.push(Line::from(""));

    let Some(loaded) = state.trace.loaded.as_ref() else {
        let block = if let Some(error) = &state.trace.error {
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::Error,
                message: error,
                detail: None,
            }
        } else {
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::Loading,
                message: t.trace_loading,
                detail: None,
            }
        };
        lines.extend(crate::render::list_state_lines(
            block,
            theme,
            area.width as usize,
        ));
        render_scrolled(frame, area, state, lines);
        return;
    };

    let mut hits = Vec::new();
    let mut trace_lines_out = Vec::new();
    match state.trace.tab {
        TraceTab::Trace => {}
        TraceTab::Overview => overview(&mut lines, state, loaded),
        TraceTab::Requests => requests(&mut lines, state, loaded),
        TraceTab::Tools => tools(&mut lines, state, loaded),
        TraceTab::Agents => agents(&mut lines, state, loaded),
        TraceTab::Recovery => recovery(&mut lines, state, loaded),
    }

    if state.trace.tab == TraceTab::Trace {
        const TRACE_SPLIT: crate::layout::SplitSpec = crate::layout::SplitSpec {
            axis: crate::layout::SplitAxis::Horizontal,
            primary_pct: 45,
            primary_min: 24,
            detail_min: 30,
            min_dual_width: 76,
            min_dual_height: 8,
        };
        let remainder = Rect {
            y: area.y.saturating_add(2),
            height: area.height.saturating_sub(2),
            ..area
        };
        let single_focus =
            if state.trace.inspect || state.page_focus == crate::layout::SplitFocus::Detail {
                crate::layout::SplitFocus::Detail
            } else {
                crate::layout::SplitFocus::Primary
            };
        let split = crate::layout::split_pane(remainder, TRACE_SPLIT)
            .unwrap_or_else(|| crate::layout::single_pane(remainder, single_focus));
        state.page_split = Some(crate::state::PageSplit {
            screen: crate::screen::Screen::Trace,
            layout: split,
        });
        if split.primary.width > 0 && split.primary.height > 0 {
            trace_lines(
                &mut trace_lines_out,
                &mut hits,
                state,
                loaded,
                split.primary.width as usize,
            );
            let focus = hits.get(state.trace.selected).map_or(0, |hit| hit.line);
            crate::render::render_list_rows_with_spans(
                frame,
                split.primary,
                trace_lines_out,
                &hits,
                focus,
                theme,
                &mut state.screen_rows,
            );
        } else {
            state.screen_rows.clear();
        }
        if split.detail.width > 0 && split.detail.height > 0 {
            let detail = trace_inspector_lines(state, split.detail.width as usize);
            let max = detail.len().saturating_sub(split.detail.height as usize);
            state.page_scroll = state.page_scroll.min(max);
            crate::render::render_pane_at_offset(
                frame,
                split.detail,
                detail,
                state.page_scroll,
                theme,
            );
        }
        crate::render::paint_split_divider(frame, &split, theme);
        let visible: Vec<Line<'static>> = lines.into_iter().take(2).collect();
        frame.render_widget(Paragraph::new(visible), Rect { height: 2, ..area });
    } else {
        state.page_split = None;
        state.screen_rows.clear();
        render_scrolled(frame, area, state, lines);
    }
}

fn tab_line(state: &AppState) -> Line<'static> {
    let theme = &state.theme;
    let mut spans = Vec::new();
    for tab in TraceTab::ALL {
        let label = format!(" {} ", tab.label(state.t()));
        if tab == state.trace.tab {
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(theme.accent.primary)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                label,
                Style::default().fg(theme.text.secondary),
            ));
        }
    }
    if state.trace.filter != super::model::TraceFilter::All {
        spans.push(Span::styled(
            state
                .t()
                .trace_filter_label
                .replace("{}", state.trace.filter.label(state.t())),
            Style::default().fg(theme.text.muted),
        ));
    }
    Line::from(spans)
}

fn overview(
    lines: &mut Vec<Line<'static>>,
    state: &AppState,
    loaded: &leveler_client_protocol::UiObservabilityLoaded,
) {
    let theme = &state.theme;
    let s = &loaded.session;
    let dim = Style::default().fg(theme.text.secondary);
    let hi = Style::default().fg(theme.text.primary);
    lines.push(Line::from(vec![
        Span::styled(
            state
                .t()
                .trace_session
                .replace("{}", &truncate(s.session_id.as_str(), 12)),
            hi,
        ),
        Span::styled(format!("{} · {}", s.status, s.model), dim),
    ]));
    lines.push(Line::from(Span::styled(
        format!("{}  {}", s.goal, s.collaboration),
        dim,
    )));
    lines.push(Line::from(""));
    let compact = fmt_tokens_compact;
    // A figure nobody recorded shows as an em dash, never as a zero.
    let unknown = "—".to_string();
    let t = state.t();
    lines.push(kv(
        t.trace_kv_model,
        t.trace_metrics_model
            .replacen("{}", &s.request_count.to_string(), 1)
            .replacen("{}", &compact(s.input_tokens), 1)
            .replacen(
                "{}",
                &s.cached_input_tokens
                    .map(compact)
                    .unwrap_or_else(|| unknown.clone()),
                1,
            )
            .replacen("{}", &compact(s.output_tokens), 1)
            .replacen(
                "{}",
                &s.last_latency_ms
                    .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
                    .unwrap_or_else(|| unknown.clone()),
                1,
            ),
        state,
    ));
    lines.push(kv(
        t.trace_kv_spend,
        format!(
            "{}  ·  {}",
            s.cost_usd_micros
                .map(|m| format!("${:.4}", m as f64 / 1_000_000.0))
                .unwrap_or_else(|| unknown.clone()),
            s.duration_ms
                .map(|ms| {
                    let secs = ms / 1000;
                    if secs >= 60 {
                        format!("{}m {:02}s", secs / 60, secs % 60)
                    } else {
                        format!("{secs}s")
                    }
                })
                .unwrap_or_else(|| unknown.clone()),
        ),
        state,
    ));
    for lane in s
        .lanes
        .iter()
        .filter(|l| l.requests > 0 && l.lane != "total")
    {
        lines.push(kv(
            &lane.lane.to_uppercase(),
            t.trace_metrics_lane
                .replacen("{}", &lane.requests.to_string(), 1)
                .replacen("{}", &compact(lane.input_tokens), 1)
                .replacen(
                    "{}",
                    &lane
                        .cached_input_tokens
                        .map(compact)
                        .unwrap_or_else(|| unknown.clone()),
                    1,
                )
                .replacen("{}", &compact(lane.output_tokens), 1)
                .replacen(
                    "{}",
                    &lane
                        .cost_usd_micros
                        .map(|m| format!("${:.4}", m as f64 / 1_000_000.0))
                        .unwrap_or_else(|| unknown.clone()),
                    1,
                ),
            state,
        ));
    }
    lines.push(kv(
        t.trace_kv_tools,
        t.trace_metrics_tools
            .replacen("{}", &s.tool_started.to_string(), 1)
            .replacen("{}", &s.tool_finished.to_string(), 1),
        state,
    ));
    lines.push(kv(
        t.trace_kv_goal,
        t.trace_metrics_goal
            .replacen("{}", &s.compact_count.to_string(), 1)
            .replacen("{}", &s.subagent_started.to_string(), 1),
        state,
    ));
    if let Some(seq) = s.last_sequence {
        lines.push(Line::from(Span::styled(
            t.trace_window_line
                .replacen("{}", &seq.to_string(), 1)
                .replacen("{}", &loaded.window_from.to_string(), 1)
                .replacen("{}", &loaded.window_to.to_string(), 1),
            dim,
        )));
    }
}

fn trace_lines(
    lines: &mut Vec<Line<'static>>,
    hits: &mut Vec<crate::render::RowHitSpec>,
    state: &AppState,
    _loaded: &leveler_client_protocol::UiObservabilityLoaded,
    width: usize,
) {
    let theme = &state.theme;
    let t = state.t();
    let rows = state.trace.filtered();
    if rows.is_empty() {
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind: if state.trace.filter == super::model::TraceFilter::All {
                    crate::render::ListStateKind::Empty
                } else {
                    crate::render::ListStateKind::FilterEmpty
                },
                message: if state.trace.filter == super::model::TraceFilter::All {
                    t.trace_empty_window
                } else {
                    t.trace_empty_filter
                },
                detail: None,
            },
            theme,
            width,
        ));
        return;
    }
    for (i, row) in rows.iter().enumerate() {
        let selected = i == state.trace.selected;
        let dur = row
            .duration_ms
            .map(|ms| {
                if ms >= 1000 {
                    format!("{:.1}s", ms as f64 / 1000.0)
                } else {
                    format!("{ms}ms")
                }
            })
            .unwrap_or_default();
        let (glyph, tone) = match row.status.as_str() {
            "fail" | "failed" | "error" => ("✗", crate::render::RowTone::Error),
            "running" => ("●", crate::render::RowTone::Info),
            "unknown" => ("?", crate::render::RowTone::Warning),
            "ok" | "success" => ("✓", crate::render::RowTone::Success),
            _ => ("·", crate::render::RowTone::Muted),
        };
        let title = format!("#{} {}", row.sequence, row.title);
        let description = format!("{} · {}", row.class.tag(), row.target);
        let list_row = crate::render::ListRow::new(title)
            .status(glyph, tone)
            .meta(dur)
            .description(description);
        let key = row.sequence.to_string();
        let paint = crate::render::row_paint(state, &key, selected, true);
        let row_lines = crate::render::list_row_lines(&list_row, paint, theme, width);
        hits.push(crate::render::RowHitSpec {
            line: lines.len(),
            height: row_lines.len() as u16,
            key,
        });
        lines.extend(row_lines);
    }
}

fn trace_inspector_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let t = state.t();
    let theme = &state.theme;
    let Some(row) = state.trace.selected_row() else {
        return crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::Empty,
                message: t.trace_empty_window,
                detail: None,
            },
            theme,
            width,
        );
    };
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("#{} {}", row.sequence, row.title),
        Style::default()
            .fg(theme.text.primary)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!("{} · {}", row.class.tag(), row.status),
        Style::default().fg(theme.text.muted),
    )));
    for field in &row.fields {
        let text = format!("{}: {}", field.key, field.value);
        for piece in crate::render::wrap(&crate::render::text::sanitize_terminal_line(&text), width)
        {
            lines.push(Line::from(Span::styled(
                piece,
                Style::default().fg(theme.text.secondary),
            )));
        }
    }
    lines.push(Line::from(Span::styled(
        t.trace_event_meta
            .replacen("{}", &row.event_type, 1)
            .replacen("{}", &row.created_at, 1),
        Style::default().fg(theme.text.muted),
    )));
    lines
}

fn requests(
    lines: &mut Vec<Line<'static>>,
    state: &AppState,
    loaded: &leveler_client_protocol::UiObservabilityLoaded,
) {
    let theme = &state.theme;
    let s = &loaded.session;
    lines.push(Line::from(Span::styled(
        state
            .t()
            .trace_requests_summary
            .replacen("{}", &s.request_count.to_string(), 1)
            .replacen("{}", &s.input_tokens.to_string(), 1)
            .replacen("{}", &s.output_tokens.to_string(), 1)
            .replacen(
                "{}",
                &s.avg_latency_ms
                    .map(|ms| format!("{ms}ms"))
                    .unwrap_or_else(|| "—".into()),
                1,
            )
            .replacen("{}", &s.request_failures.to_string(), 1)
            .replacen("{}", &s.request_retries.to_string(), 1),
        Style::default().fg(theme.text.secondary),
    )));
    if loaded.requests.is_empty() {
        lines.push(Line::from(Span::styled(
            state.t().trace_empty_requests,
            Style::default().fg(theme.text.muted),
        )));
        return;
    }
    lines.push(Line::from(Span::styled(
        format!(
            "  {:<6} {:<16} {:>8} {:>8} {:>8} {}",
            "#", "MODEL", "IN", "OUT", "LAT", "RESULT"
        ),
        Style::default().fg(theme.text.muted),
    )));
    for (i, r) in loaded.requests.iter().enumerate() {
        let result = r
            .finish_reason
            .clone()
            .or(r.error_kind.clone())
            .unwrap_or_else(|| "—".into());
        let lat = r
            .latency_ms
            .map(|ms| format!("{ms}ms"))
            .unwrap_or_else(|| "—".into());
        lines.push(Line::from(format!(
            "  {:<6} {:<16} {:>8} {:>8} {:>8} {result}",
            i + 1,
            truncate(&r.model, 16),
            r.input_tokens,
            r.output_tokens,
            lat
        )));
    }
}

fn tools(
    lines: &mut Vec<Line<'static>>,
    state: &AppState,
    loaded: &leveler_client_protocol::UiObservabilityLoaded,
) {
    let theme = &state.theme;
    if loaded.tools.is_empty() {
        lines.push(Line::from(Span::styled(
            state.t().trace_empty_tools,
            Style::default().fg(theme.text.muted),
        )));
        return;
    }
    lines.push(Line::from(Span::styled(
        state.t().trace_tools_caption,
        Style::default().fg(theme.text.secondary),
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "  {:<16} {:>6} {:>4} {:>5} {:>5} {:>8} {:>8}",
            "TOOL", "CALLS", "OK", "FAIL", "UNFIN", "TOTAL", "AVG"
        ),
        Style::default().fg(theme.text.muted),
    )));
    for t in &loaded.tools {
        lines.push(Line::from(format!(
            "  {:<16} {:>6} {:>4} {:>5} {:>5} {:>8} {:>8}",
            truncate(&t.name, 16),
            t.calls,
            t.succeeded,
            t.failed,
            t.unfinished,
            t.total_ms.map(fmt_ms).unwrap_or_else(|| "—".into()),
            t.avg_ms.map(fmt_ms).unwrap_or_else(|| "—".into()),
        )));
    }
}

fn agents(
    lines: &mut Vec<Line<'static>>,
    state: &AppState,
    loaded: &leveler_client_protocol::UiObservabilityLoaded,
) {
    let theme = &state.theme;
    if loaded.agents.is_empty() {
        lines.push(Line::from(Span::styled(
            state.t().trace_empty_agents,
            Style::default().fg(theme.text.muted),
        )));
        return;
    }
    lines.push(Line::from(Span::styled(
        state.t().trace_agents_root,
        Style::default().fg(theme.text.primary),
    )));
    for (i, a) in loaded.agents.iter().enumerate() {
        let branch = if i + 1 == loaded.agents.len() {
            "└─"
        } else {
            "├─"
        };
        lines.push(Line::from(format!(
            "{branch} {}  {}  {}  {}",
            a.nickname, a.role, a.status, a.summary
        )));
    }
}

fn recovery(
    lines: &mut Vec<Line<'static>>,
    state: &AppState,
    loaded: &leveler_client_protocol::UiObservabilityLoaded,
) {
    let r = &loaded.recovery;
    lines.push(kv(
        state.t().trace_kv_interrupted,
        r.interrupted_turns.to_string(),
        state,
    ));
    lines.push(kv(
        state.t().trace_kv_snapshots,
        r.workspace_snapshots.to_string(),
        state,
    ));
    if r.review_stages.is_empty() {
        lines.push(kv(state.t().trace_kv_review, "—".into(), state));
    } else {
        lines.push(kv(
            state.t().trace_kv_review,
            r.review_stages.join(" → "),
            state,
        ));
    }
    lines.push(Line::from(Span::styled(
        state.t().trace_fencing_note,
        Style::default().fg(state.theme.text.muted),
    )));
}

fn kv(k: &str, v: String, state: &AppState) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            crate::render::text::pad_display(k, 22),
            Style::default().fg(state.theme.text.secondary),
        ),
        Span::styled(v, Style::default().fg(state.theme.text.primary)),
    ])
}

fn fmt_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut o: String = s.chars().take(max.saturating_sub(1)).collect();
    o.push('…');
    o
}
