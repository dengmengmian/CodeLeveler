//! Secondary Surface closure.
//!
//! Behaviour these tests pin:
//!
//! - BTW carries roles with a light glyph + alignment, never a "You"/"Assistant"
//!   title, and wraps continuation lines under the message text.
//! - Esc on an open BTW surface returns to Main and never reaches the main
//!   run's cancel path, so a running task keeps running.
//! - The shared Secondary Surface shell draws the same chrome (identity header,
//!   rules, padded content, footer hint) for BTW and for full-screen pages, and
//!   only advertises keys it really answers.

use leveler_client_protocol::{RuntimeStatus, SessionId};
use leveler_tui::action::Action;
use leveler_tui::btw::{BtwTurnState, SurfaceFocus};
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn opened() -> AppState {
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
            reasoning_effort: None,
        },
    )
}

fn frame(state: &mut AppState, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            let mut line = String::new();
            let mut x = 0u16;
            while x < buf.area.width {
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                line.push_str(sym);
                // A double-width grapheme owns the next cell too; skip it so
                // the scan does not inject a phantom space between CJK chars.
                x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            }
            line
        })
        .collect()
}

fn col_of(line: &str, needle: &str) -> Option<usize> {
    line.find(needle).map(|b| line[..b].chars().count())
}

fn btw_state(question: &str, answer: &str) -> AppState {
    let mut s = opened();
    s.surface = SurfaceFocus::Btw;
    s.btw.begin(question.into());
    s.btw.append(answer);
    s.btw.finish(BtwTurnState::Done, None);
    s
}

/// A side-thread turn shows `›` / `●`, not a heavy role title.
#[test]
fn btw_roles_are_glyphs_not_titles() {
    let mut s = btw_state("这是什么？", "这是主任务之外的短暂提问。");
    let lines = frame(&mut s, 80, 24);
    let user_row = lines
        .iter()
        .position(|l| l.contains('›'))
        .expect("user marker");
    let asst_row = lines
        .iter()
        .position(|l| l.contains('●'))
        .expect("assistant marker");
    assert!(
        lines.iter().all(|l| l.trim() != "你" && l.trim() != "助手"),
        "no role titles anywhere:\n{}",
        lines.join("\n")
    );
    // Page padding: the marker is inset, not flush to the terminal edge.
    assert_eq!(col_of(&lines[user_row], "›"), Some(2));
    assert_eq!(col_of(&lines[asst_row], "●"), Some(2));
}

/// A wrapped message stays aligned under its own text.
#[test]
fn btw_wraps_continuation_under_the_message_text() {
    let answer = "第一段很长的回答内容".repeat(8);
    let mut s = btw_state("问题", &answer);
    let lines = frame(&mut s, 60, 24);
    let asst_row = lines
        .iter()
        .position(|l| l.contains('●'))
        .expect("assistant marker");
    // Text begins after "page padding (2) + marker + gap".
    assert_eq!(col_of(&lines[asst_row], "第"), Some(4));
    let next = &lines[asst_row + 1];
    assert!(
        !next.trim().is_empty(),
        "the answer must wrap at width 60:\n{}",
        lines.join("\n")
    );
    assert!(!next.contains('●'));
    assert_eq!(
        next.chars().take(4).collect::<String>(),
        "    ",
        "continuation aligns under the text: {next:?}"
    );
}

/// The shell header carries the identity and the main run's verdict, on one row.
#[test]
fn btw_header_shows_identity_and_main_verdict() {
    use leveler_client_protocol::{MessageId, RuntimeEvent};
    let mut s = btw_state("q", "a");
    s.status = RuntimeStatus::Busy;
    let m = MessageId::new("m1");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: m.clone(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: m.clone(),
            delta: "done".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: m }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert_eq!(s.status, RuntimeStatus::Idle);
    let lines = frame(&mut s, 100, 24);
    assert!(lines[0].contains("btw"), "identity header: {:?}", lines[0]);
    assert!(
        lines[0].contains("任务已完成"),
        "the main run's own verdict, not a guess: {:?}",
        lines[0]
    );
}

/// Esc belongs to the focused surface: it closes BTW and leaves the main run
/// exactly as it was.
#[test]
fn esc_in_btw_closes_the_surface_and_keeps_the_main_task_running() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    s.surface = SurfaceFocus::Btw;
    let effects = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
    );
    assert_eq!(
        s.surface,
        SurfaceFocus::Main,
        "Esc returns to the main surface"
    );
    assert_eq!(s.status, RuntimeStatus::Busy, "the main task keeps running");
    assert!(
        effects.is_empty(),
        "Esc in BTW must not reach the main cancel path: {effects:?}"
    );
    // Control: the same key on the main surface does cancel, so the empty
    // effect above is the surface consuming Esc, not the key doing nothing.
    let mut main = opened();
    main.status = RuntimeStatus::Busy;
    let cancel = reduce(
        &mut main,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
    );
    assert!(
        !cancel.is_empty(),
        "control: Esc on a busy main surface cancels"
    );
}

/// The shell degrades cleanly at every supported width: header, rules, content
/// and footer all render, and nothing panics.
#[test]
fn btw_shell_renders_at_narrow_widths() {
    for w in [160u16, 120, 100, 80, 54, 40, 24] {
        let mut s = btw_state("问题", "回答内容");
        let lines = frame(&mut s, w, 20);
        assert_eq!(lines.len(), 20, "w={w}");
        assert!(
            lines[0].contains('←'),
            "w={w}: identity header missing:\n{}",
            lines.join("\n")
        );
        assert!(
            lines.iter().any(|l| l.starts_with('─')),
            "w={w}: shell rule missing"
        );
        assert!(
            lines.iter().any(|l| l.contains("Esc")),
            "w={w}: footer hint missing:\n{}",
            lines.join("\n")
        );
    }
}

/// A full-screen page uses the same shell: page identity on top (no stacked
/// global app header), rules, padded body, and a real footer hint.
#[test]
fn secondary_pages_use_the_shared_shell() {
    let mut s = opened();
    s.active_screen = Screen::Help;
    let lines = frame(&mut s, 100, 24);
    assert!(
        lines[0].contains("← ") && lines[0].contains("帮助"),
        "identity header: {:?}",
        lines[0]
    );
    assert!(!lines[0].contains("CodeLeveler"), "no stacked app header");
    assert!(lines[1].starts_with('─'), "rule under the header");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("Esc") && l.contains("滚动")),
        "footer hint present:\n{}",
        lines.join("\n")
    );
}

/// The Context footer lists only bindings this view really answers.
#[test]
fn context_footer_lists_only_real_keys() {
    let mut s = opened();
    s.active_screen = Screen::Context;
    let lines = frame(&mut s, 100, 24);
    let footer = lines
        .iter()
        .rev()
        .find(|l| l.contains("Esc"))
        .expect("footer hint");
    assert!(
        footer.contains("Enter"),
        "Enter toggles disclosure: {footer}"
    );
    assert!(
        !footer.contains("r 刷新") && !footer.contains("r refresh"),
        "Context has no refresh binding: {footer}"
    );
}
