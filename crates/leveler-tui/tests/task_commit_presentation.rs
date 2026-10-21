//! Visible task execution in a Chat session.
//!
//! A `commit_task` tool result is the runtime's authority that the turn now
//! owes a durable task. The TUI must show that without pretending the session
//! became a `/goal` session, and must keep showing a paused task until it
//! settles or is cancelled.

use leveler_client_protocol::{
    ModelRef, PermissionProfile, RuntimeEvent, SessionId, UiExecutionTask, UiSessionSnapshot,
};
use leveler_core::ToolCallId;
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

fn state() -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".to_string(),
            version: "0.1.0".to_string(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    )
}

fn tool_started(state: &mut AppState, id: &str, name: &str, arguments: &str) {
    reduce(
        state,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: name.to_string(),
            arguments: arguments.to_string(),
            parallel: false,
            model_step: None,
            answer_effect: None,
        }),
    );
}

fn tool_completed(state: &mut AppState, id: &str, ok: bool) {
    reduce(
        state,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new(id),
            ok,
            preview: String::new(),
            duration_ms: 1,
            applied_diff: None,
            exit_code: None,
            stop: None,
        }),
    );
}

fn snapshot(status: &str, task: Option<UiExecutionTask>) -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: Some("/repo".to_string()),
        task_status: None,
        task_terminal: None,
        goal: "会话标题".to_string(),
        model: ModelRef::parse("deepseek/v3"),
        mode: PermissionProfile::Assisted,
        branch: None,
        status: status.to_string(),
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        active_background_tasks: Vec::new(),
        plan: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        thinking: None,
        work_profile: None,
        collaboration: Some("chat".to_string()),
        execution_task: task,
        children: Vec::new(),
    }
}

fn header_text(state: &AppState) -> String {
    leveler_tui::active_goal::header(state, 80)
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[test]
fn a_successful_commit_marks_the_turn_as_executing_a_task() {
    let mut state = state();
    assert!(header_text(&state).is_empty(), "idle chat has no badge");

    tool_started(
        &mut state,
        "c1",
        "commit_task",
        r#"{"objective":"接入全部运营能力","acceptance":["go test ./..."]}"#,
    );
    assert!(
        !state.execution_task_active,
        "the declaration is visible only after the runtime accepts it"
    );

    tool_completed(&mut state, "c1", true);

    assert!(
        state.goal_mode_active,
        "the turn now runs the goal contract"
    );
    assert!(state.execution_task_active);
    let header = header_text(&state);
    assert!(
        header.contains("目标模式") && !header.contains("/goal"),
        "a model-declared task is not a /goal command: {header}"
    );
}

#[test]
fn a_rejected_commit_changes_nothing() {
    let mut state = state();
    tool_started(
        &mut state,
        "c1",
        "commit_task",
        r#"{"objective":"x","acceptance":["y"]}"#,
    );
    tool_completed(&mut state, "c1", false);

    assert!(!state.goal_mode_active);
    assert!(!state.execution_task_active);
    assert!(header_text(&state).is_empty());
}

#[test]
fn an_ordinary_tool_never_marks_a_task() {
    let mut state = state();
    tool_started(&mut state, "c1", "read_file", r#"{"path":"a.rs"}"#);
    tool_completed(&mut state, "c1", true);

    assert!(!state.goal_mode_active);
    assert!(!state.execution_task_active);
    assert!(header_text(&state).is_empty());
}

#[test]
fn a_paused_task_stays_visible_until_it_settles_or_is_cancelled() {
    let mut state = state();
    tool_started(
        &mut state,
        "c1",
        "commit_task",
        r#"{"objective":"接入全部运营能力","acceptance":["go test ./..."]}"#,
    );
    tool_completed(&mut state, "c1", true);

    // Esc / Ctrl+C stops the work window, not the logical task.
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCancelled));
    assert!(!state.goal_mode_active, "the window is no longer executing");
    assert!(
        state.execution_task_active,
        "the task still owes work and `继续` can re-enter it"
    );
    let header = header_text(&state);
    assert!(
        header.contains("已暂停"),
        "a paused task must say so: {header}"
    );

    // The user cancels the logical task: it is terminal.
    reduce(&mut state, Action::Runtime(RuntimeEvent::TaskCancelled));
    assert!(!state.execution_task_active);
    assert!(header_text(&state).is_empty());
}

#[test]
fn a_completed_turn_settles_the_visible_task() {
    let mut state = state();
    tool_started(
        &mut state,
        "c1",
        "commit_task",
        r#"{"objective":"接入全部运营能力","acceptance":["go test ./..."]}"#,
    );
    tool_completed(&mut state, "c1", true);

    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: leveler_client_protocol::MessageId::new("m1"),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: leveler_client_protocol::MessageId::new("m1"),
            delta: "全部完成。".to_string(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted {
            message_id: leveler_client_protocol::MessageId::new("m1"),
        }),
    );
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCompleted));

    assert!(!state.goal_mode_active);
    assert!(
        !state.execution_task_active,
        "a completed task stops owning the badge"
    );
}

#[test]
fn a_hard_failure_clears_the_task_badge() {
    let mut state = state();
    tool_started(
        &mut state,
        "c1",
        "commit_task",
        r#"{"objective":"接入全部运营能力","acceptance":["go test ./..."]}"#,
    );
    tool_completed(&mut state, "c1", true);

    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::TurnFailed {
            error: "internal invariant violated".into(),
            failure: None,
        }),
    );

    assert!(!state.resumable_task);
    assert!(
        !state.execution_task_active,
        "a non-resumable failure does not leave a resumable task badge"
    );
    assert!(header_text(&state).is_empty());
}

#[test]
fn a_reconnect_restores_a_running_task_from_the_snapshot() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(
                "running",
                Some(UiExecutionTask {
                    goal_id: "g1".into(),
                    objective: "接入全部运营能力".into(),
                    running: true,
                }),
            ),
        }),
    );

    assert!(state.execution_task_active);
    assert!(state.goal_mode_active, "a running task owns the turn mode");
    let header = header_text(&state);
    assert!(
        header.contains("目标模式") && header.contains("执行中"),
        "the badge must say a task is running: {header}"
    );
}

#[test]
fn a_reconnect_restores_an_owed_paused_task_from_the_snapshot() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(
                "interrupted",
                Some(UiExecutionTask {
                    goal_id: "g1".into(),
                    objective: "接入全部运营能力".into(),
                    running: false,
                }),
            ),
        }),
    );

    assert!(
        state.execution_task_active,
        "the work is still owed after a crash"
    );
    assert!(!state.goal_mode_active, "no window is executing it");
    let header = header_text(&state);
    assert!(
        header.contains("已暂停"),
        "a reconnecting client must see the task is resumable: {header}"
    );
}

#[test]
fn a_reconnect_without_a_task_shows_no_badge() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("idle", None),
        }),
    );

    assert!(!state.execution_task_active);
    assert!(header_text(&state).is_empty());
}
