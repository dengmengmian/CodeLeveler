//! Shared "here is what the file really says" hint for failed edit locates.
//!
//! `apply_patch` and `replace` fail the same way: the model anchors on a line it
//! genuinely read, then approximates the lines around it (whitespace, a dropped
//! argument, a stale body). Telling it only "not found" leaves it to guess
//! again, and it guesses the same way twice. Showing it the real text at the
//! anchor, with real line numbers, is what lets it copy the text back verbatim.

/// Locate `wanted`'s first meaningful line in `content` and render the file
/// around each place it occurs, with 1-based line numbers.
///
/// Returns `None` when the anchor appears nowhere — the edit targets content
/// that does not exist, which is a different failure and deserves a different
/// message.
pub(crate) fn real_text_at_anchor(content: &str, wanted: &str, max_sites: usize) -> Option<String> {
    let file: Vec<&str> = content.lines().collect();
    let wanted_lines: Vec<&str> = wanted.lines().collect();
    let span = wanted_lines.len().max(1);
    let (offset, needle) = wanted_lines
        .iter()
        .enumerate()
        .find(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| (i, l.trim()))?;
    if needle.is_empty() {
        return None;
    }

    // Widen one rule at a time, stopping at the first that hits. Each rule is a
    // way the model actually gets a line wrong: indentation drift, then a
    // fragment or prefix of the line, then spacing inside the line
    // (`a+b` for `a + b`) — the most common miss of all, and the reason a plain
    // substring search finds nothing.
    // A very wide line (minified or generated source) cannot be matched by
    // "does the file contain the whole line": one character of drift makes the
    // search fail entirely and the caller falls back to "none of those lines
    // exist", which is both wrong and useless. Anchor on the line's bounded
    // PREFIX instead — a hint may be fuzzy, an apply never is.
    let prefix;
    let needle = if needle.chars().count() > ANCHOR_PREFIX_CHARS {
        prefix = needle.chars().take(ANCHOR_PREFIX_CHARS).collect::<String>();
        prefix.as_str()
    } else {
        needle
    };
    let squashed = squash(needle);
    let mut hits: Vec<usize> = matches(&file, |l| l.trim() == needle);
    if hits.is_empty() {
        hits = matches(&file, |l| l.contains(needle));
    }
    if hits.is_empty() && !squashed.is_empty() {
        hits = matches(&file, |l| squash(l) == squashed);
    }
    if hits.is_empty() {
        return None;
    }

    let total = hits.len();
    let mut s = format!(
        "Its first line appears at {total} location(s){} — this is what the file REALLY \
         contains there; copy it back exactly:\n",
        if total > max_sites {
            format!(" (showing first {max_sites})")
        } else {
            String::new()
        },
    );
    for at in hits.into_iter().take(max_sites) {
        let difference = first_difference(&file, at.checked_sub(offset), &wanted_lines);
        s.push_str(&format!("--- file content at line {} ---\n", at + 1));
        // The difference goes FIRST. A minified source line is thousands of
        // characters wide, and the tool result is capped: if the excerpt came
        // first, the one sentence that names the actual mismatch could be the
        // part that gets cut. (A failed `apply_patch` against a single-line
        // `OPCApp.tsx` produced exactly that: a multi-kilobyte "could not find
        // expected lines" with the actionable line outside the cap.)
        let focus = difference.as_ref().and_then(|(_, column)| *column);
        if let Some((text, _)) = difference {
            s.push_str(&text);
        }
        s.push_str(&windowed_excerpt(&file, at, span, focus));
    }
    Some(s)
}

/// Line the hunk up with the file where its anchor was found and name the
/// first line that is not identical — a one-character slip is easy to miss
/// in an excerpt.
/// The first line the hunk and the file disagree on, with the column of the
/// first differing character so a very wide line can be windowed at the place
/// that matters.
fn first_difference(
    file: &[&str],
    start: Option<usize>,
    wanted: &[&str],
) -> Option<(String, Option<usize>)> {
    let start = start?;
    wanted.iter().enumerate().find_map(|(i, hunk)| {
        let at = start + i;
        let actual = file.get(at).copied();
        (actual != Some(*hunk)).then(|| match actual {
            Some(actual) => {
                let column = actual
                    .char_indices()
                    .zip(hunk.chars())
                    .find(|((_, a), b)| a != b)
                    .map(|((index, _), _)| actual[..index].chars().count());
                (
                    format!(
                        "first difference at file line {}:\n  file: `{}`\n  hunk: `{}`\n",
                        at + 1,
                        bounded_line(actual, column),
                        bounded_line(hunk, column),
                    ),
                    column,
                )
            }
            None => (
                format!(
                    "first difference at file line {}: the file ends there, the hunk continues with `{hunk}`\n",
                    at + 1
                ),
                None,
            ),
        })
    })
}

