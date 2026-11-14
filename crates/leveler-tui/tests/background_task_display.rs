//! The background-task display contract.
//!
//! A start is recorded once — the conversation's command row plus the strip's
//! running count. There is no second notice for it and no persistent reminder
//! for a failure. Everything a task really did (label, exit code, duration,
//! output) stays queryable from the background page, and the running count is
//! derived only from live runtime state.

use leveler_client_protocol::{RuntimeEvent, SessionId, ToolCallId, UiActiveBackgroundTask};
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

fn state(locale: leveler_tui::Locale) -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s-bg"),
            user: "tester".to_string(),
            version: "0.0.0-test".to_string(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    )
}

fn zh() -> AppState {
    state(leveler_tui::Locale::Zh)
}

/// Render the whole screen to text, exactly the frame the user reads.
fn rendered(state: &mut AppState, w: u16, h: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        out.push('\n');
    }
    out
}

/// A model `run_command` call that detaches. The tool result names the task id,
/// which is what links the later exit back to its row.
fn tool_started(state: &mut AppState, call: &str, program: &str, args: &[&str]) {
    let arguments = serde_json::json!({
        "program": program,
        "args": args,
        "background": true,
    });
    reduce(
        state,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(call),
            name: "run_command".into(),
            arguments: arguments.to_string(),
            parallel: false,
            model_step: None,
            answer_effect: None,
        }),
    );
}

fn tool_completed(state: &mut AppState, call: &str, task: &str) {
    reduce(
        state,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new(call),
            ok: true,
            preview: format!("background task started\ntask_id: {task}\nstatus: running"),
            duration_ms: 3,
            applied_diff: None,
            exit_code: Some(0),
            stop: None,
        }),
    );
}

fn started(state: &mut AppState, task: &str, program: &str, args: &[&str]) {
    reduce(
        state,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: task.into(),
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            pid: Some(4242),
        }),
    );
}

fn exited(state: &mut AppState, task: &str, code: i32, ok: bool) {
    reduce(
        state,
        Action::Runtime(RuntimeEvent::BackgroundTaskExited {
            task_id: task.into(),
            exit_code: Some(code),
            duration_ms: 1200,
            ok,
            stopped: false,
            output: format!("[stdout] log for {task}\n"),
        }),
    );
}

fn stopped(state: &mut AppState, task: &str) {
    reduce(
        state,
        Action::Runtime(RuntimeEvent::BackgroundTaskExited {
            task_id: task.into(),
            exit_code: Some(143),
            duration_ms: 900,
            ok: false,
            stopped: true,
            output: String::new(),
        }),
    );
}

fn active(task: &str, program: &str, args: &[&str]) -> UiActiveBackgroundTask {
    UiActiveBackgroundTask {
        task_id: task.into(),
        program: program.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        elapsed_ms: 400,
        pid: Some(4242),
        ready_evidence: None,
        lifetime: None,
    }
}

fn reconciled(state: &mut AppState, tasks: Vec<UiActiveBackgroundTask>) {
    reduce(
        state,
        Action::Runtime(RuntimeEvent::BackgroundTasksReconciled { tasks }),
    );
}

/// The jobs page, as the user would open it from the strip.
fn jobs_page(state: &mut AppState, selected: &str) -> String {
    state.active_screen = Screen::ActivityList;
    state.background_list_selected = Some(selected.to_string());
    rendered(state, 100, 30)
}

// --- Gate A: one start fact -------------------------------------------------

#[test]
fn a_start_is_the_conversation_row_and_the_live_count_only() {
    let mut s = zh();
    tool_started(&mut s, "c1", "npm", &["run", "dev"]);
    tool_completed(&mut s, "c1", "t1");
    started(&mut s, "t1", "npm", &["run", "dev"]);

    assert!(
        s.notification.is_none(),
        "a start must not raise a second notice: {:?}",
        s.notification
    );
    let frame = rendered(&mut s, 100, 30);
    assert!(frame.contains("后台运行"), "{frame}");
    assert!(frame.contains("npm run dev"), "{frame}");
    assert!(!frame.contains("已在后台启动"), "{frame}");
    assert!(frame.contains("后台 1"), "{frame}");
    assert!(s.background_task_labels.contains_key("t1"));
    assert!(
        s.background_task_labels["t1"].label.contains("npm run dev"),
        "the full command stays in the task record"
    );
}

#[test]
fn concurrent_starts_never_accumulate_a_notice() {
    let mut s = zh();
    started(&mut s, "t1", "npm", &["run", "dev"]);
    started(&mut s, "t2", "make", &["up"]);
    assert!(s.notification.is_none(), "{:?}", s.notification);

    let frame = rendered(&mut s, 100, 30);
    assert!(!frame.contains("已在后台启动"), "{frame}");
    assert!(frame.contains("后台 2"), "{frame}");
}

#[test]
fn a_repeated_start_for_the_same_task_stays_one_task() {
    let mut s = zh();
    started(&mut s, "t1", "make", &["up"]);
    started(&mut s, "t1", "make", &["up"]);
    assert!(s.notification.is_none(), "{:?}", s.notification);
    let frame = rendered(&mut s, 100, 30);
    assert!(frame.contains("后台 1"), "{frame}");
    assert!(!frame.contains("已在后台启动"), "{frame}");
}

