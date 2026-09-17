//! Composer editing tests: Unicode graphemes, multiline, paste, history (§69.2).

use leveler_tui::composer::Composer;

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

#[test]
fn canonical_text_expands_without_consuming() {
    let mut c = Composer::default();
    c.insert_str("/goal ");
    let paste = "a\nb\nc\nd\ne\nf";
    c.insert_paste(paste);
    let canonical = c.canonical_text();
    assert_eq!(canonical, format!("/goal {paste}"));
    // Non-destructive: presentation and pending pastes are untouched…
    assert!(c.text().contains("[Pasted: 6 lines]"), "{}", c.text());
    // …and take() still returns the same canonical content afterwards.
    assert_eq!(c.take(), canonical);
    assert!(c.is_empty());
}

#[test]
fn canonical_text_without_pastes_is_the_buffer_verbatim() {
    let mut c = Composer::default();
    c.insert_str("[Pasted: 5 lines]");
    assert_eq!(c.canonical_text(), "[Pasted: 5 lines]");
}

// ---- Image tokens ---------------------------------------------------------
//
// An image is a first-class attachment, and `[图片 #1]` is where the user put
// it in their sentence. The token lives in the buffer as one atomic unit: the
// k-th token names the k-th staged attachment, and nothing an edit can do is
// allowed to break that.

const ZH: &str = "[图片 #{}]";

fn with_tokens() -> Composer {
    let mut c = Composer::new();
    c.set_image_token_template(ZH);
    c
}

#[test]
fn an_image_token_goes_in_at_the_cursor() {
    let mut c = with_tokens();
    c.insert_str("对比 ");
    assert_eq!(c.insert_image_token(), 0);
    c.insert_str("和 ");
    assert_eq!(c.insert_image_token(), 1);
    c.insert_str("的差异");
    assert_eq!(c.text(), "对比 [图片 #1] 和 [图片 #2] 的差异");
    assert_eq!(c.image_token_count(), 2);
}

/// Pasting a second image BEFORE the first one makes it the first image: the
/// numbers follow the sentence, and so must the attachments.
#[test]
fn a_token_inserted_earlier_takes_the_earlier_number() {
    let mut c = with_tokens();
    c.insert_str("尾");
    c.insert_image_token();
    c.move_to_line_start();
    assert_eq!(
        c.insert_image_token(),
        0,
        "it went in ahead of the other one"
    );
    assert_eq!(c.text(), "[图片 #1] 尾[图片 #2] ");
}

/// A token is one unit to the cursor: ← from just after it lands before it,
/// never in the middle of `#1]`.
#[test]
fn the_cursor_steps_over_a_token_whole() {
    let mut c = with_tokens();
    c.insert_image_token();
    let after = c.cursor();
    c.move_left(); // over the trailing space
    c.move_left(); // over the token
    assert_eq!(c.cursor(), 0, "the token is one step, not nine");
    c.move_right();
    assert_eq!(c.cursor(), after - 1);
}

/// Backspace at the end of a token removes the token, not its last bracket.
#[test]
fn backspace_takes_the_whole_token() {
    let mut c = with_tokens();
    c.insert_str("看 ");
    c.insert_image_token();
    c.backspace(); // the space the token brought with it
    c.backspace();
    assert_eq!(c.text(), "看 ");
    assert_eq!(c.image_token_count(), 0);
}

/// Delete (forward) is the same unit.
#[test]
fn forward_delete_takes_the_whole_token() {
    let mut c = with_tokens();
    c.insert_image_token();
    c.insert_str("后");
    c.move_to_line_start();
    c.delete();
    assert_eq!(c.image_token_count(), 0);
    assert_eq!(c.text(), "后", "the space it brought goes with it");
}

/// Deleting the middle image renumbers the rest, so the numbers the user reads
/// always count 1, 2, 3 — and still name the attachments in that order.
#[test]
fn removing_one_token_renumbers_the_others() {
    let mut c = with_tokens();
    for i in 0..3 {
        c.insert_image_token();
        c.insert_str(&format!("t{i} "));
    }
    assert_eq!(c.text(), "[图片 #1] t0 [图片 #2] t1 [图片 #3] t2 ");
    c.remove_image_token(2);
    assert_eq!(c.text(), "[图片 #1] t0 t1 [图片 #2] t2 ");
}