/// Render `file` around 0-based line `around`, wide enough to cover a `span`-line
/// edit. A window shorter than the edit would hide the very lines it got wrong.
pub(crate) fn excerpt(file: &[&str], around: usize, span: usize) -> String {
    if file.is_empty() {
        return "(file is empty)\n".to_string();
    }
    const LEAD: usize = 2;
    const TRAIL: usize = 2;
    let start = around.saturating_sub(LEAD);
    let end = (around + span.max(1) + TRAIL).min(file.len());
    let mut s = String::new();
    for (i, line) in file.iter().enumerate().take(end).skip(start) {
        s.push_str(&format!(
            "{:>4}\u{2502} {}\n",
            i + 1,
            bounded_line(line, None)
        ));
    }
    s
}

/// Long lines are windowed, never printed whole.
///
/// A minified or generated line can be tens of thousands of characters. Echoing
/// it into an error message burns the tool-result budget on text the model
/// already has, and the cap then cuts the message before the part that says what
/// is wrong. Under-matching is not a risk here: the hint only ever quotes the
/// file's own bytes.
const MAX_HINT_LINE_CHARS: usize = 240;
/// How much of a very wide line is used to find where it lives.
const ANCHOR_PREFIX_CHARS: usize = 120;
/// Characters kept on each side of the focus column inside a long line.
const HINT_LINE_WINDOW: usize = 100;

pub(crate) fn bounded_line(line: &str, focus: Option<usize>) -> String {
    let total = line.chars().count();
    if total <= MAX_HINT_LINE_CHARS {
        return line.to_string();
    }
    let focus = focus.unwrap_or(0).min(total);
    let start = focus.saturating_sub(HINT_LINE_WINDOW);
    let end = (start + MAX_HINT_LINE_CHARS).min(total);
    let slice: String = line.chars().skip(start).take(end - start).collect();
    format!("…[{start}..{end} of {total} chars] {slice} …")
}

fn windowed_excerpt(file: &[&str], around: usize, span: usize, focus: Option<usize>) -> String {
    if file.is_empty() {
        return "(file is empty)\n".to_string();
    }
    const LEAD: usize = 2;
    const TRAIL: usize = 2;
    let start = around.saturating_sub(LEAD);
    let end = (around + span.max(1) + TRAIL).min(file.len());
    let mut s = String::new();
    for (i, line) in file.iter().enumerate().take(end).skip(start) {
        s.push_str(&format!(
            "{:>4}\u{2502} {}\n",
            i + 1,
            bounded_line(line, if i == around { focus } else { None })
        ));
    }
    s
}

/// A line with every whitespace character removed, so `a+b;` and `  a + b;`
/// compare equal.
fn squash(line: &str) -> String {
    line.chars().filter(|c| !c.is_whitespace()).collect()
}

fn matches(file: &[&str], pred: impl Fn(&str) -> bool) -> Vec<usize> {
    file.iter()
        .enumerate()
        .filter(|(_, l)| pred(l))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_at_the_anchor_not_the_top_of_the_file() {
        let content = "a\nb\nc\nfn target(x: u8) -> u8 {\n    x + 1\n}\n";
        let hint = real_text_at_anchor(content, "fn target(x: u8) -> u8 {\n    x+1", 3).unwrap();
        assert!(
            hint.contains("4\u{2502} fn target(x: u8) -> u8 {"),
            "{hint}"
        );
        assert!(hint.contains("5\u{2502}     x + 1"), "{hint}");
    }

    /// The excerpt alone was not enough: a model missed a one-character
    /// difference (`"#,` for `"#`) four times running, then rewrote the whole
    /// file. The hint names the first line that differs, side by side.
    #[test]
    fn names_the_first_line_that_differs() {
        let content = "fn t() {\n    write(\n        r#\"\n[a]\n\"#\n    )\n}\n";
        let wanted = "    write(\n        r#\"\n[a]\n\"#,\n    )";
        let hint = real_text_at_anchor(content, wanted, 3).unwrap();
        assert!(hint.contains("first difference at file line 5"), "{hint}");
        assert!(hint.contains("file: `\"#`"), "{hint}");
        assert!(hint.contains("hunk: `\"#,`"), "{hint}");
    }

    /// A single very wide line must not blow the hint (and the tool-result
    /// budget) up, and the actionable difference must survive.
    #[test]
    fn a_minified_line_is_windowed_around_the_difference() {
        let prefix = "x".repeat(4000);
        let line = format!("{prefix}const a = b + c;{prefix}");
        let content = format!("before\n{line}\nafter\n");
        let wanted = format!("{prefix}const a = b - c;{prefix}");
        let hint = real_text_at_anchor(&content, &wanted, 3).unwrap();
        assert!(
            hint.chars().count() < 2_000,
            "a wide line must be windowed: {} chars",
            hint.chars().count()
        );
        assert!(hint.contains("first difference at file line 2"), "{hint}");
        assert!(
            hint.contains("const a = b + c;"),
            "the real text at the difference is shown: {hint}"
        );
        assert!(
            hint.contains("chars]"),
            "the window says how much was elided: {hint}"
        );
    }

    #[test]
    fn no_anchor_in_the_file_yields_no_hint() {
        assert!(real_text_at_anchor("a\nb\n", "fn nope() {}", 3).is_none());
    }

    #[test]
    fn a_blank_wanted_block_yields_no_hint() {
        assert!(real_text_at_anchor("a\nb\n", "\n  \n", 3).is_none());
    }
}
