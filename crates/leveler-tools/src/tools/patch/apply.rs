//! Apply parsed Update hunks to file content.

use super::parse::UpdateChunk;
use super::seek::seek_sequence;

/// The runtime's placeholder for a value hidden from the model.
const REDACTED: &str = leveler_core::secret::SECRET_PLACEHOLDER;

/// What the model needs to know when a hunk carries the placeholder.
const REDACTION_NOTE: &str = "Tool output replaces secret values with `[REDACTED]`, \
which is a placeholder for a hidden value — not the file's literal text.";

/// A resolved hunk: the splice plus the lines to show in the applied diff.
///
/// `display_old` is deliberately separate from the file's real lines: a masked
/// match must not put the hidden credential into user-visible metadata.
struct ResolvedOp {
    start: usize,
    len: usize,
    replacement: Vec<String>,
    display_old: Vec<String>,
}

/// Apply a sequence of update chunks to `original`, returning the new content.
/// Chunks must apply in file order; on any hunk that cannot be located this
/// returns an error and makes no partial change (the caller commits nothing).
pub fn apply_update(original: &str, chunks: &[UpdateChunk]) -> Result<String, String> {
    apply_update_located(original, chunks).map(|(content, _)| content)
}

/// Apply update chunks and report WHERE each one landed.
///
/// A hunk is located by content, so its line numbers are discovered here and
/// nowhere else: the patch text the model wrote carries none. Callers that
/// present the edit need this, and must never re-derive a position from the
/// request instead.
pub fn apply_update_located(
    original: &str,
    chunks: &[UpdateChunk],
) -> Result<(String, Vec<crate::tools::applied_diff::AppliedHunk>), String> {
    // Preserve the file's byte conventions: strip a leading BOM and fold CRLF to
    // LF for matching (the model's hunks are LF), then restore both on write —
    // otherwise a CRLF file comes back with mixed endings and a BOM file's
    // first-line context never matches.
    let had_bom = original.starts_with('\u{FEFF}');
    let stripped = original.strip_prefix('\u{FEFF}').unwrap_or(original);
    let uses_crlf = detect_crlf(stripped);
    let normalized = if uses_crlf {
        stripped.replace("\r\n", "\n")
    } else {
        stripped.to_string()
    };
    let mut file = split_lines(&normalized);

    // Resolve every hunk to an op before mutating.
    let mut ops: Vec<ResolvedOp> = Vec::new();
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
        let carries_redaction = chunk.old_lines.iter().any(|line| line.contains(REDACTED));
        // A placeholder is not a value: writing it back would replace the real
        // credential with the literal text `[REDACTED]`.
        if carries_redaction && replacement.iter().any(|line| line.contains(REDACTED)) {
            return Err(redaction_write_refused());
        }

        let (match_start, matched_len) = if chunk.old_lines.is_empty() {
            // A context-less pure addition means "append at end";
            // only a `@@` label pins it to a specific spot.
            let at = if had_label {
                start.min(file.len())
            } else {
                file.len()
            };
            (at, 0)
        } else if carries_redaction {
            // Exact first: a file may literally contain the placeholder.
            if let Some(i) = seek_sequence(&file, &chunk.old_lines, start, chunk.is_eof) {
                (i, chunk.old_lines.len())
            } else {
                match seek_redacted(&file, &chunk.old_lines, start, chunk.is_eof) {
                    RedactedSeek::Found(i) => (i, chunk.old_lines.len()),
                    RedactedSeek::Ambiguous { count } => {
                        return Err(redacted_ambiguous(count));
                    }
                    RedactedSeek::None => {
                        return Err(redacted_not_found(&chunk.old_lines));
                    }
                }
            }
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

        // A masked match must not surface the hidden value in the applied diff.
        let display_old = if carries_redaction {
            chunk.old_lines.clone()
        } else {
            let end = (match_start + matched_len).min(file.len());
            file[match_start.min(file.len())..end].to_vec()
        };
        ops.push(ResolvedOp {
            start: match_start,
            len: matched_len,
            replacement,
            display_old,
        });
        // A pure-append hunk occupies no span in the file, so it must not
        // advance the cursor — otherwise the next hunk would search past EOF.
        if !chunk.old_lines.is_empty() {
            search_from = match_start + matched_len;
        }
    }

    // Apply bottom-up so earlier indices stay valid. Sort by start first so a
    // hunk authored out of file order (e.g. an EOF append listed before an
    // edit) still splices at a valid, non-shifting index.
    ops.sort_by_key(|op| op.start);
    // Record each landing site against the file as it is now, and against the
    // file as it will be once earlier hunks have shifted it.
    let mut located = Vec::with_capacity(ops.len());
    let mut delta: i64 = 0;
    for op in &ops {
        let old_start = op.start.min(file.len()) + 1;
        let new_start = (old_start as i64 + delta).max(1) as usize;
        delta += op.replacement.len() as i64 - op.display_old.len() as i64;
        located.push(crate::tools::applied_diff::AppliedHunk {
            old_start,
            old_lines: op.display_old.clone(),
            new_start,
            new_lines: op.replacement.clone(),
        });
    }
    for op in ops.into_iter().rev() {
        let end = (op.start + op.len).min(file.len());
        let start = op.start.min(file.len());
        file.splice(start..end, op.replacement);
    }

    // Normalize to a trailing newline on any non-empty result, regardless of
    // whether the original had one (files must end with a newline).
    let mut out = file.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    // Restore the original line ending and BOM.
    if uses_crlf {
        out = out.replace('\n', "\r\n");
    }
    if had_bom {
        out.insert(0, '\u{FEFF}');
    }
    Ok((out, located))
}

