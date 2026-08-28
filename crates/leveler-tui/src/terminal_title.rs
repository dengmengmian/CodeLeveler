//! Terminal Chrome — dynamic task title (presentation-only projection).
//!
//! One job: make several CodeLeveler tabs identifiable at a glance and show
//! which one currently needs attention. Shape: `<icon> <task> · <activity>`,
//! e.g. `◐ Completion Truth 收口 · 跑测试`, idle fallback `○ CodeLeveler`.
//!
//! Strictly a projection: it reads existing structured TUI state
//! (`status_phase`, the transcript's turn-end truth, the structured activity
//! label, background-task presence) and writes terminal chrome. It owns no
//! lifecycle, persists nothing, and its failure is silently ignored — a tab
//! title can never break a run. The task identity half is deliberately
//! sticky: it is captured when a turn starts and survives until a new turn
//! starts with a different request (or the conversation resets), so a
//! finished tab keeps reading `✓ task · 已完成` for the user who is looking
//! at five tabs from elsewhere — unlike the in-TUI roster, this surface is
//! specifically an out-of-focus indicator.

use std::io::Write;

use crate::state::AppState;
use crate::status_line::{StatusPhase, status_phase};
use crate::transcript::{TranscriptItem, TurnEndStatus};

/// Presentation-only status. Derived from existing truth on every projection;
/// never persisted, never consulted by the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalTaskStatus {
    Idle,
    Running,
    Waiting,
    NeedsUser,
    Completed,
    Failed,
}

impl TerminalTaskStatus {
    /// The frozen icon set. The symbol itself carries the state — titles
    /// cannot rely on color, and emoji render inconsistently across
    /// terminals.
    fn icon(self) -> &'static str {
        match self {
            TerminalTaskStatus::Idle => "○",
            TerminalTaskStatus::Running => "◐",
            TerminalTaskStatus::Waiting => "◌",
            TerminalTaskStatus::NeedsUser => "!",
            TerminalTaskStatus::Completed => "✓",
            TerminalTaskStatus::Failed => "×",
        }
    }
}

/// Overall display budget (cells, not bytes) and the split that keeps the
/// identity recognizable before any activity detail survives.
const TITLE_MAX_CELLS: usize = 64;
const IDENTITY_MAX_CELLS: usize = 40;

/// A terminal title reaches tab bars, window managers, multiplexers and
/// screenshots, and it travels inside an OSC sequence — so user-controlled
/// text must not smuggle control bytes (ESC/BEL/ST would break out of the
/// title; newlines corrupt it). Collapse anything C0/C1 to a space.
fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = true;
    for ch in text.chars() {
        let mapped = if ch.is_control() || ch == '\u{7f}' {
            ' '
        } else {
            ch
        };
        if mapped == ' ' {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(mapped);
            last_space = false;
        }
    }
    out.trim().to_string()
}

/// The turn-end truth the title obeys: same hierarchy as the transcript
/// marker, never upgraded. `Unverified` deliberately does NOT get a ✓ — the
/// title has no room for the in-TUI caveat text, and a bare ✓ would claim
/// more than Completion Truth does.
fn terminal_status(end: TurnEndStatus) -> TerminalTaskStatus {
    match end {
        TurnEndStatus::Completed | TurnEndStatus::Answered => TerminalTaskStatus::Completed,
        TurnEndStatus::Unverified
        | TurnEndStatus::Truncated
        | TurnEndStatus::Incomplete
        | TurnEndStatus::Failed
        | TurnEndStatus::Cancelled => TerminalTaskStatus::Failed,
    }
}

fn last_turn_end(state: &AppState) -> Option<TurnEndStatus> {
    state
        .transcript
        .items()
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::TurnEnd(block) => Some(block.status),
            _ => None,
        })
}

fn last_user_text(state: &AppState) -> Option<String> {
    state
        .transcript
        .items()
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::User(text) => {
                let line = sanitize(text.lines().next().unwrap_or(text));
                (!line.is_empty()).then_some(line)
            }
            _ => None,
        })
}

/// Attention beats failure beats work beats waiting beats done: the title
/// answers "do I need to look at this tab".
fn derive_status(state: &AppState) -> TerminalTaskStatus {
    match status_phase(state) {
        StatusPhase::AwaitingUser => TerminalTaskStatus::NeedsUser,
        StatusPhase::Busy => TerminalTaskStatus::Running,
        StatusPhase::Idle => {
            if !state.background_task_labels.is_empty() {
                return TerminalTaskStatus::Waiting;
            }
            match last_turn_end(state) {
                Some(end) => terminal_status(end),
                None => TerminalTaskStatus::Idle,
            }
        }
    }
}

