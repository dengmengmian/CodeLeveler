//! Workbench layout: fixed Header / Plan / Input / Footer + scrollable Conversation.
//!
//! Layout (top → bottom):
//! Header · Conversation (scroll) · gap · Notice? · Status? · Plan · gap? · Input ·
//! Hint+usage+clock footer
//!
//! `Notice` is the global Notice Surface: user-action feedback (busy refusal,
//! copied, model switched). It owns a real layout row — never a floating toast.
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
    COMPOSER_MAX_ROWS, composer_box_lines, composer_visible_rows, render_attachments,
    render_slash_popup,
};
use crate::screen::Screen;
use crate::state::AppState;
use crate::status_line::status_lines;

/// The conversation never shrinks below this, whatever the plan dock wants.
const MIN_CONVERSATION_ROWS: u16 = 3;

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
        .filter(|s| is_plan_step_settled(s.status))
        .count();
    (k, n)
}

fn is_plan_step_settled(status: PlanStepStatus) -> bool {
    matches!(status, PlanStepStatus::Done | PlanStepStatus::Skipped)
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
    let all_success = plan.steps.iter().all(|s| is_plan_step_settled(s.status));
    !all_success
}

/// Summary after `计划`/`plan`. The plan is the agent's declared progress, so
/// the summary says how much it declared done and, when available, the current
/// running or failed step. It never invents a "next" step when none is active.
pub(crate) fn plan_summary_label(plan: &UiPlan, live: bool, t: &UiText) -> String {
    let (done, total) = plan_done_total(plan);
    if total == 0 {
        return t.active_plan.to_string();
    }
    let fill = |template: &str| {
        template
            .replace("{done}", &done.to_string())
            .replace("{total}", &total.to_string())
    };
    if !live {
        return fill(t.plan_last_recorded);
    }
    let progress = fill(t.plan_n_done);
    if let Some(step) = plan
        .steps
        .iter()
        .find(|s| s.status == PlanStepStatus::Failed)
    {
        return format!(
            "{progress} · {}",
            t.plan_failed_item
                .replace("{step}", &(step.index + 1).to_string())
                .replace("{description}", &step.description)
        );
    }
    if let Some(step) = plan
        .steps
        .iter()
        .find(|s| s.status == PlanStepStatus::Running)
    {
        return format!(
            "{progress} · {}",
            t.plan_running_item.replace("{step}", &step.description)
        );
    }
    let pending = plan
        .steps
        .iter()
        .filter(|s| s.status == PlanStepStatus::Pending)
        .count();
    if pending > 0 {
        format!(
            "{progress} · {}",
            t.plan_pending_count.replace("{n}", &pending.to_string())
        )
    } else {
        progress
    }
}

/// One-line plan summary. The arrow opens the full Plan page; there is no
/// second inline expansion mode.
pub(crate) fn plan_chrome_title(plan: &UiPlan, live: bool, t: &UiText) -> String {
    let glyph = if plan
        .steps
        .iter()
        .any(|step| step.status == PlanStepStatus::Failed)
    {
        "!"
    } else if live
        && plan
            .steps
            .iter()
            .any(|step| step.status == PlanStepStatus::Running)
    {
        "●"
    } else {
        "○"
    };
    format!(
        "{glyph} {} · {} ↗",
        t.active_plan,
        plan_summary_label(plan, live, t)
    )
}

