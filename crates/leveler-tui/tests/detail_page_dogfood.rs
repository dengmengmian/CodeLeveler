//! Headless dogfood for the Detail Pages.
//!
//! Drives the real reducer with real `RuntimeEvent`s and renders through the
//! real screen path — the same projection the interactive TUI paints. This is
//! the closest observable stand-in when no interactive terminal is available:
//! it proves the pages read live runtime state, not a fixture.

use leveler_client_protocol::{RuntimeEvent, SessionId};
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use unicode_width::UnicodeWidthStr;

fn opened() -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".into(),
            version: "0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    )
}

fn render_text(state: &mut AppState, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut x = 0;
        while x < buf.area.width {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += UnicodeWidthStr::width(sym).max(1) as u16;
        }
        out.push('\n');
    }
    out
}

fn runtime(state: &mut AppState, event: RuntimeEvent) {
    reduce(state, Action::Runtime(event));
}

fn child_update(done: bool, detail: &str) -> RuntimeEvent {
    RuntimeEvent::SubAgentUpdated {
        id: "c1".into(),
        nickname: "Euclid".into(),
        role: "explorer".into(),
        title: Some("调查选择器归属".into()),
        done,
        ok: true,
        detail: detail.into(),
        profile_id: None,
        profile_role: None,
        read_only: true,
        agent: None,
        contribution: None,
        outcome: None,
        stop: None,
        limit: None,
        background: Some(true),
        scope: Vec::new(),
    }
}

fn child_activity(phase: &str, tool: &str, preview: &str, is_error: bool) -> RuntimeEvent {
    RuntimeEvent::SubAgentActivity {
        id: "c1".into(),
        phase: phase.into(),
        tool: tool.into(),
        preview: preview.into(),
        is_error,
    }
}

/// CASE A — a background task's Detail Page follows real, streamed output and
/// offers a real stop while it runs. Esc only leaves.
#[test]
fn dogfood_background_task_detail_page() {
    let mut s = opened();
    runtime(
        &mut s,
        RuntimeEvent::BackgroundTaskStarted {
            task_id: "bg-1".into(),
            program: "make".into(),
            args: vec!["up".into()],
        },
    );
    for chunk in [
        "[+] Building web\n",
        "[+] Starting service\n",
        "server listening on :3000\n",
    ] {
        runtime(
            &mut s,
            RuntimeEvent::BackgroundTaskOutput {
                task_id: "bg-1".into(),
                chunk: chunk.into(),
            },
        );
    }
    s.activity_open = Some(leveler_tui::activity::ActivityId::Background("bg-1".into()));
    s.active_screen = Screen::Activity;

    let before = render_text(&mut s, 90, 24);
    println!("--- BACKGROUND TASK (running) ---\n{before}");
    assert!(before.starts_with('←'), "nav owns the edge: {before}");
    assert!(
        before.contains("  make up"),
        "object on the gutter: {before}"
    );
    assert!(
        before.contains("  命令"),
        "section title on the gutter: {before}"
    );
    assert!(
        before.contains("    $ make up"),
        "command body indented: {before}"
    );
    assert!(
        before.contains("  输出"),
        "output section present: {before}"
    );
    assert!(
        before.contains("    server listening on :3000"),
        "live output rendered: {before}"
    );
    assert!(before.contains("● 运行中"), "status glyph: {before}");
    assert!(before.contains("x 停止"), "running page can stop: {before}");

    // A new chunk arrives while the page is open: it must appear.
    runtime(
        &mut s,
        RuntimeEvent::BackgroundTaskOutput {
            task_id: "bg-1".into(),
            chunk: "request 200 OK\n".into(),
        },
    );
    let after = render_text(&mut s, 90, 24);
    assert!(after.contains("request 200 OK"), "live update: {after}");

    // Esc leaves; the task keeps running (non-destructive back).
    reduce(
        &mut s,
        Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        )),
    );
    assert_eq!(s.active_screen, Screen::Conversation);
    assert!(
        s.background_task_labels
            .get("bg-1")
            .is_some_and(|c| c.is_running()),
        "Esc must not stop the task"
    );

    // Reopening shows the same live state, not a reset page.
    s.activity_open = Some(leveler_tui::activity::ActivityId::Background("bg-1".into()));
    s.active_screen = Screen::Activity;
    let reopened = render_text(&mut s, 90, 24);
    assert!(reopened.contains("request 200 OK"), "state survives reopen");
    assert!(reopened.contains("x 停止"), "{reopened}");

    // Terminal state: the stop action disappears.
    runtime(
        &mut s,
        RuntimeEvent::BackgroundTaskExited {
            task_id: "bg-1".into(),
            exit_code: Some(0),
            duration_ms: 19_020,
            ok: true,
            stopped: false,
            output: String::new(),
        },
    );
    let done = render_text(&mut s, 90, 24);
    println!("--- BACKGROUND TASK (done) ---\n{done}");
    assert!(done.contains("✓ 已完成"), "{done}");
    assert!(
        !done.contains("x 停止"),
        "terminal page cannot stop: {done}"
    );
}

