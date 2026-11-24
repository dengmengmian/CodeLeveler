//! P5 overlay-surface closure: the conversation keeps its inline decision
//! card; independent pages and surfaces get one framed modal; a modal owns the
//! pointer; and a very short side-thread terminal keeps its input usable.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use leveler_client_protocol::SessionId;
use leveler_client_protocol::UiClarificationRequest;
use leveler_tui::action::Action;
use leveler_tui::btw::{BtwTurnState, SurfaceFocus};
use leveler_tui::overlay::{
    ClarificationOverlay, Overlay, SelectionModel, SelectionOption, render_modal_overlay,
};
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

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
            model_notice: None,
            thinking: None,
        },
    )
}

fn mode_overlay() -> Overlay {
    let model = SelectionModel::new(
        "选择权限模式",
        vec![
            SelectionOption::new("ask", "ask").description("执行前确认"),
            SelectionOption::new("auto", "auto")
                .description("自动执行")
                .current(true),
            SelectionOption::new("full", "full").description("完全访问"),
        ],
        false,
    );
    Overlay::ModePicker(Box::new(model))
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
                x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            }
            line
        })
        .collect()
}

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

#[test]
fn conversation_picker_stays_inline_and_unframed() {
    let mut s = opened();
    s.overlay = Some(mode_overlay());
    let lines = frame(&mut s, 120, 40);
    let title_row = lines
        .iter()
        .position(|line| line.contains("选择权限模式"))
        .expect("picker title");
    assert!(
        title_row > 24,
        "inline overlay belongs in the composer slot, not the middle of the page: {title_row}\n{}",
        lines.join("\n")
    );
    assert!(
        !lines[title_row].contains('┌') && !lines[title_row].contains('│'),
        "inline overlay must stay unframed: {:?}",
        lines[title_row]
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("选择权限模式"))
            .count(),
        1
    );
    assert!(lines.iter().any(|line| line.contains("↑↓ 移动")));
}

#[test]
fn secondary_picker_is_one_framed_modal_with_its_title_once() {
    let mut s = opened();
    s.active_screen = Screen::Settings;
    s.overlay = Some(mode_overlay());
    let lines = frame(&mut s, 120, 40);
    let title_row = lines
        .iter()
        .position(|line| line.contains("选择权限模式"))
        .unwrap_or_else(|| panic!("modal title:\n{}", lines.join("\n")));
    let line = &lines[title_row];
    assert!(
        line.contains('┌') && line.contains('┐'),
        "title is in the frame: {line:?}"
    );
    let modal_x = line.find('┌').unwrap();
    assert!(modal_x > 0, "modal is inset in its host: {line:?}");
    assert!(line.contains("选择权限模式"));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("选择权限模式"))
            .count(),
        1,
        "the frame owns the title; the inline first row must not repeat it:\n{}",
        lines.join("\n")
    );
    let body = lines[title_row..]
        .iter()
        .find(|line| line.trim_start().starts_with('│'))
        .expect("modal body border");
    assert!(body.contains('│'));
    for option in ["ask", "auto", "full"] {
        assert!(
            lines[title_row..].iter().any(|line| line.contains(option)),
            "option {option} missing from the modal:\n{}",
            lines.join("\n")
        );
    }
    assert!(
        lines[title_row..]
            .iter()
            .any(|line| line.contains("↑↓ 移动"))
    );
}

#[test]
fn secondary_clarification_names_its_decision_once_and_keeps_the_question() {
    let mut s = opened();
    s.active_screen = Screen::Settings;
    s.overlay = Some(Overlay::Clarification(Box::new(ClarificationOverlay::new(
        UiClarificationRequest::single(
            leveler_client_protocol::ClarificationId::new("c1"),
            "选择发布策略",
            vec!["灰度发布".into(), "全量发布".into()],
        ),
    ))));
    let lines = frame(&mut s, 120, 40);
    let title = "需要澄清";
    assert_eq!(
        lines.iter().filter(|line| line.contains(title)).count(),
        1,
        "the frame owns the heading; the prompt row must not repeat it:\n{}",
        lines.join("\n")
    );
    assert!(
        lines.iter().any(|line| line.contains("选择发布策略")),
        "the live question survives:\n{}",
        lines.join("\n")
    );
    assert!(lines.iter().any(|line| line.contains("灰度发布")));
}

