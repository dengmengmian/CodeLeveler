//! Composer editing tests: Unicode graphemes, multiline, paste, history (§69.2).

use leveler_tui::composer::Composer;

#[test]
fn typing_replaces_a_prefilled_suggestion() {
    let mut composer = Composer::new();
    composer.replace_suggestion("运行完整测试");

    composer.insert_str("/help");

    assert_eq!(composer.text(), "/help");
}

#[test]
fn submitting_without_editing_accepts_the_prefilled_suggestion() {
    let mut composer = Composer::new();
    composer.replace_suggestion("继续");

    assert_eq!(composer.take(), "继续");
    assert!(composer.is_empty());
}

#[test]
fn inserts_cjk_by_grapheme() {
    let mut c = Composer::new();
    c.insert_str("你好");
    assert_eq!(c.text(), "你好");
    assert_eq!(c.len(), 2, "two graphemes, not six bytes");
    assert_eq!(c.cursor(), 2);
}

#[test]
fn cursor_moves_over_cjk_not_bytes() {
    let mut c = Composer::new();
    c.insert_str("你好");
    c.move_left();
    assert_eq!(c.cursor(), 1);
    c.insert_char('x');
    assert_eq!(c.text(), "你x好");
}

#[test]
fn backspace_deletes_one_emoji_grapheme() {
    let mut c = Composer::new();
    c.insert_str("a👍b");
    c.backspace(); // remove 'b'
    assert_eq!(c.text(), "a👍");
    c.backspace(); // remove the emoji as one unit
    assert_eq!(c.text(), "a");
}

#[test]
fn display_width_accounts_for_fullwidth() {
    let mut c = Composer::new();
    c.insert_str("你"); // one grapheme, two display columns
    let (row, col) = c.cursor_row_col_display();
    assert_eq!(row, 0);
    assert_eq!(col, 2);
}

#[test]
fn multiline_line_count_and_cursor_position() {
    let mut c = Composer::new();
    c.insert_str("ab");
    c.newline();
    c.insert_str("cd");
    assert_eq!(c.line_count(), 2);
    assert_eq!(c.lines(), vec!["ab", "cd"]);
    let (row, col) = c.cursor_row_col_display();
    assert_eq!((row, col), (1, 2));
}

#[test]
fn home_and_end_are_line_aware() {
    let mut c = Composer::new();
    c.insert_str("ab\ncd");
    c.move_to_line_start();
    assert_eq!(c.cursor(), 3, "start of the 'cd' line");
    c.move_to_line_end();
    assert_eq!(c.cursor(), 5);
}

#[test]
fn kill_to_line_end_and_start() {
    let mut c = Composer::new();
    c.insert_str("hello");
    c.move_to_line_start();
    c.move_right();
    c.move_right();
    c.kill_to_line_end();
    assert_eq!(c.text(), "he");
    c.kill_to_line_start();
    assert_eq!(c.text(), "");
}

#[test]
fn delete_word_back_removes_last_word() {
    let mut c = Composer::new();
    c.insert_str("hello world");
    c.delete_word_back();
    assert_eq!(c.text(), "hello ");
}

#[test]
fn paste_normalizes_crlf() {
    let mut c = Composer::new();
    c.insert_str("a\r\nb\rc");
    assert_eq!(c.text(), "a\nb\nc");
    assert_eq!(c.line_count(), 3);
}

#[test]
fn short_paste_inserts_text_directly() {
    let mut c = Composer::new();

    c.insert_paste("hello\r\nworld");

    assert_eq!(c.text(), "hello\nworld");
    assert_eq!(c.take(), "hello\nworld");
}

#[test]
fn large_paste_uses_placeholder_and_expands_on_take() {
    let mut c = Composer::new();
    // 5+ lines hits the placeholder threshold.
    let pasted = (0..6)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");

    c.insert_str("before ");
    c.insert_paste(&pasted);
    c.insert_str(" after");

    assert!(
        c.text().contains("[Pasted: 6 lines]"),
        "composer: {}",
        c.text()
    );
    assert!(!c.text().contains("line 5"));
    assert_eq!(c.take(), format!("before {pasted} after"));
    assert_eq!(c.history(), &[format!("before {pasted} after")]);
}

#[test]
fn five_line_paste_collapses_to_chip() {
    let mut c = Composer::new();
    let pasted = "a\nb\nc\nd\ne";
    c.insert_paste(pasted);
    assert_eq!(c.text(), "[Pasted: 5 lines]");
    assert_eq!(c.take(), pasted);
}

#[test]
fn take_records_history_and_clears() {
    let mut c = Composer::new();
    c.insert_str("one");
    assert_eq!(c.take(), "one");
    assert!(c.is_empty());

    c.insert_str("two");
    assert_eq!(c.take(), "two");

    // Up browses back through history (single-line buffer).
    c.up();
    assert_eq!(c.text(), "two");
    c.up();
    assert_eq!(c.text(), "one");
    c.down();
    assert_eq!(c.text(), "two");
    c.down();
    assert_eq!(c.text(), "", "past newest restores the (empty) draft");
}

#[test]
fn history_skips_consecutive_duplicates_and_empties() {
    let mut c = Composer::new();
    c.insert_str("x");
    c.take();
    c.insert_str("x");
    c.take();
    c.take(); // empty
    // Only one "x" recorded; up() twice stays on "x".
    c.up();
    assert_eq!(c.text(), "x");
    c.up();
    assert_eq!(c.text(), "x");
}

#[test]
fn multiline_up_down_move_cursor_not_history() {
    let mut c = Composer::new();
    c.insert_str("abc\nxy");
    // cursor at end (row 1, col 2). Up should land on row 0, clamped column.
    c.up();
    let (row, _) = c.cursor_row_col_display();
    assert_eq!(row, 0);
}
