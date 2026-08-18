//! Workbench layout: fixed Header / Plan / Input / Footer + scrollable Conversation.
//!
//! Layout (top → bottom):
//! Header · Conversation (scroll) · gap · Status? · Plan · gap? · Input · gap · Footer
//!
//! `/btw` is a floating card over the Conversation bottom — not main history.

use leveler_client_protocol::{PlanStepStatus, UiPlan, UiPlanStep};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::i18n::UiText;
use crate::render::{
    COMPOSER_MAX_ROWS, btw_card_lines, composer_box_lines, composer_visible_rows,
    render_attachments, render_slash_popup,
};
use crate::screen::Screen;
use crate::state::AppState;
use crate::status_line::status_line_content;
use crate::transcript::TranscriptItem;

/// Done-count / total for a multi-step plan (`k/n`).
///
/// `k` counts steps that have finished successfully or were skipped; failed and
/// still-running/pending steps stay out of the numerator so progress does not
/// look complete while work remains.
pub(crate) fn plan_done_total(plan: &UiPlan) -> (usize, usize) {
    let n = plan.steps.len();
    let k = plan
        .steps
        .iter()
        .filter(|s| matches!(s.status, PlanStepStatus::Done | PlanStepStatus::Skipped))
        .count();
    (k, n)
}

/// Whether the sticky plan chrome should stay on screen.
///
/// Hide when every step is Done/Skipped (including a single 1/1 success) — the
/// answer is already in the transcript and a finished checklist only steals
/// space. Keep visible while anything is Pending/Running, or if any step Failed
/// so the user can still see what broke.
pub(crate) fn plan_panel_should_show(plan: &UiPlan) -> bool {
    if plan.steps.is_empty() {
        return false;
    }
    let all_success = plan
        .steps
        .iter()
        .all(|s| matches!(s.status, PlanStepStatus::Done | PlanStepStatus::Skipped));
    !all_success
}

/// The step the user should look at: running, else next pending, else first failed.
pub(crate) fn plan_current_step(plan: &UiPlan) -> Option<&UiPlanStep> {
    plan.steps
        .iter()
        .find(|s| s.status == PlanStepStatus::Running)
        .or_else(|| {
            plan.steps
                .iter()
                .find(|s| s.status == PlanStepStatus::Pending)
        })
        .or_else(|| {
            plan.steps
                .iter()
                .find(|s| s.status == PlanStepStatus::Failed)
        })
}

/// One-line plan chrome title. Always includes `k/n` and the current step when
/// the plan has steps — including when the panel is collapsed — so progress is
/// scannable from the conversation chrome.
pub(crate) fn plan_chrome_title(plan: &UiPlan, collapsed: bool, t: &UiText) -> String {
    let disclosure = if collapsed { "▶" } else { "▼" };
    let (k, n) = plan_done_total(plan);
    let mut title = format!("{disclosure} {} {k}/{n}", t.active_plan);
    if let Some(step) = plan_current_step(plan) {
        let desc = step.description.trim();
        if !desc.is_empty() {
            title.push_str(&format!(" · {}. {desc}", step.index + 1));
        }
    }
    title
}

