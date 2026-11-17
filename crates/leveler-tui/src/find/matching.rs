//! Case-insensitive literal matching over authored text.
//!
//! Every offset this module returns is a byte range in the ORIGINAL string and
//! sits on a char boundary, so a caller can slice the text and a renderer can
//! split spans without a multi-byte character ever being cut in half. The
//! match is a lowercase comparison, not a full Unicode case fold: `İ` and `i`
//! compare equal, `ß` and `SS` do not. That is the same rule a reader expects
//! from a terminal find box, and it keeps every offset mappable.

/// One match in the original text: a byte range on char boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextMatch {
    pub start: usize,
    pub end: usize,
}

impl TextMatch {
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Every non-overlapping case-insensitive occurrence of `needle`.
///
/// An empty or whitespace-only needle matches nothing: a find box with a
/// space is not a request for every row in the session.
pub fn find_all(haystack: &str, needle: &str, limit: usize) -> Vec<TextMatch> {
    if needle.is_empty() || haystack.is_empty() || limit == 0 {
        return Vec::new();
    }
    if needle.chars().all(char::is_whitespace) {
        return Vec::new();
    }
    if needle.is_ascii() {
        return find_ascii_needle(haystack, needle, limit);
    }
    find_folded(haystack, needle, limit)
}

/// An ASCII needle can only match at a char boundary of a UTF-8 string (a
/// multi-byte character has no ASCII byte in it), so this path needs no
/// folding and no offset map.
fn find_ascii_needle(haystack: &str, needle: &str, limit: usize) -> Vec<TextMatch> {
    let hay = haystack.as_bytes();
    let needle = needle.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at + needle.len() <= hay.len() {
        if hay[at..at + needle.len()].eq_ignore_ascii_case(needle) {
            out.push(TextMatch {
                start: at,
                end: at + needle.len(),
            });
            if out.len() >= limit {
                break;
            }
            at += needle.len();
        } else {
            at += 1;
        }
    }
    out
}

/// Where one source character's lowercased expansion landed.
struct FoldSpan {
    folded_start: usize,
    folded_end: usize,
    orig_start: usize,
    orig_end: usize,
}

fn find_folded(haystack: &str, needle: &str, limit: usize) -> Vec<TextMatch> {
    let (folded_hay, spans) = fold_with_map(haystack);
    let (folded_needle, _) = fold_with_map(needle);
    if folded_needle.is_empty() || spans.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut from = 0;
    while from < folded_hay.len() {
        let Some(found) = folded_hay[from..].find(&folded_needle) else {
            break;
        };
        let start = from + found;
        let end = start + folded_needle.len();
        let orig_start = spans
            .partition_point(|span| span.folded_end <= start)
            .min(spans.len() - 1);
        let orig_end_at = spans
            .partition_point(|span| span.folded_start < end)
            .saturating_sub(1);
        let (orig_start, orig_end) = (spans[orig_start].orig_start, spans[orig_end_at].orig_end);
        if orig_end > orig_start {
            out.push(TextMatch {
                start: orig_start,
                end: orig_end,
            });
            if out.len() >= limit {
                break;
            }
        }
        from = end;
    }
    out
}

fn fold_with_map(text: &str) -> (String, Vec<FoldSpan>) {
    let mut folded = String::with_capacity(text.len());
    let mut spans = Vec::with_capacity(text.len());
    for (orig_start, ch) in text.char_indices() {
        let folded_start = folded.len();
        for lower in ch.to_lowercase() {
            folded.push(lower);
        }
        if folded.len() == folded_start {
            folded.push(ch);
        }
        spans.push(FoldSpan {
            folded_start,
            folded_end: folded.len(),
            orig_start,
            orig_end: orig_start + ch.len_utf8(),
        });
    }
    (folded, spans)
}

/// A one-line context window around `matched`, flattened for terminal paint.
///
/// Returns `(snippet, match_start, match_end)` where the offsets index the
/// snippet. Newlines and control characters become spaces, so a tool result
/// never puts a second row into one result line. The match keeps its own
/// character range after flattening because every replaced character is
/// replaced one-for-one.
pub fn snippet(haystack: &str, matched: TextMatch, context_chars: usize) -> (String, usize, usize) {
    let prefix = &haystack[..matched.start.min(haystack.len())];
    let win_start = prefix
        .char_indices()
        .rev()
        .take(context_chars)
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0);
    let suffix = &haystack[matched.end.min(haystack.len())..];
    let win_end = matched.end.min(haystack.len())
        + suffix
            .char_indices()
            .nth(context_chars)
            .map(|(index, _)| index)
            .unwrap_or(suffix.len());

    let flatten = |text: &str| -> String {
        text.chars()
            .map(|ch| {
                if ch.is_control() || is_bidi_control(ch) {
                    ' '
                } else {
                    ch
                }
            })
            .collect()
    };
    let before = flatten(&haystack[win_start..matched.start]);
    let hit = flatten(&haystack[matched.start..matched.end]);
    let after = flatten(&haystack[matched.end..win_end]);
    let mut out = String::new();
    if win_start > 0 {
        out.push('…');
    }
    out.push_str(&before);
    let start = out.len();
    out.push_str(&hit);
    let end = out.len();
    out.push_str(&after);
    if win_end < haystack.len() {
        out.push('…');
    }
    (out, start, end)
}

