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
use leveler_model::{
    ReasoningConfig, ReasoningEffort, ReasoningStyle, ThinkingCapabilities, ThinkingLevel,
};
use leveler_tui::action::{Action, Effect, EffectCompletion};
use leveler_tui::overlay::Overlay;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
// ratatui re-exports the same crossterm the TUI's `Action::Key` speaks.
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn state() -> AppState {
    state_for(SessionId::new("s1"))
}

fn state_for(session_id: SessionId) -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id,
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

/// The same snapshot with a real model label, so the status chip's model half
/// is asserted alongside the level.
fn with_model(mut session: UiSessionSnapshot) -> UiSessionSnapshot {
    session.model = Some(leveler_client_protocol::ModelRef::parse("test/model-x").unwrap());
    session
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

/// One process-wide empty `LEVELER_HOME`, so a test that boots a real runtime
/// cannot read the developer's own config.
fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    // SAFETY: test-only process isolation; set once before any runtime boots.
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

/// A scripted plain-text answer for the local fake provider.
fn local_text(content: &str) -> leveler_test_support::MockResponse {
    let frame = serde_json::json!({
        "choices": [{"delta": {"content": content}, "finish_reason": "stop"}]
    })
    .to_string();
    leveler_test_support::MockResponse::Sse {
        body: format!("data: {frame}\n\ndata: [DONE]\n\n"),
    }
}

/// Wait for the runtime's own re-projected snapshot, never a fabricated one.
async fn wait_for_session_updated(
    rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
) -> UiSessionSnapshot {
    use tokio::sync::broadcast::error::RecvError;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::SessionUpdated { session })) => return session,
            Ok(Ok(_)) => continue,
            Ok(Err(RecvError::Lagged(_))) => continue,
            other => panic!("the runtime never confirmed the level: {other:?}"),
        }
    }
}

async fn wait_for_turn_end(rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
    use tokio::sync::broadcast::error::RecvError;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(event)) => match event {
                RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnCompletedWithWarnings { .. }
                | RuntimeEvent::TurnAnswered
                | RuntimeEvent::TurnTruncated { .. }
                | RuntimeEvent::TurnIncomplete { .. }
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnCancelled => return,
                _ => continue,
            },
            Ok(Err(RecvError::Lagged(_))) => continue,
            other => panic!("the turn never settled: {other:?}"),
        }
    }
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
        "thinking_flag",
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

/// The visible choices a declared capability really produces, straight from the
/// domain projection — the picker must not invent a list of its own.
fn visible_choices(style: ReasoningStyle, supported: &[ReasoningEffort]) -> Vec<ThinkingLevel> {
    ThinkingCapabilities::of(
        true,
        &ReasoningConfig {
            style,
            default_effort: supported.last().copied(),
            supported_efforts: supported.to_vec(),
        },
    )
    .levels()
    .to_vec()
}

