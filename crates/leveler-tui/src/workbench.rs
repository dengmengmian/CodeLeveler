//! Workbench layout: fixed Header / Plan / Input / Footer + scrollable Conversation.
//!
//! Layout (top → bottom):
//! Header · Conversation (scroll) · gap · Status? · Queue · Plan · gap? · Input · gap · Footer
//!
//! `/btw` is a floating card over the Conversation bottom — not main history.

use leveler_client_protocol::{PlanStepStatus, UiPlan, UiPlanStep};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::footer_queue::{queue_panel_height, queue_panel_lines};
use crate::i18n::UiText;
use crate::render::{
    COMPOSER_MAX_ROWS, btw_card_lines, composer_box_lines, composer_visible_rows, item_render,
    items_need_gap, render_attachments, render_slash_popup,
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
/// scannable without `/steps`.
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

    let attach_rows: u16 = if state.pending_attachments.is_empty() {
        0
    } else {
        1
    };
    let queue_rows = queue_panel_height(state);
    let plan_rows = plan_panel_height(state);
    let composer_rows =
        composer_visible_rows(state, area.width as usize).clamp(3, COMPOSER_MAX_ROWS + 2) as u16;
    // Header: single status line + hairline separator (2 rows). Footer 1.
    let header_rows: u16 = 2;
    let footer_rows: u16 = 1;
    // One blank row between transcript and bottom chrome so the last answer /
    // turn-end marker does not sit flush on the composer border (parity with
    // conversation_footer). Status only takes a row when it has content so we
    // do not stack two empty strips when idle.
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
    // (status / queue / plan / attachments) sits on top of it — matches
    // conversation_footer; blank below always so Context footer is not flush
    // on the composer border.
    let chrome_above = status_rows
        .saturating_add(queue_rows)
        .saturating_add(plan_rows)
        .saturating_add(attach_rows);
    let pre_composer_gap: u16 = if chrome_above > 0 { 1 } else { 0 };
    let post_composer_gap: u16 = 1;

    let chunks = Layout::vertical([
        Constraint::Length(header_rows),
        Constraint::Min(3), // conversation viewport
        Constraint::Length(gap_rows),
        Constraint::Length(status_rows),
        Constraint::Length(queue_rows),
        Constraint::Length(plan_rows),
        Constraint::Length(attach_rows),
        Constraint::Length(pre_composer_gap),
        Constraint::Length(composer_rows),
        Constraint::Length(post_composer_gap),
        Constraint::Length(footer_rows),
    ])
    .split(area);

    render_header(frame, chunks[0], state);
    render_conversation(frame, chunks[1], state);
    // chunks[2] = gap (leave blank)
    if status_rows > 0 {
        frame.render_widget(Paragraph::new(status_line), chunks[3]);
    }
    render_queue_panel(frame, chunks[4], state);
    render_plan_panel(frame, chunks[5], state);
    render_attachments(frame, chunks[6], state);
    // chunks[7] = pre_composer_gap (leave blank)
    render_input(frame, chunks[8], state);
    // chunks[9] = post_composer_gap (leave blank)
    render_footer(frame, chunks[10], state);

    // /btw floats over the conversation viewport (not in the scroll stream).
    render_btw_overlay(frame, chunks[1], state);
    // Toast over conversation bottom — must not change vertical layout.
    render_notification_toast(frame, chunks[1], state);

    if state.active_screen == Screen::Conversation && state.overlay.is_none() {
        render_slash_popup(frame, chunks[1], chunks[8], state);
    }
    if let Some(overlay) = &state.overlay {
        crate::overlay::render_overlay(frame, area, overlay, &state.theme);
    }
}

// ── Header (single-line environment strip + rule — no model / tokens) ───────

fn render_header(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let width = area.width as usize;
    let [status, rule_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    frame.render_widget(Paragraph::new(header_status_line(state, width)), status);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(width),
            Style::default().fg(theme.border),
        ))),
        rule_area,
    );
}

/// Progressive single-line header that degrades as the terminal narrows.
///
/// Wide:   `CodeLeveler v0.1.0 · ~/.../repo ·  main a899d24`
/// Medium: `CodeLeveler v0.1.0 · repo ·  main`
/// Narrow: `CodeLeveler ·  main`
fn header_status_line(state: &AppState, width: usize) -> Line<'static> {
    let theme = &state.theme;
    let brand = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(theme.muted);
    let git = Style::default().fg(theme.success);

    let version = state.version();
    let branch = state.branch.as_deref().unwrap_or("—").to_string();
    let sid = short_session(state);
    let home_repo = display_repo(state);
    let base_repo = repo_basename(&home_repo);
    let mid_repo = middle_ellipsis_repo(&home_repo);

    let ver = format!(" v{version}");
    // Richest → sparsest plain-text candidates; first that fits wins.
    let texts = [
        format!("CodeLeveler{ver} · {mid_repo} ·  {branch} {sid}"),
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
    style_header_text(&chosen, brand, muted, git)
}