/// The short activity half. Structured sources only — the overlay kind, the
/// existing activity label, the turn-end truth — never model prose.
fn activity(state: &AppState, status: TerminalTaskStatus) -> Option<String> {
    let t = state.t();
    match status {
        TerminalTaskStatus::Idle => None,
        TerminalTaskStatus::NeedsUser => Some(match &state.overlay {
            Some(crate::overlay::Overlay::Clarification(_)) => t.title_awaiting_input.to_string(),
            _ => t.title_awaiting_approval.to_string(),
        }),
        TerminalTaskStatus::Running => state
            .activity
            .as_deref()
            .map(|a| sanitize(a.lines().next().unwrap_or(a)))
            .filter(|a| !a.is_empty())
            .or_else(|| Some(t.title_working.to_string())),
        TerminalTaskStatus::Waiting => Some(t.title_waiting_background.to_string()),
        TerminalTaskStatus::Completed => Some(t.title_completed.to_string()),
        TerminalTaskStatus::Failed => Some(
            match last_turn_end(state) {
                Some(TurnEndStatus::Unverified) => t.title_unverified,
                Some(TurnEndStatus::Cancelled) => t.title_cancelled,
                Some(TurnEndStatus::Failed) => t.title_failed,
                _ => t.title_incomplete,
            }
            .to_string(),
        ),
    }
}

/// Compose within the display budget. Truncation priority: icon, then a
/// recognizable identity, then activity — several tabs must never collapse
/// into indistinguishable `◐ 跑测试`.
fn compose(status: TerminalTaskStatus, identity: Option<&str>, activity: Option<&str>) -> String {
    let icon = status.icon();
    let Some(identity) = identity.filter(|s| !s.is_empty()) else {
        return format!("{icon} CodeLeveler");
    };
    let identity = crate::render::truncate_display(identity, IDENTITY_MAX_CELLS);
    let base = format!("{icon} {identity}");
    match activity.filter(|a| !a.is_empty()) {
        Some(act) => {
            let used = unicode_width::UnicodeWidthStr::width(base.as_str());
            let room = TITLE_MAX_CELLS.saturating_sub(used + 3);
            if room < 4 {
                base
            } else {
                format!("{base} · {}", crate::render::truncate_display(act, room))
            }
        }
        None => base,
    }
}

/// The sticky projection. `maybe_apply` is called once per redraw; a write
/// happens only when the composed title actually changed (no per-frame, and
/// no per-token/per-second churn — nothing time-derived feeds the title).
#[derive(Default)]
pub(crate) struct TerminalTitleProjection {
    last: Option<String>,
    /// Sticky task identity: captured when work starts, replaced when a NEW
    /// request starts work, cleared when the conversation resets.
    identity: Option<String>,
}

impl TerminalTitleProjection {
    pub(crate) fn project(&mut self, state: &AppState) -> String {
        let status = derive_status(state);
        let last_user = last_user_text(state);
        match (&last_user, status) {
            // The conversation reset (clear / fresh session): no identity.
            (None, _) => self.identity = None,
            // A turn is live: the request that started it is the identity.
            // Sticky across activity changes; replaced only by a new request.
            (Some(user), TerminalTaskStatus::Running | TerminalTaskStatus::NeedsUser) => {
                if self.identity.as_deref() != Some(user.as_str()) {
                    self.identity = Some(user.clone());
                }
            }
            // Terminal/idle/waiting states keep the finished task's identity
            // on the tab until something new starts.
            _ => {
                if self.identity.is_none() {
                    self.identity = last_user;
                }
            }
        }
        let activity = activity(state, status);
        compose(status, self.identity.as_deref(), activity.as_deref())
    }