/// B + C. `/thinking` opens the picker, and it lists exactly the levels this
/// model's declared capability can distinguish — one entry per distinct effect,
/// never two names for one request, and never the seven canonical names.
///
/// The four capability shapes the product promises to keep distinct, weakest
/// route first. Each row is `(style, declared levels, the words the selector
/// must show)`; the expected words are asserted against the real projection, so
/// a fake level cannot reappear without failing here.
#[test]
fn the_selector_shows_only_distinct_choices() {
    const CANONICAL: [&str; 7] = ["auto", "off", "minimal", "low", "medium", "high", "max"];
    let shapes: [(ReasoningStyle, &[ReasoningEffort], &[&str]); 4] = [
        // Boolean: `high` and `max` are the same request, and the route can be
        // told to disable thinking, so three entries and no separate `high`.
        (
            ReasoningStyle::ThinkingFlag,
            &[ReasoningEffort::High],
            &["auto", "off", "max"],
        ),
        // Low/high: `max` is the strongest this route has, so `high` folds
        // into it — still settable by name, never a second row.
        (
            ReasoningStyle::OpenAiEffort,
            &[ReasoningEffort::Low, ReasoningEffort::High],
            &["auto", "low", "max"],
        ),
        // Low/medium/high: same fold at a wider top, and no `off` on a route
        // that has no word for disabling thinking.
        (
            ReasoningStyle::OpenAiEffort,
            &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
            &["auto", "low", "medium", "max"],
        ),
        // Low/medium/high/xhigh: `high` and `max` really differ, so both stay.
        (
            ReasoningStyle::OpenAiEffort,
            &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
            ],
            &["auto", "low", "medium", "high", "max"],
        ),
    ];

    for (style, supported, expected) in shapes {
        let choices = visible_choices(style, supported);
        assert_eq!(
            choices.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            expected,
            "the domain projection decides the visible words for {style:?}/{supported:?}"
        );
        assert!(
            !choices.is_empty(),
            "an adjustable route must offer something"
        );

        let mut s = opened(snapshot(ThinkingLevel::Auto, None, &choices));
        typed(&mut s, "/thinking");
        reduce(&mut s, key(KeyCode::Enter));
        assert!(
            matches!(s.overlay, Some(Overlay::ThinkingPicker(_))),
            "the picker opens for {style:?}/{supported:?}"
        );
        let screen = frame(&mut s, 100, 30);
        let text = flat(&screen);
        for word in expected {
            assert!(
                text.contains(*word),
                "missing `{word}` for {style:?}/{supported:?}:\n{screen}"
            );
        }
        // Every canonical word that this route does not distinguish must be
        // absent: no fake level, and no second name for one effect.
        for word in CANONICAL {
            if !expected.contains(&word) {
                assert!(
                    !text.contains(word),
                    "`{word}` must not appear for {style:?}/{supported:?}:\n{screen}"
                );
            }
        }
        assert_no_native_leak(&screen);
    }
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
    assert!(screen.contains("(max)"), "{screen}");
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
    assert!(screen.contains("(medium)"), "{screen}");
    assert_no_native_leak(&screen);
}

#[test]
fn status_tracks_confirmed_levels_model_switch_resume_and_new_session() {
    let choices = [ThinkingLevel::Auto, ThinkingLevel::High, ThinkingLevel::Max];
    let mut s = opened(snapshot(ThinkingLevel::High, None, &choices));
    for session_override in [
        Some(ThinkingLevel::High),
        Some(ThinkingLevel::Max),
        Some(ThinkingLevel::Auto),
        None,
    ] {
        let mut updated = snapshot(ThinkingLevel::High, session_override, &choices);
        updated.model = Some(leveler_client_protocol::ModelRef::parse("test/model-a").unwrap());
        let level = updated.thinking.as_ref().unwrap().effective;
        reduce(
            &mut s,
            Action::Runtime(RuntimeEvent::SessionUpdated { session: updated }),
        );
        let screen = frame(&mut s, 100, 24);
        assert!(screen.contains(&format!("model-a ({level})")), "{screen}");
        assert!(!screen.contains("think:"), "{screen}");
        assert_no_native_leak(&screen);
    }
    let mut resumed = snapshot(ThinkingLevel::High, Some(ThinkingLevel::Max), &choices);
    resumed.model = Some(leveler_client_protocol::ModelRef::parse("test/model-b").unwrap());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: resumed }),
    );
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("model-b (max)"), "{screen}");
    let mut fresh = snapshot(ThinkingLevel::High, None, &choices);
    fresh.id = SessionId::new("s2");
    fresh.model = Some(leveler_client_protocol::ModelRef::parse("test/model-c").unwrap());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: fresh }),
    );
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("model-c (high)"), "{screen}");
    assert_no_native_leak(&screen);
}

#[test]
fn fixed_and_unsupported_models_do_not_show_a_parenthesized_level() {
    for access in [
        leveler_client_protocol::UiThinkingAccess::Fixed,
        leveler_client_protocol::UiThinkingAccess::Unsupported,
    ] {
        let mut snap = unsupported_snapshot();
        snap.model = Some(leveler_client_protocol::ModelRef::parse("test/model").unwrap());
        let thinking = snap.thinking.as_mut().unwrap();
        thinking.access = access;
        thinking.current = ThinkingLevel::High;
        thinking.session_override = Some(ThinkingLevel::High);
        let mut s = opened(snap);
        let screen = frame(&mut s, 100, 24);
        assert!(!screen.contains("model ("), "{screen}");
        assert_no_native_leak(&screen);
    }
}

/// A route that can distinguish every level: `high` and `max` are two
/// different requests, so both are offered and both are settable by name.
fn every_level() -> [ThinkingLevel; 5] {
    [
        ThinkingLevel::Auto,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Max,
    ]
}

