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

/// How many completed background entries the TUI keeps reopenable.
pub(crate) const MAX_COMPLETED_BACKGROUND: usize = 8;
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
        let status = match chrome.ok {
            None => ActivityStatus::Running,
            Some(true) => ActivityStatus::Completed,
            Some(false) => ActivityStatus::Failed,
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
            ActivityStatus::Completed | ActivityStatus::Failed => done.push(row),
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
            ActivityStatus::Completed | ActivityStatus::Failed => done.push(row),
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

fn activity_glyph(status: ActivityStatus) -> &'static str {
    match status {
        ActivityStatus::Running => "●",
        ActivityStatus::Waiting => "◌",
        ActivityStatus::Completed => "✓",
        ActivityStatus::Failed => "✕",
        ActivityStatus::Interrupted => "⏸",
        ActivityStatus::Unreported => "?",
    }
}

pub(crate) fn prune_completed_background(state: &mut AppState) {
    let completed: Vec<String> = state
        .background_task_labels
        .iter()
        .filter(|(_, c)| !c.is_running())
        .map(|(id, _)| id.clone())
        .collect();
    if completed.len() <= MAX_COMPLETED_BACKGROUND {
        return;
    }
    let mut ranked: Vec<(u64, String)> = completed
        .into_iter()
        .filter_map(|id| {
            state
                .background_task_labels
                .get(&id)
                .map(|c| (c.started_elapsed_secs, id))
        })
        .collect();
    ranked.sort_by_key(|(started, _)| *started);
    let drop_n = ranked.len().saturating_sub(MAX_COMPLETED_BACKGROUND);
    for (_, id) in ranked.into_iter().take(drop_n) {
        state.background_task_labels.remove(&id);
        if matches!(&state.activity_selected, Some(ActivityId::Background(open)) if open == &id) {
            state.activity_selected = None;
        }
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

/// Status-strip activity lines and the summary index each line belongs to.
pub(crate) fn status_activity_lines(
    state: &AppState,
    width: usize,
    t: &UiText,
) -> Vec<ActivityRow> {
    let all = summaries(state);
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
    let all = summaries(state);
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
        ActivityId::Background(_) => Vec::new(),
    }
}

pub(crate) fn close(state: &mut AppState) {
    state.activity_open = None;
    state.active_screen = crate::screen::Screen::Conversation;
    state.screen_scroll = 0;
}

pub(crate) fn ensure_selection(state: &mut AppState) {
    let all = summaries(state);
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
    fn background_completed_stays_reopenable() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-2".into(),
            BackgroundTaskChrome {
                label: "cargo test --workspace".into(),
                started_elapsed_secs: 0,
                ok: Some(true),
                exit_code: Some(0),
                duration_ms: Some(48_000),
                output: "ok".into(),
            },
        );
        let rows = summaries(&state);
        assert_eq!(rows[0].status, ActivityStatus::Completed);
        assert_eq!(rows[0].duration_secs, 48);
        assert!(background_chrome(&state, "bg-2").is_some());
        let line = compact_row(&rows[0], false, 80);
        assert!(line.contains('✓'), "{line}");
    }

    #[test]
    fn background_failed_activity_render() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-fail".into(),
            BackgroundTaskChrome {
                label: "cargo test --workspace".into(),
                started_elapsed_secs: 0,
                ok: Some(false),
                exit_code: Some(101),
                duration_ms: Some(74_000),
                output: "failed".into(),
            },
        );
        let rows = summaries(&state);
        assert_eq!(rows[0].status, ActivityStatus::Failed);
        let line = compact_row(&rows[0], false, 80);
        assert!(line.contains('✕'), "{line}");
        assert!(!line.contains("运行中"), "{line}");
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
                steps: Vec::new(),
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
                steps: Vec::new(),
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
                steps: Vec::new(),
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
                steps: Vec::new(),
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
    fn prune_keeps_running_and_bounds_completed() {
        let mut state = test_state();
        for i in 0..12u64 {
            state.background_task_labels.insert(
                format!("done-{i}"),
                BackgroundTaskChrome {
                    label: format!("cmd {i}"),
                    started_elapsed_secs: i,
                    ok: Some(true),
                    exit_code: Some(0),
                    duration_ms: Some(1),
                    output: String::new(),
                },
            );
        }
        state.background_task_labels.insert(
            "live".into(),
            BackgroundTaskChrome::running("cargo test", 99),
        );
        prune_completed_background(&mut state);
        assert!(state.background_task_labels.contains_key("live"));
        let completed = state
            .background_task_labels
            .values()
            .filter(|c| !c.is_running())
            .count();
        assert_eq!(completed, MAX_COMPLETED_BACKGROUND);
    }

    #[test]
    fn activity_detail_open_close() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-2".into(),
            BackgroundTaskChrome::running("cargo test --workspace", 0),
        );
        crate::activity::ensure_selection(&mut state);
        let effects = crate::activity::open_selected(&mut state);
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
                steps: vec!["read_file".into()],
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
                steps: vec!["read_file".into()],
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
}
