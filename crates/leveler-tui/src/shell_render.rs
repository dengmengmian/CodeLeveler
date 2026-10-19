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

/// A bounded suffix of output suitable for a live/collapsed preview. Starts at
/// a line boundary so a multibyte character is never split.
pub fn tail_source(output: &str, max_bytes: usize) -> &str {
    if output.len() <= max_bytes {
        return output;
    }
    let mut cut = output.len() - max_bytes;
    while cut < output.len() && !output.is_char_boundary(cut) {
        cut += 1;
    }
    match output[cut..].find('\n') {
        Some(rel) => &output[cut + rel + 1..],
        None => &output[cut..],
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
    fn tail_source_starts_on_a_line_boundary() {
        let text = "aaa\nbbb\nccc\n";
        assert_eq!(tail_source(text, 8), "ccc\n");
    }
}
