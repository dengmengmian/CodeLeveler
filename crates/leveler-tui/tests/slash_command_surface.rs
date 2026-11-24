//! The complete slash-command surface.
//!
//! One test per registry-wide contract: every listed command is dispatched (not
//! silently unknown), every alias resolves to its canonical command, every
//! IdleOnly command refuses while a turn runs, and `/help <command>` opens the
//! detail page for every listed command. The registry is the source; adding a
//! command without a handler, help entry or busy rule turns this file red.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{RuntimeStatus, SessionId};
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::screen::{BusyPolicy, SLASH_DEFS, slash_primary};
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

fn state() -> AppState {
    let mut state = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".into(),
            version: "0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 128_000,
            locale: leveler_tui::Locale::En,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    state.status = RuntimeStatus::Idle;
    state.runtime_connected = true;
    state
}

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn type_text(state: &mut AppState, text: &str) {
    for ch in text.chars() {
        reduce(state, key(KeyCode::Char(ch)));
    }
}

fn run(state: &mut AppState, text: &str) {
    type_text(state, text);
    reduce(state, key(KeyCode::Enter));
}

fn unknown_message(state: &AppState, token: &str) -> String {
    state
        .t()
        .unknown_command
        .replace("{name}", token.trim_start_matches('/'))
}

#[test]
fn every_registered_command_reaches_a_handler() {
    for def in SLASH_DEFS.iter().filter(|def| def.listed) {
        let mut state = state();
        run(&mut state, def.name);
        if let Some(notification) = &state.notification {
            assert_ne!(
                notification.message,
                unknown_message(&state, def.name),
                "{} fell through to the unknown-command path",
                def.name
            );
        }
    }
}

#[test]
fn every_alias_resolves_to_its_canonical_command() {
    for def in SLASH_DEFS {
        for alias in def.aliases {
            assert_eq!(
                slash_primary(alias),
                Some(def.name),
                "{alias} does not resolve to {}",
                def.name
            );
        }
    }
}

#[test]
fn every_idle_only_command_refuses_while_busy() {
    for def in SLASH_DEFS
        .iter()
        .filter(|def| def.busy == BusyPolicy::IdleOnly)
    {
        let mut state = state();
        state.status = RuntimeStatus::Busy;
        run(&mut state, def.name);
        let primary = def.name.trim_start_matches('/');
        let expected = state.t().idle_only_cmd.replace("{}", primary);
        assert_eq!(
            state
                .notification
                .as_ref()
                .map(|notice| notice.message.as_str()),
            Some(expected.as_str()),
            "{} did not take the busy gate",
            def.name
        );
        assert_eq!(
            state.active_screen,
            leveler_tui::screen::Screen::Conversation,
            "{} opened a surface while busy",
            def.name
        );
        assert!(
            state.overlay.is_none(),
            "{} opened an overlay while busy",
            def.name
        );
        assert_eq!(
            state.composer.text(),
            def.name,
            "{} ate the typed command on a busy refusal",
            def.name
        );
    }
}

#[test]
fn sessions_while_disconnected_do_not_claim_an_empty_inventory() {
    let mut state = state();
    state.runtime_connected = false;
    run(&mut state, "/sessions");
    assert_eq!(state.active_screen, leveler_tui::screen::Screen::Sessions);
    assert!(!state.sessions_loading);
    assert!(
        state.sessions_error.is_some(),
        "a failed read must say so, not show 'No sessions yet'"
    );
}

#[test]
fn every_always_command_is_not_refused_for_busy() {
    for def in SLASH_DEFS
        .iter()
        .filter(|def| def.busy == BusyPolicy::Always)
    {
        let mut state = state();
        state.status = RuntimeStatus::Busy;
        run(&mut state, def.name);
        let primary = def.name.trim_start_matches('/');
        let busy_message = state.t().idle_only_cmd.replace("{}", primary);
        assert_ne!(
            state
                .notification
                .as_ref()
                .map(|notice| notice.message.as_str()),
            Some(busy_message.as_str()),
            "{} was treated as IdleOnly",
            def.name
        );
    }
}

#[test]
fn help_opens_every_command_detail_by_name() {
    for def in SLASH_DEFS.iter().filter(|def| def.listed) {
        let mut state = state();
        run(&mut state, &format!("/help {}", def.name));
        assert_eq!(
            state.active_screen,
            leveler_tui::screen::Screen::Help,
            "/help {} did not open Help",
            def.name
        );
        assert_eq!(
            state.help.detail,
            Some(def.name),
            "/help {} did not open that detail page",
            def.name
        );
        // Esc is one level: detail -> list, then list -> conversation.
        reduce(&mut state, key(KeyCode::Esc));
        assert_eq!(state.help.detail, None);
        assert_eq!(state.active_screen, leveler_tui::screen::Screen::Help);
        reduce(&mut state, key(KeyCode::Esc));
        assert_eq!(
            state.active_screen,
            leveler_tui::screen::Screen::Conversation
        );
    }
}

#[test]
fn reopening_help_starts_from_the_full_registry() {
    let mut state = state();
    run(&mut state, "/help");
    type_text(&mut state, "MCP Server");
    reduce(&mut state, key(KeyCode::Esc));
    assert_eq!(
        state.active_screen,
        leveler_tui::screen::Screen::Conversation
    );
    run(&mut state, "/help");
    assert_eq!(state.help.query, "");
    assert!(state.help.category.is_none());
    assert!(state.help.detail.is_none());
}

#[test]
fn an_unknown_help_argument_is_not_silent() {
    let mut state = state();
    run(&mut state, "/help not-a-command");
    assert_eq!(state.active_screen, leveler_tui::screen::Screen::Help);
    assert!(state.help.detail.is_none());
    assert_eq!(state.help.query, "not-a-command");
    assert!(state.notification.is_some(), "the miss must be explained");
}

/// The reserved-Skill-name list is the contract the runtime's leading-token
/// parser uses. It must equal the client's command surface in both
/// directions: a built-in missing from `RESERVED_SLASH_NAMES` could be
/// captured by a Skill package, and a stale reserved name would make a
/// perfectly ordinary Skill unreachable by its bare slash identity.
#[test]
fn reserved_skill_names_match_the_slash_surface_exactly() {
    use std::collections::BTreeSet;

    let surface: BTreeSet<&str> = SLASH_DEFS
        .iter()
        .flat_map(|def| {
            std::iter::once(def.name.trim_start_matches('/')).chain(
                def.aliases
                    .iter()
                    .map(|alias| alias.trim_start_matches('/')),
            )
        })
        .collect();
    let reserved: BTreeSet<&str> = leveler_skills::RESERVED_SLASH_NAMES
        .iter()
        .copied()
        .collect();
    assert_eq!(
        reserved,
        surface,
        "reserved names and SLASH_DEFS disagree; missing from reserved: {:?}, stale in reserved: {:?}",
        surface.difference(&reserved).collect::<Vec<_>>(),
        reserved.difference(&surface).collect::<Vec<_>>()
    );
}