/// Paint the conversation workbench into `frame`.
pub fn render_workbench(frame: &mut Frame, state: &mut AppState) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    state.theme.paint_canvas(frame, area);

    let attach_rows: u16 = if state.pending_attachments.is_empty() {
        0
    } else {
        1
    };
    let plan_rows = plan_panel_height(state);
    // An open overlay takes the composer's slot rather than floating over the
    // transcript, so the conversation shrinks by exactly what the decision box
    // needs and the message that raised it stays visible right above it.
    let workspace_inner = crate::layout::horizontal_inset(area, crate::layout::WORKSPACE_GUTTER_X);
    let composer_rows = match &state.overlay {
        Some(ov) => {
            crate::overlay::overlay_height(ov, &state.theme, workspace_inner.width, state.locale)
                .min(area.height.saturating_sub(8))
                .max(3)
        }
        None => composer_visible_rows(state, workspace_inner.width as usize)
            .clamp(3, COMPOSER_MAX_ROWS + 2) as u16,
    };
    // Header: blank breathing row + status line + hairline separator (3 rows)
    // so the brand strip is not flush against the terminal's top edge. Footer 1.
    let header_rows: u16 = 3;
    let footer_rows: u16 = 1;
    let footer_bottom: u16 = crate::layout::FOOTER_BOTTOM_PADDING;
    // One blank row between transcript and bottom chrome so the last answer /
    // turn-end marker does not sit flush on the composer border. Status only
    // takes a row when it has content so we do not stack two empty strips
    // when idle.
    let gap_rows: u16 = 1;
    // Notifications are painted as a floating toast (see
    // `render_notification_toast`) so they never grow this strip and reflow
    // the Conversation under a live text selection / copy.
    let status_line = status_line_content(state, area.width as usize);
    let status_rows: u16 = if status_line
        .spans
        .iter()
        .any(|span| !span.content.is_empty())
    {
        1
    } else {
        0
    };
    // Breathing room around the input box: blank above only when live chrome
    // (status / plan / attachments) sits on top of it; blank below always so
    // Context footer is not flush on the composer border.
    let chrome_above = status_rows
        .saturating_add(plan_rows)
        .saturating_add(attach_rows);
    let pre_composer_gap: u16 = if chrome_above > 0 { 1 } else { 0 };
    // The hint row replaces the blank below the composer rather than adding to
    // it, so hints appearing and disappearing never reflow the transcript.
    let hints = crate::render::key_hint_line(state, workspace_inner.width as usize);
    let post_composer_gap: u16 = 1;

    let chunks = Layout::vertical([
        Constraint::Length(header_rows),
        Constraint::Min(3), // conversation viewport
        Constraint::Length(gap_rows),
        Constraint::Length(status_rows),
        Constraint::Length(plan_rows),
        Constraint::Length(attach_rows),
        Constraint::Length(pre_composer_gap),
        Constraint::Length(composer_rows),
        Constraint::Length(post_composer_gap),
        Constraint::Length(footer_rows),
        Constraint::Length(footer_bottom),
    ])
    .split(area);

    let input_slot = crate::layout::horizontal_inset(chunks[7], crate::layout::WORKSPACE_GUTTER_X);
    let hint_slot = crate::layout::horizontal_inset(chunks[8], crate::layout::WORKSPACE_GUTTER_X);
    let footer_slot = crate::layout::horizontal_inset(chunks[9], crate::layout::WORKSPACE_GUTTER_X);

    render_header(frame, chunks[0], state);
    crate::conversation::viewport::render(frame, chunks[1], state);
    {
        let input_bg = state.theme.surface.input;
        state.theme.paint_surface(frame, input_slot, input_bg);
    }
    // chunks[2] = gap (leave blank)
    if status_rows > 0 {
        frame.render_widget(Paragraph::new(status_line), chunks[3]);
    }
    render_plan_panel(frame, chunks[4], state);
    render_attachments(frame, chunks[5], state);
    // chunks[6] = pre_composer_gap (leave blank)
    match &state.overlay {
        Some(overlay) => {
            crate::overlay::render_overlay(frame, input_slot, overlay, &state.theme, state.locale)
        }
        None => render_input(frame, input_slot, state),
    }
    // chunks[8]: the key hints when there are any, otherwise the blank gap.
    if let Some(line) = hints.into_iter().next() {
        frame.render_widget(Paragraph::new(line), hint_slot);
    }
    render_footer(frame, footer_slot, state);

    // /btw floats over the conversation viewport (not in the scroll stream).
    render_btw_overlay(frame, chunks[1], state);
    // Toast over conversation bottom — must not change vertical layout.
    render_notification_toast(frame, chunks[1], state);

    if state.active_screen == Screen::Conversation && state.overlay.is_none() {
        render_slash_popup(frame, chunks[1], input_slot, state);
    }
}

// ── Header (single-line environment strip + rule — no model / tokens) ───────

fn render_header(frame: &mut Frame, area: Rect, state: &AppState) {
    // Leading blank row keeps the brand strip off the terminal's top edge.
    let [_gap, status, rule_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    // One-column left inset so the brand does not sit flush on the edge.
    let text_area = Rect {
        x: status.x + 1,
        width: status.width.saturating_sub(1),
        ..status
    };
    frame.render_widget(
        Paragraph::new(header_status_line(state, text_area.width as usize)),
        text_area,
    );
    frame.render_widget(
        Paragraph::new(header_rule_line(area.width as usize, state)),
        rule_area,
    );
}

/// The header underline: always a static hairline. The status spinner above
/// the composer is the single busy indicator — an animated strip here would be
/// a second indeterminate-progress signal competing for attention.
fn header_rule_line(width: usize, state: &AppState) -> Line<'static> {
    let theme = &state.theme;
    let border = Style::default().fg(theme.border.normal);
    if width == 0 {
        return Line::from("");
    }
    Line::from(Span::styled("─".repeat(width), border))
}

