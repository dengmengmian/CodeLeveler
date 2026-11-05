//! The user-shell output window: a bounded, scrollable viewport over the
//! complete retained output — never a fold, never a truncated log.
//!
//! Every assertion here reads what the real renderer paints (TestBackend) or
//! what the real reducer/copy path produces, so the window's height, scroll
//! clamp, follow behaviour and copy policy are checked as one contract.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use leveler_client_protocol::{
    ClientCommand, PermissionProfile, RuntimeEvent, SessionId, UiSessionSnapshot,
};
use leveler_tui::Locale;
use leveler_tui::action::{Action, Effect};
use leveler_tui::reducer::reduce;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use leveler_tui::transcript::{TranscriptItem, UserShellStatus};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const WIDTH: u16 = 100;
const HEIGHT: u16 = 40;

fn opened() -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "tester".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale: Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    s.size = (WIDTH, HEIGHT);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(),
        }),
    );
    s
}

fn snapshot() -> UiSessionSnapshot {
    UiSessionSnapshot {
        history: None,
        id: SessionId::new("s1"),
        repository: Some("/repo".into()),
        task_status: None,
        task_terminal: None,
        goal: "interactive session".into(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
        mode: PermissionProfile::Assisted,
        branch: Some("main".into()),
        status: "idle".into(),
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

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn rendered(s: &mut AppState, w: u16, h: u16) -> String {
    s.size = (w, h);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, s)).unwrap();
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

fn start(s: &mut AppState, id: &str, command: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::UserShellStarted {
            execution_id: leveler_core::UserShellId::new(id),
            command: command.into(),
            cwd: "/repo".into(),
        }),
    );
}

fn output(s: &mut AppState, id: &str, chunk: &str, stream: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::UserShellOutput {
            execution_id: leveler_core::UserShellId::new(id),
            stream: stream.into(),
            chunk: chunk.into(),
        }),
    );
}

fn finish(s: &mut AppState, id: &str, code: Option<i32>, status: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::UserShellExited {
            execution_id: leveler_core::UserShellId::new(id),
            exit_code: code,
            duration_ms: 420,
            status: status.into(),
        }),
    );
}

fn shell<'a>(s: &'a AppState, id: &str) -> Option<&'a leveler_tui::transcript::UserShellBlock> {
    s.transcript.user_shell(&leveler_core::UserShellId::new(id))
}

fn rows_of(n: usize, prefix: &str) -> String {
    (1..=n).map(|i| format!("{prefix}-{i:04}\n")).collect()
}

/// The painted output body rows of a record: rail rows that are neither the
/// status header, the `$ command` line, a blank separator, nor a window hint.
fn output_region(screen: &str) -> Vec<String> {
    screen
        .lines()
        .filter(|line| line.starts_with("  │"))
        .map(|line| line.trim_end().to_string())
        .filter(|line| {
            let body = line.trim_start_matches("  │").trim();
            !body.is_empty()
                && !body.contains("(user)")
                && !body.starts_with('$')
                && !body.contains("上方还有")
                && !body.contains("下方还有")
                && !body.contains("完整输出")
        })
        .collect()
}

// ── height: short output is natural, long output is bounded ────────────────

#[test]
fn one_line_output_is_painted_in_full_with_no_scroll_hint() {
    let mut s = opened();
    start(&mut s, "ush-1", "cat README.md");
    output(&mut s, "ush-1", "hello world\n", "stdout");
    finish(&mut s, "ush-1", Some(0), "success");
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(screen.contains("hello world"), "{screen}");
    assert!(!screen.contains("上方还有"), "nothing is hidden: {screen}");
    assert!(!screen.contains("下方还有"), "nothing is hidden: {screen}");
}

