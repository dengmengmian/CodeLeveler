//! First-class Activity projection: background tasks and child agents.
//!
//! Presentation only. Lifecycle stays on the runtime events already applied
//! to [`AppState`]. This module does not schedule, cancel, or persist work.

use unicode_width::UnicodeWidthStr;

use crate::i18n::UiText;
use crate::multi_agent::ChildStatus;
use crate::render::truncate_display;
use crate::state::{AppState, BackgroundTaskChrome};
use crate::status_line::fmt_elapsed;

/// Compact status-strip cap: how many ACTIVITIES are shown. Remaining stay
/// available on the Activity screen. A child occupies two physical lines, so
/// the strip's own row cap is derived from this in the workbench layout.
pub(crate) const MAX_STATUS_ROWS: usize = 4;

/// Stable identity. Never derived from the display title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityId {
    Background(String),
    Child(String),
}

impl ActivityId {
    pub fn as_key(&self) -> &str {
        match self {
            ActivityId::Background(id) | ActivityId::Child(id) => id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    BackgroundTask,
    ChildAgent,
}

/// UI mapping of existing runtime/child status. Not a second lifecycle owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityStatus {
    Running,
    Waiting,
    Completed,
    Failed,
    /// A background task the runtime reported `Killed`: a user/agent cancel or
    /// session cleanup. Never counted as a failure.
    Stopped,
    /// A child whose activation died with its runtime window.
    Interrupted,
    /// A child whose turn ended without its terminal reaching this view.
    Unreported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivitySummary {
    pub id: ActivityId,
    pub kind: ActivityKind,
    /// The subject: the child's nickname (with its agent name) or the
    /// background task's label. Identity, not the task description.
    pub title: String,
    /// The child's short semantic task title, when the runtime recorded one.
    /// `None` for background tasks and for children spawned before titles
    /// existed; the renderer then projects `secondary` instead.
    pub task_title: Option<String>,
    /// Supporting text: a child's task/recent step, unused for background
    /// tasks here. Kept separate from `title` so a two-line child row can put
    /// identity and task on their own lines.
    pub secondary: Option<String>,
    pub status: ActivityStatus,
    pub started_elapsed_secs: u64,
    pub duration_secs: u64,
}

pub(crate) fn summaries(state: &AppState) -> Vec<ActivitySummary> {
    let now = state.elapsed_secs;
    let mut running = Vec::new();
    let mut waiting = Vec::new();
    let mut done = Vec::new();

    for (task_id, chrome) in &state.background_task_labels {
        let duration = match chrome.duration_ms {
            Some(ms) => ms / 1000,
            None => now.saturating_sub(chrome.started_elapsed_secs),
        };
        let status = match chrome.outcome() {
            crate::state::BackgroundOutcome::Running => ActivityStatus::Running,
            crate::state::BackgroundOutcome::Completed => ActivityStatus::Completed,
            crate::state::BackgroundOutcome::Failed => ActivityStatus::Failed,
            crate::state::BackgroundOutcome::Stopped => ActivityStatus::Stopped,
        };
        let row = ActivitySummary {
            id: ActivityId::Background(task_id.clone()),
            kind: ActivityKind::BackgroundTask,
            title: chrome.label.clone(),
            task_title: None,
            secondary: None,
            status,
            started_elapsed_secs: chrome.started_elapsed_secs,
            duration_secs: duration,
        };
        match status {
            ActivityStatus::Running => running.push(row),
            // Interrupted is not finished: the next turn continues or settles it.
            ActivityStatus::Waiting | ActivityStatus::Interrupted | ActivityStatus::Unreported => {
                waiting.push(row)
            }
            ActivityStatus::Completed | ActivityStatus::Failed | ActivityStatus::Stopped => {
                done.push(row)
            }
        }
    }

    for child in &state.team.children {
        let base = if !child.nickname.is_empty() {
            child.nickname.clone()
        } else if !child.role.is_empty() {
            child.role.clone()
        } else {
            state.t().sub_agent_default.to_string()
        };
        let title = crate::multi_agent::child_label(&base, child.agent_name.as_deref());
        let secondary = if !child.purpose.is_empty() {
            Some(crate::multi_agent::name_the_workspace(
                &child.purpose,
                &state.repository,
            ))
        } else {
            child.recent_step.clone()
        };
        let status = match child.status {
            ChildStatus::Running => ActivityStatus::Running,
            ChildStatus::Waiting => ActivityStatus::Waiting,
            ChildStatus::Completed => ActivityStatus::Completed,
            ChildStatus::Failed => ActivityStatus::Failed,
            ChildStatus::Interrupted => ActivityStatus::Interrupted,
            ChildStatus::Unreported => ActivityStatus::Unreported,
        };
        let row = ActivitySummary {
            id: ActivityId::Child(child.id.clone()),
            kind: ActivityKind::ChildAgent,
            title,
            task_title: child.title.clone(),
            secondary,
            status,
            started_elapsed_secs: child.started_elapsed_secs,
            // A settled child keeps the time it took; only a live one follows
            // the turn clock.
            duration_secs: child
                .settled_elapsed_secs
                .unwrap_or(now)
                .saturating_sub(child.started_elapsed_secs),
        };
        match status {
            ActivityStatus::Running => running.push(row),
            // Interrupted is not finished: the next turn continues or settles it.
            ActivityStatus::Waiting | ActivityStatus::Interrupted | ActivityStatus::Unreported => {
                waiting.push(row)
            }
            ActivityStatus::Completed | ActivityStatus::Failed | ActivityStatus::Stopped => {
                done.push(row)
            }
        }
    }

    running.sort_by_key(|r| r.started_elapsed_secs);
    waiting.sort_by_key(|r| r.started_elapsed_secs);
    done.sort_by_key(|r| std::cmp::Reverse(r.started_elapsed_secs));
    running.extend(waiting);
    running.extend(done);
    running
}

/// Blank columns before a child row's task line: aligned under the identity,
/// past `→ ` (2) plus the status glyph and its space (2).
const CHILD_TASK_INDENT: &str = "    ";

/// One physical line of the status-strip activity block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActivityRow {
    pub text: String,
    /// The activity this line opens. Both lines of a two-line child carry the
    /// same id, so either one opens the same detail.
    pub id: Option<ActivityId>,
    /// Whether this line belongs to the currently selected activity.
    pub selected: bool,
}

/// The header line: identity, its status glyph, and the run time. A child's
/// task text goes on its own line ([`child_task_line`]) rather than being
/// concatenated here, so the identity group stays legible at any width.
///
/// The duration and `↗` sit directly after the identity; they are part of the
/// identity group, never right-aligned to the terminal edge.
pub(crate) fn compact_row(summary: &ActivitySummary, selected: bool, width: usize) -> String {
    let glyph = activity_glyph(summary.status);
    let dur = fmt_elapsed(summary.duration_secs);
    let prefix = if selected { "→ " } else { "  " };
    // `·` separates the identity from its run time, the same separator the
    // rest of the strip and the composer use.
    let suffix = format!(" · {dur} ↗");
    // Display columns, not UTF-8 bytes: `◌`/`↗`/`→` are 3 bytes and 1 cell.
    // Byte arithmetic steals four columns and ellipsizes the identity the row
    // exists to show (Windows CI: `extract` became `extrac…` at width 80).
    let chrome = UnicodeWidthStr::width(prefix)
        + UnicodeWidthStr::width(glyph)
        + UnicodeWidthStr::width(" ")
        + UnicodeWidthStr::width(suffix.as_str());
    let budget = width.saturating_sub(chrome).max(8);
    let identity = truncate_display(&summary.title, budget);
    format!("{prefix}{glyph} {identity}{suffix}")
}

/// A child's compact task line, or `None` when there is nothing to show (a
/// background task, or a child with neither a title nor a task).
///
/// The semantic title is preferred. A child recorded before titles existed
/// falls back to the first line of its purpose: a plain projection of the
/// text the runtime recorded, not a semantic title.
fn child_task_line(summary: &ActivitySummary, width: usize) -> Option<String> {
    if summary.kind != ActivityKind::ChildAgent {
        return None;
    }
    let semantic = summary
        .task_title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let fallback = summary
        .secondary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.lines().next().unwrap_or(s).trim().to_string());
    let text = semantic.or(fallback)?;
    let avail = width
        .saturating_sub(UnicodeWidthStr::width(CHILD_TASK_INDENT))
        .max(1);
    Some(format!(
        "{CHILD_TASK_INDENT}{}",
        truncate_display(&text, avail)
    ))
}

