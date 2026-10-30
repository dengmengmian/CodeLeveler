//! Terminal-safe projection of user-shell output for the inline record.
//!
//! The runtime streams raw stdout/stderr chunks. This module keeps the renderer
//! free of control-sequence hazards and applies the one terminal behaviour a
//! line-oriented TUI can honour safely: carriage-return overwrite (progress
//! bars). ANSI and other controls are stripped before buffering; `\r` survives
//! through a private sentinel until this projection.

use crate::transcript::OutputMark;

/// One full breath of a running shell's left rail and status glyph.
pub const SHELL_BREATH_PERIOD: std::time::Duration = std::time::Duration::from_millis(1500);

/// A sentinel that survives `sanitize_terminal_output` (it is not a control).
/// User text containing it is vanishingly unlikely; it is replaced back to CR
/// below and never painted.
const CR_SENTINEL: char = '\u{E000}';

/// Sanitize untrusted terminal text while preserving carriage returns.
pub fn sanitize_shell_output(chunk: &str) -> String {
    let marked: String = chunk
        .chars()
        .map(|c| if c == '\r' { CR_SENTINEL } else { c })
        .collect();
    leveler_core::sanitize_terminal_output(&marked).replace(CR_SENTINEL, "\r")
}

/// One final visible output line and the stream it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutputLine {
    pub text: String,
    pub stderr: bool,
}

/// Project raw output into the lines a terminal would leave on screen.
///
/// Only `\r` and `\n` are interpreted. Cursor movement is already stripped by
/// the sanitizer, so a malformed sequence can never move or erase other rows.
pub fn render_output(output: &str, marks: &[OutputMark]) -> Vec<ShellOutputLine> {
    if output.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current: Vec<char> = Vec::new();
    let mut col = 0usize;
    let mut line_start = 0usize;
    let mut newline_at: Option<usize> = None;

    for (index, ch) in output.char_indices() {
        match ch {
            '\n' => {
                let stderr = stream_at(marks, line_start);
                lines.push(ShellOutputLine {
                    text: current.iter().collect(),
                    stderr,
                });
                current.clear();
                col = 0;
                line_start = index + ch.len_utf8();
                newline_at = Some(index);
            }
            '\r' => {
                col = 0;
            }
            _ => {
                if col < current.len() {
                    current[col] = ch;
                } else {
                    while current.len() < col {
                        current.push(' ');
                    }
                    current.push(ch);
                }
                col += 1;
            }
        }
    }
    let _ = newline_at;
    if !current.is_empty() {
        lines.push(ShellOutputLine {
            text: current.iter().collect(),
            stderr: stream_at(marks, line_start),
        });
    }
    lines
}

fn stream_at(marks: &[OutputMark], offset: usize) -> bool {
    marks
        .iter()
        .rev()
        .find(|mark| mark.offset <= offset)
        .is_some_and(|mark| mark.stderr)
}

/// The clean plain-text projection of raw captured output for Copy Output.
///
/// ANSI/VT style sequences are removed and other control characters are
/// neutralized, while `\r` overwrites are resolved exactly as the terminal
/// view resolves them. Tabs, real `│`/`$`/`▏` glyphs, code indentation and
/// every printable Unicode character survive verbatim: this is the source
/// content, not a cleaned-up rendering of it.
pub fn copy_text(raw: &str) -> String {
    let clean = leveler_core::sanitize_terminal_output_preserving_layout(raw);
    let mut out = render_output(&clean, &[])
        .into_iter()
        .map(|line| line.text)
        .collect::<Vec<_>>()
        .join("\n");
    if clean.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Read the complete combined capture for an execution. The caller advertises
/// a directory only after every sidecar reports a finished copy, so a missing
/// file here is a real error (pruned or removed), never a cue to copy a
/// partial tail instead.
pub fn read_complete_capture(directory: &std::path::Path) -> Result<String, String> {
    let path = directory.join("combined.log");
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("完整输出不可用 ({}): {error}", path.display()))?;
    Ok(copy_text(&String::from_utf8_lossy(&bytes)))
}

/// Rows one user-shell output window shows before it starts scrolling. The
/// window is a viewport, never a limit: nothing below is dropped.
pub const SHELL_OUTPUT_WINDOW_ROWS: usize = 12;

/// One visual (already wrapped) row of a terminal output window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutputRow {
    pub text: String,
    pub stderr: bool,
    /// This row continues the previous logical line (a screen wrap), so a copy
    /// rejoins it with nothing instead of inventing a newline.
    pub continued: bool,
}

