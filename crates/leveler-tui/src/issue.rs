//! Issue read model: what currently needs the user's attention.
//!
//! One projection over REAL state, never a second error log. An issue stands
//! for a fact the runtime already reported:
//!
//! - a **failed turn** — `RuntimeStatus::Error` is the runtime's own "the last
//!   turn ended in failure" state, and the transcript's failure block carries
//!   its typed summary/subtitle/detail;
//! - a **failed background task** — the task's failed terminal state in the
//!   projection the runtime's events built, keyed by its stable task id.
//!
//! What is deliberately NOT here:
//!
//! - ordinary tool failures (`exit 1` from a test command the agent is working
//!   through). They stay in the execution timeline; a non-zero exit is not a
//!   session-level fault;
//! - provider retries still in flight (`ModelRetrying` is normal operation);
//!   only exhausted retries that failed the turn become issues;
//! - MCP health. No structured MCP state exists in the client protocol, and a
//!   classifier over error text would be invented data; MCP tool failures sit
//!   in the timeline like any other tool failure.
//!
//! Lifecycle: an issue is active exactly as long as its source fact is. A
//! failed turn stops being active when a later turn reports `Busy`/`Idle`; a
//! failed task stops being active when its entry leaves the projection (the
//! bounded terminal window, a session switch). The STRIP's reminder, however,
//! is an attention signal, not a durable fact: an unviewed one-shot failure
//! reminds, and viewing it — opening the issue page, or that task's own detail
//! — clears the reminder while the record stays active and listed as history.
//! A failed turn is a persistent fault: only the runtime's own status leaving
//! `Error` clears it, never a viewing.

use leveler_client_protocol::RuntimeStatus;

use crate::state::AppState;

/// The kind of real state an issue stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IssueKind {
    /// The runtime reported the last turn failed (provider, authentication,
    /// runtime, permission — its typed category is in the block's subtitle).
    TurnFailure,
    /// A background task reached the runtime's failed terminal state. A
    /// stopped task is not a failure and never appears here.
    BackgroundFailure,
}

impl IssueKind {
    /// Stable wire-free key for the kind (dedupe + selection identity).
    pub fn key(self) -> &'static str {
        match self {
            Self::TurnFailure => "turn-failure",
            Self::BackgroundFailure => "background-failure",
        }
    }
}

/// One active issue, pointing at the real record it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Issue {
    pub kind: IssueKind,
    /// Dedupe key: the task's stable id, or the kind key for the turn failure.
    pub key: String,
    /// Short label: the failure's own summary, or the task's command label.
    pub label: String,
    /// One muted machine line: provider · category for a turn failure, exit
    /// code and duration for a task.
    pub meta: Option<String>,
    /// The safe technical detail (turn failures only; task detail lives in the
    /// task's own page).
    pub detail: Option<String>,
    /// New since the user last looked. Seen is not solved: an unread issue and
    /// a read one are equally active.
    pub unread: bool,
}

/// Summary of the issues the strip must REMIND about: the persistent failed
/// turn (always) and the unviewed one-shot failures. Viewed failures are
/// history, not reminders — they stay on the page, never on the strip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct IssueSummary {
    /// What the strip's issue cell counts: the severe issue plus the unviewed
    /// background failures.
    pub total: usize,
    /// The severe issue's label — a failed turn. The strip shows it in full
    /// (truncated) instead of hiding it behind a count.
    pub severe: Option<String>,
    /// `true` while any active turn-failure issue exists.
    pub severe_active: bool,
}