// --- Gate B: no failure reminder -------------------------------------------

#[test]
fn a_failure_leaves_no_reminder_but_keeps_every_recorded_fact() {
    let mut s = zh();
    tool_started(&mut s, "c1", "npm", &["run", "dev"]);
    tool_completed(&mut s, "c1", "t1");
    started(&mut s, "t1", "npm", &["run", "dev"]);
    tool_started(&mut s, "c2", "make", &["e2e-fail"]);
    tool_completed(&mut s, "c2", "t2");
    started(&mut s, "t2", "make", &["e2e-fail"]);
    exited(&mut s, "t2", 2, false);

    assert!(s.notification.is_none(), "{:?}", s.notification);
    let frame = rendered(&mut s, 100, 30);
    assert!(!frame.contains("后台失败"), "{frame}");
    assert!(!frame.contains("问题 1"), "{frame}");
    assert!(s.issue_hit.is_none(), "a failed task is not an issue");
    // The still-running task is the only thing the strip counts.
    assert!(frame.contains("后台 1"), "{frame}");

    // The failure itself is unchanged data, not display state.
    let failed = &s.background_task_labels["t2"];
    assert_eq!(failed.ok, Some(false));
    assert_eq!(failed.exit_code, Some(2));
    assert_eq!(failed.duration_ms, Some(1200));
    assert!(failed.output.contains("log for t2"));
    assert!(!failed.is_running());

    // And it is still reachable from the background page.
    let jobs = jobs_page(&mut s, "t2");
    assert!(jobs.contains("make e2e-fail"), "{jobs}");
    assert!(jobs.contains("EXIT 2"), "{jobs}");
    assert!(jobs.contains("log for t2"), "{jobs}");
}

#[test]
fn only_failures_never_make_the_strip_an_issue_cell() {
    let mut s = zh();
    started(&mut s, "t1", "make", &["e2e-fail"]);
    exited(&mut s, "t1", 1, false);
    started(&mut s, "t2", "npm", &["test"]);
    exited(&mut s, "t2", 1, false);

    let frame = rendered(&mut s, 100, 30);
    // The failures stay in the conversation; the strip says nothing about
    // them and offers no cell to click.
    assert!(frame.contains("后台任务失败"), "{frame}");
    assert!(!frame.contains("后台失败"), "{frame}");
    assert!(!frame.contains('⚠'), "{frame}");
    assert!(!frame.contains('↗'), "{frame}");
    assert!(s.issue_hit.is_none(), "{:?}", s.issue_hit);
    assert!(s.background_hit.is_none(), "{:?}", s.background_hit);
    assert!(s.notification.is_none(), "{:?}", s.notification);
    assert_eq!(s.background_task_labels.len(), 2);
}

#[test]
fn a_stopped_task_is_not_a_failure_and_leaves_no_trace_on_the_strip() {
    let mut s = zh();
    started(&mut s, "t1", "npm", &["run", "dev"]);
    stopped(&mut s, "t1");

    assert!(s.background_task_labels["t1"].stopped);
    assert!(!s.background_task_labels["t1"].is_failed());
    let frame = rendered(&mut s, 100, 30);
    assert!(!frame.contains("后台 1"), "{frame}");
    assert!(!frame.contains("后台失败"), "{frame}");
    assert!(s.issue_hit.is_none());
}

// --- Gate C: the count is the live lifecycle -------------------------------

#[test]
fn the_count_follows_start_duplicate_failure_and_finish() {
    let mut s = zh();
    started(&mut s, "t1", "npm", &["run", "dev"]);
    started(&mut s, "t2", "make", &["up"]);
    assert!(rendered(&mut s, 100, 30).contains("后台 2"));

    // A replay is not a new process.
    started(&mut s, "t2", "make", &["up"]);
    let frame = rendered(&mut s, 100, 30);
    assert!(frame.contains("后台 2"), "{frame}");
    assert!(!frame.contains("后台 3"), "{frame}");

    exited(&mut s, "t1", 1, false);
    let frame = rendered(&mut s, 100, 30);
    assert!(frame.contains("后台 1"), "{frame}");
    assert!(!frame.contains("后台 2"), "{frame}");

    exited(&mut s, "t2", 0, true);
    let frame = rendered(&mut s, 100, 30);
    assert!(!frame.contains("后台 1"), "{frame}");
    assert!(s.background_hit.is_none(), "hidden when nothing runs");
    assert_eq!(s.background_task_labels["t2"].ok, Some(true));
}

#[test]
fn a_fast_start_and_exit_never_leaves_a_stale_count() {
    let mut s = zh();
    started(&mut s, "t1", "make", &["quiet"]);
    exited(&mut s, "t1", 0, true);
    let frame = rendered(&mut s, 100, 30);
    assert!(!frame.contains("后台 1"), "{frame}");
    assert!(s.background_hit.is_none());
}

