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

/// Whether the plan has work the user should still see outside a running turn:
/// anything Pending/Running, or a failure. A plan whose every step is
/// Done/Skipped is finished history.
///
/// This is the ARCHIVE question (`reduce`: does a settled checklist still earn
/// a transcript block, or was the answer it describes already written?). What
/// the live strip shows is [`plan_indicator_should_show`], which is this plus
/// the running turn.
pub(crate) fn plan_panel_should_show(plan: &UiPlan) -> bool {
    if plan.steps.is_empty() {
        return false;
    }
    let all_success = plan.steps.iter().all(|s| is_plan_step_settled(s.status));
    !all_success
}

/// Whether the plan indicator joins the state strip.
///
/// While the turn runs, the plan is live state — including the moment it is
/// declared complete (✓ before the answer forecloses it). Outside a running
/// turn, only open or failed work stays: a finished checklist with nothing
/// executing is history, and the conversation already has it.
pub(crate) fn plan_indicator_should_show(plan: &UiPlan, live: bool) -> bool {
    !plan.steps.is_empty() && (live || plan_panel_should_show(plan))
}

/// The plan's half of the state strip, before fitting decides what survives.
///
/// One structured value so the glyph, the progress and the state word cannot
/// drift apart between the widest and the narrowest terminal.
struct PlanStrip {
    glyph: &'static str,
    glyph_color: ratatui::style::Color,
    /// `计划 3/4` — the plan's name and its declared progress.
    core: String,
    /// The step declared in progress, or the plan's state word. Never a step
    /// the plan has not declared.
    detail: Option<String>,
    detail_color: ratatui::style::Color,
}

