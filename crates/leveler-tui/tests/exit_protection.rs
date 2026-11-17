//! The exit decision, end to end through the reducer.
//!
//! Unit tests in `leveler-tui::exit` prove the projection; these prove that
//! only the structured impact reaches the dialog, that the safe choice is the
//! default, that "stay" mutates nothing, and that "continue" refuses to leave
//! when the affected work changed after the user looked at it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use leveler_client_protocol::mock::MockRuntimeClient;
use leveler_client_protocol::{
    InteractiveRuntimeClient, NotificationLevel, RuntimeStatus, SessionId, UiSessionSnapshot,
};
use leveler_tui::action::{Action, Effect, EffectCompletion};
use leveler_tui::exit::{ClientExitScope, ExitImpact, ExitTask, ExitTaskKind, project};
use leveler_tui::overlay::Overlay;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

fn state(detachable: bool) -> AppState {
    let mut state = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "tester".to_string(),
            version: "0.0.0".to_string(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale: leveler_tui::Locale::En,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    state.runtime_outlives_client = detachable;
    state
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn task(kind: ExitTaskKind, id: &str, label: &str) -> ExitTask {
    ExitTask {
        kind,
        id: id.to_string(),
        label: label.to_string(),
        elapsed_ms: Some(2_000),
        block: None,
    }
}

fn impact(detachable: bool, tasks: Vec<ExitTask>) -> ExitImpact {
    ExitImpact { detachable, tasks }
}

fn open_dialog(state: &mut AppState, tasks: Vec<ExitTask>) {
    let detachable = state.runtime_outlives_client;
    let effects = reduce(
        state,
        Action::ExitImpactLoaded {
            generation: 0,
            result: Ok(impact(detachable, tasks)),
        },
    );
    assert!(effects.is_empty(), "{effects:?}");
}

fn confirm(state: &mut AppState) -> Vec<Effect> {
    reduce(state, Action::Key(key(KeyCode::Enter)))
}

fn dialog(state: &AppState) -> &leveler_tui::overlay::exit_confirm::ExitConfirmOverlay {
    match state.overlay.as_ref().expect("exit dialog is open") {
        Overlay::ConfirmExit(ov) => ov,
        other => panic!("expected exit dialog, got {other:?}"),
    }
}

#[test]
fn no_affected_work_exits_through_the_verified_cleanup_path() {
    let mut s = state(true);
    let effects = reduce(
        &mut s,
        Action::ExitImpactLoaded {
            generation: 0,
            result: Ok(impact(true, Vec::new())),
        },
    );
    assert_eq!(effects, vec![Effect::Quit]);
    assert!(!s.exit.inspecting);
}

#[test]
fn work_opens_the_dialog_with_stay_already_focused() {
    let mut s = state(true);
    open_dialog(
        &mut s,
        vec![
            task(
                ExitTaskKind::BackgroundTask,
                "bg-1",
                "cargo test --workspace",
            ),
            task(ExitTaskKind::UserShell, "ush-1", "npm run dev"),
        ],
    );
    let ov = dialog(&s);
    let focused = ov
        .model
        .visible_rows()
        .into_iter()
        .find(|(_, _, is_cursor)| *is_cursor)
        .map(|(_, option, _)| option.key.clone());
    assert_eq!(focused.as_deref(), Some("stay"));
    assert!(
        ov.model
            .visible_rows()
            .iter()
            .any(|(_, option, _)| option.key == "background" && option.is_enabled()),
        "a detachable runtime may offer continuation"
    );
}

#[test]
fn esc_stays_and_leaves_every_runtime_and_input_fact_untouched() {
    let mut s = state(true);
    s.composer.replace("unfinished draft");
    s.pending_inputs
        .push(leveler_tui::pending_inputs::PendingInput::queued(
            "queued follow-up",
            SessionId::new("s1"),
        ));
    s.status = RuntimeStatus::Busy;
    open_dialog(
        &mut s,
        vec![task(ExitTaskKind::BackgroundTask, "bg-1", "cargo test")],
    );
    let before = (
        s.composer.text().to_string(),
        s.pending_inputs.len(),
        s.status,
        s.quit_pending,
    );
    let effects = reduce(&mut s, Action::Key(key(KeyCode::Esc)));
    assert!(effects.is_empty());
    assert!(s.overlay.is_none(), "Esc closes the dialog");
    assert!(!s.quit_pending);
    assert_eq!(
        (
            s.composer.text().to_string(),
            s.pending_inputs.len(),
            s.status,
            s.quit_pending,
        ),
        before,
        "stay changes no session, task, or input fact"
    );
}

#[test]
fn stop_is_an_explicit_choice_and_quits() {
    let mut s = state(true);
    open_dialog(
        &mut s,
        vec![task(ExitTaskKind::BackgroundTask, "bg-1", "cargo test")],
    );
    // The default is stay; Down wraps to the first option, "stop".
    let effects = reduce(&mut s, Action::Key(key(KeyCode::Down)));
    assert!(effects.is_empty());
    assert_eq!(confirm(&mut s), vec![Effect::Quit]);
    assert!(s.overlay.is_none());
}

#[test]
fn continue_asks_the_runtime_to_confirm_before_leaving() {
    let mut s = state(true);
    open_dialog(
        &mut s,
        vec![task(ExitTaskKind::BackgroundTask, "bg-1", "cargo test")],
    );
    // Stay -> background is one step up.
    let effects = reduce(&mut s, Action::Key(key(KeyCode::Up)));
    assert!(effects.is_empty());
    let effects = confirm(&mut s);
    assert_eq!(
        effects,
        vec![Effect::ContinueExit { generation: 0 }],
        "continue never exits on the dialog's stale list alone"
    );
    assert!(s.quit_pending);
    assert!(
        s.exit.pending_continue.is_some(),
        "the confirmed set is retained for the re-check"
    );
}

#[test]
fn an_embedded_runtime_disables_continuation_instead_of_faking_it() {
    let mut s = state(false);
    open_dialog(
        &mut s,
        vec![task(ExitTaskKind::BackgroundTask, "bg-1", "cargo test")],
    );
    let ov = dialog(&s);
    let background = ov
        .model
        .visible_rows()
        .into_iter()
        .find(|(_, option, _)| option.key == "background")
        .map(|(_, option, _)| option.clone())
        .expect("background row");
    assert!(!background.is_enabled());
    assert!(
        background
            .disabled_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("ends with the TUI")),
        "{:?}",
        background.disabled_reason
    );
    // Navigation never lands on a disabled row, and the dialog's default is
    // the safe one. Enter therefore keeps the session rather than acting on
    // the unavailable choice.
    let effects = confirm(&mut s);
    assert!(
        effects.is_empty(),
        "no exit may start from the safe default"
    );
    assert!(s.overlay.is_none(), "stay closes the dialog");
    assert!(s.running);
}

#[test]
fn a_turn_already_cancelling_blocks_continuation_even_on_a_daemon() {
    let mut snap = snapshot();
    snap.status = "running".into();
    let projected = project(&snap, ClientExitScope::DetachesFromRuntime, true);
    assert!(!projected.can_continue());
    let mut s = state(true);
    let effects = reduce(
        &mut s,
        Action::ExitImpactLoaded {
            generation: 0,
            result: Ok(projected),
        },
    );
    assert!(effects.is_empty());
    let ov = dialog(&s);
    let background = ov
        .model
        .visible_rows()
        .into_iter()
        .find(|(_, option, _)| option.key == "background")
        .map(|(_, option, _)| option.clone())
        .unwrap();
    assert!(!background.is_enabled());
    assert!(
        background
            .disabled_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("already stopping")),
        "{:?}",
        background.disabled_reason
    );
}

