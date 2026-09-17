//! Locating a sequence of lines within a file.
//!
//! The comparison is byte-exact, and that is a correctness property rather
//! than strictness for its own sake. A located hunk is applied by splicing the
//! caller's lines over the file's, and an unchanged (` `) context line is part
//! of that splice — so a hunk located by a loose comparison rewrites the file's
//! real bytes on lines the caller never asked to change. Trailing whitespace
//! disappears, typographic quotes become ASCII, spacing is reformatted, all
//! inside a call that reports success and reports nothing about it.
//!
//! The runtime locates what the caller actually specified. It does not infer
//! what the caller probably meant.

/// Find the start index of the first contiguous run in `file` byte-identical
/// to `pattern`, searching from `start`. When `eof` is set the search begins at
/// the end of the file. Returns `None` when no run matches exactly.
pub fn seek_sequence(
    file: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start.min(file.len()));
    }
    if pattern.len() > file.len() {
        return None;
    }

    let search_start = if eof {
        file.len().saturating_sub(pattern.len()).max(start)
    } else {
        start
    };

    let mut i = search_start;
    while i + pattern.len() <= file.len() {
        if (0..pattern.len()).all(|j| file[i + j] == pattern[j]) {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exact_match() {
        let file = v(&["a", "b", "c", "d"]);
        assert_eq!(seek_sequence(&file, &v(&["b", "c"]), 0, false), Some(1));
    }

    #[test]
    fn respects_start() {
        let file = v(&["a", "b", "a", "b"]);
        assert_eq!(seek_sequence(&file, &v(&["a", "b"]), 1, false), Some(2));
    }

    /// A line that differs only in trailing whitespace is a different line.
    /// Matching it would splice the pattern's spelling over the file's, so the
    /// whitespace would vanish from a line nobody asked to change.
    #[test]
    fn trailing_whitespace_is_not_tolerated() {
        let file = v(&["fn main() {  ", "}"]);
        assert_eq!(
            seek_sequence(&file, &v(&["fn main() {", "}"]), 0, false),
            None
        );
    }

    #[test]
    fn eof_anchors_to_end() {
        let file = v(&["x", "end", "x", "end"]);
        assert_eq!(seek_sequence(&file, &v(&["end"]), 0, true), Some(3));
    }

    #[test]
    fn no_match_returns_none() {
        let file = v(&["a", "b"]);
        assert_eq!(seek_sequence(&file, &v(&["z"]), 0, false), None);
    }

    /// Spacing drift inside a line is a different line. `a+b` does not locate
    /// `a + b`, because applying it would rewrite the file's spacing.
    #[test]
    fn internal_spacing_drift_is_not_squashed_away() {
        let file = v(&["fn target(x: u8) -> u8 {", "    x + 1", "}"]);
        assert_eq!(
            seek_sequence(
                &file,
                &v(&["fn target(x: u8) -> u8 {", "    x+1", "}"]),
                0,
                false
            ),
            None
        );
    }

    /// Indentation is program structure. A pattern written flush-left must not
    /// locate the same statement at any nesting level.
    #[test]
    fn indentation_is_not_trimmed_away() {
        let file = v(&["if a:", "    x = 1", "if b:", "        x = 1"]);
        assert_eq!(seek_sequence(&file, &v(&["x = 1"]), 0, false), None);
        // The correctly indented pattern still finds its own line.
        assert_eq!(
            seek_sequence(&file, &v(&["        x = 1"]), 0, false),
            Some(3)
        );
    }

    /// Typographic punctuation is different source bytes. Folding it to ASCII
    /// to find a match would then write the ASCII back over the file.
    #[test]
    fn typographic_punctuation_is_not_folded_to_ascii() {
        let file = v(&["let s = \u{2018}x\u{2019};", "a\u{2014}b", "c\u{00A0}d"]);
        assert_eq!(seek_sequence(&file, &v(&["let s = 'x';"]), 0, false), None);
        assert_eq!(seek_sequence(&file, &v(&["a-b"]), 0, false), None);
        assert_eq!(seek_sequence(&file, &v(&["c d"]), 0, false), None);
        // The real bytes still match themselves.
        assert_eq!(seek_sequence(&file, &v(&["c\u{00A0}d"]), 0, false), Some(2));
    }
}