/// Build the plan indicator from the live plan.
///
/// The state is read from the plan alone: a running step breathes, a completed
/// plan is a green check, a failure is red, and a plan that is merely the last
/// record says so. Nothing is inferred from the runtime (a tool count cannot
/// make a step "done") and no step is promoted to "in progress" that the plan
/// did not declare.
fn plan_strip(state: &AppState, plan: &UiPlan, live: bool) -> PlanStrip {
    let t = state.t();
    let theme = &state.theme;
    let (done, total) = plan_done_total(plan);
    let core = format!("{} {}/{}", t.active_plan, done, total);
    let failed = plan
        .steps
        .iter()
        .any(|step| step.status == PlanStepStatus::Failed);
    let all_settled = plan.steps.iter().all(|s| is_plan_step_settled(s.status));
    if failed {
        return PlanStrip {
            glyph: "✕",
            glyph_color: theme.status.error,
            core,
            detail: Some(t.plan_state_failed.to_string()),
            detail_color: theme.status.error,
        };
    }
    if live && all_settled {
        return PlanStrip {
            glyph: "✓",
            glyph_color: theme.status.success,
            core,
            detail: Some(t.plan_state_done.to_string()),
            detail_color: theme.status.success,
        };
    }
    if live {
        // The breath is luminance only, and it reads elapsed time, never a
        // frame counter: a burst of stream deltas must not speed it up.
        let glyph_color = if state.live_animation {
            theme.plan.breathe(crate::motion::breath_level(
                state.motion_clock,
                crate::motion::PLAN_BREATH_PERIOD,
            ))
        } else {
            theme.plan.primary
        };
        let detail = plan
            .steps
            .iter()
            .find(|s| s.status == PlanStepStatus::Running)
            .map(|step| step.description.clone())
            .or_else(|| {
                let pending = plan
                    .steps
                    .iter()
                    .filter(|s| s.status == PlanStepStatus::Pending)
                    .count();
                (pending > 0).then(|| t.plan_pending_count.replace("{n}", &pending.to_string()))
            });
        return PlanStrip {
            glyph: "●",
            glyph_color,
            core,
            detail,
            detail_color: theme.text.secondary,
        };
    }
    PlanStrip {
        glyph: "○",
        glyph_color: theme.text.muted,
        core,
        detail: Some(t.plan_state_last.to_string()),
        detail_color: theme.text.muted,
    }
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
    // The live user-shell row is the ONE running-local-command indicator. It
    // shares the user-shell status source with the inline record and vanishes
    // on the terminal event, so the two can never disagree.
    let live_rows: u16 = u16::from(state.running_user_shell().is_some());
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
    // Runtime status (spinner / elapsed / tokens) and the plan indicator share
    // ONE state strip: the runtime is the turn's state, the plan is its
    // declared shape, and two rows for one moment of the turn spent a row of
    // conversation on chrome. The strip is never merged with the transient
    // notice above it. Every strip of bottom chrome sits on the composer's
    // grid: the same workspace gutter, applied through `chrome_slot` below.
    // The text is laid out for that inner width, not the terminal's.
    let status_block = status_lines(state, workspace_inner.width as usize);
    let status_has_content = status_block
        .iter()
        .any(|line| line.spans.iter().any(|span| !span.content.is_empty()));
    let plan_visible = state
        .plan
        .as_ref()
        .is_some_and(|plan| plan_indicator_should_show(plan, state.is_busy()));
    // Any painted cell earns the row: the runtime's own line, a live plan, a
    // running background task, or an active issue. (Visibility here and the
    // renderer's own filters must agree, or a cell would be counted for a row
    // it never gets to paint on.)
    let background_visible = crate::activity::running_background_count(state) > 0;
    let issue_visible = !crate::issue::issue_summary(state).is_empty();
    let strip_rows: u16 =
        u16::from(status_has_content || plan_visible || background_visible || issue_visible);
    // Breathing room around the input box: blank above only when live chrome
    // (the state strip / attachments / 待发送) sits on top of it; blank below
    // always so Context footer is not flush on the composer border.
    let chrome_above = strip_rows
        .saturating_add(attach_rows)
        .saturating_add(pending_rows)
        .saturating_add(live_rows);
    let pre_composer_gap: u16 = if chrome_above > 0 { 1 } else { 0 };

    // One breathing row between the Context footer and the roster, so the
    // process list reads as its own surface rather than a second footer.
    let team_gap: u16 = if team_rows > 0 { 1 } else { 0 };

    let chunks = Layout::vertical([
        Constraint::Length(header_rows),
        Constraint::Min(MIN_CONVERSATION_ROWS), // conversation viewport
        Constraint::Length(gap_rows),
        Constraint::Length(notice_rows), // Notice Surface (0 rows when idle)
        Constraint::Length(strip_rows),  // runtime status + plan indicator
        Constraint::Length(attach_rows),
        Constraint::Length(pending_rows),
        Constraint::Length(live_rows),
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

    // The one horizontal rule for the bottom chrome: notice, strip,
    // attachments, pending inputs, composer and footer all start on the
    // composer's left edge. (The roster applies the same gutter inside its own
    // renderer.)
    let chrome_slot =
        |slot: Rect| crate::layout::horizontal_inset(slot, crate::layout::WORKSPACE_GUTTER_X);
    let notice_slot = chrome_slot(chunks[3]);
    let strip_slot = chrome_slot(chunks[4]);
    let attach_slot = chrome_slot(chunks[5]);
    let pending_slot = chrome_slot(chunks[6]);
    let live_slot = chrome_slot(chunks[7]);
    let input_slot = chrome_slot(chunks[9]);
    let footer_slot = chrome_slot(chunks[10]);

    render_header(frame, chunks[0], state);
    crate::conversation::viewport::render(frame, chunks[1], state);
    // The composer's own plane. An overlay takes this slot instead, and the
    // decision sits on the conversation's background — so the input surface
    // must not be painted underneath it.
    if state.overlay.is_none() {
        let input_bg = state.theme.surface.input;
        state.theme.paint_surface(frame, input_slot, input_bg);
    }
    // chunks[2] = gap (leave blank)
    render_notice(frame, notice_slot, state);
    render_state_strip(frame, strip_slot, state);
    render_attachments(frame, attach_slot, state);
    crate::pending_inputs::render(frame, pending_slot, state, area.height);
    render_shell_live_activity(frame, live_slot, state);
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
        crate::render::render_shell_completion(frame, chunks[1], input_slot, state);
        render_slash_popup(frame, chunks[1], input_slot, state);
    }
}

/// The live local command above the composer: spinner, command and elapsed on
/// one row, with a clickable stop control. It is not a second activity panel;
/// it is the same execution state the inline record paints.
fn render_shell_live_activity(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let Some(shell) = state.running_user_shell() else {
        state.shell_live_stop_rect = None;
        state.shell_live_background_rect = None;
        return;
    };
    if area.width == 0 || area.height == 0 {
        state.shell_live_stop_rect = None;
        state.shell_live_background_rect = None;
        return;
    }
    let theme = &state.theme;
    let elapsed_ms =
        (state.motion_clock.as_millis() as i64 - shell.started_motion_ms).max(0) as u64;
    let elapsed = if elapsed_ms >= 60_000 {
        format!("{}m{:02}s", elapsed_ms / 60_000, (elapsed_ms / 1000) % 60)
    } else {
        format!("{:.1}s", elapsed_ms as f64 / 1000.0)
    };
    let stop = state.t().user_shell_stop;
    let stop_label = format!("✕ {stop}");
    let stop_w = UnicodeWidthStr::width(stop_label.as_str()) as u16;
    let background = state.t().user_shell_background;
    let background_label = format!("↗ {background}");
    let background_w = UnicodeWidthStr::width(background_label.as_str()) as u16;
    let separator = " · ";
    let separator_w = UnicodeWidthStr::width(separator) as u16;
    let controls_w = background_w + separator_w + stop_w;
    // Time-based, not frame-based: a burst of output repaints many frames and
    // must not make the spinner spin faster.
    let spinner = if state.live_animation {
        crate::status_line::spinner_frame((state.motion_clock.as_millis() / 120) as u64)
    } else {
        "◆"
    };
    let command = shell
        .command
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let fixed = 1 + 1 + 1 + 1 + UnicodeWidthStr::width(elapsed.as_str()) + 3;
    let room = (area.width as usize)
        .saturating_sub(fixed + controls_w as usize + 3)
        .max(4);
    let command = crate::render::truncate_display(&command, room);
    let line = Line::from(vec![
        Span::styled(
            format!("{spinner} "),
            Style::default().fg(theme.shell.running),
        ),
        Span::styled(command, Style::default().fg(theme.text.primary)),
        Span::styled(
            format!("… {elapsed}"),
            Style::default().fg(theme.text.muted),
        ),
    ]);
    let paragraph = Paragraph::new(vec![line]);
    frame.render_widget(paragraph, area);
    if controls_w + 2 <= area.width {
        let x = area
            .x
            .saturating_add(area.width.saturating_sub(controls_w).saturating_sub(1));
        let style = Style::default().fg(theme.text.secondary);
        let buf = frame.buffer_mut();
        buf.set_stringn(x, area.y, &background_label, background_w as usize, style);
        state.shell_live_background_rect = Some((x, area.y, background_w, 1));
        let separator_x = x.saturating_add(background_w);
        buf.set_stringn(
            separator_x,
            area.y,
            separator,
            separator_w as usize,
            Style::default().fg(theme.text.muted),
        );
        let stop_x = separator_x.saturating_add(separator_w);
        buf.set_stringn(stop_x, area.y, &stop_label, stop_w as usize, style);
        state.shell_live_stop_rect = Some((stop_x, area.y, stop_w, 1));
    } else {
        state.shell_live_stop_rect = None;
        state.shell_live_background_rect = None;
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
    let goal_range = crate::active_goal::has_visible_goal(state).then_some((start, width));
    (Line::from(spans), goal_range)
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

// ── State strip (runtime status + plan indicator, one row) ──────────────────

/// Columns between two painted cells of the strip.
///
/// Three cells either way: ` · ` when the runtime side has been squeezed to
/// its bare label (a narrow row reads as one line better with a separator),
/// and plain spacing when the runtime still carries its own metrics. The
/// constant width keeps every later cell's x position stable.
const STRIP_GAP: usize = 3;

/// Between the plan's progress and its current step / state word.
const STRIP_DETAIL_SEP: &str = " · ";

/// The plan's entry to the full Plan page, including its leading space.
const STRIP_ARROW: &str = " ↗";

/// Below this the runtime's live state stops being readable; every claim
/// leaves at least this much for it.
const STRIP_LABEL_FLOOR: usize = 6;

/// What a painted strip cell opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripTarget {
    Background,
    Issue,
    Plan,
}

/// One painted cell of the strip: its spans, its width, and what it opens.
struct StripItem {
    target: StripTarget,
    spans: Vec<Span<'static>>,
    width: usize,
}

/// The state strip: `◉ 命令运行中 · 15s · 20 次工具 · 后台 2 ↗ · ⚠ 后台失败 1 ↗ · ● 计划 3/4 ↗`.
///
/// One row, one paint, four cells with independent data and interaction:
/// runtime status (left, keeps its own rendering and animation), then the
/// background indicator, the issue indicator and the plan indicator — each a
/// real fact from its own owner, each publishing its painted range as a hit
/// region. Nothing here merges the underlying state; the strip only decides
/// who gets columns.
///
/// Claims follow the product's priority: a blocking issue, the runtime's live
/// state, active issues, the plan's progress and entry, the background entry,
/// then the runtime's secondary numbers. A claim either fits or is skipped
/// (the issue and plan degrade within their own claim first), so the row never
/// wraps and a later cell never paints over an earlier one.
fn render_state_strip(frame: &mut Frame, area: Rect, state: &mut AppState) {
    if area.height == 0 || area.width == 0 {
        state.background_hit = None;
        state.issue_hit = None;
        state.plan_hit = None;
        return;
    }
    let width = area.width as usize;
    let live = state.is_busy();

    // Facts, each from its owner.
    let running = crate::activity::running_background_count(state);
    let issues = crate::issue::issue_summary(state);
    let indicator = state
        .plan
        .as_ref()
        .filter(|plan| plan_indicator_should_show(plan, live))
        .map(|plan| plan_strip(state, plan, live));

    // The runtime label is measured first and forms every later budget:
    // statistics are negotiable, the live state word is not.
    let label = crate::status_line::status_strip_line(state, width, false);
    let label_natural_w = label.width();
    let label_present = label.spans.iter().any(|span| !span.content.is_empty());
    let label_floor = if label_present {
        label_natural_w.min(STRIP_LABEL_FLOOR)
    } else {
        0
    };
    // `committed` counts the claimed cells plus the gaps around them (one
    // before the first claim — the label's own gap — and one between cells).
    // The label's floor is checked on top of it, so no claim can squeeze the
    // runtime's live state out.
    let mut committed = 0usize;
    let mut claimed: Vec<StripItem> = Vec::new();
    let claim = |item: StripItem, committed: &mut usize, claimed: &mut Vec<StripItem>| {
        let gap = if claimed.is_empty() && !label_present {
            0
        } else {
            STRIP_GAP
        };
        if label_floor + *committed + gap + item.width <= width {
            *committed += gap + item.width;
            claimed.push(item);
            true
        } else {
            false
        }
    };

    // 1. Blocking issue. It outranks everything: the label is truncated to
    //    whatever survives, and only when even a few columns are impossible
    //    does it retreat to its count form.
    let severe_claimed = issues
        .severe
        .as_deref()
        .and_then(|severe| severe_strip(state, &issues, severe, width.saturating_sub(label_floor)))
        .is_some_and(|item| claim(item, &mut committed, &mut claimed));
    // 2. Active issues (when no blocking one owns the cell): the active issue
    //    outranks the plan. Labelled when it fits, the count form otherwise. A
    //    seen issue is exactly as active as an unread one — only its marker
    //    differs, and that lives on the page.
    if !severe_claimed && !issues.is_empty() {
        // Claim first, degrade on failure: the labelled form is offered to the
        // budget itself, never predicted (a prediction that ignores the gap
        // let the labelled cell win the choice and then fail the claim,
        // leaving the issues unstated while the background entry took the
        // columns). A blocking issue never wears a counting label.
        let placed = !issues.severe_active
            && claim(
                issue_strip(state, &issues, true),
                &mut committed,
                &mut claimed,
            );
        if !placed {
            claim(
                issue_strip(state, &issues, false),
                &mut committed,
                &mut claimed,
            );
        }
    }
    // 3. The plan's progress and entry (`● 计划 k/n ↗`) — its floor, claimed
    //    before the plan's detail and before the background entry.
    let plan_pos = indicator.as_ref().and_then(|ind| {
        let item = plan_floor_strip(state, ind);
        claim(item, &mut committed, &mut claimed).then(|| claimed.len() - 1)
    });
    // 4. Background entry (running tasks only): an entry, not a report.
    if running > 0 {
        claim(
            background_strip(state, running),
            &mut committed,
            &mut claimed,
        );
    }

    // The label takes what is left after every claim; the plan's detail then
    // refills from THAT leftover, and the runtime's statistics from what the
    // plan leaves. Both are the strip's last two priorities.
    let label_budget = width.saturating_sub(committed);
    let label_line = crate::status_line::status_strip_line(state, label_budget, false);
    let label_w = label_line.width();
    let plan_item = plan_pos.and_then(|pos| {
        let ind = indicator.as_ref()?;
        let floor_w = claimed[pos].width;
        let budget = label_budget.saturating_sub(label_w) + floor_w;
        let (line, width) = fit_plan_strip(state, ind, budget)?;
        Some(StripItem {
            target: StripTarget::Plan,
            spans: line.spans,
            width,
        })
    });
    let plan_extra = plan_item
        .as_ref()
        .zip(plan_pos)
        .map(|(item, pos)| item.width.saturating_sub(claimed[pos].width))
        .unwrap_or(0);
    // `label_budget` is the label's own pool (width minus the claimed cells):
    // the plan's detail and the runtime's statistics share only what the label
    // does not use, and the label is never squeezed below its own budget.
    let runtime =
        crate::status_line::status_strip_line(state, label_budget.saturating_sub(plan_extra), true);
    let runtime_w = runtime.width();

    // Assemble in reading order: runtime, background, issues, plan.
    let mut spans = runtime.spans;
    let mut hits: Vec<(StripTarget, u16, u16)> = Vec::new();
    let mut cursor = runtime_w;
    let mut first = true;
    for target in [
        StripTarget::Background,
        StripTarget::Issue,
        StripTarget::Plan,
    ] {
        let item = match target {
            StripTarget::Plan => plan_item.as_ref(),
            _ => claimed.iter().find(|item| item.target == target),
        };
        let Some(item) = item else {
            continue;
        };
        if cursor > 0 || !first {
            let separator = if first && runtime_w > label_w {
                " ".repeat(STRIP_GAP)
            } else {
                " · ".to_string()
            };
            spans.push(Span::raw(separator));
            cursor += STRIP_GAP;
        }
        let x0 = area.x.saturating_add(cursor as u16);
        let x1 = x0.saturating_add(item.width as u16);
        hits.push((item.target, x0, x1));
        spans.extend(item.spans.iter().cloned());
        cursor += item.width;
        first = false;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    // Hits are the PAINTED ranges, one per cell: a click can never open the
    // plan from under the runtime's numbers, nor the issues from the plan.
    let find = |target: StripTarget| {
        hits.iter()
            .find(|(kind, _, _)| *kind == target)
            .map(|(_, x0, x1)| (area.y, *x0, *x1))
    };
    state.background_hit = find(StripTarget::Background);
    state.issue_hit = find(StripTarget::Issue);
    state.plan_hit = find(StripTarget::Plan);
}

/// `后台 N ↗` — RUNNING background tasks only. A finished task is history (the
/// list page keeps it) and a failed one is an issue, never a count here.
///
/// The keyboard focus reads the same as the plan's: reversed while
/// `WorkbenchFocus::Background` owns the keys (Enter opens the jobs list).
fn background_strip(state: &AppState, running: usize) -> StripItem {
    let mut ink = Style::default().fg(state.theme.accent.primary);
    if state.workbench_focus == crate::state::WorkbenchFocus::Background {
        ink = ink.add_modifier(Modifier::REVERSED);
    }
    let label = state
        .t()
        .background_running_count
        .replace("{}", &running.to_string());
    let text = format!("{label}{STRIP_ARROW}");
    let width = UnicodeWidthStr::width(text.as_str());
    StripItem {
        target: StripTarget::Background,
        spans: vec![Span::styled(text, ink)],
        width,
    }
}

/// The issue cell in its two non-blocking forms: `⚠ 后台失败 N ↗` names the
/// kind and its count, and the bare `⚠ N ↗` is the compact form when the
/// labelled one does not fit. Every form counts real active issues; none of
/// them clears anything. (A blocking issue owns its own cell — see
/// [`severe_strip`] — because "问题 3" would hide which failure it is.)
fn issue_strip(state: &AppState, summary: &crate::issue::IssueSummary, labeled: bool) -> StripItem {
    let t = state.t();
    let (glyph, ink) = if summary.severe_active {
        ("✕ ", Style::default().fg(state.theme.status.error))
    } else {
        ("⚠ ", Style::default().fg(state.theme.status.warning))
    };
    let body = if !labeled {
        summary.total.to_string()
    } else {
        t.background_failed_only
            .replace("{}", &summary.total.to_string())
    };
    let text = format!("{glyph}{body}");
    let width = UnicodeWidthStr::width(text.as_str()) + UnicodeWidthStr::width(STRIP_ARROW);
    StripItem {
        target: StripTarget::Issue,
        spans: vec![Span::styled(text, ink), Span::styled(STRIP_ARROW, ink)],
        width,
    }
}

/// The BLOCKING issue's cell: `✕ <failure summary> ↗`, truncated to the room
/// it is offered, and only when even a truncated label cannot be stated does
/// it retreat to `✕ N ↗`. `None` when not even that fits — at which point the
/// runtime label owns the row. The label is the transcript failure block's own
/// summary: one fact, one wording.
fn severe_strip(
    state: &AppState,
    summary: &crate::issue::IssueSummary,
    severe: &str,
    room: usize,
) -> Option<StripItem> {
    let ink = Style::default().fg(state.theme.status.error);
    let head = "✕ ";
    // The named failure alone hides how many OTHER issues are active; a
    // `+N` suffix keeps the count without stealing the line. `N` is real
    // (the active list's length), never a repeat of the same fact.
    let rest = summary.total.saturating_sub(1);
    let suffix = if rest > 0 {
        format!(" +{rest}")
    } else {
        String::new()
    };
    let chrome = UnicodeWidthStr::width(head)
        + UnicodeWidthStr::width(STRIP_ARROW)
        + UnicodeWidthStr::width(suffix.as_str());
    let avail = room.saturating_sub(chrome);
    if avail >= 2 {
        let text = format!(
            "{head}{}{suffix}",
            crate::render::truncate_display(severe, avail)
        );
        let width = UnicodeWidthStr::width(text.as_str()) + UnicodeWidthStr::width(STRIP_ARROW);
        return Some(StripItem {
            target: StripTarget::Issue,
            spans: vec![Span::styled(text, ink), Span::styled(STRIP_ARROW, ink)],
            width,
        });
    }
    let text = format!("{head}{}", summary.total);
    let width = UnicodeWidthStr::width(text.as_str()) + UnicodeWidthStr::width(STRIP_ARROW);
    (width <= room).then(|| StripItem {
        target: StripTarget::Issue,
        spans: vec![Span::styled(text, ink), Span::styled(STRIP_ARROW, ink)],
        width,
    })
}

/// The plan's progress and entry at its floor form: glyph, `计划 k/n`, arrow —
/// no step name. Claimed as a floor so the plan's own detail never decides
/// whether the entry exists.
fn plan_floor_strip(state: &AppState, ind: &PlanStrip) -> StripItem {
    let budget = plan_core_width(ind) + UnicodeWidthStr::width(STRIP_ARROW);
    let (line, width) =
        fit_plan_strip(state, ind, budget).expect("core + arrow always fits its own budget");
    StripItem {
        target: StripTarget::Plan,
        spans: line.spans,
        width,
    }
}

fn plan_core_width(ind: &PlanStrip) -> usize {
    UnicodeWidthStr::width(ind.glyph) + 1 + UnicodeWidthStr::width(ind.core.as_str())
}

/// Fit the plan indicator into `budget` columns, degrading in priority order:
/// progress → arrow → step detail (which truncates before it drops).
///
/// `None` when not even `计划 k/n` fits: a clipped glyph would say nothing the
/// reader can use, and the runtime's state outranks the plan outright.
fn fit_plan_strip(
    state: &AppState,
    ind: &PlanStrip,
    budget: usize,
) -> Option<(Line<'static>, usize)> {
    let focused = state.workbench_focus == crate::state::WorkbenchFocus::Plan;
    let paint = |spans: Vec<Span<'static>>| -> Vec<Span<'static>> {
        if focused {
            spans
                .into_iter()
                .map(|span| {
                    Span::styled(
                        span.content.into_owned(),
                        span.style.add_modifier(Modifier::REVERSED),
                    )
                })
                .collect()
        } else {
            spans
        }
    };
    let glyph = format!("{} ", ind.glyph);
    let core_w = UnicodeWidthStr::width(glyph.as_str()) + UnicodeWidthStr::width(ind.core.as_str());
    if core_w > budget {
        return None;
    }
    let core_spans = || {
        vec![
            Span::styled(glyph.clone(), Style::default().fg(ind.glyph_color)),
            Span::styled(
                ind.core.clone(),
                Style::default()
                    .fg(state.theme.plan.primary)
                    .add_modifier(Modifier::BOLD),
            ),
        ]
    };
    let arrow_span = || Span::styled(STRIP_ARROW, Style::default().fg(state.theme.plan.primary));
    let arrow_w = UnicodeWidthStr::width(STRIP_ARROW);
    // Full form: detail + arrow, the detail truncated to what is left after
    // the parts that outrank it. A sliver under two columns is not a step
    // name, so the detail drops instead.
    if let Some(detail) = ind.detail.as_deref() {
        let fixed = core_w + UnicodeWidthStr::width(STRIP_DETAIL_SEP) + arrow_w;
        let room = budget.saturating_sub(fixed);
        if room >= 2 {
            let text = crate::render::truncate_display(detail, room);
            let used = fixed + UnicodeWidthStr::width(text.as_str());
            let mut spans = core_spans();
            spans.push(Span::styled(
                format!("{STRIP_DETAIL_SEP}{text}"),
                Style::default().fg(ind.detail_color),
            ));
            spans.push(arrow_span());
            return Some((Line::from(paint(spans)), used));
        }
    }
    // The arrow outranks the detail: the entry to the full plan survives a
    // step name that does not.
    if core_w + arrow_w <= budget {
        let mut spans = core_spans();
        spans.push(arrow_span());
        return Some((Line::from(paint(spans)), core_w + arrow_w));
    }
    Some((Line::from(paint(core_spans())), core_w))
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
    /// Below this the roster shows the role alone so that ` · … ↗` still fits.
    const NARROW_ROSTER_WIDTH: usize = 56;
    const META_GAP: usize = 4;
    let bound = width.min(ROSTER_MAX_WIDTH);
    // Below this the long label (`探索 Agent · Euclid · `) leaves no room for
    // ` · … ↗`; the row keeps the detail affordance and shows the role alone.
    let narrow = bound < NARROW_ROSTER_WIDTH;
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
        let label = if narrow {
            &row.narrow_label
        } else {
            &row.label
        };
        let head = format!("  {branch} {label} · ");
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
        // The meta column aligns across rows while there is room. When there is
        // not, the row keeps its own meta reachable rather than letting the
        // shared column push ` · … ↗` past the panel edge — a wider label (the
        // role it now carries) must not cost the reader the detail affordance.
        let meta_w = UnicodeWidthStr::width(format!(" · {meta} ↗").as_str());
        let aligned = meta_x.saturating_sub(head_w + activity_w);
        let reachable = bound.saturating_sub(head_w + activity_w + meta_w);
        let pad = aligned.min(reachable);
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
        // The overflow row is the roster's one way to the full list: the panel
        // answers "what is running right now" and cannot grow, so the children
        // it cannot paint must stay reachable from here.
        let mut hit = hits.clone();
        hit.push((
            area.y + 1 + shown_children as u16,
            crate::activity::ActivityId::Team,
        ));
        hits = hit;
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{MEMBER_INDENT}{}",
                    t.agent_roster_more.replace("{n}", &hidden.to_string())
                ),
                Style::default().fg(theme.text.secondary),
            ),
            Span::styled(
                format!(" {}", t.team_page_open_hint),
                Style::default().fg(theme.accent.primary),
            ),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), area);
    hits
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

