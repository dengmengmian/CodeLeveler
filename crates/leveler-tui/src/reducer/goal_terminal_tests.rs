//! Active Goal terminal projection. The goal lifecycle and the presence of a
//! final answer are independent facts; the header must not turn an accepted
//! goal red because the model closed with a bookkeeping call.

use std::time::Instant;

use leveler_client_protocol::{RuntimeEvent, SessionId};

use super::reduce;
use crate::action::Action;
use crate::active_goal::{ActiveGoal, GoalPhase};
use crate::state::{AppState, Boot};
use crate::theme::Theme;

fn state() -> AppState {
    let mut state = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s-goal"),
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
    state.size = (100, 30);
    state.goal_mode_active = true;
    state.execution_task_active = true;
    state.active_goal = Some(ActiveGoal::running(
        "ship the change".into(),
        Instant::now(),
    ));
    state
}

/// The real TUI dogfood terminal: `update_goal(complete)` ended the turn with
/// no final prose. The goal passed its acceptance gate; "no answer" is a
/// separate UI fact, never a goal failure.
#[test]
fn an_accepted_goal_stays_completed_when_the_turn_has_no_final_prose() {
    let mut state = state();
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCompleted));
    let goal = state.active_goal.as_ref().expect("the goal stays visible");
    assert_eq!(goal.phase(), GoalPhase::Completed);
    assert!(
        !state.execution_task_active,
        "an accepted goal does not keep an owed-task badge"
    );
}

/// A turn that never completed the goal is not green. The transcript still
/// reports the missing answer, but the goal header stays actionable.
#[test]
fn a_failed_turn_keeps_the_goal_failed() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::TurnFailed {
            error: "boom".into(),
            failure: None,
            recovery: None,
        }),
    );
    assert_eq!(
        state.active_goal.as_ref().map(ActiveGoal::phase),
        Some(GoalPhase::Failed)
    );
}

/// A user pause is not a completion. The turn answered inside the authorized
/// range, so the terminal marker is an answer — but the goal still owes the
/// withheld stage, so the header must say paused and the task must stay
/// resumable. Rendering `✓ 目标已完成` here is exactly the lie this guards.
#[test]
fn a_user_pause_keeps_the_goal_paused_not_completed() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::TurnPaused {
            reason: "目标已暂停 · 等待你授权继续".into(),
        }),
    );
    let goal = state.active_goal.as_ref().expect("the goal stays visible");
    assert_eq!(goal.phase(), GoalPhase::Paused);
    assert_ne!(goal.phase(), GoalPhase::Completed);
    assert_ne!(goal.phase(), GoalPhase::Failed);
    assert!(
        state.execution_task_active,
        "the goal still owes the withheld stage"
    );
    assert!(state.resumable_task, "the user can continue it");
    assert_eq!(
        state.notification.as_ref().map(|n| n.message.as_str()),
        Some("目标已暂停 · 等待你授权继续")
    );
}

/// The real accepted-shape refusal from session `1c3bf9aa` (DevOrder,
/// 2026-10-30): the runtime refused `update_goal(complete)` because its test
/// vocabulary did not contain the suite the model had just run.
///
/// The user must see that the closeout was turned down. They must not see a
/// cause the runtime did not report, and the runtime's model-facing English
/// reasoning is not theirs to read. A session recorded before the refusal
/// carried its gate reaches this fallback; the tagged path is covered by
/// [`a_refused_closeout_states_the_gate_the_runtime_named`].
#[test]
fn a_refused_closeout_is_shown_as_a_warning_without_an_invented_cause() {
    use leveler_client_protocol::ToolCallId;

    let mut state = state();
    let id = ToolCallId::new("goal-1");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: id.clone(),
            name: "update_goal".into(),
            arguments: serde_json::json!({
                "status": "complete",
                "summary": "本地全套已启动，13 组真实回归 191 条断言全过"
            })
            .to_string(),
            parallel: false,
            model_step: Some(24),
            answer_effect: None,
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id,
            ok: false,
            preview: "update_goal(complete) was refused: the user's stated scope is not fully \
                      accounted for.\n- the user explicitly requires the tests to pass, but no \
                      test command ran in this task"
                .into(),
            duration_ms: 1,
            applied_diff: None,
            exit_code: None,
            stop: None,
        }),
    );

    let screen = screen(&mut state, 120);
    // Wide glyphs claim two cells, so a row read cell-by-cell carries a filler
    // between CJK characters. Compare without whitespace.
    let painted: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(painted.contains("目标收尾"), "{screen}");
    assert!(screen.contains('⚠'), "{screen}");
    assert!(
        painted.contains("目标未更新：收尾请求被拒绝"),
        "the row states the fact: {screen}"
    );
    assert!(
        !painted.contains("需先说明状态"),
        "the row must not name a check the runtime never ran: {screen}"
    );
}

/// The runtime now names the gate in the result preview. Each kind gets its
/// own localized sentence, and the runtime's reasoning for the model stays
/// behind the fold. This is the live event → transcript → frame chain a user
/// reads after a refused closeout.
#[test]
fn a_refused_closeout_states_the_gate_the_runtime_named() {
    use leveler_client_protocol::ToolCallId;

    for (kind, note) in [
        ("outstanding_children", "子任务仍在运行"),
        ("user_pause_boundary", "目标已暂停"),
        ("invalid_status", "缺少状态"),
        ("invalid_verification", "无法解析"),
        ("invalid_requirements", "无法解析"),
        ("verification_refused", "验收证据不成立"),
        ("requirements_refused", "用户要求的条件尚未全部满足"),
    ] {
        let mut state = state();
        let id = ToolCallId::new("goal-1");
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::ToolCallStarted {
                id: id.clone(),
                name: "update_goal".into(),
                arguments: serde_json::json!({"status": "complete"}).to_string(),
                parallel: false,
                model_step: Some(7),
                answer_effect: None,
            }),
        );
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::ToolCallCompleted {
                id,
                ok: false,
                preview: format!("[goal_refused:{kind}]\nruntime account for {kind}"),
                duration_ms: 1,
                applied_diff: None,
                exit_code: None,
                stop: None,
            }),
        );

        let screen = screen(&mut state, 120);
        let painted: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            painted.contains(note),
            "{kind} must state its own reason: {screen}"
        );
        assert!(screen.contains('⚠'), "{kind} is a warning: {screen}");
        assert!(
            !painted.contains(&format!("runtimeaccountfor{kind}")),
            "{kind}: the model-facing account stays behind the fold: {screen}"
        );
        assert!(
            !painted.contains("goal_refused"),
            "{kind}: the wire tag is never painted: {screen}"
        );
        assert!(
            !painted.contains("需先说明状态"),
            "{kind}: no invented cause: {screen}"
        );
    }
}

fn screen(state: &mut AppState, width: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, state))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