/// The authoritative projection of a user shell's retained output: every logical
/// line resolved (carriage-return overwrites applied), sanitized and hard-wrapped
/// to `width` cells. The renderer and the scroll owner both read THIS, so the
/// painted rows and the scroll clamp can never describe different layouts.
pub fn output_rows(output: &str, marks: &[OutputMark], width: usize) -> Vec<ShellOutputRow> {
    let room = width.max(1);
    // The live stream is sanitized as it is appended, but a restored snapshot
    // is the runtime's own tail and may still carry style sequences. Sanitizing
    // in the projection means no cell ever paints an escape, whichever path
    // produced the text. Already-clean text passes through unchanged, so the
    // stream marks (byte offsets) keep pointing at the same bytes.
    let clean = sanitize_shell_output(output);
    let mut rows = Vec::new();
    for line in render_output(&clean, marks) {
        let text = crate::render::sanitize_terminal_line(&line.text);
        for (index, wrapped) in wrap_terminal_line(&text, room).into_iter().enumerate() {
            rows.push(ShellOutputRow {
                text: wrapped,
                stderr: line.stderr,
                continued: index > 0,
            });
        }
    }
    rows
}

/// The slice of a bounded output window that is actually painted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutputWindow {
    pub rows: Vec<ShellOutputRow>,
    /// All visual rows the retained output has at this width.
    pub total_rows: usize,
    /// Index of the first painted row.
    pub start: usize,
    pub hidden_above: usize,
    pub hidden_below: usize,
}

/// Painted height for `total` rows: short output keeps its natural height and
/// long output stops growing at [`SHELL_OUTPUT_WINDOW_ROWS`].
pub fn output_window_height(total: usize) -> usize {
    total.min(SHELL_OUTPUT_WINDOW_ROWS)
}

/// Largest scroll offset (in visual rows from the top) for `total` rows.
pub fn output_max_scroll(total: usize) -> usize {
    total.saturating_sub(output_window_height(total))
}

/// Project the window a reader is looking at. `follow` pins the window to the
/// newest rows; otherwise `scroll` is an offset in visual rows from the top,
/// clamped so the window can never leave the content.
pub fn output_window(
    output: &str,
    marks: &[OutputMark],
    width: usize,
    scroll: usize,
    follow: bool,
) -> ShellOutputWindow {
    let rows = output_rows(output, marks, width);
    let total_rows = rows.len();
    let height = output_window_height(total_rows);
    let max_scroll = total_rows.saturating_sub(height);
    let start = if follow {
        max_scroll
    } else {
        scroll.min(max_scroll)
    };
    let end = (start + height).min(total_rows);
    let mut window_rows = rows;
    window_rows.truncate(end);
    window_rows.drain(..start);
    ShellOutputWindow {
        hidden_above: start,
        hidden_below: total_rows.saturating_sub(end),
        rows: window_rows,
        total_rows,
        start,
    }
}

