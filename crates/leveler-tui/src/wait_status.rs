//! Projection of *why* the main agent cannot proceed.
//!
//! Presentation only. The runtime remains authoritative for task lifecycle;
//! this module never infers a wait from assistant prose, and it never treats
//! a still-running background process as "Main is waiting" unless an
//! in-flight `wait_task` / `spawn_agent` actually blocks the loop.

use leveler_client_protocol::RuntimeStatus;

use crate::i18n::UiText;
use crate::multi_agent::ChildStatus;
use crate::overlay::Overlay;
use crate::render::truncate_display;
use crate::state::AppState;
use crate::status_line::{StatusPhase, fmt_elapsed, status_phase};
use crate::transcript::{ToolCallBlock, ToolStatus, TranscriptItem};

/// How many wait targets to disclose under the status line.
const MAX_TARGETS: usize = 3;

/// Typed wait reason. Distinct dependencies, not one generic "Waiting".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitKind {
    BackgroundTask,
    ChildAgent,
    Model,
    Approval,
}

/// Lifecycle of a disclosed dependency. Failed/cancelled work is not shown
/// as running — those entries leave the live map on `BackgroundTaskExited`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetLifecycle {
    Running,
}

/// One blocking dependency the user can read without opening diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WaitTarget {
    pub label: String,
    pub detail: Option<String>,
    pub running_secs: Option<u64>,
    pub lifecycle: TargetLifecycle,
}

/// UI projection of the current wait. Not a second scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WaitView {
    pub kind: WaitKind,
    pub wait_secs: u64,
    /// Authoritative count (may exceed [`WaitTarget`]s actually disclosed).
    pub count: usize,
    pub targets: Vec<WaitTarget>,
}

/// Derive the wait, if Main is actually blocked.
pub(crate) fn project(state: &AppState) -> Option<WaitView> {
    if let Some(view) = project_approval(state) {
        return Some(view);
    }
    if state.status != RuntimeStatus::Busy {
        return None;
    }

    let running = running_tools(state);
    if let Some(view) = project_background_wait(state, &running) {
        return Some(view);
    }
    if let Some(view) = project_child_wait(state, &running) {
        return Some(view);
    }
    if running.is_empty() && state.activity.is_none() {
        return Some(WaitView {
            kind: WaitKind::Model,
            wait_secs: state.elapsed_secs,
            count: 1,
            targets: Vec::new(),
        });
    }
    None
}

fn project_approval(state: &AppState) -> Option<WaitView> {
    if status_phase(state) != StatusPhase::AwaitingUser {
        return None;
    }
    matches!(&state.overlay, Some(Overlay::Approval(_))).then_some(WaitView {
        kind: WaitKind::Approval,
        wait_secs: 0,
        count: 1,
        targets: Vec::new(),
    })
}

fn project_background_wait(state: &AppState, running: &[&ToolCallBlock]) -> Option<WaitView> {
    let waits: Vec<&&ToolCallBlock> = running
        .iter()
        .filter(|call| call.name == "wait_task")
        .collect();
    if waits.is_empty() {
        return None;
    }

    let mut targets = Vec::new();
    for call in &waits {
        let Some(task_id) = wait_task_id(&call.arguments) else {
            continue;
        };
        let Some(chrome) = state.background_task_labels.get(&task_id) else {
            continue;
        };
        targets.push(WaitTarget {
            label: chrome.label.clone(),
            detail: None,
            running_secs: Some(elapsed_since(
                state.elapsed_secs,
                chrome.started_elapsed_secs,
            )),
            lifecycle: TargetLifecycle::Running,
        });
    }

    // A wait_task whose target already left the live map is unwinding, not a
    // live background wait — do not keep a stale "等待后台任务".
    if targets.is_empty() {
        return None;
    }

    let wait_secs = waits
        .iter()
        .map(|call| elapsed_since(state.elapsed_secs, call.started_elapsed_secs))
        .min()
        .unwrap_or(0);
    let count = targets.len();
    targets.truncate(MAX_TARGETS);
    Some(WaitView {
        kind: WaitKind::BackgroundTask,
        wait_secs,
        count,
        targets,
    })
}