/// Progressive single-line header that degrades as the terminal narrows.
///
/// Wide:   `CodeLeveler v0.1.0 · repo ·  main ●`
/// Medium: `CodeLeveler v0.1.0 ·  main ●`
/// Narrow: `CodeLeveler ·  main ●`
fn header_status_line(state: &AppState, width: usize) -> Line<'static> {
    let theme = &state.theme;
    let brand = Style::default()
        .fg(theme.accent.primary)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(theme.text.muted);
    let secondary = Style::default().fg(theme.text.secondary);
    let git = Style::default().fg(theme.status.success);

    let version = state.version();
    let branch = dirty_display(state.branch.as_deref().unwrap_or("—"));
    let full_repo = crate::status_line::home_collapsed_repo(state);
    let base_repo = repo_basename(&full_repo);

    let ver = format!(" v{version}");
    // Richest → sparsest plain-text candidates; first that fits wins.
    let texts = [
        format!("CodeLeveler{ver} · {full_repo} ·  {branch}"),
        format!("CodeLeveler{ver} · {base_repo} ·  {branch}"),
        format!("CodeLeveler{ver} ·  {branch}"),
        format!("CodeLeveler ·  {branch}"),
        "CodeLeveler".to_string(),
    ];

    let chosen = texts
        .into_iter()
        .find(|t| unicode_width::UnicodeWidthStr::width(t.as_str()) <= width)
        .unwrap_or_else(|| truncate("CodeLeveler", width));

    // Re-style the chosen plain string by scanning known prefixes.
    style_header_text(&chosen, brand, muted, secondary, git)
}

/// Apply brand / muted / secondary / git colors onto a pre-sized header string.
fn style_header_text(
    text: &str,
    brand: Style,
    muted: Style,
    secondary: Style,
    git: Style,
) -> Line<'static> {
    // Split on " · " while preserving separators as muted.
    let mut spans = Vec::new();
    let parts: Vec<&str> = text.split(" · ").collect();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ".to_string(), muted));
        }
        if i == 0 {
            // "CodeLeveler" [+ " vX.Y.Z"]
            if let Some(rest) = part.strip_prefix("CodeLeveler") {
                spans.push(Span::styled("CodeLeveler".to_string(), brand));
                if !rest.is_empty() {
                    spans.push(Span::styled(rest.to_string(), muted));
                }
            } else {
                spans.push(Span::styled((*part).to_string(), brand));
            }
        } else if part.starts_with('') || part.contains('') {
            spans.push(Span::styled((*part).to_string(), git));
        } else {
            spans.push(Span::styled((*part).to_string(), secondary));
        }
    }
    Line::from(spans)
}

/// Render the branch's dirty marker as a spaced dot instead of a glued `*`,
/// so `main*` reads as `main ●` and the marker is not mistaken for the name.
fn dirty_display(branch: &str) -> String {
    match branch.strip_suffix('*') {
        Some(base) => format!("{base} ●"),
        None => branch.to_string(),
    }
}

fn repo_basename(repo: &str) -> String {
    std::path::Path::new(repo)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(repo)
        .to_string()
}

// ── Plan panel ──────────────────────────────────────────────────────────────

/// Plan chrome only while the plan has open work (or failures). Empty /
/// fully-succeeded plans (including 1/1 ✓) take no rows.
fn plan_panel_height(state: &AppState) -> u16 {
    match &state.plan {
        Some(p) if plan_panel_should_show(p) => {
            if state.plan_collapsed {
                1
            } else {
                (p.steps.len() + 1).min(6) as u16
            }
        }
        _ => 0,
    }
}