/// Apply brand / muted / git colors onto a pre-sized header string.
fn style_header_text(text: &str, brand: Style, muted: Style, git: Style) -> Line<'static> {
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
            spans.push(Span::styled((*part).to_string(), muted));
        }
    }
    Line::from(spans)
}

fn display_repo(state: &AppState) -> String {
    let repo = if state.repository.is_empty() {
        "—"
    } else {
        state.repository.as_str()
    };
    match leveler_core::environment().var_os("HOME") {
        Some(h) => {
            let hs = h.to_string_lossy();
            if let Some(rest) = repo.strip_prefix(hs.as_ref()) {
                format!("~{rest}")
            } else {
                repo.to_string()
            }
        }
        None => repo.to_string(),
    }
}

/// `~/projects/services/example-service` → `~/.../example-service`
fn middle_ellipsis_repo(repo: &str) -> String {
    let base = repo_basename(repo);
    if repo == base || repo == "—" {
        return base;
    }
    if let Some(rest) = repo.strip_prefix("~/") {
        if rest.contains('/') {
            return format!("~/.../{base}");
        }
    } else if repo.contains('/') {
        return format!(".../{base}");
    }
    repo.to_string()
}

fn repo_basename(repo: &str) -> String {
    std::path::Path::new(repo)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(repo)
        .to_string()
}

fn short_session(state: &AppState) -> String {
    let raw = state.session_id.as_str();
    if raw.len() <= 8 {
        raw.to_string()
    } else {
        raw[raw.len().saturating_sub(8)..].to_string()
    }
}

// ── Conversation viewport ───────────────────────────────────────────────────

fn render_conversation(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let theme = &state.theme;
    let width = area.width as usize;
    let height = area.height as usize;
    if height == 0 || width == 0 {
        state.conversation_rect = None;
        state.scroll_bottom_rect = None;
        return;
    }

    let all = build_conversation_lines(state, width);
    // Cache plain text for mouse selection / clipboard.
    state.conversation_plain = all.iter().map(crate::selection::line_to_plain).collect();
    state.conversation_plain_width = width;

    let total = all.len();
    let max_scroll = total.saturating_sub(height);
    let scroll = if state.conversation_auto_scroll {
        max_scroll
    } else {
        state.conversation_scroll.min(max_scroll)
    };

    let mut lines: Vec<Line> = all
        .into_iter()
        .enumerate()
        .skip(scroll)
        .take(height)
        .map(|(abs_row, line)| {
            crate::selection::apply_selection_highlight(line, abs_row, &state.selection, theme)
        })
        .collect();

    // Pad with blanks so the viewport stays stable height.
    while lines.len() < height {
        lines.push(Line::from(""));
    }

    frame.render_widget(Paragraph::new(lines), area);

    // Mouse hit-testing for next events.
    state.conversation_rect = Some((area.x, area.y, area.width, area.height));

    // Scroll-to-bottom affordance: only when pinned away from live edge.
    // Hide while selecting/copying so the badge cannot cover or steal mouse
    // hits on the text the user is trying to select (was centered on the last
    // row and blocked mid-line copy).
    if max_scroll > 0 && scroll < max_scroll && !state.selection.is_active() {
        let below = max_scroll - scroll;
        let n = state.conversation_unread.max(below);
        let hint = if n > 1 {
            format!(" ▼{n} ")
        } else {
            " ▼ ".to_string()
        };
        let hint_w = (hint.chars().count() as u16).max(1).min(area.width);
        // Bottom-right, not center — less likely to sit on prose mid-line.
        let x = area.x.saturating_add(area.width.saturating_sub(hint_w));
        let y = area.y.saturating_add(area.height.saturating_sub(1));
        let btn = Rect {
            x,
            y,
            width: hint_w,
            height: 1,
        };
        state.scroll_bottom_rect = Some((btn.x, btn.y, btn.width, btn.height));
        frame.render_widget(
            Paragraph::new(Span::styled(
                hint,
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.code_bg)
                    .add_modifier(Modifier::BOLD),
            )),
            btn,
        );
    } else {
        state.scroll_bottom_rect = None;
    }
}