/// Directional overrides and isolates can visually reorder a single result
/// row. They are format characters, not `is_control`, so a search snippet
/// neutralises them explicitly; every replacement stays one character, so the
/// match range is unaffected.
fn is_bidi_control(ch: char) -> bool {
    matches!(ch,
        '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(haystack: &str, needle: &str, limit: usize) -> Vec<(usize, usize)> {
        find_all(haystack, needle, limit)
            .into_iter()
            .map(|m| (m.start, m.end))
            .collect()
    }

    #[test]
    fn ascii_matching_ignores_case() {
        let text = "Implement the Refund approval workflow";
        assert_eq!(ranges(text, "refund", 10), vec![(14, 20)]);
        assert_eq!(ranges(text, "REFUND", 10), vec![(14, 20)]);
        assert_eq!(&text[14..20], "Refund");
        assert!(find_all(text, "missing", 10).is_empty());
    }

    #[test]
    fn chinese_matches_at_byte_offsets_on_char_boundaries() {
        let text = "请实现退款审批流程，退款审批需要幂等";
        let matches = find_all(text, "退款审批", 10);
        assert_eq!(matches.len(), 2);
        for m in &matches {
            assert!(text.is_char_boundary(m.start) && text.is_char_boundary(m.end));
            assert_eq!(&text[m.start..m.end], "退款审批");
        }
    }

    #[test]
    fn ascii_needle_inside_cjk_text_is_found() {
        let text = "测试 refund_idempotency_test 通过";
        let matches = find_all(text, "REFUND_IDEMPOTENCY_TEST", 10);
        assert_eq!(matches.len(), 1);
        assert_eq!(
            &text[matches[0].start..matches[0].end],
            "refund_idempotency_test"
        );
    }

    #[test]
    fn a_multichar_lowercase_expansion_maps_back_to_one_source_char() {
        // 'İ' lowercases to "i" + combining dot, two folded chars for one
        // source char. A match starting inside the expansion must snap to the
        // whole source character, never split it.
        let text = "İSTANBUL";
        let matches = find_all(text, "i̇", 10);
        assert_eq!(matches.len(), 1);
        assert_eq!((matches[0].start, matches[0].end), (0, "İ".len()));
        let folded = find_all(text, "stanbul", 10);
        assert_eq!(folded.len(), 1);
        assert!(text.is_char_boundary(folded[0].start));
        assert_eq!(&text[folded[0].start..folded[0].end], "STANBUL");
        assert_eq!(folded[0].start, "İ".len());
    }

    #[test]
    fn matches_are_non_overlapping_and_limited() {
        let text = "aaaaa";
        assert_eq!(ranges(text, "aa", 10), vec![(0, 2), (2, 4)]);
        assert_eq!(ranges(text, "aa", 1), vec![(0, 2)]);
        assert_eq!(ranges("abcabcabc", "abc", 2), vec![(0, 3), (3, 6)]);
    }

    #[test]
    fn empty_whitespace_and_zero_limit_match_nothing() {
        assert!(find_all("anything", "", 10).is_empty());
        assert!(find_all("anything", "   ", 10).is_empty());
        assert!(find_all("anything", "any", 0).is_empty());
        assert!(find_all("", "any", 10).is_empty());
    }

    #[test]
    fn snippet_centres_the_match_and_flattens_controls() {
        let long = format!("{}refund{}", "x".repeat(50), "y".repeat(50));
        let found = find_all(&long, "refund", 1)[0];
        let (text, start, end) = snippet(&long, found, 10);
        assert_eq!(&text[start..end], "refund");
        assert!(text.starts_with('…') && text.ends_with('…'));
        assert!(text.chars().count() <= 28, "{text:?}");

        let multiline = "line one\n\tREFUND\tapproved\nline three";
        let found = find_all(multiline, "refund", 1)[0];
        let (text, start, end) = snippet(multiline, found, 40);
        assert_eq!(&text[start..end], "REFUND");
        assert!(!text.contains('\n') && !text.contains('\t'), "{text:?}");
        assert!(text.contains("line one") && text.contains("approved"));
    }

    #[test]
    fn snippet_neutralises_directional_overrides() {
        let text = "before \u{202E}evil\u{202C} after";
        let found = find_all(text, "evil", 1)[0];
        let (out, start, end) = snippet(text, found, 80);
        assert!(
            !out.contains('\u{202E}') && !out.contains('\u{202C}'),
            "{out:?}"
        );
        assert_eq!(&out[start..end], "evil");
    }

    #[test]
    fn snippet_without_elision_keeps_the_whole_line() {
        let text = "PASS refund_idempotency_test";
        let found = find_all(text, "refund", 1)[0];
        let (out, start, end) = snippet(text, found, 80);
        assert_eq!((start, end), (5, 11));
        assert_eq!(out, text);
    }
}
