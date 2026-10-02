//! Headless dogfood for TUI input geometry: where the insertion point actually
//! paints in terminal cells.
//!
//! A caret is not a view-model fact. It is the product of the real renderer's
//! wrapping, the display width it assigns each grapheme, and the terminal cursor
//! the frame places. A unit test on `wrap_with_caret` cannot see a renderer that
//! adds the prompt width twice, or a placeholder that leaks into the cursor
//! column — both of which shipped.
//!
//! So this drives the real reducer with real key events, renders through
//! `leveler_tui::render::render` into a `TestBackend`, and reads back the cursor
//! the frame placed with `get_cursor_position()`. That is exactly the cell a
//! terminal would put its cursor on: deterministic, no model, no network, no
//! font. It is the same seam `clarification_tabs.rs` and `prompt_suggestion.rs`
//! already use, applied as a standing acceptance contract.
//!
//! The oracle is independent of the code under test: expected columns are
//! computed by wrapping GRAPHEMES with `unicode-width` (the third-party
//! primitive), never by calling the production layout helper.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use leveler_client_protocol::{
    ClarificationId, ClarificationQuestionKind, RuntimeEvent, SessionId, UiClarificationQuestion,
    UiClarificationRequest,
};
use leveler_tui::action::Action;
use leveler_tui::overlay::{Overlay, SelectionModel, SelectionOption};
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

/// Where the editable buffer starts inside the composer box: `│` + inner pad
/// (1) + the `› ` prompt (2). The caret column is this origin plus the display
/// width of the buffer up to the cursor; the prompt is NOT added again.
const COMPOSER_TEXT_ORIGIN: u16 = 4;

fn boot(locale: leveler_tui::Locale) -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "dogfood".into(),
            version: "0.0.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    )
}

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn ctrl(c: char) -> Action {
    Action::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn type_text(state: &mut AppState, text: &str) {
    for ch in text.chars() {
        reduce(state, key(KeyCode::Char(ch)));
    }
}

/// One rendered frame: the painted characters and the cursor cell the frame
/// placed, both read from the backend.
struct Frame {
    screen: String,
    caret: (u16, u16),
}

fn draw(state: &mut AppState, w: u16, h: u16) -> Frame {
    state.size = (w, h);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state)).unwrap();
    let caret = term
        .get_cursor_position()
        .map(|p| (p.x, p.y))
        .unwrap_or((0, 0));
    let buf = term.backend().buffer();
    let mut screen = String::new();
    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            screen.push_str(sym);
            x += UnicodeWidthStr::width(sym).max(1) as u16;
        }
        screen.push('\n');
    }
    Frame { screen, caret }
}

impl Frame {
    fn row(&self, y: u16) -> String {
        self.screen
            .lines()
            .nth(y as usize)
            .unwrap_or("")
            .to_string()
    }

    fn diag(&self, state: &AppState, scenario: &str, expected: (u16, u16)) -> String {
        format!(
            "scenario: {scenario}\n\
             buffer: {:?}\n\
             logical grapheme cursor: {}\n\
             expected caret: {expected:?}\n\
             actual caret: {:?}\n\
             rendered row: {:?}\n\
             frame:\n{}",
            state.composer.text(),
            state.composer.cursor(),
            self.caret,
            self.row(self.caret.1),
            self.screen,
        )
    }
}

fn assert_caret(frame: &Frame, state: &AppState, scenario: &str, expected: (u16, u16)) {
    assert_eq!(
        frame.caret,
        expected,
        "{}",
        frame.diag(state, scenario, expected)
    );
}

/// Painted text up to (not including) a terminal column. Handles wide cells by
/// accumulating each character's display width.
fn before_column(row: &str, col: u16) -> String {
    let mut out = String::new();
    let mut used = 0u16;
    for ch in row.chars() {
        if used >= col {
            break;
        }
        out.push(ch);
        used += UnicodeWidthStr::width(ch.to_string().as_str()).max(1) as u16;
    }
    out
}

/// Independent grapheme-aware wrap, used only as the dogfood oracle. Returns
/// visual rows for `text` at `room` columns.
fn oracle_rows(text: &str, room: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    let mut used = 0usize;
    for g in text.graphemes(true) {
        let gw = UnicodeWidthStr::width(g);
        if used > 0 && used + gw > room {
            rows.push(String::new());
            used = 0;
        }
        rows.last_mut().unwrap().push_str(g);
        used += gw;
    }
    rows
}