/// Whether the file predominantly uses CRLF line endings, so a rewrite should
/// too. Counts `\r\n` against LF-only line endings; ties break toward LF.
fn detect_crlf(s: &str) -> bool {
    let crlf = s.matches("\r\n").count();
    let lf_only = s.matches('\n').count() - crlf;
    crlf > lf_only
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

/// What a redaction-aware search found.
enum RedactedSeek {
    Found(usize),
    Ambiguous { count: usize },
    None,
}

/// Locate a hunk whose `old_lines` contain the runtime placeholder.
///
/// This is not fuzzy matching. Every non-placeholder line must be byte-equal;
/// on a placeholder line the text before and after the placeholder must match
/// and the hidden span must be non-empty. The result must be unique: two
/// candidate sites are an ambiguity the runtime refuses to guess through.
fn seek_redacted(file: &[String], pattern: &[String], start: usize, eof: bool) -> RedactedSeek {
    if pattern.is_empty() {
        return RedactedSeek::Found(start.min(file.len()));
    }
    if pattern.len() > file.len() {
        return RedactedSeek::None;
    }
    let search_start = if eof {
        file.len().saturating_sub(pattern.len()).max(start)
    } else {
        start
    };
    let mut found: Option<usize> = None;
    let mut count = 0usize;
    let mut i = search_start;
    while i + pattern.len() <= file.len() {
        if pattern
            .iter()
            .zip(&file[i..i + pattern.len()])
            .all(|(want, have)| masked_line_matches(want, have))
        {
            count += 1;
            if found.is_none() {
                found = Some(i);
            } else {
                return RedactedSeek::Ambiguous { count };
            }
        }
        i += 1;
    }
    match found {
        Some(i) => RedactedSeek::Found(i),
        None => RedactedSeek::None,
    }
}

/// Whether `file_line` is the line `pattern` describes, allowing exactly the
/// placeholder to stand for one hidden, non-empty span.
fn masked_line_matches(pattern: &str, file_line: &str) -> bool {
    let Some((prefix, suffix)) = placeholder_parts(pattern) else {
        return pattern == file_line;
    };
    file_line.len() > prefix.len() + suffix.len()
        && file_line.starts_with(prefix)
        && file_line.ends_with(suffix)
}

/// The text around the single placeholder in `line`.
///
/// More than one placeholder is not supported by this matcher: two hidden
/// spans could slide against each other, so the result is not a unique match.
fn placeholder_parts(line: &str) -> Option<(&str, &str)> {
    let at = line.find(REDACTED)?;
    let suffix = &line[at + REDACTED.len()..];
    if suffix.contains(REDACTED) {
        return None;
    }
    Some((&line[..at], suffix))
}

fn redaction_write_refused() -> String {
    format!(
        "refusing to apply this hunk: it would write `{REDACTED}` into the file, replacing a \
         real secret with a placeholder. {REDACTION_NOTE} Delete the whole affected line, replace \
         it with a real value or environment reference, or leave that line out of the hunk and \
         anchor on a nearby line without a secret."
    )
}

fn redacted_ambiguous(count: usize) -> String {
    format!(
        "the hunk's redacted line is ambiguous: it matches {count} candidate sites in the file; \
         refusing to guess. {REDACTION_NOTE} Add an `@@` anchor or enough non-secret context to \
         make the target unique."
    )
}

fn redacted_not_found(lines: &[String]) -> String {
    let hidden: Vec<String> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(REDACTED))
        .map(|(index, _)| format!("hunk line {}", index + 1))
        .collect();
    format!(
        "could not locate this hunk: {} contains `{REDACTED}`. {REDACTION_NOTE} The line as \
         written cannot match the file byte-for-byte, and a hunk that keeps a redacted line as \
         context would write the placeholder back. Anchor on a nearby line that has no secret, or \
         delete/replace the whole affected line.",
        hidden.join(", ")
    )
}

