//! Dogfood: the Thinking Level control, through the REAL TUI reducer and
//! renderer.
//!
//! Deterministic and offline by design: the runtime's projection is fed in as
//! the snapshot the real protocol carries, and everything the user sees after
//! that is production code — the slash registry and its autocomplete, the
//! reducer's command handling, the selection overlay, and the composer/status
//! renderer. No provider is contacted, and the capability in each scenario is
//! declared explicitly, so the visible choices are asserted against a fact
//! rather than against a live model.
//!
//! ```text
//! cargo test -p leveler-cli --test thinking_dogfood -- --nocapture
//! ```

use leveler_client_protocol::{ClientCommand, RuntimeEvent, SessionId, UiSessionSnapshot};
use leveler_model::ThinkingLevel;
use leveler_tui::action::{Action, Effect};
use leveler_tui::overlay::Overlay;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
// ratatui re-exports the same crossterm the TUI's `Action::Key` speaks.
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn state() -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            thinking: None,
        },
    )
}

/// A snapshot carrying only what this test drives: the session exists, and the
/// runtime reported a Thinking Level for it.
fn snapshot(
    configured: ThinkingLevel,
    session_override: Option<ThinkingLevel>,
    choices: &[ThinkingLevel],
) -> UiSessionSnapshot {
    let current = session_override.unwrap_or(configured);
    UiSessionSnapshot {
        id: leveler_client_protocol::SessionId::new("s1"),
        goal: "dogfood".into(),
        thinking: Some(leveler_client_protocol::UiThinkingState {
            configured,
            session_override,
            current,
            effective: if choices.contains(&current) {
                current
            } else {
                ThinkingLevel::Auto
            },
            access: leveler_client_protocol::UiThinkingAccess::Adjustable,
            choices: choices.to_vec(),
        }),
        ..blank()
    }
}

/// A session snapshot with nothing in it but an id: this test drives the
/// Thinking Level, and every other field is the schema's own empty value.
fn blank() -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: None,
        task_status: None,
        task_terminal: None,
        goal: "dogfood".into(),
        model: None,
        mode: leveler_client_protocol::PermissionProfile::Assisted,
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

/// A model whose thinking cannot be adjusted at all.
fn unsupported_snapshot() -> UiSessionSnapshot {
    UiSessionSnapshot {
        thinking: Some(leveler_client_protocol::UiThinkingState {
            configured: ThinkingLevel::Auto,
            session_override: None,
            current: ThinkingLevel::Auto,
            effective: ThinkingLevel::Auto,
            access: leveler_client_protocol::UiThinkingAccess::Unsupported,
            choices: Vec::new(),
        }),
        ..blank()
    }
}

fn opened(snapshot: UiSessionSnapshot) -> AppState {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snapshot }),
    );
    s
}

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn typed(s: &mut AppState, text: &str) -> Vec<Effect> {
    let mut effects = Vec::new();
    for ch in text.chars() {
        effects = reduce(s, key(KeyCode::Char(ch)));
    }
    effects
}

/// Everything the user can see right now, as the real renderer paints it.
fn frame(state: &mut AppState, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        for x in 0..w {
            out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    out
}

/// A frame with its padding removed: wide glyphs are painted with a space
/// column after them, so a phrase search must ignore spacing.
fn flat(screen: &str) -> String {
    screen.replace(' ', "")
}

/// The one level word the user is allowed to read, whatever the route calls it.
fn assert_no_native_leak(screen: &str) {
    for native in [
        "xhigh",
        "reasoning_effort",
        "output_config",
        "budget_tokens",
    ] {
        assert!(
            !screen.contains(native),
            "provider parameter `{native}` reached the user:\n{screen}"
        );
    }
}

/// A. `/thi` completes to the command, with the description the registry gives.
#[test]
fn autocomplete_offers_thinking() {
    let mut s = opened(snapshot(ThinkingLevel::Auto, None, &[ThinkingLevel::Auto]));
    typed(&mut s, "/thi");
    let screen = frame(&mut s, 100, 24);
    assert!(
        screen.contains("/thinking"),
        "autocomplete must offer the command:\n{screen}"
    );
    assert_no_native_leak(&screen);
}

/// B + C. `/thinking` opens the picker, and it lists the levels this model can
/// actually distinguish — not the seven canonical names.
#[test]
fn the_selector_shows_only_distinct_choices() {
    // A route with low/medium/high/xhigh: `high` and `max` differ, so both.
    let mut s = opened(snapshot(
        ThinkingLevel::Auto,
        None,
        &[
            ThinkingLevel::Auto,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Max,
        ],
    ));
    typed(&mut s, "/thinking");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(s.overlay, Some(Overlay::ThinkingPicker(_))),
        "the picker opens"
    );
    let screen = frame(&mut s, 100, 28);
    println!(
        "---FRAME---
{screen}
---END---"
    );
    for level in ["auto", "low", "medium", "high", "max"] {
        assert!(screen.contains(level), "missing {level}:\n{screen}");
    }
    assert!(
        !screen.contains("minimal"),
        "a level this model lacks:\n{screen}"
    );
    assert!(
        !screen.contains("off"),
        "a level this model lacks:\n{screen}"
    );
    assert_no_native_leak(&screen);

    // A boolean route: `high` and `max` are the same request, so the selector
    // offers three things and never two names for one effect.
    let mut s = opened(snapshot(
        ThinkingLevel::Auto,
        None,
        &[ThinkingLevel::Auto, ThinkingLevel::Off, ThinkingLevel::Max],
    ));
    typed(&mut s, "/thinking");
    reduce(&mut s, key(KeyCode::Enter));
    let screen = frame(&mut s, 100, 28);
    for level in ["auto", "off", "max"] {
        assert!(screen.contains(level), "missing {level}:\n{screen}");
    }
    assert!(
        !screen.contains("high"),
        "a boolean route has no separate `high`:\n{screen}"
    );
    assert_no_native_leak(&screen);
}