/// `(visual row, display column within that row)` for a cursor at grapheme
/// index `cursor` of `text`, wrapped at `room` columns.
fn oracle_caret(text: &str, cursor: usize, room: usize) -> (u16, u16) {
    let mut row = 0usize;
    let mut col = 0usize;
    for (index, g) in text.graphemes(true).enumerate() {
        if index == cursor {
            break;
        }
        let gw = UnicodeWidthStr::width(g);
        if col > 0 && col + gw > room {
            row += 1;
            col = 0;
        }
        col += gw;
    }
    (row as u16, col as u16)
}

/// Expected composer caret for the current buffer and the composer box the last
/// frame published.
fn expected_composer_caret(state: &AppState, w: u16, h: u16) -> (u16, u16) {
    let (bx, by, bw, _) = state
        .input_rect
        .expect("a rendered frame publishes the composer rect");
    // interior width = box - 2 borders; content = interior - 1 inner pad;
    // per-row room = content - 2 prompt columns.
    let room = (bw as usize).saturating_sub(5).max(1);
    let (rel_row, col) =
        oracle_caret(state.composer.text(), state.composer.cursor(), room);
    let _ = (w, h);
    (bx + COMPOSER_TEXT_ORIGIN + col, by + 1 + rel_row)
}

// ── Main Composer ────────────────────────────────────────────────────────────

#[test]
fn dogfood_empty_composer_caret_sits_before_the_placeholder() {
    for (locale, width) in [
        (leveler_tui::Locale::Zh, 80u16),
        (leveler_tui::Locale::Zh, 48),
        (leveler_tui::Locale::Zh, 24),
        (leveler_tui::Locale::En, 80),
        (leveler_tui::Locale::En, 48),
    ] {
        let mut s = boot(locale);
        let f = draw(&mut s, width, 20);
        let (bx, by, _, _) = s.input_rect.expect("composer rect");
        let expected = (bx + COMPOSER_TEXT_ORIGIN, by + 1);
        assert_caret(&f, &s, &format!("empty placeholder w={width} {locale:?}"), expected);
        assert!(s.composer.is_empty(), "the placeholder is not the buffer");
        // The caret is immediately after the prompt, before the placeholder:
        // everything before it trims back to the `›` glyph.
        let before = before_column(&f.row(f.caret.1), f.caret.0);
        assert!(
            before.trim_end().ends_with('›'),
            "caret must sit between the prompt and the hint: {before:?}\n{}",
            f.screen
        );
    }
}

#[test]
fn dogfood_ascii_caret_tracks_left_right_home_end() {
    let mut s = boot(leveler_tui::Locale::Zh);
    type_text(&mut s, "abc");
    let mut f = draw(&mut s, 80, 20);
    assert_caret(&f, &s, "ascii end", expected_composer_caret(&s, 80, 20));

    for (action, scenario) in [
        (key(KeyCode::Left), "ascii left"),
        (key(KeyCode::Left), "ascii left x2"),
        (key(KeyCode::Right), "ascii right"),
        (key(KeyCode::Home), "ascii home"),
        (key(KeyCode::End), "ascii end again"),
    ] {
        reduce(&mut s, action);
        f = draw(&mut s, 80, 20);
        assert_caret(&f, &s, scenario, expected_composer_caret(&s, 80, 20));
    }
}