fn project_child_wait(state: &AppState, running: &[&ToolCallBlock]) -> Option<WaitView> {
    let spawns: Vec<&&ToolCallBlock> = running
        .iter()
        .filter(|call| call.name == "spawn_agent")
        .collect();
    if spawns.is_empty() {
        return None;
    }

    let mut targets: Vec<WaitTarget> = state
        .team
        .children
        .iter()
        .filter(|child| matches!(child.status, ChildStatus::Running | ChildStatus::Waiting))
        .map(|child| {
            let label = if !child.nickname.is_empty() {
                child.nickname.clone()
            } else if !child.role.is_empty() {
                child.role.clone()
            } else {
                state.t().sub_agent_default.to_string()
            };
            WaitTarget {
                label,
                detail: child.recent_step.clone(),
                running_secs: Some(elapsed_since(
                    state.elapsed_secs,
                    child.started_elapsed_secs,
                )),
                lifecycle: TargetLifecycle::Running,
            }
        })
        .collect();

    if targets.is_empty() {
        for call in &spawns {
            targets.push(WaitTarget {
                label: spawn_agent_role_label(&call.arguments, state.t()),
                detail: None,
                running_secs: Some(elapsed_since(state.elapsed_secs, call.started_elapsed_secs)),
                lifecycle: TargetLifecycle::Running,
            });
        }
    }

    let wait_secs = spawns
        .iter()
        .map(|call| elapsed_since(state.elapsed_secs, call.started_elapsed_secs))
        .min()
        .unwrap_or(0);
    let count = targets.len().max(1);
    targets.truncate(MAX_TARGETS);
    Some(WaitView {
        kind: WaitKind::ChildAgent,
        wait_secs,
        count,
        targets,
    })
}

fn running_tools(state: &AppState) -> Vec<&ToolCallBlock> {
    for item in state.transcript.items().iter().rev() {
        match item {
            TranscriptItem::ToolGroup(group) => {
                return group
                    .calls
                    .iter()
                    .filter(|call| call.status == ToolStatus::Running)
                    .collect();
            }
            TranscriptItem::Assistant(_) | TranscriptItem::TurnEnd(_) => return Vec::new(),
            _ => {}
        }
    }
    Vec::new()
}