#[test]
fn ten_lines_fit_without_a_window_and_a_hundred_lines_are_a_window() {
    let mut s = opened();
    start(&mut s, "ush-10", "seq 1 10");
    output(&mut s, "ush-10", &rows_of(10, "row"), "stdout");
    finish(&mut s, "ush-10", Some(0), "success");
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    for n in 1..=10 {
        assert!(screen.contains(&format!("row-{n:04}")), "row {n}: {screen}");
    }
    assert!(!screen.contains("上方还有"), "{screen}");

    let mut s = opened();
    start(&mut s, "ush-100", "seq 1 100");
    output(&mut s, "ush-100", &rows_of(100, "row"), "stdout");
    finish(&mut s, "ush-100", Some(0), "success");
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    let body = output_region(&screen);
    assert!(
        body.len() <= leveler_tui::SHELL_OUTPUT_WINDOW_ROWS + 1,
        "the record stops growing at the window height: {} rows\n{screen}",
        body.len()
    );
    assert!(
        screen.contains("row-0100"),
        "the newest row is visible: {screen}"
    );
    assert!(
        screen.contains("上方还有 88 行"),
        "the rows above the window are counted: {screen}"
    );
}

#[test]
fn a_thousand_line_result_stays_bounded_and_keeps_every_row() {
    let mut s = opened();
    start(&mut s, "ush-1k", "find .");
    output(&mut s, "ush-1k", &rows_of(1000, "hit"), "stdout");
    finish(&mut s, "ush-1k", Some(0), "success");
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(screen.contains("hit-1000"), "{screen}");
    let body = output_region(&screen);
    assert!(
        body.len() <= leveler_tui::SHELL_OUTPUT_WINDOW_ROWS + 1,
        "a 1000-line result must not lay out 1000 rows: {}\n{screen}",
        body.len()
    );
    let record = shell(&s, "ush-1k").expect("record");
    assert_eq!(
        record.output.lines().count(),
        1000,
        "every line is retained, not truncated for display"
    );
}

// ── scrolling: inside the record, never the conversation ──────────────────

fn scroll_record(s: &mut AppState, up: bool, times: usize) {
    for _ in 0..times {
        reduce(
            s,
            Action::Mouse(MouseEvent {
                kind: if up {
                    MouseEventKind::ScrollUp
                } else {
                    MouseEventKind::ScrollDown
                },
                column: 12,
                row: record_row(s),
                modifiers: KeyModifiers::empty(),
            }),
        );
    }
}

/// A screen row inside the record's output body.
fn record_row(s: &AppState) -> u16 {
    let width = leveler_tui::conversation::geometry::content_width(s);
    let (start, end) = s.item_span(0, width).expect("the record owns lines");
    let (_, y, _, _) = s.conv.rect.expect("laid out");
    y + (start + (end - start) / 2) as u16
}

#[test]
fn the_wheel_scrolls_the_record_without_moving_the_conversation() {
    let mut s = opened();
    start(&mut s, "ush-w", "seq 1 200");
    output(&mut s, "ush-w", &rows_of(200, "row"), "stdout");
    finish(&mut s, "ush-w", Some(0), "success");
    let _ = rendered(&mut s, WIDTH, HEIGHT);
    let conversation_before = s.conv.scroll;
    scroll_record(&mut s, true, 4);
    let record = shell(&s, "ush-w").expect("record");
    assert!(!record.view.follow, "scrolling up leaves follow");
    assert_eq!(record.view.scroll, 188 - 12, "four wheel steps of 3 rows");
    assert_eq!(
        s.conv.scroll, conversation_before,
        "the Conversation never moves under a record scroll"
    );

    // Scrolling back down to the bottom resumes following.
    scroll_record(&mut s, false, 40);
    let record = shell(&s, "ush-w").expect("record");
    assert!(record.view.follow, "back at the bottom: follow resumes");
    assert_eq!(
        s.conv.scroll, conversation_before,
        "still no Conversation movement"
    );
}