#[test]
fn dogfood_cjk_caret_tracks_graphemes_and_wide_cells() {
    for text in ["中", "中文", "这是什么意思"] {
        let mut s = boot(leveler_tui::Locale::Zh);
        type_text(&mut s, text);
        let mut f = draw(&mut s, 80, 20);
        assert_caret(
            &f,
            &s,
            &format!("cjk end {text:?}"),
            expected_composer_caret(&s, 80, 20),
        );
        // The caret is exactly after the last painted grapheme and never inside
        // a wide glyph: for pure CJK the offset from the text origin is even.
        let (bx, _, _, _) = s.input_rect.unwrap();
        assert_eq!(
            (f.caret.0 - bx - COMPOSER_TEXT_ORIGIN) % 2,
            0,
            "caret split a wide glyph for {text:?}: {:?}",
            f.caret
        );
        // Backspace deletes one grapheme and the caret follows it.
        reduce(&mut s, key(KeyCode::Backspace));
        f = draw(&mut s, 80, 20);
        assert_caret(
            &f,
            &s,
            &format!("cjk backspace {text:?}"),
            expected_composer_caret(&s, 80, 20),
        );
        assert_eq!(
            UnicodeSegmentation::graphemes(s.composer.text(), true).count() + 1,
            UnicodeSegmentation::graphemes(text, true).count().max(1),
            "backspace removed exactly one grapheme"
        );

        // Home / End are line-aware and land on legal columns.
        reduce(&mut s, key(KeyCode::Home));
        f = draw(&mut s, 80, 20);
        assert_caret(
            &f,
            &s,
            &format!("cjk home {text:?}"),
            expected_composer_caret(&s, 80, 20),
        );
        reduce(&mut s, key(KeyCode::End));
        f = draw(&mut s, 80, 20);
        assert_caret(
            &f,
            &s,
            &format!("cjk end-again {text:?}"),
            expected_composer_caret(&s, 80, 20),
        );
    }
}

#[test]
fn dogfood_mixed_cjk_ascii_caret_never_drifts() {
    let mut s = boot(leveler_tui::Locale::Zh);
    type_text(&mut s, "abc中文def");
    let mut f = draw(&mut s, 80, 20);
    assert_caret(&f, &s, "mixed end", expected_composer_caret(&s, 80, 20));

    // Walk left across the ASCII/CJK boundary; each step is one grapheme and
    // the oracle (grapheme + unicode-width) must agree at every stop.
    for step in 0..8 {
        reduce(&mut s, key(KeyCode::Left));
        f = draw(&mut s, 80, 20);
        assert_caret(
            &f,
            &s,
            &format!("mixed left {step}"),
            expected_composer_caret(&s, 80, 20),
        );
    }
    for step in 0..8 {
        reduce(&mut s, key(KeyCode::Right));
        f = draw(&mut s, 80, 20);
        assert_caret(
            &f,
            &s,
            &format!("mixed right {step}"),
            expected_composer_caret(&s, 80, 20),
        );
    }
}

#[test]
fn dogfood_combining_mark_is_one_grapheme_and_one_cell() {
    let mut s = boot(leveler_tui::Locale::Zh);
    type_text(&mut s, "e\u{301}");
    assert_eq!(s.composer.len(), 1, "e + U+0301 is one grapheme");
    let mut f = draw(&mut s, 80, 20);
    assert_caret(&f, &s, "combining end", expected_composer_caret(&s, 80, 20));

    // Left enters the cluster, Right leaves it, and neither leaves the cursor
    // between the two code points as a separate stop.
    reduce(&mut s, key(KeyCode::Left));
    assert_eq!(s.composer.cursor(), 0, "one grapheme: one step back");
    f = draw(&mut s, 80, 20);
    assert_caret(&f, &s, "combining start", expected_composer_caret(&s, 80, 20));
    reduce(&mut s, key(KeyCode::Right));
    assert_eq!(s.composer.cursor(), 1);
    reduce(&mut s, key(KeyCode::Backspace));
    assert!(s.composer.is_empty(), "backspace removes the whole cluster");
    f = draw(&mut s, 80, 20);
    assert_caret(&f, &s, "combining cleared", expected_composer_caret(&s, 80, 20));
}

#[test]
fn dogfood_emoji_modifier_and_zwj_caret_stay_consistent() {
    for text in ["👍\u{1f3fd}", "👨\u{200d}👩\u{200d}👧\u{200d}👦"] {
        let mut s = boot(leveler_tui::Locale::Zh);
        type_text(&mut s, text);
        assert_eq!(s.composer.len(), 1, "{text:?} is one user-visible grapheme");
        let mut f = draw(&mut s, 80, 20);
        // The contract is not a fixed cell count: it is that the cursor column
        // equals the width the renderer's own primitive assigns the cluster.
        let (bx, by, _, _) = s.input_rect.unwrap();
        let expected = (
            bx + COMPOSER_TEXT_ORIGIN + UnicodeWidthStr::width(text) as u16,
            by + 1,
        );
        assert_caret(&f, &s, &format!("emoji end {text:?}"), expected);
        reduce(&mut s, key(KeyCode::Backspace));
        assert!(s.composer.is_empty(), "one grapheme, one backspace");
        f = draw(&mut s, 80, 20);
        assert_caret(
            &f,
            &s,
            &format!("emoji cleared {text:?}"),
            (bx + COMPOSER_TEXT_ORIGIN, by + 1),
        );
    }
}