pub(crate) fn activity_glyph(status: ActivityStatus) -> &'static str {
    match status {
        ActivityStatus::Running => "●",
        ActivityStatus::Waiting => "◌",
        ActivityStatus::Completed => "✓",
        ActivityStatus::Failed => "✕",
        ActivityStatus::Stopped => "■",
        ActivityStatus::Interrupted => "⏸",
        ActivityStatus::Unreported => "?",
    }
}

pub(crate) fn running_background_count(state: &AppState) -> usize {
    state
        .background_task_labels
        .values()
        .filter(|c| c.is_running())
        .count()
}

pub(crate) fn background_chrome<'a>(
    state: &'a AppState,
    id: &str,
) -> Option<&'a BackgroundTaskChrome> {
    state.background_task_labels.get(id)
}

/// Whether the status strip has any child-agent row to focus. Background tasks
/// no longer occupy that strip, so they never make `WorkbenchFocus::Activity`
/// reachable.
pub(crate) fn has_status_activities(state: &AppState) -> bool {
    summaries(state)
        .iter()
        .any(|s| s.kind == ActivityKind::ChildAgent)
}

/// Aggregated background-job state for the input footer.
///
/// `None` when there is nothing to say: no running task and no unacknowledged
/// failure. A completed or stopped task never earns a footer slot — its place
/// is the list page's "recently finished" history. This is the whole reason
/// background work no longer renders as rows in the conversation body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackgroundFooterSummary {
    pub running: usize,
    /// The oldest running task's elapsed seconds, for the aggregate duration.
    pub oldest_running_secs: u64,
    /// The single running task's label, when exactly one is running.
    pub single_label: Option<String>,
    /// Failed tasks the user has not acknowledged yet.
    pub unread_failed: usize,
}

impl BackgroundFooterSummary {
    pub fn is_empty(&self) -> bool {
        self.running == 0 && self.unread_failed == 0
    }
}

pub(crate) fn footer_summary(state: &AppState) -> Option<BackgroundFooterSummary> {
    let now = state.elapsed_secs;
    let mut running = 0usize;
    let mut oldest = 0u64;
    let mut single: Option<String> = None;
    let mut unread_failed = 0usize;
    for (id, chrome) in &state.background_task_labels {
        match chrome.outcome() {
            crate::state::BackgroundOutcome::Running => {
                running += 1;
                let elapsed = now.saturating_sub(chrome.started_elapsed_secs);
                if running == 1 || elapsed > oldest {
                    oldest = elapsed;
                }
                single = Some(chrome.label.clone());
            }
            crate::state::BackgroundOutcome::Failed => {
                if !state.background_failures_seen.contains(id) {
                    unread_failed += 1;
                }
            }
            crate::state::BackgroundOutcome::Completed
            | crate::state::BackgroundOutcome::Stopped => {}
        }
    }
    let summary = BackgroundFooterSummary {
        running,
        oldest_running_secs: oldest,
        // A name is only shown when it is unambiguous; two tasks read as a
        // count, never as a rotating carousel.
        single_label: (running == 1).then_some(single).flatten(),
        unread_failed,
    };
    (!summary.is_empty()).then_some(summary)
}

/// Mark every currently-failed task as seen.
///
/// The footer badge is an attention signal, not durable data: acknowledging
/// clears the reminder only. Terminal state, exit code, output and history are
/// all untouched, so the list page still shows the failures.
pub(crate) fn acknowledge_failures(state: &mut AppState) {
    let failed: Vec<String> = state
        .background_task_labels
        .iter()
        .filter(|(id, chrome)| chrome.is_failed() && !state.background_failures_seen.contains(*id))
        .map(|(id, _)| id.clone())
        .collect();
    for id in failed {
        state.background_failures_seen.insert(id);
    }
}

/// Acknowledge one task's failure, if it has one. Used when its detail opens.
pub(crate) fn acknowledge_failure(state: &mut AppState, id: &str) {
    if state
        .background_task_labels
        .get(id)
        .is_some_and(|c| c.is_failed())
    {
        state.background_failures_seen.insert(id.to_string());
    }
}

/// The background-jobs list page's two ordered sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackgroundJobList {
    /// Still running, oldest first (the durable servers lead).
    pub running: Vec<ActivitySummary>,
    /// Recently finished, newest first.
    pub finished: Vec<ActivitySummary>,
}

impl BackgroundJobList {
    pub fn is_empty(&self) -> bool {
        self.running.is_empty() && self.finished.is_empty()
    }

    /// Flat row order the selection index addresses: running, then finished.
    pub fn rows(&self) -> impl Iterator<Item = &ActivitySummary> {
        self.running.iter().chain(self.finished.iter())
    }
}

pub(crate) fn background_job_list(state: &AppState) -> BackgroundJobList {
    let mut running = Vec::new();
    let mut finished = Vec::new();
    for summary in summaries(state)
        .into_iter()
        .filter(|s| s.kind == ActivityKind::BackgroundTask)
    {
        match summary.status {
            ActivityStatus::Running => running.push(summary),
            _ => finished.push(summary),
        }
    }
    BackgroundJobList { running, finished }
}