/// Paint the conversation workbench into `frame`.
pub fn render_workbench(frame: &mut Frame, state: &mut AppState) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    state.theme.paint_canvas(frame, area);

    let attach_rows: u16 =
        if state.pending_attachments.is_empty() && state.composer.file_reference_count() == 0 {
            0
        } else {
            1
        };
    let team_rows = team_panel_height(state);
    // 待发送 sits directly above the composer: input the user wrote and has
    // not sent is part of what they can do now, not of the conversation.
    let pending_rows = crate::pending_inputs::panel_height(state, area.height);
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
    // Global Notice Surface: transient feedback for a user action. It claims a
    // real layout row so it can never float over content or drift to the
    // Conversation's bottom-right; with no notice it costs zero rows and the
    // transcript keeps its full height.
    let notice_rows: u16 = u16::from(state.notification.is_some());
    // Runtime status (spinner / elapsed / tokens) is persistent execution
    // state — a separate strip, never merged with the transient notice.
    // Every strip of bottom chrome sits on the composer's grid: the same
    // workspace gutter, applied through `chrome_slot` below. The status text
    // is laid out for that inner width, not the terminal's.
    let status_block = status_lines(state, workspace_inner.width as usize);
    let status_rows: u16 = if status_block
        .iter()
        .any(|line| line.spans.iter().any(|span| !span.content.is_empty()))
    {
        status_block
            .len()
            .min(1 + crate::activity::MAX_STATUS_ROWS * 2) as u16
    } else {
        0
    };
    // Breathing room around the input box: blank above only when live chrome
    // (status / plan / attachments) sits on top of it; blank below always so
    // Context footer is not flush on the composer border.
    // The plan dock's own height is not known yet (it depends on this gap via
    // the row budget), but its VISIBILITY is — and that is all the gap needs.
    let plan_visible = state.plan.as_ref().is_some_and(plan_panel_should_show);
    let chrome_above = status_rows
        .saturating_add(u16::from(plan_visible))
        .saturating_add(attach_rows)
        .saturating_add(pending_rows);
    let pre_composer_gap: u16 = if chrome_above > 0 { 1 } else { 0 };

    // One breathing row between the Context footer and the roster, so the
    // process list reads as its own surface rather than a second footer.
    let team_gap: u16 = if team_rows > 0 { 1 } else { 0 };

    // Everything the plan dock does NOT get: the fixed chrome plus the
    // conversation's own floor. What is left is the dock's row budget, so a
    // long plan grows into real space instead of being cut at a constant.
    let reserved = header_rows
        .saturating_add(MIN_CONVERSATION_ROWS)
        .saturating_add(gap_rows)
        .saturating_add(notice_rows)
        .saturating_add(status_rows)
        .saturating_add(attach_rows)
        .saturating_add(pending_rows)
        .saturating_add(pre_composer_gap)
        .saturating_add(composer_rows)
        .saturating_add(footer_rows)
        .saturating_add(team_gap)
        .saturating_add(team_rows)
        .saturating_add(footer_bottom);
    let plan_rows = plan_panel_height(state, area.height.saturating_sub(reserved));

    let chunks = Layout::vertical([
        Constraint::Length(header_rows),
        Constraint::Min(MIN_CONVERSATION_ROWS), // conversation viewport
        Constraint::Length(gap_rows),
        Constraint::Length(notice_rows), // Notice Surface (0 rows when idle)
        Constraint::Length(status_rows),
        Constraint::Length(plan_rows),
        Constraint::Length(attach_rows),
        Constraint::Length(pending_rows),
        Constraint::Length(pre_composer_gap),
        Constraint::Length(composer_rows),
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

    // The one horizontal rule for the bottom chrome: notice, status,
    // attachments, pending inputs, composer and footer all start on the
    // composer's left edge. (The plan dock and the roster apply the same
    // gutter inside their own renderers.)
    let chrome_slot =
        |slot: Rect| crate::layout::horizontal_inset(slot, crate::layout::WORKSPACE_GUTTER_X);
    let notice_slot = chrome_slot(chunks[3]);
    let status_slot = chrome_slot(chunks[4]);
    let attach_slot = chrome_slot(chunks[6]);
    let pending_slot = chrome_slot(chunks[7]);
    let input_slot = chrome_slot(chunks[9]);
    let footer_slot = chrome_slot(chunks[10]);

    render_header(frame, chunks[0], state);
    crate::conversation::viewport::render(frame, chunks[1], state);
    {
        let input_bg = state.theme.surface.input;
        state.theme.paint_surface(frame, input_slot, input_bg);
    }
    // chunks[2] = gap (leave blank)
    render_notice(frame, notice_slot, state);
    if status_rows > 0 {
        frame.render_widget(Paragraph::new(status_block.clone()), status_slot);
    }
    let plan_area = crate::layout::horizontal_inset(chunks[5], crate::layout::WORKSPACE_GUTTER_X);
    state.plan_hit = (plan_rows > 0 && plan_area.width > 0).then_some((
        plan_area.y,
        plan_area.x,
        plan_area.x.saturating_add(plan_area.width),
    ));
    render_plan_panel(frame, chunks[5], state);
    render_attachments(frame, attach_slot, state);
    crate::pending_inputs::render(frame, pending_slot, state, area.height);
    // chunks[8] = pre_composer_gap (leave blank)
    match &state.overlay {
        Some(overlay) => {
            crate::overlay::render_overlay(frame, input_slot, overlay, &state.theme, state.locale)
        }
        None => render_input(frame, input_slot, state),
    }
    render_footer(frame, footer_slot, state);
    // chunks[11] = breathing row; the roster docks under the footer.
    state.activity_hits = render_team_panel(frame, chunks[12], state);

    // /btw is its own surface (see `crate::btw`), not an overlay here.

    if state.active_screen == Screen::Conversation && state.overlay.is_none() {
        render_slash_popup(frame, chunks[1], input_slot, state);
    }
}

// ── Header (single-line environment strip + rule — no model / tokens) ───────

fn render_header(frame: &mut Frame, area: Rect, state: &mut AppState) {
    // Leading blank row keeps the brand strip off the terminal's top edge.
    let [_gap, status, rule_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    // One-column horizontal inset keeps both the brand and the right-hand
    // goal affordance off the terminal edge.
    let text_area = Rect {
        x: status.x + 1,
        width: status.width.saturating_sub(2),
        ..status
    };
    let (line, goal_range) = header_line_parts(state, text_area.width as usize);
    state.goal_hit = goal_range.map(|(start, end)| {
        (
            status.y,
            text_area.x.saturating_add(start as u16),
            text_area.x.saturating_add(end as u16),
        )
    });
    frame.render_widget(Paragraph::new(line), text_area);
    frame.render_widget(
        Paragraph::new(header_rule_line(area.width as usize, state)),
        rule_area,
    );
}

/// The header row: the identity strip on the left, the Active Goal indicator
/// flushed right.
///
/// The goal never starves the identity: it is offered only the columns left
/// after `CodeLeveler`, up to a fixed ceiling, and as the row narrows it drops
/// its title first and then itself. The identity strip is the row's floor.
fn header_line_parts(state: &AppState, width: usize) -> (Line<'static>, Option<(usize, usize)>) {
    if width == 0 {
        return (Line::from(""), None);
    }
    // "CodeLeveler" plus a gap is the minimum the identity strip keeps.
    const MIN_IDENTITY: usize = 10;
    const GAP: usize = 2;
    // The goal is useful context, but it must not turn the header into a copy
    // of the prompt or erase the repository identity on wide terminals.
    const MAX_GOAL_COLS: usize = 48;
    let goal_budget = width.saturating_sub(MIN_IDENTITY + GAP).min(MAX_GOAL_COLS);
    let mut goal = crate::active_goal::header(state, goal_budget);
    if goal.is_empty() {
        return (header_status_line(state, width), None);
    }
    if state.workbench_focus == crate::state::WorkbenchFocus::Goal {
        for span in &mut goal {
            span.style = span.style.add_modifier(Modifier::REVERSED);
        }
    }
    let goal_w: usize = goal
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let left_room = width.saturating_sub(goal_w + 1);
    let left = header_status_line(state, left_room);
    let left_w: usize = left
        .spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let gap = width.saturating_sub(left_w + goal_w).max(1);
    let mut spans = left.spans;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(goal);
    let start = width.saturating_sub(goal_w);
    (Line::from(spans), Some((start, width)))
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
/// `budget` is the rows the layout can actually spare for the dock (after the
/// header, composer, footer and the conversation's own minimum). The body is
/// the summary window: finished steps drop out and at most five unfinished
/// steps remain, so a long plan no longer claims a row per completed step.
fn plan_panel_height(state: &AppState, budget: u16) -> u16 {
    match &state.plan {
        Some(p) if plan_panel_should_show(p) && budget > 0 => 1,
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
    // collaboration surface yields immediately.
    if !state.is_busy() && state.team.active().next().is_none() {
        return 0;
    }
    if state.collaboration_collapsed || state.team.surface_is_terminal(state.elapsed_secs) {
        return 1;
    }
    // Roster shape: aggregate row + up to 4 child rows + an overflow row. Capped
    // so the runtime surface stays a caption on the task, never most of the
    // viewport.
    let children = state.team.children.len();
    let shown = children.min(4);
    let overflow = usize::from(children > shown);
    (1 + shown + overflow).min(7) as u16
}

fn render_team_panel(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
) -> Vec<(u16, crate::activity::ActivityId)> {
    if area.height == 0 {
        return Vec::new();
    }
    let theme = &state.theme;
    let t = state.t();
    // Same primary task baseline as the plan dock and the conversation
    // content: the collaboration row is task-runtime state, a sibling of the
    // plan — not chrome hanging off the terminal's left edge.
    let area = crate::layout::horizontal_inset(area, crate::layout::WORKSPACE_GUTTER_X);
    if area.width == 0 {
        return Vec::new();
    }
    const MEMBER_INDENT: &str = "  ";
    let terminal = state.team.surface_is_terminal(state.elapsed_secs);
    if area.height == 1 || terminal {
        // Compact / terminal: one truthful row, nothing else. The glyph is
        // part of that truth, so it comes from the same place the row's words
        // do — a settled team that lost a child says so in both.
        let glyph = crate::multi_agent::collaboration_glyph(&state.team, terminal);
        let row = format!(
            "{glyph} {}",
            crate::multi_agent::collaboration_compact_line(&state.team, t)
        );
        let bad = state
            .team
            .children
            .iter()
            .any(|c| c.status == crate::multi_agent::ChildStatus::Failed);
        let color = if bad {
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
        return Vec::new();
    }
    // One aggregate summary, then one clickable line per child. The summary
    // replaces the old Main row; the conversation keeps the durable record.
    let summary =
        crate::multi_agent::collaboration_runtime_line(&state.team, state.elapsed_secs, t);
    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled("● ", Style::default().fg(theme.accent.primary)),
        Span::styled(
            summary,
            Style::default()
                .fg(theme.text.primary)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    let rows = crate::multi_agent::roster_rows(&state.team, state.elapsed_secs, t);
    let budget = (area.height as usize).saturating_sub(lines.len());
    let child_rows = rows.len();
    let avail = budget;
    let shown_children = if child_rows <= avail {
        child_rows
    } else {
        avail.saturating_sub(1)
    };
    let width = area.width as usize;
    // Narrow terminals truncate the task before dropping elapsed time or the
    // detail affordance. The roster is a compact process list, not a full-width
    // table: on wide terminals the elapsed column aligns to the roster's own
    // content inside a bounded readable width — never to the terminal's
    // right edge, which turns each row into two disconnected islands.
    const ROSTER_MAX_WIDTH: usize = 96;
    const META_GAP: usize = 4;
    let bound = width.min(ROSTER_MAX_WIDTH);
    let visible: Vec<&crate::multi_agent::AgentRosterRow> =
        rows.iter().take(shown_children).collect();
    // Shared column width: the widest meta among the rows that carry one.
    let meta_col = visible
        .iter()
        .filter_map(|row| row.meta.as_deref())
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0);
    // Every row's activity respects the shared meta reservation so the
    // column stays vertically aligned; the column position itself is
    // content-driven (widest head+activity plus breathing room).
    let mut prepared: Vec<(String, usize, String, usize)> = Vec::new();
    let mut content_w = 0usize;
    for (index, row) in visible.iter().enumerate() {
        let branch = if index + 1 == shown_children && shown_children == child_rows {
            "└─"
        } else {
            "├─"
        };
        let head = format!("  {branch} {} · ", row.label);
        let head_w = UnicodeWidthStr::width(head.as_str());
        let cap = if meta_col > 0 {
            bound
                .saturating_sub(meta_col + META_GAP + UnicodeWidthStr::width(" ·  ↗") + head_w)
                .max(4)
        } else {
            width.saturating_sub(head_w).max(4)
        };
        let activity = truncate(row.activity.clone(), cap);
        let activity_w = UnicodeWidthStr::width(activity.as_str());
        content_w = content_w.max(head_w + activity_w);
        prepared.push((head, head_w, activity, activity_w));
    }
    let meta_x = content_w + META_GAP;
    let mut hits = Vec::new();
    for (index, (row, (head, head_w, activity, activity_w))) in
        visible.iter().zip(prepared).enumerate()
    {
        let color = match row.tone {
            crate::multi_agent::RosterTone::Active => theme.accent.primary,
            crate::multi_agent::RosterTone::Done => theme.status.success,
            crate::multi_agent::RosterTone::Failed => theme.status.error,
        };
        let mut spans = vec![
            Span::styled(head, Style::default().fg(color)),
            Span::styled(activity, Style::default().fg(theme.text.secondary)),
        ];
        let meta = row.meta.as_deref().unwrap_or("");
        let pad = meta_x.saturating_sub(head_w + activity_w);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(
            format!(" · {meta} ↗"),
            Style::default().fg(theme.text.secondary),
        ));
        lines.push(Line::from(spans));
        if let Some(child) = state.team.children.get(index) {
            hits.push((
                area.y + 1 + index as u16,
                crate::activity::ActivityId::Child(child.id.clone()),
            ));
        }
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
    hits
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
    // Only a running turn can have a step under way; outside one, the step the
    // plan declared in progress is part of the last record, not highlighted.
    let live = state.is_busy();
    // The plan is task-level state, a sibling of the conversation — so it
    // starts at the same content baseline instead of hanging off the left
    // edge, and its steps sit one level inside their own header.
    let area = crate::layout::horizontal_inset(area, crate::layout::WORKSPACE_GUTTER_X);
    if area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let raw = plan_chrome_title(plan, live, t);
    let title = if width > 2 {
        let without_arrow = raw.strip_suffix(" ↗").unwrap_or(&raw);
        format!("{} ↗", truncate(without_arrow, width - 2))
    } else {
        truncate(raw, width)
    };
    let style = if state.workbench_focus == crate::state::WorkbenchFocus::Plan {
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD)
    };
    frame.render_widget(Paragraph::new(Line::from(Span::styled(title, style))), area);
}

// ── Notice Surface (global user-action feedback) ────────────────────────────

/// One fixed, left-aligned row for the current notice.
///
/// The row is already reserved by the caller's layout (`notice_rows`), so this
/// only paints. A too-narrow terminal truncates instead of wrapping: the strip
/// never grows and never shifts the transcript underneath it.
fn render_notice(frame: &mut Frame, area: Rect, state: &AppState) {
    let Some(note) = state.notification.as_ref() else {
        return;
    };
    if area.height == 0 || area.width < 4 {
        return;
    }
    let theme = &state.theme;
    let (icon, color) = match note.level {
        leveler_client_protocol::NotificationLevel::Info => ("ℹ", theme.accent.primary),
        leveler_client_protocol::NotificationLevel::Warning => ("⚠", theme.status.warning),
        leveler_client_protocol::NotificationLevel::Error => ("✕", theme.status.error),
    };
    let text = truncate(format!("{icon} {}", note.message), area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, Style::default().fg(color)))),
        area,
    );
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

/// The single bottom status row: key hints (left), usage chips and the wall
/// clock (right).
///
/// The clock is the row's highest-priority cell — the persistent anchor — so it
/// is placed first at the far right and never wraps to a line of its own. Usage
/// chips yield to it, and hints (lowest) simply truncate into whatever the
/// right-hand block leaves. The row is always one physical row, so hints
/// appearing or disappearing never reflow the transcript.
fn render_footer(frame: &mut Frame, area: Rect, state: &mut AppState) {
    if area.height == 0 || area.width == 0 {
        state.background_footer_hit = None;
        return;
    }
    let width = area.width as usize;
    let dim = Style::default().fg(state.theme.text.secondary);
    // Breathing room between the hints, the background summary, the usage chips
    // and the clock.
    const GAP: usize = 2;

    let clock = state.clock_label.clone();
    let clock_w = UnicodeWidthStr::width(clock.as_str());
    let usage = crate::status_line::footer_usage_line(state);
    let usage_w = usage.as_deref().map(UnicodeWidthStr::width).unwrap_or(0);

    // The background summary is placed before the usage/clock chips. It is the
    // only place background work is visible on the main screen, so a long task
    // label is clipped to keep the right-side chips on screen rather than
    // pushing them off. An unread failure outranks those chips entirely: a
    // failure the user cannot see is the one thing the footer must not hide.
    let has_failure = crate::activity::footer_summary(state).is_some_and(|s| s.unread_failed > 0);
    let right_reserve = if has_failure {
        0
    } else {
        (if clock_w > 0 { clock_w + GAP } else { 0 })
            + (if usage_w > 0 { usage_w + GAP } else { 0 })
    };
    let bg_cap = width
        .saturating_sub(right_reserve)
        .saturating_sub(if right_reserve > 0 { GAP } else { 0 })
        .max(1);
    let bg_spans = crate::render::background_footer_spans(state, bg_cap);
    let bg_w = bg_spans
        .as_ref()
        .map(|spans| {
            spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum::<usize>()
        })
        .unwrap_or(0);

    // Place from the right: clock, then usage, then the summary. An unread
    // failure owns the row over those chips: the badge must never be the thing
    // squeezed out.
    let reserve_chips = !has_failure;
    let mut cursor = width;
    let clock_start = if reserve_chips && clock_w > 0 && clock_w <= cursor {
        cursor -= clock_w;
        Some(cursor)
    } else {
        None
    };
    let usage_start = if reserve_chips && usage_w > 0 && cursor >= GAP + usage_w {
        cursor -= GAP + usage_w;
        Some(cursor)
    } else {
        None
    };
    let sep = if clock_start.is_some() || usage_start.is_some() {
        GAP
    } else {
        0
    };
    let bg_start = if bg_w > 0 && cursor >= sep + bg_w {
        cursor -= sep + bg_w;
        Some(cursor)
    } else {
        None
    };
    let any_right = clock_start.is_some() || usage_start.is_some() || bg_start.is_some();
    let left_limit = if any_right {
        cursor.saturating_sub(GAP)
    } else {
        width
    };

    // Record the painted summary for click-to-open; the mouse handler reads it.
    state.background_footer_hit =
        bg_start.map(|start| (area.y, start as u16, (start + bg_w) as u16));

    if left_limit > 0
        && let Some(line) = crate::render::key_hint_line(state, left_limit)
            .into_iter()
            .next()
    {
        frame.render_widget(
            Paragraph::new(line),
            Rect {
                x: area.x,
                y: area.y,
                width: left_limit as u16,
                height: 1,
            },
        );
    }
    if let (Some(start), Some(spans)) = (bg_start, bg_spans) {
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                x: area.x + start as u16,
                y: area.y,
                width: bg_w as u16,
                height: 1,
            },
        );
    }
    if usage_start.is_some()
        && let Some(text) = usage
    {
        let start = usage_start.unwrap_or(0);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, dim))),
            Rect {
                x: area.x + start as u16,
                y: area.y,
                width: usage_w as u16,
                height: 1,
            },
        );
    }
    if let Some(start) = clock_start {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(clock, dim))),
            Rect {
                x: area.x + start as u16,
                y: area.y,
                width: clock_w as u16,
                height: 1,
            },
        );
    }
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
        let roster_row = row_of(&lines, "agents running");
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

    /// 待发送 joins the bottom control area without displacing anything: the
    /// roster keeps its dock below the footer, the composer stays on screen,
    /// and a long list is bounded instead of pushing either out.
    #[test]
    fn pending_inputs_sit_above_the_composer_and_leave_the_roster_where_it_was() {
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
        for i in 1..=9 {
            state
                .pending_inputs
                .push(crate::pending_inputs::PendingInput::queued(
                    format!("note {i}"),
                    state.session_id.clone(),
                ));
        }
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
        let pending_row = row_of(&lines, "Not sent · 9");
        let input_row = row_of(&lines, "Type a message");
        let footer_row = row_of(&lines, "Context");
        let roster_row = row_of(&lines, "agents running");
        assert!(pending_row < input_row, "{lines:#?}");
        assert!(
            input_row < footer_row && footer_row < roster_row,
            "{lines:#?}"
        );
        assert!(row_of(&lines, "6 more") > pending_row, "{lines:#?}");
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
                call_id: None,
                always_persists: true,
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
        let roster_row = row_of(&lines, "agents running");
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
                .all(|l| !l.contains("agents running")),
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

    /// Density closure: on a wide terminal the elapsed column
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
            .filter(|l| l.contains("55s ↗"))
            .map(|l| col_of(l, "55s ↗").unwrap())
            .collect();
        assert_eq!(meta_cols.len(), 3, "all three children carry elapsed metas");
        assert!(
            meta_cols.iter().all(|c| *c == meta_cols[0]),
            "meta is one shared column: {meta_cols:?}"
        );
        assert!(
            meta_cols[0] < 110,
            "meta aligns to the roster content, not the 180-column right edge: {}",
            meta_cols[0]
        );
        for l in lines.iter().filter(|l| l.contains("55s ↗")) {
            let col = l.find("55s ↗").unwrap();
            assert!(l[..col].ends_with(" · "), "metadata separator: {l:?}");
            let end = l.trim_end().len();
            assert!(end < 130, "the roster keeps a bounded visual width: {end}");
        }
    }

    /// The agent ROSTER row for `activity`.
    ///
    /// The Activity strip above the roster mentions the same tool with the
    /// child nickname and task on the same clickable line.
    fn roster_row<'a>(lines: &'a [String], activity: &str) -> &'a String {
        lines
            .iter()
            .find(|l| l.contains(activity) && l.contains('↗'))
            .unwrap_or_else(|| panic!("no roster row for {activity}: {lines:#?}"))
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
        let row = roster_row(&lines, "read_file");
        assert!(row.contains("55s"), "elapsed still shown: {row:?}");
        assert!(!row.contains("tokens"), "no fake token value: {row:?}");

        // Control: real usage still renders, so the rule above is "do not
        // invent a number", not "never show one".
        let reported = render_workbench(
            120,
            30,
            crate::i18n::Locale::En,
            55,
            team_with_usage(&[("a1", "explorer", "read_file", 362_000)]),
            false,
        );
        let row = roster_row(&reported, "read_file");
        assert!(
            row.contains("55s ↗"),
            "elapsed and detail affordance stay: {row:?}"
        );
        assert!(
            reported.iter().any(|line| line.contains("362k tokens")),
            "aggregate usage moves to the summary: {reported:#?}"
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
            .filter(|l| l.contains("55s ↗"))
            .map(|l| col_of(l, "55s ↗").unwrap())
            .collect();
        assert_eq!(meta_cols.len(), 2);
        assert_eq!(
            meta_cols[0], meta_cols[1],
            "double-width text must not skew the column"
        );
    }

    /// Narrow terminals truncate the task but preserve time and detail access.
    #[test]
    fn narrow_terminal_preserves_elapsed_and_detail_affordance() {
        let lines = render_workbench(
            46,
            30,
            crate::i18n::Locale::En,
            55,
            team_with_usage(&[("a1", "explorer", "read_file", 362_000)]),
            false,
        );
        let row = roster_row(&lines, "read_file");
        assert!(
            row.contains("55s ↗"),
            "narrow width preserves the detail affordance: {row:?}"
        );

        // The other side of the rule: given room, the same row shows the same
        // meta. The exact cut is content-dependent — the meta is dropped when
        // it cannot fit beside head and activity, not at a fixed column — so
        // this pins the behaviour from both sides rather than one boundary
        // cell that would move with the label.
        let wide = render_workbench(
            72,
            30,
            crate::i18n::Locale::En,
            55,
            team_with_usage(&[("a1", "explorer", "read_file", 362_000)]),
            false,
        );
        let row = roster_row(&wide, "read_file");
        assert!(
            row.contains("55s"),
            "given room, the meta is shown: {row:?}"
        );
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
                title: None,
                profile_id: Some((*role).into()),
                agent_name: None,
                read_only: true,
                contribution: None,
                started_elapsed_secs: 0,
                stop: None,
                limit: None,
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
    fn team_panel_is_one_summary_plus_one_line_per_clickable_agent() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.elapsed_secs = 106;
        let specs = [
            ("c1", "Euclid", "摸清前台手工发单全链路", 13_000),
            ("c2", "Newton", "摸清后台目录规格编辑链路", 16_979),
        ];
        for (id, nickname, task, tokens) in specs {
            state.team.apply_update(crate::multi_agent::ChildUpdate {
                id: id.into(),
                nickname: nickname.into(),
                role: "explorer".into(),
                done: false,
                ok: false,
                detail: task.into(),
                title: Some(task.into()),
                profile_id: None,
                agent_name: None,
                read_only: true,
                contribution: None,
                stop: None,
                limit: None,
                started_elapsed_secs: 0,
            });
            state.team.apply_progress(id, true, tokens, 0);
        }

        let rows = panel_rows(&state, 3);
        let text = rows.join("\n");
        assert!(
            squash(&rows[0]).contains("2个agents正在运行·29.9ktokens·1m46s"),
            "summary: {text}"
        );
        assert!(
            squash(&rows[1]).contains("Euclid·摸清前台手工发单全链路·1m46s↗"),
            "first child stays on one line: {text}"
        );
        assert!(
            squash(&rows[2]).contains("Newton·摸清后台目录规格编辑链路·1m46s↗"),
            "second child stays on one line: {text}"
        );
        assert!(
            !text.contains("主 Agent"),
            "no duplicate coordinator row: {text}"
        );
    }

    #[test]
    fn rendered_team_rows_keep_both_detail_targets() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.team = team_with(&[
            ("c1", "explorer", "摸清前台手工发单全链路"),
            ("c2", "explorer", "摸清后台目录规格编辑链路"),
        ]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();

        let ids: Vec<&str> = state
            .activity_hits
            .iter()
            .map(|(_, id)| id.as_key())
            .collect();
        assert_eq!(ids, vec!["c1", "c2"], "both ↗ rows open their own detail");
    }

    #[test]
    fn no_team_no_panel_and_no_stolen_rows() {
        let empty = render_with_team(crate::multi_agent::TaskTeamView::default());
        let staffed = render_with_team(team_with(&[
            ("a1", "explorer", "look"),
            ("w1", "worker", "implement"),
        ]));
        // Roster shape: the team summary is the standing title.
        assert!(!empty.contains("● Main"), "{empty}");
        assert!(staffed.contains("2 agents running"), "{staffed}");
        assert!(!staffed.contains("● Main"), "{staffed}");
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

    /// Every strip stacked above the composer — the notice, the runtime
    /// status, pending attachments — starts on the same column as the
    /// composer's border: one grid, not chrome hanging off the terminal edge.
    #[test]
    fn strips_above_the_composer_share_its_left_edge() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        for width in [80u16, 44, 12] {
            let mut state = test_state();
            state.theme = crate::theme::Theme::dark();
            state.status = leveler_client_protocol::RuntimeStatus::Busy;
            state.notification = Some(crate::state::Notification {
                level: leveler_client_protocol::NotificationLevel::Warning,
                message: "选择权限模式: 完全访问（免审批）".into(),
            });
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| crate::render::render(frame, &mut state))
                .unwrap();
            let buf = terminal.backend().buffer();
            let row = |needle: &str| -> Option<String> {
                (0..buf.area.height)
                    .map(|y| {
                        (0..buf.area.width)
                            .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                            .collect::<String>()
                    })
                    .find(|l| l.contains(needle))
            };
            let first_col = |line: &str| line.chars().position(|c| !c.is_whitespace());
            let gutter = Some(crate::layout::WORKSPACE_GUTTER_X as usize);
            let notice = row("\u{26a0}").expect("notice row");
            assert_eq!(first_col(&notice), gutter, "{width}: notice {notice:?}");
            if width >= 44 {
                // Wide glyphs occupy two cells, so match on the ASCII clock.
                let status = row(" · 0s").expect("status row");
                assert_eq!(first_col(&status), gutter, "{width}: status {status:?}");
                // The composer is the LAST box on screen (the welcome card
                // above it is a box too).
                let border = (0..buf.area.height)
                    .rev()
                    .map(|y| {
                        (0..buf.area.width)
                            .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                            .collect::<String>()
                    })
                    .find(|l| l.contains('\u{256d}'))
                    .expect("composer border");
                assert_eq!(first_col(&border), gutter, "{width}: composer {border:?}");
            }
        }
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
        let lines: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        buf.cell((x, y))
                            .and_then(|c| c.symbol().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect()
            })
            .collect();
        // The usage chips now sit far-right; the row's left content is the key
        // hints. That left content still starts on the workspace gutter.
        let footer_row = lines
            .iter()
            .position(|l| l.contains("15k/1M") || l.contains("15k"))
            .unwrap_or_else(|| panic!("footer context line missing:\n{}", lines.join("\n")));
        let first = lines[footer_row].find(|c: char| !c.is_whitespace());
        assert_eq!(
            first,
            Some(crate::layout::WORKSPACE_GUTTER_X as usize),
            "footer left content must sit on the workspace gutter: {:?}",
            lines[footer_row]
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

    /// The clock is a far-right anchor on the shared bottom row: it ends at the
    /// workspace gutter with a gap before the usage chips, never glued to them.
    #[test]
    fn footer_renders_the_local_wall_clock_after_the_runtime_chips() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = test_state();
        state.context_window_tokens = 1_048_576;
        state.context_tokens = 254_000;
        state.token_input = 1000;
        state.token_cached = 990;
        state.clock_label = "22:22".into();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let buf = terminal.backend().buffer();
        let lines: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        buf.cell((x, y))
                            .and_then(|c| c.symbol().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect()
            })
            .collect();
        // Match on the ASCII counts/clock, not the localized labels: a 2-cell
        // CJK glyph has an empty continuation cell between it and its neighbour.
        let footer = lines
            .iter()
            .find(|l| l.contains("254k/1M"))
            .unwrap_or_else(|| panic!("footer context line missing:\n{}", lines.join("\n")));
        assert!(footer.contains("99%"), "usage chips stay visible: {footer}");
        // The clock anchors the right edge and is separated from the chips.
        assert!(
            footer.trim_end().ends_with("22:22"),
            "clock anchors the right edge: {footer:?}"
        );
        assert!(
            !footer.contains("99% · 22:22"),
            "clock is its own cell, not glued to the chips: {footer:?}"
        );
    }

    /// Render the workbench with a clock, optional usage and optional busy run.
    fn render_clock_case(
        width: u16,
        height: u16,
        clock: &str,
        usage: bool,
        busy: bool,
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
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        );
        state.clock_label = clock.into();
        if usage {
            state.context_tokens = 18_400;
            state.token_input = 18_400;
            state.token_cached = 2_000;
        }
        if busy {
            state.status = leveler_client_protocol::RuntimeStatus::Busy;
            state.elapsed_secs = 12;
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

    /// Case 1 — empty session: the clock shares the hint row and is never a
    /// line of its own below it (the "empty transcript item" the user saw).
    #[test]
    fn clock_shares_the_bottom_row_with_the_hints() {
        let lines = render_clock_case(100, 30, "09:30", false, false);
        let clock_row = row_of(&lines, "09:30");
        let hint_row = row_of(&lines, "Shift+Tab");
        assert_eq!(
            clock_row,
            hint_row,
            "clock and hints share one row:\n{}",
            lines.join("\n")
        );
        assert!(
            lines.iter().filter(|l| l.contains("09:30")).count() == 1,
            "the clock is not repeated on another row:\n{}",
            lines.join("\n")
        );
    }

    /// Case 1 — the clock is the far-right anchor of the row: it ends exactly
    /// at the workspace gutter on the right edge, never padded mid-row.
    #[test]
    fn clock_is_right_aligned_on_the_bottom_row() {
        let lines = render_clock_case(100, 30, "09:30", false, false);
        let clock_row = row_of(&lines, "09:30");
        let col = col_of(&lines[clock_row], "09:30").expect("clock present");
        assert_eq!(
            col + 5,
            100 - crate::layout::WORKSPACE_GUTTER_X as usize,
            "clock anchors the right gutter: {:?}",
            lines[clock_row]
        );
    }

    /// Case 3 — a narrow row keeps the clock whole, clips the hints, and never
    /// wraps the clock onto a second line.
    #[test]
    fn narrow_row_keeps_the_clock_and_truncates_hints() {
        let lines = render_clock_case(40, 24, "09:30", false, false);
        let clock_row = row_of(&lines, "09:30");
        assert_eq!(
            lines.iter().filter(|l| l.contains("09:30")).count(),
            1,
            "clock must not wrap to a second line:\n{}",
            lines.join("\n")
        );
        assert!(
            lines[clock_row].contains('…'),
            "hints clip before the clock: {:?}",
            lines[clock_row]
        );
        let col = col_of(&lines[clock_row], "09:30").expect("clock present");
        assert_eq!(col + 5, 40 - crate::layout::WORKSPACE_GUTTER_X as usize);
    }

    /// Case 2/3 — usage chips ride next to the clock when there is room, and
    /// yield to it when the row is narrow. The clock always survives.
    #[test]
    fn usage_chips_yield_to_the_clock_when_the_row_is_narrow() {
        let wide = render_clock_case(100, 24, "09:30", true, false);
        let wide_row = row_of(&wide, "09:30");
        assert!(
            wide[wide_row].contains("18k"),
            "usage shown when wide: {:?}",
            wide[wide_row]
        );

        let narrow = render_clock_case(24, 24, "09:30", true, false);
        let nrow = row_of(&narrow, "09:30");
        assert!(
            !narrow[nrow].contains("18k"),
            "usage yields on a narrow row: {:?}",
            narrow[nrow]
        );
        assert_eq!(narrow.iter().filter(|l| l.contains("09:30")).count(), 1);
    }

    /// Case 4 — the running-state status strip and the clock are different
    /// rows: the footer work must not touch the runtime status.
    #[test]
    fn clock_merge_keeps_the_running_status_strip() {
        let lines = render_clock_case(100, 30, "09:37", true, true);
        let status_row = lines
            .iter()
            .position(|l| l.contains("12s"))
            .unwrap_or_else(|| panic!("busy status missing:\n{}", lines.join("\n")));
        let clock_row = row_of(&lines, "09:37");
        assert!(
            status_row < clock_row,
            "runtime status stays above the bottom row"
        );
        assert!(lines[status_row].contains("12s"));
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

    /// The plan is the agent's declared progress: the header says how much it
    /// has declared done, and no "当前 2/3" cursor that reads as the runtime
    /// tracking where execution is.
    #[test]
    fn a_live_plan_title_reports_declared_progress_without_a_cursor() {
        let t = crate::i18n::Locale::Zh.text();
        let title = plan_chrome_title(&sample_plan(), true, t);
        assert!(title.starts_with('●'), "{title}");
        assert!(title.contains("已完成 1/3"), "{title}");
        assert!(!title.contains("当前"), "no runtime cursor: {title}");
        assert!(
            title.contains("进行中：edit module"),
            "the one-line summary names the running step: {title}"
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
                agent_name: None,
                read_only: false,
                title: None,
                purpose: "check deps".into(),
                status: *st,
                contribution: crate::multi_agent::Contribution::Pending,
                recent_step: None,
                input_tokens: 0,
                output_tokens: 0,
                started_elapsed_secs: 0,
                settled_elapsed_secs: None,
                detail: None,
                activity: Vec::new(),
                stop: None,
                limit: None,
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
        // Aggregate sits on the task baseline; children form one indented tree.
        let main = rows
            .iter()
            .find(|r| r.contains('●'))
            .unwrap_or_else(|| panic!("summary row rendered:\n{}", rows.join("\n")));
        let main_col = main.len() - main.trim_start().len();
        assert_eq!(
            main_col,
            crate::layout::WORKSPACE_GUTTER_X as usize,
            "summary row on the content baseline: {main:?}"
        );
        let member = rows
            .iter()
            .find(|r| r.contains("├─") || r.contains("└─"))
            .unwrap_or_else(|| panic!("member row rendered:\n{}", rows.join("\n")));
        let member_col = member.len() - member.trim_start().len();
        assert_eq!(
            member_col,
            main_col + 2,
            "children are one level inside the summary: {main:?} / {member:?}"
        );
    }

    /// Matrix D/M: the aggregate leads; an active child shows its task,
    /// elapsed and detail affordance.
    #[test]
    fn roster_shows_aggregate_usage_and_child_elapsed() {
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
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: false,
            contribution: None,
            started_elapsed_secs: 0,
            stop: None,
            limit: None,
        });
        team.apply_progress("c1", true, 120_000, 48_000);
        state.team = team;
        let rows = panel_rows(&state, 3).join("\n");
        // Cell-by-cell extraction pads wide glyphs with spaces; compare
        // space-stripped for CJK anchors.
        let squashed = rows.replace(' ', "");
        assert!(rows.contains('●'), "{rows}");
        assert!(squashed.contains("1个agents正在运行"), "{rows}");
        assert!(squashed.contains("审计生命周期与身份"), "{rows}");
        assert!(squashed.contains("4m09s"), "elapsed: {rows}");
        assert!(rows.contains("168k tokens"), "aggregate usage: {rows}");
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
    /// never as ✓; active siblings keep the roster expanded.
    #[test]
    fn roster_failure_truth_with_active_sibling() {
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
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: false,
            contribution: None,
            started_elapsed_secs: 4,
            stop: None,
            limit: None,
        });
        team.apply_update(crate::multi_agent::ChildUpdate {
            id: "bad1".into(),
            nickname: "Newton".into(),
            role: "worker".into(),
            done: true,
            ok: false,
            detail: "died".into(),
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: false,
            contribution: None,
            started_elapsed_secs: 5,
            stop: None,
            limit: None,
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
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: false,
            contribution: None,
            started_elapsed_secs: 6,
            stop: None,
            limit: None,
        });
        state.team = team;
        let rows = panel_rows(&state, 5).join("\n");
        let squashed = rows.replace(' ', "");
        assert!(squashed.contains("1个agents正在运行"), "{rows}");
        assert!(squashed.contains("工作未完成"), "{rows}");
        assert!(squashed.contains("auditc"), "{rows}");
    }

    /// §14: when EVERY child settles, the surface collapses to one truthful
    /// terminal line — an incomplete child keeps the count honest and the
    /// line never reads as pure success.
    #[test]
    fn roster_all_settled_collapses_to_truthful_terminal_line() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        // The children settle at 3s on the turn clock, and the frame is drawn
        // then — a settlement stamped after "now" is not a state a turn reaches.
        state.elapsed_secs = 3;
        let mut team = crate::multi_agent::TaskTeamView::default();
        for (id, ok) in [("ok1", true), ("bad1", false)] {
            team.apply_update(crate::multi_agent::ChildUpdate {
                id: id.into(),
                nickname: "Euclid".into(),
                role: "explorer".into(),
                done: true,
                ok,
                detail: "done".into(),
                title: None,
                profile_id: None,
                agent_name: None,
                read_only: false,
                contribution: None,
                started_elapsed_secs: 3,
                stop: None,
                limit: None,
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

    /// Matrix J: narrow terminals truncate the task but preserve elapsed time
    /// and the detail affordance.
    #[test]
    fn roster_narrow_width_preserves_elapsed_and_detail_affordance() {
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
        assert!(rows.contains('↗'), "{rows}");
        assert!(
            rows.contains("1m 40s"),
            "elapsed remains reachable when narrow: {rows}"
        );
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

    /// The one-line plan is a sibling of the conversation and starts at the
    /// same content baseline.
    #[test]
    fn plan_summary_uses_the_workspace_content_baseline() {
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
        let header_col = header.len() - header.trim_start().len();
        assert_eq!(
            header_col,
            crate::layout::WORKSPACE_GUTTER_X as usize,
            "header sits on the content baseline: {header:?}"
        );
        assert!(header.contains('↗'), "{header:?}");
        assert!(
            rows.iter().skip(1).all(|row| row.trim().is_empty()),
            "{rows:?}"
        );
    }

    /// The summary names the running step because the full list lives on its
    /// own page.
    #[test]
    fn plan_chrome_title_keeps_progress_and_detail_affordance() {
        let t = crate::i18n::Locale::Zh.text();
        let title = plan_chrome_title(&sample_plan(), true, t);
        assert!(title.starts_with('●'), "{title}");
        assert!(
            title.contains("已完成 1/3 · 进行中：edit module"),
            "{title}"
        );
        assert!(title.ends_with('↗'), "{title}");
    }

    /// Outside a running turn the plan is the last record of what the agent
    /// declared, not work under way: no "进行中" anywhere in the header.
    #[test]
    fn a_plan_outside_a_running_turn_reads_as_the_last_record() {
        let t = crate::i18n::Locale::Zh.text();
        let title = plan_chrome_title(&sample_plan(), false, t);
        assert!(title.contains("最后记录 1/3"), "{title}");
        assert!(!title.contains("进行中"), "{title}");
        let t = crate::i18n::Locale::En.text();
        let title = plan_chrome_title(&sample_plan(), false, t);
        assert!(title.contains("last recorded 1/3"), "{title}");
        assert!(!title.contains("in progress"), "{title}");
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
        assert!(title.contains("已完成 0/4"), "{title}");
        assert!(title.contains("4 项待办"), "{title}");
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
        assert!(title.contains("1/2 completed"), "{title}");
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
        assert_eq!(plan_panel_height(&state, 40), 0);

        state.plan = Some(UiPlan { steps: vec![] });
        assert_eq!(plan_panel_height(&state, 40), 0);

        state.plan = Some(sample_plan());
        assert_eq!(plan_panel_height(&state, 40), 1);
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
        assert!(text.contains("task A、task B"), "{text}");
        assert!(
            !text.contains("├─ Euclid") && !text.contains("└─ Newton"),
            "{text}"
        );
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
        s.transcript
            .complete_tool(&call, true, "ok".into(), 1, None);
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

    // ── One-line Plan summary ───────────────────────────────────────────────

    fn plan_panel_rows(state: &AppState, height: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(70, height)).unwrap();
        terminal
            .draw(|f| {
                render_plan_panel(
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

    /// A plan with `total` steps: `current` running, everything before it done.
    fn running_plan(total: usize, current: usize) -> UiPlan {
        UiPlan {
            steps: (0..total)
                .map(|i| UiPlanStep {
                    index: i,
                    description: format!("步骤 {}", i + 1),
                    status: match i.cmp(&current) {
                        std::cmp::Ordering::Less => PlanStepStatus::Done,
                        std::cmp::Ordering::Equal => PlanStepStatus::Running,
                        std::cmp::Ordering::Greater => PlanStepStatus::Pending,
                    },
                })
                .collect(),
        }
    }

    /// The workbench always reserves exactly one row for an active plan. The
    /// complete list belongs to the Plan page.
    #[test]
    fn plan_summary_is_always_one_row() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.plan = Some(running_plan(9, 5));
        let rows = plan_panel_rows(&state, 10);
        let visible: Vec<&String> = rows.iter().filter(|row| !row.trim().is_empty()).collect();
        assert_eq!(visible.len(), 1, "{rows:?}");
        let line = squash(visible[0]);
        assert!(line.contains("已完成5/9"), "{line}");
        assert!(line.contains("进行中：步骤6"), "{line}");
        assert!(line.ends_with('↗'), "{line}");
    }

    /// The summary carries declared progress and never mistakes the running
    /// ordinal for the done count.
    #[test]
    fn the_plan_header_carries_declared_progress() {
        let t = crate::i18n::Locale::Zh.text();
        let title = plan_chrome_title(&running_plan(9, 5), true, t);
        assert!(title.contains("已完成 5/9"), "{title}");
        assert!(!title.contains("6/9"), "{title}");
        let t = crate::i18n::Locale::En.text();
        let title = plan_chrome_title(&running_plan(9, 5), true, t);
        assert!(title.contains("5/9 completed"), "{title}");
        assert!(!title.contains("6/9"), "{title}");
    }

    /// P9: one running glyph for both plan surfaces (`/plan` used ●, the dock
    /// used →, so the same state read as two different things).
    #[test]
    fn both_plan_surfaces_use_the_same_running_glyph() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.plan = Some(running_plan(3, 1));
        let rows = plan_panel_rows(&state, 5);
        assert!(
            rows.iter().any(|r| r.trim_start().starts_with('●')),
            "{rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.trim_start().starts_with('→')),
            "{rows:?}"
        );
    }

    /// A fully finished plan still leaves the workbench entirely.
    #[test]
    fn a_finished_plan_takes_no_rows_even_with_an_adaptive_viewport() {
        let mut state = test_state();
        state.plan = Some(UiPlan {
            steps: (0..9)
                .map(|i| UiPlanStep {
                    index: i,
                    description: format!("步骤 {}", i + 1),
                    status: PlanStepStatus::Done,
                })
                .collect(),
        });
        assert_eq!(plan_panel_height(&state, 40), 0);
    }

    /// An active plan never takes more than one row, and no row is painted when
    /// the layout has no room.
    #[test]
    fn plan_summary_height_is_fixed() {
        let mut state = test_state();
        state.plan = Some(running_plan(9, 5));
        assert_eq!(plan_panel_height(&state, 40), 1);
        assert_eq!(plan_panel_height(&state, 1), 1);
        assert_eq!(plan_panel_height(&state, 0), 0);
    }

    /// A failed step owns the one-line status and stays visibly actionable.
    #[test]
    fn failed_plan_summary_names_the_failed_step() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let mut plan = running_plan(4, 2);
        plan.steps[2].status = PlanStepStatus::Failed;
        state.plan = Some(plan);
        let line = squash(&plan_panel_rows(&state, 4).join("\n"));
        assert!(line.starts_with("!计划"), "{line}");
        assert!(line.contains("第3步失败：步骤3"), "{line}");
        assert!(line.ends_with('↗'), "{line}");
    }

    // ── Notice Surface ─────────────────────────────────────────────────────

    /// Full-workbench render with an optional Notice and optional scrollback.
    fn render_notice_case(
        width: u16,
        height: u16,
        notice: Option<(leveler_client_protocol::NotificationLevel, &str)>,
        history: usize,
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
                locale: crate::i18n::Locale::En,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        );
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.elapsed_secs = 24;
        if let Some((level, message)) = notice {
            state.notification = Some(crate::state::Notification {
                level,
                message: message.to_string(),
            });
        }
        for i in 0..history {
            state.transcript.push_user(format!("ROW{i}"));
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

    /// The Notice Surface owns a real, left-aligned row directly above the
    /// runtime status strip — never a bottom-right toast over the Conversation.
    #[test]
    fn notice_owns_a_left_aligned_row_above_the_runtime_status() {
        let lines = render_notice_case(
            80,
            24,
            Some((
                leveler_client_protocol::NotificationLevel::Warning,
                "Agent is running · Wait or cancel",
            )),
            0,
        );
        let notice_row = row_of(&lines, "⚠");
        assert_eq!(
            col_of(&lines[notice_row], "⚠"),
            Some(crate::layout::WORKSPACE_GUTTER_X as usize),
            "left-aligned on the composer's grid, not the terminal edge"
        );
        assert!(
            lines[notice_row].contains("Agent is running"),
            "the notice text is on its own row: {:?}",
            lines[notice_row]
        );
        assert!(
            notice_row < row_of(&lines, "Type a message"),
            "the notice is a fixed strip, not floating in the composer"
        );
    }

    /// A notice is accounted in layout: the row comes out of the Conversation
    /// viewport, so the chrome below keeps its exact position.
    #[test]
    fn a_notice_costs_exactly_one_layout_row() {
        let base = render_notice_case(80, 24, None, 0);
        let with = render_notice_case(
            80,
            24,
            Some((leveler_client_protocol::NotificationLevel::Info, "ready")),
            0,
        );
        assert!(
            base.iter().all(|l| !l.contains('ℹ')),
            "no notice → no notice row"
        );
        assert_eq!(
            row_of(&with, "Type a message"),
            row_of(&base, "Type a message"),
            "the notice must not push the composer"
        );
        let status_row = row_of(&with, "24s");
        assert_eq!(
            row_of(&with, "ℹ") + 1,
            status_row,
            "the notice sits directly above the runtime status strip"
        );
        assert_eq!(
            row_of(&base, "24s"),
            status_row,
            "the runtime status stays where it was"
        );
    }

    /// The row the notice takes is the Conversation's: the viewport is exactly
    /// one row shorter with a notice than without one.
    #[test]
    fn a_notice_shrinks_the_conversation_viewport_by_one_row() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        fn conv_height(notice: bool) -> u16 {
            let mut s = test_state();
            s.status = leveler_client_protocol::RuntimeStatus::Busy;
            s.elapsed_secs = 24;
            if notice {
                s.notification = Some(crate::state::Notification {
                    level: leveler_client_protocol::NotificationLevel::Info,
                    message: "ready".into(),
                });
            }
            let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
            term.draw(|f| crate::render::render(f, &mut s)).unwrap();
            s.conv.rect.expect("conversation viewport rendered").3
        }

        assert_eq!(
            conv_height(false) - conv_height(true),
            1,
            "the notice takes exactly one row from the transcript"
        );
    }

    /// Scrollback does not move a notice: the row sits below the fixed-height
    /// Conversation viewport, not inside it.
    #[test]
    fn notice_stays_fixed_when_the_transcript_scrolls() {
        let empty = render_notice_case(
            80,
            24,
            Some((
                leveler_client_protocol::NotificationLevel::Warning,
                "watch out",
            )),
            0,
        );
        let full = render_notice_case(
            80,
            24,
            Some((
                leveler_client_protocol::NotificationLevel::Warning,
                "watch out",
            )),
            40,
        );
        assert_eq!(
            row_of(&empty, "⚠"),
            row_of(&full, "⚠"),
            "transcript length must not move the notice row"
        );
    }

    /// On a narrow terminal the notice truncates to a single row — it never
    /// wraps and grows the strip.
    #[test]
    fn a_long_notice_truncates_to_one_row() {
        let lines = render_notice_case(
            28,
            24,
            Some((
                leveler_client_protocol::NotificationLevel::Warning,
                "Agent is running · Wait for the current task to finish or cancel it first",
            )),
            0,
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains('⚠')).count(),
            1,
            "exactly one notice row, no wrap: {lines:#?}"
        );
        let notice_row = row_of(&lines, "⚠");
        assert!(
            lines[notice_row].trim_end().ends_with('…'),
            "a clipped notice ends in an ellipsis: {:?}",
            lines[notice_row]
        );
        assert!(
            lines.iter().all(|l| !l.contains("cancel it first")),
            "the truncated tail must not spill onto another row"
        );
    }

    /// The surface holds one current fact: a second notice replaces the first
    /// (the reducer always overwrites the single slot).
    #[test]
    fn a_new_notice_replaces_the_previous_one() {
        let mut s = test_state();
        s.notification = Some(crate::state::Notification {
            level: leveler_client_protocol::NotificationLevel::Info,
            message: "first".into(),
        });
        s.notification = Some(crate::state::Notification {
            level: leveler_client_protocol::NotificationLevel::Warning,
            message: "second".into(),
        });
        assert_eq!(s.notification.as_ref().unwrap().message, "second");
    }

    // ── Background jobs in the input footer ────────────────────────────────

    fn bg_running(label: &str, started: u64) -> crate::state::BackgroundTaskChrome {
        crate::state::BackgroundTaskChrome::running(label, started)
    }

    fn bg_terminal(
        label: &str,
        ok: bool,
        stopped: bool,
        started: u64,
        duration_ms: u64,
    ) -> crate::state::BackgroundTaskChrome {
        crate::state::BackgroundTaskChrome {
            label: label.into(),
            started_elapsed_secs: started,
            ok: Some(ok),
            stopped,
            exit_code: if ok { Some(0) } else { Some(1) },
            duration_ms: Some(duration_ms),
            output: String::new(),
        }
    }

    fn render_background_case(
        width: u16,
        height: u16,
        tasks: Vec<(&str, crate::state::BackgroundTaskChrome)>,
        seen: &[&str],
        focus: crate::state::WorkbenchFocus,
    ) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
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
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        );
        state.elapsed_secs = 90;
        state.clock_label = "09:30".into();
        state.background_task_labels.clear();
        for (id, chrome) in tasks {
            state.background_task_labels.insert(id.into(), chrome);
        }
        for id in seen {
            state.background_failures_seen.insert((*id).into());
        }
        state.workbench_focus = focus;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0u16;
                while x < buf.area.width {
                    let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                    row.push_str(sym);
                    // Advance past a wide glyph's continuation cell so display
                    // columns (CJK) stay aligned with the painted row.
                    x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
                }
                row
            })
            .collect()
    }

    /// Case A — nothing running, no unread failure: no background section at
    /// all, and the footer stays the single existing row.
    #[test]
    fn no_background_tasks_add_nothing_to_the_footer() {
        let lines =
            render_background_case(100, 30, vec![], &[], crate::state::WorkbenchFocus::Input);
        let all = lines.join("\n");
        assert!(!all.contains("后台"), "{all}");
        assert!(!all.contains('↗'), "{all}");
        // The footer is still one row: hints and clock share it.
        assert_eq!(row_of(&lines, "09:30"), row_of(&lines, "Ctrl+?"));
    }

    /// Case B — one running task shows its name in the footer, not a body row.
    #[test]
    fn one_running_task_is_named_in_the_footer_only() {
        let lines = render_background_case(
            100,
            30,
            vec![("bg-1", bg_running("make up", 28))],
            &[],
            crate::state::WorkbenchFocus::Input,
        );
        let all = lines.join("\n");
        assert!(all.contains("make up"), "{all}");
        assert_eq!(
            lines.iter().filter(|l| l.contains("make up")).count(),
            1,
            "the task appears once, in the footer: {all}"
        );
        assert!(lines[row_of(&lines, "make up")].contains('↗'), "{all}");
    }

    /// Case C — several running tasks aggregate into a single count, never one
    /// row per task, and the whole block occupies the existing footer row.
    #[test]
    fn several_running_tasks_aggregate_into_one_footer_block() {
        let lines = render_background_case(
            100,
            30,
            vec![
                ("bg-1", bg_running("make up", 10)),
                ("bg-2", bg_running("cargo test", 20)),
                ("bg-3", bg_running("dev server", 30)),
            ],
            &[],
            crate::state::WorkbenchFocus::Input,
        );
        let all = lines.join("\n");
        assert!(all.contains("后台 3"), "{all}");
        assert_eq!(
            lines.iter().filter(|l| l.contains('↗')).count(),
            1,
            "one aggregate block: {all}"
        );
        for label in ["make up", "cargo test", "dev server"] {
            assert!(
                !all.contains(label),
                "{label} must not get its own row: {all}"
            );
        }
    }

    /// Case D — running and failed are separate counts with separate inks.
    #[test]
    fn running_and_failed_are_separate_counts() {
        let lines = render_background_case(
            120,
            30,
            vec![
                ("bg-1", bg_running("make up", 10)),
                ("bg-2", bg_running("cargo test", 20)),
                ("bg-3", bg_terminal("npm start", false, false, 0, 30_000)),
            ],
            &[],
            crate::state::WorkbenchFocus::Input,
        );
        let all = lines.join("\n");
        assert!(all.contains("后台 2"), "{all}");
        assert!(all.contains("失败 1"), "{all}");
        assert!(
            !all.contains("后台 3"),
            "failed is not folded into running: {all}"
        );
    }

    /// Case E — only failures: the short failure block, not a count of tasks.
    #[test]
    fn failures_alone_read_as_a_failure_block() {
        let lines = render_background_case(
            120,
            30,
            vec![
                ("bg-1", bg_terminal("npm start", false, false, 0, 30_000)),
                ("bg-2", bg_terminal("cargo test", false, false, 0, 24_000)),
            ],
            &[],
            crate::state::WorkbenchFocus::Input,
        );
        let all = lines.join("\n");
        assert!(all.contains("后台失败 2"), "{all}");
        assert!(!all.contains("后台 2"), "{all}");
    }

    /// A completed task never lingers in the footer — its history is the list.
    #[test]
    fn a_completed_task_does_not_linger_in_the_footer() {
        let lines = render_background_case(
            120,
            30,
            vec![("bg-1", bg_terminal("cargo check", true, false, 0, 18_000))],
            &[],
            crate::state::WorkbenchFocus::Input,
        );
        let all = lines.join("\n");
        assert!(!all.contains("cargo check"), "{all}");
        assert!(!all.contains('↗'), "{all}");
    }

    /// A stopped task is not a failure: no badge, no error ink.
    #[test]
    fn a_stopped_task_is_not_counted_as_a_failure() {
        let lines = render_background_case(
            120,
            30,
            vec![("bg-1", bg_terminal("npm start", false, true, 0, 24_000))],
            &[],
            crate::state::WorkbenchFocus::Input,
        );
        let all = lines.join("\n");
        assert!(!all.contains("失败"), "{all}");
        assert!(!all.contains('↗'), "{all}");
    }

    /// An acknowledged failure stops reminding; the task itself is untouched.
    #[test]
    fn acknowledged_failures_leave_the_footer() {
        let lines = render_background_case(
            120,
            30,
            vec![("bg-1", bg_terminal("npm start", false, false, 0, 30_000))],
            &["bg-1"],
            crate::state::WorkbenchFocus::Input,
        );
        let all = lines.join("\n");
        assert!(!all.contains("失败"), "{all}");
        assert!(!all.contains('↗'), "{all}");
    }

    /// A focused summary is reachable: it carries the `→` marker and the hint
    /// row names Enter.
    #[test]
    fn a_focused_background_summary_advertises_enter() {
        let lines = render_background_case(
            120,
            30,
            vec![("bg-1", bg_running("make up", 10))],
            &[],
            crate::state::WorkbenchFocus::Background,
        );
        let all = lines.join("\n");
        assert!(all.contains("→ ● make up"), "{all}");
        assert!(all.contains("Enter 后台任务"), "{all}");
    }

    /// Long command labels truncate; the clock and the right chips survive.
    #[test]
    fn a_long_command_truncates_before_the_right_chips() {
        let label = "API_PROXY_TARGET=http://localhost:8080 exec npm start -- --watch";
        let lines = render_background_case(
            60,
            30,
            vec![("bg-1", bg_running(label, 10))],
            &[],
            crate::state::WorkbenchFocus::Input,
        );
        let row = &lines[row_of(&lines, "09:30")];
        assert!(row.contains("09:30"), "the clock survives: {row:?}");
        assert!(row.chars().count() <= 60, "no overflow: {row:?}");
        assert!(!row.contains("--watch"), "the tail is dropped: {row:?}");
    }

    /// CJK labels are measured in display columns, not bytes, and never panic.
    #[test]
    fn a_cjk_command_keeps_display_width() {
        let lines = render_background_case(
            48,
            30,
            vec![("bg-1", bg_running("构建前端生产包并部署到预发环境", 10))],
            &[],
            crate::state::WorkbenchFocus::Input,
        );
        for row in &lines {
            assert!(
                unicode_width::UnicodeWidthStr::width(row.as_str()) <= 48,
                "row overflows: {row:?}"
            );
        }
    }

    /// Narrow terminals never underflow, overlap or panic. A running task
    /// keeps the clock; an unread failure outranks the clock, which is exactly
    /// the responsive contract.
    #[test]
    fn narrow_terminals_are_safe_with_a_background_summary() {
        for width in [12u16, 16, 20, 24, 32, 40] {
            let running_only = render_background_case(
                width,
                24,
                vec![("bg-1", bg_running("make up", 10))],
                &[],
                crate::state::WorkbenchFocus::Input,
            );
            assert_eq!(running_only.len(), 24, "one row per line at width {width}");
            assert!(
                running_only.join("\n").contains("09:30"),
                "the clock survives a running summary at width {width}"
            );
            for row in &running_only {
                assert!(
                    unicode_width::UnicodeWidthStr::width(row.as_str()) <= width as usize,
                    "width {width} row overflows: {row:?}"
                );
            }

            let with_failure = render_background_case(
                width,
                24,
                vec![
                    ("bg-1", bg_running("make up", 10)),
                    ("bg-2", bg_terminal("npm start", false, false, 0, 30_000)),
                ],
                &[],
                crate::state::WorkbenchFocus::Input,
            );
            assert_eq!(with_failure.len(), 24);
            for row in &with_failure {
                assert!(
                    unicode_width::UnicodeWidthStr::width(row.as_str()) <= width as usize,
                    "width {width} failure row overflows: {row:?}"
                );
            }
        }
    }
}