#[test]
fn scrolled_up_output_is_reachable_row_by_row() {
    let mut s = opened();
    start(&mut s, "ush-up", "seq 1 200");
    output(&mut s, "ush-up", &rows_of(200, "row"), "stdout");
    finish(&mut s, "ush-up", Some(0), "success");
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(screen.contains("row-0200"), "{screen}");
    // Click the header: the record's window takes the keys.
    let width = leveler_tui::conversation::geometry::content_width(&s);
    let (start, _) = s.item_span(0, width).unwrap();
    let (_, y, _, _) = s.conv.rect.unwrap();
    let header_row = y + start as u16;
    reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: header_row,
            modifiers: KeyModifiers::empty(),
        }),
    );
    assert!(
        s.shell_output_focus.is_some(),
        "the header focuses the window"
    );
    // Home = the first row, End = the newest row.
    reduce(&mut s, key(KeyCode::Home));
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(
        screen.contains("row-0001"),
        "the head is reachable: {screen}"
    );
    assert!(screen.contains("下方还有"), "{screen}");
    reduce(&mut s, key(KeyCode::End));
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(screen.contains("row-0200"), "{screen}");
    assert!(
        shell(&s, "ush-up").unwrap().view.follow,
        "End resumes follow"
    );
    // Esc returns the keys to the composer.
    reduce(&mut s, key(KeyCode::Esc));
    assert!(s.shell_output_focus.is_none(), "Esc releases the window");
}

// ── live output: follow, or hold the reader's position ────────────────────

#[test]
fn new_output_follows_the_tail_until_the_reader_scrolls_up() {
    let mut s = opened();
    start(&mut s, "ush-live", "cargo test");
    output(&mut s, "ush-live", &rows_of(40, "live"), "stdout");
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(screen.contains("live-0040"), "{screen}");

    // Streaming continues while following: the window keeps the newest rows.
    output(&mut s, "ush-live", &rows_of(60, "more"), "stdout");
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(screen.contains("more-0060"), "{screen}");
    assert!(shell(&s, "ush-live").unwrap().view.follow);

    // The reader scrolls up; new output must not yank them to the bottom.
    scroll_record(&mut s, true, 3);
    let pinned = shell(&s, "ush-live").unwrap().view;
    assert!(!pinned.follow);
    output(&mut s, "ush-live", &rows_of(80, "later"), "stdout");
    let after = shell(&s, "ush-live").unwrap().view;
    assert_eq!(after.scroll, pinned.scroll, "the reading position is held");
    assert!(!after.follow);
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(
        !screen.contains("later-0080"),
        "the tail did not steal the view: {screen}"
    );
    // The record still knows how much arrived below.
    assert!(screen.contains("下方还有"), "{screen}");
}

#[test]
fn a_finished_command_keeps_its_scroll_position_after_completion() {
    let mut s = opened();
    start(&mut s, "ush-done", "git log");
    output(&mut s, "ush-done", &rows_of(100, "commit"), "stdout");
    let _ = rendered(&mut s, WIDTH, HEIGHT);
    scroll_record(&mut s, true, 5);
    let before = shell(&s, "ush-done").unwrap().view;
    finish(&mut s, "ush-done", Some(0), "success");
    let after = shell(&s, "ush-done").unwrap().view;
    assert_eq!(after, before, "completion does not move the reader");
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    // Five wheel steps of three rows left the window at rows 74..=85.
    assert!(screen.contains("commit-0085"), "{screen}");
    assert!(!screen.contains("commit-0100"), "{screen}");
}

// ── geometry: resize re-wraps the same content ────────────────────────────

#[test]
fn resizing_rewraps_the_window_without_losing_content() {
    let mut s = opened();
    start(&mut s, "ush-resize", "cat long");
    let long = "abcdefghij".repeat(12);
    let body: String = (1..=40).map(|i| format!("{long}-{i:03}\n")).collect();
    output(&mut s, "ush-resize", &body, "stdout");
    finish(&mut s, "ush-resize", Some(0), "success");
    let narrow = rendered(&mut s, 60, HEIGHT);
    let wide = rendered(&mut s, 140, HEIGHT);
    assert!(narrow.contains("abcdefghij"), "{narrow}");
    assert!(wide.contains("abcdefghij"), "{wide}");
    let record = shell(&s, "ush-resize").expect("record");
    assert_eq!(
        record.output.lines().count(),
        40,
        "a resize never edits the retained output"
    );
    // Both widths paint a bounded window over the same rows.
    assert!(narrow.contains("上方还有"), "{narrow}");
    assert!(wide.contains("上方还有"), "{wide}");
}

