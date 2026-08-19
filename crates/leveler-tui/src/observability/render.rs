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
    let theme = &state.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(screen_title("Runtime Observatory", theme));
    lines.push(tab_line(state));
    lines.push(Line::from(""));

    let Some(loaded) = state.trace.loaded.as_ref() else {
        lines.push(Line::from(Span::styled(
            "查询中… 或尚无 durable 记录。",
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
        "1-6 tabs  Tab  ↑↓  Enter inspect  f filter  Esc 返回",
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
        let label = format!(" {} ", tab.label());
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
            format!("  filter:{}", state.trace.filter.label()),
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
            format!("Session  {}  ", truncate(s.session_id.as_str(), 12)),
            hi,
        ),
        Span::styled(format!("{} · {}", s.status, s.model), dim),
    ]));
    lines.push(Line::from(Span::styled(
        format!("{}  {} / {}", s.goal, s.work_profile, s.collaboration),
        dim,
    )));
    lines.push(Line::from(""));
    lines.push(kv(
        "MODEL",
        format!(
            "requests {}  in {}  out {}  last {}",
            s.request_count,
            fmt_tokens_compact(s.input_tokens.min(u32::MAX as u64) as u32),
            fmt_tokens_compact(s.output_tokens.min(u32::MAX as u64) as u32),
            s.last_latency_ms
                .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
                .unwrap_or_else(|| "—".into()),
        ),
        state,
    ));
    lines.push(kv(
        "TOOLS",
        format!("started {}  finished {}", s.tool_started, s.tool_finished),
        state,
    ));
    lines.push(kv(
        "GOAL",
        format!(
            "verify×{}  compact {}  agents {}  repair {}",
            s.verification_runs, s.compact_count, s.subagent_started, s.repair_started
        ),
        state,
    ));
    if let Some(seq) = s.last_sequence {
        lines.push(Line::from(Span::styled(
            format!(
                "last sequence {seq}  window {}–{}",
                loaded.window_from, loaded.window_to
            ),
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
            "此窗口没有可展示的 durable 事件。",
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
                format!("    type {}  at {}", row.event_type, row.created_at),
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
        format!(
            "Requests {}  in {}  out {}  avg {}  fail {}  retries {}",
            s.request_count,
            s.input_tokens,
            s.output_tokens,
            s.avg_latency_ms
                .map(|ms| format!("{ms}ms"))
                .unwrap_or_else(|| "—".into()),
            s.request_failures,
            s.request_retries
        ),
        Style::default().fg(theme.text.secondary),
    )));
    if loaded.requests.is_empty() {
        lines.push(Line::from(Span::styled(
            "无 durable model_requests 行（TokenUsage 流是 transient）。",
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
            "此会话没有 tool 记录。",
            Style::default().fg(theme.text.muted),
        )));
        return;
    }
    lines.push(Line::from(Span::styled(
        "全会话汇总（非当前窗口）",
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
            "此窗口没有 durable SubAgent 起止事件。",
            Style::default().fg(theme.text.muted),
        )));
        return;
    }
    lines.push(Line::from(Span::styled(
        "Main",
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
        "Interrupted turns",
        r.interrupted_turns.to_string(),
        state,
    ));
    lines.push(kv("Repair attempts", r.repair_attempts.to_string(), state));
    lines.push(kv(
        "Workspace snapshots",
        r.workspace_snapshots.to_string(),
        state,
    ));
    if r.review_stages.is_empty() {
        lines.push(kv("Review stages", "—".into(), state));
    } else {
        lines.push(kv("Review stages", r.review_stages.join(" → "), state));
    }
    lines.push(Line::from(Span::styled(
        "Owner epoch / fencing 不在协议上暴露。",
        Style::default().fg(state.theme.text.muted),
    )));
}

fn kv(k: &str, v: String, state: &AppState) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{k:<22}"),
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
