use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate a string to at most `max` display columns, adding an ellipsis.
/// Measures and cuts by DISPLAY WIDTH (CJK chars are 2 cells), never by char
/// count — otherwise wide text overflows its box and clobbers the right border.
pub(crate) fn truncate_display(s: &str, max: usize) -> String {
    // Neutralize control chars (a raw \r returns the cursor to column 0, \t
    // tab-expands) so a single-line summary built from arbitrary tool output
    // can't corrupt the row. Newlines and other C0 controls become spaces.
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if UnicodeWidthStr::width(flat.as_str()) <= max {
        return flat;
    }
    let budget = max.saturating_sub(1); // reserve a column for '…'
    let mut out = String::new();
    let mut w = 0;
    for ch in flat.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Pick the slice of `text` to show in a `room`-wide input area so the cursor at
/// display column `ccol` stays visible, scrolling horizontally when the line is
/// wider than the area. Returns the visible slice and the horizontal scroll (in
/// display columns) applied. All math is in display cells (CJK-safe).
// Kept for cell-window unit tests; soft-wrap path is the live composer layout.
#[allow(dead_code)]
pub(crate) fn composer_text_window(text: &str, ccol: usize, room: usize) -> (String, usize) {
    if room == 0 {
        return (String::new(), 0);
    }
    if UnicodeWidthStr::width(text) <= room {
        return (text.to_string(), 0);
    }
    // Keep the caret one cell from the right edge when scrolled.
    let hscroll = ccol.saturating_sub(room.saturating_sub(1));
    if hscroll == 0 {
        return (display_window(text, hscroll, room), hscroll);
    }
    if room == 1 {
        return ("…".to_string(), hscroll);
    }

    // Make horizontal scrolling visible. Without this, a long single-line
    // prompt looks like its beginning simply disappeared.
    let mut piece = String::from("…");
    piece.push_str(&display_window(text, hscroll, room - 1));
    // The leading marker consumes one cell before the scrolled text, so report
    // one less display column to keep the rendered cursor aligned.
    (piece, hscroll.saturating_sub(1))
}

/// The substring of `s` covering display columns `[start_col, start_col + room)`.
/// A wide char straddling either boundary is dropped and shown as a blank so the
/// column grid stays aligned.
#[allow(dead_code)]
pub(crate) fn display_window(s: &str, start_col: usize, room: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize; // display column of the next char in `s`
    let mut w = 0usize; // width already emitted into the window
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col + cw <= start_col {
            col += cw; // entirely left of the window
            continue;
        }
        if col < start_col {
            // Straddles the left edge: show a blank for its visible half.
            out.push(' ');
            w += 1;
            col += cw;
            if w >= room {
                break;
            }
            continue;
        }
        if w + cw > room {
            break; // would straddle the right edge
        }
        out.push(ch);
        w += cw;
        col += cw;
    }
    out
}

/// Make a raw line safe for cell-based TUI output: expand tabs (terminal tab
/// stops would desync the ratatui buffer) and neutralise other C0 controls
/// (`\r` etc. would rewind the cursor when printed by crossterm).
pub(crate) fn sanitize_terminal_line(s: &str) -> String {
    const TABSTOP: usize = 8;
    let mut out = String::with_capacity(s.len());
    let mut col = 0usize;
    for ch in s.chars() {
        if ch == '\t' {
            let spaces = TABSTOP - (col % TABSTOP);
            out.extend(std::iter::repeat_n(' ', spaces));
            col += spaces;
        } else if ch == '\n' || ch == '\r' || ch.is_control() {
            // Flatten — callers already split on newlines; a bare control must
            // not reach Print().
            out.push(' ');
            col += 1;
        } else {
            out.push(ch);
            col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    out
}

/// Width-aware wrap that preserves explicit newlines.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for logical in text.split('\n') {
        if logical.is_empty() {
            out.push(String::new());
            continue;
        }
        for piece in textwrap::wrap(logical, width) {
            out.push(piece.into_owned());
        }
    }
    out
}

/// Take a display-width prefix of at most `room` cells; return (piece, rest).
/// Used by the composer soft-wrap path so a long typed line becomes multiple
/// visual rows instead of overflowing the box.
pub(crate) fn take_display_prefix(text: &str, room: usize) -> (String, &str) {
    if room == 0 || text.is_empty() {
        return (String::new(), text);
    }
    let mut w = 0usize;
    let mut end = 0usize;
    for (i, ch) in text.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w > 0 && w + cw > room {
            break;
        }
        if w == 0 && cw > room {
            // Force progress on an oversized glyph.
            end = i + ch.len_utf8();
            break;
        }
        w += cw;
        end = i + ch.len_utf8();
    }
    (text[..end].to_string(), &text[end..])
}