#[test]
fn new_work_during_continue_reopens_the_dialog_instead_of_leaving_it_running() {
    let mut s = state(true);
    open_dialog(
        &mut s,
        vec![task(ExitTaskKind::BackgroundTask, "bg-1", "cargo test")],
    );
    reduce(&mut s, Action::Key(key(KeyCode::Up)));
    assert_eq!(
        confirm(&mut s),
        vec![Effect::ContinueExit { generation: 0 }]
    );

    let current = impact(
        true,
        vec![
            task(ExitTaskKind::BackgroundTask, "bg-1", "cargo test"),
            task(ExitTaskKind::UserShell, "ush-new", "make watch"),
        ],
    );
    let effects = reduce(
        &mut s,
        Action::ExitRechecked {
            generation: 0,
            result: Ok(current),
        },
    );
    assert!(effects.is_empty());
    assert!(s.running, "the client must not leave yet");
    assert!(!s.quit_pending);
    assert!(s.overlay.is_some(), "the user sees the changed set");
    assert_eq!(
        s.notification.as_ref().map(|n| n.level),
        Some(NotificationLevel::Warning)
    );
    assert_eq!(dialog(&s).impact.tasks.len(), 2);
}

#[test]
fn continue_recheck_with_a_smaller_set_exits_and_reports_the_real_count() {
    let mut s = state(true);
    open_dialog(
        &mut s,
        vec![
            task(ExitTaskKind::BackgroundTask, "bg-1", "cargo test"),
            task(ExitTaskKind::BackgroundTask, "bg-2", "npm run dev"),
        ],
    );
    reduce(&mut s, Action::Key(key(KeyCode::Up)));
    s.quit_pending = true;
    s.exit.pending_continue = Some(impact(
        true,
        vec![
            task(ExitTaskKind::BackgroundTask, "bg-1", "cargo test"),
            task(ExitTaskKind::BackgroundTask, "bg-2", "npm run dev"),
        ],
    ));
    let effects = reduce(
        &mut s,
        Action::ExitRechecked {
            generation: 0,
            result: Ok(impact(
                true,
                vec![task(ExitTaskKind::BackgroundTask, "bg-1", "cargo test")],
            )),
        },
    );
    assert!(effects.is_empty());
    assert!(!s.running);
    assert_eq!(s.exit.continued_count, 1);
}