/// J. A fresh session with no override is High, and the status line says so in
/// CodeLeveler's word — never the legacy `think:` chip, never `auto`, never a
/// provider parameter.
#[test]
fn a_fresh_session_defaults_to_high_in_the_users_word() {
    let snap = with_model(snapshot(ThinkingLevel::High, None, &every_level()));
    let thinking = snap.thinking.as_ref().unwrap();
    assert_eq!(thinking.configured, ThinkingLevel::High);
    assert_eq!(thinking.session_override, None);
    assert_eq!(thinking.current, ThinkingLevel::High);
    assert_eq!(thinking.effective, ThinkingLevel::High);

    let mut s = opened(snap);
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("model-x (high)"), "{screen}");
    assert!(
        !screen.contains("think:"),
        "the retired `think:` chip is back:\n{screen}"
    );
    assert!(
        !screen.contains("(auto)"),
        "High must not be painted as auto:\n{screen}"
    );
    assert_no_native_leak(&screen);
}

/// K. `auto` is a state of its own: the status line reads `(auto)` and never
/// `(high)`, even though the configured default is High.
#[test]
fn auto_is_painted_as_auto_and_never_as_high() {
    let choices = every_level();
    let mut s = opened(with_model(snapshot(ThinkingLevel::High, None, &choices)));
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
        "auto is sent as the level auto: {effects:?}"
    );

    let confirmed = with_model(snapshot(
        ThinkingLevel::High,
        Some(ThinkingLevel::Auto),
        &choices,
    ));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: confirmed }),
    );
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("model-x (auto)"), "{screen}");
    assert!(
        !screen.contains("(high)"),
        "auto must not be painted as the configured high:\n{screen}"
    );
    assert_no_native_leak(&screen);
}

/// L. `reset` clears the override and returns to the configured level; it is a
/// different request from `auto` and ends on a different value.
#[test]
fn reset_returns_to_the_configured_level_and_differs_from_auto() {
    let choices = every_level();
    let mut s = opened(with_model(snapshot(
        ThinkingLevel::High,
        Some(ThinkingLevel::Auto),
        &choices,
    )));
    let screen = frame(&mut s, 100, 24);
    assert!(
        screen.contains("model-x (auto)"),
        "the session override is what is in effect:\n{screen}"
    );

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
        "reset sends no level: {effects:?}"
    );

    let confirmed = with_model(snapshot(ThinkingLevel::High, None, &choices));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: confirmed }),
    );
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("model-x (high)"), "{screen}");
    assert!(!screen.contains("(auto)"), "reset is not auto:\n{screen}");
    assert_no_native_leak(&screen);
}

/// M. Reset returns to *this model's* configured level. With the global default
/// High and the model configured Low, the session returns to Low — a hardcoded
/// `reset = high` would fail here.
#[test]
fn reset_returns_to_the_per_model_level_not_the_global_one() {
    let choices = every_level();
    let mut s = opened(with_model(snapshot(ThinkingLevel::Low, None, &choices)));
    let screen = frame(&mut s, 100, 24);
    assert!(
        screen.contains("model-x (low)"),
        "the model's own configured level:\n{screen}"
    );

    typed(&mut s, "/thinking max");
    reduce(&mut s, key(KeyCode::Enter));
    let overridden = with_model(snapshot(
        ThinkingLevel::Low,
        Some(ThinkingLevel::Max),
        &choices,
    ));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated {
            session: overridden,
        }),
    );
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("model-x (max)"), "{screen}");

    typed(&mut s, "/thinking reset");
    reduce(&mut s, key(KeyCode::Enter));
    let reset = with_model(snapshot(ThinkingLevel::Low, None, &choices));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: reset }),
    );
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("model-x (low)"), "{screen}");
    assert!(
        !screen.contains("(high)"),
        "reset must land on the configured Low, not the global High:\n{screen}"
    );
    assert_no_native_leak(&screen);
}

