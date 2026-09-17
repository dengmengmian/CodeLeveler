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

/// The image-token template when none has been set: `[image #1]`.
const DEFAULT_IMAGE_TOKEN: &str = "[image #{}]";

/// A multiline text buffer with a grapheme cursor and input history.
#[derive(Debug, Clone)]
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
    /// How an attached image is written into the sentence, e.g. `[图片 #{}]`.
    /// The token is ordinary text — it is what the user reads AND what the
    /// model receives — but it edits as one unit, and its number is its
    /// position among the tokens, which is the index of the image it names.
    image_token: String,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            stash: None,
            pending_pastes: Vec::new(),
            image_token: DEFAULT_IMAGE_TOKEN.to_string(),
        }
    }
}

/// Split a token template into the text before and after its number.
fn token_affixes(template: &str) -> (&str, &str) {
    template.split_once("{}").unwrap_or((template, ""))
}

/// Byte spans of every well-formed image token in `s`, with the number each
/// carries, in buffer order. Text that merely looks like the start of one is
/// skipped, not guessed at.
fn scan_image_tokens(s: &str, template: &str) -> Vec<(std::ops::Range<usize>, usize)> {
    let (prefix, suffix) = token_affixes(template);
    if prefix.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = s[from..].find(prefix) {
        let start = from + rel;
        let digits_at = start + prefix.len();
        let digits: String = s[digits_at..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        // Resume past this prefix either way: a broken token must not hide a
        // good one that follows it.
        from = digits_at;
        if digits.is_empty() || !s[digits_at + digits.len()..].starts_with(suffix) {
            continue;
        }
        let end = digits_at + digits.len() + suffix.len();
        if let Ok(number) = digits.parse::<usize>() {
            found.push((start..end, number));
            from = end;
        }
    }
    found
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

    /// Delete the grapheme before the cursor — or, when that grapheme is part
    /// of an image token, the whole token. Half a token names nothing.
    pub fn backspace(&mut self) {
        self.begin_edit();
        if self.cursor == 0 {
            return;
        }
        if let Some(range) = self.token_across(true) {
            self.cut_token(range);
            return;
        }
        let start = byte_of_grapheme(&self.buffer, self.cursor - 1);
        let end = byte_of_grapheme(&self.buffer, self.cursor);
        self.buffer.replace_range(start..end, "");
        self.cursor -= 1;
    }

    /// Delete the grapheme at the cursor, or the whole image token it is in.
    pub fn delete(&mut self) {
        self.begin_edit();
        if self.cursor >= self.len() {
            return;
        }
        if let Some(range) = self.token_across(false) {
            self.cut_token(range);
            return;
        }
        let start = byte_of_grapheme(&self.buffer, self.cursor);
        let end = byte_of_grapheme(&self.buffer, self.cursor + 1);
        self.buffer.replace_range(start..end, "");
    }

    /// Remove an image token, and the single space it brought in with it, so
    /// deleting a picture does not leave a gap in the sentence.
    fn cut_token(&mut self, range: std::ops::Range<usize>) {
        let mut end = range.end;
        if self.buffer[end..].starts_with(' ') {
            end += 1;
        }
        let mut text = self.buffer.clone();
        text.replace_range(range.start..end, "");
        self.rewrite(text, range.start);
    }

    /// Delete the whitespace-delimited word before the cursor (`Ctrl+W`).
    ///
    /// An image token carries a space of its own, so a word delete that walked
    /// into one would leave `[图片 ` behind — a name for nothing. The token is
    /// one word.
    pub fn delete_word_back(&mut self) {
        self.begin_edit();
        if let Some(range) = self.token_across(true) {
            self.cut_token(range);
            return;
        }
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

    /// ← one grapheme, or one whole image token: the cursor never rests INSIDE
    /// `[图片 #1]`, where typing would break the name of a real attachment.
    /// Either edge of a token is a place to stand; the middle is not.
    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        if let Some(range) = self.token_interior() {
            self.cursor = grapheme_count(&self.buffer[..range.start]);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.len() {
            self.cursor += 1;
        }
        if let Some(range) = self.token_interior() {
            self.cursor = grapheme_count(&self.buffer[..range.end]);
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

    /// The buffer with paste placeholders expanded back to their real content.
    /// `text()` is presentation (chips stay chips); THIS is the canonical form
    /// that submission paths must use — a placeholder is never user content.
    /// Each pending paste replaces its chip once, oldest first.
    pub fn canonical_text(&self) -> String {
        let mut text = self.buffer.clone();
        for paste in &self.pending_pastes {
            text = text.replacen(&paste.placeholder, &paste.content, 1);
        }
        text
    }

    /// Take the buffer for submission: clears it and records it in history
    /// (skipping empty and consecutive-duplicate entries, ).
    pub fn take(&mut self) -> String {
        let text = self.canonical_text();
        self.buffer.clear();
        self.pending_pastes.clear();
        self.cursor = 0;
        self.history_index = None;
        self.stash = None;
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

    /// Start editing the live buffer.
    fn begin_edit(&mut self) {
        self.commit_history_browse();
    }

    fn set_buffer(&mut self, s: String) {
        self.cursor = grapheme_count(&s);
        self.buffer = s;
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
    }

    // ---- image tokens -------------------------------------------------------

    /// Set how an attached image is written into the sentence. The session's
    /// language decides this, so it is set once the locale is known.
    pub fn set_image_token_template(&mut self, template: &str) {
        self.image_token = template.to_string();
    }

    /// How many images the sentence names.
    pub fn image_token_count(&self) -> usize {
        scan_image_tokens(&self.buffer, &self.image_token).len()
    }

    /// Byte spans and numbers of the tokens, in buffer order.
    fn image_tokens(&self) -> Vec<(std::ops::Range<usize>, usize)> {
        scan_image_tokens(&self.buffer, &self.image_token)
    }

    /// Write an image into the sentence at the cursor, and answer where it
    /// belongs among the staged images: its position among the tokens. An
    /// image pasted ahead of another one IS the earlier image.
    pub fn insert_image_token(&mut self) -> usize {
        self.begin_edit();
        let at = byte_of_grapheme(&self.buffer, self.cursor);
        let index = self
            .image_tokens()
            .iter()
            .filter(|(range, _)| range.end <= at)
            .count();
        let token = self.image_token.replace("{}", &(index + 1).to_string());
        self.buffer.insert_str(at, &format!("{token} "));
        self.cursor += grapheme_count(&token) + 1;
        self.renumber_image_tokens();
        index
    }

    /// Remove the `ordinal`-th token (1-based) and the space it brought with
    /// it, then renumber what is left.
    pub fn remove_image_token(&mut self, ordinal: usize) {
        self.begin_edit();
        let Some((range, _)) = self.image_tokens().get(ordinal.wrapping_sub(1)).cloned() else {
            return;
        };
        let mut end = range.end;
        if self.buffer[end..].starts_with(' ') {
            end += 1;
        }
        let cursor_byte = byte_of_grapheme(&self.buffer, self.cursor);
        let mut text = self.buffer.clone();
        text.replace_range(range.start..end, "");
        let moved = if cursor_byte <= range.start {
            cursor_byte
        } else if cursor_byte >= end {
            cursor_byte - (end - range.start)
        } else {
            range.start
        };
        self.rewrite(text, moved);
        self.renumber_image_tokens();
    }

    /// Enforce the one invariant, whatever an edit did to the text: the k-th
    /// token names the k-th staged image. Answers with the index each
    /// surviving token names *now*, in order, so the caller can rebuild its
    /// list — it never rebuilds an attachment from the text, only reorders
    /// the ones it already holds.
    ///
    /// A token an edit broke is gone, a token naming an image that is not
    /// staged is gone, and a token the user copied is one image referred to
    /// once. Called after every edit, so what the user reads and what the
    /// model receives cannot drift apart.
    pub fn reconcile_image_tokens(&mut self, staged: usize) -> Vec<usize> {
        let mut keep = Vec::new();
        let mut seen = Vec::new();
        let mut drop: Vec<std::ops::Range<usize>> = Vec::new();
        for (range, number) in self.image_tokens() {
            let index = number.wrapping_sub(1);
            if number == 0 || number > staged || seen.contains(&index) {
                drop.push(range);
            } else {
                seen.push(index);
                keep.push(index);
            }
        }
        for range in drop.into_iter().rev() {
            let mut end = range.end;
            if self.buffer[end..].starts_with(' ') {
                end += 1;
            }
            let cursor_byte = byte_of_grapheme(&self.buffer, self.cursor);
            let mut text = self.buffer.clone();
            text.replace_range(range.start..end, "");
            let moved = if cursor_byte <= range.start {
                cursor_byte
            } else if cursor_byte >= end {
                cursor_byte - (end - range.start)
            } else {
                range.start
            };
            self.rewrite(text, moved);
        }
        self.renumber_image_tokens();
        keep
    }

    /// Rewrite every token's number to its position, keeping the cursor where
    /// the user left it — the numbers can change width (9 → 10), and a cursor
    /// that ended up inside a token is put just after it.
    fn renumber_image_tokens(&mut self) {
        let tokens = self.image_tokens();
        if tokens.is_empty() {
            return;
        }
        let cursor_byte = byte_of_grapheme(&self.buffer, self.cursor);
        let mut out = String::with_capacity(self.buffer.len());
        let mut moved = None;
        let mut last = 0usize;
        for (position, (range, _)) in tokens.iter().enumerate() {
            if moved.is_none() && cursor_byte <= range.start {
                moved = Some(out.len() + (cursor_byte - last));
            }
            out.push_str(&self.buffer[last..range.start]);
            out.push_str(&self.image_token.replace("{}", &(position + 1).to_string()));
            if moved.is_none() && cursor_byte < range.end {
                moved = Some(out.len());
            }
            last = range.end;
        }
        let tail = out.len();
        out.push_str(&self.buffer[last..]);
        // `unwrap_or` evaluates its argument either way, and past the last
        // token `cursor_byte - last` is the only form that is not a subtraction
        // waiting to underflow.
        let moved = moved
            .unwrap_or_else(|| tail + cursor_byte.saturating_sub(last))
            .min(out.len());
        self.rewrite(out, moved);
    }

    /// Replace the buffer and place the cursor at a byte offset within it.
    fn rewrite(&mut self, text: String, cursor_byte: usize) {
        self.cursor = grapheme_count(&text[..cursor_byte.min(text.len())]);
        self.buffer = text;
    }

    /// Move every token's number up by `by`, to make room for images that are
    /// being put back ahead of this draft. Without it a restored message and
    /// the draft written since would both start at `#1`, and one set of names
    /// would read as a copy of the other.
    pub fn shift_image_token_numbers(&mut self, by: usize) {
        if by == 0 {
            return;
        }
        let tokens = self.image_tokens();
        let mut out = String::with_capacity(self.buffer.len());
        let mut last = 0usize;
        for (range, number) in &tokens {
            out.push_str(&self.buffer[last..range.start]);
            out.push_str(&self.image_token.replace("{}", &(number + by).to_string()));
            last = range.end;
        }
        out.push_str(&self.buffer[last..]);
        let cursor = self.cursor.min(grapheme_count(&out));
        self.buffer = out;
        self.cursor = cursor;
    }

    /// The token the cursor is strictly inside — not at either edge, which is
    /// where a caret may rest.
    fn token_interior(&self) -> Option<std::ops::Range<usize>> {
        let at = byte_of_grapheme(&self.buffer, self.cursor);
        self.image_tokens()
            .into_iter()
            .find_map(|(range, _)| (at > range.start && at < range.end).then_some(range))
    }

    /// The token the cursor is inside or at the far edge of, as a byte span.
    /// `before` looks at what a backward step would cross, otherwise forward.
    fn token_across(&self, before: bool) -> Option<std::ops::Range<usize>> {
        let at = byte_of_grapheme(&self.buffer, self.cursor);
        self.image_tokens().into_iter().find_map(|(range, _)| {
            let crosses = if before {
                at > range.start && at <= range.end
            } else {
                at >= range.start && at < range.end
            };
            crosses.then_some(range)
        })
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