impl IssueSummary {
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

/// Every active issue, in display order: the failed turn first, then failed
/// background tasks oldest-first (the projection's own start clock, so the
/// order is deterministic and never a hash iteration order). Unviewed failures
/// lead their viewed history, so what needs attention is what the eye lands on.
pub(crate) fn active_issues(state: &AppState) -> Vec<Issue> {
    let mut issues = Vec::new();
    if state.status == RuntimeStatus::Error {
        let block = state.transcript.last_failure_block();
        let (label, meta, detail) = match block {
            Some(block) => (
                block.summary.clone(),
                block.subtitle.clone(),
                (!block.detail.is_empty()).then(|| block.detail.clone()),
            ),
            // A failure the transcript never got a block for (a legacy event):
            // the status itself is still the fact, so it stays an issue with
            // the block's own title vocabulary.
            None => (
                state.t().failure_title.to_string(),
                None,
                None,
            ),
        };
        issues.push(Issue {
            kind: IssueKind::TurnFailure,
            key: IssueKind::TurnFailure.key().to_string(),
            label,
            meta,
            detail,
            unread: false,
        });
    }
    let mut failed: Vec<(&String, &crate::state::BackgroundTaskChrome)> = state
        .background_task_labels
        .iter()
        .filter(|(_, chrome)| chrome.is_failed())
        .collect();
    failed.sort_by(|a, b| {
        let unread = |id: &String| !state.background_failures_seen.contains(id);
        unread(b.0)
            .cmp(&unread(a.0))
            .then_with(|| a.1.started_elapsed_secs.cmp(&b.1.started_elapsed_secs))
            .then_with(|| a.0.cmp(b.0))
    });
    for (id, chrome) in failed {
        let mut meta = Vec::new();
        if let Some(code) = chrome.exit_code {
            meta.push(format!("exit {code}"));
        }
        if let Some(ms) = chrome.duration_ms {
            meta.push(crate::status_line::fmt_elapsed(ms / 1000));
        }
        issues.push(Issue {
            kind: IssueKind::BackgroundFailure,
            key: id.clone(),
            label: chrome.label.clone(),
            meta: (!meta.is_empty()).then(|| meta.join(" · ")),
            detail: None,
            unread: !state.background_failures_seen.contains(id),
        });
    }
    issues
}

/// Sum up what the strip must remind about: the severe label when a failed
/// turn is active, and the count of issues that still need attention.
pub(crate) fn issue_summary(state: &AppState) -> IssueSummary {
    let issues = active_issues(state);
    let severe = issues
        .iter()
        .find(|issue| issue.kind == IssueKind::TurnFailure)
        .map(|issue| issue.label.clone());
    let unread = issues
        .iter()
        .filter(|issue| issue.kind == IssueKind::BackgroundFailure && issue.unread)
        .count();
    IssueSummary {
        total: usize::from(severe.is_some()) + unread,
        severe_active: severe.is_some(),
        severe,
    }
}

/// Keep the page's cursor on a row that still exists; the first active issue
/// when the old pick is gone. A cleared issue leaves no dangling selection,
/// and an expansion for a row that is no longer active is dropped with it.
pub(crate) fn ensure_selection(state: &mut AppState) {
    let issues = active_issues(state);
    state
        .issue_expanded
        .retain(|key| issues.iter().any(|issue| &issue.key == key));
    if state
        .issue_selected
        .as_ref()
        .is_none_or(|key| !issues.iter().any(|issue| &issue.key == key))
    {
        state.issue_selected = issues.first().map(|issue| issue.key.clone());
    }
}

/// Whether the issue's row is expanded on the page.
pub(crate) fn is_expanded(state: &AppState, key: &str) -> bool {
    state.issue_expanded.contains(key)
}

/// Toggle the selected issue's expansion. Selecting and expanding are the
/// same interaction: the selected (clicked) row is the row acted on, and the
/// state is keyed by stable identity so it survives returns and resizes.
pub(crate) fn toggle_expanded(state: &mut AppState) {
    ensure_selection(state);
    let Some(key) = state.issue_selected.clone() else {
        return;
    };
    if !state.issue_expanded.remove(&key) {
        state.issue_expanded.insert(key);
    }
}

/// Collapse the selected row when it is expanded; `true` when it was. Esc's
/// first level on the page is the open row, the second leaves for the
/// conversation — and the back glyph runs the same rule.
pub(crate) fn collapse_selected(state: &mut AppState) -> bool {
    let Some(key) = state.issue_selected.clone() else {
        return false;
    };
    state.issue_expanded.remove(&key)
}

/// The issue the page's cursor is on, if it still exists. Keyed by stable
/// identity, so a failure that left the projection is never acted on.
pub(crate) fn selected_issue(state: &AppState) -> Option<Issue> {
    let key = state.issue_selected.as_deref()?;
    active_issues(state).into_iter().find(|issue| issue.key == key)
}

/// Move the page's selection over the active issues.
pub(crate) fn move_selection(state: &mut AppState, delta: isize) {
    ensure_selection(state);
    let issues = active_issues(state);
    if issues.is_empty() {
        return;
    }
    let current = state
        .issue_selected
        .as_ref()
        .and_then(|key| issues.iter().position(|issue| &issue.key == key))
        .unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(issues.len().saturating_sub(1))
    };
    state.issue_selected = Some(issues[next].key.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{BackgroundTaskChrome, Notification};

    fn state() -> AppState {
        AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
                session_id: leveler_client_protocol::SessionId::new("s1"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        )
    }

    fn failed_task(state: &mut AppState, id: &str, started: u64, exit_code: i32) {
        state.background_task_labels.insert(
            id.to_string(),
            BackgroundTaskChrome {
                ok: Some(false),
                exit_code: Some(exit_code),
                duration_ms: Some(137_000),
                ..BackgroundTaskChrome::running(format!("go test -race {id}"), started)
            },
        );
    }

    #[test]
    fn a_quiet_session_has_no_issues() {
        let state = state();
        assert!(issue_summary(&state).is_empty());
        assert!(active_issues(&state).is_empty());
    }

    /// Running, completed and stopped tasks are not issues. A cancel is a
    /// user/agent decision, never a failure.
    #[test]
    fn only_failed_tasks_are_issues() {
        let mut state = state();
        state.background_task_labels.insert(
            "run".into(),
            BackgroundTaskChrome::running("npm run dev", 0),
        );
        state.background_task_labels.insert(
            "ok".into(),
            BackgroundTaskChrome {
                ok: Some(true),
                ..BackgroundTaskChrome::running("cargo build", 1)
            },
        );
        state.background_task_labels.insert(
            "stopped".into(),
            BackgroundTaskChrome {
                ok: Some(false),
                stopped: true,
                ..BackgroundTaskChrome::running("sleep 99", 2)
            },
        );
        assert!(active_issues(&state).is_empty(), "nothing failed");

        failed_task(&mut state, "t1", 3, 1);
        let issues = active_issues(&state);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, IssueKind::BackgroundFailure);
        assert_eq!(issues[0].key, "t1");
        assert_eq!(issues[0].label, "go test -race t1");
        assert_eq!(issues[0].meta.as_deref(), Some("exit 1 · 2m 17s"));
        assert!(issues[0].unread);
    }

