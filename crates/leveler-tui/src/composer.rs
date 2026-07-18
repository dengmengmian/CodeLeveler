//! The Composer: a self-authored multiline text editor.
//!
//! Not a Readline/Reedline wrapper . All positions are
//! **grapheme** indices, never byte offsets, so Chinese, emoji, and combining
//! marks edit and cursor correctly . Display columns are computed with
//! Unicode width, so full-width characters occupy two cells.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Collapse multi-line / bulk pastes into a one-line chip once either threshold
/// is hit. Kept low so a typical 5–12 line snippet folds.
const PASTE_PLACEHOLDER_MIN_CHARS: usize = 200;
const PASTE_PLACEHOLDER_MIN_LINES: usize = 5;

/// Count graphemes in a string.
fn grapheme_count(s: &str) -> usize {
    s.graphemes(true).count()
}

/// Byte offset where the `g`-th grapheme starts (or `s.len()` if `g` is the end).
fn byte_of_grapheme(s: &str, g: usize) -> usize {
    s.grapheme_indices(true)
        .nth(g)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// A multiline text buffer with a grapheme cursor and input history.
#[derive(Debug, Default, Clone)]
pub struct Composer {
    buffer: String,
    /// Cursor position as a grapheme index in `[0, len]`.
    cursor: usize,
    history: Vec<String>,
    /// `Some(i)` while browsing history; `None` when editing the live buffer.
    history_index: Option<usize>,
    /// The live draft, stashed while browsing history so it can be restored.
    stash: Option<String>,
    pending_pastes: Vec<PendingPaste>,
    /// Whether the whole buffer is an auto-filled next-step suggestion. The
    /// first edit replaces it; submitting without editing accepts it.
    suggested: bool,
}

#[derive(Debug, Clone)]
struct PendingPaste {
    placeholder: String,
    content: String,
}

impl Composer {
    pub fn new() -> Self {
        Self::default()
    }

    // ---- inspection ---------------------------------------------------------

    /// The current text.
    pub fn text(&self) -> &str {
        &self.buffer
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Number of graphemes in the buffer.
    pub fn len(&self) -> usize {
        grapheme_count(&self.buffer)
    }

    /// Cursor position as a grapheme index.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Text before the cursor, used by completion providers.
    pub fn text_before_cursor(&self) -> &str {
        &self.buffer[..byte_of_grapheme(&self.buffer, self.cursor)]
    }

    /// Logical lines (split on `\n`), for rendering.
    pub fn lines(&self) -> Vec<&str> {
        self.buffer.split('\n').collect()
    }

    /// Number of logical lines (at least 1).
    pub fn line_count(&self) -> usize {
        self.buffer.split('\n').count()
    }

    fn is_multiline(&self) -> bool {
        self.buffer.contains('\n')
    }

    /// Cursor as `(row, display_col)` where `row` is the logical line index and
    /// `display_col` is the sum of Unicode display widths before the cursor on
    /// that row. Used to place the terminal cursor .
    pub fn cursor_row_col_display(&self) -> (usize, usize) {
        let mut row = 0;
        let mut col_start_byte = 0;
        let cursor_byte = byte_of_grapheme(&self.buffer, self.cursor);
        // Find the start of the line the cursor is on.
        for (i, ch) in self.buffer.char_indices() {
            if i >= cursor_byte {
                break;
            }
            if ch == '\n' {
                row += 1;
                col_start_byte = i + 1;
            }
        }
        let col = self.buffer[col_start_byte..cursor_byte].width();
        (row, col)
    }

    // ---- editing ------------------------------------------------------------

    /// Insert text at the cursor. Normalizes `\r\n` / `\r` to `\n` so pasted
    /// content keeps a single line convention .
    pub fn insert_str(&mut self, s: &str) {
        self.begin_edit();
        let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
        let at = byte_of_grapheme(&self.buffer, self.cursor);
        self.buffer.insert_str(at, &normalized);
        self.cursor += grapheme_count(&normalized);
    }

    /// Insert pasted text. Large pastes are represented by a short placeholder
    /// in the editor and expanded back to the original text on submission.
    pub fn insert_paste(&mut self, s: &str) {
        let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
        let chars = normalized.chars().count();
        let lines = normalized.lines().count().max(1);
        if chars < PASTE_PLACEHOLDER_MIN_CHARS && lines < PASTE_PLACEHOLDER_MIN_LINES {
            self.insert_str(&normalized);
            return;
        }

        let id = self.pending_pastes.len() + 1;
        // Match common agent TUIs: short chip, unique when multiple pastes stack.
        let placeholder = if id == 1 {
            format!("[Pasted: {lines} lines]")
        } else {
            format!("[Pasted: {lines} lines #{id}]")
        };
        self.pending_pastes.push(PendingPaste {
            placeholder: placeholder.clone(),
            content: normalized,
        });
        self.insert_str(&placeholder);
    }

    pub fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.insert_str(c.encode_utf8(&mut buf));
    }

    /// Replace the whitespace-delimited token immediately before the cursor.
    pub fn replace_token_before_cursor(&mut self, replacement: &str) {
        self.begin_edit();
        let end = byte_of_grapheme(&self.buffer, self.cursor);
        let start = self.buffer[..end]
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(index, ch)| index + ch.len_utf8())
            .unwrap_or(0);
        self.buffer.replace_range(start..end, replacement);
        self.cursor = grapheme_count(&self.buffer[..start]) + grapheme_count(replacement);
    }

    /// Insert a newline (soft submit: `Ctrl+J` / `Alt+Enter`, ).
    pub fn newline(&mut self) {
        self.insert_char('\n');
    }

    /// Delete the grapheme before the cursor.
    pub fn backspace(&mut self) {
        self.begin_edit();
        if self.cursor == 0 {
            return;
        }
        let start = byte_of_grapheme(&self.buffer, self.cursor - 1);
        let end = byte_of_grapheme(&self.buffer, self.cursor);
        self.buffer.replace_range(start..end, "");
        self.cursor -= 1;
    }

    /// Delete the grapheme at the cursor.
    pub fn delete(&mut self) {
        self.begin_edit();
        if self.cursor >= self.len() {
            return;
        }
        let start = byte_of_grapheme(&self.buffer, self.cursor);
        let end = byte_of_grapheme(&self.buffer, self.cursor + 1);
        self.buffer.replace_range(start..end, "");
    }

    /// Delete the whitespace-delimited word before the cursor (`Ctrl+W`).
    pub fn delete_word_back(&mut self) {
        self.begin_edit();
        let graphs: Vec<&str> = self.buffer.graphemes(true).collect();
        let mut start = self.cursor;
        // Skip trailing whitespace.
        while start > 0 && graphs[start - 1].trim().is_empty() {
            start -= 1;
        }
        // Skip the word.
        while start > 0 && !graphs[start - 1].trim().is_empty() {
            start -= 1;
        }
        let from = byte_of_grapheme(&self.buffer, start);
        let to = byte_of_grapheme(&self.buffer, self.cursor);
        self.buffer.replace_range(from..to, "");
        self.cursor = start;
    }

    // ---- cursor movement ----------------------------------------------------

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.len() {
            self.cursor += 1;
        }
    }

    /// Move to the start of the current logical line (`Home` / `Ctrl+A`).
    pub fn move_to_line_start(&mut self) {
        self.cursor = self.line_bounds().0;
    }

    /// Move to the end of the current logical line (`End` / `Ctrl+E`).
    pub fn move_to_line_end(&mut self) {
        self.cursor = self.line_bounds().1;
    }

    /// `Up`: move up a line in a multiline buffer; on the first line (or when
    /// single-line) browse submission history backward.
    pub fn up(&mut self) {
        if self.is_multiline() {
            let (row, _) = self.cursor_row_col_display();
            if row > 0 {
                self.move_vertical(-1);
                return;
            }
        }
        self.history_prev();
    }

    /// `Down`: move down a line in a multiline buffer; on the last line while
    /// browsing history (or when single-line) step history forward / restore draft.
    pub fn down(&mut self) {
        if self.is_multiline() {
            let (row, _) = self.cursor_row_col_display();
            if row + 1 < self.line_count() {
                self.move_vertical(1);
                return;
            }
            // Last line of a multi-line history entry → next history / draft.
            if self.history_index.is_some() {
                self.history_next();
            }
            return;
        }
        self.history_next();
    }

    /// Whether there is at least one submission to recall with ↑.
    pub fn has_history(&self) -> bool {
        !self.history.is_empty()
    }

    /// True while ↑/↓ is stepping through history (draft is stashed).
    pub fn is_browsing_history(&self) -> bool {
        self.history_index.is_some()
    }

    /// Delete from the cursor to the end of the line (`Ctrl+K`).
    pub fn kill_to_line_end(&mut self) {
        self.begin_edit();
        let (_, end) = self.line_bounds();
        let from = byte_of_grapheme(&self.buffer, self.cursor);
        let to = byte_of_grapheme(&self.buffer, end);
        self.buffer.replace_range(from..to, "");
    }

    /// Delete from the start of the line to the cursor (`Ctrl+U`).
    pub fn kill_to_line_start(&mut self) {
        self.begin_edit();
        let (start, _) = self.line_bounds();
        let from = byte_of_grapheme(&self.buffer, start);
        let to = byte_of_grapheme(&self.buffer, self.cursor);
        self.buffer.replace_range(from..to, "");
        self.cursor = start;
    }

    /// Grapheme indices of the current line's start and end (excluding `\n`).
    fn line_bounds(&self) -> (usize, usize) {
        let graphs: Vec<&str> = self.buffer.graphemes(true).collect();
        let mut start = self.cursor;
        while start > 0 && graphs[start - 1] != "\n" {
            start -= 1;
        }
        let mut end = self.cursor;
        while end < graphs.len() && graphs[end] != "\n" {
            end += 1;
        }
        (start, end)
    }

    /// Move the cursor up/down one logical line, keeping the grapheme column.
    fn move_vertical(&mut self, delta: isize) {
        let graphs: Vec<&str> = self.buffer.graphemes(true).collect();
        // Split into lines of grapheme index ranges.
        let mut lines: Vec<(usize, usize)> = Vec::new();
        let mut start = 0;
        for (i, g) in graphs.iter().enumerate() {
            if *g == "\n" {
                lines.push((start, i));
                start = i + 1;
            }
        }
        lines.push((start, graphs.len()));

        // Locate current line + column.
        let (mut cur_line, mut col) = (0, 0);
        for (li, (s, e)) in lines.iter().enumerate() {
            if self.cursor >= *s && self.cursor <= *e {
                cur_line = li;
                col = self.cursor - s;
                break;
            }
        }
        let target = cur_line as isize + delta;
        if target < 0 || target as usize >= lines.len() {
            return;
        }
        let (s, e) = lines[target as usize];
        self.cursor = (s + col).min(e);
    }

    // ---- history ------------------------------------------------------------

    /// Take the buffer for submission: clears it and records it in history
    /// (skipping empty and consecutive-duplicate entries, ).
    pub fn take(&mut self) -> String {
        let mut text = std::mem::take(&mut self.buffer);
        for paste in std::mem::take(&mut self.pending_pastes) {
            text = text.replace(&paste.placeholder, &paste.content);
        }
        self.cursor = 0;
        self.history_index = None;
        self.stash = None;
        self.suggested = false;
        let trimmed = text.trim();
        if !trimmed.is_empty() && self.history.last().map(|h| h.as_str()) != Some(text.as_str()) {
            self.history.push(text.clone());
        }
        text
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_index {
            None => {
                self.stash = Some(self.buffer.clone());
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_index = Some(idx);
        self.set_buffer(self.history[idx].clone());
    }

    fn history_next(&mut self) {
        let Some(idx) = self.history_index else {
            return;
        };
        if idx + 1 < self.history.len() {
            self.history_index = Some(idx + 1);
            self.set_buffer(self.history[idx + 1].clone());
        } else {
            // Past the newest entry: restore the live draft.
            self.history_index = None;
            let draft = self.stash.take().unwrap_or_default();
            self.set_buffer(draft);
        }
    }

    /// Once the user edits while browsing history, detach from the history entry.
    fn commit_history_browse(&mut self) {
        self.history_index = None;
        self.stash = None;
    }

    /// Start editing the live buffer. Auto-filled suggestions behave like a
    /// selected completion: the first edit replaces the whole suggestion.
    fn begin_edit(&mut self) {
        self.commit_history_browse();
        if self.suggested {
            self.buffer.clear();
            self.cursor = 0;
            self.suggested = false;
        }
    }

    fn set_buffer(&mut self, s: String) {
        self.cursor = grapheme_count(&s);
        self.buffer = s;
        self.suggested = false;
    }

    /// Seed history (e.g. from a persisted store).
    pub fn set_history(&mut self, history: Vec<String>) {
        self.history = history;
    }

    /// The submission history, oldest first (for persistence).
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Replace the buffer wholesale, placing the cursor at the end (used by
    /// autocompletion). Does not touch history.
    pub fn replace(&mut self, text: impl Into<String>) {
        self.commit_history_browse();
        self.pending_pastes.clear();
        self.buffer = text.into();
        self.cursor = grapheme_count(&self.buffer);
        self.suggested = false;
    }

    /// Fill the composer with an actionable next step. Enter accepts it;
    /// typing or deleting first replaces it as one selected unit.
    pub fn replace_suggestion(&mut self, text: impl Into<String>) {
        self.replace(text);
        self.suggested = true;
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    #[test]
    fn take_records_history_and_skips_empty_and_dupes() {
        let mut c = Composer::new();
        c.replace("修复登录问题");
        assert_eq!(c.take(), "修复登录问题");
        c.replace("增加测试");
        assert_eq!(c.take(), "增加测试");
        c.replace("增加测试");
        let _ = c.take(); // consecutive duplicate skipped
        c.replace("   ");
        let _ = c.take(); // whitespace-only skipped
        assert_eq!(
            c.history(),
            &["修复登录问题".to_string(), "增加测试".to_string()]
        );
    }

    #[test]
    fn up_from_empty_recalls_last_submission() {
        let mut c = Composer::new();
        c.set_history(vec!["修复登录问题".into(), "增加测试".into()]);
        assert!(c.is_empty());
        c.up();
        assert_eq!(c.text(), "增加测试");
        assert!(c.is_browsing_history());
        c.up();
        assert_eq!(c.text(), "修复登录问题");
        // Cursor at end.
        assert_eq!(c.cursor(), grapheme_count("修复登录问题"));
    }

    #[test]
    fn down_restores_draft_after_history_browse() {
        let mut c = Composer::new();
        c.set_history(vec!["修复登录问题".into(), "增加测试".into()]);
        c.replace("修复");
        c.up();
        assert_eq!(c.text(), "增加测试");
        c.up();
        assert_eq!(c.text(), "修复登录问题");
        c.down();
        assert_eq!(c.text(), "增加测试");
        c.down();
        assert_eq!(c.text(), "修复");
        assert!(!c.is_browsing_history());
    }

    #[test]
    fn empty_draft_restores_to_empty_after_browse() {
        let mut c = Composer::new();
        c.set_history(vec!["hello".into()]);
        c.up();
        assert_eq!(c.text(), "hello");
        c.down();
        assert!(c.is_empty());
    }

    #[test]
    fn multiline_history_entry_up_on_first_line_goes_older() {
        let mut c = Composer::new();
        c.set_history(vec!["older".into(), "line1\nline2".into()]);
        c.up();
        assert_eq!(c.text(), "line1\nline2");
        // Cursor is at end (last line). Move to first line then Up → older.
        c.move_to_line_start();
        // end is last line; go home of last line then up
        while c.cursor_row_col_display().0 > 0 {
            c.move_vertical(-1);
        }
        assert_eq!(c.cursor_row_col_display().0, 0);
        c.up();
        assert_eq!(c.text(), "older");
    }

    #[test]
    fn slash_commands_are_recorded_like_normal_tasks() {
        let mut c = Composer::new();
        c.replace("/model deepseek");
        let _ = c.take();
        c.replace("/btw 为什么这样设计");
        let _ = c.take();
        assert_eq!(
            c.history(),
            &[
                "/model deepseek".to_string(),
                "/btw 为什么这样设计".to_string()
            ]
        );
    }
}
