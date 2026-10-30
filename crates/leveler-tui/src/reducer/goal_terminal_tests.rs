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