fn wait_task_id(arguments: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(arguments).ok()?;
    v.get("task_id")
        .and_then(|id| id.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn spawn_agent_role_label(arguments: &str, t: &UiText) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return t.sub_agent_default.to_string();
    };
    let role = v
        .get("profile")
        .or_else(|| v.get("role"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    match role {
        "explorer" => t.sub_agent_explorer,
        "worker" => t.sub_agent_worker,
        "reviewer" => t.sub_agent_reviewer,
        _ => t.sub_agent_default,
    }
    .to_string()
}

fn elapsed_since(now: u64, started: u64) -> u64 {
    now.saturating_sub(started)
}

/// Status-line headline: the wait reason, never a generic "等待任务".
pub(crate) fn headline(view: &WaitView, t: &UiText) -> String {
    let reason = match (view.kind, view.count) {
        (WaitKind::BackgroundTask, n) if n > 1 => {
            t.waiting_background_n.replace("{}", &n.to_string())
        }
        (WaitKind::BackgroundTask, _) => t.title_waiting_background.to_string(),
        (WaitKind::ChildAgent, n) if n > 1 => t.waiting_child_n.replace("{}", &n.to_string()),
        (WaitKind::ChildAgent, _) => t.waiting_child.to_string(),
        (WaitKind::Model, _) => t.waiting_model.to_string(),
        (WaitKind::Approval, _) => t.overlay_approval.to_string(),
    };
    match view.kind {
        WaitKind::Approval => reason,
        _ => format!("{reason} · {}", fmt_elapsed(view.wait_secs)),
    }
}

/// Compact indented disclosure of live wait targets. No last-activity text:
/// the runtime does not currently expose an authoritative activity timestamp
/// for background processes.
pub(crate) fn disclosure_plain(view: &WaitView, t: &UiText, width: usize) -> Vec<String> {
    if matches!(view.kind, WaitKind::Model | WaitKind::Approval) {
        return Vec::new();
    }
    let compact = view.targets.len() > 1;
    view.targets
        .iter()
        .flat_map(|target| format_target(target, t, width, compact))
        .collect()
}

fn format_target(target: &WaitTarget, t: &UiText, width: usize, compact: bool) -> Vec<String> {
    let running = match target.lifecycle {
        TargetLifecycle::Running => t.wait_target_running,
    };
    let elapsed = target.running_secs.map(fmt_elapsed).unwrap_or_default();
    let label_width = width.saturating_sub(4).max(8);
    let label = truncate_display(&target.label, label_width);
    if compact {
        let line = match (target.detail.as_deref(), elapsed.is_empty()) {
            (Some(detail), false) => {
                let detail = truncate_display(detail, 24);
                format!("  ● {label} · {detail} · {elapsed}")
            }
            (Some(detail), true) => {
                let detail = truncate_display(detail, 24);
                format!("  ● {label} · {detail}")
            }
            (None, false) => format!("  ● {label} · {running} · {elapsed}"),
            (None, true) => format!("  ● {label} · {running}"),
        };
        vec![truncate_display(&line, width.max(1))]
    } else {
        let head = truncate_display(&format!("  ● {label}"), width.max(1));
        let mut rows = vec![head];
        if let Some(detail) = target.detail.as_deref().filter(|d| !d.is_empty()) {
            rows.push(truncate_display(
                &format!(
                    "    {}",
                    truncate_display(detail, width.saturating_sub(4).max(8))
                ),
                width.max(1),
            ));
        }
        let meta = if elapsed.is_empty() {
            format!("    {running}")
        } else {
            format!("    {running} · {elapsed}")
        };
        rows.push(truncate_display(&meta, width.max(1)));
        rows
    }
}

/// Plain-text render used by tests as the copy contract for the status strip.
#[cfg(test)]
fn render_plain(view: &WaitView, t: &UiText, width: usize) -> String {
    let mut out = headline(view, t);
    for line in disclosure_plain(view, t, width) {
        out.push('\n');
        out.push_str(&line);
    }
    out
}

/// True when the live map still has background work that is *not* blocking Main.
pub(crate) fn non_blocking_background_count(state: &AppState) -> usize {
    if project(state).is_some_and(|v| v.kind == WaitKind::BackgroundTask) {
        return 0;
    }
    state.background_task_labels.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{BackgroundTaskChrome, Boot};
    use crate::theme::Theme;
    use leveler_client_protocol::{
        ApprovalId, MessageId, SessionId, ToolCallId, UiApprovalRequest,
    };

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

    fn start_wait(state: &mut AppState, call_id: &str, task_id: &str, started: u64) {
        state.status = RuntimeStatus::Busy;
        state.elapsed_secs = started.saturating_add(42);
        state.transcript.push_tool_started(
            ToolCallId::new(call_id),
            "wait_task".into(),
            serde_json::json!({ "task_id": task_id }).to_string(),
            false,
            started,
        );
        state.activity = Some("等待任务".into());
    }

    fn live_background(state: &mut AppState, task_id: &str, label: &str, started: u64) {
        state.background_task_labels.insert(
            task_id.into(),
            BackgroundTaskChrome {
                label: label.into(),
                started_elapsed_secs: started,
            },
        );
    }

    fn zh() -> &'static UiText {
        crate::i18n::Locale::Zh.text()
    }

    fn assert_no_generic_wait(text: &str) {
        for line in text.lines() {
            if line.contains("等待任务") {
                assert!(
                    line.contains("等待后台任务"),
                    "generic 等待任务 leaked: {text}"
                );
            }
        }
        assert!(
            !text.contains("可能卡住")
                && !text.contains("似乎无响应")
                && !text.contains("建议取消"),
            "elapsed-only stall warning is forbidden: {text}"
        );
        assert!(
            !text.contains("最近活动") && !text.contains("无新活动"),
            "last-activity must not be fabricated: {text}"
        );
    }

    #[test]
    fn background_wait_target_render() {
        let mut state = test_state();
        live_background(&mut state, "bg-2", "cargo test --workspace", 10);
        start_wait(&mut state, "w1", "bg-2", 12);
        state.elapsed_secs = 12 + 42;

        let view = project(&state).expect("wait reason");
        assert_eq!(view.kind, WaitKind::BackgroundTask);
        let text = render_plain(&view, zh(), 80);
        assert!(text.contains("等待后台任务"), "{text}");
        assert!(text.contains("cargo test --workspace"), "{text}");
        assert!(text.contains("运行中"), "{text}");
        assert!(!text.contains("bg-2"), "ids are not user-facing: {text}");
        assert!(
            !text.contains("task_id") && !text.contains('{'),
            "raw JSON must not appear: {text}"
        );
        assert_no_generic_wait(&text);
        assert_eq!(view.wait_secs, 42);
        assert_eq!(view.targets[0].running_secs, Some(44));
    }

    #[test]
    fn child_wait_target_render() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.elapsed_secs = 86;
        state.transcript.push_tool_started(
            ToolCallId::new("s1"),
            "spawn_agent".into(),
            serde_json::json!({ "profile": "worker", "task": "run cargo check" }).to_string(),
            false,
            0,
        );
        state
            .team
            .children
            .push(crate::multi_agent::ChildAgentView {
                id: "child-uuid-should-not-render".into(),
                nickname: "Worker".into(),
                role: "执行 Agent".into(),
                profile_id: Some("worker".into()),
                capabilities: Vec::new(),
                purpose: "check".into(),
                status: ChildStatus::Running,
                contribution: crate::multi_agent::Contribution::Pending,
                recent_step: Some("shell_command · cargo check".into()),
                input_tokens: 0,
                output_tokens: 0,
                started_elapsed_secs: 8,
                detail: None,
            });

        let view = project(&state).expect("child wait");
        assert_eq!(view.kind, WaitKind::ChildAgent);
        let text = render_plain(&view, zh(), 80);
        assert!(text.contains("等待子 Agent"), "{text}");
        assert!(text.contains("Worker"), "{text}");
        assert!(text.contains("cargo check"), "{text}");
        assert!(!text.contains("child-uuid-should-not-render"), "{text}");
        assert_no_generic_wait(&text);
    }

    #[test]
    fn model_wait_render() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.elapsed_secs = 12;
        let view = project(&state).expect("model wait");
        assert_eq!(view.kind, WaitKind::Model);
        let text = render_plain(&view, zh(), 80);
        assert!(text.contains("等待模型"), "{text}");
        assert!(!text.contains("等待任务"), "{text}");
    }

    #[test]
    fn approval_wait_render() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.overlay = Some(Overlay::Approval(Box::new(
            crate::overlay::ApprovalOverlay::new(UiApprovalRequest {
                id: ApprovalId::new("a1"),
                tool: "run_command".into(),
                summary: "git push".into(),
                command: Some("git push".into()),
                risks: vec![],
            }),
        )));
        let view = project(&state).expect("approval wait");
        assert_eq!(view.kind, WaitKind::Approval);
        let text = render_plain(&view, zh(), 80);
        assert!(text.contains("等待审批"), "{text}");
        assert!(!text.contains("等待后台任务"), "{text}");
    }

    #[test]
    fn background_running_does_not_force_waiting() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        live_background(&mut state, "bg-1", "cargo test --workspace", 0);
        state.activity = Some("正在汇总审计结果".into());
        state.transcript.push_tool_started(
            ToolCallId::new("r1"),
            "read_file".into(),
            serde_json::json!({ "path": "README.md" }).to_string(),
            false,
            0,
        );
        assert!(
            project(&state).is_none(),
            "a live background process is not itself a Main wait"
        );
        assert_eq!(non_blocking_background_count(&state), 1);
    }

    #[test]
    fn completed_dependency_clears_wait() {
        let mut state = test_state();
        live_background(&mut state, "bg-2", "cargo test --workspace", 0);
        start_wait(&mut state, "w1", "bg-2", 0);
        assert_eq!(
            project(&state).map(|v| v.kind),
            Some(WaitKind::BackgroundTask)
        );

        state.background_task_labels.remove("bg-2");
        assert!(
            project(&state).is_none()
                || project(&state).is_some_and(|v| v.kind != WaitKind::BackgroundTask),
            "a settled background task must not keep a stale wait"
        );
    }

    #[test]
    fn failed_dependency_not_rendered_running() {
        let mut state = test_state();
        start_wait(&mut state, "w1", "bg-fail", 0);
        // Exit already applied: the live map has no entry, so nothing is
        // rendered as 运行中.
        let view = project(&state);
        if let Some(view) = view {
            assert!(
                view.targets
                    .iter()
                    .all(|t| t.lifecycle != TargetLifecycle::Running)
                    || view.kind != WaitKind::BackgroundTask,
                "failed work must not render as running: {view:?}"
            );
        }
    }

    #[test]
    fn missing_last_activity_is_not_fabricated() {
        let mut state = test_state();
        live_background(&mut state, "bg-2", "cargo test --workspace", 0);
        start_wait(&mut state, "w1", "bg-2", 0);
        let view = project(&state).unwrap();
        let text = render_plain(&view, zh(), 80);
        assert!(text.contains("运行中"), "{text}");
        assert!(!text.contains("最近活动"), "{text}");
        assert!(!text.contains("无新活动"), "{text}");
    }

    #[test]
    fn wait_target_truncation() {
        let mut state = test_state();
        let long = format!(
            "cargo test --workspace {}",
            "very-long-package-name-".repeat(20)
        );
        live_background(&mut state, "bg-2", &long, 0);
        start_wait(&mut state, "w1", "bg-2", 0);
        let view = project(&state).unwrap();
        let text = render_plain(&view, zh(), 40);
        assert!(text.contains("等待后台任务"), "{text}");
        assert!(
            text.lines()
                .all(|line| unicode_width::UnicodeWidthStr::width(line) <= 80),
            "{text}"
        );
        assert!(!text.contains('{'), "{text}");
    }

    #[test]
    fn multi_dependency_wait_uses_count() {
        let mut state = test_state();
        live_background(&mut state, "bg-a", "cargo test", 0);
        live_background(&mut state, "bg-b", "npm run dev", 0);
        live_background(&mut state, "bg-c", "cargo clippy", 0);
        state.status = RuntimeStatus::Busy;
        state.elapsed_secs = 48;
        for (i, id) in ["bg-a", "bg-b", "bg-c"].iter().enumerate() {
            state.transcript.push_tool_started(
                ToolCallId::new(format!("w{i}")),
                "wait_task".into(),
                serde_json::json!({ "task_id": id }).to_string(),
                true,
                0,
            );
        }
        let view = project(&state).unwrap();
        assert_eq!(view.count, 3);
        let text = render_plain(&view, zh(), 80);
        assert!(text.contains("等待 3 个后台任务"), "{text}");
        assert!(text.contains("cargo test"), "{text}");
        assert!(text.contains("npm run dev"), "{text}");
    }

    #[test]
    fn english_copy_is_translated() {
        let t = crate::i18n::Locale::En.text();
        let view = WaitView {
            kind: WaitKind::BackgroundTask,
            wait_secs: 12,
            count: 1,
            targets: vec![WaitTarget {
                label: "cargo test".into(),
                detail: None,
                running_secs: Some(10),
                lifecycle: TargetLifecycle::Running,
            }],
        };
        let text = render_plain(&view, t, 80);
        assert!(text.contains("waiting for background task"), "{text}");
        assert!(text.contains("running"), "{text}");
        assert!(!text.contains("Wait Task"), "{text}");
    }

    #[test]
    fn wait_projection_ignores_assistant_prose() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.transcript.begin_assistant(MessageId::new("m1"));
        state.transcript.append_assistant(
            &MessageId::new("m1"),
            "现在开始编译 workspace 自身的 crate...",
        );
        live_background(&mut state, "bg-2", "cargo test --workspace", 0);
        assert!(
            project(&state).is_none() || project(&state).is_some_and(|v| v.kind == WaitKind::Model),
            "prose must not invent a background wait"
        );
    }
}
