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

/// The glyph for a plan step (never color-only, spec §31.1). Shared with the
/// workbench plan dock so one state never reads as two different things.
pub(crate) fn plan_glyph(status: PlanStepStatus) -> &'static str {
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
                    PlanStepStatus::Done => theme.status.success,
                    PlanStepStatus::Failed => theme.status.error,
                    PlanStepStatus::Running if state.is_busy() => theme.accent.primary,
                    _ => theme.text.secondary,
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
            Style::default().fg(theme.text.secondary),
        ))),
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t.help_scroll.to_string(),
        Style::default().fg(theme.text.secondary),
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
            Style::default().fg(theme.accent.primary),
        )));
        for b in &subs {
            let (glyph, color) = match b.status {
                ToolStatus::Running => ("●", theme.accent.primary),
                ToolStatus::Ok => ("✓", theme.status.success),
                ToolStatus::Failed => ("✗", theme.status.error),
            };
            let mut spans = vec![
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(
                    sub_agent_display_name(b, t),
                    Style::default()
                        .fg(theme.accent.primary)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ),
            ];
            spans.push(Span::styled(
                format!(" · {}", sub_agent_status(b, t)),
                Style::default().fg(theme.text.secondary),
            ));
            let usage = sub_agent_usage(b, t);
            if !usage.is_empty() {
                spans.push(Span::styled(
                    format!(" · {usage}"),
                    Style::default().fg(theme.text.secondary),
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
                        Style::default().fg(theme.text.secondary),
                    ),
                ]));
            }
        }
        lines.push(Line::from(""));
    }

    match &state.plan {
        Some(plan) if !plan.steps.is_empty() => {
            // Running is the runtime's turn status, not a step the plan
            // still declares in progress after the turn ended.
            let running = state.is_busy();
            let orch_glyph = if running { "●" } else { "✓" };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{orch_glyph} {}", t.agents_orchestrator),
                    Style::default().fg(theme.accent.primary),
                ),
                Span::styled(
                    if running {
                        format!("  {}", t.sub_agent_running)
                    } else {
                        format!("  {}", t.agents_idle)
                    },
                    Style::default().fg(theme.text.secondary),
                ),
            ]));
            let last = plan.steps.len().saturating_sub(1);
            for (i, step) in plan.steps.iter().enumerate() {
                let branch = if i == last { "└─" } else { "├─" };
                let color = match step.status {
                    PlanStepStatus::Done => theme.status.success,
                    PlanStepStatus::Failed => theme.status.error,
                    PlanStepStatus::Running if state.is_busy() => theme.accent.primary,
                    _ => theme.text.secondary,
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{branch} "),
                        Style::default().fg(theme.text.secondary),
                    ),
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
            Style::default().fg(theme.text.secondary),
        ))),
        _ => {}
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t.agents_scroll_hint,
        Style::default().fg(theme.text.secondary),
    )));
    crate::render::render_scrolled(frame, area, state, lines);
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::{RuntimeStatus, SessionId, UiPlan, UiPlanStep};

    fn state_with_open_plan(status: RuntimeStatus) -> AppState {
        let mut state = AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::En,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        );
        state.status = status;
        state.plan = Some(UiPlan {
            steps: vec![
                UiPlanStep {
                    index: 0,
                    description: "read".into(),
                    status: PlanStepStatus::Done,
                },
                UiPlanStep {
                    index: 1,
                    description: "edit".into(),
                    status: PlanStepStatus::Running,
                },
            ],
        });
        state
    }

    fn agents_screen_text(state: &AppState) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|f| render_agents_screen(f, f.area(), state))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..20)
            .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether the orchestrator is running is the runtime's turn status. A step
    /// the plan still declares in progress after the turn ended is not work
    /// under way.
    #[test]
    fn the_orchestrator_is_running_only_while_a_turn_runs() {
        // English: a wide CJK glyph spans two buffer cells.
        let t = crate::i18n::Locale::En.text();
        let idle = agents_screen_text(&state_with_open_plan(RuntimeStatus::Idle));
        let orchestrator = idle
            .lines()
            .find(|l| l.contains(t.agents_orchestrator))
            .expect("orchestrator row");
        assert!(orchestrator.contains(t.agents_idle), "{idle}");
        assert!(!orchestrator.contains(t.sub_agent_running), "{idle}");

        let busy = agents_screen_text(&state_with_open_plan(RuntimeStatus::Busy));
        let orchestrator = busy
            .lines()
            .find(|l| l.contains(t.agents_orchestrator))
            .expect("orchestrator row");
        assert!(orchestrator.contains(t.sub_agent_running), "{busy}");
    }
}
