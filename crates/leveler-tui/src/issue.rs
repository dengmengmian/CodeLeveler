//! Issue read model: what currently needs the user's attention.
//!
//! One projection over REAL state, never a second error log. An issue stands
//! for a fact the runtime already reported:
//!
//! - a **failed turn** — `RuntimeStatus::Error` is the runtime's own "the last
//!   turn ended in failure" state, and the transcript's failure block carries
//!   its typed summary/subtitle/detail;
//! - a **client/runtime version mismatch** — two known build identities
//!   disagree. It stays active until a matching build is observed, so the
//!   risk outlives any dismissible notice.
//!
//! What is deliberately NOT here:
//!
//! - a **failed background task**. Its process exited, but the session is
//!   still healthy: the failure is a record (exit code, duration, output) in
//!   the background page and the conversation row, not a standing session
//!   fault. Nothing accumulates, so nothing needs acknowledging;
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
//! version mismatch stops when a matching build is observed. Neither is
//! cleared by viewing it.

use leveler_client_protocol::RuntimeStatus;

use crate::state::AppState;

/// The kind of real state an issue stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IssueKind {
    /// The runtime reported the last turn failed (provider, authentication,
    /// runtime, permission — its typed category is in the block's subtitle).
    TurnFailure,
    /// The client and the runtime it talks to are known to be different
    /// builds. Only a matching observed build clears it.
    RuntimeVersion,
}

impl IssueKind {
    /// Stable wire-free key for the kind (dedupe + selection identity).
    pub fn key(self) -> &'static str {
        match self {
            Self::TurnFailure => "turn-failure",
            Self::RuntimeVersion => "runtime-version",
        }
    }
}

/// One active issue, pointing at the real record it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Issue {
    pub kind: IssueKind,
    /// Dedupe key: the kind key, stable across re-reductions.
    pub key: String,
    /// Short label: the failure's own summary.
    pub label: String,
    /// One muted machine line: provider · category for a turn failure.
    pub meta: Option<String>,
    /// The safe technical detail (turn failures only).
    pub detail: Option<String>,
}

/// What the strip's issue cell counts: the failed turn and a version mismatch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct IssueSummary {
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

/// Every active issue, in display order: the failed turn first, then a
/// client/runtime version mismatch.
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
            None => (state.t().failure_title.to_string(), None, None),
        };
        issues.push(Issue {
            kind: IssueKind::TurnFailure,
            key: IssueKind::TurnFailure.key().to_string(),
            label,
            meta,
            detail,
        });
    }
    if state.runtime_version_mismatch {
        let meta = state.runtime_build.as_ref().and_then(|build| {
            let expected = leveler_client_protocol::BuildIdentity::current();
            if !build.is_known() || !expected.is_known() {
                return None;
            }
            Some(
                state
                    .t()
                    .runtime_version_meta
                    .replacen("{}", &expected.diagnostic(), 1)
                    .replacen("{}", &build.diagnostic(), 1),
            )
        });
        issues.push(Issue {
            kind: IssueKind::RuntimeVersion,
            key: IssueKind::RuntimeVersion.key().to_string(),
            label: state.t().runtime_version_mismatch.to_string(),
            meta,
            detail: None,
        });
    }
    issues
}

