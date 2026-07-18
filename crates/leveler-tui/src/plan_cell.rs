use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use leveler_client_protocol::PlanStepStatus;

use crate::render::{
    render_scrolled, screen_title, sub_agent_detail, sub_agent_display_name, sub_agent_status,
    sub_agent_usage, truncate_display,
};
use crate::state::AppState;
use crate::transcript::{ToolStatus, TranscriptItem};

/// The glyph for a plan step (never color-only, spec §31.1).
fn plan_glyph(status: PlanStepStatus) -> &'static str {
    match status {
        PlanStepStatus::Pending => "○",
        PlanStepStatus::Running => "●",
        PlanStepStatus::Done => "✓",
        PlanStepStatus::Failed => "✗",
        PlanStepStatus::Skipped => "–",
    }
}

pub(crate) fn render_plan_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let t = state.t();
    let mut lines: Vec<Line> = vec![screen_title(t.screen_plan, theme), Line::from("")];
    match &state.plan {
        Some(plan) if !plan.steps.is_empty() => {
            for step in &plan.steps {
                let color = match step.status {
                    PlanStepStatus::Done => theme.success,
                    PlanStepStatus::Failed => theme.error,
                    PlanStepStatus::Running => theme.accent,
                    _ => theme.muted,
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} ", plan_glyph(step.status)),
                        Style::default().fg(color),
                    ),
                    Span::raw(format!("{}. {}", step.index + 1, step.description)),
                ]));
            }
        }
        _ => lines.push(Line::from(Span::styled(
            t.no_plan.to_string(),
            Style::default().fg(theme.muted),
        ))),
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t.help_scroll.to_string(),
        Style::default().fg(theme.muted),
    )));
    render_scrolled(frame, area, state, lines);
}

/// The Agents screen (spec §54): the orchestrator and its task nodes, each run
/// by a sub-agent. Per-agent model/token and true parallelism are not surfaced
/// by the runtime's event stream today; nodes run sequentially.
pub(crate) fn render_agents_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let t = state.t();
    let mut lines: Vec<Line> = vec![screen_title(t.screen_agents, theme), Line::from("")];

    // Sub-agents spawned via `spawn_agent` (direct-mode multi-agent). These live
    // in the transcript, not the orchestrator plan, so list them here too.
    let subs: Vec<&crate::transcript::SubAgentBlock> = state
        .transcript
        .items()
        .iter()
        .filter_map(|i| match i {
            TranscriptItem::SubAgent(b) => Some(b),
            _ => None,
        })
        .collect();
    if !subs.is_empty() {
        lines.push(Line::from(Span::styled(
            t.agents_sub_agents,
            Style::default().fg(theme.accent),
        )));
        for b in &subs {
            let (glyph, color) = match b.status {
                ToolStatus::Running => ("●", theme.accent),
                ToolStatus::Ok => ("✓", theme.success),
                ToolStatus::Failed => ("✗", theme.error),
            };
            let mut spans = vec![
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(
                    sub_agent_display_name(b, t),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ),
            ];
            spans.push(Span::styled(
                format!(" · {}", sub_agent_status(b, t)),
                Style::default().fg(theme.muted),
            ));
            let usage = sub_agent_usage(b, t);
            if !usage.is_empty() {
                spans.push(Span::styled(
                    format!(" · {usage}"),
                    Style::default().fg(theme.muted),
                ));
            }
            lines.push(Line::from(spans));
            let detail_label = if b.status == ToolStatus::Running {
                t.sub_agent_task
            } else {
                t.sub_agent_result
            };
            let displayed_detail =
                format!("{detail_label}{}", sub_agent_detail(b.detail.trim(), t));
            let detail = displayed_detail.trim();
            if !detail.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        truncate_display(detail, area.width.saturating_sub(3).max(1) as usize),
                        Style::default().fg(theme.muted),
                    ),
                ]));
            }
        }
        lines.push(Line::from(""));
    }

    match &state.plan {
        Some(plan) if !plan.steps.is_empty() => {
            let running = plan
                .steps
                .iter()
                .any(|s| s.status == PlanStepStatus::Running);
            let orch_glyph = if running { "●" } else { "✓" };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{orch_glyph} {}", t.agents_orchestrator),
                    Style::default().fg(theme.accent),
                ),
                Span::styled(
                    if running {
                        format!("  {}", t.sub_agent_running)
                    } else {
                        format!("  {}", t.agents_idle)
                    },
                    Style::default().fg(theme.muted),
                ),
            ]));
            let last = plan.steps.len().saturating_sub(1);
            for (i, step) in plan.steps.iter().enumerate() {
                let branch = if i == last { "└─" } else { "├─" };
                let color = match step.status {
                    PlanStepStatus::Done => theme.success,
                    PlanStepStatus::Failed => theme.error,
                    PlanStepStatus::Running => theme.accent,
                    _ => theme.muted,
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{branch} "), Style::default().fg(theme.muted)),
                    Span::styled(
                        format!("{} ", plan_glyph(step.status)),
                        Style::default().fg(color),
                    ),
                    Span::raw(step.description.clone()),
                ]));
            }
        }
        // Only the empty state when there are neither sub-agents nor plan nodes.
        _ if subs.is_empty() => lines.push(Line::from(Span::styled(
            t.agents_empty,
            Style::default().fg(theme.muted),
        ))),
        _ => {}
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t.agents_scroll_hint,
        Style::default().fg(theme.muted),
    )));
    crate::render::render_scrolled(frame, area, state, lines);
}