    /// Looking at a failure is not solving it: acknowledging clears the
    /// "new" marker and the reminder, and the issue stays active on the page.
    #[test]
    fn seen_is_not_solved() {
        let mut state = state();
        failed_task(&mut state, "t1", 3, 1);
        assert_eq!(issue_summary(&state).total, 1, "an unseen failure reminds");
        crate::activity::acknowledge_failures(&mut state);
        let issues = active_issues(&state);
        assert_eq!(issues.len(), 1, "an acknowledged failure is still active");
        assert!(!issues[0].unread, "but it is no longer new");
        assert!(
            issue_summary(&state).is_empty(),
            "and it stops reminding; the record is what stays, not the reminder"
        );
        assert!(
            state.background_task_labels.contains_key("t1"),
            "the failed record is never deleted by viewing it"
        );
    }

    /// One viewed failure does not swallow the next: a new failure is a new
    /// stable identity, so it reminds on its own.
    #[test]
    fn a_new_failure_reminds_after_earlier_ones_were_viewed() {
        let mut state = state();
        failed_task(&mut state, "t1", 3, 1);
        crate::activity::acknowledge_failures(&mut state);
        assert!(issue_summary(&state).is_empty());
        failed_task(&mut state, "t2", 9, 2);
        let summary = issue_summary(&state);
        assert_eq!(summary.total, 1, "only the unviewed failure counts");
        let issues = active_issues(&state);
        assert_eq!(issues[0].key, "t2", "unread failures lead the page");
        assert_eq!(issues[1].key, "t1", "viewed ones follow as history");
        assert!(issues[0].unread);
        assert!(!issues[1].unread);
    }

    /// Viewing failures never clears the failed turn: its activity is the
    /// runtime's status, and only the status changing is recovery.
    #[test]
    fn viewing_never_clears_a_failed_turn() {
        let mut state = state();
        state.status = RuntimeStatus::Error;
        failed_task(&mut state, "t1", 3, 1);
        assert_eq!(issue_summary(&state).total, 2);
        crate::activity::acknowledge_failures(&mut state);
        let summary = issue_summary(&state);
        assert!(summary.severe_active, "the failed turn still stands");
        assert_eq!(summary.total, 1, "only the unviewed failure was cleared");
        assert_eq!(summary.severe.as_deref(), Some("请求失败"));
        state.status = RuntimeStatus::Idle;
        assert!(
            issue_summary(&state).is_empty(),
            "recovery is the runtime's own status change, nothing else"
        );
    }