#[test]
fn secondary_modal_keeps_its_identity_and_keys_across_supported_sizes() {
    for (w, h) in [
        (120u16, 40u16),
        (100, 30),
        (80, 24),
        (60, 20),
        (40, 16),
        (40, 8),
        (40, 4),
    ] {
        let mut s = opened();
        s.active_screen = Screen::Settings;
        s.overlay = Some(mode_overlay());
        let lines = frame(&mut s, w, h);
        assert_eq!(lines.len(), h as usize, "{w}x{h}");
        assert!(
            lines
                .iter()
                .any(|line| line.contains('┌') && line.contains("选择权限模式")),
            "{w}x{h}: title frame missing:\n{}",
            lines.join("\n")
        );
        assert!(
            lines.iter().any(|line| line.contains("ask")),
            "{w}x{h}: the focused choice must survive:\n{}",
            lines.join("\n")
        );
        if h >= 8 {
            assert!(
                lines.iter().any(|line| line.contains("↑↓ 移动")),
                "{w}x{h}: key hint must survive:\n{}",
                lines.join("\n")
            );
        }
        assert!(
            lines.iter().any(|line| line.contains('│')),
            "{w}x{h}: modal body missing:\n{}",
            lines.join("\n")
        );
    }
}

#[test]
fn a_host_shorter_than_three_rows_still_shows_the_modal_identity() {
    for h in [2u16, 1] {
        let overlay = mode_overlay();
        let mut term = Terminal::new(TestBackend::new(40, h)).unwrap();
        term.draw(|f| {
            render_modal_overlay(
                f,
                Rect::new(0, 0, 40, h),
                &overlay,
                &Theme::no_color(),
                leveler_tui::Locale::Zh,
            )
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..h {
            for x in 0..40 {
                text.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            text.push('\n');
        }
        assert!(
            text.split_whitespace()
                .collect::<String>()
                .contains("选择权限模式"),
            "h={h}: {text}"
        );
        if h == 2 {
            assert!(text.contains("ask"), "h={h}: {text}");
        }
    }
}

#[test]
fn secondary_modal_owns_the_pointer_and_escape_closes_only_the_modal() {
    let mut s = opened();
    s.active_screen = Screen::Help;
    let _ = frame(&mut s, 100, 30);
    let back = s.back_hit.expect("help publishes its back hit");
    // Control: with no overlay the page's own back row really leaves it.
    reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: back.1,
            row: back.0,
            modifiers: KeyModifiers::empty(),
        }),
    );
    assert_eq!(s.active_screen, Screen::Conversation);

    let mut s = opened();
    s.active_screen = Screen::Help;
    s.overlay = Some(mode_overlay());
    let _ = frame(&mut s, 100, 30);
    let back = s.back_hit.expect("the page still publishes its geometry");
    let effects = reduce(
        &mut s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: back.1,
            row: back.0,
            modifiers: KeyModifiers::empty(),
        }),
    );
    assert!(effects.is_empty());
    assert_eq!(
        s.active_screen,
        Screen::Help,
        "a click behind a secondary modal must not reach the page"
    );
    assert!(matches!(s.overlay, Some(Overlay::ModePicker(_))));

    // Esc belongs to the modal first; it answers no page navigation.
    reduce(&mut s, key(KeyCode::Esc));
    assert!(s.overlay.is_none());
    assert_eq!(s.active_screen, Screen::Help);
    // The page still has its own Esc afterwards.
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn btw_surface_does_not_host_the_main_decision_and_keeps_its_escape() {
    let mut s = opened();
    s.surface = SurfaceFocus::Btw;
    s.btw.begin("q".into());
    s.btw.append("a");
    s.btw.finish(BtwTurnState::Done, None);
    s.overlay = Some(mode_overlay());
    let lines = frame(&mut s, 80, 24);
    assert!(
        !lines.iter().any(|line| line.contains("选择权限模式")),
        "a main-surface decision waits for the main surface; the side thread must not paint a modal it does not own:\n{}",
        lines.join("\n")
    );
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(
        s.surface,
        SurfaceFocus::Main,
        "Esc belongs to the side surface while it is focused"
    );
    assert!(
        matches!(s.overlay, Some(Overlay::ModePicker(_))),
        "leaving BTW is navigation and must not answer the pending decision"
    );
    // Back on the main surface the same decision is the inline card and owns
    // Escape; closing it does not leave the conversation.
    let lines = frame(&mut s, 80, 24);
    assert!(lines.iter().any(|line| line.contains("选择权限模式")));
    reduce(&mut s, key(KeyCode::Esc));
    assert!(s.overlay.is_none());
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn btw_short_terminal_keeps_answer_input_and_escape() {
    for h in [8u16, 6, 4, 3] {
        let mut s = opened();
        s.surface = SurfaceFocus::Btw;
        s.btw.begin("问题".into());
        s.btw.append("回答内容");
        s.btw.finish(BtwTurnState::Done, None);
        let lines = frame(&mut s, 40, h);
        assert_eq!(lines.len(), h as usize, "h={h}");
        assert!(
            lines.iter().any(|line| line.contains("回答内容")),
            "h={h}: answer body vanished:\n{}",
            lines.join("\n")
        );
        assert!(
            lines.iter().any(|line| line.contains("Esc")),
            "h={h}: footer vanished:\n{}",
            lines.join("\n")
        );
        assert!(
            lines.iter().any(|line| line.contains("输入消息")),
            "h={h}: the input row must survive the compact form:\n{}",
            lines.join("\n")
        );
        assert!(
            s.input_rect.is_some_and(|(_, _, w, ch)| w > 0 && ch > 0),
            "h={h}: composer geometry must stay usable"
        );
    }
    // Two rows cannot hold both an answer and the input. The input and the
    // exit hint are the parts that must survive.
    let mut s = opened();
    s.surface = SurfaceFocus::Btw;
    s.btw.begin("问题".into());
    s.btw.append("回答内容");
    s.btw.finish(BtwTurnState::Done, None);
    let lines = frame(&mut s, 40, 2);
    assert!(lines.iter().any(|line| line.contains("Esc")), "{:?}", lines);
    assert!(
        lines.iter().any(|line| line.contains("输入消息")),
        "{:?}",
        lines
    );
    assert!(
        s.input_rect.is_some_and(|(_, _, w, ch)| w > 0 && ch > 0),
        "{:?}",
        lines
    );
}

#[test]
fn memory_overlay_owns_escape_and_leaves_the_memory_page_open() {
    let mut s = opened();
    s.active_screen = Screen::Memory;
    s.overlay = Some(mode_overlay());
    let lines = frame(&mut s, 100, 30);
    assert!(
        lines
            .iter()
            .any(|line| line.contains('┌') && line.contains("选择权限模式")),
        "a decision over Memory uses the framed form:\n{}",
        lines.join("\n")
    );
    reduce(&mut s, key(KeyCode::Esc));
    assert!(s.overlay.is_none());
    assert_eq!(
        s.active_screen,
        Screen::Memory,
        "Esc closes the overlay first"
    );
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn memory_surface_is_still_the_live_product_page() {
    let mut s = opened();
    s.composer.replace("/memory");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.active_screen, Screen::Memory);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, leveler_tui::action::Effect::Send(_))),
        "the page asks the runtime for its real counters: {effects:?}"
    );
    let lines = frame(&mut s, 100, 30);
    assert!(
        lines.iter().any(|line| line.contains('│')),
        "the active memory page still renders its own framed surface:\n{}",
        lines.join("\n")
    );
}
