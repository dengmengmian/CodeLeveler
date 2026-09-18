//! Full-screen `/trace` renderer. Conversation geometry is not reused.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render::{render_list_focused, render_scrolled, screen_title};
use crate::state::AppState;
use crate::status_line::fmt_tokens_compact;

use super::model::TraceTab;

pub fn render_trace_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let area = crate::secondary::legacy_frame(frame, area, state);
    let theme = &state.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let t = state.t();
    lines.push(screen_title(t.trace_title, theme));
    lines.push(tab_line(state));
    lines.push(Line::from(""));

    let Some(loaded) = state.trace.loaded.as_ref() else {
        lines.push(Line::from(Span::styled(
            t.trace_loading,
            Style::default().fg(theme.text.secondary),
        )));
        render_scrolled(frame, area, state, lines);
        return;
    };

    match state.trace.tab {
        TraceTab::Overview => overview(&mut lines, state, loaded),
        TraceTab::Trace => trace_lines(&mut lines, state, loaded),
        TraceTab::Requests => requests(&mut lines, state, loaded),
        TraceTab::Tools => tools(&mut lines, state, loaded),
        TraceTab::Agents => agents(&mut lines, state, loaded),
        TraceTab::Recovery => recovery(&mut lines, state, loaded),
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t.trace_hint,
        Style::default().fg(theme.text.muted),
    )));

    if state.trace.tab == TraceTab::Trace {
        let focus = 3usize.saturating_add(state.trace.selected); // header rows
        render_list_focused(frame, area, lines, focus, theme);
    } else {
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
        format!("{}  {} / {}", s.goal, s.work_profile, s.collaboration),
        dim,
    )));
    lines.push(Line::from(""));
    let compact = |n: u64| fmt_tokens_compact(n.min(u32::MAX as u64) as u32);
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
            .replacen("{}", &s.verification, 1)
            .replacen("{}", &s.verification_runs.to_string(), 1)
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
    state: &AppState,
    _loaded: &leveler_client_protocol::UiObservabilityLoaded,
) {
    let theme = &state.theme;
    let rows = state.trace.filtered();
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            state.t().trace_empty_window,
            Style::default().fg(theme.text.secondary),
        )));
        return;
    }
    for (i, row) in rows.iter().enumerate() {
        let selected = i == state.trace.selected;
        let style = if selected {
            Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text.primary)
        };
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
        let mark = if selected { ">" } else { " " };
        lines.push(Line::from(Span::styled(
            format!(
                "{mark}#{:<5} {:<8} {:<16} {:<24} {dur}",
                row.sequence,
                row.class.tag(),
                truncate(&row.title, 16),
                truncate(&row.target, 24),
            ),
            style,
        )));
        if selected && state.trace.inspect {
            for field in &row.fields {
                lines.push(Line::from(Span::styled(
                    format!("    {}: {}", field.key, field.value),
                    Style::default().fg(theme.text.secondary),
                )));
            }
            lines.push(Line::from(Span::styled(
                state
                    .t()
                    .trace_event_meta
                    .replacen("{}", &row.event_type, 1)
                    .replacen("{}", &row.created_at, 1),
                Style::default().fg(theme.text.muted),
            )));
        }
    }
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
