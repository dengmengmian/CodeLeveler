//! What the user is told when the runtime event stream connects or reconnects.
//!
//! The transport reports facts (which build answered, whether the process
//! changed). This file locks the presentation verdicts: a version mismatch asks
//! for a restart and stays visible after its notice expires, a restart while a
//! turn was running warns about that turn, and a benign reconnect stays silent.
//! Silence matters as much as wording: a notice that fires while nothing was
//! running teaches users to ignore the ones that matter.

use leveler_client_protocol::{PermissionProfile, RuntimeEvent, SessionId, UiSessionSnapshot};
use leveler_tui::Locale;
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

use leveler_core::BuildIdentity;

fn idle_snapshot(status: &str) -> UiSessionSnapshot {
    UiSessionSnapshot {
        history: None,
        id: SessionId::new("s1"),
        repository: Some("/repo".into()),
        task_status: None,
        task_terminal: None,
        goal: "g".into(),
        model: None,
        mode: PermissionProfile::Assisted,
        branch: None,
        status: status.into(),
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        latest_turn_ordinal: None,
        active_tools: Vec::new(),
        interrupted_tool_calls: Vec::new(),
        active_background_tasks: Vec::new(),
        background_tasks: Vec::new(),
        plan: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        thinking: None,
        work_profile: None,
        collaboration: None,
        execution_task: None,
        children: Vec::new(),
    }
}

fn state(locale: Locale, status: &str) -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "tester".into(),
            version: "1.0.22".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: idle_snapshot(status),
        }),
    );
    // A reconnect notice must be the only thing under test.
    s.notification = None;
    s
}

fn reconnect(s: &mut AppState, build: BuildIdentity, restarted: Option<bool>) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::RuntimeReconnected {
            runtime_build: build,
            runtime_restarted: restarted,
        }),
    );
}

fn observe(s: &mut AppState, build: BuildIdentity) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::RuntimeBuildObserved {
            runtime_build: build,
        }),
    );
}

/// The build this test binary was compiled from, claiming a different source.
fn foreign_build() -> BuildIdentity {
    let mut build = BuildIdentity::current();
    build.revision = "0000000000000000000000000000000000000000".into();
    build.fingerprint = "foreign-fingerprint".into();
    build.dirty = false;
    build
}

#[test]
fn a_runtime_from_a_different_build_asks_for_a_restart() {
    let mut s = state(Locale::Zh, "idle");
    let expected = BuildIdentity::current();
    let build = foreign_build();
    assert!(
        !expected.matches(&build),
        "the fixture must really be a different build"
    );

    reconnect(&mut s, build, Some(false));

    let notice = s
        .notification
        .as_ref()
        .expect("a version mismatch is not silent");
    assert_eq!(notice.message, "CodeLeveler 版本不一致，请重启应用。");
    assert_eq!(
        notice.level,
        leveler_client_protocol::NotificationLevel::Warning
    );
    assert!(
        s.runtime_version_mismatch,
        "the risk is state, not only a transient notice"
    );
}

/// A warning that expires must not take the risk with it: the durable flag is
/// what the issue row projects, and only an observed matching build clears it.
#[test]
fn a_version_mismatch_outlives_its_notice() {
    let mut s = state(Locale::Zh, "idle");
    reconnect(&mut s, foreign_build(), Some(false));
    assert!(s.notification.is_some());

    s.notification = None;
    assert!(
        s.runtime_version_mismatch,
        "dismissing the notice solved nothing"
    );

    observe(&mut s, BuildIdentity::current());
    assert!(!s.runtime_version_mismatch, "a matching build is the fix");
}

/// Unknown is not a change: a runtime that cannot state its identity must
/// neither raise nor clear an anomaly. The last confirmed answer stands.
#[test]
fn an_unknown_build_neither_raises_nor_clears_a_mismatch() {
    let mut s = state(Locale::Zh, "idle");
    observe(&mut s, BuildIdentity::default());
    assert!(!s.runtime_version_mismatch);
    assert!(s.notification.is_none(), "unknown proves nothing");

    reconnect(&mut s, foreign_build(), Some(false));
    assert!(s.runtime_version_mismatch);

    reconnect(&mut s, BuildIdentity::default(), Some(false));
    assert!(
        s.runtime_version_mismatch,
        "an unreadable identity cannot prove the mismatch is gone"
    );
}

/// The first connection to a stale daemon is a real mismatch, and it happens
/// without a reconnect. The observation event carries it.
#[test]
fn the_first_connection_also_reports_a_mismatch() {
    let mut s = state(Locale::Zh, "idle");

    observe(&mut s, BuildIdentity::current());
    assert!(!s.runtime_version_mismatch, "a matching build is silent");
    assert!(s.notification.is_none());

    observe(&mut s, foreign_build());
    assert!(s.runtime_version_mismatch);
    assert_eq!(
        s.notification.as_ref().map(|n| n.message.as_str()),
        Some("CodeLeveler 版本不一致，请重启应用。")
    );
}

