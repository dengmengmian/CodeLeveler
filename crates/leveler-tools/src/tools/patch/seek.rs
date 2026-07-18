//! Locating a sequence of lines within a file, with progressively looser
//! whitespace tolerance.

/// Find the start index of the first contiguous run in `file` matching
/// `pattern`, searching from `start`. When `eof` is set the search is anchored
/// to the end of the file. Returns `None` if no run matches under any pass.
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

    // Progressive looseness: exact → trailing ws → edge ws → Unicode punctuation
    // normalize → internal whitespace squash (`a+b` vs `a + b`). Models often
    // re-type a body they just read with spacing drift; squash recovers that.
    let matchers: [fn(&str, &str) -> bool; 5] = [
        |a, b| a == b,
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
        |a, b| normalize(a) == normalize(b),
        |a, b| squash_ws(a) == squash_ws(b),
    ];

    for matches in matchers {
        let mut i = search_start;
        while i + pattern.len() <= file.len() {
            if (0..pattern.len()).all(|j| matches(&file[i + j], &pattern[j])) {
                return Some(i);
            }
            i += 1;
        }
    }
    None
}

/// Drop all whitespace so spacing-only drift still locates a line.
fn squash_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Fold typographic Unicode punctuation to its ASCII equivalent (and trim), so a
/// patch authored in plain ASCII still locates context lines that contain fancy
/// quotes, dashes, or exotic spaces.
fn normalize(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            // Hyphen/dash code points → '-'
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            // Fancy single quotes → '\''
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            // Fancy double quotes → '"'
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            // Non-breaking / exotic spaces → ' '
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
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

    #[test]
    fn trailing_whitespace_tolerated() {
        let file = v(&["fn main() {  ", "}"]);
        assert_eq!(
            seek_sequence(&file, &v(&["fn main() {", "}"]), 0, false),
            Some(0)
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

    #[test]
    fn internal_whitespace_squash_recovers_spacing_drift() {
        let file = v(&["fn target(x: u8) -> u8 {", "    x + 1", "}"]);
        // Model dropped spaces inside the body line it just read.
        assert_eq!(
            seek_sequence(
                &file,
                &v(&["fn target(x: u8) -> u8 {", "    x+1", "}"]),
                0,
                false
            ),
            Some(0),
            "squash pass must locate spacing-drifted body"
        );
    }

    #[test]
    fn unicode_punctuation_normalized_to_ascii() {
        // File has typographic quotes/dash/nbsp; the model's patch used plain
        // ASCII. The matcher normalizes these before comparing.
        let file = v(&["let s = \u{2018}x\u{2019};", "a\u{2014}b", "c\u{00A0}d"]);
        assert_eq!(
            seek_sequence(&file, &v(&["let s = 'x';"]), 0, false),
            Some(0)
        );
        assert_eq!(seek_sequence(&file, &v(&["a-b"]), 0, false), Some(1));
        assert_eq!(seek_sequence(&file, &v(&["c d"]), 0, false), Some(2));
    }
}
