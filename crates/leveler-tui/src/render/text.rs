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

/// Pad `s` on the right to `width` COLUMNS, not characters. A label column
/// built with `{:<10}` counts "运行时长" as 4 and "状态" as 2, so the values
/// beside them started two columns apart.
pub(crate) fn pad_display(s: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(s);
    let mut out = s.to_string();
    for _ in used..width {
        out.push(' ');
    }
    out
}

/// Cut `s` from the FRONT to at most `max` display columns, marking the cut
/// with a leading ellipsis. The mirror of [`truncate_display`], for values
/// whose tail is the informative end — a path is named by where it ends, so
/// dropping `/scratchpad/h-inv` to keep `/private/tmp/agent-501/…` answers
/// nothing.
pub(crate) fn elide_head(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if UnicodeWidthStr::width(flat.as_str()) <= max {
        return flat;
    }
    let budget = max.saturating_sub(1); // reserve a column for '…'
    let mut tail: Vec<char> = Vec::new();
    let mut w = 0;
    for ch in flat.chars().rev() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        tail.push(ch);
        w += cw;
    }
    let mut out = String::from("…");
    out.extend(tail.iter().rev());
    out
}

#[cfg(test)]
mod pad_tests {
    use super::{elide_head, pad_display};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn a_cut_path_keeps_its_tail_and_says_it_was_cut() {
        let out = elide_head("/a/very/long/prefix/scratchpad/h-inv", 20);
        assert!(out.starts_with('…'), "{out:?}");
        assert!(out.ends_with("h-inv"), "{out:?}");
        assert!(UnicodeWidthStr::width(out.as_str()) <= 20, "{out:?}");
    }

    #[test]
    fn a_path_that_fits_is_left_alone() {
        assert_eq!(elide_head("/repo", 20), "/repo");
    }

    #[test]
    fn eliding_measures_by_width_not_char_count() {
        let out = elide_head("前缀很长啊啊啊/仓库", 10);
        assert!(UnicodeWidthStr::width(out.as_str()) <= 10, "{out:?}");
        assert!(out.ends_with("仓库"), "{out:?}");
    }

    #[test]
    fn a_label_column_lines_up_across_scripts() {
        for label in ["状态", "运行时长", "退出码", "exit"] {
            assert_eq!(UnicodeWidthStr::width(pad_display(label, 12).as_str()), 12);
        }
    }

    #[test]
    fn a_label_wider_than_its_column_is_left_alone() {
        assert_eq!(pad_display("运行时长", 4), "运行时长");
    }
}