/// N. The picker shows the session's current level and the configured default as
/// two separate facts. With an override they must not be collapsed into one.
#[test]
fn the_selector_separates_current_from_configured() {
    let choices = every_level();
    let mut s = opened(with_model(snapshot(
        ThinkingLevel::High,
        Some(ThinkingLevel::Auto),
        &choices,
    )));
    typed(&mut s, "/thinking");
    reduce(&mut s, key(KeyCode::Enter));
    assert!(matches!(s.overlay, Some(Overlay::ThinkingPicker(_))));
    let screen = frame(&mut s, 100, 30);
    let title = flat(&screen);
    assert!(title.contains("当前：auto"), "{screen}");
    assert!(title.contains("默认：high"), "{screen}");
    // The choices are the model's real ones, not the seven canonical names.
    for level in ["auto", "low", "medium", "high", "max"] {
        assert!(title.contains(level), "missing {level}:\n{screen}");
    }
    assert!(!title.contains("minimal"), "{screen}");
    assert_no_native_leak(&screen);

    // A fresh session has no override, so both lines read the same level.
    let mut fresh = opened(with_model(snapshot(ThinkingLevel::High, None, &choices)));
    typed(&mut fresh, "/thinking");
    reduce(&mut fresh, key(KeyCode::Enter));
    let screen = frame(&mut fresh, 100, 30);
    let title = flat(&screen);
    assert!(title.contains("当前：high"), "{screen}");
    assert!(title.contains("默认：high"), "{screen}");
    assert_no_native_leak(&screen);
}

/// O. `max` is the user's word even where the route's own word is `xhigh`: the
/// canonical level is Max, the provider projection stays private, and `high`
/// remains a settable name because the route really does distinguish it.
#[test]
fn max_is_painted_as_max_where_the_route_says_xhigh() {
    let choices = every_level();
    let mut s = opened(with_model(snapshot(
        ThinkingLevel::Auto,
        Some(ThinkingLevel::Max),
        &choices,
    )));
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("model-x (max)"), "{screen}");
    assert!(!screen.contains("xhigh"), "{screen}");

    typed(&mut s, "/thinking high");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::SetThinkingLevel {
                level: Some(ThinkingLevel::High),
                ..
            })]
        ),
        "the distinct `high` under `max` is still settable: {effects:?}"
    );
    assert_no_native_leak(&frame(&mut s, 100, 24));
}

/// P. An unconfirmed Thinking command must not paint the level as if it were in
/// effect: the status line keeps the last confirmed level until the runtime
/// answers, and a refusal leaves it there.
#[test]
fn an_unconfirmed_thinking_command_does_not_paint_the_new_level() {
    let choices = every_level();
    let mut s = opened(with_model(snapshot(ThinkingLevel::High, None, &choices)));
    typed(&mut s, "/thinking max");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    let [Effect::Send(command @ ClientCommand::SetThinkingLevel { .. })] = effects.as_slice()
    else {
        panic!("{effects:?}");
    };
    let command = command.clone();
    let screen = frame(&mut s, 100, 24);
    assert!(
        screen.contains("model-x (high)"),
        "the unconfirmed level is not painted:\n{screen}"
    );
    assert!(
        !screen.contains("(max)"),
        "no optimistic max before the runtime confirms:\n{screen}"
    );
    assert!(
        flat(&screen).contains("正在请求"),
        "the user is told the request is in flight:\n{screen}"
    );

    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::CommandRejected {
            command,
            message: "runtime config persistence failed".into(),
            snapshot: None,
        }),
    );
    let screen = frame(&mut s, 100, 24);
    assert!(
        screen.contains("model-x (high)"),
        "a refused command leaves the confirmed level:\n{screen}"
    );
    assert!(
        !screen.contains("(max)"),
        "a refused max is not painted as success:\n{screen}"
    );
    assert_no_native_leak(&screen);
}