#[test]
fn a_failed_recheck_keeps_the_client_open_with_a_real_error() {
    let mut s = state(true);
    s.exit.pending_continue = Some(impact(true, Vec::new()));
    s.quit_pending = true;
    let effects = reduce(
        &mut s,
        Action::ExitRechecked {
            generation: 0,
            result: Err("runtime disconnected".into()),
        },
    );
    assert!(effects.is_empty());
    assert!(s.running);
    assert!(!s.quit_pending);
    assert_eq!(
        s.notification.as_ref().map(|n| n.level),
        Some(NotificationLevel::Error)
    );
}

#[test]
fn a_failed_impact_read_never_claims_that_work_was_stopped() {
    let mut s = state(true);
    let effects = reduce(
        &mut s,
        Action::ExitImpactLoaded {
            generation: 0,
            result: Err("snapshot unavailable".into()),
        },
    );
    assert!(effects.is_empty());
    assert!(s.running);
    assert!(!s.quit_pending);
    assert!(s.overlay.is_none());
    assert_eq!(
        s.notification.as_ref().map(|n| n.level),
        Some(NotificationLevel::Error)
    );
}

#[test]
fn repeated_exit_keys_while_inspecting_do_not_start_a_second_read() {
    let mut s = state(true);
    s.composer.replace("/exit");
    let first = reduce(&mut s, Action::Key(key(KeyCode::Enter)));
    assert_eq!(first, vec![Effect::InspectExit { generation: 1 }]);
    s.composer.replace("/exit");
    let second = reduce(&mut s, Action::Key(key(KeyCode::Enter)));
    assert!(second.is_empty(), "one read at a time: {second:?}");
}

#[test]
fn a_late_impact_answer_for_a_dismissed_dialog_is_ignored() {
    let mut s = state(true);
    s.exit.generation = 2;
    let effects = reduce(
        &mut s,
        Action::ExitImpactLoaded {
            generation: 1,
            result: Ok(impact(
                true,
                vec![task(ExitTaskKind::BackgroundTask, "bg-old", "old")],
            )),
        },
    );
    assert!(effects.is_empty());
    assert!(s.overlay.is_none(), "no stale dialog may reopen");
}

