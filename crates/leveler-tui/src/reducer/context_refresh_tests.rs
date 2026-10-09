//! Resuming a session must show the runtime's context accounting immediately —
//! not only after the next turn — and a stale accounting must never survive a
//! model switch or a compaction.
//!
//! The client never computes a token figure here: it asks the runtime
//! (`QueryContext`) when a session opens or a setting changes, and adopts the
//! answer. These tests pin the request and the adoption, plus the two
//! invalidations that would otherwise show one model's denominator under
//! another model's name.

use leveler_client_protocol::{
    ClientCommand, CommandId, PermissionProfile, RuntimeEvent, SessionId, UiSessionSnapshot,
};
use leveler_model::{
    ContextAccounting, ContextPressure, FoldRequirement, ModelRef, TokenCountKind,
};

use super::reduce;
use crate::action::{Action, Effect};
use crate::state::{AppState, Boot};

fn state() -> AppState {
    let mut s = AppState::new(
        crate::theme::Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: crate::i18n::Locale::En,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    s.size = (120, 40);
    s
}

fn snapshot(model: &str) -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: Some("/repo".into()),
        task_status: None,
        task_terminal: None,
        goal: "g".into(),
        model: ModelRef::parse(model),
        mode: PermissionProfile::Assisted,
        branch: None,
        status: "idle".into(),
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
        collaboration: None,
        children: Vec::new(),
    }
}

fn accounting() -> ContextAccounting {
    ContextAccounting {
        model: ModelRef::new("deepseek", "deepseek-chat"),
        context_window_tokens: Some(128_000),
        compact_at_tokens: Some(64_000),
        output_reservation_tokens: Some(32_000),
        headroom_tokens: Some(0),
        input_capacity_tokens: Some(96_000),
        fold_state: FoldRequirement::None,
        used_tokens: 10_000,
        free_tokens: Some(118_000),
        token_count_kind: TokenCountKind::Estimated,
        pressure: ContextPressure::Normal,
        categories: Vec::new(),
        last_compaction: None,
        reasoning_projection: None,
    }
}

fn queries(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|effect| matches!(effect, Effect::Send(ClientCommand::QueryContext { .. })))
        .count()
}

fn answer_context(s: &mut AppState, accounting: Option<ContextAccounting>) {
    let query_id = s
        .context
        .pending_query_id
        .clone()
        .expect("opening a session must leave a query this view owns");
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ContextLoaded {
            query_id: Some(query_id),
            accounting,
        }),
    );
}

/// Opening (or resuming) a session asks the runtime for its accounting, so the
/// footer and `/context` show the compaction axis without waiting for a turn.
#[test]
fn opening_a_session_asks_the_runtime_for_the_context() {
    let mut s = state();
    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("deepseek/v3"),
        }),
    );
    assert_eq!(queries(&effects), 1, "{effects:?}");
    assert!(
        s.context.pending_query_id.is_some(),
        "the query must be owned so a foreign answer is ignored"
    );
}

/// The runtime's answer reaches the gauge: projected input over the effective
/// input capacity, the runtime's own numbers.
#[test]
fn the_runtime_answer_becomes_the_footer_gauge() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("deepseek/v3"),
        }),
    );
    answer_context(&mut s, Some(accounting()));
    assert_eq!(
        s.context.loaded.as_ref().map(|a| a.used_tokens),
        Some(10_000)
    );
    let chip = crate::status_line::footer_ctx_chip(&s).expect("a resolved capacity is a gauge");
    assert!(chip.contains("10k/96k"), "compaction axis: {chip}");
}

/// A model switch changes the resolved policy, so the previous accounting's
/// window, reservation and soft threshold are wrong. The client drops them and
/// re-asks; the runtime has already dropped its copy.
#[test]
fn a_model_switch_drops_the_stale_accounting_and_re_asks() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("deepseek/v3"),
        }),
    );
    answer_context(&mut s, Some(accounting()));
    assert!(s.context.loaded.is_some());

    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated {
            session: snapshot("deepseek/reasoner"),
        }),
    );
    assert!(
        s.context.loaded.is_none(),
        "one model's denominator must not survive under another's name"
    );
    assert_eq!(s.context_tokens, 0, "the token fallback is stale too");
    assert_eq!(queries(&effects), 1, "the new policy must be re-asked");
}

/// After a compaction the measured transcript is gone; neither the snapshot
/// nor the token fallback may keep showing the pre-fold size.
#[test]
fn a_compaction_clears_both_the_accounting_and_the_fallback() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("deepseek/v3"),
        }),
    );
    answer_context(&mut s, Some(accounting()));
    s.context_tokens = 200_000;
    s.token_input = 150_000;

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ContextCompacted { from: 40, to: 1 }),
    );
    assert!(s.context.loaded.is_none());
    assert_eq!(s.context_tokens, 0);
    assert_eq!(s.token_input, 0);
    assert!(
        crate::status_line::footer_ctx_chip(&s).is_none(),
        "no figure may be shown until the next request is measured"
    );
}

/// A runtime that has no accounting (a fresh process) answers `None`, and the
/// client must not keep a value from the session it just left.
#[test]
fn a_none_answer_clears_the_previous_sessions_gauge() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("deepseek/v3"),
        }),
    );
    answer_context(&mut s, Some(accounting()));

    // A different session with a different id opens; its answer carries none.
    let mut other = snapshot("deepseek/v3");
    other.id = SessionId::new("s2");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: other }),
    );
    assert!(s.context.loaded.is_none());
    // A foreign/stale answer must not resurrect it either.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ContextLoaded {
            query_id: Some(CommandId::generate()),
            accounting: Some(accounting()),
        }),
    );
    assert!(s.context.loaded.is_none());
}