pub(crate) fn ensure_list_selection(state: &mut AppState) {
    let list = background_job_list(state);
    if list.is_empty() {
        state.background_list_selected = None;
        return;
    }
    if state
        .background_list_selected
        .as_ref()
        .is_none_or(|id| !list.rows().any(|s| s.id.as_key() == id))
    {
        state.background_list_selected = list.rows().next().map(|s| s.id.as_key().to_string());
    }
}

pub(crate) fn move_list_selection(state: &mut AppState, delta: isize) {
    ensure_list_selection(state);
    let list = background_job_list(state);
    if list.is_empty() {
        return;
    }
    let ids: Vec<String> = list.rows().map(|s| s.id.as_key().to_string()).collect();
    let current = state
        .background_list_selected
        .as_ref()
        .and_then(|id| ids.iter().position(|c| c == id))
        .unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(ids.len().saturating_sub(1))
    };
    state.background_list_selected = ids.get(next).cloned();
}

pub(crate) fn selected_list_task(state: &AppState) -> Option<String> {
    state.background_list_selected.clone()
}

/// Open the background-jobs list. Opening is what acknowledges the current
/// failures, so returning to the conversation no longer shows the badge while
/// the task history is unchanged.
pub(crate) fn open_background_list(state: &mut AppState) {
    acknowledge_failures(state);
    ensure_list_selection(state);
    state.active_screen = crate::screen::Screen::ActivityList;
    state.screen_scroll = 0;
}

pub(crate) fn close_background_list(state: &mut AppState) {
    state.active_screen = crate::screen::Screen::Conversation;
    state.screen_scroll = 0;
}

/// Open the selected list row's detail page.
pub(crate) fn open_list_selected(state: &mut AppState) -> Vec<crate::action::Effect> {
    ensure_list_selection(state);
    let Some(id) = selected_list_task(state) else {
        return Vec::new();
    };
    open(state, ActivityId::Background(id))
}

/// Status-strip activity lines and the summary index each line belongs to.
///
/// Only child agents reach this strip. Background tasks are aggregated into
/// the input footer ([`footer_summary`]) and listed on the background-jobs
/// page, so a long-lived process never occupies a row in the conversation
/// body. When the agent is actively waiting on a task, `wait_status` still
/// names it — that is main execution state, not the background summary.
pub(crate) fn status_activity_lines(
    state: &AppState,
    width: usize,
    t: &UiText,
) -> Vec<ActivityRow> {
    let all: Vec<ActivitySummary> = summaries(state)
        .into_iter()
        .filter(|s| s.kind == ActivityKind::ChildAgent)
        .collect();
    if all.is_empty() {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for row in all.iter().take(MAX_STATUS_ROWS) {
        let selected = state.activity_selected.as_ref() == Some(&row.id);
        rows.push(ActivityRow {
            text: compact_row(row, selected, width),
            id: Some(row.id.clone()),
            selected,
        });
        if let Some(text) = child_task_line(row, width) {
            rows.push(ActivityRow {
                text,
                id: Some(row.id.clone()),
                selected,
            });
        }
    }
    let hidden = all.len().saturating_sub(MAX_STATUS_ROWS);
    if hidden > 0 {
        rows.push(ActivityRow {
            text: truncate_display(
                &t.activity_more.replace("{}", &hidden.to_string()),
                width.max(1),
            ),
            id: None,
            selected: false,
        });
    }
    rows
}

pub(crate) fn select_delta(state: &mut AppState, delta: isize) {
    let all: Vec<ActivitySummary> = summaries(state)
        .into_iter()
        .filter(|s| s.kind == ActivityKind::ChildAgent)
        .collect();
    if all.is_empty() {
        state.activity_selected = None;
        return;
    }
    let current = state
        .activity_selected
        .as_ref()
        .and_then(|id| all.iter().position(|s| &s.id == id))
        .unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(all.len().saturating_sub(1))
    };
    state.activity_selected = all.get(next).map(|s| s.id.clone());
}

pub(crate) fn open_selected(state: &mut AppState) -> Vec<crate::action::Effect> {
    ensure_selection(state);
    let Some(id) = state.activity_selected.clone() else {
        return Vec::new();
    };
    open(state, id)
}

pub(crate) fn open(state: &mut AppState, id: ActivityId) -> Vec<crate::action::Effect> {
    state.activity_open = Some(id.clone());
    state.activity_selected = Some(id.clone());
    state.active_screen = crate::screen::Screen::Activity;
    state.screen_scroll = 0;
    // Opening starts at the newest output, following it.
    state.activity_view = ActivityView::default();
    match id {
        ActivityId::Child(child_id) => {
            let already = state
                .team
                .children
                .iter()
                .any(|c| c.id == child_id && c.detail.is_some());
            if already {
                Vec::new()
            } else {
                vec![crate::action::Effect::Send(
                    leveler_client_protocol::ClientCommand::QueryChildContribution {
                        session_id: state.session_id.clone(),
                        child_id,
                        query_id: Some(leveler_client_protocol::CommandId::generate()),
                    },
                )]
            }
        }
        ActivityId::Background(task_id) => {
            // Opening a failed task is seeing it: clear its footer reminder
            // without touching the terminal record.
            acknowledge_failure(state, &task_id);
            Vec::new()
        }
    }
}

pub(crate) fn close(state: &mut AppState) {
    state.activity_open = None;
    state.active_screen = crate::screen::Screen::Conversation;
    state.screen_scroll = 0;
    state.activity_view = ActivityView::default();
}

/// The Activity Detail viewport. Mirrors the conversation viewport's follow
/// semantics: auto-follow sticks to the newest output, scrolling back pauses
/// it and counts what arrived, and jumping to the bottom resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityView {
    /// Offset (in content lines) from the top while the user reads back.
    pub scroll: usize,
    /// Stick to the newest output while true.
    pub follow: bool,
    /// Content lines that arrived while follow was paused.
    pub unread: usize,
    /// Last measured content line count, to detect growth while paused.
    pub last_len: usize,
    /// Last measured maximum scroll, published by the renderer each frame.
    pub max_scroll: usize,
    /// Last measured viewport height in rows, for page-sized scrolls.
    pub viewport_height: usize,
}

impl Default for ActivityView {
    fn default() -> Self {
        Self {
            scroll: 0,
            follow: true,
            unread: 0,
            last_len: 0,
            max_scroll: 0,
            viewport_height: 0,
        }
    }
}

/// Publish this frame's measured geometry and track growth while the user is
/// reading back. Called by the renderer, which is the only place the content
/// height is known. Returns true when a repaint is due.
pub(crate) fn sync_view(state: &mut AppState, total: usize, height: usize) -> bool {
    let max_scroll = total.saturating_sub(height);
    let view = &mut state.activity_view;
    let mut changed = false;
    if view.viewport_height != height {
        view.viewport_height = height;
        changed = true;
    }
    if view.max_scroll != max_scroll {
        view.max_scroll = max_scroll;
        changed = true;
    }
    if view.follow {
        if view.scroll != max_scroll {
            view.scroll = max_scroll;
            changed = true;
        }
        if view.unread != 0 {
            view.unread = 0;
            changed = true;
        }
    } else if total > view.last_len {
        view.unread = view.unread.saturating_add(total - view.last_len);
        changed = true;
    }
    if view.last_len != total {
        view.last_len = total;
        changed = true;
    }
    changed
}