// ── content: ANSI, wrapping, unicode, stderr ──────────────────────────────

#[test]
fn ansi_is_removed_long_lines_wrap_and_unicode_survives() {
    let mut s = opened();
    start(&mut s, "ush-ansi", "ls --color");
    output(
        &mut s,
        "ush-ansi",
        "\u{1b}[31mred\u{1b}[0m and \u{1b}[1;32mbold green\u{1b}[0m\n",
        "stdout",
    );
    let wide = "字".repeat(160);
    output(&mut s, "ush-ansi", &format!("{wide}\n"), "stdout");
    finish(&mut s, "ush-ansi", Some(0), "success");
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(screen.contains("red and bold green"), "{screen}");
    assert!(!screen.contains('\u{1b}'), "no escape reaches the screen");
    let record = shell(&s, "ush-ansi").expect("record");
    // The wrapped row is reachable, and no cell overruns the terminal.
    assert!(
        screen
            .lines()
            .all(|line| unicode_width::UnicodeWidthStr::width(line) <= WIDTH as usize),
        "a row overruns the terminal"
    );
    assert!(!record.output.is_empty());
}

#[test]
fn stderr_and_stdout_keep_their_own_semantics() {
    let mut s = opened();
    start(&mut s, "ush-err", "make");
    output(&mut s, "ush-err", "building\n", "stdout");
    output(&mut s, "ush-err", "error: boom\n", "stderr");
    finish(&mut s, "ush-err", Some(1), "failed");
    let record = shell(&s, "ush-err").expect("record");
    let stdout_at = record.output.find("building").unwrap();
    let stderr_at = record.output.find("error: boom").unwrap();
    assert!(!record.stream_at(stdout_at), "stdout provenance");
    assert!(record.stream_at(stderr_at), "stderr provenance");
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(screen.contains("building"), "{screen}");
    assert!(screen.contains("error: boom"), "{screen}");
    assert!(screen.contains("exit code 1"), "{screen}");
    assert!(
        screen.contains("✗"),
        "the failure status is painted: {screen}"
    );
}

#[test]
fn a_cancelled_command_keeps_the_output_it_produced() {
    let mut s = opened();
    start(&mut s, "ush-int", "long-job");
    output(&mut s, "ush-int", &rows_of(30, "partial"), "stdout");
    finish(&mut s, "ush-int", None, "cancelled");
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(screen.contains("partial-0030"), "{screen}");
    assert!(screen.contains("已取消"), "{screen}");
    assert!(
        !screen.contains("上方还有 30 行"),
        "no false accounting: {screen}"
    );
}

// ── copy: what the clipboard gets ─────────────────────────────────────────

#[test]
fn copy_output_is_the_complete_text_without_ui_decoration() {
    let mut s = opened();
    start(&mut s, "ush-copy", "seq 1 100");
    let body = rows_of(100, "copy");
    output(&mut s, "ush-copy", &body, "stdout");
    finish(&mut s, "ush-copy", Some(0), "success");
    let _ = rendered(&mut s, WIDTH, HEIGHT);
    s.composer.replace("/copy output");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    let copied = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::CopyUserShellOutput {
                directory,
                retained,
            } => {
                assert!(directory.is_none(), "no capture in this fixture");
                Some(retained.clone())
            }
            _ => None,
        })
        .expect("copy output effect");
    assert_eq!(
        copied, body,
        "the clipboard gets every retained byte, not the painted window"
    );
    for chrome in ["│ ", "$ seq", "Ran (user)", "上方还有", "下方还有"] {
        assert!(!copied.contains(chrome), "UI chrome leaked: {chrome:?}");
    }
}

