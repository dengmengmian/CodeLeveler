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
    items_need_gap, render_attachments, render_slash_popup, sub_agent_tree_lines,
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
    // Header: blank breathing row + status line + hairline separator (3 rows)
    // so the brand strip is not flush against the terminal's top edge. Footer 1.
    let header_rows: u16 = 3;
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

/// The header underline. Idle: a static hairline. Busy: an accent segment that
/// slides back and forth (indeterminate progress) so the top of the screen shows
/// the agent is working. Uses a heavy glyph for the moving part so the motion is
/// still visible under `NO_COLOR`.
fn header_rule_line(width: usize, state: &AppState) -> Line<'static> {
    let theme = &state.theme;
    let border = Style::default().fg(theme.border);
    if width == 0 {
        return Line::from("");
    }
    if !state.is_busy() {
        return Line::from(Span::styled("─".repeat(width), border));
    }

    let seg = (width / 6).clamp(6, 24).min(width);
    let travel = width.saturating_sub(seg);
    // Ping-pong the segment start across [0, travel] to avoid edge wrap. Two
    // cells per frame keeps the slide lively without looking frantic.
    let start = if travel == 0 {
        0
    } else {
        let period = travel * 2;
        let phase = (state.tick as usize).wrapping_mul(2) % period;
        if phase <= travel {
            phase
        } else {
            period - phase
        }
    };
    let after = width - start - seg;

    let accent = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    if start > 0 {
        spans.push(Span::styled("─".repeat(start), border));
    }
    spans.push(Span::styled("━".repeat(seg), accent));
    if after > 0 {
        spans.push(Span::styled("─".repeat(after), border));
    }
    Line::from(spans)
}

