//! Workbench layout: fixed Header / Plan / Input / Footer + scrollable Conversation.
//!
//! Layout (top → bottom):
//! Header · Conversation (scroll) · gap · Status? · Plan · gap? · Input · gap · Footer
//!
//! `/btw` is a floating card over the Conversation bottom — not main history.

use leveler_client_protocol::{PlanStepStatus, UiPlan};
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
use crate::status_line::status_lines;
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

/// Summary after `计划`/`plan`: active item when one is actually Running,
/// otherwise the completed count. Never claims item #1 is in progress just
/// because nothing is done yet.
pub(crate) fn plan_summary_label(plan: &UiPlan, t: &UiText) -> String {
    let (done, total) = plan_done_total(plan);
    if total == 0 {
        return t.active_plan.to_string();
    }
    if let Some(step) = plan
        .steps
        .iter()
        .find(|s| s.status == PlanStepStatus::Running)
    {
        t.plan_item_in_progress
            .replace("{current}", &(step.index + 1).to_string())
            .replace("{total}", &total.to_string())
    } else {
        t.plan_n_done
            .replace("{done}", &done.to_string())
            .replace("{total}", &total.to_string())
    }
}

/// One-line plan chrome title. A Running step reads as "第 n/N 项进行中"
/// rather than "0/N", which looked like zero progress while work was underway.
pub(crate) fn plan_chrome_title(plan: &UiPlan, collapsed: bool, t: &UiText) -> String {
    let disclosure = if collapsed { "▶" } else { "▼" };
    let summary = plan_summary_label(plan, t);
    if plan
        .steps
        .iter()
        .any(|s| s.status == PlanStepStatus::Running)
    {
        format!("{disclosure} {} · {summary}", t.active_plan)
    } else {
        format!("{disclosure} {} {summary}", t.active_plan)
    }
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
    let team_rows = team_panel_height(state);
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
    let status_block = status_lines(state, area.width as usize);
    let status_rows: u16 = if status_block
        .iter()
        .any(|line| line.spans.iter().any(|span| !span.content.is_empty()))
    {
        status_block.len().min(5) as u16
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

    // One breathing row between the Context footer and the roster, so the
    // process list reads as its own surface rather than a second footer.
    let team_gap: u16 = if team_rows > 0 { 1 } else { 0 };
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
        // The agent runtime roster lives BELOW the composer/status chrome:
        // above the composer is the work (plan, conversation), the composer is
        // what the user can do now, and below it is who is executing in the
        // background. Last in the stack also means a too-short terminal
        // squeezes the roster before it can crush approval/input/plan.
        Constraint::Length(team_gap),
        Constraint::Length(team_rows),
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
        frame.render_widget(Paragraph::new(status_block.clone()), chunks[3]);
        let (_, ids) =
            crate::activity::status_activity_lines(state, area.width as usize, state.t());
        let headline = status_block
            .len()
            .saturating_sub(ids.len())
            .min(status_rows as usize);
        state.activity_hits = ids
            .into_iter()
            .enumerate()
            .filter(|(i, _)| headline + i < status_rows as usize)
            .map(|(i, id)| (chunks[3].y + (headline + i) as u16, id))
            .collect();
    } else {
        state.activity_hits.clear();
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
    // chunks[10] = breathing row; the roster docks under the footer.
    render_team_panel(frame, chunks[11], state);

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

/// Rows the Task Team panel needs, or 0 when it should not appear.
///
/// Capped: a task is the primary object and the team is a caption on it. A
/// panel that grows with the child count would turn the workbench into an
/// agent dashboard, which is the shape this product deliberately avoids.
fn team_panel_height(state: &AppState) -> u16 {
    // Visibility is an ACTIVITY question, not a composition one: the surface
    // is live while children work or block, lingers briefly as a terminal
    // summary after the last one settles, then leaves. History stays in the
    // Agents screen / transcript — a settled team is not runtime state.
    if !state.team.surface_visible(state.elapsed_secs) {
        return 0;
    }
    // The turn clock stops when the turn ends, so the linger window alone
    // cannot expire the terminal row afterwards — and it shouldn't: once the
    // turn settles, 任务已完成 is the single completion owner (§16) and the
    // collaboration surface yields immediately. Open blocking findings keep
    // it on screen: the user still owes those a decision.
    if !state.is_busy() && state.team.open_blocking() == 0 {
        return 0;
    }
    if state.collaboration_collapsed || state.team.surface_is_terminal(state.elapsed_secs) {
        return 1;
    }
    // Roster shape: optional blocking banner + Main row + up to 4 child rows
    // + an overflow row. Capped so the runtime surface stays a caption on
    // the task, never most of the viewport.
    let children = state.team.children.len();
    let shown = children.min(4);
    let overflow = usize::from(children > shown);
    let banner = usize::from(state.team.open_blocking() > 0);
    (banner + 1 + shown + overflow).min(7) as u16
}

fn render_team_panel(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let theme = &state.theme;
    let t = state.t();
    // Same primary task baseline as the plan dock and the conversation
    // content: the collaboration row is task-runtime state, a sibling of the
    // plan — not chrome hanging off the terminal's left edge.
    let area = crate::layout::horizontal_inset(area, crate::layout::WORKSPACE_GUTTER_X);
    if area.width == 0 {
        return;
    }
    const MEMBER_INDENT: &str = "  ";
    let blocking = state.team.open_blocking() > 0;
    let terminal = state.team.surface_is_terminal(state.elapsed_secs);
    if area.height == 1 || terminal {
        // Compact / terminal: one truthful row, nothing else.
        let glyph = if terminal { "✓" } else { "◉" };
        let row = format!(
            "{glyph} {}",
            crate::multi_agent::collaboration_compact_line(&state.team, t)
        );
        let bad = state
            .team
            .children
            .iter()
            .any(|c| c.status == crate::multi_agent::ChildStatus::Failed);
        let color = if blocking || bad {
            theme.status.error
        } else if terminal {
            theme.status.success
        } else {
            theme.accent.primary
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate(row, area.width as usize),
                Style::default().fg(color),
            ))),
            area,
        );
        return;
    }
    // Active agent runtime roster: WHO is working, WHAT each is doing, for
    // HOW LONG, at what accumulated usage. A process-list shape — no boxes,
    // no cards. Main leads as the coordinator/root row (it is not a spawned
    // child); the existing structured activity label is its source, with a
    // truthful "正在工作" fallback rather than invented detail.
    let mut lines: Vec<Line> = Vec::new();
    if blocking {
        lines.push(Line::from(Span::styled(
            truncate(
                crate::multi_agent::team_panel_title(&state.team, t),
                area.width as usize,
            ),
            Style::default()
                .fg(theme.status.error)
                .add_modifier(Modifier::BOLD),
        )));
    }
    let rows = crate::multi_agent::roster_rows(
        &state.team,
        state.activity.as_deref(),
        state.elapsed_secs,
        t,
    );
    let budget = (area.height as usize).saturating_sub(lines.len());
    // Main always leads; children take what remains. If they do not all
    // fit, one line is reserved for the "还有 N 个 Agent…" overflow note.
    let child_rows = rows.len() - 1;
    let avail = budget.saturating_sub(1);
    let shown_children = if child_rows <= avail {
        child_rows
    } else {
        avail.saturating_sub(1)
    };
    let width = area.width as usize;
    // Narrow terminals drop the meta column rather than squeezing the
    // activity into unreadability.
    let narrow = width < 48;
    // The roster is a compact process list, not a full-width table: on wide
    // terminals the `elapsed · tokens` column aligns to the roster's own
    // content inside a bounded readable width — never to the terminal's
    // right edge, which turns each row into two disconnected islands.
    const ROSTER_MAX_WIDTH: usize = 96;
    const META_GAP: usize = 4;
    let bound = width.min(ROSTER_MAX_WIDTH);
    let visible: Vec<&crate::multi_agent::AgentRosterRow> =
        rows.iter().take(1 + shown_children).collect();
    // Shared column width: the widest meta among the rows that carry one.
    let meta_col = if narrow {
        0
    } else {
        visible
            .iter()
            .filter_map(|row| row.meta.as_deref())
            .map(UnicodeWidthStr::width)
            .max()
            .unwrap_or(0)
    };
    // Every row's activity respects the shared meta reservation so the
    // column stays vertically aligned; the column position itself is
    // content-driven (widest head+activity plus breathing room).
    let mut prepared: Vec<(String, usize, String, usize)> = Vec::new();
    let mut content_w = 0usize;
    for row in &visible {
        let head = format!("{} {}  ", row.glyph, row.label);
        let head_w = UnicodeWidthStr::width(head.as_str());
        let cap = if meta_col > 0 {
            bound.saturating_sub(meta_col + META_GAP + head_w).max(4)
        } else {
            width.saturating_sub(head_w).max(4)
        };
        let activity = truncate(row.activity.clone(), cap);
        let activity_w = UnicodeWidthStr::width(activity.as_str());
        content_w = content_w.max(head_w + activity_w);
        prepared.push((head, head_w, activity, activity_w));
    }
    let meta_x = content_w + META_GAP;
    for (row, (head, head_w, activity, activity_w)) in visible.iter().zip(prepared) {
        let color = match row.tone {
            crate::multi_agent::RosterTone::Main => theme.accent.primary,
            crate::multi_agent::RosterTone::Active => theme.accent.primary,
            crate::multi_agent::RosterTone::Done => theme.status.success,
            crate::multi_agent::RosterTone::Failed => theme.status.error,
        };
        let mut spans = vec![
            Span::styled(head, Style::default().fg(color)),
            Span::styled(activity, Style::default().fg(theme.text.secondary)),
        ];
        if let Some(meta) = row.meta.as_deref()
            && !narrow
        {
            let pad = meta_x.saturating_sub(head_w + activity_w);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(
                meta.to_string(),
                Style::default().fg(theme.text.secondary),
            ));
        }
        lines.push(Line::from(spans));
    }
    let hidden = child_rows - shown_children;
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "{MEMBER_INDENT}{}",
                t.agent_roster_more.replace("{n}", &hidden.to_string())
            ),
            Style::default().fg(theme.text.secondary),
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
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
    // The plan is task-level state, a sibling of the conversation — so it
    // starts at the same content baseline instead of hanging off the left
    // edge, and its steps sit one level inside their own header.
    let area = crate::layout::horizontal_inset(area, crate::layout::WORKSPACE_GUTTER_X);
    if area.width == 0 {
        return;
    }
    const STEP_INDENT: &str = "  ";
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
                Span::styled(format!("{STEP_INDENT}{g} "), Style::default().fg(c)),
                Span::styled(
                    truncate(
                        format!("{}. {}", step.index + 1, step.description),
                        area.width.saturating_sub(3 + STEP_INDENT.len() as u16) as usize,
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

    /// Render the workbench with a given team and return the screen text.
    fn render_with_team(team: crate::multi_agent::TaskTeamView) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = AppState::new(
            crate::theme::Theme::default(),
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
        // An active team surface only shows during a busy turn — settled
        // collaborations yield to the main completion state.
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.team = team;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            out.push('\n');
        }
        out
    }

    /// Full-workbench render with a team, a plan, custom size/locale, and a
    /// turn clock (so `elapsed · tokens` metas materialize).
    fn render_workbench(
        width: u16,
        height: u16,
        locale: crate::i18n::Locale,
        elapsed: u64,
        team: crate::multi_agent::TaskTeamView,
        plan: bool,
    ) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = AppState::new(
            crate::theme::Theme::default(),
            crate::state::Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        );
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.elapsed_secs = elapsed;
        // A used context so the `Context …` footer chip actually renders.
        state.context_tokens = 59_000;
        state.team = team;
        if plan {
            state.plan = Some(sample_plan());
        }
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        buf.cell((x, y))
                            .and_then(|c| c.symbol().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect::<String>()
            })
            .collect()
    }

    fn team_with_usage(specs: &[(&str, &str, &str, u32)]) -> crate::multi_agent::TaskTeamView {
        let mut team = team_with(
            &specs
                .iter()
                .map(|(id, role, act, _)| (*id, *role, *act))
                .collect::<Vec<_>>(),
        );
        for (id, _, _, tokens) in specs {
            if *tokens > 0 {
                team.apply_progress(id, true, *tokens / 2, *tokens - *tokens / 2);
            }
        }
        team
    }

    /// Terminal COLUMN of `needle` (each extracted cell is one char, so the
    /// char count — not the byte offset — is the column).
    fn col_of(line: &str, needle: &str) -> Option<usize> {
        line.find(needle).map(|b| line[..b].chars().count())
    }

    fn row_of(lines: &[String], needle: &str) -> usize {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` not on screen:\n{}", lines.join("\n")))
    }

    /// Final IA (placement closure): Plan, then the composer, then the
    /// Context footer — and the roster BELOW all of them, one breathing row
    /// under the footer. The roster is a bottom runtime surface, not
    /// pre-composer chrome.
    #[test]
    fn the_roster_docks_below_the_context_footer_and_never_above_plan() {
        let lines = render_workbench(
            100,
            34,
            crate::i18n::Locale::En,
            55,
            team_with_usage(&[("a1", "explorer", "read_file", 158_000)]),
            true,
        );
        let plan_row = row_of(&lines, "edit module");
        let input_row = row_of(&lines, "Type a message");
        let footer_row = row_of(&lines, "Context");
        let roster_row = row_of(&lines, "● Main");
        assert!(plan_row < input_row, "plan must stay above the composer");
        assert!(input_row < footer_row, "composer above the Context footer");
        assert!(
            footer_row < roster_row,
            "the roster docks BELOW the Context footer"
        );
        assert!(
            roster_row > footer_row + 1,
            "one breathing row between footer and roster"
        );
    }

    /// Approval state: the approval body, its keyboard hints and the Context
    /// footer stay one contiguous unit; the roster comes only after all of
    /// them — never between the approval choices and their hints.
    #[test]
    fn approval_body_hints_and_footer_stay_contiguous_above_the_roster() {
        use leveler_client_protocol::{ApprovalId, UiApprovalRequest};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = AppState::new(
            crate::theme::Theme::default(),
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
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.elapsed_secs = 55;
        state.context_tokens = 59_000;
        state.team = team_with_usage(&[("a1", "explorer", "read_file", 158_000)]);
        state.overlay = Some(crate::overlay::Overlay::Approval(Box::new(
            crate::overlay::ApprovalOverlay::new(UiApprovalRequest {
                id: ApprovalId::new("a1"),
                tool: "run_command".into(),
                summary: "git push".into(),
                command: Some("git push".into()),
                risks: vec!["network".into()],
            }),
        )));
        let mut terminal = Terminal::new(TestBackend::new(100, 34)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let lines: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        buf.cell((x, y))
                            .and_then(|c| c.symbol().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect::<String>()
            })
            .collect();
        let body_row = row_of(&lines, "git push");
        let footer_row = row_of(&lines, "Context");
        let roster_row = row_of(&lines, "● Main");
        assert!(
            body_row < footer_row,
            "approval body renders above the footer"
        );
        assert!(
            footer_row < roster_row,
            "the roster must not wedge into the approval/composer unit"
        );
        assert!(
            lines[body_row..footer_row]
                .iter()
                .all(|l| !l.contains("● Main")),
            "no roster row between the approval body and the footer"
        );
    }

    /// With no collaboration there is no roster row and the footer stays the
    /// last content strip — the move must not reserve dead space.
    #[test]
    fn no_team_leaves_the_footer_as_the_bottom_strip() {
        let lines = render_workbench(
            100,
            30,
            crate::i18n::Locale::En,
            0,
            crate::multi_agent::TaskTeamView::default(),
            true,
        );
        let footer_row = row_of(&lines, "Context");
        assert!(
            lines
                .iter()
                .skip(footer_row + 1)
                .all(|l| l.trim().is_empty()),
            "nothing renders below the footer without an active team"
        );
    }

    /// Density closure: on a wide terminal the `elapsed · tokens` column
    /// aligns to the roster's own bounded content width, NOT the terminal's
    /// right edge — and stays one aligned column across rows of very
    /// different activity lengths.
    #[test]
    fn wide_terminal_meta_is_bounded_and_aligned_not_edge_pinned() {
        let lines = render_workbench(
            180,
            30,
            crate::i18n::Locale::En,
            55,
            team_with_usage(&[
                (
                    "a1",
                    "explorer",
                    "analyzing the repository layout in depth",
                    362_000,
                ),
                ("a2", "explorer", "grep", 248_000),
                ("a3", "explorer", "shell_command", 91_000),
            ]),
            false,
        );
        let meta_cols: Vec<usize> = lines
            .iter()
            .filter(|l| l.contains("55s ·"))
            .map(|l| col_of(l, "55s ·").unwrap())
            .collect();
        assert_eq!(meta_cols.len(), 3, "all three children carry usage metas");
        assert!(
            meta_cols.iter().all(|c| *c == meta_cols[0]),
            "meta is one shared column: {meta_cols:?}"
        );
        assert!(
            meta_cols[0] < 110,
            "meta aligns to the roster content, not the 180-column right edge: {}",
            meta_cols[0]
        );
        for l in lines.iter().filter(|l| l.contains("55s ·")) {
            let col = l.find("55s ·").unwrap();
            assert!(
                l[..col].ends_with("    "),
                "at least four columns of breathing room before the meta: {l:?}"
            );
            let end = l.trim_end().len();
            assert!(end < 130, "the roster keeps a bounded visual width: {end}");
        }
    }

    /// A row whose usage is genuinely zero shows elapsed only — the column
    /// never invents a token figure to fill itself.
    #[test]
    fn zero_usage_shows_elapsed_without_a_fake_token_value() {
        let lines = render_workbench(
            120,
            30,
            crate::i18n::Locale::En,
            55,
            team_with_usage(&[("a1", "explorer", "read_file", 0)]),
            false,
        );
        let row = lines
            .iter()
            .find(|l| l.contains("read_file"))
            .expect("child row renders");
        assert!(row.contains("55s"), "elapsed still shown: {row:?}");
        assert!(
            !row.contains('·'),
            "no `· tokens` half for a zero-usage child: {row:?}"
        );
    }

    /// CJK activity labels are double-width; the shared meta column must stay
    /// aligned across a Chinese row and an ASCII row.
    #[test]
    fn cjk_activity_keeps_the_meta_column_aligned() {
        let lines = render_workbench(
            140,
            30,
            crate::i18n::Locale::Zh,
            55,
            team_with_usage(&[
                ("a1", "explorer", "已审查，无阻塞问题", 362_000),
                ("a2", "explorer", "shell_command", 248_000),
            ]),
            false,
        );
        let meta_cols: Vec<usize> = lines
            .iter()
            .filter(|l| l.contains("55s ·"))
            .map(|l| col_of(l, "55s ·").unwrap())
            .collect();
        assert_eq!(meta_cols.len(), 2);
        assert_eq!(
            meta_cols[0], meta_cols[1],
            "double-width text must not skew the column"
        );
    }

    /// Narrow terminals keep identity/activity and drop the meta entirely —
    /// unchanged by the placement move.
    #[test]
    fn narrow_terminal_still_drops_the_meta_column() {
        let lines = render_workbench(
            46,
            30,
            crate::i18n::Locale::En,
            55,
            team_with_usage(&[("a1", "explorer", "read_file", 362_000)]),
            false,
        );
        let row = lines
            .iter()
            .find(|l| l.contains("read_file"))
            .expect("child row renders");
        assert!(!row.contains("55s"), "narrow width drops the meta: {row:?}");
    }

    fn team_with(children: &[(&str, &str, &str)]) -> crate::multi_agent::TaskTeamView {
        let mut team = crate::multi_agent::TaskTeamView::default();
        for (id, role, purpose) in children {
            team.apply_update(crate::multi_agent::ChildUpdate {
                id: (*id).into(),
                nickname: "Newton".into(),
                role: (*role).into(),
                done: false,
                ok: false,
                detail: (*purpose).into(),
                profile_id: Some((*role).into()),
                capabilities: vec!["read_file".into()],
                contribution: None,
                started_elapsed_secs: 0,
            });
        }
        team
    }

    #[test]
    fn a_team_panel_explains_why_the_user_is_waiting() {
        let screen = render_with_team(team_with(&[
            ("a1", "explorer", "analyzing repository structure"),
            ("w1", "worker", "implementing the change"),
        ]));
        assert!(
            screen.contains("analyzing repository structure"),
            "{screen}"
        );
        assert!(screen.contains("implementing the change"), "{screen}");
    }

    #[test]
    fn no_team_no_panel_and_no_stolen_rows() {
        let empty = render_with_team(crate::multi_agent::TaskTeamView::default());
        let staffed = render_with_team(team_with(&[
            ("a1", "explorer", "look"),
            ("w1", "worker", "implement"),
        ]));
        // Roster shape: no standing title — presence is the Main row itself.
        assert!(!empty.contains("● Main"), "{empty}");
        assert!(staffed.contains("● Main"), "{staffed}");
        assert!(staffed.contains("Explorer"), "{staffed}");
    }

    /// The layout indices shift when a panel is inserted; a footer that moved
    /// would be an off-by-one nobody notices until it ships.
    #[test]
    fn inserting_the_team_panel_does_not_displace_the_rest_of_the_chrome() {
        let empty = render_with_team(crate::multi_agent::TaskTeamView::default());
        let lines: Vec<&str> = empty.lines().collect();
        assert!(
            lines.len() >= 30,
            "the workbench must still fill the terminal: {} rows",
            lines.len()
        );
        // Composer and footer chrome still render with no team present.
        assert!(
            empty.trim().len() > 50,
            "an empty team must not blank the workbench: {empty}"
        );
    }

    #[test]
    fn a_blocking_finding_is_named_in_the_panel_title() {
        let mut team = team_with(&[("w1", "worker", "implement"), ("r1", "reviewer", "review")]);
        let mut c = leveler_client_protocol::ChildContribution {
            role: "reviewer".into(),
            profile_id: Some("reviewer".into()),
            profile_role: Some("reviewer".into()),
            capabilities: vec!["code_review".into()],
            source: Some("independent_reviewer".into()),
            findings_total: 2,
            findings_acknowledged: 2,
            findings_accepted: 1,
            findings_verified: 0,
            findings_rejected: 0,
            findings_open_blocking: 1,
        };
        c.findings_open_blocking = 1;
        team.apply_update(crate::multi_agent::ChildUpdate {
            id: "r1".into(),
            nickname: "reviewer".into(),
            role: "reviewer".into(),
            done: true,
            ok: true,
            detail: "done".into(),
            profile_id: None,
            capabilities: Vec::new(),
            contribution: Some(c),
            started_elapsed_secs: 0,
        });
        let screen = render_with_team(team);
        assert!(screen.contains("1 blocking"), "{screen}");
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
        assert!(
            title.contains("第 2/3 项进行中"),
            "running item, not a zero-progress fraction: {title}"
        );
        assert!(
            !title.contains("edit module"),
            "the numbered list under the header already carries the description: {title}"
        );
        assert!(
            !title.contains("2."),
            "the numbered list under the header already carries the index: {title}"
        );
    }

    fn active_team() -> crate::multi_agent::TaskTeamView {
        let mut team = crate::multi_agent::TaskTeamView::default();
        for (i, st) in [
            crate::multi_agent::ChildStatus::Running,
            crate::multi_agent::ChildStatus::Waiting,
        ]
        .iter()
        .enumerate()
        {
            team.children.push(crate::multi_agent::ChildAgentView {
                id: format!("c{i}"),
                nickname: String::new(),
                role: if i == 0 {
                    "探索 Agent".into()
                } else {
                    "审查 Agent".into()
                },
                profile_id: None,
                capabilities: Vec::new(),
                purpose: "check deps".into(),
                status: *st,
                contribution: crate::multi_agent::Contribution::Pending,
                recent_step: None,
                input_tokens: 0,
                output_tokens: 0,
                started_elapsed_secs: 0,
                detail: None,
                steps: Vec::new(),
            });
        }
        team
    }

    fn panel_rows(state: &AppState, height: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(70, height)).unwrap();
        terminal
            .draw(|f| {
                render_team_panel(
                    f,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 70,
                        height,
                    },
                    state,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..70)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .collect()
    }

    /// Wide glyphs leave pad cells in the TestBackend buffer ("已 完 成"), so
    /// substring assertions compare with spaces stripped.
    fn squash(row: &str) -> String {
        row.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// The collaboration surface shares the primary task baseline with the
    /// plan dock, and its member rows sit one level inside the header —
    /// the real screenshot had every row at column 0.
    #[test]
    fn collaboration_panel_shares_the_task_baseline_with_indented_members() {
        let mut state = test_state();
        state.team = active_team();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let rows = panel_rows(&state, 4);
        // Roster shape: every row (Main + children) sits on the shared task
        // baseline — a process list, not a header with indented members.
        let main = rows
            .iter()
            .find(|r| r.contains('●'))
            .unwrap_or_else(|| panic!("main row rendered:\n{}", rows.join("\n")));
        let main_col = main.len() - main.trim_start().len();
        assert_eq!(
            main_col,
            crate::layout::WORKSPACE_GUTTER_X as usize,
            "main row on the content baseline: {main:?}"
        );
        let member = rows
            .iter()
            .find(|r| r.contains('○'))
            .unwrap_or_else(|| panic!("member row rendered:\n{}", rows.join("\n")));
        let member_col = member.len() - member.trim_start().len();
        assert_eq!(
            member_col, main_col,
            "children share the same baseline: {main:?} / {member:?}"
        );
    }

    /// Matrix D/M: Main leads with its structured activity label; an active
    /// child shows activity plus right-aligned elapsed · usage.
    #[test]
    fn roster_shows_main_activity_and_child_meta() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.activity = Some("正在汇总审计结果".into());
        state.elapsed_secs = 249;
        let mut team = crate::multi_agent::TaskTeamView::default();
        team.apply_update(crate::multi_agent::ChildUpdate {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            done: false,
            ok: false,
            detail: "审计生命周期与身份".into(),
            profile_id: None,
            capabilities: Vec::new(),
            contribution: None,
            started_elapsed_secs: 0,
        });
        team.apply_progress("c1", true, 120_000, 48_000);
        state.team = team;
        let rows = panel_rows(&state, 3).join("\n");
        // Cell-by-cell extraction pads wide glyphs with spaces; compare
        // space-stripped for CJK anchors.
        let squashed = rows.replace(' ', "");
        assert!(rows.contains('●'), "{rows}");
        assert!(squashed.contains("正在汇总审计结果"), "{rows}");
        assert!(squashed.contains("审计生命周期与身份"), "{rows}");
        assert!(squashed.contains("4m09s"), "elapsed: {rows}");
        assert!(rows.contains("168k"), "usage: {rows}");
    }

    /// Matrix L: no usage reported yet → elapsed only, never a fake 0.
    #[test]
    fn roster_omits_usage_until_it_exists() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.elapsed_secs = 61;
        state.team = team_with(&[("c1", "explorer", "look around")]);
        let rows = panel_rows(&state, 3).join("\n");
        assert!(rows.contains("1m 01s"), "{rows}");
        assert!(!rows.contains("· 0"), "no fake usage figure: {rows}");
    }

    /// Matrix F/E (§15/§16): an incomplete child renders truthfully with !,
    /// never as ✓; completed siblings keep ✓ while Main keeps working.
    #[test]
    fn roster_failure_truth_and_integrating_main() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.activity = Some("正在整合结果".into());
        let mut team = team_with(&[("ok1", "explorer", "audit a"), ("bad1", "worker", "fix b")]);
        team.apply_update(crate::multi_agent::ChildUpdate {
            id: "ok1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            done: true,
            ok: true,
            detail: "done".into(),
            profile_id: None,
            capabilities: Vec::new(),
            contribution: None,
            started_elapsed_secs: 4,
        });
        team.apply_update(crate::multi_agent::ChildUpdate {
            id: "bad1".into(),
            nickname: "Newton".into(),
            role: "worker".into(),
            done: true,
            ok: false,
            detail: "died".into(),
            profile_id: None,
            capabilities: Vec::new(),
            contribution: None,
            started_elapsed_secs: 5,
        });
        // A third child still active keeps the surface in full roster shape
        // (frozen transience: all-settled collapses to the terminal line).
        team.apply_update(crate::multi_agent::ChildUpdate {
            id: "live1".into(),
            nickname: "Kepler".into(),
            role: "explorer".into(),
            done: false,
            ok: false,
            detail: "audit c".into(),
            profile_id: None,
            capabilities: Vec::new(),
            contribution: None,
            started_elapsed_secs: 6,
        });
        state.team = team;
        let rows = panel_rows(&state, 5).join("\n");
        let squashed = rows.replace(' ', "");
        assert!(rows.contains('✓'), "{rows}");
        assert!(rows.contains('!'), "{rows}");
        assert!(rows.contains('○'), "{rows}");
        assert!(
            !squashed.contains("✓执行"),
            "an incomplete worker must never wear the success glyph: {rows}"
        );
        assert!(squashed.contains("工作未完成"), "{rows}");
        assert!(squashed.contains("正在整合结果"), "{rows}");
    }

    /// §14: when EVERY child settles, the surface collapses to one truthful
    /// terminal line — an incomplete child keeps the count honest and the
    /// line never reads as pure success.
    #[test]
    fn roster_all_settled_collapses_to_truthful_terminal_line() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let mut team = crate::multi_agent::TaskTeamView::default();
        for (id, ok) in [("ok1", true), ("bad1", false)] {
            team.apply_update(crate::multi_agent::ChildUpdate {
                id: id.into(),
                nickname: "Euclid".into(),
                role: "explorer".into(),
                done: true,
                ok,
                detail: "done".into(),
                profile_id: None,
                capabilities: Vec::new(),
                contribution: None,
                started_elapsed_secs: 3,
            });
        }
        state.team = team;
        let rows = panel_rows(&state, 4).join("\n");
        let squashed = rows.replace(' ', "");
        assert!(squashed.contains("未完成"), "truthful terminal: {rows}");
        assert!(
            !rows.contains('●'),
            "the full roster yields to the terminal line: {rows}"
        );
    }

    /// Matrix J: narrow terminals drop the meta column, keep identity+activity.
    #[test]
    fn roster_narrow_width_drops_meta_keeps_activity() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.elapsed_secs = 100;
        state.team = team_with(&[("c1", "explorer", "audit the lifecycle paths")]);
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(40, 3)).unwrap();
        terminal
            .draw(|f| {
                render_team_panel(
                    f,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 40,
                        height: 3,
                    },
                    &state,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let rows: String = (0..3)
            .map(|y| {
                (0..40)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rows.contains('○'), "{rows}");
        assert!(!rows.contains("1m 40s"), "meta dropped when narrow: {rows}");
    }

    /// Matrix §18: more children than rows → an overflow note, never a
    /// viewport-eating roster.
    #[test]
    fn roster_overflow_names_the_hidden_agents() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.team = team_with(&[
            ("c1", "explorer", "a"),
            ("c2", "explorer", "b"),
            ("c3", "worker", "c"),
            ("c4", "reviewer", "d"),
            ("c5", "explorer", "e"),
            ("c6", "explorer", "f"),
        ]);
        let height = team_panel_height(&state);
        assert!(height <= 7, "capped: {height}");
        let rows = panel_rows(&state, height).join("\n");
        assert!(
            rows.replace(' ', "").contains("还有"),
            "overflow note: {rows}"
        );
    }

    /// Once every child settles the surface shows one brief terminal row and
    /// then leaves entirely; a failed child is never worded as completed.
    #[test]
    fn settled_collaboration_shows_one_terminal_row_then_leaves() {
        let mut state = test_state();
        state.team = active_team();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.elapsed_secs = 100;
        assert!(team_panel_height(&state) > 1, "active team is expanded");

        for c in &mut state.team.children {
            c.status = crate::multi_agent::ChildStatus::Completed;
        }
        state.team.settled_at_elapsed = Some(100);
        assert_eq!(team_panel_height(&state), 1, "terminal = one row");
        let rows = panel_rows(&state, 1);
        assert!(squash(&rows[0]).contains("已完成"), "{rows:?}");
        assert!(rows[0].contains('✓'), "{rows:?}");

        state.elapsed_secs = 100 + crate::multi_agent::COLLABORATION_TERMINAL_SECS;
        assert_eq!(
            team_panel_height(&state),
            0,
            "after the linger window the surface is gone — history lives in Task Detail"
        );

        // Turn over → 任务已完成 is the single completion owner; the terminal
        // row yields at once even inside the linger window (the turn clock
        // has stopped, so the window could otherwise never expire).
        state.elapsed_secs = 101;
        state.status = leveler_client_protocol::RuntimeStatus::Idle;
        assert_eq!(
            team_panel_height(&state),
            0,
            "an idle turn never keeps a settled collaboration row"
        );
        state.status = leveler_client_protocol::RuntimeStatus::Busy;

        // Failure wording stays truthful in the terminal row.
        state.elapsed_secs = 101;
        state.team.children[1].status = crate::multi_agent::ChildStatus::Failed;
        let rows = panel_rows(&state, 1);
        assert!(!squash(&rows[0]).contains("已完成"), "{rows:?}");
        assert!(squash(&rows[0]).contains("未完成"), "{rows:?}");
    }

    /// The plan is a sibling of the conversation, not a footnote to the
    /// status line: its header starts at the content baseline and its steps
    /// sit one level inside it. Real frames showed both at column 0, which
    /// gave the dock no hierarchy at all.
    #[test]
    fn plan_dock_header_and_steps_form_one_indent_level() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut state = test_state();
        state.plan = Some(sample_plan());
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        terminal
            .draw(|f| {
                let area = Rect {
                    x: 0,
                    y: 0,
                    width: 60,
                    height: 6,
                };
                render_plan_panel(f, area, &state);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..6)
            .map(|y| {
                (0..60)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .collect();
        let header = &rows[0];
        let step = rows
            .iter()
            .find(|r| r.trim_start().starts_with('→') || r.trim_start().starts_with('○'))
            .unwrap_or_else(|| panic!("no plan step rendered:\n{}", rows.join("\n")));
        let header_col = header.len() - header.trim_start().len();
        let step_col = step.len() - step.trim_start().len();
        assert_eq!(
            header_col,
            crate::layout::WORKSPACE_GUTTER_X as usize,
            "header sits on the content baseline: {header:?}"
        );
        assert!(
            step_col > header_col,
            "steps indent one level inside the header: {header:?} / {step:?}"
        );
    }

    #[test]
    fn plan_chrome_title_keeps_progress_when_collapsed() {
        let t = crate::i18n::Locale::Zh.text();
        let title = plan_chrome_title(&sample_plan(), true, t);
        assert!(title.starts_with('▶'), "{title}");
        assert!(title.contains("第 2/3 项进行中"), "{title}");
    }

    #[test]
    fn plan_chrome_does_not_fabricate_an_active_item() {
        let t = crate::i18n::Locale::Zh.text();
        let plan = UiPlan {
            steps: vec![
                UiPlanStep {
                    index: 0,
                    description: "侦察仓库结构".into(),
                    status: PlanStepStatus::Pending,
                },
                UiPlanStep {
                    index: 1,
                    description: "验证构建".into(),
                    status: PlanStepStatus::Pending,
                },
                UiPlanStep {
                    index: 2,
                    description: "深挖发布流程".into(),
                    status: PlanStepStatus::Pending,
                },
                UiPlanStep {
                    index: 3,
                    description: "综合证据".into(),
                    status: PlanStepStatus::Pending,
                },
            ],
        };
        let title = plan_chrome_title(&plan, true, t);
        assert!(title.contains("0/4 完成"), "{title}");
        assert!(
            !title.contains("进行中"),
            "pending is not in-progress: {title}"
        );
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
        assert!(title.contains("1/2 done"), "{title}");
        assert!(
            !title.contains("in progress"),
            "a pending step is not claimed as active: {title}"
        );
        assert!(
            !title.contains("next work"),
            "do not promote the next pending description to the title: {title}"
        );
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
