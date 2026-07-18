//! A minimal inline terminal renderer: committed history is
//! printed into the terminal's normal scrollback (so native scroll + copy just
//! work), and a variable-height "footer" (the live region: in-progress output,
//! status, composer) is redrawn in place at the bottom each frame.
//!
//! This deliberately does NOT use the alternate screen. It manages the cursor
//! itself via ANSI, so all output must be pre-wrapped to the terminal width
//! (line wrapping is disabled by the caller) — an over-long line would desync
//! the row math.
//!
//! The renderer is generic over the output `Write` so its escape-sequence
//! output can be unit-tested without a real terminal.

use std::io::{self, Write};

use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{Clear, ClearType, ScrollUp};
use crossterm::{cursor, queue};
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;

use crate::markdown::{disp_width, grapheme_width};

/// Tracks the transcript/footer geometry so each frame can append below the
/// prior history (top-anchored) and erase exactly the footer
/// it drew.
#[derive(Debug, Default)]
pub struct InlineTerminal {
    /// Absolute viewport row (0-based) just below the last committed history
    /// line. New history is appended here and grows downward; the footer is
    /// pinned below the gap. Starts at 0 (the caller homes the cursor first).
    content_bottom: u16,
    /// Rows the footer occupied last frame (0 before the first draw).
    footer_height: u16,
    /// The cursor's row offset within the footer at the end of last frame, so
    /// we can move back to the footer's top to erase it.
    cursor_row_in_footer: u16,
}

impl InlineTerminal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Draw one frame. `history` is only the NEWLY-committed lines (the delta):
    /// they are appended just below the prior history (top-anchored, growing
    /// downward), and the `footer` (live region) is pinned to the viewport
    /// bottom with a blank gap between. When history reaches the footer the
    /// excess is scrolled into native scrollback. The cursor is left at
    /// `footer_cursor` = (col, row-within-footer).
    ///
    /// `width` is the terminal width; every line is truncated to it.
    /// `footer_cursor` of `None` hides the terminal cursor for this frame
    /// (e.g. while a choice-only overlay is open).
    pub fn frame<W: Write>(
        &mut self,
        w: &mut W,
        width: u16,
        height: u16,
        history: &[Line<'static>],
        footer: &[Line<'static>],
        footer_cursor: Option<(u16, u16)>,
    ) -> io::Result<()> {
        // Clamp the live footer to the viewport, leaving at least one row of
        // headroom above it. Two reasons: a footer taller than the screen (e.g.
        // a long streaming tail) would otherwise scroll its overflow into
        // scrollback on every repaint, stacking duplicate copies; and a footer
        // that fills the entire height (reaching the top row) makes the terminal
        // scroll one line per repaint even when the content is unchanged. Drop
        // the top overflow rows — the composer at the bottom stays; the dropped
        // lines reappear in full once the item finalizes into history.
        let budget = height.saturating_sub(1).max(1);
        let drop = (footer.len() as u16).saturating_sub(budget);
        let footer = &footer[drop as usize..];
        let footer_height = footer.len() as u16;

        // Erase the transient footer (and the gap) that sits below committed
        // history BEFORE any ScrollUp. The footer is live-region content that is
        // reprinted every frame; if ScrollUp runs first, it shoves the stale
        // footer up into native scrollback, and when that same content later
        // finalizes into history it is printed again — so scrolling back shows it
        // twice (the "duplicate render" ghosting). Clearing first means ScrollUp
        // can only carry real history (above) and blank rows into scrollback.
        queue!(
            w,
            cursor::MoveTo(0, self.content_bottom),
            Clear(ClearType::FromCursorDown)
        )?;

        // The footer sits IMMEDIATELY AFTER the committed history so the
        // conversation flows continuously top-down (no blank gap, no jump to the
        // screen bottom). Only when history + footer would overflow the viewport
        // do we scroll the top of history into native scrollback and let the
        // footer settle at the bottom.
        let content_after = self.content_bottom.saturating_add(history.len() as u16);
        let overflow = content_after
            .saturating_add(footer_height)
            .saturating_sub(height);
        let footer_top = if overflow > 0 {
            queue!(w, ScrollUp(overflow))?;
            self.content_bottom = self.content_bottom.saturating_sub(overflow);
            // ScrollUp shifted the cleared region up; reposition to the new end
            // of committed history before appending the delta.
            queue!(w, cursor::MoveTo(0, self.content_bottom))?;
            height.saturating_sub(footer_height)
        } else {
            content_after
        };

        // Append the new history lines contiguously; the cursor then rests at
        // `footer_top`, right where the footer begins.
        for line in history {
            write_line(w, line, width)?;
            queue!(w, Print("\r\n"))?;
        }
        self.content_bottom = footer_top;

        // Draw the footer right after the history.
        queue!(w, cursor::MoveTo(0, footer_top))?;
        for (i, line) in footer.iter().enumerate() {
            write_line(w, line, width)?;
            if i + 1 < footer.len() {
                queue!(w, Print("\r\n"))?;
            }
        }

        // 5. Park the cursor at the requested footer position (shifted up by any
        //    dropped top rows), or hide it when the frame has no text input.
        let last_row = footer_height.saturating_sub(1);
        let cy = match footer_cursor {
            Some((cx, cy)) => {
                let cy = cy.saturating_sub(drop).min(last_row);
                // The cursor is currently on `last_row`; move up to the target.
                if last_row > cy {
                    queue!(w, cursor::MoveUp(last_row - cy))?;
                }
                queue!(w, cursor::MoveToColumn(cx), cursor::Show)?;
                cy
            }
            None => {
                queue!(w, cursor::Hide)?;
                last_row
            }
        };

        self.footer_height = footer_height;
        self.cursor_row_in_footer = cy;
        w.flush()
    }

    /// Forget the tracked footer so the next frame repaints from the cursor
    /// down without trying to erase stale geometry (used after a resize).
    pub fn reset(&mut self) {
        self.content_bottom = 0;
        self.footer_height = 0;
        self.cursor_row_in_footer = 0;
    }

    /// Erase the footer entirely (e.g. on exit), leaving history intact.
    pub fn clear_footer<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        if self.footer_height > 0 {
            if self.cursor_row_in_footer > 0 {
                queue!(w, cursor::MoveUp(self.cursor_row_in_footer))?;
            }
            queue!(w, cursor::MoveToColumn(0), Clear(ClearType::FromCursorDown))?;
        }
        self.footer_height = 0;
        self.cursor_row_in_footer = 0;
        w.flush()
    }
}