fn render_plan_panel(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let Some(plan) = state.plan.as_ref().filter(|p| plan_panel_should_show(p)) else {
        return;
    };
    let theme = &state.theme;
    let t = state.t();
    let title = truncate(
        plan_chrome_title(plan, state.plan_collapsed, t),
        area.width as usize,
    );

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        title,
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    ))];

    if !state.plan_collapsed {
        for step in plan.steps.iter().take(area.height as usize - 1) {
            let (g, c) = match step.status {
                PlanStepStatus::Done => ("✓", theme.status.success),
                PlanStepStatus::Running => ("→", theme.accent.primary),
                PlanStepStatus::Failed => ("✗", theme.status.error),
                PlanStepStatus::Skipped => ("–", theme.text.secondary),
                PlanStepStatus::Pending => ("○", theme.text.secondary),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{g} "), Style::default().fg(c)),
                Span::styled(
                    truncate(
                        format!("{}. {}", step.index + 1, step.description),
                        area.width.saturating_sub(3) as usize,
                    ),
                    Style::default().fg(if step.status == PlanStepStatus::Running {
                        theme.text.primary
                    } else {
                        theme.text.secondary
                    }),
                ),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

// ── Floating notification toast (over Conversation bottom) ──────────────────

fn render_notification_toast(frame: &mut Frame, conv: Rect, state: &AppState) {
    let Some(note) = state.notification.as_ref() else {
        return;
    };
    if conv.height == 0 || conv.width < 8 {
        return;
    }
    let theme = &state.theme;
    let color = match note.level {
        leveler_client_protocol::NotificationLevel::Info => theme.accent.primary,
        leveler_client_protocol::NotificationLevel::Warning => theme.status.warning,
        leveler_client_protocol::NotificationLevel::Error => theme.status.error,
    };
    // One-line toast, bottom of conversation, right-aligned margin — no layout slot.
    let msg = truncate(
        format!(" {} ", note.message),
        conv.width.saturating_sub(2) as usize,
    );
    let w = (UnicodeWidthStr::width(msg.as_str()) as u16)
        .max(1)
        .min(conv.width.saturating_sub(2).max(1));
    let x = conv
        .x
        .saturating_add(conv.width.saturating_sub(w).saturating_sub(1));
    let y = conv.y.saturating_add(conv.height.saturating_sub(1));
    let area = Rect {
        x,
        y,
        width: w,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            msg,
            Style::default()
                .fg(color)
                .bg(theme.surface.elevated)
                .add_modifier(Modifier::BOLD),
        )),
        area,
    );
}

// ── /btw floating card (over Conversation bottom) ───────────────────────────

fn render_btw_overlay(frame: &mut Frame, conv: Rect, state: &AppState) {
    if conv.height < 4 || conv.width < 12 {
        return;
    }
    let Some(block) = state
        .transcript
        .items()
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::Btw(b) => Some(b),
            _ => None,
        })
    else {
        return;
    };

    let width = conv.width as usize;
    let lines = btw_card_lines(
        block,
        &state.theme,
        width.saturating_sub(2).max(12),
        state.t(),
    );
    if lines.is_empty() {
        return;
    }
    // Float above the bottom of the conversation viewport with a 1-col margin.
    let h = (lines.len() as u16)
        .min(conv.height.saturating_sub(1))
        .max(1);
    let y = conv.y + conv.height.saturating_sub(h);
    let x = conv.x.saturating_add(1);
    let w = conv.width.saturating_sub(2).max(1);
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    frame.render_widget(Paragraph::new(lines), area);
}

// ── Input box ───────────────────────────────────────────────────────────────

fn render_input(frame: &mut Frame, area: Rect, state: &mut AppState) {
    state.input_rect = Some((area.x, area.y, area.width, area.height));
    let (lines, (cx, cy)) = composer_box_lines(state, area.width as usize);
    let shown: Vec<Line> = lines.into_iter().take(area.height as usize).collect();
    frame.render_widget(Paragraph::new(shown), area);
    // Cursor only when Input owns focus (Conversation focus is for scrolling).
    let input_focused = state.overlay.is_none()
        && state.active_screen == Screen::Conversation
        && state.workbench_focus == crate::state::WorkbenchFocus::Input;
    if input_focused {
        let x = area.x + cx;
        let y = area.y + cy;
        if x < area.x + area.width && y < area.y + area.height {
            frame.set_cursor_position(ratatui::layout::Position::new(x, y));
        }
    }
}