#[test]
fn dogfood_clearing_a_long_cjk_draft_returns_the_caret_to_the_prompt() {
    let mut s = boot(leveler_tui::Locale::Zh);
    type_text(&mut s, "这是一个很长的中文草稿");
    let f = draw(&mut s, 80, 20);
    assert!(f.caret.0 > s.input_rect.unwrap().0 + COMPOSER_TEXT_ORIGIN);

    reduce(&mut s, ctrl('u'));
    assert!(s.composer.is_empty(), "Ctrl+U clears the line");
    let f = draw(&mut s, 80, 20);
    let (bx, by, _, _) = s.input_rect.unwrap();
    assert_caret(
        &f,
        &s,
        "cleared to placeholder",
        (bx + COMPOSER_TEXT_ORIGIN, by + 1),
    );
    let before = before_column(&f.row(f.caret.1), f.caret.0);
    assert!(
        before.trim_end().ends_with('›'),
        "cleared caret sits before the re-appeared placeholder: {before:?}"
    );
}

// ── Soft wrap and resize ─────────────────────────────────────────────────────

#[test]
fn dogfood_soft_wrap_cjk_keeps_caret_on_a_legal_cell() {
    // Narrow enough that a 12-grapheme CJK draft wraps several times. The
    // per-row room is read from the composer rect the frame published, and the
    // expected rows come from the grapheme/unicode-width oracle.
    let text = "中文中文中文中文中文中文"; // 12 graphemes, width 24
    let mut s = boot(leveler_tui::Locale::Zh);
    type_text(&mut s, text);
    let mut f = draw(&mut s, 15, 20);
    let (bx, _by, bw, _) = s.input_rect.unwrap();
    let room = (bw as usize).saturating_sub(5);
    let rows = oracle_rows(text, room);
    assert!(rows.len() >= 2, "a narrow composer must wrap: {rows:?}");

    // End of the buffer: last visual row, immediately after the final glyph.
    assert_caret(&f, &s, "wrap cjk end", expected_composer_caret(&s, 15, 20));
    assert_eq!(
        (f.caret.0 - bx - COMPOSER_TEXT_ORIGIN) % 2,
        0,
        "caret split a wide glyph: {:?}",
        f.caret
    );

    // A caret exactly on the wrap boundary with text remaining belongs to the
    // next row, where the rest of the line starts.
    let first_len = rows[0].graphemes(true).count();
    let total = text.graphemes(true).count();
    for _ in 0..(total - first_len) {
        reduce(&mut s, key(KeyCode::Left));
    }
    assert_eq!(s.composer.cursor(), first_len, "end of the first visual row");
    f = draw(&mut s, 15, 20);
    let (bx, by, _, _) = s.input_rect.unwrap();
    assert_eq!(
        f.caret,
        (bx + COMPOSER_TEXT_ORIGIN, by + 2),
        "{}",
        f.diag(&s, "wrap boundary", (bx + COMPOSER_TEXT_ORIGIN, by + 2))
    );

    // Stepping onto the continuation row keeps the glyph's two cells: the
    // caret column stays even and the row opens with a whole CJK character.
    reduce(&mut s, key(KeyCode::Right));
    f = draw(&mut s, 15, 20);
    assert_eq!(f.caret.1, by + 2, "caret stays on the continuation row");
    assert_eq!(
        (f.caret.0 - bx - COMPOSER_TEXT_ORIGIN) % 2,
        0,
        "continuation caret split a wide glyph: {:?}",
        f.caret
    );
    let row = before_column(&f.row(f.caret.1), f.caret.0);
    assert!(
        row.ends_with('文'),
        "continuation row opens with a whole glyph: {row:?}"
    );
}