/// Rows of the composer box that fit in `height`, and how many leading rows
/// were dropped.
///
/// The box is a top border, the content rows, then the bottom border. A short
/// slot used to keep the prefix (`take(height)`), which deletes the bottom
/// border and leaves the footer painted through an open frame. Keep the tail
/// so the closing border survives.
fn clip_composer_box(lines: Vec<Line<'static>>, height: usize) -> (Vec<Line<'static>>, usize) {
    if height == 0 {
        return (Vec::new(), 0);
    }
    if lines.len() <= height {
        return (lines, 0);
    }
    let skip = lines.len() - height;
    let shown: Vec<Line<'static>> = lines.into_iter().skip(skip).collect();
    (shown, skip)
}

fn render_input(frame: &mut Frame, area: Rect, state: &mut AppState) {
    state.input_rect = Some((area.x, area.y, area.width, area.height));
    let (lines, (cx, cy)) = composer_box_lines(state, area.width as usize);
    let (shown, skip) = clip_composer_box(lines, area.height as usize);
    let cy = cy.saturating_sub(skip as u16);
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
        return;
    }
    let width = area.width as usize;
    let dim = Style::default().fg(state.theme.text.secondary);
    // Breathing room between the hints, the usage chips and the clock.
    const GAP: usize = 2;

    let clock = state.clock_label.clone();
    let clock_w = UnicodeWidthStr::width(clock.as_str());
    // The composer chip is `width - 6` on this same chrome slot. Once that
    // budget cannot keep both permission and the collaboration axis, context
    // and key hints would outrank them, so they leave together.
    let show_secondary = crate::status_line::status_control_fields_fit(
        state,
        (area.width as usize).saturating_sub(6),
    );
    let usage = show_secondary
        .then(|| crate::status_line::footer_usage_line(state, width.saturating_sub(clock_w + GAP)))
        .flatten();
    let usage_w = usage.as_ref().map(Line::width).unwrap_or(0);

    // Place from the right: clock, then usage chips. Background work is NOT
    // here: the state strip's background and issue cells are its one place,
    // and a second summary on this row is what made a long command squat on
    // the footer.
    let mut cursor = width;
    let clock_start = if clock_w > 0 && clock_w <= cursor {
        cursor -= clock_w;
        Some(cursor)
    } else {
        None
    };
    let usage_start = if usage_w > 0 && cursor >= GAP + usage_w {
        cursor -= GAP + usage_w;
        Some(cursor)
    } else {
        None
    };
    let any_right = clock_start.is_some() || usage_start.is_some();
    let left_limit = if any_right {
        cursor.saturating_sub(GAP)
    } else {
        width
    };

    if show_secondary
        && left_limit > 0
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
    if usage_start.is_some()
        && let Some(text) = usage
    {
        let start = usage_start.unwrap_or(0);
        frame.render_widget(
            Paragraph::new(text),
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

    #[test]
    fn a_short_composer_slot_keeps_the_bottom_border() {
        let lines = vec![
            Line::from("TOP"),
            Line::from("CONTENT"),
            Line::from("BOTTOM"),
        ];
        let (shown, skip) = clip_composer_box(lines, 2);
        assert_eq!(skip, 1, "drop the top border, not the closing one");
        assert_eq!(line_text(&shown[0]), "CONTENT");
        assert_eq!(line_text(&shown[1]), "BOTTOM");
    }

    #[test]
    fn a_composer_slot_that_fits_drops_nothing() {
        let lines = vec![Line::from("TOP"), Line::from("BOTTOM")];
        let (shown, skip) = clip_composer_box(lines, 4);
        assert_eq!(skip, 0);
        assert_eq!(shown.len(), 2);
        assert_eq!(line_text(&shown[1]), "BOTTOM");
    }

    #[test]
    fn a_zero_height_composer_slot_draws_nothing() {
        let (shown, skip) = clip_composer_box(vec![Line::from("TOP")], 0);
        assert!(shown.is_empty());
        assert_eq!(skip, 0);
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

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
                model_notice: None,
                thinking: None,
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
                model_notice: None,
                thinking: None,
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
                model_notice: None,
                thinking: None,
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
                model_notice: None,
                thinking: None,
            },
        );
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.elapsed_secs = elapsed;
        // A used context so the `Context …` footer chip actually renders.
        state.token_input = 59_000;
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

    /// The full workbench paints the runtime status and the plan indicator on
    /// ONE row, and that row is the same row the runtime alone would use: the
    /// plan rides the existing strip instead of costing the conversation
    /// another line.
    #[test]
    fn the_full_workbench_paints_one_state_strip_row() {
        let with = render_workbench(
            100,
            34,
            crate::i18n::Locale::En,
            15,
            crate::multi_agent::TaskTeamView::default(),
            true,
        );
        let without = render_workbench(
            100,
            34,
            crate::i18n::Locale::En,
            15,
            crate::multi_agent::TaskTeamView::default(),
            false,
        );
        let plan_row = row_of(&with, "plan 1/3");
        assert_eq!(
            with.iter().filter(|l| l.contains("plan 1/3")).count(),
            1,
            "plan progress appears exactly once:\n{}",
            with.join("\n")
        );
        assert!(
            with[plan_row].contains("waiting for model"),
            "runtime status and plan indicator share the row: {:?}",
            with[plan_row]
        );
        let runtime_only_row = row_of(&without, "waiting for model");
        assert_eq!(
            plan_row, runtime_only_row,
            "the plan must not push the strip to another row"
        );
        // Composer and footer keep their rows with and without the plan.
        assert_eq!(
            row_of(&with, "Type a message"),
            row_of(&without, "Type a message")
        );
        // And nothing else on screen repeats the fraction.
        assert_eq!(without.iter().filter(|l| l.contains("plan 1/3")).count(), 0);
    }

    /// The merged strip survives a narrow terminal inside the FULL workbench:
    /// both sides stay on one row and the composer keeps its full width.
    #[test]
    fn a_narrow_workbench_keeps_the_merged_strip_inside_the_row() {
        let lines = render_workbench(
            60,
            24,
            crate::i18n::Locale::En,
            15,
            crate::multi_agent::TaskTeamView::default(),
            true,
        );
        let strip_row = row_of(&lines, "plan 1/3");
        let strip = &lines[strip_row];
        assert!(
            strip.contains("waiting for model") || strip.contains('⠋'),
            "runtime state survives: {strip:?}"
        );
        assert!(
            UnicodeWidthStr::width(strip.trim_end()) <= 60,
            "the strip never overflows the terminal: {strip:?}"
        );
        // The plan is above the composer, and the composer is intact.
        let input_row = row_of(&lines, "Type a message");
        assert!(strip_row < input_row);
        assert!(
            lines[input_row..].iter().take(3).any(|l| l.contains('╰')),
            "the composer's closing border survives the narrow strip:\n{}",
            lines.join("\n")
        );
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
        let footer_row = row_of(&lines, "29%/200k");
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
                model_notice: None,
                thinking: None,
            },
        );
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.elapsed_secs = 55;
        state.token_input = 59_000;
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
        let footer_row = row_of(&lines, "29%/200k");
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
                model_notice: None,
                thinking: None,
            },
        );
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.elapsed_secs = 55;
        state.token_input = 59_000;
        state.team = team_with_usage(&[("a1", "explorer", "read_file", 158_000)]);
        state.overlay = Some(crate::overlay::Overlay::Approval(Box::new(
            crate::overlay::ApprovalOverlay::new(UiApprovalRequest {
                grant: None,
                requires_human_consent: false,
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
        let footer_row = row_of(&lines, "29%/200k");
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
        let footer_row = row_of(&lines, "29%/200k");
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

    /// The agent ROSTER row whose caption is `caption`.
    ///
    /// The caption cell carries the child's spawn title (or its state), so the
    /// caller names the row with the same text the panel paints. Matching on
    /// "any row with a detail affordance" would silently assert against the
    /// wrong child as soon as a team has more than one.
    fn roster_row<'a>(lines: &'a [String], caption: &str) -> &'a String {
        lines
            .iter()
            .find(|l| l.contains(caption) && l.contains('↗'))
            .unwrap_or_else(|| panic!("no roster row for {caption}: {lines:#?}"))
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
        let row = roster_row(&lines, "explorer task");
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
        let row = roster_row(&reported, "explorer task");
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
        let row = roster_row(&lines, "explorer task");
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
        let row = roster_row(&wide, "explorer task");
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
                // The spawn title and the brief are DIFFERENT strings here on
                // purpose: a fixture that sets both to the same text cannot
                // tell which one the panel actually painted.
                title: Some(format!("{role} task")),
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
        // The panel states the runtime's spawn title. It must NOT paint the
        // brief: a spawn prompt is an instruction, not a caption for a state.
        assert!(screen.contains("explorer task"), "{screen}");
        assert!(screen.contains("worker task"), "{screen}");
        assert!(
            !screen.contains("analyzing repository structure")
                && !screen.contains("implementing the change"),
            "a spawn prompt must never be painted as a caption: {screen}"
        );
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

    fn screen_rows(state: &mut AppState, width: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, state))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        buf.cell((x, y))
                            .and_then(|cell| cell.symbol().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn short_transcript_first_message_stays_at_top_through_thinking_and_resize() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut state = test_state();
        state.transcript.push_user("stable first question".into());
        state.conv.auto_scroll = true;
        for (stage, width, height) in [
            ("first", 80, 24),
            ("live", 80, 24),
            ("done", 80, 24),
            ("resized", 100, 32),
            ("narrowed", 60, 28),
        ] {
            if stage == "live" {
                state.transcript.append_thought(
                    "first reasoning line\nsecond reasoning line\nthird reasoning line",
                );
            } else if stage == "done" {
                state.transcript.finish_thought(Some(400));
            }
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| crate::render::render(frame, &mut state))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let rows: Vec<String> = (0..height)
                .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            let first_row = rows
                .iter()
                .position(|row| row.contains("stable first question"))
                .unwrap();
            let (_, top, _, _) = state.conv.rect.unwrap();
            if stage == "live" {
                assert!(
                    rows.iter().any(|row| row.contains("reasoning line")),
                    "a live Thought paints while the turn runs: {rows:#?}"
                );
            } else if stage == "done" {
                assert!(!rows.iter().any(|row| row.contains("reasoning line")));
            }
            // The band is exactly as tall as the message: the text opens on
            // the first row of the viewport.
            assert_eq!(
                first_row,
                top as usize,
                "{stage}: first message must start at viewport top:\n{}",
                rows.join("\n")
            );
            let (x, _, _, _) = state.conv.rect.unwrap();
            assert_eq!(
                crate::conversation::geometry::screen_to_content(&state, x, first_row as u16)
                    .unwrap()
                    .row,
                0,
                "the text row is the prompt's first body row"
            );
        }
    }

    #[test]
    #[ignore = "manual short conversation visual evidence"]
    fn short_transcript_top_visual_evidence() {
        let dir = std::path::Path::new("/tmp/leveler-top-preview");
        std::fs::create_dir_all(dir).unwrap();
        let mut state = test_state();
        state.transcript.push_user("stable first question".into());
        state.conv.auto_scroll = true;
        for stage in ["first", "live", "done"] {
            if stage == "live" {
                state.transcript.append_thought(
                    "first reasoning line\nsecond reasoning line\nthird reasoning line",
                );
            } else if stage == "done" {
                state.transcript.finish_thought(Some(400));
            }
            std::fs::write(
                dir.join(format!("{stage}.txt")),
                screen_rows(&mut state, 80).join("\n"),
            )
            .unwrap();
        }
    }

    fn status_fixture(collaboration: &str, busy: bool) -> AppState {
        let mut state = test_state();
        state.model_label = "deepseek/deepseek-v4-flash".into();
        state.thinking = Some(leveler_client_protocol::UiThinkingState {
            configured: leveler_model::ThinkingLevel::Max,
            session_override: None,
            current: leveler_model::ThinkingLevel::Max,
            effective: leveler_model::ThinkingLevel::Max,
            access: leveler_client_protocol::UiThinkingAccess::Adjustable,
            choices: vec![leveler_model::ThinkingLevel::Max],
        });
        state.mode_label = "Assisted".into();
        state.collaboration = collaboration.into();
        state.goal_mode_active = collaboration == "goal";
        state.context_window_tokens = 1_000_000;
        state.token_input = 11_000;
        state.token_input = 11_000;
        state.token_cached = 9_570;
        state.clock_label = "09:30".into();
        if busy {
            state.status = leveler_client_protocol::RuntimeStatus::Busy;
            state.activity = Some("思考".into());
            state.tick = 0;
        }
        state
    }

    /// Narrow terminals drop context, cache and key hints before collaboration
    /// or permission. A busy row keeps a real activity word at every width.
    #[test]
    fn narrow_status_keeps_axis_and_permission_ahead_of_context() {
        for width in [120u16, 80, 60, 48, 40, 32, 28, 24, 20] {
            for busy in [false, true] {
                let mut state = status_fixture("goal", busy);
                let rows = screen_rows(&mut state, width);
                let screen = rows.join("\n");
                let axis = screen.contains("goal");
                let permission = screen.contains("auto");
                // Wide glyphs occupy two cells, so the joined row reads
                // `上 下 文`. The ASCII usage figures stay contiguous.
                let context = screen.contains("11k/1M") || screen.contains("87%");
                let hint = screen.contains("Shift+Tab") || screen.contains("Ctrl+C");
                if context || hint {
                    assert!(
                        axis && permission,
                        "width {width} busy {busy} dropped a control while keeping secondary chrome:\n{screen}"
                    );
                }
                if busy {
                    assert!(
                        screen.contains('思') || screen.contains('考'),
                        "width {width} busy row has no activity label:\n{screen}"
                    );
                }
                if width >= 48 && busy {
                    assert!(
                        axis && permission,
                        "width {width} must keep activity, axis and permission:\n{screen}"
                    );
                }
                if width == 40 {
                    assert!(
                        axis && permission,
                        "width 40 must keep the axis and permission ahead of context:\n{screen}"
                    );
                }
                if width == 20 && busy {
                    assert!(
                        !context && !hint,
                        "width 20 must drop context and hints before the axis:\n{screen}"
                    );
                }
            }
        }

        let mut chat = status_fixture("chat", false);
        let screen = screen_rows(&mut chat, 80).join("\n");
        assert!(screen.contains("chat"), "{screen}");
        assert!(
            !screen.contains("goal"),
            "chat must not paint the goal axis:\n{screen}"
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
        state.token_input = 15_000;
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
            .position(|l| l.contains("1%/1M"))
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
        state.token_input = 254_000;
        state.token_cached = 251_460;
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
            .find(|l| l.contains("24%/1M"))
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
                model_notice: None,
                thinking: None,
            },
        );
        state.clock_label = clock.into();
        if usage {
            state.token_input = 18_400;
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

    #[test]
    fn footer_context_bar_tracks_usage_color_and_yields_before_numbers() {
        use ratatui::{Terminal, backend::TestBackend};
        for (used, expected_color) in [
            (2_000, 0),
            (138_000, 0),
            (140_000, 1),
            (178_000, 1),
            (180_000, 2),
            (220_000, 2),
        ] {
            let mut state = test_state();
            state.theme = crate::theme::Theme::default();
            state.locale = crate::i18n::Locale::En;
            state.token_input = used;
            state.token_cached = used / 100 * 99;
            state.clock_label = "09:30".into();
            let color = match expected_color {
                0 => state.theme.status.info,
                1 => state.theme.status.warning,
                _ => state.theme.status.error,
            };
            for width in [100, 30] {
                let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
                terminal
                    .draw(|frame| render_footer(frame, frame.area(), &mut state))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let text = (0..width)
                    .map(|x| buffer[(x, 0)].symbol())
                    .collect::<String>();
                assert!(text.contains(&format!("{}%/200k", used / 2_000)), "{text}");
                assert!(text.contains("cache 99%"), "{text}");
                assert!(!text.contains("上下文"), "{text}");
                if width == 100 {
                    let bar = (0..width)
                        .find(|&x| buffer[(x, 0)].symbol() == "█")
                        .expect("filled bar");
                    assert_eq!(buffer[(bar, 0)].fg, color, "{text}");
                } else {
                    assert!(!text.contains('█') && !text.contains('░'), "{text}");
                }
            }
        }
    }

    /// Writes real workbench frames for visual inspection, never production data.
    #[test]
    #[ignore = "manual footer visual evidence"]
    fn footer_context_visual_evidence() {
        use ratatui::{Terminal, backend::TestBackend};
        let dir = std::path::Path::new("/tmp/leveler-footer-preview");
        std::fs::create_dir_all(dir).unwrap();
        for (name, used, width) in [
            ("low", 2_000, 100),
            ("warning", 140_000, 100),
            ("high", 180_000, 100),
            ("narrow", 2_000, 30),
        ] {
            let mut state = test_state();
            state.theme = crate::theme::Theme::default();
            state.token_input = used;
            state.token_input = 1_000;
            state.token_cached = 990;
            state.clock_label = "09:30".into();
            let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
            terminal
                .draw(|frame| crate::render::render(frame, &mut state))
                .unwrap();
            let buf = terminal.backend().buffer();
            let mut output = String::new();
            for y in 0..20 {
                let mut x = 0;
                while x < width {
                    let symbol = buf[(x, y)].symbol();
                    output.push_str(symbol);
                    x += UnicodeWidthStr::width(symbol).max(1) as u16;
                }
                output.push('\n');
            }
            std::fs::write(dir.join(format!("{name}.txt")), output).unwrap();
        }
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

    /// Case 2/3 — the context bar is the first thing to yield on a narrow row;
    /// the numbers and the clock survive. The clock always survives.
    #[test]
    fn usage_bar_yields_before_the_numbers_when_the_row_is_narrow() {
        let wide = render_clock_case(100, 24, "09:30", true, false);
        let wide_row = row_of(&wide, "09:30");
        assert!(
            wide[wide_row].contains("9%/200k"),
            "usage shown when wide: {:?}",
            wide[wide_row]
        );
        assert!(
            wide[wide_row].contains('█') || wide[wide_row].contains('░'),
            "the bar is painted when the row has room: {:?}",
            wide[wide_row]
        );

        let narrow = render_clock_case(24, 24, "09:30", true, false);
        let nrow = row_of(&narrow, "09:30");
        assert!(
            !narrow[nrow].contains('█') && !narrow[nrow].contains('░'),
            "the bar yields on a narrow row: {:?}",
            narrow[nrow]
        );
        assert!(
            narrow[nrow].contains("9%/200k"),
            "the numbers outlive the bar: {:?}",
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
                if buf.cell((x, y)).is_some_and(|c| c.symbol() == "❯") {
                    first_marker_y = Some(y);
                    break;
                }
            }
            if first_marker_y.is_some() {
                break;
            }
        }
        // The band is exactly as tall as the message; the marker opens its
        // first body row at the content top.
        assert_eq!(
            first_marker_y,
            Some(cy),
            "scrolled-to-top prompt starts at the content top, not the header"
        );
        for x in 0..buf.area.width {
            let sym = buf.cell((x, cy - 1)).map(|c| c.symbol()).unwrap_or(" ");
            assert!(
                sym != "❯" && !sym.starts_with('R'),
                "row above content must stay empty, col {x} is {sym:?}"
            );
        }
        let bottom_pad = cy.saturating_add(ch);
        for x in 0..buf.area.width {
            let sym = buf.cell((x, bottom_pad)).map(|c| c.symbol()).unwrap_or(" ");
            let badge = sym.contains('↓')
                || sym.contains('行')
                || sym
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c.is_whitespace());
            assert!(
                sym.trim().is_empty() || badge,
                "row below content must stay empty, col {x} is {sym:?}"
            );
        }
    }

    #[test]
    fn conversation_prompt_card_starts_on_the_gutter_with_a_padded_marker() {
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
                if buf.cell((x, y)).is_some_and(|c| c.symbol() == "❯") {
                    marker_x = Some(x);
                    break;
                }
            }
        }
        assert_eq!(
            marker_x,
            Some(
                crate::conversation::geometry::GUTTER_X
                    + crate::render::transcript_lines::USER_PROMPT_HPAD as u16
            ),
            "the card starts at the gutter and pads its marker one cell inside"
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
                    sym.trim().is_empty()
                        || sym.contains('↓')
                        || sym.contains('行')
                        || sym
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c.is_whitespace()),
                    "col {x} row {y} leaked past content right {content_right}: {sym:?}"
                );
            }
        }
    }

    #[test]
    fn dark_automated_render_owns_surfaces() {
        assert_opaque_workbench(crate::theme::Theme::dark());
    }

    /// The scroll-to-bottom badge counts CONTENT LINES, so it must say so.
    /// `▼36` read as "36 new messages"; `↓ 36 行` cannot.
    #[test]
    fn scroll_badge_names_the_unit_as_lines() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = test_state();
        state.transcript.push_user("overflow ".repeat(400));
        state.conv.auto_scroll = false;
        state.conv.scroll = 0;
        state.conv.unread = 36;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut state))
            .unwrap();
        assert!(state.conv.scroll_bottom_rect.is_some(), "badge is visible");
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
        }
        assert!(text.contains('↓'), "badge keeps the scroll glyph: {text:?}");
        assert!(text.contains('行'), "badge names the line unit: {text:?}");
        assert!(!text.contains('▼'), "the ambiguous glyph is gone: {text:?}");
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
        state.repository = Some("/Users/me/Develop/app/codeleveler".into());
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
            state: Default::default(),
        }
    }

    /// The plan is the agent's declared progress: the indicator says how much
    /// it has declared done and names the running step — never the step's
    /// ordinal as a cursor that reads as the runtime tracking execution.
    #[test]
    fn a_live_plan_indicator_reports_declared_progress_without_a_cursor() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let ind = plan_strip(&state, &sample_plan(), true);
        assert_eq!(ind.glyph, "●");
        assert_eq!(ind.core, "计划 1/3");
        assert_eq!(ind.detail.as_deref(), Some("edit module"));
        assert!(!ind.core.contains("当前"), "no runtime cursor");
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
            title: Some("审计生命周期与身份".into()),
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
            title: Some("audit c".into()),
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
        // 96 columns is the roster's own bounded width; the affordance must fit
        // inside it rather than being cut off by the terminal.
        let wide = {
            use ratatui::Terminal;
            use ratatui::backend::TestBackend;
            let mut terminal = Terminal::new(TestBackend::new(96, height)).unwrap();
            terminal
                .draw(|f| {
                    render_team_panel(
                        f,
                        Rect {
                            x: 0,
                            y: 0,
                            width: 96,
                            height,
                        },
                        &state,
                    );
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        let wide_rows: String = (0..wide.area.height)
            .map(|y| {
                (0..wide.area.width)
                    .map(|x| {
                        wide.cell((x, y))
                            .and_then(|c| c.symbol().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            wide_rows.replace(' ', "").contains("查看全部"),
            "the overflow row names its own affordance: {wide_rows}"
        );
        // And it is a real target: the children the panel cannot paint must stay
        // reachable, which is the whole point of the row.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(70, height)).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|f| {
                hits = render_team_panel(
                    f,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 70,
                        height,
                    },
                    &state,
                );
            })
            .unwrap();
        let team_row = hits
            .iter()
            .find(|(_, id)| matches!(id, crate::activity::ActivityId::Team));
        assert!(
            team_row.is_some(),
            "the overflow row is a click target: {hits:?}"
        );
    }

    /// The Team page lists every child, not only the ones the roster had room
    /// for, and Enter on it opens the selected child's detail.
    #[test]
    fn the_team_page_lists_every_child_and_enter_opens_its_detail() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
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
        // The page as its own entry point leaves it: open, first child selected.
        // The click path into it is covered by the roster overflow test above.
        state.active_screen = crate::screen::Screen::Team;
        state.team_selected = state.team.children.first().map(|child| child.id.clone());
        assert_eq!(state.active_screen, crate::screen::Screen::Team);
        assert_eq!(
            state.team_selected.as_deref(),
            Some("c1"),
            "the page opens on the first child so Enter has a subject"
        );

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|f| crate::render::render(f, &mut state))
            .unwrap();
        let buf = terminal.backend().buffer();
        let screen: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        buf.cell((x, y))
                            .and_then(|c| c.symbol().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Every child is on the page, including the two the roster hid.
        for id in ["c1", "c2", "c3", "c4", "c5", "c6"] {
            assert!(
                state.team.children.iter().any(|c| c.id == id),
                "fixture sanity: {id}"
            );
        }
        let listed = state.screen_rows.len();
        assert_eq!(listed, 6, "all six children are on the page: {screen}");

        // Enter opens the selected child's Activity detail and keeps the work.
        let action = crate::action::Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        let effects = crate::reducer::reduce(&mut state, action);
        assert_eq!(state.active_screen, crate::screen::Screen::Activity);
        assert!(
            matches!(
                state.activity_open,
                Some(crate::activity::ActivityId::Child(ref id)) if id == "c1"
            ),
            "Enter opens the selected child: {:?}",
            state.activity_open
        );
        assert!(
            effects.iter().all(|e| !matches!(
                e,
                crate::action::Effect::Send(
                    leveler_client_protocol::ClientCommand::CancelChild { .. }
                )
            )),
            "opening a detail never cancels the child: {effects:?}"
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

    /// The plan indicator is a sibling of the runtime status on ONE row and
    /// starts at the same content baseline as every other workbench line.
    #[test]
    fn the_plan_indicator_shares_the_state_strip_row() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.plan = Some(sample_plan());
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        terminal
            .draw(|f| {
                // The strip slot as the workbench hands it over: the full row,
                // inset by the workspace gutter.
                let area = crate::layout::horizontal_inset(
                    Rect {
                        x: 0,
                        y: 0,
                        width: 60,
                        height: 6,
                    },
                    crate::layout::WORKSPACE_GUTTER_X,
                );
                render_state_strip(f, area, &mut state);
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
        let line = &rows[0];
        let line_col = line.len() - line.trim_start().len();
        assert_eq!(
            line_col,
            crate::layout::WORKSPACE_GUTTER_X as usize,
            "the strip sits on the content baseline: {line:?}"
        );
        assert!(line.contains('↗'), "{line:?}");
        assert!(
            rows.iter().skip(1).all(|row| row.trim().is_empty()),
            "runtime and plan share ONE row: {rows:?}"
        );
    }

    /// The indicator names the running step because the full list lives on its
    /// own page — and the arrow is the entry to it.
    #[test]
    fn the_plan_indicator_keeps_progress_and_the_detail_affordance() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let rows = strip_rows(&mut state, Some(sample_plan()), 100);
        let line = rows[0].trim_end();
        assert!(line.contains("● 计划 1/3 · edit module ↗"), "{line}");
        assert!(
            !line.contains("已完成 1/3"),
            "the fraction already carries the done count: {line}"
        );
    }

    /// Outside a running turn the plan is the last record of what the agent
    /// declared, not work under way: no "进行中" anywhere on the strip.
    #[test]
    fn a_plan_outside_a_running_turn_reads_as_the_last_record() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Idle;
        let rows = strip_rows(&mut state, Some(sample_plan()), 100);
        let line = rows[0].trim_end();
        assert!(line.contains("○ 计划 1/3 · 最后记录 ↗"), "{line}");
        assert!(!line.contains("进行中"), "{line}");
    }

    /// A pending step is not work under way, and the next pending description
    /// must never be promoted into the "current step" slot.
    #[test]
    fn the_plan_indicator_does_not_fabricate_an_active_item() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
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
            state: Default::default(),
        };
        let rows = strip_rows(&mut state, Some(plan), 100);
        let line = rows[0].trim_end();
        assert!(line.contains("● 计划 0/4 · 4 项待办"), "{line}");
        assert!(!line.contains("侦察仓库结构"), "{line}");
        assert!(
            !line.contains("进行中"),
            "pending is not in-progress: {line}"
        );
    }

    /// A plan the agent declared complete while the turn still runs reports
    /// exactly that — a green check, no breathing, progress at n/n.
    #[test]
    fn a_completed_plan_reads_done_and_stops_breathing() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.theme = crate::theme::Theme::dark();
        state.motion_clock = std::time::Duration::from_millis(600); // the dim end
        let buf = strip_buffer(&mut state, Some(completed_plan()), 100);
        let rows = strip_rows(&mut state, Some(completed_plan()), 100);
        let line = rows[0].trim_end();
        assert!(line.contains("✓ 计划 2/2 · 已完成 ↗"), "{line}");
        let glyph = (0..buf.area.width)
            .map(|x| buf[(x, 0)].clone())
            .find(|cell| cell.symbol() == "✓")
            .expect("completion glyph painted");
        assert_eq!(
            glyph.fg, state.theme.status.success,
            "a settled plan is static green, never a breathing purple"
        );
        assert_ne!(glyph.fg, state.theme.plan.breathe(0.0));
    }

    /// The running glyph breathes between the two purple shades on the UI's
    /// motion clock, and the rest of the indicator does not move with it.
    #[test]
    fn a_running_plan_glyph_breathes_between_the_two_purples() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.theme = crate::theme::Theme::dark();
        let ink = |state: &mut AppState, ms: u64| {
            state.motion_clock = std::time::Duration::from_millis(ms);
            let buf = strip_buffer(state, Some(running_plan(3, 1)), 100);
            (0..buf.area.width)
                .map(|x| buf[(x, 0)].clone())
                .find(|cell| cell.symbol() == "●")
                .expect("running glyph painted")
                .fg
        };
        let bright = ink(&mut state, 0);
        let dim = ink(&mut state, 600);
        assert_eq!(bright, state.theme.plan.primary);
        assert_eq!(dim, state.theme.plan.dim);
        assert_ne!(bright, dim, "the breath has to move");
        // The core ink stays the identity purple at both ends.
        state.motion_clock = std::time::Duration::from_millis(600);
        let buf = strip_buffer(&mut state, Some(running_plan(3, 1)), 100);
        let counts = (0..buf.area.width)
            .map(|x| buf[(x, 0)].clone())
            .find(|cell| cell.symbol() == "计")
            .expect("progress painted");
        assert_eq!(counts.fg, state.theme.plan.primary);
    }

    /// Keyboard focus still lands on the plan half of the strip: the plan
    /// spans reverse, the runtime's do not.
    #[test]
    fn plan_focus_reverses_only_the_plan_half() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.activity = Some("运行 cargo test".into());
        state.workbench_focus = crate::state::WorkbenchFocus::Plan;
        let buf = strip_buffer(&mut state, Some(sample_plan()), 100);
        let cells = strip_buffer_cells(&buf);
        let (_, x0, x1) = state.plan_hit.expect("plan hit published");
        for (x, (symbol, _)) in cells.iter().enumerate() {
            // A wide char owns two cells and only the first carries both the
            // glyph and its style; the continuation cell is painted by it.
            let continuation =
                x > 0 && UnicodeWidthStr::width(cells[x - 1].0.as_str()) == 2 && symbol == " ";
            if continuation {
                continue;
            }
            let reversed = buf[(x as u16, 0)].modifier.contains(Modifier::REVERSED);
            let in_plan = (x0 as usize..x1 as usize).contains(&x);
            assert_eq!(
                reversed, in_plan,
                "x={x} ({symbol:?}): focus reverses exactly the plan half"
            );
        }
    }

    /// With motion off (reduced-motion preference), the glyph is a static
    /// identity purple — same state, no animation.
    #[test]
    fn with_motion_off_the_plan_glyph_is_static() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.theme = crate::theme::Theme::dark();
        state.live_animation = false;
        let ink = |state: &mut AppState, ms: u64| {
            state.motion_clock = std::time::Duration::from_millis(ms);
            let buf = strip_buffer(state, Some(running_plan(3, 1)), 100);
            (0..buf.area.width)
                .map(|x| buf[(x, 0)].clone())
                .find(|cell| cell.symbol() == "●")
                .expect("running glyph painted")
                .fg
        };
        assert_eq!(ink(&mut state, 0), state.theme.plan.primary);
        assert_eq!(ink(&mut state, 600), state.theme.plan.primary);
    }

    /// The whole painted indicator — glyph through arrow — is the click/Enter
    /// target, and nothing else on the row is.
    #[test]
    fn the_plan_hit_covers_the_whole_painted_indicator() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let buf = strip_buffer(&mut state, Some(sample_plan()), 100);
        let line = strip_rows(&mut state, Some(sample_plan()), 100);
        let line = line[0].trim_end();
        let (row, x0, x1) = state.plan_hit.expect("plan hit published");
        assert_eq!(row, 0);
        let painted = painted_text(&buf, x0 as usize, x1 as usize);
        assert_eq!(painted, "● 计划 1/3 · edit module ↗", "{line}");
        let cells = strip_buffer_cells(&buf);
        assert!(
            cells[x1 as usize..].iter().all(|(c, _)| c == " "),
            "nothing painted past the hit: {cells:?}"
        );
        let runtime = painted_text(&buf, 0, x0 as usize - STRIP_GAP);
        assert!(
            !runtime.contains('●') && !runtime.contains('↗'),
            "the runtime side is outside the hit: {runtime:?}"
        );
    }

    /// The hit region is the plan's exact painted x-range at any width, so a
    /// click can never open the plan from the runtime side or miss the arrow.
    #[test]
    fn the_plan_hit_follows_the_row_at_every_width() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let cases = [
            (24u16, running_plan(4, 2)),
            (40, running_plan(4, 2)),
            (60, running_plan(4, 2)),
            (100, running_plan(4, 2)),
            (140, running_plan(4, 2)),
        ];
        for (width, plan) in cases {
            let buf = strip_buffer(&mut state, Some(plan), width);
            let Some((_, x0, x1)) = state.plan_hit else {
                panic!("w={width}: plan indicator is visible, so it must be clickable");
            };
            let painted = painted_text(&buf, x0 as usize, x1 as usize);
            assert!(
                painted.starts_with('●') && painted.contains("计划 2/4"),
                "w={width}: the hit starts at the glyph and holds the progress: {painted:?}"
            );
            assert!(
                ["步骤 3 ↗", "2/4 ↗", "2/4"]
                    .iter()
                    .any(|end| painted.ends_with(end)),
                "w={width}: the hit ends exactly where the painting ends: {painted:?}"
            );
            let cells = strip_buffer_cells(&buf);
            assert!(
                cells[x1 as usize..].iter().all(|(c, _)| c == " "),
                "w={width}: nothing painted past the hit: {cells:?}"
            );
            assert!(
                cells[..x0 as usize].iter().all(|(c, _)| !c.contains('●')),
                "w={width}: the glyph belongs to the plan, not the runtime: {cells:?}"
            );
        }
    }

    /// At a width where the runtime's secondary numbers no longer fit, the
    /// plan's progress, current step and arrow survive — statistics yield
    /// before the plan's step does, and nothing wraps.
    #[test]
    fn the_plan_outranks_runtime_statistics_when_the_row_is_tight() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.activity = Some("正在检查 WorkerConfig 并对齐执行路径".into());
        state.elapsed_secs = 15;
        state.turn_tool_calls = 20;
        state.token_input = 64_590;
        state.token_output = 716;
        let plan = running_plan(4, 2);
        // A row wide enough for both sides in full.
        let wide = strip_rows(&mut state, Some(plan.clone()), 120);
        let wide = wide[0].trim_end();
        assert!(wide.contains("20 次工具"), "{wide}");
        assert!(wide.contains("● 计划 2/4 · 步骤 3 ↗"), "{wide}");
        // A tighter row: the statistics are what yields, in their own order.
        let tight = strip_rows(&mut state, Some(plan.clone()), 60);
        let tight = tight[0].trim_end();
        assert!(tight.contains("计划 2/4"), "{tight}");
        assert!(tight.contains('↗'), "{tight}");
        assert!(
            !tight.contains("次工具"),
            "tool count yields to the plan: {tight}"
        );
        // Every width from cramped to full: one row, never wider than the
        // terminal, and never a bare animation glyph without a state word.
        // A long runtime label may own the whole narrow row (priority 1) —
        // what it may never do is crowd the plan into a meaningless stub.
        for width in 18..80u16 {
            let rows = strip_rows(&mut state, Some(plan.clone()), width);
            assert_eq!(rows.len(), 1, "w={width}: one row, always");
            let line = rows[0].trim_end();
            assert!(
                UnicodeWidthStr::width(line) <= width as usize,
                "w={width}: the strip may never overflow: {line:?}"
            );
            assert!(
                line.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "w={width}: the strip always carries a state word: {line:?}"
            );
            assert!(
                !line.contains('●') || line.contains("计划"),
                "w={width}: a plan glyph without its progress is a stub: {line:?}"
            );
        }
        // With a short label there is room for both sides even on a narrow
        // row — the narrow form the product targets. Whatever it keeps of the
        // step name, the run's state, progress and entry stay on the row.
        state.activity = Some("运行中".into());
        let narrow = strip_rows(&mut state, Some(plan), 30);
        let narrow = narrow[0].trim_end();
        assert!(narrow.contains("⠋ 运行中"), "{narrow}");
        assert!(narrow.contains("● 计划 2/4"), "{narrow}");
        assert!(narrow.ends_with('↗'), "{narrow}");
        // And with even less room the step name drops before the arrow does.
        let cramped = strip_rows(&mut state, Some(running_plan(4, 2)), 25);
        let cramped = cramped[0].trim_end();
        assert!(cramped.contains("● 计划 2/4 ↗"), "{cramped}");
        assert!(!cramped.contains('步'), "{cramped}");
    }

    fn completed_plan() -> UiPlan {
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
                    status: PlanStepStatus::Done,
                },
            ],
            state: Default::default(),
        }
    }

    #[test]
    fn plan_indicator_hidden_until_steps_exist() {
        // Busy goal with no plan must not reserve chrome for "等待计划".
        assert!(!plan_indicator_should_show(
            &UiPlan {
                steps: vec![],
                state: Default::default(),
            },
            true
        ));
        assert!(plan_indicator_should_show(&sample_plan(), true));
    }

    /// The archive question is unchanged: a fully settled plan is history and
    /// earns a transcript block only when it has open or failed work.
    #[test]
    fn finished_plan_including_single_step_stays_out_of_history() {
        let one_done = UiPlan {
            steps: vec![UiPlanStep {
                index: 0,
                description: "清理/重置任务计划".into(),
                status: PlanStepStatus::Done,
            }],
            state: Default::default(),
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
            state: Default::default(),
        };
        assert!(!plan_panel_should_show(&multi_done));

        let still_open = UiPlan {
            steps: vec![UiPlanStep {
                index: 0,
                description: "only step".into(),
                status: PlanStepStatus::Running,
            }],
            state: Default::default(),
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
            state: Default::default(),
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
            "你是只读探查。task A 的完整提示词".into(),
            0,
        );
        s.transcript
            .set_sub_agent_title("agent-1", Some("查事件入口".into()));
        s.transcript.push_sub_agent_started(
            "agent-2".into(),
            "Newton".into(),
            "explorer".into(),
            "你是只读探查。task B 的完整提示词".into(),
            0,
        );
        s.transcript
            .set_sub_agent_title("agent-2", Some("查执行链".into()));
        let lines = crate::conversation::build::build_conversation_lines(&s, 100);
        let text = lines.iter().map(rule_plain).collect::<Vec<_>>().join("\n");
        assert!(text.contains("2 个 agents 正在运行"), "{text}");
        assert!(text.contains("查事件入口、查执行链"), "{text}");
        assert!(!text.contains("你是只读探查"), "{text}");
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
            None,
            None,
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
            .position(|l| l.trim() == "最终回答")
            .unwrap_or_else(|| panic!("answer missing: {plain:?}"));
        assert!(answer >= 1, "tool group must precede the answer: {plain:?}");
        assert!(
            plain[answer - 1].trim().is_empty(),
            "a blank line must separate the final answer from the tool group: {plain:?}"
        );
    }

    // ── State strip: runtime status + plan indicator ────────────────────────

    /// Paint the state strip alone into a one-row test terminal with `plan`
    /// installed, and hand back the painted buffer. One production call site,
    /// so these tests see exactly what the workbench paints.
    fn strip_buffer(
        state: &mut AppState,
        plan: Option<UiPlan>,
        width: u16,
    ) -> ratatui::buffer::Buffer {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        state.plan = plan;
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_state_strip(f, area, state);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// The painted rows as logical text: each wide char owns two buffer cells
    /// and only the first holds its symbol, so the reader steps over the
    /// continuation cell by display width and keeps every real space.
    fn strip_rows(state: &mut AppState, plan: Option<UiPlan>, width: u16) -> Vec<String> {
        let buf = strip_buffer(state, plan, width);
        (0..buf.area.height)
            .map(|y| {
                let mut out = String::new();
                let mut skip = 0usize;
                for x in 0..buf.area.width {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let symbol = buf[(x, y)].symbol();
                    out.push_str(symbol);
                    skip = UnicodeWidthStr::width(symbol).saturating_sub(1);
                }
                out
            })
            .collect()
    }

    /// The buffer cells themselves, so hit ranges can be checked by column
    /// (a string indexes bytes, and a wide char owns two cells).
    fn strip_buffer_cells(buf: &ratatui::buffer::Buffer) -> Vec<(String, ratatui::style::Color)> {
        (0..buf.area.width)
            .map(|x| {
                let cell = &buf[(x, 0)];
                (cell.symbol().to_string(), cell.fg)
            })
            .collect()
    }

    /// The logical text painted in columns `x0..x1`, stepping over wide-char
    /// continuation cells exactly as the row reader does.
    fn painted_text(buf: &ratatui::buffer::Buffer, x0: usize, x1: usize) -> String {
        let mut out = String::new();
        let mut skip = 0usize;
        for x in 0..buf.area.width {
            let x = usize::from(x);
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let symbol = buf[(x as u16, 0)].symbol();
            if (x0..x1).contains(&x) {
                out.push_str(symbol);
            }
            if x >= x1 {
                break;
            }
            skip = UnicodeWidthStr::width(symbol).saturating_sub(1);
        }
        out
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
            state: Default::default(),
        }
    }

    /// The strip is always exactly one row, whatever the plan's shape — the
    /// complete list belongs to the Plan page.
    #[test]
    fn the_state_strip_is_always_one_row() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let rows = strip_rows(&mut state, Some(running_plan(9, 5)), 70);
        assert_eq!(rows.len(), 1, "{rows:?}");
        let line = rows[0].trim_end();
        assert!(line.contains("● 计划 5/9 · 步骤 6 ↗"), "{line}");
        assert!(line.ends_with('↗'), "{line}");
    }

    /// The indicator carries declared progress and never mistakes the running
    /// ordinal for the done count.
    #[test]
    fn the_plan_indicator_carries_declared_progress() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let rows = strip_rows(&mut state, Some(running_plan(9, 5)), 70);
        let line = rows[0].trim_end();
        assert!(line.contains("计划 5/9"), "{line}");
        assert!(!line.contains("6/9"), "{line}");
        assert!(!line.contains("步骤 5"), "done steps drop out: {line}");
    }

    /// A failed step owns the indicator: red, named as a failure, still
    /// actionable through the arrow.
    #[test]
    fn a_failed_plan_says_so_and_stays_reachable() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.theme = crate::theme::Theme::dark();
        let mut plan = running_plan(4, 2);
        plan.steps[2].status = PlanStepStatus::Failed;
        let buf = strip_buffer(&mut state, Some(plan.clone()), 100);
        let rows = strip_rows(&mut state, Some(plan), 100);
        let line = rows[0].trim_end();
        assert!(line.contains("✕ 计划 2/4 · 执行失败 ↗"), "{line}");
        let glyph = (0..buf.area.width)
            .map(|x| buf[(x, 0)].clone())
            .find(|cell| cell.symbol() == "✕")
            .expect("failure glyph painted");
        assert_eq!(
            glyph.fg, state.theme.status.error,
            "the failure ink is the error token"
        );
    }

    fn bg_running_task(label: &str, started: u64) -> crate::state::BackgroundTaskChrome {
        crate::state::BackgroundTaskChrome::running(label, started)
    }

    fn bg_failed_task(
        label: &str,
        started: u64,
        exit_code: i32,
    ) -> crate::state::BackgroundTaskChrome {
        crate::state::BackgroundTaskChrome {
            ok: Some(false),
            exit_code: Some(exit_code),
            duration_ms: Some(90_000),
            ..crate::state::BackgroundTaskChrome::running(label, started)
        }
    }

    /// All four cells on ONE row, each with its own painted hit region: the
    /// runtime's live state, the running count, the active issues and the plan.
    #[test]
    fn the_strip_paints_all_four_cells_on_one_row() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.activity = Some("运行 cargo test".into());
        state
            .background_task_labels
            .insert("b1".into(), bg_running_task("make up", 3));
        state
            .background_task_labels
            .insert("b2".into(), bg_running_task("npm run dev", 5));
        state
            .background_task_labels
            .insert("f1".into(), bg_failed_task("go test -race ./...", 7, 1));

        let buf = strip_buffer(&mut state, Some(sample_plan()), 140);
        let rows = strip_rows(&mut state, Some(sample_plan()), 140);
        let line = rows[0].trim_end();
        assert!(line.contains("运行 cargo test"), "{line}");
        assert!(line.contains("后台 2 ↗"), "{line}");
        assert!(line.contains("后台失败 1 ↗"), "{line}");
        assert!(line.contains("计划 1/3"), "{line}");
        assert!(
            !line.contains("go test -race"),
            "the failed task's command is not chrome: {line}"
        );

        // Reading order Background → Issue → Plan, hit ranges exactly the
        // painted run of each cell, and never overlapping.
        let (brow, bx0, bx1) = state.background_hit.expect("background hit");
        let (irow, ix0, ix1) = state.issue_hit.expect("issue hit");
        let (prow, px0, px1) = state.plan_hit.expect("plan hit");
        assert!(brow == irow && irow == prow && brow == 0, "one row");
        assert!(
            bx1 <= ix0,
            "cells do not overlap: b={bx0}..{bx1} i={ix0}..{ix1}"
        );
        assert!(
            ix1 <= px0,
            "cells do not overlap: i={ix0}..{ix1} p={px0}..{px1}"
        );
        assert_eq!(painted_text(&buf, bx0 as usize, bx1 as usize), "后台 2 ↗");
        assert_eq!(
            painted_text(&buf, ix0 as usize, ix1 as usize),
            "⚠ 后台失败 1 ↗"
        );
        assert!(
            painted_text(&buf, px0 as usize, px1 as usize).starts_with("● 计划 1/3"),
            "plan hit covers the painted indicator"
        );
    }

    /// A failed turn is the blocking cell: red, named with the failure's own
    /// summary — never hidden behind a bare count.
    #[test]
    fn a_blocking_turn_failure_is_named_on_the_strip() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Error;
        state.theme = crate::theme::Theme::dark();
        state
            .transcript
            .push_failure(crate::transcript::FailureBlock {
                display: crate::fold::DisplayMode::Collapsed,
                title: "请求失败".into(),
                summary: "模型认证失败".into(),
                subtitle: Some("deepseek · authentication · HTTP 401".into()),
                detail: String::new(),
            });
        let buf = strip_buffer(&mut state, None, 120);
        let rows = strip_rows(&mut state, None, 120);
        let line = rows[0].trim_end();
        assert!(line.contains("✕ 模型认证失败 ↗"), "{line}");
        assert!(
            !line.contains('⚠'),
            "blocking is not a warning glyph: {line}"
        );
        let glyph = (0..buf.area.width)
            .map(|x| buf[(x, 0)].clone())
            .find(|cell| cell.symbol() == "✕")
            .expect("blocking glyph painted");
        assert_eq!(glyph.fg, state.theme.status.error);

        // Other active issues are not hidden behind the named one: they are
        // the count suffix, and the Issue page lists them.
        state
            .background_task_labels
            .insert("f1".into(), bg_failed_task("npm start", 7, 1));
        let rows = strip_rows(&mut state, None, 120);
        let line = rows[0].trim_end();
        assert!(line.contains("✕ 模型认证失败 +1 ↗"), "{line}");
        assert!(
            !line.contains("⚠ 后台失败"),
            "the blocking cell owns the issue slot: {line}"
        );
        state.background_task_labels.clear();

        // A later turn clears the cell — the block stays in the transcript.
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let rows = strip_rows(&mut state, None, 120);
        assert!(
            !rows[0].contains('✕') && !rows[0].contains("模型认证失败"),
            "a running turn is not a failure: {rows:?}"
        );
        assert!(state.transcript.last_failure_block().is_some());
    }

    /// Width is the only variable this row has, so the rules are exact:
    /// nothing overflows, the runtime's state word never vanishes, the active
    /// issue outranks the plan (a plan cell without an issue cell is
    /// impossible), the issue degrades to its count form before it drops, and
    /// the same width always paints the same row.
    #[test]
    fn the_strip_degrades_deterministically_as_it_narrows() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.activity = Some("运行 cargo test".into());
        state
            .background_task_labels
            .insert("b1".into(), bg_running_task("make up", 3));
        state
            .background_task_labels
            .insert("f1".into(), bg_failed_task("npm start", 7, 1));
        let plan = running_plan(4, 2);

        let mut rows_by_width = Vec::new();
        let mut previous: Option<(bool, bool)> = None;
        for width in 18..140u16 {
            let rows = strip_rows(&mut state, Some(plan.clone()), width);
            let line = rows[0].trim_end().to_string();
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= width as usize,
                "w={width}: never overflows: {line:?}"
            );
            assert!(
                line.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "w={width}: the runtime state never vanishes: {line:?}"
            );
            // Labelled (`⚠ 后台失败 1`) or compact (`⚠ 1`): the ink is the
            // cell's identity at every width.
            let has_issue = line.contains('⚠') || line.contains('✕');
            let has_plan = line.contains("计划 ");
            assert!(
                !has_plan || has_issue,
                "w={width}: the issue outranks the plan: {line:?}"
            );
            // Presence curves are monotone in width: once the issue (or the
            // plan, which yields to it) is shown, a wider row still shows it.
            // The background cell is claimed LAST, so it may appear in a band
            // where the plan's floor cannot fit — it can never displace the
            // plan, but it legitimately fills room the plan cannot use.
            if let Some((issue_before, plan_before)) = previous {
                assert!(
                    !issue_before || has_issue,
                    "w={width}: the issue must not vanish as the row widens: {line:?}"
                );
                assert!(
                    !plan_before || has_plan,
                    "w={width}: the plan must not vanish as the row widens: {line:?}"
                );
            }
            previous = Some((has_issue, has_plan));
            rows_by_width.push(line);
        }
        // Deterministic: the same width repaints the same row.
        let again = strip_rows(&mut state, Some(plan.clone()), 77);
        assert_eq!(again[0].trim_end(), rows_by_width[77 - 18]);

        // Wide: all four cells. Wide-mid: the background yields to the plan.
        // Narrow: the issue keeps a count while the plan is gone.
        let wide = strip_rows(&mut state, Some(plan.clone()), 139);
        let wide = wide[0].trim_end();
        assert!(
            wide.contains("后台 1") && wide.contains("后台失败 1") && wide.contains("计划 2/4"),
            "{wide:?}"
        );
        let mid = strip_rows(&mut state, Some(plan.clone()), 40);
        let mid = mid[0].trim_end();
        assert!(mid.contains("计划 2/4"), "{mid:?}");
        assert!(mid.contains("后台失败 1"), "{mid:?}");
        let narrow = strip_rows(&mut state, Some(plan.clone()), 24);
        let narrow = narrow[0].trim_end();
        assert!(narrow.contains("后台失败 1"), "{narrow:?}");
        assert!(!narrow.contains("后台 1"), "{narrow:?}");
        let narrowest = strip_rows(&mut state, Some(plan), 19);
        let narrowest = narrowest[0].trim_end();
        assert!(narrowest.contains("⚠ 1 ↗"), "{narrowest:?}");
        assert!(!narrowest.contains("计划"), "{narrowest:?}");
    }

    /// Running count only: a settled task leaves the background cell, and a
    /// failed one is an issue instead — never both.
    #[test]
    fn a_settled_task_leaves_the_background_cell() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Idle;
        state.background_task_labels.insert(
            "b1".into(),
            crate::state::BackgroundTaskChrome {
                ok: Some(true),
                ..bg_running_task("make up", 3)
            },
        );
        let rows = strip_rows(&mut state, None, 100);
        assert!(!rows[0].contains("后台"), "{rows:?}");
        assert!(state.background_hit.is_none());

        state
            .background_task_labels
            .insert("f1".into(), bg_failed_task("npm start", 7, 1));
        let rows = strip_rows(&mut state, None, 100);
        assert!(rows[0].contains("后台失败 1"), "{rows:?}");
        assert!(
            !rows[0].contains("后台 1"),
            "a failed task is not a running count: {rows:?}"
        );
        assert!(state.background_hit.is_none());
        assert!(state.issue_hit.is_some());
    }

    /// A settled plan outside its turn is history, not chrome: the strip is
    /// the runtime status alone and the plan publishes no hit region.
    #[test]
    fn a_settled_plan_outside_its_turn_leaves_no_strip() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Idle;
        let rows = strip_rows(&mut state, Some(completed_plan()), 70);
        assert!(rows[0].trim().is_empty(), "{rows:?}");
        assert!(state.plan_hit.is_none(), "{:?}", state.plan_hit);
        assert!(!plan_indicator_should_show(&completed_plan(), false));
        assert!(
            plan_indicator_should_show(&completed_plan(), true),
            "while the turn still runs the completion is live state"
        );
    }

    /// Without a plan the strip is exactly the runtime status line: the merge
    /// added nothing when there is nothing to merge.
    #[test]
    fn without_any_plan_the_strip_is_the_runtime_line_verbatim() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.activity = Some("运行 cargo test".into());
        let standalone = crate::status_line::status_strip_line(&state, 70, true).to_string();
        let merged = strip_rows(&mut state, None, 70);
        assert_eq!(merged[0].trim_end(), standalone);
        assert!(state.plan_hit.is_none(), "{:?}", state.plan_hit);
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
                model_notice: None,
                thinking: None,
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
            pid: None,
            last_output_at: None,
            output: String::new(),
            unknown: false,
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
                model_notice: None,
                thinking: None,
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

    /// Nothing running and nothing failed: neither the background nor the
    /// issue cell exists, and the footer stays the single existing row.
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

    /// One running task is ONE count cell on the state strip — never its raw
    /// command, and never a row of its own anywhere in the chrome.
    #[test]
    fn one_running_task_is_a_count_cell_not_a_command() {
        let lines = render_background_case(
            100,
            30,
            vec![("bg-1", bg_running("make up", 28))],
            &[],
            crate::state::WorkbenchFocus::Input,
        );
        let all = lines.join("\n");
        assert!(all.contains("后台 1"), "{all}");
        assert!(
            !all.contains("make up"),
            "the raw command belongs to the jobs list, not the chrome: {all}"
        );
        let strip = row_of(&lines, "后台 1");
        assert!(lines[strip].contains('↗'), "{all}");
    }

    /// Several running tasks aggregate into one count; no task is ever named.
    #[test]
    fn several_running_tasks_aggregate_into_one_strip_cell() {
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
            lines.iter().filter(|l| l.contains("后台 3")).count(),
            1,
            "one aggregate cell: {all}"
        );
        for label in ["make up", "cargo test", "dev server"] {
            assert!(
                !all.contains(label),
                "{label} must not get its own row: {all}"
            );
        }
    }

    /// Running and failed are separate cells with separate inks: a failed task
    /// is an issue, never folded into the running count.
    #[test]
    fn running_and_failed_are_separate_cells() {
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
        assert!(all.contains("后台失败 1"), "{all}");
        assert!(
            !all.contains("后台 3"),
            "failed is not folded into running: {all}"
        );
    }

    /// Only failures: the issue cell names the kind and its count.
    #[test]
    fn failures_alone_read_as_an_issue_cell() {
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
        assert!(all.contains('⚠'), "{all}");
        assert!(all.contains("后台失败 2"), "{all}");
        assert!(!all.contains("后台 2"), "{all}");
    }

    /// A completed task never lingers in the chassis — its history is the list.
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
        assert!(!all.contains("后台"), "{all}");
        assert!(!all.contains('↗'), "{all}");
    }

    /// A stopped task is not a failure: no issue cell, no error ink.
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

    /// Acknowledged means READ, not solved: the reminder leaves the strip, and
    /// the failure itself stays as an active record on its page and in the
    /// jobs list. An unviewed one still reminds.
    #[test]
    fn a_viewed_failure_stops_reminding_an_unviewed_one_does_not() {
        let lines = render_background_case(
            120,
            30,
            vec![("bg-1", bg_terminal("npm start", false, false, 0, 30_000))],
            &[],
            crate::state::WorkbenchFocus::Input,
        );
        let all = lines.join("\n");
        assert!(
            all.contains("后台失败 1"),
            "an unseen failure reminds: {all}"
        );

        let lines = render_background_case(
            120,
            30,
            vec![("bg-1", bg_terminal("npm start", false, false, 0, 30_000))],
            &["bg-1"],
            crate::state::WorkbenchFocus::Input,
        );
        let all = lines.join("\n");
        assert!(
            !all.contains("后台失败"),
            "a viewed one-shot failure stops reminding: {all}"
        );
    }

    /// A focused background cell is reachable: the hint row names Enter.
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
        assert!(all.contains("后台 1"), "{all}");
        assert!(all.contains("Enter 后台任务"), "{all}");
    }

    /// A long command label cannot eat the row: the strip counts the task,
    /// and the clock and the right chips keep their cells.
    #[test]
    fn a_long_command_cannot_push_the_right_chips() {
        let label = "API_PROXY_TARGET=http://localhost:8080 exec npm start -- --watch";
        let lines = render_background_case(
            60,
            30,
            vec![("bg-1", bg_running(label, 10))],
            &[],
            crate::state::WorkbenchFocus::Input,
        );
        let all = lines.join("\n");
        assert!(!all.contains("API_PROXY_TARGET"), "{all}");
        let row = &lines[row_of(&lines, "09:30")];
        assert!(row.contains("09:30"), "the clock survives: {row:?}");
        assert!(row.chars().count() <= 60, "no overflow: {row:?}");
        assert!(
            all.contains("后台 1"),
            "the task is still reachable as a count: {all}"
        );
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
    /// The live row is the second stop surface and the pointer entry for
    /// backgrounding; both controls are published as hit rects.
    #[test]
    fn the_live_shell_row_publishes_background_and_stop_hit_rects() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = test_state();
        state.transcript.push_user_shell_started(
            leveler_core::UserShellId::new("ush-live"),
            "go test ./...".into(),
            "/repo".into(),
            0,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 4)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_shell_live_activity(frame, area, &mut state);
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let compact = text.replace(' ', "");
        assert!(compact.contains("↗转后台"), "{text}");
        assert!(compact.contains("✕停止"), "{text}");
        assert!(text.contains("go test ./..."), "{text}");
        assert!(
            state.shell_live_background_rect.is_some(),
            "the background control has a click target"
        );
        assert!(
            state.shell_live_stop_rect.is_some(),
            "the stop control has a click target"
        );
    }
}