    /// The failed turn is the severe issue; its label and machine line come
    /// from the transcript's own failure block, never a re-worded copy.
    #[test]
    fn a_failed_turn_is_the_severe_issue() {
        let mut state = state();
        state.status = RuntimeStatus::Error;
        state.transcript.push_failure(crate::transcript::FailureBlock {
            display: crate::fold::DisplayMode::Collapsed,
            title: "请求失败".into(),
            summary: "模型认证失败".into(),
            subtitle: Some("deepseek · authentication · HTTP 401".into()),
            detail: "status  401".into(),
        });
        let summary = issue_summary(&state);
        assert_eq!(summary.total, 1);
        assert!(summary.severe_active);
        assert_eq!(summary.severe.as_deref(), Some("模型认证失败"));
        let issues = active_issues(&state);
        assert_eq!(issues[0].kind, IssueKind::TurnFailure);
        assert_eq!(
            issues[0].meta.as_deref(),
            Some("deepseek · authentication · HTTP 401")
        );
        assert!(issues[0].detail.as_deref().is_some_and(|d| d.contains("401")));
    }

    /// A later turn clears the severe issue: the runtime's status is the fact
    /// that decides activity, and the block stays in the transcript as history.
    #[test]
    fn a_later_turn_clears_the_severe_issue_but_not_its_history() {
        let mut state = state();
        state.status = RuntimeStatus::Error;
        state.transcript.push_failure(crate::transcript::FailureBlock {
            display: crate::fold::DisplayMode::Collapsed,
            title: "请求失败".into(),
            summary: "模型认证失败".into(),
            subtitle: None,
            detail: String::new(),
        });
        assert!(issue_summary(&state).severe_active);
        state.status = RuntimeStatus::Busy;
        assert!(issue_summary(&state).is_empty(), "the turn is running again");
        assert!(
            state.transcript.last_failure_block().is_some(),
            "history keeps the failure"
        );
    }

    /// A failed tool call in a healthy session is NOT an issue: status stays
    /// Busy/Idle and nothing else promotes it. This is the lock against
    /// escalating `exit 1` into a session-level fault.
    #[test]
    fn an_ordinary_tool_failure_is_not_an_issue() {
        let mut state = state();
        state.status = RuntimeStatus::Busy;
        let call = leveler_client_protocol::ToolCallId::new("c1");
        state.transcript.push_tool_started(
            call.clone(),
            "run_command".into(),
            r#"{"program":"cargo","args":["test"]}"#.into(),
            false,
            0,
            None,
            None,
        );
        state
            .transcript
            .complete_tool(&call, false, "exit 1".into(), 3, None);
        assert!(active_issues(&state).is_empty());
    }

    /// Several distinct problems aggregate; a single kind names itself.
    #[test]
    fn kinds_aggregate_and_order_deterministically() {
        let mut state = state();
        failed_task(&mut state, "t2", 9, 1);
        failed_task(&mut state, "t1", 3, 2);
        let summary = issue_summary(&state);
        assert_eq!(summary.total, 2);
        assert!(!summary.severe_active);
        let issues = active_issues(&state);
        assert_eq!(issues[0].key, "t1", "oldest first");
        assert_eq!(issues[1].key, "t2");

        state.status = RuntimeStatus::Error;
        let summary = issue_summary(&state);
        assert!(summary.severe_active);
        assert_eq!(summary.total, 3);
    }

    /// The strip summary never counts one fact twice: a task cannot fail
    /// twice, and the turn failure is keyed by kind, not repeated per event.
    #[test]
    fn one_fact_is_one_issue() {
        let mut state = state();
        failed_task(&mut state, "t1", 3, 1);
        state.status = RuntimeStatus::Error;
        // Repeated reductions of "the same" failure do not multiply it: the
        // model is derived, so it cannot drift from the source.
        for _ in 0..5 {
            let _ = issue_summary(&state);
        }
        assert_eq!(issue_summary(&state).total, 2);
    }

    /// A notification is not a source: a dismissed notice changes nothing.
    #[test]
    fn notices_are_not_issues() {
        let mut state = state();
        state.notification = Some(Notification {
            level: leveler_client_protocol::NotificationLevel::Error,
            message: "some transient notice".into(),
        });
        assert!(active_issues(&state).is_empty());
    }
}
