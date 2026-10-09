//! A startup fact the launcher carried (see `Boot::model_notice`) must be shown.
use leveler_client_protocol::{NotificationLevel, SessionId};
use leveler_tui::state::{AppState, Boot};

fn boot(notice: Option<String>) -> Boot {
    Boot {
        session_id: SessionId::new("s1"),
        user: "u".into(),
        version: "0.1.0".into(),
        show_welcome: false,
        draft_path: None,
        history_path: None,
        context_window: 1000,
        locale: leveler_tui::Locale::Zh,
        untrusted_config: Vec::new(),
        model_notice: notice,
        thinking: None,
    }
}

#[test]
fn a_boot_model_notice_is_shown_as_a_warning() {
    let state = AppState::new(
        leveler_tui::Theme::no_color(),
        boot(Some("默认模型 x 不可用".into())),
    );
    let note = state.notification.expect("the startup fact must be shown");
    assert_eq!(note.level, NotificationLevel::Warning);
    assert!(note.message.contains("默认模型 x 不可用"));
}

#[test]
fn no_boot_notice_leaves_the_status_line_clear() {
    let state = AppState::new(leveler_tui::Theme::no_color(), boot(None));
    assert!(state.notification.is_none());
}