/// E. Choosing `max` sends the canonical level for this session.
#[test]
fn choosing_a_level_sends_the_canonical_intent() {
    let mut s = opened(snapshot(
        ThinkingLevel::Auto,
        None,
        &[ThinkingLevel::Auto, ThinkingLevel::Max],
    ));
    typed(&mut s, "/thinking max");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::SetThinkingLevel {
                level: Some(ThinkingLevel::Max),
                ..
            })]
        ),
        "the command carries the canonical level: {effects:?}"
    );
    // The runtime re-projects and sends the new state; the status line then
    // reads it in the user's words, never the route's.
    let mut updated = snapshot(
        ThinkingLevel::High,
        Some(ThinkingLevel::Max),
        &[ThinkingLevel::Auto, ThinkingLevel::High, ThinkingLevel::Max],
    );
    updated.thinking.as_mut().unwrap().effective = ThinkingLevel::Max;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: updated }),
    );
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("think:max"), "{screen}");
    assert_no_native_leak(&screen);
}

/// F. `auto` is a level: it sets an explicit "no override" for the session.
#[test]
fn auto_is_a_level_and_not_a_reset() {
    let mut s = opened(snapshot(
        ThinkingLevel::High,
        Some(ThinkingLevel::Max),
        &[
            ThinkingLevel::Auto,
            ThinkingLevel::Low,
            ThinkingLevel::High,
            ThinkingLevel::Max,
        ],
    ));
    typed(&mut s, "/thinking auto");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::SetThinkingLevel {
                level: Some(ThinkingLevel::Auto),
                ..
            })]
        ),
        "{effects:?}"
    );
}

/// G. `reset` clears the override, which is a different request from `auto`, and
/// the picker offers it without the user having to remember a command.
#[test]
fn reset_clears_the_override_and_is_offered_in_the_picker() {
    let mut s = opened(snapshot(
        ThinkingLevel::High,
        Some(ThinkingLevel::Max),
        &[
            ThinkingLevel::Auto,
            ThinkingLevel::Low,
            ThinkingLevel::High,
            ThinkingLevel::Max,
        ],
    ));
    typed(&mut s, "/thinking");
    reduce(&mut s, key(KeyCode::Enter));
    let screen = frame(&mut s, 100, 28);
    assert!(
        flat(&screen).contains("恢复默认"),
        "the picker offers a way back to the configured level:\n{screen}"
    );
    // The configured level is named, so the user knows what they return to.
    assert!(screen.contains("high"), "{screen}");
    assert_no_native_leak(&screen);

    // Resetting from the command line sends no level at all.
    reduce(&mut s, key(KeyCode::Esc));
    typed(&mut s, "/thinking reset");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::SetThinkingLevel {
                level: None,
                ..
            })]
        ),
        "{effects:?}"
    );
}

/// H. A model that cannot be asked says so instead of offering a fake list.
#[test]
fn an_unsupported_model_says_so() {
    let mut s = opened(unsupported_snapshot());
    typed(&mut s, "/thinking");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(
        s.overlay.is_none(),
        "no picker for a model that cannot be asked"
    );
    // The runtime's answer reaches the user as a notice; the frame then shows
    // no level at all, because there is none to show.
    let notice = s.notification.as_ref().expect("the user is told why");
    assert!(
        flat(&notice.message).contains("不支持"),
        "the user is told why: {notice:?}"
    );
    let screen = frame(&mut s, 100, 24);
    // And the status line does not claim a level is in effect.
    assert!(
        !screen.contains("think:"),
        "no level is shown for a model that has none:\n{screen}"
    );
    assert_no_native_leak(&screen);
}

/// I. An invalid level is refused with the real vocabulary, in the user's words.
#[test]
fn an_invalid_level_names_the_real_options() {
    let mut s = opened(snapshot(ThinkingLevel::Auto, None, &[ThinkingLevel::Auto]));
    typed(&mut s, "/thinking xhigh");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        effects.is_empty(),
        "nothing is sent for a level that is not one: {effects:?}"
    );
    assert!(
        !s.composer.is_empty() || s.overlay.is_none(),
        "the composer is not swallowed by a bad command"
    );
    let screen = frame(&mut s, 100, 24);
    for level in ThinkingLevel::ALL {
        assert!(screen.contains(level.as_str()), "missing {level}: {screen}");
    }
}

/// The configured level a user returns to is the canonical word, whichever
/// capability resolution produced it.
#[test]
fn the_configured_level_is_shown_in_the_users_words() {
    let mut s = opened(snapshot(
        ThinkingLevel::Medium,
        None,
        &[
            ThinkingLevel::Auto,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Max,
        ],
    ));
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("think:medium"), "{screen}");
    assert_no_native_leak(&screen);
}
