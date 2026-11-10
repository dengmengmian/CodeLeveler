//! The blocked terminal is a distinct client state: the goal is unsettled, so
//! the user can continue it, and the model's declared summary is displayed as
//! the reason without becoming the runtime's resume action.

use leveler_client_protocol::{AnswerEffect, RuntimeEvent, RuntimeStatus, SessionId, ToolCallId};

use super::reduce;
use crate::action::Action;
use crate::state::{AppState, Boot};
use crate::theme::Theme;
use crate::transcript::{TranscriptItem, TurnEndStatus};

fn state() -> AppState {
    let mut state = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s-blocked"),
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
    state.size = (120, 40);
    state
}

/// The model declared the goal blocked, with the summary and a concrete next
/// step the way the audited session did.
fn declare_blocked(state: &mut AppState) {
    let id = ToolCallId::new("goal");
    reduce(
        state,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: id.clone(),
            name: "update_goal".into(),
            arguments: serde_json::json!({
                "status": "blocked",
                "summary": "页面与7条旅程已交付；死信/对账/席位旅程受环境数据阻断",
                "next_step": "补齐死信与席位数据后重跑 e2e",
            })
            .to_string(),
            parallel: false,
            model_step: None,
            answer_effect: Some(AnswerEffect::Bookkeeping),
        }),
    );
    reduce(
        state,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: None,
            id,
            ok: true,
            preview: "目标已更新".into(),
            duration_ms: 10,
            applied_diff: None,
        }),
    );
}

#[test]
fn a_blocked_goal_is_resumable_and_offers_the_runtime_continuation() {
    let mut state = state();
    declare_blocked(&mut state);
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::TurnBlocked {
            reason: "死信数据缺失，无法完成旅程验证".into(),
        }),
    );

    assert_eq!(state.status, RuntimeStatus::Idle);
    assert!(
        state.resumable_task,
        "the runtime leaves a blocked goal unsettled; the client must not withdraw continuation"
    );
    let end = state
        .transcript
        .last_turn_end()
        .expect("the blocked terminal is on the transcript");
    assert_eq!(end.status, TurnEndStatus::Blocked);
    assert_eq!(
        end.detail.as_deref(),
        Some("死信数据缺失，无法完成旅程验证"),
        "the model-declared reason reaches the terminal verbatim"
    );

    // The offered next action is the runtime's continuation, not the model's
    // (possibly stale) suggestion.
    assert_eq!(
        state.prompt_suggestion.as_deref(),
        Some("继续"),
        "the model's next_step must not masquerade as the confirmed resume action"
    );
    // It stays visible as a recap, where it is the model's own advice.
    assert!(
        state.transcript.items().iter().any(|item| matches!(
            item,
            TranscriptItem::Recap(recap)
                if recap.next_step.as_deref() == Some("补齐死信与席位数据后重跑 e2e")
        )),
        "the model's declared next step is kept as history, not dropped"
    );
}

#[test]
fn an_ordinary_incomplete_turn_is_not_a_resumable_goal_terminal() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::TurnIncomplete {
            reason: "预算已耗尽".into(),
        }),
    );
    assert!(!state.resumable_task);
    let end = state
        .transcript
        .last_turn_end()
        .expect("the incomplete terminal is on the transcript");
    assert_eq!(end.status, TurnEndStatus::Incomplete);
}