/// Write one styled line as ANSI, truncated to `width` display columns.
fn write_line<W: Write>(w: &mut W, line: &Line<'static>, width: u16) -> io::Result<()> {
    let max = width as usize;
    let mut used = 0usize;
    for span in &line.spans {
        if used >= max {
            break;
        }
        let content = &span.content;
        let piece = truncate_to(content, max - used);
        if piece.is_empty() {
            continue;
        }
        used += disp_width(&piece);
        apply_style(w, span.style)?;
        queue!(w, Print(piece), SetAttribute(Attribute::Reset))?;
    }
    // Erase whatever this row held past the text we just wrote. Scrolling
    // repaints rows the frame's own clear no longer covers, so a shorter line
    // landing on a longer one would otherwise keep the old tail and splice two
    // lines into one on screen.
    queue!(w, Clear(ClearType::UntilNewLine))?;
    Ok(())
}

/// Truncate `s` to at most `max` display columns (whole graphemes), measuring
/// with the terminal-truthful width (emoji = 2) and neutralizing control chars
/// (a raw tab expands, a raw \r returns the cursor to column 0) so a piece can't
/// desync the inline row math or corrupt the line.
fn truncate_to(s: &str, max: usize) -> String {
    let clean: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if disp_width(&clean) <= max {
        return clean;
    }
    let mut out = String::new();
    let mut w = 0usize;
    for g in clean.graphemes(true) {
        let gw = grapheme_width(g);
        if w + gw > max {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out
}

/// Apply a ratatui style's foreground + bold/italic to the writer.
fn apply_style<W: Write>(w: &mut W, style: ratatui::style::Style) -> io::Result<()> {
    if let Some(color) = style.fg {
        write_fg(w, color)?;
    }
    if style.add_modifier.contains(Modifier::BOLD) {
        queue!(w, SetAttribute(Attribute::Bold))?;
    }
    if style.add_modifier.contains(Modifier::ITALIC) {
        queue!(w, SetAttribute(Attribute::Italic))?;
    }
    Ok(())
}

/// Write a foreground color as ANSI. The inline renderer already owns ANSI
/// output directly, so this avoids terminal-backend differences in tests.
fn write_fg<W: Write>(w: &mut W, c: Color) -> io::Result<()> {
    match c {
        Color::Reset => write!(w, "\x1b[39m"),
        Color::Black => write!(w, "\x1b[30m"),
        Color::Red => write!(w, "\x1b[31m"),
        Color::Green => write!(w, "\x1b[32m"),
        Color::Yellow => write!(w, "\x1b[33m"),
        Color::Blue => write!(w, "\x1b[34m"),
        Color::Magenta => write!(w, "\x1b[35m"),
        Color::Cyan => write!(w, "\x1b[36m"),
        Color::Gray => write!(w, "\x1b[37m"),
        Color::DarkGray => write!(w, "\x1b[90m"),
        Color::LightRed => write!(w, "\x1b[91m"),
        Color::LightGreen => write!(w, "\x1b[92m"),
        Color::LightYellow => write!(w, "\x1b[93m"),
        Color::LightBlue => write!(w, "\x1b[94m"),
        Color::LightMagenta => write!(w, "\x1b[95m"),
        Color::LightCyan => write!(w, "\x1b[96m"),
        Color::White => write!(w, "\x1b[97m"),
        Color::Rgb(r, g, b) => write!(w, "\x1b[38;2;{r};{g};{b}m"),
        Color::Indexed(i) => write!(w, "\x1b[38;5;{i}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;
    use ratatui::text::Span;

    fn render(
        term: &mut InlineTerminal,
        history: &[Line<'static>],
        footer: &[Line<'static>],
        cur: (u16, u16),
    ) -> String {
        let mut buf: Vec<u8> = Vec::new();
        term.frame(&mut buf, 80, 24, history, footer, Some(cur))
            .unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn truncate_to_measures_emoji_and_neutralizes_control_chars() {
        // Emoji occupies 2 columns (terminal-truthful), not 1.
        assert_eq!(truncate_to("🔴x", 2), "🔴");
        // A raw tab/CR would expand or reset the cursor — neutralize to a space.
        let out = truncate_to("a\tb\rc", 10);
        assert!(!out.contains('\t') && !out.contains('\r'));
        assert_eq!(out, "a b c");
    }

    #[test]
    fn markdown_https_link_paints_clean_text_via_write_line() {
        // Drive the real paint path (write_line → truncate_to) with lines from
        // the shipped markdown renderer. OSC-in-span would become visible
        // garbage like " ]8;;https://…\\docs"; underline-only must stay clean.
        let doc = crate::markdown::MdDoc::parse("see [docs](https://example.com/path) please");
        let lines = doc.to_lines(80, &crate::theme::Theme::dark());
        let mut buf = Vec::new();
        for line in &lines {
            write_line(&mut buf, line, 80).unwrap();
            buf.push(b'\n');
        }
        let painted = String::from_utf8(buf).unwrap();
        // Visible text must show clean link label + surroundings.
        assert!(
            painted.contains("docs"),
            "link label missing from paint: {painted:?}"
        );
        assert!(
            painted.contains("see") && painted.contains("please"),
            "surrounding text missing: {painted:?}"
        );
        // truncate_to turns ESC into space, so a leaked OSC would leave "]8;;"
        // and the raw URL as displayable characters — that must not happen.
        assert!(
            !painted.contains("]8;;"),
            "OSC 8 payload leaked into paint path: {painted:?}"
        );
        assert!(
            !painted.contains("example.com"),
            "URL must not paint as visible text via truncate_to: {painted:?}"
        );
        // Direct truncate_to on a malicious OSC span still neutralizes ESC,
        // documenting why we never embed OSC in Span content for this path.
        let mangled = truncate_to(
            "\u{1b}]8;;https://example.com/path\u{1b}\\docs\u{1b}]8;;\u{1b}\\",
            80,
        );
        assert!(
            mangled.contains("]8;;") || mangled.contains("example.com"),
            "control: ESC-in-content is mangled by truncate_to: {mangled:?}"
        );
        assert!(!mangled.contains('\u{1b}'));
    }

    #[test]
    fn footer_anchors_after_history_not_pinned_to_bottom() {
        let mut t = InlineTerminal::new();
        // 3 committed lines + a 1-line footer in a 24-row viewport: the footer
        // sits right after the history (row 4), not jumped to the bottom (24).
        let out = render(
            &mut t,
            &[Line::from("a"), Line::from("b"), Line::from("c")],
            &[Line::from("› hi")],
            (4, 0),
        );
        assert!(out.contains("› hi"));
        assert!(
            out.contains("\u{1b}[4;1H"),
            "footer anchored right after history (row 4): {out:?}"
        );
        assert!(
            !out.contains("\u{1b}[24;1H"),
            "must NOT pin to the viewport bottom: {out:?}"
        );
        assert_eq!(t.footer_height, 1);
    }

    #[test]
    fn second_frame_erases_gap_and_footer_before_reprinting() {
        let mut t = InlineTerminal::new();
        render(&mut t, &[], &[Line::from("a"), Line::from("b")], (0, 1));
        // Now a history line commits and the footer shrinks to one row.
        let out = render(
            &mut t,
            &[Line::from("committed")],
            &[Line::from("x")],
            (0, 0),
        );
        // It repositions to the top of history and clears downward (gap + old
        // footer) before reprinting.
        assert!(out.contains("\u{1b}[1;1H"), "should MoveTo row 1: {out:?}");
        assert!(out.contains("\u{1b}[J"), "should clear from cursor down");
        assert!(out.contains("committed"), "history line printed");
        assert!(out.contains('x'), "new footer printed");
    }

    #[test]
    fn history_beyond_footer_scrolls_into_scrollback() {
        // A 3-row viewport with a 1-row footer leaves 2 rows for history.
        let mut t = InlineTerminal::new();
        let mut buf: Vec<u8> = Vec::new();
        t.frame(
            &mut buf,
            80,
            3,
            &[Line::from("l1")],
            &[Line::from("› ")],
            Some((2, 0)),
        )
        .unwrap();
        assert_eq!(t.content_bottom, 1);
        // Commit two more lines: only two rows fit above the footer, so one row
        // must scroll off the top.
        let mut buf: Vec<u8> = Vec::new();
        t.frame(
            &mut buf,
            80,
            3,
            &[Line::from("l2"), Line::from("l3")],
            &[Line::from("› ")],
            Some((2, 0)),
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("\u{1b}[1S"), "should ScrollUp 1: {out:?}");
        assert_eq!(t.content_bottom, 2, "history pinned just above footer");
    }

    #[test]
    fn appended_history_anchors_below_prior_history_not_at_footer() {
        // Frame 1: only a footer, no history (the welcome-less start). The
        // footer pins to the bottom; content_bottom stays at the top.
        let mut t = InlineTerminal::new();
        render(
            &mut t,
            &[Line::from("welcome")],
            &[Line::from("› ")],
            (2, 0),
        );
        // welcome occupies row 0; content_bottom advanced past it.
        assert_eq!(t.content_bottom, 1, "history anchored at top");

        // Frame 2: a new user line commits. It must be written just BELOW the
        // welcome (row 1), i.e. after a MoveTo to row 2 (1-based), NOT down at
        // the old footer row near the bottom.
        let out = render(
            &mut t,
            &[Line::from("› 你好")],
            &[Line::from("● ..."), Line::from("› ")],
            (2, 1),
        );
        let hi_at = out.find("你好").expect("user line present");
        let move_to_row2 = out.find("\u{1b}[2;1H").expect("MoveTo row 2 present");
        assert!(
            move_to_row2 < hi_at,
            "user line must be printed at the top (row 2), not the footer: {out:?}"
        );
        assert_eq!(t.content_bottom, 2, "content grew by one row");
    }

    #[test]
    fn footer_taller_than_viewport_is_clamped_not_spilled() {
        // A live footer (e.g. a long streaming tail) taller than the screen must
        // be clamped to the viewport, else printing the overflow scrolls copies
        // into scrollback on every repaint — duplicating content.
        let mut t = InlineTerminal::new();
        let footer: Vec<Line<'static>> = (0..8).map(|i| Line::from(format!("f{i}"))).collect();
        let mut buf: Vec<u8> = Vec::new();
        t.frame(&mut buf, 80, 5, &[], &footer, Some((0, 7)))
            .unwrap();
        let out = String::from_utf8(buf).unwrap();

        // Clamped to height-1 (=4), leaving one row of headroom so the footer
        // never fills the screen (which would scroll one line per repaint).
        assert_eq!(t.footer_height, 4, "footer clamped to height-1");
        assert!(!out.contains("f3"), "dropped top of footer: {out:?}");
        assert!(
            out.contains("f4") && out.contains("f7"),
            "bottom kept: {out:?}"
        );
        // Exactly 3 line breaks (4 clamped rows), never 7 (the unclamped spill).
        assert_eq!(out.matches("\r\n").count(), 3, "no spill past the screen");

        // A second identical frame must be stable — no repeated ScrollUp.
        let mut buf2: Vec<u8> = Vec::new();
        t.frame(&mut buf2, 80, 5, &[], &footer, Some((0, 7)))
            .unwrap();
        let out2 = String::from_utf8(buf2).unwrap();
        assert!(
            !out2.contains('S'),
            "no ScrollUp on a stable frame: {out2:?}"
        );
    }

    #[test]
    fn clears_transient_footer_before_scrolling_it_into_scrollback() {
        // Repro of the "scroll up shows duplicate render" ghosting: a streaming
        // message lives in the footer, then finalizes into a large history delta
        // that overflows the viewport. If ScrollUp runs before the footer is
        // cleared, the stale footer is pushed into native scrollback and then
        // reprinted as history — two copies. The clear MUST precede the ScrollUp.
        let mut t = InlineTerminal::new();
        // Frame 1: one committed line + a footer (the live tail).
        render(
            &mut t,
            &[Line::from("h")],
            &[Line::from("streamed tail")],
            (0, 0),
        );
        assert_eq!(t.content_bottom, 1);

        // Frame 2: the message finalizes into 30 history lines (overflow) with a
        // tiny footer, so overflow (>content_bottom) would otherwise scroll the
        // old footer into scrollback.
        let history: Vec<Line<'static>> = (0..30).map(|i| Line::from(format!("m{i}"))).collect();
        let mut buf: Vec<u8> = Vec::new();
        t.frame(
            &mut buf,
            80,
            24,
            &history,
            &[Line::from("› ")],
            Some((2, 0)),
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();

        let clear = out.find("\u{1b}[J").expect("clears from cursor down");
        let scroll = out.find('S').expect("scrolls up (…S)");
        assert!(
            clear < scroll,
            "footer must be cleared BEFORE ScrollUp, else it duplicates: {out:?}"
        );
    }

    #[test]
    fn truncates_overlong_line_to_width() {
        let mut buf: Vec<u8> = Vec::new();
        let long = "x".repeat(200);
        write_line(&mut buf, &Line::from(long), 10).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let xs = s.chars().filter(|c| *c == 'x').count();
        assert_eq!(xs, 10, "line truncated to width");
    }

    #[test]
    fn styled_span_emits_color_and_reset() {
        let mut buf: Vec<u8> = Vec::new();
        let line = Line::from(Span::styled("hi", Style::default().fg(Color::Rgb(1, 2, 3))));
        write_line(&mut buf, &line, 80).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("hi"));
        assert!(s.contains("\u{1b}[38;2;1;2;3m"), "rgb fg set: {s:?}");
    }
    #[test]
    fn every_painted_row_is_erased_to_the_end_of_the_line() {
        // A terminal only forgets what we erase. A row repainted with a SHORTER
        // line keeps the tail of the longer line that was there before, which
        // reaches the user as corrupted text spliced out of two different lines
        // (seen live: "4. Git 提交 & 推送" repainted over "1. Makefile 版本注入删除"
        // rendered as "4. Git 提交 & 推送入删除"). Scrolling repaints rows that the
        // frame's own clear no longer covers, so erasing per row is the only
        // thing that holds regardless of how the content shifted.
        let mut t = InlineTerminal::new();
        let out = render(
            &mut t,
            &[Line::from("history one"), Line::from("history two")],
            &[Line::from("footer one"), Line::from("footer two")],
            (0, 0),
        );
        let erases = out.matches("\u{1b}[K").count();
        assert!(
            erases >= 4,
            "expected an erase-to-end-of-line after each of the 4 painted rows, \
             found {erases}: {out:?}"
        );
    }
}
