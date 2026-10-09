//! Headless dogfood for the Background Tasks console (master/detail page).
//!
//! Drives the real reducer with real `RuntimeEvent`s and renders through the
//! real screen path, so the assertions prove the page reads live projection
//! state: selection, per-task output, stderr distinction and per-task scroll.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
            model_notice: None,
            thinking: None,
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
    let _ = reduce(state, Action::Runtime(event));
}

fn press(s: &mut AppState, code: KeyCode) {
    let _ = reduce(s, Action::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn start(state: &mut AppState, id: &str, program: &str, pid: Option<u32>) {
    runtime(
        state,
        RuntimeEvent::BackgroundTaskStarted {
            task_id: id.into(),
            program: program.into(),
            args: vec![],
            pid,
        },
    );
}

fn output(state: &mut AppState, id: &str, chunk: &str) {
    runtime(
        state,
        RuntimeEvent::BackgroundTaskOutput {
            task_id: id.into(),
            chunk: chunk.into(),
        },
    );
}

/// The detail pane shows exactly the selected task's stdout/stderr and its
/// runtime facts; another task's lines never appear on it.
#[test]
fn detail_pane_shows_only_the_selected_tasks_output() {
    let mut s = opened();
    for (id, program, marker, pid) in [
        ("bg-1", "make", "out-one", 1001),
        ("bg-2", "npm", "out-two", 1002),
        ("bg-3", "cargo", "out-three", 1003),
        ("bg-4", "worker", "out-four", 1004),
    ] {
        start(&mut s, id, program, Some(pid));
        output(&mut s, id, &format!("[stdout] {marker}\n"));
    }
    s.active_screen = Screen::ActivityList;
    s.background_list_selected = Some("bg-2".into());
    let text = render_text(&mut s, 100, 40);
    println!("--- BACKGROUND CONSOLE (bg-2 selected) ---\n{text}");
    assert!(
        text.contains("out-two"),
        "selected output must show:\n{text}"
    );
    assert!(text.contains("1002"), "selected pid must show:\n{text}");
    for foreign in ["out-one", "out-three", "out-four"] {
        assert!(
            !text.contains(foreign),
            "{foreign} belongs to another task:\n{text}"
        );
    }

    // Selecting another task swaps the detail, not the log store.
    s.background_list_selected = Some("bg-3".into());
    let text = render_text(&mut s, 100, 40);
    assert!(text.contains("out-three"), "{text}");
    assert!(!text.contains("out-two"), "{text}");
    assert!(text.contains("1003"), "{text}");
}

/// The runtime tags a chunk's stream; the page keeps stderr visible and
/// distinct instead of flattening it into stdout.
#[test]
fn stderr_lines_are_visually_distinct() {
    let mut s = opened();
    start(&mut s, "bg-1", "make", Some(4242));
    output(&mut s, "bg-1", "[stdout] fine\n");
    output(&mut s, "bg-1", "[stderr] boom\n");
    s.active_screen = Screen::ActivityList;
    s.background_list_selected = Some("bg-1".into());
    let text = render_text(&mut s, 100, 30);
    println!("--- BACKGROUND CONSOLE (stdout+stderr) ---\n{text}");
    assert!(text.contains("fine"), "{text}");
    assert!(
        text.contains("[stderr] boom"),
        "stderr must stay labeled:\n{text}"
    );
    assert!(
        !text.contains("[stdout] fine"),
        "the redundant stdout tag is stripped:\n{text}"
    );
}

/// Scroll state is per task: pausing on one task's log must not pause another,
/// and returning restores the paused task's position.
#[test]
fn per_task_scroll_is_independent() {
    let mut s = opened();
    for (index, (id, program)) in [("bg-a", "make"), ("bg-b", "npm")].into_iter().enumerate() {
        s.elapsed_secs = index as u64 + 1;
        start(&mut s, id, program, None);
        for i in 0..40 {
            output(&mut s, id, &format!("[stdout] {program}-{i}\n"));
        }
    }
    s.active_screen = Screen::ActivityList;
    s.background_list_selected = Some("bg-a".into());
    // Let the renderer publish the viewport, then leave follow.
    let _ = render_text(&mut s, 100, 24);
    press(&mut s, KeyCode::Enter); // focus output
    press(&mut s, KeyCode::PageUp);
    let _ = render_text(&mut s, 100, 24);
    assert_eq!(
        s.background_task_views.get("bg-a").map(|v| v.follow),
        Some(false),
        "bg-a must be paused after scrolling back"
    );

    // Switch to bg-b: it is an independent, still-following view.
    press(&mut s, KeyCode::Tab); // back to the list
    s.background_list_selected = Some("bg-b".into());
    let _ = render_text(&mut s, 100, 24);
    assert_eq!(
        s.background_task_views.get("bg-b").map(|v| v.follow),
        Some(true),
        "bg-b must not inherit bg-a's paused state"
    );
    assert_eq!(
        s.background_task_views.get("bg-a").map(|v| v.follow),
        Some(false),
        "bg-a's position is retained"
    );

    // End resumes follow on the selected task only.
    press(&mut s, KeyCode::Enter);
    press(&mut s, KeyCode::End);
    let _ = render_text(&mut s, 100, 24);
    assert_eq!(
        s.background_task_views.get("bg-b").map(|v| v.follow),
        Some(true)
    );
    assert_eq!(
        s.background_task_views.get("bg-a").map(|v| v.follow),
        Some(false)
    );
}

/// Standard terminal sizes keep the list, the selected task's state and its
/// recent output; a failure's exit code is visible in the list itself.
#[test]
fn standard_terminal_sizes_keep_list_state_and_recent_logs() {
    let mut s = opened();
    s.elapsed_secs = 1;
    start(&mut s, "bg-run", "make", Some(1));
    output(&mut s, "bg-run", "[stdout] ready on :8080\n");
    s.elapsed_secs = 2;
    start(&mut s, "bg-fail", "npm", Some(2));
    output(&mut s, "bg-fail", "[stderr] EADDRINUSE\n");
    runtime(
        &mut s,
        RuntimeEvent::BackgroundTaskExited {
            task_id: "bg-fail".into(),
            exit_code: Some(1),
            duration_ms: 3_000,
            ok: false,
            stopped: false,
            output: String::new(),
        },
    );
    s.active_screen = Screen::ActivityList;
    s.background_list_selected = Some("bg-run".into());
    for (w, h) in [(80, 24), (120, 40)] {
        let text = render_text(&mut s, w, h);
        assert!(text.contains("make"), "list missing at {w}x{h}:\n{text}");
        assert!(
            text.contains("npm"),
            "finished row missing at {w}x{h}:\n{text}"
        );
        assert!(
            text.contains("ready on :8080"),
            "recent log missing at {w}x{h}:\n{text}"
        );
        assert!(
            text.contains("EXIT 1"),
            "exit code missing at {w}x{h}:\n{text}"
        );
    }

    s.background_list_selected = Some("bg-fail".into());
    let text = render_text(&mut s, 100, 30);
    assert!(text.contains("EADDRINUSE"), "{text}");
}

/// A chatty task cannot grow the TUI projection without bound, and painting
/// the page stays pure (no panic) after the cap is hit.
#[test]
fn high_volume_output_keeps_the_projection_bounded() {
    let mut s = opened();
    start(&mut s, "bg-1", "make", Some(7));
    for i in 0..20_000 {
        output(&mut s, "bg-1", &format!("[stdout] line-{i}\n"));
    }
    let len = s
        .background_task_labels
        .get("bg-1")
        .map(|c| c.output.len())
        .unwrap_or(0);
    assert!(len <= 128 * 1024, "projection grew to {len} bytes");
    assert!(
        s.background_task_labels
            .get("bg-1")
            .is_some_and(|c| c.output.contains("line-19999")),
        "the newest line stays in the window"
    );
    s.active_screen = Screen::ActivityList;
    s.background_list_selected = Some("bg-1".into());
    let text = render_text(&mut s, 100, 30);
    assert!(
        text.contains("Background") || text.contains("后台任务"),
        "{text}"
    );
    // Follow is still on: the newest line is what the reader sees.
    assert_eq!(
        s.background_task_views.get("bg-1").map(|v| v.follow),
        Some(true)
    );
}