#[test]
fn failures_never_hide_a_still_running_task() {
    let mut s = zh();
    started(&mut s, "run", "npm", &["run", "dev"]);
    started(&mut s, "bad1", "make", &["e2e-fail"]);
    exited(&mut s, "bad1", 1, false);
    started(&mut s, "bad2", "make", &["check-fail"]);
    exited(&mut s, "bad2", 1, false);

    let frame = rendered(&mut s, 100, 30);
    assert!(frame.contains("后台 1"), "{frame}");
    assert!(!frame.contains("后台失败"), "{frame}");
    assert!(!frame.contains("问题"), "{frame}");
}

#[test]
fn a_retry_after_a_failure_counts_the_new_running_task() {
    let mut s = zh();
    started(&mut s, "attempt-1", "make", &["check-fail"]);
    exited(&mut s, "attempt-1", 1, false);
    started(&mut s, "attempt-2", "make", &["check"]);
    let frame = rendered(&mut s, 100, 30);
    assert!(frame.contains("后台 1"), "{frame}");
    assert!(!frame.contains("后台失败"), "{frame}");
}

#[test]
fn the_count_does_not_need_an_active_turn() {
    let mut s = zh();
    assert_eq!(s.status, leveler_client_protocol::RuntimeStatus::Idle);
    started(&mut s, "t1", "make", &["up"]);
    assert!(rendered(&mut s, 100, 30).contains("后台 1"));
    assert!(s.background_hit.is_some());
}

#[test]
fn a_reconnect_snapshot_is_the_live_truth_and_never_double_counts() {
    let mut s = zh();
    reconciled(&mut s, vec![active("t1", "npm", &["run", "dev"])]);
    assert!(rendered(&mut s, 100, 30).contains("后台 1"));

    // Replaying the same snapshot is idempotent.
    reconciled(&mut s, vec![active("t1", "npm", &["run", "dev"])]);
    let frame = rendered(&mut s, 100, 30);
    assert!(frame.contains("后台 1"), "{frame}");
    assert!(!frame.contains("后台 2"), "{frame}");
    assert!(s.notification.is_none(), "{:?}", s.notification);

    // A snapshot without the task means it is no longer running.
    reconciled(&mut s, Vec::new());
    let frame = rendered(&mut s, 100, 30);
    assert!(!frame.contains("后台 1"), "{frame}");
    assert!(s.background_hit.is_none());
}

#[test]
fn a_turn_failure_is_still_an_issue_even_with_background_failures() {
    let mut s = zh();
    started(&mut s, "t1", "make", &["e2e-fail"]);
    exited(&mut s, "t1", 1, false);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnFailed {
            error: "provider failed".into(),
            failure: None,
            recovery: None,
        }),
    );
    let frame = rendered(&mut s, 100, 30);
    // The failed turn is the only issue; it is named, not counted.
    assert!(frame.contains("provider failed"), "{frame}");
    assert!(!frame.contains("后台失败"), "{frame}");
    assert!(s.issue_hit.is_some());
}

// --- Gate E: layout and locale ---------------------------------------------

#[test]
fn the_removed_prompts_are_absent_in_both_locales() {
    for locale in [leveler_tui::Locale::Zh, leveler_tui::Locale::En] {
        let mut s = state(locale);
        started(&mut s, "t1", "npm", &["run", "dev"]);
        exited(&mut s, "t1", 1, false);
        let frame = rendered(&mut s, 100, 30);
        for forbidden in [
            "已在后台启动",
            "Started in the background",
            "后台失败",
            "background failed",
        ] {
            assert!(
                !frame.contains(forbidden),
                "{locale:?} frame still shows {forbidden:?}:\n{frame}"
            );
        }
        assert!(s.issue_hit.is_none(), "{locale:?}");
    }
}

#[test]
fn a_start_adds_no_extra_layout_row() {
    for locale in [leveler_tui::Locale::Zh, leveler_tui::Locale::En] {
        let mut s = state(locale);
        let before = rendered(&mut s, 100, 30);
        let composer_before = s.input_rect;
        started(&mut s, "t1", "npm", &["run", "dev"]);
        let after = rendered(&mut s, 100, 30);

        assert_eq!(
            composer_before, s.input_rect,
            "{locale:?}: a start may not move the composer"
        );
        assert_eq!(before.lines().count(), after.lines().count(), "{locale:?}");
        // No blank placeholder between the conversation and the strip.
        let blank_rows_before = before.lines().filter(|line| line.trim().is_empty()).count();
        let blank_rows_after = after.lines().filter(|line| line.trim().is_empty()).count();
        assert!(
            blank_rows_after <= blank_rows_before,
            "{locale:?}: a start must not open a blank row\nbefore:\n{before}\nafter:\n{after}"
        );
        let lines: Vec<&str> = after.lines().collect();
        let strip_y = s.background_hit.expect("the count cell has a hit rect").1 as usize;
        assert!(
            !lines[strip_y].trim().is_empty(),
            "{locale:?}: the strip row is painted, not an invisible hit area"
        );
    }
}