/// CASE B — the Sub-agent Detail Page folds real `SubAgentActivity` into the
/// activity stream language: no raw tool spam, the live call expanded, the
/// settled runs collapsed, and the final result shown without duplicates.
#[test]
fn dogfood_sub_agent_detail_page() {
    let mut s = opened();
    s.status = leveler_client_protocol::RuntimeStatus::Busy;
    runtime(&mut s, child_update(false, "调查 portal-web 的选择器归属"));
    runtime(
        &mut s,
        RuntimeEvent::SubAgentProgress {
            id: "c1".into(),
            active: true,
            input_tokens: 1200,
            output_tokens: 340,
            cached_input_tokens: 0,
        },
    );
    for _ in 0..6 {
        runtime(
            &mut s,
            child_activity(
                "tool_started",
                "read_file",
                r#"{"path":"public-order.css"}"#,
                false,
            ),
        );
        runtime(
            &mut s,
            child_activity("tool_finished", "read_file", "ok", false),
        );
    }
    for _ in 0..9 {
        runtime(
            &mut s,
            child_activity(
                "tool_started",
                "run_command",
                r#"{"program":"rg","args":["selector","."]}"#,
                false,
            ),
        );
        runtime(
            &mut s,
            child_activity("tool_finished", "run_command", "match", false),
        );
    }
    runtime(
        &mut s,
        child_activity(
            "tool_started",
            "grep",
            r#"{"pattern":"public-order","path":"web"}"#,
            false,
        ),
    );
    s.activity_open = Some(leveler_tui::activity::ActivityId::Child("c1".into()));
    s.active_screen = Screen::Activity;

    let running = render_text(&mut s, 90, 40);
    println!("--- SUB-AGENT (running) ---\n{running}");
    assert!(running.contains("  Euclid"), "{running}");
    assert!(running.contains("● 运行中"), "{running}");
    assert!(running.contains("读取 6 个文件"), "{running}");
    assert!(running.contains("执行了 9 个命令"), "{running}");
    assert!(
        running.contains("● 搜索代码"),
        "current expanded: {running}"
    );
    assert!(running.contains("public-order"), "target shown: {running}");
    assert!(
        !running.contains("run_command"),
        "no raw tool spam:\n{running}"
    );
    assert!(
        !running.contains("read_file"),
        "no raw tool spam:\n{running}"
    );
    for line in running.lines() {
        assert!(
            !(line.contains('●') && line.contains('✓')),
            "one authoritative status per row: {line:?}"
        );
    }

    // The child finishes: the live call settles and the result takes over.
    runtime(
        &mut s,
        child_activity("tool_finished", "grep", "found", false),
    );
    runtime(
        &mut s,
        child_update(true, "已完成 portal-web 选择器归属调查。"),
    );
    let done = render_text(&mut s, 90, 40);
    println!("--- SUB-AGENT (done) ---\n{done}");
    assert!(done.contains("✓ 已完成"), "{done}");
    assert!(
        done.contains("已完成 portal-web 选择器归属调查。"),
        "{done}"
    );
    assert!(
        !done.contains("x 停止"),
        "settled child cannot stop: {done}"
    );
    assert!(!done.contains("● 搜索代码"), "no live call left: {done}");
}