    /// Best-effort: compute, dedupe, and try to write. A terminal that
    /// ignores or rejects OSC titles degrades silently — never an error the
    /// run can see.
    pub(crate) fn maybe_apply(&mut self, state: &AppState, out: &mut impl Write) {
        let title = self.project(state);
        if self.last.as_deref() == Some(title.as_str()) {
            return;
        }
        let _ = crossterm::execute!(out, crossterm::terminal::SetTitle(title.as_str()));
        self.last = Some(title);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::TurnEndStatus;
    use leveler_client_protocol::{RuntimeStatus, SessionId};

    fn test_state() -> AppState {
        AppState::new(
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
        )
    }

    fn end_turn(state: &mut AppState, status: TurnEndStatus) {
        state.transcript.push_turn_end(status, 1, 10, None, None);
    }

    #[test]
    fn idle_fallback_is_the_bare_product_name() {
        let mut p = TerminalTitleProjection::default();
        assert_eq!(p.project(&test_state()), "○ CodeLeveler");
    }

    #[test]
    fn running_shows_task_identity_and_structured_activity() {
        let mut state = test_state();
        state
            .transcript
            .push_user("修复 Completion Truth 收口".to_string());
        state.status = RuntimeStatus::Busy;
        state.activity = Some("跑测试".into());
        let mut p = TerminalTitleProjection::default();
        assert_eq!(p.project(&state), "◐ 修复 Completion Truth 收口 · 跑测试");
    }

    #[test]
    fn identity_stays_stable_across_activity_changes() {
        let mut state = test_state();
        state.transcript.push_user("Completion Truth".to_string());
        state.status = RuntimeStatus::Busy;
        let mut p = TerminalTitleProjection::default();
        for act in ["搜索代码", "修改实现", "跑测试"] {
            state.activity = Some(act.to_string());
            let title = p.project(&state);
            assert!(
                title.starts_with("◐ Completion Truth · "),
                "identity must not rename itself: {title}"
            );
        }
    }

    #[test]
    fn approval_wins_over_running_work() {
        use leveler_client_protocol::{ApprovalId, UiApprovalRequest};
        let mut state = test_state();
        state.transcript.push_user("Task".to_string());
        state.status = RuntimeStatus::Busy;
        state.activity = Some("Agents running".into());
        state.overlay = Some(crate::overlay::Overlay::Approval(Box::new(
            crate::overlay::ApprovalOverlay::new(UiApprovalRequest {
                id: ApprovalId::new("a1"),
                tool: "run_command".into(),
                summary: "git push".into(),
                command: Some("git push".into()),
                risks: vec![],
            }),
        )));
        let mut p = TerminalTitleProjection::default();
        assert_eq!(p.project(&state), "! Task · 等待授权");
    }

    #[test]
    fn completed_keeps_the_identity_until_a_new_task_starts() {
        let mut state = test_state();
        state.transcript.push_user("Task A".to_string());
        state.status = RuntimeStatus::Busy;
        let mut p = TerminalTitleProjection::default();
        p.project(&state);
        state.status = RuntimeStatus::Idle;
        end_turn(&mut state, TurnEndStatus::Completed);
        assert_eq!(p.project(&state), "✓ Task A · 已完成");
        // Still there on the next projection — no transient disappearance.
        assert_eq!(p.project(&state), "✓ Task A · 已完成");
        // A new request replaces it.
        state.transcript.push_user("Task B".to_string());
        state.status = RuntimeStatus::Busy;
        state.activity = None;
        let title = p.project(&state);
        assert!(title.starts_with("◐ Task B"), "{title}");
    }

    #[test]
    fn failure_truth_never_renders_a_checkmark() {
        for end in [
            TurnEndStatus::Failed,
            TurnEndStatus::Incomplete,
            TurnEndStatus::Truncated,
            TurnEndStatus::Unverified,
            TurnEndStatus::Cancelled,
        ] {
            let mut state = test_state();
            state.transcript.push_user("Task".to_string());
            state.status = RuntimeStatus::Idle;
            end_turn(&mut state, end);
            let mut p = TerminalTitleProjection::default();
            let title = p.project(&state);
            assert!(title.starts_with("× "), "{end:?}: {title}");
            assert!(!title.contains('✓'), "{end:?}: {title}");
        }
    }

    #[test]
    fn background_tasks_read_as_waiting_when_the_turn_is_over() {
        let mut state = test_state();
        state.transcript.push_user("Task".to_string());
        state.status = RuntimeStatus::Idle;
        state
            .background_task_labels
            .insert("bg-1".into(), "cargo test".into());
        let mut p = TerminalTitleProjection::default();
        assert_eq!(p.project(&state), "◌ Task · 等待后台任务");
    }

    #[test]
    fn control_bytes_in_the_task_text_cannot_escape_the_title() {
        let hostile = "evil\x1b]0;pwned\x07\ntitle\rhere\ttoo";
        let clean = sanitize(hostile);
        assert!(!clean.contains('\x1b'), "{clean:?}");
        assert!(!clean.contains('\x07'), "{clean:?}");
        assert!(!clean.contains('\n') && !clean.contains('\r'), "{clean:?}");
        let mut state = test_state();
        state.transcript.push_user(hostile.to_string());
        state.status = RuntimeStatus::Busy;
        let mut p = TerminalTitleProjection::default();
        let title = p.project(&state);
        assert!(
            title.chars().all(|c| !c.is_control()),
            "no control byte may survive into the title: {title:?}"
        );
    }

    #[test]
    fn cjk_width_is_measured_in_cells_and_identity_survives_truncation() {
        let mut state = test_state();
        state.transcript.push_user(
            "把清理页面列表改紧凑一点，然后修复点击展开详情的交互，再顺手把所有边距统一"
                .to_string(),
        );
        state.status = RuntimeStatus::Busy;
        state.activity = Some("正在运行 workspace 的完整 cargo test 套件与全部集成测试".into());
        let mut p = TerminalTitleProjection::default();
        let title = p.project(&state);
        let width = unicode_width::UnicodeWidthStr::width(title.as_str());
        assert!(
            width <= super::TITLE_MAX_CELLS + 2,
            "width {width}: {title}"
        );
        assert!(title.starts_with("◐ 把清理页面列表"), "{title}");
    }

    #[test]
    fn identical_projections_write_the_title_once() {
        let mut state = test_state();
        state.transcript.push_user("Task".to_string());
        state.status = RuntimeStatus::Busy;
        state.activity = Some("跑测试".into());
        let mut p = TerminalTitleProjection::default();
        let mut sink: Vec<u8> = Vec::new();
        p.maybe_apply(&state, &mut sink);
        let first = sink.len();
        assert!(first > 0, "first projection writes");
        p.maybe_apply(&state, &mut sink);
        assert_eq!(sink.len(), first, "identical title must not rewrite");
        state.activity = Some("修改实现".into());
        p.maybe_apply(&state, &mut sink);
        assert!(sink.len() > first, "semantic change writes again");
    }
}