/// Q. The whole chain, once, with nothing hand-injected: `/thinking max` typed
/// into the real composer, carried as a `ClientCommand` into the real runtime,
/// persisted on the session, projected by the real execution policy through the
/// real provider encoder (`xhigh`), and painted back from the runtime's own
/// re-projected snapshot as the user's word `(max)`.
///
/// Offline and deterministic: the provider is a local fake, the capability is
/// declared in the fixture, and no step fabricates the snapshot the TUI sees.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thinking_max_runs_the_whole_chain_to_the_wire_and_back_to_the_screen() {
    use leveler_app::{Application, InProcessRuntimeClient};
    use leveler_client_protocol::InteractiveRuntimeClient;
    use leveler_execution::PermissionProfile;
    use leveler_model::ModelRef;
    use leveler_project::Layout;
    use leveler_test_support::MockServer;
    use std::sync::Arc;

    isolate_global_config();
    let server = MockServer::start(vec![local_text("done")]).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/models")).unwrap();
    std::fs::write(
        tmp.path().join("configs/providers/mock.yaml"),
        format!(
            "id: mock\nprotocol: openai_chat\nbase_url: {}\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("configs/models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities:
  streaming: true
  tool_calling: true
  parallel_tool_calls: false
  structured_output: true
  reasoning: true
  vision: false
reasoning:
  style: open_ai_effort
  supported_efforts: [low, medium, high, x_high]
  default_effort: medium
thinking: high
limits:
  context_window: 131072
  reliable_context: 65536
  max_output_tokens: 2048
  max_tool_schema_bytes: 16384
  max_parallel_tool_calls: 1
compatibility:
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#,
    )
    .unwrap();
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::new("mock", "m");
    let session = app
        .create_session(&model, "thinking-dogfood")
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app,
        model,
        PermissionProfile::Assisted,
        false,
    ));

    // The only snapshot the TUI ever sees is the runtime's own: configured
    // High, no override, so the status line starts on the user's word `(high)`.
    let opened = client.snapshot(&session).await.unwrap();
    {
        let thinking = opened.thinking.as_ref().unwrap();
        assert_eq!(thinking.configured, ThinkingLevel::High);
        assert_eq!(thinking.session_override, None);
        assert_eq!(thinking.current, ThinkingLevel::High);
    }
    let mut s = state_for(session.clone());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: opened }),
    );
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("(high)"), "{screen}");
    assert_no_native_leak(&screen);

    // Type the command exactly as the user does; the effect is the real command.
    let mut rx = client.subscribe();
    typed(&mut s, "/thinking max");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    let command = match effects.as_slice() {
        [Effect::Send(command @ ClientCommand::SetThinkingLevel { .. })] => command.clone(),
        other => panic!("the composer must emit one command: {other:?}"),
    };
    // Runtime settings require the version observed by the production TUI
    // dispatcher. Raw/unversioned delivery remains refused and changes nothing.
    let mut envelope = leveler_client_protocol::CommandEnvelope {
        command_id: leveler_client_protocol::CommandId::generate(),
        session_id: session.clone(),
        expected_version: None,
        issued_at: leveler_core::now().to_rfc3339(),
        command,
    };
    let refused = client.deliver(envelope.clone()).await.unwrap_err();
    assert!(
        matches!(refused, leveler_client_protocol::ClientError::Runtime(ref message)
            if message.contains("snapshot version required")),
        "{refused:?}"
    );
    let unchanged = client.snapshot(&session).await.unwrap();
    assert_eq!(
        Some(unchanged.last_sequence.unwrap_or(0)),
        s.snapshot_version
    );
    assert_eq!(unchanged.thinking.as_ref().unwrap().session_override, None);

    envelope.command_id = leveler_client_protocol::CommandId::generate();
    envelope.expected_version = Some(s.snapshot_version.expect("observed runtime version"));
    client.deliver(envelope).await.unwrap();

    // The runtime re-projects and answers with its own snapshot; nothing here
    // fabricates one.
    let confirmed = wait_for_session_updated(&mut rx).await;
    {
        let thinking = confirmed.thinking.as_ref().unwrap();
        assert_eq!(thinking.session_override, Some(ThinkingLevel::Max));
        assert_eq!(thinking.current, ThinkingLevel::Max);
    }
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: confirmed }),
    );
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("(max)"), "{screen}");
    assert!(
        !screen.contains("(high)"),
        "the session override is what is in effect:\n{screen}"
    );
    assert_no_native_leak(&screen);

    // A real turn reaches the fake provider; the canonical `max` is this
    // route's native `xhigh` on the wire.
    let mut turn_rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "hello".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for_turn_end(&mut turn_rx).await;
    let bodies = server.request_bodies().await;
    let body: serde_json::Value =
        serde_json::from_str(bodies.last().expect("the fake provider was never called")).unwrap();
    assert_eq!(
        body.get("reasoning_effort")
            .and_then(|value| value.as_str()),
        Some("xhigh"),
        "the canonical Max must reach this route as xhigh: {body}"
    );

    // ...and after the turn the screen still says the user's word, never the
    // provider's.
    let after = client.snapshot(&session).await.unwrap();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionUpdated { session: after }),
    );
    let screen = frame(&mut s, 100, 24);
    assert!(screen.contains("(max)"), "{screen}");
    assert_no_native_leak(&screen);
}