/// Work that may have stopped is the more urgent of two risks; the version
/// anomaly still survives on its own state.
#[test]
fn work_at_risk_outranks_the_version_notice_but_not_its_state() {
    let mut s = state(Locale::Zh, "running");
    assert!(s.is_busy());

    reconnect(&mut s, foreign_build(), Some(true));

    assert_eq!(
        s.notification.as_ref().map(|n| n.message.as_str()),
        Some("刚才的任务已中断，部分操作可能已完成，请检查结果。")
    );
    assert!(
        s.runtime_version_mismatch,
        "the version risk is still on the issue row"
    );
}

#[test]
fn a_restart_while_a_turn_runs_warns_about_that_turn() {
    let mut s = state(Locale::Zh, "running");
    assert!(s.is_busy(), "the fixture must be running a turn");

    reconnect(&mut s, BuildIdentity::current(), Some(true));

    let notice = s.notification.as_ref().expect("a lost turn is not silent");
    assert_eq!(
        notice.message,
        "刚才的任务已中断，部分操作可能已完成，请检查结果。"
    );
    assert_eq!(
        notice.level,
        leveler_client_protocol::NotificationLevel::Warning
    );
}

/// The reported defect: startup, nothing running, and the notice still fired.
#[test]
fn a_reconnect_with_nothing_running_says_nothing() {
    let mut s = state(Locale::Zh, "idle");
    assert!(!s.is_busy());

    reconnect(&mut s, BuildIdentity::current(), Some(true));

    assert!(
        s.notification.is_none(),
        "nothing was running, so nothing was lost: {:?}",
        s.notification
    );
}

/// A stream reset on the same process cannot stop work it still owns.
#[test]
fn a_stream_reset_on_the_same_process_is_silent_even_mid_turn() {
    let mut s = state(Locale::Zh, "running");

    reconnect(&mut s, BuildIdentity::current(), Some(false));

    assert!(
        s.notification.is_none(),
        "the process never changed: {:?}",
        s.notification
    );
}

/// An unreadable identity proves nothing, so it must not be read as a new
/// version; a running turn still deserves the warning.
#[test]
fn an_unknown_build_is_not_a_version_change() {
    let mut s = state(Locale::Zh, "running");

    reconnect(&mut s, BuildIdentity::default(), None);

    let notice = s.notification.as_ref().expect("the turn may have stopped");
    assert_eq!(notice.message, "刚才的任务状态无法确认，请检查后再继续。");
    assert!(!s.runtime_version_mismatch);
}

/// A failed resync is a sticky error: the view may be stale, and only a
/// successful snapshot proves it is current again. The raw failure is
/// diagnostic, never user copy.
#[test]
fn a_resync_failure_is_sticky_until_a_snapshot_proves_recovery() {
    let mut s = state(Locale::Zh, "idle");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionResyncFailed {
            error: "engine error: unexpected EOF".into(),
        }),
    );
    let notice = s.notification.as_ref().expect("a stale view is not silent");
    assert_eq!(
        notice.level,
        leveler_client_protocol::NotificationLevel::Error
    );
    assert_eq!(
        notice.message,
        "暂时无法获取最新任务状态，显示的进度可能不准确。"
    );
    assert!(
        !notice.message.contains("engine") && !notice.message.contains("EOF"),
        "the raw failure stays diagnostic: {}",
        notice.message
    );

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: idle_snapshot("idle"),
        }),
    );
    assert!(
        s.notification.is_none(),
        "a canonical snapshot is the recovery proof"
    );
}

#[test]
fn the_notices_speak_the_ui_language() {
    let mut s = state(Locale::En, "running");
    reconnect(&mut s, foreign_build(), Some(false));
    assert_eq!(
        s.notification.as_ref().map(|n| n.message.as_str()),
        Some("CodeLeveler versions don't match. Restart the app.")
    );

    let mut s = state(Locale::En, "running");
    reconnect(&mut s, BuildIdentity::current(), Some(true));
    assert_eq!(
        s.notification.as_ref().map(|n| n.message.as_str()),
        Some(
            "The task in progress was interrupted; some actions may have completed — check the result."
        )
    );

    let mut s = state(Locale::En, "running");
    reconnect(&mut s, BuildIdentity::default(), None);
    assert_eq!(
        s.notification.as_ref().map(|n| n.message.as_str()),
        Some("We couldn't confirm the state of the task in progress. Check before continuing.")
    );

    let mut s = state(Locale::En, "idle");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionResyncFailed {
            error: "boom".into(),
        }),
    );
    assert_eq!(
        s.notification.as_ref().map(|n| n.message.as_str()),
        Some("Couldn't get the latest task state; the progress shown may be inaccurate.")
    );
}