// ── Footer ──────────────────────────────────────────────────────────────────

fn render_footer(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let muted = Style::default().fg(theme.text.secondary);
    let width = area.width as usize;

    // Footer: Context + optional cache hit rate. Shortcuts live in /help · Ctrl+?.
    let text = match crate::status_line::footer_status_line(state) {
        Some(line) => crate::render::truncate_display(&line, width),
        None => String::new(),
    };
    frame.render_widget(Paragraph::new(Line::from(Span::styled(text, muted))), area);
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn truncate(s: impl AsRef<str>, width: usize) -> String {
    crate::render::truncate_display(s.as_ref(), width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::PlanStepStatus;
    use leveler_client_protocol::{SessionId, UiPlan, UiPlanStep};

    fn test_state() -> AppState {
        AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        )
    }

    fn render_theme(theme: crate::theme::Theme) -> (crate::theme::Theme, ratatui::buffer::Buffer) {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = AppState::new(
            theme,
            crate::state::Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        (state.theme.clone(), terminal.backend().buffer().clone())
    }

    fn buffer_has_bg(buf: &ratatui::buffer::Buffer, want: ratatui::style::Color) -> bool {
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf.cell((x, y)).is_some_and(|c| c.bg == want) {
                    return true;
                }
            }
        }
        false
    }

    fn assert_opaque_workbench(theme: crate::theme::Theme) {
        use ratatui::style::Color;

        let canvas = theme.surface.canvas;
        let input = theme.surface.input;
        let (resolved, buf) = render_theme(theme);
        let header = buf.cell((2, 1)).expect("header cell");
        let conversation = buf.cell((2, 8)).expect("conversation cell");
        assert_ne!(header.bg, Color::Reset, "{:?} header leaked", resolved.id);
        assert_eq!(header.bg, canvas, "{:?} header canvas", resolved.id);
        assert_eq!(
            conversation.bg, canvas,
            "{:?} conversation canvas",
            resolved.id
        );
        assert!(
            buffer_has_bg(&buf, input),
            "{:?} input surface missing",
            resolved.id
        );
    }

    #[test]
    fn input_box_uses_workspace_gutter_and_inner_pad() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = test_state();
        state.theme = crate::theme::Theme::dark();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let (ix, iy, iw, ih) = state.input_rect.expect("input_rect published");
        assert_eq!(ix, crate::layout::WORKSPACE_GUTTER_X);
        assert_eq!(iw, 80 - crate::layout::WORKSPACE_GUTTER_X * 2);
        let buf = terminal.backend().buffer();
        let mut border_x = None;
        let mut prompt_x = None;
        for y in iy..iy.saturating_add(ih) {
            for x in 0..buf.area.width {
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                if border_x.is_none() && sym == "╭" {
                    border_x = Some(x);
                }
                if prompt_x.is_none() && sym == "›" {
                    prompt_x = Some(x);
                }
            }
        }
        assert_eq!(border_x, Some(ix), "input border follows the slot");
        assert_eq!(
            prompt_x,
            Some(ix + 1 + crate::layout::INPUT_INTERNAL_PADDING_X),
            "prompt sits one inner pad after the border"
        );
    }

    #[test]
    fn footer_uses_horizontal_gutter_and_bottom_pad() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = test_state();
        state.theme = crate::theme::Theme::dark();
        state.context_window_tokens = 1_000_000;
        state.context_tokens = 15_000;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut context_x = None;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf.cell((x, y)).is_some_and(|c| c.symbol() == "C") {
                    // First 'C' of "Context" near the bottom.
                    if y + 3 >= buf.area.height {
                        context_x = Some(x);
                        break;
                    }
                }
            }
        }
        assert_eq!(
            context_x,
            Some(crate::layout::WORKSPACE_GUTTER_X),
            "footer text must sit on the workspace gutter"
        );
        let last = buf.area.height.saturating_sub(1);
        for x in 0..buf.area.width {
            let sym = buf.cell((x, last)).map(|c| c.symbol()).unwrap_or(" ");
            assert!(
                sym.trim().is_empty(),
                "bottom pad row must be blank, col {x} is {sym:?}"
            );
        }
    }

    #[test]
    fn slash_popup_content_starts_after_border_and_pad() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = test_state();
        state.theme = crate::theme::Theme::dark();
        state.composer.replace("/");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let (ix, iy, _, _) = state.input_rect.expect("input slot");
        let buf = terminal.backend().buffer();
        let mut model_x = None;
        for y in iy.saturating_sub(12)..iy {
            for x in 0..buf.area.width {
                if buf.cell((x, y)).is_some_and(|c| c.symbol() == "/") {
                    model_x = Some(x);
                    break;
                }
            }
            if model_x.is_some() {
                break;
            }
        }
        assert_eq!(
            model_x,
            Some(ix + 1 + 1),
            "first command char is border + inner pad after popup origin"
        );
    }

    #[test]
    fn scrolled_conversation_keeps_top_and_bottom_breathing_room() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = test_state();
        state.theme = crate::theme::Theme::dark();
        for i in 0..40 {
            state.transcript.push_user(format!("ROW{i}"));
        }
        state.conv.auto_scroll = false;
        state.conv.scroll = 0;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let (cx, cy, _cw, ch) = state.conv.rect.expect("content rect");
        assert_eq!(cx, crate::layout::WORKSPACE_GUTTER_X);
        assert_eq!(
            cy,
            3 + crate::layout::CONVERSATION_PADDING_TOP,
            "content starts one row below the header rule"
        );
        assert!(ch >= 1);
        let buf = terminal.backend().buffer();
        let mut first_marker_y = None;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf.cell((x, y)).is_some_and(|c| c.symbol() == "▌") {
                    first_marker_y = Some(y);
                    break;
                }
            }
            if first_marker_y.is_some() {
                break;
            }
        }
        assert_eq!(
            first_marker_y,
            Some(cy),
            "scrolled-to-top first glyph sits on the content top, not the header"
        );
        for x in 0..buf.area.width {
            let sym = buf.cell((x, cy - 1)).map(|c| c.symbol()).unwrap_or(" ");
            assert!(
                sym != "▌" && !sym.starts_with('R'),
                "row above content must stay empty, col {x} is {sym:?}"
            );
        }
        let bottom_pad = cy.saturating_add(ch);
        for x in 0..buf.area.width {
            let sym = buf.cell((x, bottom_pad)).map(|c| c.symbol()).unwrap_or(" ");
            let badge =
                sym.contains('▼') || sym.chars().all(|c| c.is_ascii_digit() || c.is_whitespace());
            assert!(
                sym.trim().is_empty() || badge,
                "row below content must stay empty, col {x} is {sym:?}"
            );
        }
    }

    #[test]
    fn conversation_first_glyph_sits_on_the_gutter() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = test_state();
        state.theme = crate::theme::Theme::dark();
        state.transcript.push_user("GUTTERMARK".into());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut marker_x = None;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf.cell((x, y)).is_some_and(|c| c.symbol() == "▌") {
                    marker_x = Some(x);
                    break;
                }
            }
        }
        assert_eq!(
            marker_x,
            Some(crate::conversation::geometry::GUTTER_X),
            "top-level conversation marker must sit at the gutter, not the edge"
        );
    }

    #[test]
    fn conversation_wrapping_stays_inside_the_right_gutter() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = test_state();
        state.theme = crate::theme::Theme::dark();
        state.transcript.push_user("字".repeat(80));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let (cx, cy, cw, ch) = state.conv.rect.expect("conversation content rect");
        let content_right = cx.saturating_add(cw);
        let buf = terminal.backend().buffer();
        for y in cy..cy.saturating_add(ch) {
            for x in content_right..buf.area.width {
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                assert!(
                    sym.trim().is_empty() || sym.contains('▼'),
                    "col {x} row {y} leaked past content right {content_right}: {sym:?}"
                );
            }
        }
    }

    #[test]
    fn dark_automated_render_owns_surfaces() {
        assert_opaque_workbench(crate::theme::Theme::dark());
    }

    #[test]
    fn light_automated_render_owns_surfaces() {
        assert_opaque_workbench(crate::theme::Theme::light());
    }

    fn rule_plain(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn header_rule_is_static_hairline_when_idle() {
        let state = test_state();
        assert!(!state.is_busy());
        let line = header_rule_line(40, &state);
        let plain = rule_plain(&line);
        assert_eq!(plain, "─".repeat(40), "idle rule must be a plain hairline");
    }

    #[test]
    fn header_rule_stays_static_while_busy() {
        // The status spinner is the ONE busy indicator; a second animated
        // strip at the top competes with it for attention (dual-signal).
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        for tick in [0u64, 3, 7, 50] {
            state.tick = tick;
            let plain = rule_plain(&header_rule_line(48, &state));
            assert_eq!(
                plain,
                "─".repeat(48),
                "busy rule must stay a plain hairline at tick {tick}"
            );
        }
    }

    #[test]
    fn header_shows_full_path_when_wide_and_basename_when_narrow() {
        let mut state = test_state();
        state.repository = "/Users/me/Develop/app/codeleveler".into();
        state.branch = Some("main".into());
        let full = crate::status_line::home_collapsed_repo(&state);
        // Wide terminal fits the full home-collapsed path.
        let wide = rule_plain(&header_status_line(&state, 120));
        assert!(
            wide.contains(&full),
            "wide header should show full path: {wide}"
        );
        // Mid terminal degrades to the basename only.
        let narrow = rule_plain(&header_status_line(&state, 46));
        assert!(narrow.contains("codeleveler"), "narrow: {narrow}");
        assert!(
            !narrow.contains("Develop/app"),
            "narrow header should drop the full path: {narrow}"
        );
    }

    #[test]
    fn conversation_lines_reuses_cache_until_an_input_changes() {
        let mut s = test_state();
        s.transcript.push_user("hello".into());

        let a = s.conversation_lines(40);
        let b = s.conversation_lines(40);
        assert!(
            std::rc::Rc::ptr_eq(&a, &b),
            "unchanged inputs must return the very same cached Rc"
        );
        // The cached lines must equal a fresh uncached build (no staleness).
        assert_eq!(
            *a,
            crate::conversation::build::build_conversation_lines(&s, 40)
        );

        // A transcript mutation bumps the version → rebuild with new content.
        s.transcript.push_user("world".into());
        let c = s.conversation_lines(40);
        assert!(
            !std::rc::Rc::ptr_eq(&a, &c),
            "a content change must invalidate the cache"
        );
        assert_eq!(
            *c,
            crate::conversation::build::build_conversation_lines(&s, 40)
        );

        // A width change also rebuilds.
        let d = s.conversation_lines(60);
        assert!(!std::rc::Rc::ptr_eq(&c, &d), "a width change must rebuild");

        // An in-place edit via items_mut bumps the version too.
        let _ = s.transcript.items_mut();
        let e = s.conversation_lines(60);
        assert!(
            !std::rc::Rc::ptr_eq(&d, &e),
            "items_mut must invalidate the cache"
        );
    }

    fn sample_plan() -> UiPlan {
        UiPlan {
            steps: vec![
                UiPlanStep {
                    index: 0,
                    description: "read code".into(),
                    status: PlanStepStatus::Done,
                },
                UiPlanStep {
                    index: 1,
                    description: "edit module".into(),
                    status: PlanStepStatus::Running,
                },
                UiPlanStep {
                    index: 2,
                    description: "verify".into(),
                    status: PlanStepStatus::Pending,
                },
            ],
        }
    }

    #[test]
    fn plan_chrome_title_includes_kn_and_current_step_when_expanded() {
        let t = crate::i18n::Locale::Zh.text();
        let title = plan_chrome_title(&sample_plan(), false, t);
        assert!(title.starts_with('▼'), "{title}");
        assert!(title.contains("1/3"), "done/total: {title}");
        assert!(
            title.contains("2.") && title.contains("edit module"),
            "current running step: {title}"
        );
    }

    #[test]
    fn plan_chrome_title_keeps_progress_when_collapsed() {
        let t = crate::i18n::Locale::Zh.text();
        let title = plan_chrome_title(&sample_plan(), true, t);
        assert!(title.starts_with('▶'), "{title}");
        assert!(title.contains("1/3"), "{title}");
        assert!(title.contains("edit module"), "{title}");
    }

    #[test]
    fn plan_chrome_prefers_next_pending_when_none_running() {
        let t = crate::i18n::Locale::En.text();
        let plan = UiPlan {
            steps: vec![
                UiPlanStep {
                    index: 0,
                    description: "done step".into(),
                    status: PlanStepStatus::Done,
                },
                UiPlanStep {
                    index: 1,
                    description: "next work".into(),
                    status: PlanStepStatus::Pending,
                },
            ],
        };
        let title = plan_chrome_title(&plan, true, t);
        assert!(title.contains("1/2"), "{title}");
        assert!(title.contains("next work"), "{title}");
    }

    #[test]
    fn plan_panel_hidden_until_steps_exist() {
        let mut state = AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        );
        // Busy goal with no plan must not reserve chrome for "等待计划".
        state.goal_mode_active = true;
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.plan = None;
        assert_eq!(plan_panel_height(&state), 0);

        state.plan = Some(UiPlan { steps: vec![] });
        assert_eq!(plan_panel_height(&state), 0);

        state.plan = Some(sample_plan());
        state.plan_collapsed = true;
        assert_eq!(plan_panel_height(&state), 1);

        state.plan_collapsed = false;
        assert_eq!(plan_panel_height(&state), 4); // title + 3 steps
    }

    #[test]
    fn finished_plan_including_single_step_hides_panel() {
        let one_done = UiPlan {
            steps: vec![UiPlanStep {
                index: 0,
                description: "清理/重置任务计划".into(),
                status: PlanStepStatus::Done,
            }],
        };
        assert!(
            !plan_panel_should_show(&one_done),
            "1/1 complete must not keep sticky plan chrome"
        );

        let multi_done = UiPlan {
            steps: vec![
                UiPlanStep {
                    index: 0,
                    description: "a".into(),
                    status: PlanStepStatus::Done,
                },
                UiPlanStep {
                    index: 1,
                    description: "b".into(),
                    status: PlanStepStatus::Skipped,
                },
            ],
        };
        assert!(!plan_panel_should_show(&multi_done));

        let still_open = UiPlan {
            steps: vec![UiPlanStep {
                index: 0,
                description: "only step".into(),
                status: PlanStepStatus::Running,
            }],
        };
        assert!(
            plan_panel_should_show(&still_open),
            "single in-progress step still needs the panel"
        );

        let failed = UiPlan {
            steps: vec![UiPlanStep {
                index: 0,
                description: "broke".into(),
                status: PlanStepStatus::Failed,
            }],
        };
        assert!(
            plan_panel_should_show(&failed),
            "failed plan stays visible so the failure is scannable"
        );
    }

    #[test]
    fn consecutive_sub_agents_render_as_one_tree_in_conversation() {
        let mut s = test_state();
        s.transcript.push_sub_agent_started(
            "agent-1".into(),
            "Euclid".into(),
            "explorer".into(),
            "task A".into(),
            0,
        );
        s.transcript.push_sub_agent_started(
            "agent-2".into(),
            "Newton".into(),
            "explorer".into(),
            "task B".into(),
            0,
        );
        let lines = crate::conversation::build::build_conversation_lines(&s, 100);
        let text = lines.iter().map(rule_plain).collect::<Vec<_>>().join("\n");
        assert!(text.contains("2 个 agents 正在运行"), "{text}");
        assert!(text.contains("├─ Euclid"), "{text}");
        assert!(text.contains("└─ Newton"), "{text}");
    }

    #[test]
    fn final_answer_is_separated_from_the_last_tool_group() {
        let mut s = test_state();
        let call = leveler_client_protocol::ToolCallId::new("t1");
        s.transcript.push_tool_started(
            call.clone(),
            "read_file".into(),
            r#"{"path":"README.md"}"#.into(),
            false,
            0,
        );
        s.transcript.complete_tool(&call, true, "ok".into(), 1);
        let id = leveler_client_protocol::MessageId::new("m1");
        s.transcript.begin_assistant(id.clone());
        s.transcript.append_assistant(&id, "最终回答");
        s.transcript.finish_assistant(&id);

        let lines = crate::conversation::build::build_conversation_lines(&s, 80);
        let plain: Vec<String> = lines.iter().map(rule_plain).collect();
        let answer = plain
            .iter()
            .position(|l| l.contains("● 最终回答"))
            .unwrap_or_else(|| panic!("answer missing: {plain:?}"));
        assert!(answer >= 1, "tool group must precede the answer: {plain:?}");
        assert!(
            plain[answer - 1].trim().is_empty(),
            "a blank line must separate the final answer from the tool group: {plain:?}"
        );
    }
}
