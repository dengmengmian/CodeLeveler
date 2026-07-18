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
    let span = wanted.lines().count().max(1);
    let needle = wanted.lines().find(|l| !l.trim().is_empty())?.trim();
    if needle.is_empty() {
        return None;
    }

    // Widen one rule at a time, stopping at the first that hits. Each rule is a
    // way the model actually gets a line wrong: indentation drift, then a
    // fragment or prefix of the line, then spacing inside the line
    // (`a+b` for `a + b`) — the most common miss of all, and the reason a plain
    // substring search finds nothing.
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
        s.push_str(&format!("--- file content at line {} ---\n", at + 1));
        s.push_str(&excerpt(&file, at, span));
    }
    Some(s)
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
        s.push_str(&format!("{:>4}\u{2502} {line}\n", i + 1));
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

    #[test]
    fn no_anchor_in_the_file_yields_no_hint() {
        assert!(real_text_at_anchor("a\nb\n", "fn nope() {}", 3).is_none());
    }

    #[test]
    fn a_blank_wanted_block_yields_no_hint() {
        assert!(real_text_at_anchor("a\nb\n", "\n  \n", 3).is_none());
    }
}