#[test]
fn dogfood_terminal_resize_preserves_buffer_cursor_and_geometry() {
    let mut s = boot(leveler_tui::Locale::Zh);
    type_text(&mut s, "abc这是中文def");
    let buffer_before = s.composer.text().to_string();
    let cursor_before = s.composer.cursor();

    for width in [24u16, 14, 24] {
        let f = draw(&mut s, width, 20);
        assert_eq!(s.composer.text(), buffer_before, "resize changed the buffer");
        assert_eq!(s.composer.cursor(), cursor_before, "resize changed the cursor");
        assert_caret(
            &f,
            &s,
            &format!("resize w={width}"),
            expected_composer_caret(&s, width, 20),
        );
    }
}

// ── Paste chip ───────────────────────────────────────────────────────────────

#[test]
fn dogfood_large_paste_chip_keeps_caret_on_the_chip() {
    let mut s = boot(leveler_tui::Locale::Zh);
    type_text(&mut s, "请分析：");
    let pasted: String = (0..977)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    reduce(&mut s, Action::Paste(pasted.clone()));
    assert!(
        s.composer.text().contains("[Pasted: 977 lines]"),
        "a large paste becomes a chip: {:?}",
        s.composer.text()
    );
    let mut f = draw(&mut s, 80, 20);
    assert_caret(&f, &s, "paste chip end", expected_composer_caret(&s, 80, 20));

    // Typing after the chip keeps the caret after the real editable content.
    type_text(&mut s, "，重点看这里");
    f = draw(&mut s, 80, 20);
    assert_caret(&f, &s, "paste chip + cjk", expected_composer_caret(&s, 80, 20));
    // The chip is presentation; the canonical payload is still every line.
    let canonical = s.composer.canonical_text();
    assert!(canonical.contains(&pasted), "canonical payload lost");
}

// ── Clarification field ──────────────────────────────────────────────────────

fn open_text_question(s: &mut AppState) {
    let request = UiClarificationRequest {
        id: ClarificationId::new("c1"),
        question: "需要澄清".into(),
        options: Vec::new(),
        questions: vec![UiClarificationQuestion {
            header: "未登录态".into(),
            question: "未登录态怎么处理".into(),
            kind: ClarificationQuestionKind::Text,
            options: Vec::new(),
            allow_other: false,
            min_choices: 0,
            max_choices: None,
        }],
    };
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ClarificationRequested { request }),
    );
}

#[test]
fn dogfood_clarification_cjk_field_caret_follows_typed_text() {
    let mut s = boot(leveler_tui::Locale::Zh);
    open_text_question(&mut s);
    type_text(&mut s, "这是什么意思");
    let f = draw(&mut s, 80, 30);
    let before = before_column(&f.row(f.caret.1), f.caret.0);
    assert!(
        before.trim_end().ends_with("> 这是什么意思"),
        "clarification caret must sit right after the typed CJK: {before:?}\n{}",
        f.screen
    );
}

#[test]
fn dogfood_clarification_cleared_field_caret_returns_to_the_prompt() {
    let mut s = boot(leveler_tui::Locale::Zh);
    open_text_question(&mut s);
    type_text(&mut s, "中文答案");
    reduce(&mut s, ctrl('u'));
    assert!(s.composer.is_empty(), "the live draft is untouched by the field");
    let f = draw(&mut s, 80, 30);
    let before = before_column(&f.row(f.caret.1), f.caret.0);
    assert!(
        before.trim_end().ends_with(">"),
        "an empty clarification field puts the caret after its prompt: {before:?}"
    );
}

// ── Selection query ──────────────────────────────────────────────────────────

fn open_searchable_picker(s: &mut AppState) {
    let model = SelectionModel::new(
        "选择模型",
        vec![
            SelectionOption::new("m1", "deepseek/v3"),
            SelectionOption::new("m2", "中文模型"),
        ],
        true,
    );
    s.overlay = Some(Overlay::ModelPicker(Box::new(model)));
}

#[test]
fn dogfood_selection_query_cjk_caret_follows_the_query() {
    for query in ["中", "中文", "abc中文"] {
        let mut s = boot(leveler_tui::Locale::Zh);
        open_searchable_picker(&mut s);
        type_text(&mut s, query);
        let f = draw(&mut s, 80, 30);
        let before = before_column(&f.row(f.caret.1), f.caret.0);
        assert!(
            before.trim_end().ends_with(query),
            "search caret must sit after {query:?}: {before:?}\n{}",
            f.screen
        );
    }
}