#[test]
fn slash_exit_and_its_quit_alias_both_start_the_same_read() {
    for command in ["/exit", "/quit"] {
        let mut s = state(true);
        s.composer.replace(command);
        let effects = reduce(&mut s, Action::Key(key(KeyCode::Enter)));
        assert_eq!(
            effects,
            vec![Effect::InspectExit { generation: 1 }],
            "{command}"
        );
        assert!(s.exit.inspecting, "{command}");
    }
}

#[test]
fn a_late_impact_answer_cannot_reopen_a_dialog_after_terminal_loss() {
    let mut s = state(true);
    reduce(&mut s, Action::TerminalLost);
    let effects = reduce(
        &mut s,
        Action::ExitImpactLoaded {
            generation: 0,
            result: Ok(impact(
                true,
                vec![task(ExitTaskKind::BackgroundTask, "bg-1", "cargo test")],
            )),
        },
    );
    assert!(effects.is_empty());
    assert!(
        s.overlay.is_none(),
        "there is no terminal left to show it on"
    );
}

#[test]
fn terminal_loss_runs_one_bounded_stop_and_exits_even_on_failure() {
    let mut s = state(true);
    let effects = reduce(&mut s, Action::TerminalLost);
    assert_eq!(effects, vec![Effect::QuitSignal]);
    assert!(s.terminal_lost);
    let again = reduce(&mut s, Action::TerminalLost);
    assert!(again.is_empty(), "one stop attempt, not a loop");

    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::TerminalLostCleanupFinished(Err(
            "timed out waiting for the runtime to confirm cleanup".into(),
        ))),
    );
    assert!(!s.running, "terminal loss must not strand the process");
    assert!(
        s.exit.stop_unconfirmed.is_some(),
        "the unconfirmed stop is reported, never claimed as success"
    );
}

#[test]
fn a_long_task_list_is_bounded_but_the_count_stays_true() {
    let mut s = state(true);
    s.size = (110, 42);
    let tasks: Vec<_> = (1..=20)
        .map(|index| {
            task(
                ExitTaskKind::BackgroundTask,
                &format!("bg-{index}"),
                &format!("long-task-{index}"),
            )
        })
        .collect();
    open_dialog(&mut s, tasks);
    let (_, lines, _) =
        leveler_tui::overlay::content_lines(s.overlay.as_ref().unwrap(), &s.theme, 90, s.locale);
    let text: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
    assert!(
        text.iter().any(|line| line.contains("20 tasks")),
        "{text:#?}"
    );
    assert!(
        text.iter().any(|line| line.contains("12 more not shown")),
        "{text:#?}"
    );
    assert!(
        !text.iter().any(|line| line.contains("long-task-9")),
        "only the bounded head is listed"
    );
}

#[test]
fn a_44x20_terminal_shows_the_title_work_and_safe_choice_together() {
    let mut s = state(true);
    s.size = (44, 20);
    open_dialog(
        &mut s,
        vec![
            task(ExitTaskKind::AgentTurn, "agent-turn", ""),
            task(ExitTaskKind::BackgroundTask, "bg-1", "cargo test"),
        ],
    );
    let text = rendered(&mut s, 44, 20);
    assert!(text.contains("Work is running"), "{text}");
    assert!(text.contains("2 tasks will be affected"), "{text}");
    assert!(text.contains("cargo test"), "{text}");
    assert!(text.contains("Stay"), "{text}");
}

fn rendered(state: &mut AppState, width: u16, height: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|frame| leveler_tui::render::render(frame, state))
        .unwrap();
    let buffer = term.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        let mut x = 0u16;
        while x < width {
            let symbol = buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
            out.push_str(symbol);
            x += unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
        }
        out.push('\n');
    }
    out
}

fn snapshot() -> UiSessionSnapshot {
    let mock = MockRuntimeClient::new(SessionId::new("s1"));
    futures::executor::block_on(mock.snapshot(&SessionId::new("s1"))).unwrap()
}