/// Scroll the Activity Detail by `delta` lines (negative = up). Scrolling up
/// leaves follow; reaching the bottom resumes it and clears the unread count.
pub(crate) fn scroll_lines(state: &mut AppState, delta: isize) {
    let view = &mut state.activity_view;
    if delta < 0 {
        view.follow = false;
        view.scroll = view
            .scroll
            .saturating_sub(delta.unsigned_abs())
            .min(view.max_scroll);
    } else {
        view.scroll = (view.scroll + delta as usize).min(view.max_scroll);
        if view.scroll >= view.max_scroll {
            view.follow = true;
            view.unread = 0;
        }
    }
}

/// Page the Activity Detail. Empty viewports still advance by one row so a
/// page key never becomes a no-op.
pub(crate) fn scroll_page(state: &mut AppState, direction: isize) {
    let page = state.activity_view.viewport_height.saturating_sub(1).max(1);
    scroll_lines(state, direction * page as isize);
}

/// `Home` / `g`: jump to the first line and stop following.
pub(crate) fn to_top(state: &mut AppState) {
    state.activity_view.follow = false;
    state.activity_view.scroll = 0;
}

/// `End` / `G`: jump to the newest line and resume following.
pub(crate) fn to_bottom(state: &mut AppState) {
    state.activity_view.follow = true;
    state.activity_view.unread = 0;
    state.activity_view.scroll = state.activity_view.max_scroll;
}

/// Cap on terminal background tasks kept reopenable. Live tasks are bounded
/// by the runtime's concurrency limit; terminal ones would otherwise
/// accumulate over a long session with many sequential commands. This is the
/// list page's "recently finished" window; the runtime registry and the
/// transcript remain the durable history behind it.
const MAX_TERMINAL_BACKGROUND: usize = 8;

/// Drop the oldest terminal entries past the cap so a long session cannot grow
/// the projection without bound. Ordered by start time, which is monotonic on
/// the turn clock. A dropped entry that is still open falls back to the stale
/// view. The durable history is the transcript and the runtime registry.
pub(crate) fn bound_terminal_background(state: &mut AppState) {
    let mut terminal: Vec<(String, u64)> = state
        .background_task_labels
        .iter()
        .filter(|(_, c)| !c.is_running())
        .map(|(id, c)| (id.clone(), c.started_elapsed_secs))
        .collect();
    let excess = terminal.len().saturating_sub(MAX_TERMINAL_BACKGROUND);
    if excess == 0 {
        return;
    }
    terminal.sort_by_key(|(_, started)| *started);
    for (id, _) in terminal.into_iter().take(excess) {
        state.background_task_labels.remove(&id);
        state.background_failures_seen.remove(&id);
        if matches!(&state.background_list_selected, Some(sel) if sel == &id) {
            state.background_list_selected = None;
        }
        if matches!(&state.activity_open, Some(ActivityId::Background(open)) if open == &id) {
            state.activity_open = None;
        }
        if matches!(&state.activity_selected, Some(ActivityId::Background(sel)) if sel == &id) {
            state.activity_selected = None;
        }
    }
}

