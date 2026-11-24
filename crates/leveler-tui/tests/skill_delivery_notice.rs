//! The compact status-line note for what this turn's message selected.
//!
//! The durable record is the engine's skill-delivery audit; this file only
//! pins what the person sees while the turn runs: loaded skills are quiet
//! information, a refusal is a warning that names the reason, and a host
//! block is never mistaken for a loaded procedure.
use leveler_client_protocol::{
    NotificationLevel, RuntimeEvent, SessionId, UiSkillDelivery, UiSkillDeliveryEntry,
    UiSkillDeliveryFailure,
};
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

fn state(locale: leveler_tui::Locale) -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 1000,
            locale,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    )
}

fn deliver(state: &mut AppState, delivery: UiSkillDelivery) {
    reduce(
        state,
        Action::Runtime(RuntimeEvent::SkillDelivery { delivery }),
    );
}

fn entry(requested_as: &str, failure: Option<UiSkillDeliveryFailure>) -> UiSkillDeliveryEntry {
    UiSkillDeliveryEntry {
        name: requested_as
            .trim_start_matches(['/', '$'])
            .split(' ')
            .next()
            .unwrap_or(requested_as)
            .to_string(),
        requested_as: requested_as.to_string(),
        scope: None,
        source: None,
        bytes: 0,
        failure,
    }
}

#[test]
fn a_loaded_slash_skill_is_reported_as_information() {
    let mut s = state(leveler_tui::Locale::Zh);
    deliver(
        &mut s,
        UiSkillDelivery {
            entries: vec![entry("/deploy", None)],
            blocked: None,
        },
    );
    let note = s.notification.expect("a selected skill must be reported");
    assert_eq!(note.level, NotificationLevel::Info);
    assert!(note.message.contains("/deploy"), "{}", note.message);
    assert!(note.message.contains(" 已加载"), "{}", note.message);
}

#[test]
fn a_refused_skill_is_a_warning_that_names_the_reason() {
    let mut s = state(leveler_tui::Locale::Zh);
    deliver(
        &mut s,
        UiSkillDelivery {
            entries: vec![entry("$deploy", Some(UiSkillDeliveryFailure::Disabled))],
            blocked: None,
        },
    );
    let note = s.notification.expect("a refusal must not be silent");
    assert_eq!(note.level, NotificationLevel::Warning);
    assert!(note.message.contains("$deploy"), "{}", note.message);
    assert!(note.message.contains("已禁用"), "{}", note.message);
    assert!(!note.message.contains(" 已加载"), "{}", note.message);
}

#[test]
fn one_message_can_report_both_a_loaded_and_a_refused_skill() {
    let mut s = state(leveler_tui::Locale::Zh);
    deliver(
        &mut s,
        UiSkillDelivery {
            entries: vec![
                entry("$alpha", None),
                entry("$beta", Some(UiSkillDeliveryFailure::Untrusted)),
            ],
            blocked: None,
        },
    );
    let note = s.notification.expect("both outcomes must be reported");
    assert_eq!(note.level, NotificationLevel::Warning);
    assert!(note.message.contains("$alpha"), "{}", note.message);
    assert!(note.message.contains(" 已加载"), "{}", note.message);
    assert!(note.message.contains("$beta"), "{}", note.message);
    assert!(note.message.contains("未信任"), "{}", note.message);
}

#[test]
fn a_blocked_turn_never_reads_as_a_loaded_skill() {
    let mut s = state(leveler_tui::Locale::Zh);
    deliver(
        &mut s,
        UiSkillDelivery {
            entries: Vec::new(),
            blocked: Some(UiSkillDeliveryFailure::CapabilityUnavailable),
        },
    );
    let note = s.notification.expect("a host block must be surfaced");
    assert_eq!(note.level, NotificationLevel::Warning);
    assert!(note.message.contains("不可用"), "{}", note.message);
    assert!(!note.message.contains(" 已加载"), "{}", note.message);
}

#[test]
fn the_note_follows_the_client_locale() {
    let mut s = state(leveler_tui::Locale::En);
    deliver(
        &mut s,
        UiSkillDelivery {
            entries: vec![entry(
                "/deploy",
                Some(UiSkillDeliveryFailure::UserInvocationDisabled),
            )],
            blocked: None,
        },
    );
    let note = s.notification.expect("a refusal must not be silent");
    assert_eq!(note.level, NotificationLevel::Warning);
    assert!(note.message.contains("not loaded"), "{}", note.message);
    assert!(
        note.message.contains("manual invocation"),
        "{}",
        note.message
    );
}