/// The safety net: whatever an edit does to the text, the tokens that survive
/// are renumbered 1..n and the caller is told which attachments they name, so
/// what the user reads and what the model receives can never disagree.
#[test]
fn reconciling_reports_the_surviving_attachments_in_order() {
    let mut c = with_tokens();
    // A blunt edit that cuts the second token in half.
    c.replace("[图片 #1] [图片 # [图片 #3] ");
    assert_eq!(c.reconcile_image_tokens(3), vec![0, 2]);
    assert_eq!(c.text(), "[图片 #1] [图片 # [图片 #2] ");
}

/// A token the user copied is one image referred to once: the copy goes.
#[test]
fn a_duplicated_token_is_not_a_second_image() {
    let mut c = with_tokens();
    c.replace("[图片 #1] [图片 #1] [图片 #2] ");
    assert_eq!(c.reconcile_image_tokens(2), vec![0, 1]);
    assert_eq!(c.text(), "[图片 #1] [图片 #2] ");
}

/// A token naming an image that is not staged names nothing.
#[test]
fn a_token_past_the_staged_images_is_dropped() {
    let mut c = with_tokens();
    c.replace("[图片 #1] [图片 #7] ");
    assert_eq!(c.reconcile_image_tokens(1), vec![0]);
    assert_eq!(c.text(), "[图片 #1] ");
}

/// Alt+Backspace is word-delete, and a token contains a space. It must not
/// leave `[图片 ` behind.
#[test]
fn word_delete_does_not_cut_a_token_in_half() {
    let mut c = with_tokens();
    c.insert_str("看 ");
    c.insert_image_token();
    c.delete_word_back();
    c.delete_word_back();
    assert_eq!(c.image_token_count(), 0);
    assert!(
        !c.text().contains("图片"),
        "half a token is left: {:?}",
        c.text()
    );
}

/// The token is what the user wrote, so it is what the model reads.
#[test]
fn a_token_survives_submission_as_text() {
    let mut c = with_tokens();
    c.insert_image_token();
    c.insert_str("这是什么");
    assert_eq!(c.take(), "[图片 #1] 这是什么");
}

// ---- Word movement (Option+←/→) ------------------------------------------

/// Option+← lands on the start of the previous word and edits nothing.
#[test]
fn move_word_left_reaches_the_previous_word_start() {
    let mut c = Composer::new();
    c.insert_str("hello world");
    assert_eq!(c.cursor(), 11);
    c.move_word_left();
    assert_eq!(c.cursor(), 6);
    assert_eq!(c.text(), "hello world", "cursor movement must not edit");
}

/// Option+→ lands on the end of the next word and does not type an `f`.
#[test]
fn move_word_right_reaches_the_next_word_end() {
    let mut c = Composer::new();
    c.insert_str("hello world");
    c.move_to_line_start();
    c.move_word_right();
    assert_eq!(c.cursor(), 5);
    assert_eq!(c.text(), "hello world", "cursor movement must not edit");
}

/// Alternating Option+←/→ never touches the text.
#[test]
fn alternating_word_moves_leave_the_text_untouched() {
    let mut c = Composer::new();
    c.insert_str("hello world test");
    c.move_word_left();
    c.move_word_left();
    c.move_word_right();
    c.move_word_right();
    assert_eq!(c.text(), "hello world test");
    assert_eq!(c.cursor(), 16, "back where it started");
}

/// The step is grapheme-based: a CJK word moves whole, never by byte.
#[test]
fn word_moves_are_grapheme_safe() {
    let mut c = Composer::new();
    c.insert_str("你好 世界");
    c.move_word_left();
    assert_eq!(c.cursor(), 3, "start of 世界");
    c.move_word_left();
    assert_eq!(c.cursor(), 0, "start of 你好");
    assert_eq!(c.text(), "你好 世界");
}

/// A word move never leaves the caret inside an image token.
#[test]
fn word_moves_step_over_a_token_whole() {
    let mut c = with_tokens();
    c.insert_image_token(); // "[图片 #1] "
    c.insert_str("看");
    c.move_word_left();
    assert_eq!(c.cursor(), 8, "lands after the token, before 看");
    c.move_word_left();
    assert_eq!(c.cursor(), 0, "the token is one word, not two");
}