/// Flatten transcript (+ live reasoning) into display lines for the viewport.
pub fn build_conversation_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let theme = &state.theme;
    let t = state.t();
    let mut out: Vec<Line<'static>> = Vec::new();

    // Empty session: brand splash (logo + tagline) instead of a blank void.
    if crate::splash::conversation_is_empty(state) {
        return crate::splash::splash_lines(state, width, theme, t);
    }

    let items = state.transcript.items();
    for (idx, item) in items.iter().enumerate() {
        // Welcome card is retired; /btw is a floating overlay, not scroll content.
        if matches!(item, TranscriptItem::Welcome(_) | TranscriptItem::Btw(_)) {
            continue;
        }
        if idx > 0 && items_need_gap(&items[idx - 1], item) {
            out.push(Line::from(""));
        }
        // Message types are distinguished by shape, not role headings:
        // `>` user prompt, `●` agent prose, status glyphs for tool activity.
        match item {
            TranscriptItem::User(text) => {
                for line in wrap_simple(text, width.saturating_sub(2).max(1)) {
                    out.push(Line::from(vec![
                        Span::styled("> ", Style::default().fg(theme.accent)),
                        Span::styled(line, Style::default().fg(theme.user_message)),
                    ]));
                }
            }
            TranscriptItem::Assistant(_) => {
                out.extend(item_render(item, theme, width, state.tools_expanded, t));
            }
            TranscriptItem::ToolGroup(group) => {
                // Product activity stream — not a raw tool trace:
                // Silent (ls/list_files/probes) stay out; Normal exploration
                // aggregates; Important edits/runs stay one bold line each.
                out.extend(crate::activity_stream::render_group(
                    group,
                    theme,
                    width,
                    state.locale,
                    t,
                ));
            }
            _ => {
                out.extend(item_render(item, theme, width, state.tools_expanded, t));
            }
        }
    }

    // Live reasoning as activity while in flight.
    if !state.reasoning.is_empty() {
        out.push(Line::from(""));
        let disclosure = if state.reasoning_expanded {
            "▾"
        } else {
            "▸"
        };
        let reasoning_lines: Vec<&str> = state
            .reasoning
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let n = reasoning_lines.len();
        out.push(Line::from(vec![
            Span::styled(
                format!("{disclosure} {} · {n} lines", t.thinking),
                Style::default().fg(theme.muted),
            ),
            Span::styled(
                if state.reasoning_expanded {
                    String::new()
                } else {
                    "  Ctrl+O".to_string()
                },
                Style::default().fg(theme.border),
            ),
        ]));
        if state.reasoning_expanded {
            // Cap body so CoT cannot flood the viewport; show honest remainder.
            const MAX_EXPANDED_REASONING: usize = 24;
            for line in reasoning_lines.iter().take(MAX_EXPANDED_REASONING) {
                out.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(theme.muted),
                )));
            }
            if n > MAX_EXPANDED_REASONING {
                out.push(Line::from(Span::styled(
                    format!("  … (+{} lines)", n - MAX_EXPANDED_REASONING),
                    Style::default().fg(theme.border),
                )));
            }
        }
    }

    out
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
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ))];

    if !state.plan_collapsed {
        for step in plan.steps.iter().take(area.height as usize - 1) {
            let (g, c) = match step.status {
                PlanStepStatus::Done => ("✓", theme.success),
                PlanStepStatus::Running => ("→", theme.accent),
                PlanStepStatus::Failed => ("✗", theme.error),
                PlanStepStatus::Skipped => ("–", theme.muted),
                PlanStepStatus::Pending => ("○", theme.muted),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{g} "), Style::default().fg(c)),
                Span::styled(
                    truncate(
                        format!("{}. {}", step.index + 1, step.description),
                        area.width.saturating_sub(3) as usize,
                    ),
                    Style::default().fg(if step.status == PlanStepStatus::Running {
                        theme.text
                    } else {
                        theme.muted
                    }),
                ),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

// ── Prompt Queue (between Conversation and Plan) ────────────────────────────

fn render_queue_panel(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let lines = queue_panel_lines(state, area.width as usize);
    if lines.is_empty() {
        return;
    }
    let shown: Vec<Line> = lines.into_iter().take(area.height as usize).collect();
    frame.render_widget(Paragraph::new(shown), area);
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
        leveler_client_protocol::NotificationLevel::Info => theme.accent,
        leveler_client_protocol::NotificationLevel::Warning => theme.warning,
        leveler_client_protocol::NotificationLevel::Error => theme.error,
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
                .bg(theme.code_bg)
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
    let muted = Style::default().fg(theme.muted);
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

fn wrap_simple(s: &str, width: usize) -> Vec<String> {
    crate::render::wrap(s, width)
}

/// How many lines the conversation content needs at `width` (for scroll math).
pub fn conversation_line_count(state: &AppState, width: usize) -> usize {
    build_conversation_lines(state, width).len()
}

/// Clamp scroll after resize / content change. Returns true if state changed.
///
/// Called from the event loop after layout-affecting updates so a user who
/// scrolled up is not shoved past the content end, and auto-follow sticks to
/// the latest activity line.
pub fn sync_conversation_scroll(state: &mut AppState, width: usize, height: usize) -> bool {
    let total = conversation_line_count(state, width);
    let max_scroll = total.saturating_sub(height.max(1));
    let mut changed = false;

    // Track growth while the user is reading history → drive ▼ N.
    if !state.conversation_auto_scroll && total > state.conversation_last_len {
        state.conversation_unread = state.conversation_unread.saturating_add(1);
        changed = true;
    }
    if state.conversation_auto_scroll {
        state.conversation_unread = 0;
    }
    state.conversation_last_len = total;

    if state.conversation_auto_scroll {
        if state.conversation_scroll != max_scroll {
            state.conversation_scroll = max_scroll;
            return true;
        }
        return changed;
    }
    if state.conversation_scroll > max_scroll {
        state.conversation_scroll = max_scroll;
        return true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::PlanStepStatus;
    use leveler_client_protocol::{SessionId, UiPlan, UiPlanStep};

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
}