/// Hard-wrap one logical line at `width` display cells, preserving leading
/// indentation and internal spacing. Unlike prose wrapping, terminal output is
/// never re-flowed at word boundaries.
pub fn wrap_terminal_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        let (piece, next) = crate::render::text::take_display_prefix(rest, width);
        out.push(piece);
        rest = next;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carriage_return_overwrites_instead_of_appending() {
        let lines = render_output("progress 10%\rprogress 100%\ndone\n", &[]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "progress 100%");
        assert_eq!(lines[1].text, "done");
    }

    #[test]
    fn carriage_return_never_erases_beyond_the_line() {
        assert_eq!(render_output("abcdef\rxy", &[])[0].text, "xycdef");
    }

    #[test]
    fn stream_marks_survive_interleaving() {
        let out = "a\nb\nc\n";
        let marks = vec![
            OutputMark {
                offset: 0,
                stderr: false,
            },
            OutputMark {
                offset: 2,
                stderr: true,
            },
            OutputMark {
                offset: 4,
                stderr: false,
            },
        ];
        let lines = render_output(out, &marks);
        assert!(!lines[0].stderr);
        assert!(lines[1].stderr);
        assert!(!lines[2].stderr);
    }

    #[test]
    fn sanitize_preserves_cr_and_strips_ansi() {
        let clean = sanitize_shell_output("\u{1b}[31mred\u{1b}[0m\rnext");
        assert!(!clean.contains('\u{1b}'));
        assert!(clean.contains('\r'));
        assert!(clean.contains("red"));
    }

    #[test]
    fn wrapping_preserves_indentation_and_wide_glyphs() {
        assert_eq!(
            wrap_terminal_line("    indented text", 80),
            vec!["    indented text"]
        );
        let rows = wrap_terminal_line("字字字字字", 4);
        assert_eq!(rows, vec!["字字", "字字", "字"]);
    }

    #[test]
    fn copy_text_keeps_source_glyphs_and_strips_only_style() {
        let raw = "\u{1b}[32m│ name │ value │\u{1b}[0m\n\
                   \tSELECT s.id,\n\
                   \u{1b}[31m▏\u{1b}[0m real ▏ glyph\n\
                   $\n+\n-\n@@\n";
        let copied = copy_text(raw);
        assert_eq!(
            copied, "│ name │ value │\n\tSELECT s.id,\n▏ real ▏ glyph\n$\n+\n-\n@@\n",
            "the visual/content glyphs are source text; only ANSI is removed"
        );
        assert!(!copied.contains('\u{1b}'));
    }

    /// A short result has no window at all: its natural height is the window.
    #[test]
    fn a_short_result_is_its_own_window() {
        let output: String = (1..=4).map(|n| format!("row-{n}\n")).collect();
        let window = output_window(&output, &[], 78, 0, true);
        assert_eq!(window.total_rows, 4);
        assert_eq!(window.rows.len(), 4);
        assert_eq!(window.hidden_above, 0);
        assert_eq!(window.hidden_below, 0);
        assert_eq!(output_max_scroll(window.total_rows), 0, "nothing to scroll");
    }

    /// A long result is clamped to the fixed window height, and the hidden rows
    /// are counted on both sides rather than dropped.
    #[test]
    fn a_long_result_is_a_clamped_window_with_counted_edges() {
        let output: String = (1..=100).map(|n| format!("row-{n:03}\n")).collect();
        let tail = output_window(&output, &[], 78, 0, true);
        assert_eq!(tail.rows.len(), SHELL_OUTPUT_WINDOW_ROWS);
        assert_eq!(tail.start, 100 - SHELL_OUTPUT_WINDOW_ROWS);
        assert_eq!(tail.hidden_above, 88);
        assert_eq!(tail.hidden_below, 0);
        assert_eq!(tail.rows.last().unwrap().text, "row-100");

        let head = output_window(&output, &[], 78, 0, false);
        assert_eq!(head.start, 0);
        assert_eq!(head.rows[0].text, "row-001");
        assert_eq!(head.hidden_below, 88);

        // A scroll past the end clamps to the newest page instead of blanking.
        let clamped = output_window(&output, &[], 78, 10_000, false);
        assert_eq!(clamped.start, 88);
        assert_eq!(clamped.rows.last().unwrap().text, "row-100");
    }

    /// Wrapping is part of the projection, so a copy can rejoin a wrapped row
    /// with nothing while a real newline stays a newline.
    #[test]
    fn wrapped_rows_are_marked_as_continuations() {
        let rows = output_rows("aaaaaaaaaa\nb\n", &[], 4);
        let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts, vec!["aaaa", "aaaa", "aa", "b"]);
        let continued: Vec<bool> = rows.iter().map(|row| row.continued).collect();
        assert_eq!(continued, vec![false, true, true, false]);
    }

    /// Stream provenance survives the projection, so a window keeps the
    /// stdout/stderr distinction.
    #[test]
    fn projected_rows_keep_their_stream() {
        let marks = vec![
            OutputMark {
                offset: 0,
                stderr: false,
            },
            OutputMark {
                offset: 4,
                stderr: true,
            },
        ];
        let rows = output_rows("out\nerr\n", &marks, 40);
        assert!(!rows[0].stderr);
        assert!(rows[1].stderr);
    }

    /// ANSI style is removed before wrapping, so a wide glyph's wrap points are
    /// measured on what is actually painted.
    #[test]
    fn ansi_is_stripped_before_wrapping() {
        let rows = output_rows("\u{1b}[31mabcd\u{1b}[0m", &[], 2);
        let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts, vec!["ab", "cd"]);
    }

    #[test]
    fn copy_text_resolves_carriage_return_progress() {
        assert_eq!(
            copy_text("progress 10%\rprogress 100%\ndone\n"),
            "progress 100%\ndone\n"
        );
    }

    #[test]
    fn copy_text_preserves_blank_lines_and_a_trailing_newline() {
        assert_eq!(copy_text("a\n\nb\n"), "a\n\nb\n");
        assert_eq!(copy_text("\n"), "\n");
        assert_eq!(copy_text(""), "");
        assert_eq!(copy_text("no newline"), "no newline");
    }
}