pub(crate) fn ensure_selection(state: &mut AppState) {
    let all: Vec<ActivitySummary> = summaries(state)
        .into_iter()
        .filter(|s| s.kind == ActivityKind::ChildAgent)
        .collect();
    if all.is_empty() {
        state.activity_selected = None;
        if state.workbench_focus == crate::state::WorkbenchFocus::Activity {
            state.workbench_focus = crate::state::WorkbenchFocus::Input;
        }
        return;
    }
    if state
        .activity_selected
        .as_ref()
        .is_none_or(|id| !all.iter().any(|s| &s.id == id))
    {
        state.activity_selected = Some(all[0].id.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Boot;
    use crate::theme::Theme;
    use leveler_client_protocol::{RuntimeStatus, SessionId};

    fn test_state() -> AppState {
        AppState::new(
            Theme::no_color(),
            Boot {
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
        )
    }

    #[test]
    fn background_activity_row_render() {
        let mut state = test_state();
        state.elapsed_secs = 192;
        state.background_task_labels.insert(
            "bg-2".into(),
            BackgroundTaskChrome::running("cargo test --workspace", 10),
        );
        let rows = summaries(&state);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, ActivityKind::BackgroundTask);
        assert_eq!(rows[0].status, ActivityStatus::Running);
        assert_eq!(rows[0].title, "cargo test --workspace");
        assert_eq!(rows[0].duration_secs, 182);
        let line = compact_row(&rows[0], false, 80);
        assert!(line.contains("cargo test --workspace"), "{line}");
        assert!(line.contains('↗'), "{line}");
        assert!(line.contains('●'), "{line}");
        assert!(!line.contains("bg-2"), "{line}");
    }

    /// The activity projection reads the live roster, so retiring a settled
    /// child at the turn boundary removes it from the current activity area —
    /// its transcript history is untouched.
    #[test]
    fn a_retired_child_leaves_the_activity_projection() {
        let mut state = test_state();
        state.elapsed_secs = 100;
        state.team.apply_update(crate::multi_agent::ChildUpdate {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            done: false,
            ok: false,
            detail: "加固 extract 模块".into(),
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: false,
            contribution: None,
            stop: None,
            limit: None,
            started_elapsed_secs: 0,
        });
        state.team.apply_update(crate::multi_agent::ChildUpdate {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            done: true,
            ok: true,
            detail: "完成".into(),
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: false,
            contribution: None,
            stop: None,
            limit: None,
            started_elapsed_secs: 100,
        });
        assert!(
            summaries(&state)
                .iter()
                .any(|r| r.id == ActivityId::Child("c1".into())),
            "a settled child is current activity until its turn ends"
        );

        assert!(state.team.retire_settled(state.elapsed_secs));
        assert!(
            summaries(&state).is_empty(),
            "the previous turn's terminal child is not this turn's activity: {:?}",
            summaries(&state)
        );
    }

    /// Three children spawned into the same repository read as three copies of
    /// the same absolute path: the model opens every brief with "你在仓库
    /// <repo>（工作区根目录）中…", which fills the row before the reason starts.
    #[test]
    fn a_child_row_spends_its_width_on_the_reason_not_the_repo_path() {
        let mut state = test_state();
        state.repository = "/private/tmp/agent-501/-Users-example-long-scratch/example-etl".into();
        state.elapsed_secs = 10;
        state.team.apply_update(crate::multi_agent::ChildUpdate {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: "default".into(),
            done: false,
            ok: false,
            detail: "你在仓库 /private/tmp/agent-501/-Users-example-long-scratch/example-etl（工作区根目录）中加固 ETL 的 extract 模块。".into(),
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: false,
            contribution: None,
            stop: None,
            limit: None,
            started_elapsed_secs: 0,
        });
        let rows = crate::activity::status_activity_lines(&state, 80, state.t());
        let header = &rows[0].text;
        let task = rows
            .iter()
            .map(|r| r.text.as_str())
            .find(|t| t.contains("extract"))
            .expect("the task line");
        for line in [header, task] {
            assert!(
                unicode_width::UnicodeWidthStr::width(line) <= 80,
                "the row must budget display columns, not bytes: {line}"
            );
        }
        assert!(header.contains("Euclid"), "{header}");
        assert!(
            !header.contains("/private/tmp"),
            "the repo path never spends the header: {header}"
        );
        assert!(!task.contains("/private/tmp"), "{task}");
        assert!(
            task.contains("example-etl"),
            "the repository is still named: {task}"
        );
        assert!(task.contains("extract"), "{task}");
    }

    /// A child that finished took the time it took. The row followed the turn
    /// clock instead, so a 100-second analyze read "7m 10s" ten minutes later,
    /// beside its own ✓.
    #[test]
    fn a_settled_child_keeps_the_time_it_took() {
        let mut state = test_state();
        state.elapsed_secs = 100;
        state.team.apply_update(crate::multi_agent::ChildUpdate {
            id: "c1".into(),
            nickname: "analyze".into(),
            role: "reviewer".into(),
            done: false,
            ok: false,
            detail: "读代码".into(),
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: true,
            contribution: None,
            stop: None,
            limit: None,
            started_elapsed_secs: 100,
        });
        state.elapsed_secs = 200;
        state.team.apply_update(crate::multi_agent::ChildUpdate {
            id: "c1".into(),
            nickname: "analyze".into(),
            role: "reviewer".into(),
            done: true,
            ok: true,
            detail: String::new(),
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: true,
            contribution: None,
            stop: None,
            limit: None,
            started_elapsed_secs: 200,
        });

        // The turn runs on for another ten minutes.
        state.elapsed_secs = 800;
        let rows = summaries(&state);
        let child = rows
            .iter()
            .find(|r| r.kind == ActivityKind::ChildAgent)
            .expect("the child row");
        assert_eq!(child.status, ActivityStatus::Completed);
        assert_eq!(child.duration_secs, 100, "{child:?}");
    }

    #[test]
    fn child_activity_row_render() {
        let mut state = test_state();
        state.elapsed_secs = 58;
        state
            .team
            .children
            .push(crate::multi_agent::ChildAgentView {
                id: "child-uuid-hidden".into(),
                nickname: "Worker".into(),
                role: "执行 Agent".into(),
                profile_id: Some("worker".into()),
                agent_name: None,
                read_only: false,
                title: None,
                purpose: "修复 permission inheritance".into(),
                status: ChildStatus::Running,
                contribution: crate::multi_agent::Contribution::Pending,
                recent_step: Some("read_file".into()),
                input_tokens: 0,
                output_tokens: 0,
                started_elapsed_secs: 0,
                settled_elapsed_secs: None,
                detail: None,
                activity: Vec::new(),
                stop: None,
                limit: None,
            });
        let summary = summaries(&state);
        assert_eq!(summary[0].kind, ActivityKind::ChildAgent);
        assert_eq!(summary[0].status, ActivityStatus::Running);
        let rows = crate::activity::status_activity_lines(&state, 80, state.t());
        assert_eq!(rows.len(), 2, "identity and task take one line each");
        let header = &rows[0].text;
        let task = &rows[1].text;
        assert!(header.contains("Worker"), "{header}");
        assert!(header.contains('↗'), "{header}");
        assert!(!header.contains("child-uuid-hidden"), "{header}");
        assert!(task.contains("permission inheritance"), "{task}");
        assert!(!task.contains("child-uuid-hidden"), "{task}");
    }

    /// The header is identity + duration + affordance, in that order and
    /// adjacent — the duration is part of the identity group, not pushed to
    /// the terminal's right edge.
    #[test]
    fn a_child_header_keeps_the_duration_beside_the_identity() {
        let mut state = test_state();
        state.elapsed_secs = 1266; // 21m06s
        state
            .team
            .children
            .push(crate::multi_agent::ChildAgentView {
                id: "c1".into(),
                nickname: "Euclid".into(),
                role: "explorer".into(),
                profile_id: None,
                agent_name: None,
                read_only: true,
                title: Some("调查 Windows CI 两个 flaky tests".into()),
                purpose: "你在 CodeLeveler 仓库里做一次只读调查，目标是解释 Windows CI 上两个测试的偶发失败……".into(),
                status: ChildStatus::Running,
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
        let rows = crate::activity::status_activity_lines(&state, 120, state.t());
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].text, "  ● Euclid · 21m 06s ↗", "{rows:?}");
        assert_eq!(
            rows[1].text, "    调查 Windows CI 两个 flaky tests",
            "{rows:?}"
        );
        assert_eq!(rows[0].id, rows[1].id, "both lines open the same child");
    }

    /// A child recorded before titles existed has no semantic title; the task
    /// line falls back to its purpose rather than hiding the row's reason.
    #[test]
    fn a_titleless_child_falls_back_to_its_purpose() {
        let mut state = test_state();
        state
            .team
            .children
            .push(crate::multi_agent::ChildAgentView {
                id: "c1".into(),
                nickname: "Newton".into(),
                role: "explorer".into(),
                profile_id: None,
                agent_name: None,
                read_only: true,
                title: None,
                purpose: "audit the refund path
second line ignored"
                    .into(),
                status: ChildStatus::Running,
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
        let rows = crate::activity::status_activity_lines(&state, 120, state.t());
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[1].text, "    audit the refund path", "{rows:?}");
    }

    #[test]
    fn child_completed_stays_reopenable() {
        let mut state = test_state();
        state
            .team
            .children
            .push(crate::multi_agent::ChildAgentView {
                id: "c1".into(),
                nickname: "Worker".into(),
                role: "执行 Agent".into(),
                profile_id: None,
                agent_name: None,
                read_only: false,
                title: None,
                purpose: "检查 CI".into(),
                status: ChildStatus::Completed,
                contribution: crate::multi_agent::Contribution::NothingToFlag,
                recent_step: None,
                input_tokens: 10,
                output_tokens: 4,
                started_elapsed_secs: 0,
                settled_elapsed_secs: None,
                detail: None,
                activity: Vec::new(),
                stop: None,
                limit: None,
            });
        let rows = summaries(&state);
        assert_eq!(rows[0].status, ActivityStatus::Completed);
        assert!(state.team.children.iter().any(|c| c.id == "c1"));
    }

    #[test]
    fn running_sorts_before_completed() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "old".into(),
            BackgroundTaskChrome {
                label: "cargo check".into(),
                started_elapsed_secs: 0,
                ok: Some(true),
                stopped: false,
                exit_code: Some(0),
                duration_ms: Some(1000),
                output: String::new(),
            },
        );
        state.background_task_labels.insert(
            "live".into(),
            BackgroundTaskChrome::running("cargo test --workspace", 10),
        );
        let rows = summaries(&state);
        assert_eq!(rows[0].title, "cargo test --workspace");
        assert_eq!(rows[1].title, "cargo check");
    }

    #[test]
    fn activity_row_narrow_width() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-2".into(),
            BackgroundTaskChrome::running(
                "cargo test --workspace --all-features --verbose extra",
                0,
            ),
        );
        let line = compact_row(&summaries(&state)[0], false, 24);
        assert!(
            unicode_width::UnicodeWidthStr::width(line.as_str()) <= 24,
            "{line}"
        );
        assert!(line.contains('↗'), "{line}");
    }

    #[test]
    fn safe_display_does_not_surface_raw_json() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-2".into(),
            BackgroundTaskChrome::running("cargo test --workspace", 0),
        );
        let line = compact_row(&summaries(&state)[0], false, 80);
        assert!(!line.contains('{'), "{line}");
        assert!(!line.contains("task_id"), "{line}");
    }

    #[test]
    fn activity_detail_open_close() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-2".into(),
            BackgroundTaskChrome::running("cargo test --workspace", 0),
        );
        // Background rows are not in the status strip, so the detail opens from
        // the jobs list (or a footer click) by id, not via `open_selected`.
        let effects = crate::activity::open(&mut state, ActivityId::Background("bg-2".into()));
        assert!(
            effects.is_empty(),
            "opening a background row sends no command"
        );
        assert_eq!(state.active_screen, crate::screen::Screen::Activity);
        assert_eq!(
            state.activity_open,
            Some(ActivityId::Background("bg-2".into()))
        );
        crate::activity::close(&mut state);
        assert_eq!(state.active_screen, crate::screen::Screen::Conversation);
        assert!(state.activity_open.is_none());
        assert!(
            state
                .background_task_labels
                .get("bg-2")
                .unwrap()
                .is_running(),
            "close must not cancel the background task"
        );
    }

    #[test]
    fn activity_detail_live_lifecycle_update() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-2".into(),
            BackgroundTaskChrome::running("cargo test --workspace", 0),
        );
        let _ = crate::activity::open(&mut state, ActivityId::Background("bg-2".into()));
        assert_eq!(summaries(&state)[0].status, ActivityStatus::Running);
        let chrome = state.background_task_labels.get_mut("bg-2").unwrap();
        chrome.ok = Some(true);
        chrome.exit_code = Some(0);
        chrome.duration_ms = Some(8000);
        chrome.output.push_str("test result: ok\n");
        assert_eq!(
            state.activity_open,
            Some(ActivityId::Background("bg-2".into()))
        );
        let row = summaries(&state);
        assert_eq!(row[0].status, ActivityStatus::Completed);
        assert_eq!(
            background_chrome(&state, "bg-2").unwrap().output,
            "test result: ok\n"
        );
    }

    #[test]
    fn background_detail_live_output() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-2".into(),
            BackgroundTaskChrome::running("cargo test --workspace", 0),
        );
        let _ = crate::activity::open(&mut state, ActivityId::Background("bg-2".into()));
        assert!(background_chrome(&state, "bg-2").unwrap().output.is_empty());
        state
            .background_task_labels
            .get_mut("bg-2")
            .unwrap()
            .output
            .push_str("Compiling leveler-core\n");
        assert!(
            background_chrome(&state, "bg-2")
                .unwrap()
                .output
                .contains("Compiling leveler-core")
        );
    }

    #[test]
    fn activity_stale_reference_is_graceful() {
        let mut state = test_state();
        let _ = crate::activity::open(&mut state, ActivityId::Background("missing".into()));
        assert_eq!(state.active_screen, crate::screen::Screen::Activity);
        assert!(summaries(&state).is_empty());
    }

    #[test]
    fn wait_status_activity_link() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.elapsed_secs = 20;
        state.background_task_labels.insert(
            "bg-a".into(),
            BackgroundTaskChrome::running("cargo test --workspace", 0),
        );
        state.transcript.push_tool_started(
            leveler_client_protocol::ToolCallId::new("w1"),
            "wait_task".into(),
            serde_json::json!({ "task_id": "bg-a" }).to_string(),
            false,
            2,
        );
        let wait = crate::wait_status::project(&state).expect("wait");
        assert_eq!(wait.kind, crate::wait_status::WaitKind::BackgroundTask);
        let rows = summaries(&state);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, ActivityId::Background("bg-a".into()));
        assert_eq!(rows[0].status, ActivityStatus::Running);
    }

    #[test]
    fn child_wait_activity_link() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.transcript.push_tool_started(
            leveler_client_protocol::ToolCallId::new("s1"),
            "spawn_agent".into(),
            serde_json::json!({ "profile": "worker", "task": "check" }).to_string(),
            false,
            0,
        );
        state
            .team
            .children
            .push(crate::multi_agent::ChildAgentView {
                id: "c-worker".into(),
                nickname: "Worker".into(),
                role: "执行 Agent".into(),
                profile_id: Some("worker".into()),
                agent_name: None,
                read_only: false,
                title: None,
                purpose: "检查 Runtime permission".into(),
                status: ChildStatus::Running,
                contribution: crate::multi_agent::Contribution::Pending,
                recent_step: Some("read_file".into()),
                input_tokens: 0,
                output_tokens: 0,
                started_elapsed_secs: 0,
                settled_elapsed_secs: None,
                detail: None,
                activity: Vec::new(),
                stop: None,
                limit: None,
            });
        let wait = crate::wait_status::project(&state).expect("child wait");
        assert_eq!(wait.kind, crate::wait_status::WaitKind::ChildAgent);
        let rows = summaries(&state);
        assert_eq!(rows[0].id, ActivityId::Child("c-worker".into()));
        let effects = crate::activity::open(&mut state, ActivityId::Child("c-worker".into()));
        assert_eq!(state.active_screen, crate::screen::Screen::Activity);
        assert!(
            effects.iter().any(|e| matches!(
                e,
                crate::action::Effect::Send(
                    leveler_client_protocol::ClientCommand::QueryChildContribution { .. }
                )
            )),
            "{effects:?}"
        );
        assert!(
            !effects
                .iter()
                .any(|e| format!("{e:?}").to_ascii_lowercase().contains("cancel")),
            "open must not cancel: {effects:?}"
        );
    }

    #[test]
    fn child_detail_does_not_include_reasoning_fields() {
        let mut state = test_state();
        state.live_reasoning = "hidden chain of thought".into();
        state
            .team
            .children
            .push(crate::multi_agent::ChildAgentView {
                id: "c1".into(),
                nickname: "Worker".into(),
                role: "执行 Agent".into(),
                profile_id: None,
                agent_name: None,
                read_only: false,
                title: None,
                purpose: "修权限".into(),
                status: ChildStatus::Running,
                contribution: crate::multi_agent::Contribution::Pending,
                recent_step: Some("read_file".into()),
                input_tokens: 0,
                output_tokens: 0,
                started_elapsed_secs: 0,
                settled_elapsed_secs: None,
                detail: None,
                activity: Vec::new(),
                stop: None,
                limit: None,
            });
        let row = &summaries(&state)[0];
        let line = compact_row(row, false, 80);
        assert!(!line.contains("hidden chain"), "{line}");
        assert!(!line.contains("chain of thought"), "{line}");
    }

    #[test]
    fn running_background_does_not_force_main_wait() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.activity = Some("正在汇总审计结果".into());
        state.background_task_labels.insert(
            "bg-1".into(),
            BackgroundTaskChrome::running("cargo test --workspace", 0),
        );
        assert!(crate::wait_status::project(&state).is_none());
        assert_eq!(summaries(&state)[0].status, ActivityStatus::Running);
    }

    /// The viewport follows the tail until the user leaves the bottom, then
    /// freezes where they are reading and counts what arrived behind them.
    #[test]
    fn the_detail_viewport_follows_then_freezes_where_the_user_scrolled() {
        let mut state = test_state();
        assert!(sync_view(&mut state, 100, 10));
        assert!(state.activity_view.follow);
        assert_eq!(state.activity_view.scroll, 90, "following sits at the tail");

        // New output while following keeps the viewport pinned to the tail.
        sync_view(&mut state, 103, 10);
        assert_eq!(state.activity_view.scroll, 93);
        assert_eq!(state.activity_view.unread, 0);

        // One key up leaves follow and pins the position.
        scroll_lines(&mut state, -1);
        assert!(!state.activity_view.follow);
        assert_eq!(state.activity_view.scroll, 92);

        // New output while paused must not move the viewport.
        sync_view(&mut state, 110, 10);
        assert_eq!(state.activity_view.scroll, 92, "paused viewport is frozen");
        assert_eq!(
            state.activity_view.unread, 7,
            "the reader is told what arrived"
        );

        // End jumps to the newest line and resumes follow.
        to_bottom(&mut state);
        assert!(state.activity_view.follow);
        assert_eq!(state.activity_view.unread, 0);
        assert_eq!(state.activity_view.scroll, 100);
    }

    #[test]
    fn home_and_page_keys_move_the_detail_viewport() {
        let mut state = test_state();
        sync_view(&mut state, 100, 10);
        assert_eq!(state.activity_view.max_scroll, 90);
        to_top(&mut state);
        assert!(!state.activity_view.follow);
        assert_eq!(state.activity_view.scroll, 0);
        scroll_page(&mut state, 1);
        assert_eq!(state.activity_view.scroll, 9);
        scroll_page(&mut state, -1);
        assert_eq!(state.activity_view.scroll, 0);
        // Page-down past the bottom resumes follow.
        scroll_page(&mut state, 1);
        scroll_page(&mut state, 1);
        assert_eq!(state.activity_view.scroll, 18);
        to_bottom(&mut state);
        scroll_page(&mut state, 1);
        assert!(state.activity_view.follow);
    }

    /// A finished task stays in the Activity projection so its row — and the
    /// detail behind it — remain reopenable, but it is not counted as running.
    #[test]
    fn a_terminal_background_task_stays_reopenable_but_is_not_running() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-done".into(),
            BackgroundTaskChrome {
                label: "cargo test --workspace".into(),
                started_elapsed_secs: 0,
                ok: Some(true),
                stopped: false,
                exit_code: Some(0),
                duration_ms: Some(8_000),
                output: "test result: ok\n".into(),
            },
        );
        assert_eq!(running_background_count(&state), 0);
        let rows = summaries(&state);
        assert!(
            rows.iter()
                .any(|r| r.id == ActivityId::Background("bg-done".into()))
        );
        assert_eq!(rows[0].status, ActivityStatus::Completed);
        assert!(compact_row(&rows[0], false, 80).contains('↗'));
    }

    /// Finished tasks do not retire at a turn boundary: they are the list
    /// page's "recently finished" history, reopenable after their turn ends.
    #[test]
    fn a_terminal_background_task_stays_reopenable_across_turns() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-server".into(),
            BackgroundTaskChrome::running("npm run dev", 0),
        );
        state.background_task_labels.insert(
            "bg-done".into(),
            BackgroundTaskChrome {
                label: "cargo test".into(),
                started_elapsed_secs: 0,
                ok: Some(true),
                stopped: false,
                exit_code: Some(0),
                duration_ms: Some(1_000),
                output: String::new(),
            },
        );
        // `bound_terminal_background` is the only pruning left, and it keeps
        // terminal entries under the cap.
        bound_terminal_background(&mut state);
        assert!(state.background_task_labels.contains_key("bg-server"));
        assert!(state.background_task_labels.contains_key("bg-done"));
        let list = background_job_list(&state);
        assert_eq!(list.running.len(), 1);
        assert_eq!(list.finished.len(), 1);
    }

    /// The retained terminal set is bounded so a long turn with many sequential
    /// commands cannot grow the projection without limit.
    #[test]
    fn bounded_terminal_background_drops_the_oldest() {
        let mut state = test_state();
        for i in 0..(MAX_TERMINAL_BACKGROUND + 2) {
            state.background_task_labels.insert(
                format!("bg-{i}"),
                BackgroundTaskChrome {
                    label: format!("cmd {i}"),
                    started_elapsed_secs: i as u64,
                    ok: Some(true),
                    stopped: false,
                    exit_code: Some(0),
                    duration_ms: Some(1_000),
                    output: String::new(),
                },
            );
        }
        bound_terminal_background(&mut state);
        assert_eq!(
            state.background_task_labels.len(),
            MAX_TERMINAL_BACKGROUND,
            "oldest terminal entries are dropped"
        );
        assert!(!state.background_task_labels.contains_key("bg-0"));
        assert!(!state.background_task_labels.contains_key("bg-1"));
        assert!(state.background_task_labels.contains_key("bg-2"));
    }

    // ── Footer aggregation and failure acknowledgement ─────────────────────

    fn finished(
        label: &str,
        ok: bool,
        stopped: bool,
        started: u64,
        ms: u64,
    ) -> BackgroundTaskChrome {
        BackgroundTaskChrome {
            label: label.into(),
            started_elapsed_secs: started,
            ok: Some(ok),
            stopped,
            exit_code: if ok { Some(0) } else { Some(1) },
            duration_ms: Some(ms),
            output: String::new(),
        }
    }

    /// Case A: nothing to say. A completed or stopped task alone earns no
    /// footer slot.
    #[test]
    fn footer_summary_is_none_without_running_or_unread_failures() {
        let mut state = test_state();
        state.elapsed_secs = 90;
        assert!(footer_summary(&state).is_none());
        state.background_task_labels.insert(
            "bg-done".into(),
            finished("cargo check", true, false, 0, 18_000),
        );
        state.background_task_labels.insert(
            "bg-stopped".into(),
            finished("npm start", false, true, 0, 24_000),
        );
        assert!(
            footer_summary(&state).is_none(),
            "completed and stopped tasks are not footer activity"
        );
    }

    /// Case B: one running task is named.
    #[test]
    fn footer_summary_names_a_single_running_task() {
        let mut state = test_state();
        state.elapsed_secs = 90;
        state
            .background_task_labels
            .insert("bg-1".into(), BackgroundTaskChrome::running("make up", 28));
        let summary = footer_summary(&state).expect("summary");
        assert_eq!(summary.running, 1);
        assert_eq!(summary.single_label.as_deref(), Some("make up"));
        assert_eq!(summary.oldest_running_secs, 62);
        assert_eq!(summary.unread_failed, 0);
    }

    /// Case C: several running tasks aggregate to a count, and the duration is
    /// the oldest running task's, never a carousel of names.
    #[test]
    fn footer_summary_aggregates_several_running_tasks() {
        let mut state = test_state();
        state.elapsed_secs = 90;
        state
            .background_task_labels
            .insert("bg-1".into(), BackgroundTaskChrome::running("make up", 10));
        state.background_task_labels.insert(
            "bg-2".into(),
            BackgroundTaskChrome::running("cargo test", 30),
        );
        let summary = footer_summary(&state).expect("summary");
        assert_eq!(summary.running, 2);
        assert!(summary.single_label.is_none(), "a name is ambiguous");
        assert_eq!(summary.oldest_running_secs, 80);
    }

    /// Case D/E: running and failed are separate counts.
    #[test]
    fn footer_summary_keeps_running_and_failed_separate() {
        let mut state = test_state();
        state.elapsed_secs = 90;
        state
            .background_task_labels
            .insert("bg-1".into(), BackgroundTaskChrome::running("make up", 10));
        state.background_task_labels.insert(
            "bg-2".into(),
            BackgroundTaskChrome::running("cargo test", 20),
        );
        state.background_task_labels.insert(
            "bg-3".into(),
            finished("npm start", false, false, 0, 30_000),
        );
        let summary = footer_summary(&state).expect("summary");
        assert_eq!(summary.running, 2);
        assert_eq!(summary.unread_failed, 1);
    }

    /// The authoritative stop distinction: a killed task is not a failure.
    #[test]
    fn a_stopped_task_is_never_a_failure() {
        let mut state = test_state();
        state.elapsed_secs = 90;
        state
            .background_task_labels
            .insert("bg-1".into(), finished("npm start", false, true, 0, 24_000));
        let chrome = state.background_task_labels.get("bg-1").unwrap();
        assert_eq!(chrome.outcome(), crate::state::BackgroundOutcome::Stopped);
        assert!(!chrome.is_failed());
        assert!(footer_summary(&state).is_none());
        assert_eq!(
            summaries(&state)[0].status,
            ActivityStatus::Stopped,
            "the view maps the runtime Killed state to Stopped"
        );
    }

    /// An unexpected non-zero exit IS a failure.
    #[test]
    fn an_unexpected_nonzero_exit_is_a_failure() {
        let mut state = test_state();
        state.elapsed_secs = 90;
        state.background_task_labels.insert(
            "bg-1".into(),
            finished("npm start", false, false, 0, 30_000),
        );
        let chrome = state.background_task_labels.get("bg-1").unwrap();
        assert_eq!(chrome.outcome(), crate::state::BackgroundOutcome::Failed);
        assert!(chrome.is_failed());
        assert_eq!(footer_summary(&state).unwrap().unread_failed, 1);
    }

    /// Acknowledging clears the reminder only: terminal state, exit code and
    /// history are untouched.
    #[test]
    fn acknowledging_failures_clears_the_badge_but_keeps_history() {
        let mut state = test_state();
        state.elapsed_secs = 90;
        state.background_task_labels.insert(
            "bg-1".into(),
            finished("npm start", false, false, 0, 30_000),
        );
        state.background_task_labels.insert(
            "bg-2".into(),
            finished("cargo test", false, false, 0, 24_000),
        );
        assert_eq!(footer_summary(&state).unwrap().unread_failed, 2);
        acknowledge_failures(&mut state);
        assert!(
            footer_summary(&state).is_none(),
            "nothing left to remind about"
        );
        // History and terminal facts survive.
        assert!(state.background_task_labels.contains_key("bg-1"));
        assert!(state.background_task_labels.contains_key("bg-2"));
        assert!(
            state
                .background_task_labels
                .get("bg-1")
                .unwrap()
                .is_failed()
        );
        assert_eq!(background_job_list(&state).finished.len(), 2);
    }

    /// The list page groups running first, then recently finished newest first.
    #[test]
    fn the_list_orders_running_before_recently_finished() {
        let mut state = test_state();
        state.elapsed_secs = 200;
        state.background_task_labels.insert(
            "bg-r".into(),
            BackgroundTaskChrome::running("dev server", 190),
        );
        state.background_task_labels.insert(
            "bg-old".into(),
            finished("cargo check", true, false, 10, 5_000),
        );
        state.background_task_labels.insert(
            "bg-new".into(),
            finished("npm start", false, false, 100, 30_000),
        );
        let list = background_job_list(&state);
        let running: Vec<&str> = list.running.iter().map(|s| s.title.as_str()).collect();
        let done: Vec<&str> = list.finished.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(running, vec!["dev server"]);
        assert_eq!(done, vec!["npm start", "cargo check"]);
    }

    /// Selection is stable by id and walks running → finished in order.
    #[test]
    fn list_selection_walks_running_then_finished() {
        let mut state = test_state();
        state.elapsed_secs = 200;
        state.background_task_labels.insert(
            "bg-r".into(),
            BackgroundTaskChrome::running("dev server", 190),
        );
        state.background_task_labels.insert(
            "bg-new".into(),
            finished("npm start", false, false, 100, 30_000),
        );
        ensure_list_selection(&mut state);
        assert_eq!(state.background_list_selected.as_deref(), Some("bg-r"));
        move_list_selection(&mut state, 1);
        assert_eq!(state.background_list_selected.as_deref(), Some("bg-new"));
        move_list_selection(&mut state, 1);
        assert_eq!(state.background_list_selected.as_deref(), Some("bg-new"));
        move_list_selection(&mut state, -5);
        assert_eq!(state.background_list_selected.as_deref(), Some("bg-r"));
    }
}