/// CASE C — the closure: background work lives in the input footer as an
/// aggregate, then the jobs list, then a detail. It never returns to the
/// conversation body, and opening the list acknowledges a failure without
/// deleting its history.
#[test]
fn dogfood_background_jobs_footer_and_list() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(s: &mut AppState, code: KeyCode) {
        let _ = reduce(s, Action::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    let mut s = opened();
    s.locale = leveler_tui::Locale::En;
    for (id, program) in [("bg-run", "make"), ("bg-run2", "cargo"), ("bg-fail", "npm")] {
        runtime(
            &mut s,
            RuntimeEvent::BackgroundTaskStarted {
                task_id: id.into(),
                program: program.into(),
                args: vec![],
            },
        );
    }
    runtime(
        &mut s,
        RuntimeEvent::BackgroundTaskOutput {
            task_id: "bg-run".into(),
            chunk: "compiling leveler-tui\n".into(),
        },
    );
    runtime(
        &mut s,
        RuntimeEvent::BackgroundTaskExited {
            task_id: "bg-fail".into(),
            exit_code: Some(1),
            duration_ms: 30_000,
            ok: false,
            stopped: false,
            output: "Error: listen EADDRINUSE\n".into(),
        },
    );

    // The conversation body has no task rows: the aggregate is the only place
    // background work appears, and it is exactly one footer row.
    let home = render_text(&mut s, 120, 30);
    println!("--- HOME (2 running, 1 failed) ---\n{home}");
    assert_eq!(
        home.lines().filter(|l| l.contains('↗')).count(),
        1,
        "one aggregate footer block:\n{home}"
    );
    assert!(home.contains("2 jobs"), "running count:\n{home}");
    assert!(home.contains("1 failed"), "failure count:\n{home}");
    assert!(
        !home.contains("compiling"),
        "live output never leaks into the body:\n{home}"
    );

    // Footer focus → Enter opens the list.
    s.workbench_focus = leveler_tui::state::WorkbenchFocus::Background;
    press(&mut s, KeyCode::Enter);
    assert_eq!(s.active_screen, Screen::ActivityList);
    let list = render_text(&mut s, 100, 30);
    println!("--- BACKGROUND JOBS LIST ---\n{list}");
    assert!(list.contains("Running"), "{list}");
    assert!(list.contains("Recently finished"), "{list}");

    // Opening the list acknowledged the failure but kept the record.
    assert!(s.background_failures_seen.contains("bg-fail"));
    assert!(s.background_task_labels.contains_key("bg-fail"));

    // Open the running task's detail; the live tail is there.
    s.background_list_selected = Some("bg-run".into());
    press(&mut s, KeyCode::Enter);
    assert_eq!(s.active_screen, Screen::Activity);
    let detail = render_text(&mut s, 90, 24);
    println!("--- DETAIL (running) ---\n{detail}");
    assert!(detail.contains("compiling leveler-tui"), "{detail}");
    assert!(detail.contains("running"), "{detail}");

    // Esc returns to the conversation; the badge is gone, the count is not.
    press(&mut s, KeyCode::Esc);
    assert_eq!(s.active_screen, Screen::Conversation);
    let back = render_text(&mut s, 120, 30);
    println!("--- HOME (after ack) ---\n{back}");
    let footer = back
        .lines()
        .find(|l| l.contains('↗'))
        .unwrap_or_else(|| panic!("no footer summary:\n{back}"));
    assert!(
        !footer.contains("failed"),
        "the acknowledged failure stops reminding: {footer:?}"
    );
    assert!(footer.contains("2 jobs"), "{footer:?}");
    assert!(
        s.background_task_labels.contains_key("bg-fail"),
        "history survives acknowledgement"
    );
}
