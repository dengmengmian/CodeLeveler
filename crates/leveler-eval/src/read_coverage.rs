//! What a `read_file` result actually proves (C2.3C final closure).
//!
//! Three ideas that a single boolean would collapse, and must not:
//!
//! - **PathTouched** — a read of this path returned successfully. The run has
//!   seen *some* of the file.
//! - **PathFullyRead** — the result carried the whole file: it started at line
//!   1, reached the last line, and was not clipped.
//! - **EvidenceCovered** — the returned range contains the lines that actually
//!   matter. Only definable where a case declares which lines those are.
//!
//! The distinction has teeth because a request is not a result. `read_file`
//! with no range *asks* for the whole file; a byte ceiling can still return a
//! prefix. Deciding coverage from the arguments would then credit a run with
//! evidence it never received — and the miss would be invisible, because the
//! path is right there in the call.
//!
//! This module reads the *result*. `read_file` numbers every line it emits and
//! appends a truncation marker when it stops early, so the returned range is
//! recoverable from the text the model was actually sent.

/// What one successful `read_file` result carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReturnedRange {
    /// First and last line number present in the output (1-based, inclusive).
    pub first: usize,
    pub last: usize,
    /// The tool stopped early — by byte ceiling or mid-line — and said so.
    pub clipped: bool,
}

impl ReturnedRange {
    /// Whether this result carried the file whole: from line 1 to the last
    /// line, with nothing dropped.
    ///
    /// `total_lines` is the file's real length. A clipped result is never
    /// complete, however many lines it happens to contain — the marker is the
    /// tool telling us it left something out.
    pub fn is_complete(&self, total_lines: usize) -> bool {
        !self.clipped && self.first == 1 && total_lines > 0 && self.last >= total_lines
    }

    /// Whether the returned lines contain the whole of `first..=last`.
    ///
    /// Used only where a case declares which lines are the evidence. An
    /// overlapping-but-not-containing read is a miss: half a function is not
    /// the function.
    pub fn covers(&self, first: usize, last: usize) -> bool {
        self.first <= first && self.last >= last
    }
}

/// Parse what a `read_file` result actually returned.
///
/// Returns `None` when the text carries no numbered lines at all — an empty
/// range, a range past EOF, or something that is not a read result. `None`
/// means "proves nothing", never "covers everything".
pub fn returned_range(content: &str) -> Option<ReturnedRange> {
    let mut first = usize::MAX;
    let mut last = 0usize;
    for line in content.lines() {
        let digits: String = line
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.is_empty() {
            continue;
        }
        // A numbered line is `<spaces><number>\t<text>`; anything else that
        // merely starts with digits is prose and must not move the range.
        let rest = line.trim_start();
        if !rest[digits.len()..].starts_with('\t') {
            continue;
        }
        let Ok(number) = digits.parse::<usize>() else {
            continue;
        };
        first = first.min(number);
        last = last.max(number);
    }
    if last == 0 {
        return None;
    }
    Some(ReturnedRange {
        first,
        last,
        clipped: content.contains("… [truncated"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(from: usize, to: usize) -> String {
        (from..=to).map(|n| format!("{n:>6}\tline {n}\n")).collect()
    }

    /// §16 B — a partial read is real evidence about the lines it returned.
    #[test]
    fn a_ranged_read_reports_the_lines_it_returned() {
        let range = returned_range(&numbered(100, 200)).expect("numbered lines");
        assert_eq!((range.first, range.last), (100, 200));
        assert!(!range.clipped);
    }

    /// §16 D — a whole small file is complete coverage of that file.
    #[test]
    fn an_unclipped_read_from_line_one_to_the_end_is_complete() {
        let range = returned_range(&numbered(1, 40)).expect("numbered lines");
        assert!(range.is_complete(40));
    }

    /// §16 C — the load-bearing case. `read_file(path)` with no range asks for
    /// everything; the byte ceiling can still return a prefix, and the tool
    /// says so. Crediting the request rather than the result would report a
    /// file as fully seen when two thirds of it never reached the model.
    #[test]
    fn a_clipped_whole_file_request_is_not_complete_coverage() {
        let clipped = format!(
            "{}… [truncated: lines 1–1024 of 7675 lines shown (230911 bytes / ~57728 tokens total); continue with start_line=1025]\n",
            numbered(1, 1024)
        );
        let range = returned_range(&clipped).expect("numbered lines");
        assert_eq!((range.first, range.last), (1, 1024));
        assert!(range.clipped);
        assert!(
            !range.is_complete(7675),
            "a clipped result must never count as the whole file"
        );
        // And not even against its own line count: the marker is the tool
        // telling us it stopped early.
        assert!(!range.is_complete(1024));
    }

    /// §16 G — a prefix that ends before the evidence begins must not be
    /// mistaken for having seen it.
    #[test]
    fn a_prefix_that_stops_before_the_evidence_does_not_cover_it() {
        let range = returned_range(&numbered(1, 1024)).expect("numbered lines");
        assert!(!range.covers(1800, 1900));
        assert!(range.covers(500, 600));
    }

    /// §16 E/F — containment, not overlap.
    #[test]
    fn evidence_coverage_requires_containment_not_overlap() {
        let range = returned_range(&numbered(1400, 1600)).expect("numbered lines");
        assert!(range.covers(1500, 1570), "fully inside");
        assert!(
            !range.covers(1350, 1450),
            "starts before the returned range"
        );
        assert!(!range.covers(1550, 1650), "ends after the returned range");
    }

    /// §16 H — the range comes from the text, so a result with no lines proves
    /// nothing regardless of what was asked for.
    #[test]
    fn a_result_with_no_numbered_lines_proves_nothing() {
        assert_eq!(returned_range(""), None);
        assert_eq!(
            returned_range("(no lines in the requested range; the file has 40 lines)\n"),
            None
        );
        assert_eq!(returned_range("file not found: src/lib.rs"), None);
    }

    /// Prose that happens to start with a digit is not a numbered line.
    #[test]
    fn only_tab_delimited_line_numbers_count() {
        let text = "1024 bytes were skipped\n2 files matched\n";
        assert_eq!(returned_range(text), None);
    }

    /// The repeated-read note `read_file` prepends must not shift the range.
    #[test]
    fn a_leading_note_does_not_disturb_the_range() {
        let text = format!(
            "[note: this unchanged range was read multiple times; returning it again so recovery is not blocked]\n{}",
            numbered(10, 20)
        );
        let range = returned_range(&text).expect("numbered lines");
        assert_eq!((range.first, range.last), (10, 20));
    }
}
