//! Apply parsed Update hunks to file content.

use super::parse::UpdateChunk;
use super::seek::seek_sequence;

/// Apply a sequence of update chunks to `original`, returning the new content.
/// Chunks must apply in file order; on any hunk that cannot be located this
/// returns an error and makes no partial change (the caller commits nothing).
pub fn apply_update(original: &str, chunks: &[UpdateChunk]) -> Result<String, String> {
    let mut file = split_lines(original);

    // Resolve every hunk to a (start, len, replacement) op before mutating.
    let mut ops: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut search_from = 0;

    for chunk in chunks {
        let mut start = search_from;
        let mut had_label = false;

        if let Some(label) = &chunk.context_label {
            let pattern = [label.clone()];
            // Exact / whitespace-tolerant full-line match first; if the model
            // only typed a unique stem (`@@ mul_works` for `fn mul_works() {`),
            // recover when exactly one line contains that stem.
            let (idx, soft_stem) = if let Some(i) = seek_sequence(&file, &pattern, start, false) {
                (i, false)
            } else if let Some(i) = unique_line_containing(&file, label, start) {
                (i, true)
            } else {
                return Err(context_label_not_found(&file, label, start));
            };
            // Exact `@@` is an anchor *before* the hunk body (search after it).
            // Soft unique stems that locate a body are usually a prefix of the
            // first old line — search from that line. Pure soft additions
            // (empty old_lines) still insert *after* the matched line, same as
            // exact `@@` (never prepend on top of the anchor).
            start = if soft_stem && !chunk.old_lines.is_empty() {
                idx
            } else {
                idx + 1
            };
            had_label = true;
        }

        let mut replacement = chunk.new_lines.clone();
        let (match_start, matched_len) = if chunk.old_lines.is_empty() {
            // A context-less pure addition means "append at end";
            // only a `@@` label pins it to a specific spot.
            let at = if had_label {
                start.min(file.len())
            } else {
                file.len()
            };
            (at, 0)
        } else {
            match seek_sequence(&file, &chunk.old_lines, start, chunk.is_eof) {
                Some(i) => (i, chunk.old_lines.len()),
                None => {
                    let m = retry_without_trailing_blank(&file, chunk, start)?;
                    // The pattern lost its trailing blank; drop the matching
                    // one in the replacement or a stray empty line lands
                    // mid-file (at EOF the newline normalization hides it).
                    if replacement.last().is_some_and(String::is_empty) {
                        replacement.pop();
                    }
                    m
                }
            }
        };

        ops.push((match_start, matched_len, replacement));
        // A pure-append hunk occupies no span in the file, so it must not
        // advance the cursor — otherwise the next hunk would search past EOF.
        if !chunk.old_lines.is_empty() {
            search_from = match_start + matched_len;
        }
    }

    // Apply bottom-up so earlier indices stay valid. Sort by start first so a
    // hunk authored out of file order (e.g. an EOF append listed before an
    // edit) still splices at a valid, non-shifting index.
    ops.sort_by_key(|&(start, _, _)| start);
    for (start, len, replacement) in ops.into_iter().rev() {
        let end = (start + len).min(file.len());
        let start = start.min(file.len());
        file.splice(start..end, replacement);
    }

    // Normalize to a trailing newline on any non-empty result, regardless of
    // whether the original had one (files must end with a newline).
    let mut out = file.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// If a hunk's `old_lines` ends in a blank line that prevents a match, retry
/// without that trailing blank (EOF-newline tolerance).
fn retry_without_trailing_blank(
    file: &[String],
    chunk: &UpdateChunk,
    start: usize,
) -> Result<(usize, usize), String> {
    if chunk.old_lines.last().map(|s| s.is_empty()) == Some(true) {
        let trimmed = &chunk.old_lines[..chunk.old_lines.len() - 1];
        if let Some(i) = seek_sequence(file, trimmed, start, chunk.is_eof) {
            return Ok((i, trimmed.len()));
        }
    }
    Err(hunk_not_found(file, chunk, start))
}

/// Build the error for a hunk whose `old_lines` match nowhere. Showing the file
/// at `start` is near-useless: without a `@@` label `start` is line 1, so the
/// model gets the top of the file while its hunk targets line 300. The failure
/// is almost always a body the model approximated around a real anchor, so
/// anchor on the hunk's first meaningful line, find where THAT lives, and show
/// the file there — the model can then copy the real text back verbatim.
fn hunk_not_found(file: &[String], chunk: &UpdateChunk, start: usize) -> String {
    let wanted = chunk.old_lines.join("\n");
    let head = format!(
        "could not find expected lines (searched from line {}):\n{wanted}\n",
        start + 1,
    );
    let content = file.join("\n");

    match crate::tools::locate_hint::real_text_at_anchor(&content, &wanted, MAX_HINT_SITES) {
        Some(hint) => {
            format!("{head}The hunk's lines must match the file character-for-character. {hint}")
        }
        // The anchor is nowhere in the file: the hunk targets content that does
        // not exist. That is a different failure, and it needs a different fix.
        None => format!(
            "{head}None of those lines exist in the file. Read the file before patching it.\n\
             --- file content near line {} ---\n{}",
            start + 1,
            file_excerpt(file, start),
        ),
    }
}

/// How many places to show when a hunk's anchor line occurs more than once.
const MAX_HINT_SITES: usize = 3;

/// When `@@` is a unique substring of exactly one line (from `start` onward),
/// treat that line as the context anchor. Ambiguous stems still fail so the
/// model must re-read rather than patch the wrong site.
fn unique_line_containing(file: &[String], label: &str, start: usize) -> Option<usize> {
    let needle = label.trim();
    if needle.is_empty() {
        return None;
    }
    let hits: Vec<usize> = file
        .iter()
        .enumerate()
        .skip(start)
        .filter(|(_, line)| line.contains(needle))
        .map(|(i, _)| i)
        .collect();
    if hits.len() == 1 { Some(hits[0]) } else { None }
}

/// Build the error for a `@@` context line that has no exact match. A weak
/// model's top failure here is giving a PREFIX of the real line (e.g. `func
/// CountTokens` for `func CountTokens(text string) int {`), or an ambiguous
/// stem that matches several lines. Both are unrecoverable from a line-1 excerpt.
/// So scan the whole file for lines that contain the label and echo them with
/// their real line numbers — the model then copies the exact full line back.
/// Falls back to the local excerpt when nothing contains the label at all.
fn context_label_not_found(file: &[String], label: &str, start: usize) -> String {
    let needle = label.trim();
    let candidates: Vec<(usize, &String)> = file
        .iter()
        .enumerate()
        .filter(|(_, line)| !needle.is_empty() && line.contains(needle))
        .collect();

    if candidates.is_empty() {
        return format!(
            "could not find context line `{label}` (searched from line {}):\n\
             --- file content near line {} ---\n{}",
            start + 1,
            start + 1,
            file_excerpt(file, start),
        );
    }

    const MAX: usize = 8;
    let shown = candidates.len().min(MAX);
    let mut s = format!(
        "no exact match for context line `{label}`. The `@@` context must be the \
         FULL line, character-for-character. {} line(s) contain that text — copy \
         one of these exactly{}:\n",
        candidates.len(),
        if candidates.len() > MAX {
            format!(" (showing first {MAX})")
        } else {
            String::new()
        },
    );
    for (i, line) in candidates.into_iter().take(shown) {
        s.push_str(&format!("{:>4}\u{2502} {line}\n", i + 1));
    }
    s
}

/// Render a windowed excerpt of `file` around 0-based line `around`, with
/// 1-based line numbers, so a locate failure can show the model what the file
/// actually contains there — not just the lines it failed to find. This is the
/// single highest-leverage aid for a weak model's self-correction on retry.
fn file_excerpt(file: &[String], around: usize) -> String {
    let lines: Vec<&str> = file.iter().map(String::as_str).collect();
    crate::tools::locate_hint::excerpt(&lines, around, 4)
}

/// Split file content into lines, dropping the empty element a trailing newline
/// produces.
fn split_lines(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut v: Vec<String> = s.split('\n').map(str::to_string).collect();
    if s.ends_with('\n') {
        v.pop();
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(old: &[&str], new: &[&str]) -> UpdateChunk {
        UpdateChunk {
            context_label: None,
            old_lines: old.iter().map(|s| s.to_string()).collect(),
            new_lines: new.iter().map(|s| s.to_string()).collect(),
            is_eof: false,
        }
    }

    #[test]
    fn replaces_a_line() {
        let original = "a\nb\nc\n";
        let out = apply_update(original, &[chunk(&["b"], &["B"])]).unwrap();
        assert_eq!(out, "a\nB\nc\n");
    }

    #[test]
    fn replaces_with_context_preserved() {
        // context line 'a' kept, 'b' removed, 'X' added
        let out = apply_update("a\nb\nc\n", &[chunk(&["a", "b"], &["a", "X"])]).unwrap();
        assert_eq!(out, "a\nX\nc\n");
    }

    #[test]
    fn insertion_with_context_label() {
        // The `@@` label must be an actual line in the file:
        // it is located first, then the hunk's old_lines are matched after it.
        let mut c = chunk(&["let x = 1;"], &["let x = 1;", "let y = 2;"]);
        c.context_label = Some("fn main() {".to_string());
        let original = "fn main() {\nlet x = 1;\n}\n";
        let out = apply_update(original, &[c]).unwrap();
        assert_eq!(out, "fn main() {\nlet x = 1;\nlet y = 2;\n}\n");
    }

    #[test]
    fn soft_unique_partial_pure_add_inserts_after_anchor_not_before() {
        // Soft `@@ main` must not put pure additions *on top of* `fn main()`.
        let original = "fn main() {\nlet x = 1;\n}\n";
        let mut c = chunk(&[], &["inserted"]);
        c.context_label = Some("main".to_string());
        let out = apply_update(original, &[c]).unwrap();
        assert_eq!(
            out, "fn main() {\ninserted\nlet x = 1;\n}\n",
            "pure add after unique partial @@ must insert after the line: {out}"
        );
        assert!(
            !out.starts_with("inserted"),
            "must not prepend before fn main: {out}"
        );
    }

    #[test]
    fn unique_partial_context_label_recovers_like_dogfood_mul_works() {
        // Live dogfood: model used `@@ mul_works` instead of the full line
        // `    fn mul_works() {`. Unique stem must still anchor.
        let original = "\
pub fn mul(a: i32, b: i32) -> i32 {
    a * b
}

#[cfg(test)]
mod tests {
    #[test]
    fn mul_works() {
        assert_eq!(mul(3, 4), 12);
    }
}
";
        let mut c = chunk(
            &[
                "    fn mul_works() {",
                "        assert_eq!(mul(3, 4), 12);",
                "    }",
            ],
            &[
                "    fn mul_works() {",
                "        assert_eq!(mul(3, 4), 12);",
                "    }",
                "",
                "    #[test]",
                "    fn div_works() {",
                "        assert_eq!(div(1, 1), Some(1));",
                "    }",
            ],
        );
        c.context_label = Some("mul_works".to_string());
        let out = apply_update(original, &[c]).unwrap();
        assert!(
            out.contains("fn div_works()"),
            "partial unique @@ must apply: {out}"
        );
    }

    #[test]
    fn ambiguous_partial_context_label_still_errors() {
        let original = "fn foo_works() {}\nfn bar_works() {}\n";
        let mut c = chunk(&["fn foo_works() {}"], &["fn foo_works() { /* x */ }"]);
        c.context_label = Some("works".to_string());
        let err = apply_update(original, &[c]).unwrap_err();
        assert!(
            err.contains("could not find context line") || err.contains("works"),
            "{err}"
        );
    }

    #[test]
    fn eof_anchored_replacement() {
        let mut c = chunk(&["end"], &["END"]);
        c.is_eof = true;
        let out = apply_update("end\nmid\nend\n", &[c]).unwrap();
        assert_eq!(out, "end\nmid\nEND\n");
    }

    #[test]
    fn trailing_blank_retry_trims_new_lines_too() {
        // old ends with a blank line that is NOT in the file mid-file; the
        // retry drops it from the pattern, so the matching blank in new_lines
        // must be dropped as well or a stray empty line is inserted.
        let out = apply_update("a\nb\nc\n", &[chunk(&["b", ""], &["B", ""])]).unwrap();
        assert_eq!(out, "a\nB\nc\n");
    }

    #[test]
    fn missing_lines_errors() {
        let err = apply_update("a\nb\n", &[chunk(&["zzz"], &["q"])]).unwrap_err();
        assert!(err.contains("could not find"));
    }

    #[test]
    fn contextless_addition_appends_at_eof_not_top() {
        // A pure-addition hunk with no context and no `@@` label means "append";
        // it must land at EOF, not prepend to the top.
        let out = apply_update("a\nb\n", &[chunk(&[], &["added"])]).unwrap();
        assert_eq!(out, "a\nb\nadded\n");
    }

    #[test]
    fn contextless_addition_with_label_inserts_after_label() {
        // With a `@@` label, a context-less addition inserts right after it.
        let mut c = chunk(&[], &["inserted"]);
        c.context_label = Some("fn main() {".to_string());
        let out = apply_update("fn main() {\nx\n}\n", &[c]).unwrap();
        assert_eq!(out, "fn main() {\ninserted\nx\n}\n");
    }

    #[test]
    fn multiple_hunks_in_order() {
        let original = "1\n2\n3\n4\n5\n";
        let out = apply_update(
            original,
            &[chunk(&["2"], &["two"]), chunk(&["4"], &["four"])],
        )
        .unwrap();
        assert_eq!(out, "1\ntwo\n3\nfour\n5\n");
    }

    #[test]
    fn append_hunk_followed_by_edit_hunk() {
        // A pure-append hunk (empty old_lines) does not consume a position in
        // the file, so it must NOT push the search cursor to EOF — otherwise a
        // following edit hunk starts searching past the end and never locates.
        let original = "1\n2\n3\n";
        let out = apply_update(
            original,
            &[chunk(&[], &["appended"]), chunk(&["2"], &["TWO"])],
        )
        .unwrap();
        assert_eq!(out, "1\nTWO\n3\nappended\n");
    }

    #[test]
    fn append_before_a_deletion_hunk_applies_safely() {
        // The append lands at the original EOF while a later hunk shortens the
        // file; splicing bottom-up (by start index) must not panic or misplace.
        let original = "1\n2\n3\n";
        let out = apply_update(original, &[chunk(&[], &["end"]), chunk(&["2"], &[])]).unwrap();
        assert_eq!(out, "1\n3\nend\n");
    }

    #[test]
    fn locate_failure_echoes_file_content_with_line_numbers() {
        // On a miss the error must show what the file ACTUALLY looks like near
        // the search point (with line numbers), not only the lines we failed to
        // find — a weak model needs the real content to rewrite the hunk.
        let original = "alpha\nbeta\ngamma\n";
        let err = apply_update(original, &[chunk(&["nonexistent"], &["x"])]).unwrap_err();
        assert!(err.contains("nonexistent"), "names the target: {err}");
        assert!(err.contains("alpha"), "echoes real file content: {err}");
        assert!(err.contains('1'), "includes line numbers: {err}");
    }

    #[test]
    fn missing_context_label_echoes_file_content() {
        let mut c = chunk(&["x"], &["y"]);
        c.context_label = Some("fn nowhere() {".to_string());
        let err = apply_update("a\nb\nc\n", &[c]).unwrap_err();
        assert!(err.contains("fn nowhere"), "names the label: {err}");
        assert!(err.contains('a'), "echoes real file content: {err}");
    }

    #[test]
    fn partial_context_label_unique_stem_applies_without_retry() {
        // Unique `@@` stem of a real line recovers (dogfood-grade soft anchor).
        // Ambiguous stems still fail via `ambiguous_partial_context_label_still_errors`.
        let pad = "// pad\n".repeat(19); // real func lands on line 20
        let file = format!("{pad}func CountTokens(text string) int {{\n\treturn 0\n}}\n");
        let mut c = chunk(&["\treturn 0"], &["\treturn len(text)"]);
        c.context_label = Some("func CountTokens".to_string());
        let out = apply_update(&file, &[c]).unwrap();
        assert!(
            out.contains("\treturn len(text)"),
            "unique partial @@ must apply the hunk: {out}"
        );
    }

    #[test]
    fn ambiguous_context_label_lists_all_candidates() {
        // `func (c *Client)` is a prefix of several real methods; an exact match
        // is correctly impossible. The error must list every candidate (wherever
        // they sit) so the model can disambiguate, not dump the top of the file.
        let pad = "// pad\n".repeat(14); // methods land on lines 15 and 16
        let file = format!(
            "{pad}func (c *Client) Name() string {{ return c.name }}\n\
             func (c *Client) Close() error {{ return nil }}\n"
        );
        let mut c = chunk(&["x"], &["y"]);
        c.context_label = Some("func (c *Client)".to_string());
        let err = apply_update(&file, &[c]).unwrap_err();
        assert!(err.contains("Name()"), "lists first candidate: {err}");
        assert!(err.contains("Close()"), "lists second candidate: {err}");
    }
    #[test]
    fn a_hunk_that_does_not_match_shows_the_file_where_its_first_line_really_is() {
        let mut content = String::new();
        for i in 1..=40 {
            content.push_str(&format!("// filler {i}\n"));
        }
        content.push_str("func Proxy(u string) error {\n\tlog.Panicln(u)\n\treturn nil\n}\n");

        // The model recalls the signature correctly but approximates the body,
        // so the hunk cannot be located.
        let c = chunk(
            &["func Proxy(u string) error {", "\tlog.Panic(u)"],
            &["func Proxy(u string) error {", "\treturn errors.New(u)"],
        );
        let err = apply_update(&content, &[c]).unwrap_err();

        assert!(
            err.contains("41\u{2502} func Proxy(u string) error {"),
            "the error must show the real file text at the anchor line, with its real \
             line number, so the model can copy it back verbatim; got:\n{err}"
        );
        assert!(
            err.contains("42\u{2502} \tlog.Panicln(u)"),
            "and it must show the lines the hunk got wrong; got:\n{err}"
        );
    }
}
