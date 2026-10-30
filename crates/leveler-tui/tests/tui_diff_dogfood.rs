//! Full-frame dogfood for the edit-diff default: a real session event sequence
//! rendered through the production TUI frame at several terminal widths, then
//! the SAME frame after a real mouse click on the edit's head row.
//!
//! This is the closest automated stand-in for "start the TUI, look at the diff,
//! click it" without a real PTY or a live model. The dump files are written to
//! `target/dogfood/` so the rendered frames can be inspected by eye.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use leveler_client_protocol::{MessageId, RuntimeEvent, SessionId, ToolCallId};
use leveler_tui::action::Action;
use leveler_tui::conversation::build::build_conversation_lines_with_hits;
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const PATH: &str = "backend/internal/httpapi/cookies_test.go";

fn opened(width: u16, height: u16) -> AppState {
    let mut s = AppState::new(
        Theme::dark(),
        Boot {
            session_id: SessionId::new("dogfood"),
            user: "tester".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    s.size = (width, height);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new("m0"),
        }),
    );
    s
}

/// The diff the runtime reports for a created file: one added hunk from line 1.
fn created_file_diff(lines: usize) -> String {
    let content: String = (1..=lines)
        .map(|i| format!("    expectCookie{i}(t)\n"))
        .collect();
    let hunks = leveler_tools::tools::applied_diff::whole_file_hunks("", &content);
    leveler_tools::tools::applied_diff::unified_diff(PATH, &hunks).expect("real applied diff")
}

fn write_file(s: &mut AppState, id: &str, lines: usize) {
    let content: String = (1..=lines)
        .map(|i| format!("    expectCookie{i}(t)\n"))
        .collect();
    let call_id = ToolCallId::new(id);
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: call_id.clone(),
            name: "write_file".into(),
            arguments: serde_json::json!({ "path": PATH, "content": content }).to_string(),
            parallel: false,
            model_step: Some(0),
            answer_effect: None,
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: call_id,
            ok: true,
            preview: format!("Created {PATH} ({lines} lines)"),
            duration_ms: 5,
            applied_diff: Some(created_file_diff(lines)),
            exit_code: None,
            stop: None,
        }),
    );
    // Seal the round so the edit is settled history, exactly like a finished turn.
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new("m1"),
        }),
    );
}

fn screen_dump(state: &mut AppState, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            let cw = unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            x += cw;
        }
        out.push('\n');
    }
    out
}

/// The conversation's painted rows at `width`, as plain text.
fn conversation(s: &AppState, width: usize) -> Vec<String> {
    build_conversation_lines_with_hits(s, width)
        .0
        .into_iter()
        .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
        .collect()
}

fn dump(name: &str, screen: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/dogfood");
    std::fs::create_dir_all(&dir).ok();
    std::fs::write(dir.join(name), screen).ok();
}

/// The frame a narrow terminal paints must never overflow its width.
fn assert_fits(screen: &str, width: usize) {
    for line in screen.lines() {
        assert!(
            unicode_width::UnicodeWidthStr::width(line) <= width,
            "frame overflowed {width}: {line:?}"
        );
    }
}

/// 103-row created file, default view, four widths. Every applied row is in the
/// rendered conversation; the wide frame shows them on screen.
#[test]
fn dogfood_a_created_file_shows_every_row_by_default() {
    for (index, (width, height)) in [(24u16, 60u16), (40, 60), (80, 60), (120, 130)]
        .into_iter()
        .enumerate()
    {
        let mut s = opened(width, height);
        write_file(&mut s, "w", 103);
        let screen = screen_dump(&mut s, width, height);
        dump(&format!("created-103-{width}x{height}.txt"), &screen);
        assert_fits(&screen, width as usize);

        let plain = conversation(&s, width as usize);
        // A narrow column soft-wraps a row; concatenating the painted rows in
        // order must still yield the source text of every line.
        // Continuation rows carry the gutter padding, so drop spaces: the
        // tokens themselves contain none.
        let joined = plain.concat().replace(' ', "");
        for i in [1usize, 51, 103] {
            assert!(
                joined.contains(&format!("expectCookie{i}(t)")),
                "{width}x{height}: line {i} is missing from the edit"
            );
        }
        assert_eq!(
            plain.iter().filter(|l| l.contains('│')).count(),
            103,
            "{width}x{height}: the diff paints one row per line"
        );
        assert!(
            !plain.iter().any(|l| l.contains("还有")),
            "{width}x{height}: no elision hint exists"
        );
        // The tall frame is the visual evidence: the last row is on screen.
        if index == 3 {
            assert!(
                screen.contains("expectCookie103(t)"),
                "the wide frame shows the whole file:\n{screen}"
            );
            assert!(
                screen.contains("expectCookie1(t)"),
                "including its first row:\n{screen}"
            );
        }
    }
}

/// A real mouse click on the edit head folds the diff to one stat row; a second
/// click restores it. Both frames are dumped.
#[test]
fn dogfood_clicking_the_edit_head_folds_and_reopens_it() {
    let (width, height) = (120u16, 130u16);
    let mut s = opened(width, height);
    write_file(&mut s, "w", 103);
    let expanded = screen_dump(&mut s, width, height);
    dump("click-expanded-120x130.txt", &expanded);
    assert!(expanded.contains("expectCookie103(t)"));

    // The head row is where the memoized build paints it, in screen coordinates.
    // The tall frame fits the whole conversation, so the head is at a fixed
    // screen row: read it back from the painted frame itself.
    let row = expanded
        .lines()
        .position(|line| line.contains("cookies_test.go"))
        .expect("the head row is painted") as u16;

    let click = |s: &mut AppState| {
        reduce(
            s,
            Action::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 8,
                row,
                modifiers: KeyModifiers::empty(),
            }),
        )
    };
    click(&mut s);
    let folded = screen_dump(&mut s, width, height);
    dump("click-folded-120x130.txt", &folded);
    let plain = conversation(&s, width as usize);
    let heads: Vec<&String> = plain.iter().filter(|l| l.contains(PATH)).collect();
    assert_eq!(heads.len(), 1, "one row names the file: {plain:#?}");
    assert!(
        heads[0].contains("+103/-0"),
        "the folded row states the totals: {:?}",
        heads[0]
    );
    assert!(
        !plain.iter().any(|l| l.contains("expectCookie")),
        "no diff rows remain: {plain:#?}"
    );

    click(&mut s);
    let reopened = screen_dump(&mut s, width, height);
    dump("click-reopened-120x130.txt", &reopened);
    assert_eq!(reopened, expanded, "one click back restores the same frame");
}

/// Ctrl+O on a settled edit-only node folds it and restores it, from the
/// keyboard, without touching any other history.
#[test]
fn dogfood_ctrl_o_folds_the_latest_edit_only() {
    let mut s = opened(120, 60);
    write_file(&mut s, "w", 20);
    let before = screen_dump(&mut s, 120, 60);
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
    );
    let folded = screen_dump(&mut s, 120, 60);
    dump("ctrl-o-folded-120x60.txt", &folded);
    assert_ne!(before, folded);
    assert!(folded.contains("+20/-0"), "{folded}");
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
    );
    assert_eq!(screen_dump(&mut s, 120, 60), before);
}
