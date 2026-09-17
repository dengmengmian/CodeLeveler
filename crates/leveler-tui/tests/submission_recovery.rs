//! A turn input whose outcome the runtime proves unrecoverable: the old
//! command stays unknown and is never sent again, and the user gets control
//! back to start a new one.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{
    CommandId, PermissionProfile, RuntimeEvent, RuntimeStatus, SessionId, UiSessionSnapshot,
};
use leveler_tui::action::{Action, Effect, EffectCompletion};
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use leveler_tui::transcript::TranscriptItem;

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
            reasoning_effort: None,
        },
    )
}

fn idle_snapshot() -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: "/repo".to_string(),
        goal: "interactive session".to_string(),
        model: None,
        mode: PermissionProfile::Assisted,
        branch: None,
        status: "idle".to_string(),
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        plan: None,
        verification: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        reasoning: None,
        work_profile: None,
        collaboration: None,
        children: Vec::new(),
    }
}

fn enter(state: &mut AppState, text: &str) -> Vec<Effect> {
    state.composer.replace(text);
    reduce(
        state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())),
    )
}

fn submitted_id(effects: &[Effect]) -> CommandId {
    match effects {
        [Effect::Submit { command_id, .. }] => command_id.clone(),
        other => panic!("expected one Submit, got {other:?}"),
    }
}

/// Sent, no answer, then the runtime proves the outcome unrecoverable.
fn unresolvable_after_reconnect(state: &mut AppState, text: &str) -> CommandId {
    let command_id = submitted_id(&enter(state, text));
    reduce(
        state,
        Action::EffectCompleted(EffectCompletion::SubmissionUnconfirmed {
            command_id: command_id.clone(),
        }),
    );
    reduce(
        state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: idle_snapshot(),
        }),
    );
    assert!(state.is_busy(), "no answer yet still holds the session");
    let effects = reduce(
        state,
        Action::EffectCompleted(EffectCompletion::SubmissionUnresolvable {
            command_id: command_id.clone(),
            snapshot: Some(Box::new(idle_snapshot())),
        }),
    );
    assert!(effects.is_empty(), "nothing is sent again: {effects:?}");
    command_id
}

#[test]
fn an_unresolvable_submission_gives_control_back_for_a_new_command() {
    let mut s = state();
    let old = unresolvable_after_reconnect(&mut s, "部署到生产");

    assert!(s.pending_submissions.is_empty());
    assert!(!s.is_busy());
    assert!(
        s.composer.is_empty(),
        "the old text is not offered back as a draft one Enter from rerunning"
    );

    let new = submitted_id(&enter(&mut s, "先看一下日志"));
    assert_ne!(new, old, "a new logical command, not a retry");
    assert_eq!(s.pending_submissions.len(), 1);
}

#[test]
fn an_unresolvable_submission_reads_as_unknown_not_as_an_ending() {
    let mut s = state();
    unresolvable_after_reconnect(&mut s, "部署到生产");

    assert_ne!(s.status, RuntimeStatus::Error);
    let items = s.transcript.items();
    assert!(
        items
            .iter()
            .any(|item| matches!(item, TranscriptItem::User(text) if text == "部署到生产")),
        "what the user sent stays in the conversation: {items:?}"
    );
    assert!(
        !items.iter().any(|item| matches!(
            item,
            TranscriptItem::TurnEnd(_) | TranscriptItem::Error(_) | TranscriptItem::Completion(_)
        )),
        "no ending is invented: {items:?}"
    );
    let note = items
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::Note(text) => Some(text.as_str()),
            _ => None,
        })
        .expect("the unknown outcome is recorded where the user reads");
    assert!(note.contains("无法确认"), "{note}");
    for claim in ["失败", "已取消", "未送达", "已完成"] {
        assert!(!note.contains(claim), "{claim} in {note}");
    }
}

#[test]
fn a_late_answer_for_an_unresolvable_command_changes_nothing() {
    let mut s = state();
    let old = unresolvable_after_reconnect(&mut s, "部署到生产");
    let effects = reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::SubmissionUnconfirmed { command_id: old }),
    );
    assert!(effects.is_empty());
    assert!(s.pending_submissions.is_empty());
    assert!(!s.is_busy());
}