/// Sum up what the strip must report: the severe label when a failed turn is
/// active, plus a persistent version mismatch.
pub(crate) fn issue_summary(state: &AppState) -> IssueSummary {
    let issues = active_issues(state);
    let severe = issues
        .iter()
        .find(|issue| issue.kind == IssueKind::TurnFailure)
        .map(|issue| issue.label.clone());
    IssueSummary {
        total: usize::from(severe.is_some()) + usize::from(state.runtime_version_mismatch),
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

    /// Background tasks are execution records, not session issues. Running,
    /// completed, stopped and failed tasks all leave the strip's issue cell
    /// alone; a failed one keeps its exit, duration, and place in the page.
    #[test]
    fn a_failed_background_task_is_a_record_not_an_issue() {
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

        failed_task(&mut state, "t1", 3, 1);
        failed_task(&mut state, "t2", 4, 2);
        assert!(
            active_issues(&state).is_empty(),
            "a failure is not an issue"
        );
        assert!(issue_summary(&state).is_empty());

        let record = &state.background_task_labels["t1"];
        assert_eq!(record.exit_code, Some(1));
        assert_eq!(record.duration_ms, Some(137_000));
        assert!(record.is_failed());
        assert!(state.background_task_labels.contains_key("t2"));
    }

    /// A failed turn is the only session-level fault; a failed background task
    /// next to it never inflates the count and never accumulates a reminder.
    #[test]
    fn a_failed_turn_is_not_multiplied_by_background_failures() {
        let mut state = state();
        state.status = RuntimeStatus::Error;
        failed_task(&mut state, "t1", 3, 1);
        failed_task(&mut state, "t2", 4, 2);
        let summary = issue_summary(&state);
        assert_eq!(summary.total, 1);
        assert!(summary.severe_active);
        assert_eq!(summary.severe.as_deref(), Some("请求失败"));
        state.status = RuntimeStatus::Idle;
        assert!(
            issue_summary(&state).is_empty(),
            "recovery is the runtime's own status change, nothing else"
        );
        assert!(
            state.background_task_labels.contains_key("t1"),
            "the task record outlives the turn"
        );
    }

    /// The failed turn is the severe issue; its label and machine line come
    /// from the transcript's own failure block, never a re-worded copy.
    #[test]
    fn a_failed_turn_is_the_severe_issue() {
        let mut state = state();
        state.status = RuntimeStatus::Error;
        state
            .transcript
            .push_failure(crate::transcript::FailureBlock {
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
        assert!(
            issues[0]
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("401"))
        );
    }

    /// A later turn clears the severe issue: the runtime's status is the fact
    /// that decides activity, and the block stays in the transcript as history.
    #[test]
    fn a_later_turn_clears_the_severe_issue_but_not_its_history() {
        let mut state = state();
        state.status = RuntimeStatus::Error;
        state
            .transcript
            .push_failure(crate::transcript::FailureBlock {
                display: crate::fold::DisplayMode::Collapsed,
                title: "请求失败".into(),
                summary: "模型认证失败".into(),
                subtitle: None,
                detail: String::new(),
            });
        assert!(issue_summary(&state).severe_active);
        state.status = RuntimeStatus::Busy;
        assert!(
            issue_summary(&state).is_empty(),
            "the turn is running again"
        );
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

    /// The two persistent faults aggregate; background failures add nothing.
    #[test]
    fn persistent_faults_aggregate_and_order_deterministically() {
        let mut state = state();
        failed_task(&mut state, "t2", 9, 1);
        failed_task(&mut state, "t1", 3, 2);
        assert!(active_issues(&state).is_empty());

        state.status = RuntimeStatus::Error;
        state.runtime_version_mismatch = true;
        let summary = issue_summary(&state);
        assert!(summary.severe_active);
        assert_eq!(summary.total, 2);
        let issues = active_issues(&state);
        assert_eq!(issues[0].kind, IssueKind::TurnFailure);
        assert_eq!(issues[1].kind, IssueKind::RuntimeVersion);
    }

    /// The strip summary never counts one fact twice: the model is derived
    /// from state, so repeated reads cannot multiply anything.
    #[test]
    fn one_fact_is_one_issue() {
        let mut state = state();
        failed_task(&mut state, "t1", 3, 1);
        state.status = RuntimeStatus::Error;
        for _ in 0..5 {
            let _ = issue_summary(&state);
        }
        assert_eq!(issue_summary(&state).total, 1);
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

    /// A version mismatch is a durable issue of its own: it survives the
    /// warning notice that announced it, and it leaves only when the state
    /// says a matching build is serving the client.
    #[test]
    fn a_version_mismatch_persists_until_a_matching_build_is_observed() {
        let mut state = state();
        state.runtime_version_mismatch = true;
        let mut foreign = leveler_client_protocol::BuildIdentity::current();
        foreign.revision = "0000000000000000000000000000000000000000".into();
        foreign.fingerprint = "foreign-fingerprint".into();
        foreign.dirty = false;
        state.runtime_build = Some(foreign);

        state.notification = None;
        let issues = active_issues(&state);
        assert_eq!(issues.len(), 1, "dismissing the notice solved nothing");
        assert_eq!(issues[0].kind, IssueKind::RuntimeVersion);
        assert_eq!(issues[0].key, "runtime-version");
        assert_eq!(issues[0].label, "CodeLeveler 版本不一致，请重启应用。");
        assert!(
            issues[0]
                .meta
                .as_deref()
                .is_some_and(|m| m.contains("当前应用")),
            "the row names both builds: {:?}",
            issues[0].meta
        );
        assert_eq!(issue_summary(&state).total, 1);

        state.runtime_version_mismatch = false;
        assert!(
            active_issues(&state).is_empty(),
            "a matching build is the only recovery"
        );
    }

    /// The two persistent faults stay distinct rows, and the version row does
    /// not hide the failed turn's severe label.
    #[test]
    fn a_version_mismatch_and_a_failed_turn_are_separate_issues() {
        let mut state = state();
        state.status = RuntimeStatus::Error;
        state.runtime_version_mismatch = true;
        let issues = active_issues(&state);
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].kind, IssueKind::TurnFailure);
        assert_eq!(issues[1].kind, IssueKind::RuntimeVersion);
        assert_eq!(issue_summary(&state).total, 2);
    }
}
