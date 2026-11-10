//! Token accounting at the client boundary: a live request snapshot reports
//! one request, while the finished turn is settled once from the runtime's
//! durable model-call ledger.

use leveler_client_protocol::{RuntimeEvent, SessionId, UiTurnUsage};

use super::reduce;
use crate::action::Action;
use crate::state::{AppState, Boot};
use crate::theme::Theme;

fn state() -> AppState {
    let mut state = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s-usage"),
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

fn snapshot(input: u32, output: u32, cached: u32) -> RuntimeEvent {
    RuntimeEvent::TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached,
        reasoning_tokens: None,
    }
}

fn booked(input: u64, output: u64, cached: Option<u64>) -> RuntimeEvent {
    RuntimeEvent::TurnUsage {
        usage: UiTurnUsage {
            requests: 2,
            unmeasured_requests: 0,
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: cached,
        },
    }
}

#[test]
fn request_snapshots_stay_one_request_and_are_not_a_turn_sum() {
    let mut state = state();
    // The provider may repeat usage within one request; a later request is a
    // new snapshot. Neither may be added to the previous one.
    reduce(&mut state, Action::Runtime(snapshot(1_000, 100, 900)));
    reduce(&mut state, Action::Runtime(snapshot(1_200, 120, 1_100)));
    reduce(&mut state, Action::Runtime(snapshot(2_000, 200, 1_800)));
    assert_eq!(state.token_input, 2_000);
    assert_eq!(state.token_output, 200);

    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCompleted));
    let end = state
        .transcript
        .last_turn_end()
        .expect("the terminal is on the transcript");
    assert_eq!(
        (end.input_tokens, end.output_tokens, end.cached_tokens),
        (0, 0, 0),
        "a turn with no booked projection must not present a client-side sum"
    );
}

#[test]
fn the_booked_usage_arriving_before_the_terminal_is_frozen_into_its_receipt() {
    let mut state = state();
    reduce(&mut state, Action::Runtime(snapshot(1_000, 100, 900)));
    // The bridge's normal order: settled usage, then the display terminal.
    reduce(
        &mut state,
        Action::Runtime(booked(45_491_035, 141_675, Some(44_829_952))),
    );
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCompleted));

    let end = state
        .transcript
        .last_turn_end()
        .expect("the terminal is on the transcript");
    assert_eq!(
        (end.input_tokens, end.output_tokens, end.cached_tokens),
        (45_491_035, 141_675, 44_829_952)
    );
}

#[test]
fn the_booked_usage_patches_the_receipt_the_terminal_already_froze() {
    let mut state = state();
    reduce(&mut state, Action::Runtime(snapshot(1_000, 100, 900)));
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCompleted));

    // The runtime's durable aggregation, including the audited session's own
    // figures. Applying it twice (a replay) must not double it.
    let booked_usage = booked(45_491_035, 141_675, Some(44_829_952));
    reduce(&mut state, Action::Runtime(booked_usage.clone()));
    reduce(&mut state, Action::Runtime(booked_usage));

    let end = state
        .transcript
        .last_turn_end()
        .expect("the terminal is on the transcript");
    assert_eq!(end.input_tokens, 45_491_035);
    assert_eq!(end.output_tokens, 141_675);
    assert_eq!(end.cached_tokens, 44_829_952);
    assert_eq!(
        end.input_tokens + end.output_tokens,
        45_632_710,
        "the turn total is the persisted ledger, not twice it"
    );
}

#[test]
fn a_late_booked_usage_never_lands_on_the_next_turn() {
    let mut state = state();
    reduce(&mut state, Action::Runtime(snapshot(1_000, 100, 900)));
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCompleted));
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: leveler_client_protocol::UiMessage {
                id: leveler_client_protocol::MessageId::new("m-next"),
                role: leveler_client_protocol::UiRole::User,
                text: "next".into(),
                ordinal: None,
                kind: None,
                images: 0,
            },
        }),
    );
    reduce(&mut state, Action::Runtime(booked(9_000, 900, None)));
    let end = state
        .transcript
        .last_turn_end()
        .expect("the previous terminal is still on the transcript");
    assert_eq!(
        (end.input_tokens, end.output_tokens),
        (0, 0),
        "a user message between turns ends the terminal's eligibility"
    );
}