/// Progressive single-line header that degrades as the terminal narrows.
///
/// Wide:   `CodeLeveler v0.1.0 · repo ·  main ●`
/// Medium: `CodeLeveler v0.1.0 ·  main ●`
/// Narrow: `CodeLeveler ·  main ●`
fn header_status_line(state: &AppState, width: usize) -> Line<'static> {
    let theme = &state.theme;
    let brand = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(theme.muted);
    let git = Style::default().fg(theme.success);

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

    let all = state.conversation_lines(width);
    // Plain text only backs mouse selection / clipboard. Rebuild it from this
    // frame's lines while a selection is live; otherwise clear it (the mouse-down
    // path calls `ensure_conversation_plain`, which rebuilds against current
    // content on demand) so idle frames skip an O(lines) clone per repaint.
    if state.selection.is_active() {
        state.conversation_plain = all.iter().map(crate::selection::line_to_plain).collect();
        state.conversation_plain_width = width;
    } else if !state.conversation_plain.is_empty() {
        state.conversation_plain.clear();
        state.conversation_plain_width = 0;
    }

    let total = all.len();
    let max_scroll = total.saturating_sub(height);
    let scroll = if state.conversation_auto_scroll {
        max_scroll
    } else {
        state.conversation_scroll.min(max_scroll)
    };

    // Only the visible window is cloned + highlighted; the rest stays in the Rc.
    let mut lines: Vec<Line> = all
        .iter()
        .enumerate()
        .skip(scroll)
        .take(height)
        .map(|(abs_row, line)| {
            crate::selection::apply_selection_highlight(
                line.clone(),
                abs_row,
                &state.selection,
                theme,
            )
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
/// Everything `build_conversation_lines` reads that can change its output. When
/// this is unchanged the previously wrapped lines are reused verbatim. Note the
/// transcript is captured by its monotonic `version`, so any in-place item edit
/// invalidates the cache.
#[derive(Debug, PartialEq, Clone)]
pub struct ConvKey {
    version: u64,
    width: usize,
    theme_id: crate::theme::ThemeId,
    monochrome: bool,
    locale: crate::i18n::Locale,
    tools_expanded: bool,
    reasoning_expanded: bool,
    reasoning: String,
}

impl AppState {
    /// Cache-aware conversation lines: re-wraps the whole transcript only when a
    /// render input changed; otherwise returns the previously built lines (an
    /// `Rc` clone, O(1)). The empty/splash case is not cached — the splash reads
    /// repo/branch, which the transcript `version` does not track.
    pub(crate) fn conversation_lines(&self, width: usize) -> std::rc::Rc<Vec<Line<'static>>> {
        if crate::splash::conversation_is_empty(self) {
            return std::rc::Rc::new(build_conversation_lines(self, width));
        }
        let key = ConvKey {
            version: self.transcript.version(),
            width,
            theme_id: self.theme.id,
            monochrome: self.theme.monochrome,
            locale: self.locale,
            tools_expanded: self.tools_expanded,
            reasoning_expanded: self.reasoning_expanded,
            reasoning: self.reasoning.clone(),
        };
        if let Some((k, lines)) = self.conversation_cache.borrow().as_ref()
            && *k == key
        {
            return lines.clone();
        }
        let lines = std::rc::Rc::new(build_conversation_lines(self, width));
        *self.conversation_cache.borrow_mut() = Some((key, lines.clone()));
        lines
    }
}

pub fn build_conversation_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let theme = &state.theme;
    let t = state.t();
    let mut out: Vec<Line<'static>> = Vec::new();

    // Empty session: brand splash (logo + tagline) instead of a blank void.
    if crate::splash::conversation_is_empty(state) {
        return crate::splash::splash_lines(state, width, theme, t);
    }

    let items = state.transcript.items();
    let mut idx = 0;
    while idx < items.len() {
        let item = &items[idx];
        // Welcome card is retired; /btw is a floating overlay, not scroll content.
        if matches!(item, TranscriptItem::Welcome(_) | TranscriptItem::Btw(_)) {
            idx += 1;
            continue;
        }
        if idx > 0 && items_need_gap(&items[idx - 1], item) {
            out.push(Line::from(""));
        }
        // Message types are distinguished by shape, not role headings:
        // `▌` user prompt, `●` agent prose, status glyphs for tool activity.
        match item {
            TranscriptItem::User(text) => {
                // A solid heading bar + bold body marks the user's turn clearly
                // apart from the assistant's `●` bullet and normal-weight prose.
                let bar = Style::default()
                    .fg(theme.heading)
                    .add_modifier(Modifier::BOLD);
                let body = Style::default()
                    .fg(theme.user_message)
                    .add_modifier(Modifier::BOLD);
                for line in wrap_simple(text, width.saturating_sub(2).max(1)) {
                    out.push(Line::from(vec![
                        Span::styled("▌ ", bar),
                        Span::styled(line, body),
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
            TranscriptItem::SubAgent(first) => {
                // A run of consecutive sub-agent blocks renders as one tree
                // (aggregate header + ├─/└─ children). Any other item breaks
                // the run — batches split by tool calls stay separate.
                let mut blocks = vec![first];
                while let Some(TranscriptItem::SubAgent(next)) = items.get(idx + 1) {
                    blocks.push(next);
                    idx += 1;
                }
                out.extend(sub_agent_tree_lines(&blocks, theme, width, t));
            }
            _ => {
                out.extend(item_render(item, theme, width, state.tools_expanded, t));
            }
        }
        idx += 1;
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
    state.conversation_lines(width).len()
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
            },
        )
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
    fn header_rule_animates_and_keeps_width_when_busy() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        // Width is preserved across frames; a moving heavy segment is present.
        for tick in [0u64, 3, 7, 50] {
            state.tick = tick;
            let plain = rule_plain(&header_rule_line(48, &state));
            assert_eq!(
                unicode_width::UnicodeWidthStr::width(plain.as_str()),
                48,
                "busy rule must fill width at tick {tick}: {plain:?}"
            );
            assert!(plain.contains('━'), "moving segment missing at tick {tick}");
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
        assert_eq!(*a, build_conversation_lines(&s, 40));

        // A transcript mutation bumps the version → rebuild with new content.
        s.transcript.push_user("world".into());
        let c = s.conversation_lines(40);
        assert!(
            !std::rc::Rc::ptr_eq(&a, &c),
            "a content change must invalidate the cache"
        );
        assert_eq!(*c, build_conversation_lines(&s, 40));

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
        );
        s.transcript.push_sub_agent_started(
            "agent-2".into(),
            "Newton".into(),
            "explorer".into(),
            "task B".into(),
        );
        let lines = build_conversation_lines(&s, 100);
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
        );
        s.transcript.complete_tool(&call, true, "ok".into(), 1);
        let id = leveler_client_protocol::MessageId::new("m1");
        s.transcript.begin_assistant(id.clone());
        s.transcript.append_assistant(&id, "最终回答");
        s.transcript.finish_assistant(&id);

        let lines = build_conversation_lines(&s, 80);
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
