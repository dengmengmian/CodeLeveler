//! Slash-command syntax highlighting, driven through the real reducer and the
//! real frame.
//!
//! The unit tests own the rule (which token is a command) and the span split;
//! this contract owns what the terminal actually paints when keys arrive, what
//! the caret does while a command is being edited, and that the input buffer is
//! never rewritten by the paint. Colors are read back from `TestBackend` cells,
//! so a token that is styled in a `Line` but lost by the layout still fails.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use unicode_width::UnicodeWidthStr;

use leveler_client_protocol::SessionId;
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::{Theme, ThemeId};

/// `│` + inner pad (1) + `› ` prompt (2).
const COMPOSER_ORIGIN: u16 = 4;

fn boot(theme: Theme) -> AppState {
    AppState::new(
        theme,
        Boot {
            session_id: SessionId::new("s1"),
            user: "dogfood".into(),
            version: "0.0.0".into(),
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

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn type_text(state: &mut AppState, text: &str) {
    for ch in text.chars() {
        reduce(state, key(KeyCode::Char(ch)));
    }
}

struct Frame {
    term: Terminal<TestBackend>,
    caret: (u16, u16),
    origin: (u16, u16),
}

fn draw(state: &mut AppState, w: u16, h: u16) -> Frame {
    state.size = (w, h);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let caret = term
        .get_cursor_position()
        .map(|p| (p.x, p.y))
        .unwrap_or((0, 0));
    let (bx, by, _, _) = state.input_rect.expect("a frame publishes the composer");
    Frame {
        term,
        caret,
        origin: (bx + COMPOSER_ORIGIN, by + 1),
    }
}

impl Frame {
    fn fg(&self, col: u16, row: u16) -> Color {
        self.term
            .backend()
            .buffer()
            .cell((
                self.origin.0.saturating_add(col),
                self.origin.1.saturating_add(row),
            ))
            .map(|cell| cell.fg)
            .unwrap_or(Color::Reset)
    }

    fn row_text(&self, row: u16) -> String {
        let buf = self.term.backend().buffer();
        let mut out = String::new();
        for x in 0..buf.area.width {
            let cell = buf.cell((x, self.origin.1.saturating_add(row)));
            if let Some(cell) = cell {
                out.push_str(cell.symbol());
            }
        }
        out
    }
}

fn assert_command_ink(theme: &Theme, frame: &Frame, len: usize) {
    for col in 0..len {
        assert_eq!(
            frame.fg(col as u16, 0),
            theme.plan.primary,
            "command column {col} must be the soft violet"
        );
    }
}

fn assert_prose_ink(theme: &Theme, frame: &Frame, from: usize, to: usize) {
    for col in from..to {
        assert_eq!(
            frame.fg(col as u16, 0),
            theme.text.primary,
            "argument column {col} must stay prose"
        );
    }
}

#[test]
fn a_registered_command_paints_violet_in_both_themes() {
    for id in [ThemeId::Dark, ThemeId::Light] {
        let theme = Theme::named(id);
        let mut s = boot(theme.clone());
        type_text(&mut s, "/model deepseek-v4.1");
        let f = draw(&mut s, 80, 24);
        assert_command_ink(&theme, &f, "/model".len());
        assert_prose_ink(&theme, &f, "/model".len(), "/model deepseek-v4.1".len());
        assert_eq!(
            f.caret,
            (f.origin.0 + "/model deepseek-v4.1".len() as u16, f.origin.1),
            "{id}: the caret stays at the end of the typed text"
        );
        assert_eq!(s.composer.text(), "/model deepseek-v4.1");
    }
}

#[test]
fn the_highlight_follows_typing_without_enter() {
    let theme = Theme::named(ThemeId::Dark);
    let mut s = boot(theme.clone());

    // `/mo` is ambiguous between `/model` and `/mode`: not a command yet.
    type_text(&mut s, "/mo");
    let f = draw(&mut s, 80, 24);
    assert_prose_ink(&theme, &f, 0, 3);

    type_text(&mut s, "del");
    let f = draw(&mut s, 80, 24);
    assert_command_ink(&theme, &f, "/model".len());

    // The whole row repaints on each key: the frame never has to be re-entered.
    assert_eq!(s.composer.text(), "/model");
}

#[test]
fn a_unique_prefix_is_highlighted_before_it_is_complete() {
    let theme = Theme::named(ThemeId::Dark);
    let mut s = boot(theme.clone());
    type_text(&mut s, "/he");
    let f = draw(&mut s, 80, 24);
    assert_command_ink(&theme, &f, "/he".len());
}

#[test]
fn unknown_commands_stay_plain_text() {
    let theme = Theme::named(ThemeId::Dark);
    for text in ["/unknown-command", "/hlep", "/Users/me/project"] {
        let mut s = boot(theme.clone());
        type_text(&mut s, text);
        let f = draw(&mut s, 80, 24);
        assert!(
            (0..text.len()).all(|col| f.fg(col as u16, 0) != theme.plan.primary),
            "{text:?} must not be read as a command"
        );
    }
}

#[test]
fn ordinary_messages_and_shell_mode_are_untouched() {
    let theme = Theme::named(ThemeId::Dark);
    let mut s = boot(theme.clone());
    let text = "这个项目怎么本地跑起来？";
    type_text(&mut s, text);
    let f = draw(&mut s, 80, 24);
    let mut col = 0usize;
    for ch in text.chars() {
        assert_eq!(
            f.fg(col as u16, 0),
            theme.text.primary,
            "{ch:?} must keep the prose ink"
        );
        col += UnicodeWidthStr::width(ch.to_string().as_str()).max(1);
    }

    // `/` in Shell Mode is a filesystem path, not the command namespace.
    reduce(&mut s, key(KeyCode::Char('!')));
    type_text(&mut s, "/model");
    let f = draw(&mut s, 80, 24);
    assert_eq!(f.fg(0, 0), theme.text.primary);
}

#[test]
fn moving_inside_and_deleting_from_the_command_repaints_the_span() {
    let theme = Theme::named(ThemeId::Dark);
    let mut s = boot(theme.clone());
    type_text(&mut s, "/model");
    reduce(&mut s, key(KeyCode::Left));
    reduce(&mut s, key(KeyCode::Left));
    let f = draw(&mut s, 80, 24);
    assert_command_ink(&theme, &f, "/model".len());
    assert_eq!(
        f.caret,
        (f.origin.0 + 4, f.origin.1),
        "moving the caret must not move the highlight"
    );

    // Drop `l`: `/mode` is still a registered alias.
    reduce(&mut s, key(KeyCode::End));
    reduce(&mut s, key(KeyCode::Backspace));
    let f = draw(&mut s, 80, 24);
    assert_eq!(s.composer.text(), "/mode");
    assert_command_ink(&theme, &f, "/mode".len());

    // Drop `e`: `/mod` is ambiguous, so the color goes away immediately.
    reduce(&mut s, key(KeyCode::Backspace));
    let f = draw(&mut s, 80, 24);
    assert_eq!(s.composer.text(), "/mod");
    assert_prose_ink(&theme, &f, 0, "/mod".len());

    // Home + Delete removes the slash: an ordinary word, no highlight.
    reduce(&mut s, key(KeyCode::Home));
    reduce(&mut s, key(KeyCode::Delete));
    let f = draw(&mut s, 80, 24);
    assert_eq!(s.composer.text(), "mod");
    assert_prose_ink(&theme, &f, 0, 3);
}

#[test]
fn tab_completion_and_the_right_arrow_keep_the_colors_correct() {
    let theme = Theme::named(ThemeId::Dark);
    let mut s = boot(theme.clone());
    type_text(&mut s, "/mo");
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(s.composer.text(), "/model ", "Tab completes the command");
    let f = draw(&mut s, 80, 24);
    assert_command_ink(&theme, &f, "/model".len());
    assert_prose_ink(&theme, &f, "/model".len(), "/model ".len());

    // `→` accepts the inline next-step ghost, never the command; at the end of
    // a command it is a no-op and must not disturb the span or the caret.
    reduce(&mut s, key(KeyCode::Right));
    let f = draw(&mut s, 80, 24);
    assert_eq!(s.composer.text(), "/model ");
    assert_command_ink(&theme, &f, "/model".len());
    assert_eq!(f.caret, (f.origin.0 + "/model ".len() as u16, f.origin.1));
}

#[test]
fn the_argument_ghost_stays_grey_and_out_of_the_buffer() {
    let theme = Theme::named(ThemeId::Dark);
    let mut s = boot(theme.clone());
    type_text(&mut s, "/btw ");
    let f = draw(&mut s, 80, 24);
    assert_command_ink(&theme, &f, "/btw".len());
    // The ghost begins one cell after the trailing space, and is not text.
    let ghost = "<问题>";
    let mut col = "/btw ".len();
    for ch in ghost.chars() {
        assert_eq!(
            f.fg(col as u16, 0),
            theme.text.muted,
            "ghost {ch:?} must be muted"
        );
        col += UnicodeWidthStr::width(ch.to_string().as_str()).max(1);
    }
    assert_eq!(s.composer.text(), "/btw ");
}

#[test]
fn cjk_emoji_multiline_and_paste_keep_the_caret_on_the_text() {
    let theme = Theme::named(ThemeId::Dark);
    let mut s = boot(theme.clone());
    type_text(&mut s, "/model 你好🙂");
    let f = draw(&mut s, 80, 24);
    assert_command_ink(&theme, &f, "/model".len());
    assert_eq!(
        f.caret,
        (
            f.origin.0 + UnicodeWidthStr::width(s.composer.text()) as u16,
            f.origin.1
        ),
        "wide characters must not shift the caret"
    );

    // Explicit newline (Ctrl+J) keeps the command colored only on line 1.
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
    );
    type_text(&mut s, "第二行");
    let f = draw(&mut s, 80, 24);
    assert_command_ink(&theme, &f, "/model".len());
    assert_eq!(
        f.caret,
        (
            f.origin.0 + UnicodeWidthStr::width("第二行") as u16,
            f.origin.1 + 1
        ),
        "the caret follows the second line"
    );
    assert!(
        f.row_text(1).replace(' ', "").contains("第二行"),
        "{}",
        f.row_text(1)
    );

    // A bracketed paste takes the same insertion path; the paint is unchanged.
    let mut p = boot(theme.clone());
    reduce(&mut p, Action::Paste("/model 你好\n第二行".into()));
    let f = draw(&mut p, 80, 24);
    assert_eq!(p.composer.text(), "/model 你好\n第二行");
    assert_command_ink(&theme, &f, "/model".len());
    assert_eq!(
        f.caret,
        (
            f.origin.0 + UnicodeWidthStr::width("第二行") as u16,
            f.origin.1 + 1
        )
    );
}

#[test]
fn both_themes_keep_the_command_ink_readable_on_the_input_surface() {
    for id in [ThemeId::Dark, ThemeId::Light] {
        let theme = Theme::named(id);
        let ratio = leveler_tui::theme::contrast_ratio(theme.plan.primary, theme.surface.input)
            .expect("RGB themes have a contrast ratio");
        assert!(ratio >= 4.5, "{id}: command ink on input is {ratio:.2}:1");
    }
}