// ── history: resume restores output and the window still scrolls ──────────

#[test]
fn resume_restores_the_output_and_the_window_scrolls_it() {
    let mut s = opened();
    start(&mut s, "ush-hist", "git log");
    let body = rows_of(80, "hist");
    output(&mut s, "ush-hist", &body, "stdout");
    finish(&mut s, "ush-hist", Some(0), "success");

    // A reconnect resyncs the same id with the runtime's retained tail.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserShellOutputReplaced {
            execution_id: leveler_core::UserShellId::new("ush-hist"),
            output_tail: body.clone(),
            output_truncated: false,
        }),
    );
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(screen.contains("hist-0080"), "{screen}");
    let record = shell(&s, "ush-hist").expect("record");
    assert_eq!(record.output, body, "the retained tail is authoritative");
    assert!(record.view.follow, "a restored record starts at its tail");

    scroll_record(&mut s, true, 2);
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(screen.contains("hist-0066"), "{screen}");
    assert!(!shell(&s, "ush-hist").unwrap().view.follow);
}

// ── scale: a large result does not clone into the viewport ────────────────

#[test]
fn a_large_result_paints_only_its_window() {
    let mut s = opened();
    start(&mut s, "ush-big", "find /");
    let body: String = (0..20_000)
        .map(|i| format!("path/to/file-{i:05}\n"))
        .collect();
    output(&mut s, "ush-big", &body, "stdout");
    finish(&mut s, "ush-big", Some(0), "success");
    assert_eq!(
        shell(&s, "ush-big").unwrap().output.len(),
        body.len(),
        "the retained text is the complete stream (below the retention cap)"
    );
    let started = std::time::Instant::now();
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    let elapsed = started.elapsed();
    assert!(screen.contains("file-19999"), "{screen}");
    assert!(
        output_region(&screen).len() <= leveler_tui::SHELL_OUTPUT_WINDOW_ROWS + 1,
        "the viewport is bounded"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "painting a 20k-line result took {elapsed:?}"
    );
}

// ── the record keeps its status line ─────────────────────────────────────

#[test]
fn the_record_keeps_command_status_and_duration_above_the_window() {
    let mut s = opened();
    start(&mut s, "ush-head", "cat docs/brand-spec.md");
    output(&mut s, "ush-head", &rows_of(40, "brand"), "stdout");
    finish(&mut s, "ush-head", Some(0), "success");
    let screen = rendered(&mut s, WIDTH, HEIGHT);
    assert!(screen.contains("Ran (user)"), "{screen}");
    assert!(
        screen.contains("cat docs/brand-spec.md"),
        "the command is named: {screen}"
    );
    assert!(screen.contains("0.4s"), "the measured duration: {screen}");
    assert!(
        !screen.contains("已折叠"),
        "no fold wording anywhere: {screen}"
    );
}

#[test]
fn a_fold_write_no_longer_exists_for_a_record() {
    let mut s = opened();
    start(&mut s, "ush-fold", "printf hi");
    output(&mut s, "ush-fold", "hi\n", "stdout");
    finish(&mut s, "ush-fold", Some(0), "success");
    let index = s
        .transcript
        .user_shell_index(&leveler_core::UserShellId::new("ush-fold"))
        .expect("record");
    assert!(
        !s.transcript
            .set_item_display(index, leveler_tui::fold::DisplayMode::Collapsed),
        "a scroll window is not a fold"
    );
    assert!(matches!(
        &s.transcript.items()[index],
        TranscriptItem::UserShell(record) if record.status == UserShellStatus::Success
    ));
    let _ = ClientCommand::RunUserShell {
        session_id: SessionId::new("s1"),
        command: "x".into(),
    };
    let _ = Screen::Conversation;
}