/// Build the error for a hunk whose `old_lines` match nowhere. Showing the file
/// at `start` is near-useless: without a `@@` label `start` is line 1, so the
/// model gets the top of the file while its hunk targets line 300. The failure
/// is almost always a body the model approximated around a real anchor, so
/// anchor on the hunk's first meaningful line, find where THAT lives, and show
/// the file there — the model can then copy the real text back verbatim.
fn hunk_not_found(file: &[String], chunk: &UpdateChunk, start: usize) -> String {
    let wanted = chunk.old_lines.join("\n");
    // A hunk carrying the runtime placeholder is a different failure: the
    // model copied text that was never on disk. Name it instead of printing a
    // file line and a hunk line that both sanitize to the same placeholder.
    if wanted.contains(REDACTED) {
        return redacted_not_found(&chunk.old_lines);
    }
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
        None => {
            // One cause of "these lines do not exist" is the runtime itself:
            // tool output replaces credential values with a placeholder, and a
            // model that copies that output back into a patch is patching text
            // that was never in the file. Naming it here is the difference
            // between the model re-reading and it dropping the edit.
            let redaction_note = if wanted.contains(leveler_core::secret::SECRET_PLACEHOLDER) {
                format!(
                    "\nNote: this hunk contains `{}`. Tool output replaces secret values with \
                     that placeholder, so this line as written does not exist in the file. \
                     Patch around the secret instead: anchor on a nearby line that has no \
                     secret in it, or change a different line.\n",
                    leveler_core::secret::SECRET_PLACEHOLDER
                )
            } else {
                String::new()
            };
            format!(
                "{head}None of those lines exist in the file. Read the file before patching it.\
                 {redaction_note}\n--- file content near line {} ---\n{}",
                start + 1,
                file_excerpt(file, start),
            )
        }
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

/// Build the error for a `@@` context line that has no exact match. The two
/// observed failures are a PREFIX of the real line (e.g. `func CountTokens` for
/// `func CountTokens(text string) int {`) and an ambiguous stem that matches
/// several lines. Both are unrecoverable from a line-1 excerpt.
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
/// actually contains there — not just the lines it failed to find. Showing the
/// real bytes is reporting the constraint that was violated, which is what a
/// locate failure owes its caller.
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
    fn crlf_file_keeps_crlf_and_does_not_mix_endings() {
        // A CRLF file must stay CRLF: the old code kept `\r` on untouched lines
        // and joined with bare `\n`, producing a mixed-ending (corrupt) file.
        let original = "line1\r\nline2\r\nline3\r\n";
        let out = apply_update(original, &[chunk(&["line2"], &["LINE2"])]).unwrap();
        assert_eq!(out, "line1\r\nLINE2\r\nline3\r\n");
        assert!(!out.contains("\n\r"), "no mixed endings: {out:?}");
    }

    #[test]
    fn bom_first_line_context_matches_and_bom_is_preserved() {
        // The model's context line is plain `alpha`; the file's first line
        // carries a UTF-8 BOM. Matching must strip the BOM (or the first-line
        // hunk never locates), and the BOM must be restored on write.
        let original = "\u{FEFF}alpha\nbeta\n";
        let out = apply_update(original, &[chunk(&["alpha", "beta"], &["alpha", "BETA"])]).unwrap();
        assert_eq!(out, "\u{FEFF}alpha\nBETA\n");
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
        // find — the caller needs the real content to rewrite the hunk.
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

    /// REGRESSION P1-1. The model only ever sees `[REDACTED]`, so a hunk that
    /// deletes or replaces the whole secret-bearing line must still locate the
    /// real line by its unambiguous prefix/suffix — without the secret value.
    #[test]
    fn redacted_hunk_deletes_or_replaces_the_whole_secret_line() {
        let original = "before\nDEVORDER_SEED_PASSWORD='synthetic-secret'\nafter\n";
        let out = apply_update(
            original,
            &[chunk(&["DEVORDER_SEED_PASSWORD='[REDACTED]'"], &[])],
        )
        .unwrap();
        assert_eq!(out, "before\nafter\n");
        assert!(!out.contains("synthetic-secret"));

        let out = apply_update(
            original,
            &[chunk(
                &["DEVORDER_SEED_PASSWORD='[REDACTED]'"],
                &["DEVORDER_SEED_PASSWORD=\"${SEED_PASSWORD}\""],
            )],
        )
        .unwrap();
        assert!(out.contains("${SEED_PASSWORD}"), "{out}");
        assert!(!out.contains("synthetic-secret"), "{out}");
    }

    /// A patch may not write the placeholder back: that would replace the real
    /// credential with the literal text `[REDACTED]`.
    #[test]
    fn redacted_hunk_refuses_to_write_the_placeholder() {
        let original = "PASSWORD='synthetic-secret'\n";
        let err = apply_update(
            original,
            &[chunk(
                &["PASSWORD='[REDACTED]'"],
                &["PASSWORD='[REDACTED]'"],
            )],
        )
        .unwrap_err();
        assert!(
            err.contains("cannot write") || err.contains("placeholder"),
            "{err}"
        );
    }

    /// The masked match must be unique. Two identical assignments are not a
    /// licence to pick one.
    #[test]
    fn redacted_hunk_rejects_ambiguous_matches() {
        let original = "PASSWORD='one'\nPASSWORD='two'\n";
        let err = apply_update(original, &[chunk(&["PASSWORD='[REDACTED]'"], &[])]).unwrap_err();
        assert!(
            err.contains("ambiguous") || err.contains("more than one"),
            "{err}"
        );
    }

    /// A miss must name the redaction, not print two identical-looking lines.
    #[test]
    fn redacted_hunk_miss_explains_the_placeholder() {
        let original = "OTHER='x'\n";
        let err = apply_update(original, &[chunk(&["PASSWORD='[REDACTED]'"], &[])]).unwrap_err();
        assert!(err.contains("[REDACTED]"), "{err}");
        assert!(
            err.contains("placeholder") || err.contains("not the file's literal"),
            "{err}"
        );
        assert!(
            !err.contains("file: `PASSWORD='[REDACTED]'`"),
            "must not print a self-contradictory identical diff: {err}"
        );
    }

    /// If the file really contains the placeholder bytes, exact matching still
    /// wins; this is not a wildcard.
    #[test]
    fn literal_placeholder_in_the_file_is_not_a_wildcard() {
        let original = "PASSWORD='[REDACTED]'\n";
        let out = apply_update(original, &[chunk(&["PASSWORD='[REDACTED]'"], &[])]).unwrap();
        assert_eq!(out, "");
    }

    /// The applied diff is user-visible metadata. A masked match must not put
    /// the real secret into it.
    #[test]
    fn applied_diff_for_a_redacted_match_stays_masked() {
        let original = "PASSWORD='synthetic-secret'\n";
        let (_out, hunks) =
            apply_update_located(original, &[chunk(&["PASSWORD='[REDACTED]'"], &[])]).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_lines[0], "PASSWORD='[REDACTED]'");
        for line in &hunks[0].old_lines {
            assert!(!line.contains("synthetic-secret"), "{line}");
        }
    }

    /// A hunk that keeps the redacted line as context would splat the
    /// placeholder into the file if it were allowed to apply.
    #[test]
    fn redacted_context_that_would_be_rewritten_is_refused() {
        let original = "PASSWORD='synthetic-secret'\nKEEP=1\n";
        let err = apply_update(
            original,
            &[chunk(
                &["PASSWORD='[REDACTED]'", "KEEP=1"],
                &["PASSWORD='[REDACTED]'", "KEEP=2"],
            )],
        )
        .unwrap_err();
        assert!(
            err.contains("cannot write") || err.contains("placeholder"),
            "{err}"
        );
    }

    /// The dogfood case: the model copied a line out of tool output, which
    /// replaces a credential with a placeholder, so the hunk anchors on text
    /// that was never in the file. Naming the redaction is what lets the model
    /// patch around the secret instead of silently dropping the edit.
    #[test]
    fn a_hunk_that_copied_a_redacted_value_says_so() {
        let content = "database:\n  dsn: host=127.0.0.1 user=develop password=hunter2 \
                       dbname=orders\n";
        let c = chunk(
            &["  dsn: host=127.0.0.1 user=develop password=[REDACTED] dbname=orders"],
            &["  dsn: host=127.0.0.1 user=develop password=[REDACTED] dbname=orders_test"],
        );
        let err = apply_update(content, &[c]).unwrap_err();
        assert!(
            err.contains("[REDACTED]") && err.contains("Tool output replaces secret"),
            "a failed hunk that carries a redacted value must name that cause; got:\n{err}"
        );
    }
}
