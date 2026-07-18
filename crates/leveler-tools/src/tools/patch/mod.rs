//! A self-contained implementation of the restricted `*** Begin Patch` format
//! (spec §6.12, §18.3 apply_patch). Pure string-in/string-out so it is fully
//! unit-testable; the filesystem side lives in the `apply_patch` tool.

mod apply;
mod parse;
mod seek;

pub use apply::apply_update;
pub use parse::{FileChange, UpdateChunk, parse_patch};

/// Errors from parsing a patch. (Apply failures are returned as plain strings by
/// [`apply_update`] and wrapped with a path by the `apply_patch` tool.)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    #[error("invalid patch: {0}")]
    Parse(String),
}

/// Scenario fixtures for the Update-hunk edge cases. Each case applies a real
/// patch to real input and checks the exact result, locking parser + applier
/// robustness for tricky whitespace, EOF, and context-label situations.
#[cfg(test)]
mod scenarios {
    use super::{FileChange, apply_update, parse_patch};

    /// Apply the single Update section in `patch` to `input`, returning the new
    /// content. Panics if the patch has no Update section.
    fn apply(patch: &str, input: &str) -> String {
        let changes = parse_patch(patch).expect("patch should parse");
        for change in changes {
            if let FileChange::Update { chunks, .. } = change {
                return apply_update(input, &chunks).expect("hunks should apply");
            }
        }
        panic!("no Update section in patch");
    }

    #[test]
    fn s014_replacing_last_line_of_no_newline_file_adds_trailing_newline() {
        let patch = "*** Begin Patch\n*** Update File: f\n@@\n-no newline at end\n+first line\n+second line\n*** End Patch";
        assert_eq!(
            apply(patch, "no newline at end"),
            "first line\nsecond line\n"
        );
    }

    #[test]
    fn s016_pure_addition_appends_at_eof() {
        let patch =
            "*** Begin Patch\n*** Update File: f\n@@\n+added line 1\n+added line 2\n*** End Patch";
        assert_eq!(
            apply(patch, "line1\nline2\n"),
            "line1\nline2\nadded line 1\nadded line 2\n"
        );
    }

    #[test]
    fn s017_whitespace_padded_hunk_header() {
        let patch = "*** Begin Patch\n  *** Update File: f\n@@\n-old\n+new\n*** End Patch";
        assert_eq!(apply(patch, "old\n"), "new\n");
    }

    #[test]
    fn s018_whitespace_padded_patch_markers() {
        let patch = " *** Begin Patch\n*** Update File: f\n@@\n-one\n+two\n*** End Patch ";
        assert_eq!(apply(patch, "one\n"), "two\n");
    }

    #[test]
    fn s020_whitespace_padded_end_marker_line() {
        let patch = "*** Begin Patch \n*** Update File: f\n@@\n-one\n+two\n *** End Patch";
        assert_eq!(apply(patch, "one\n"), "two\n");
    }

    #[test]
    fn s021_deletion_only_hunk_keeps_surrounding_context() {
        let patch =
            "*** Begin Patch\n*** Update File: f\n@@\n line1\n-line2\n line3\n*** End Patch";
        assert_eq!(apply(patch, "line1\nline2\nline3\n"), "line1\nline3\n");
    }

    #[test]
    fn s022_end_of_file_marker() {
        let patch = "*** Begin Patch\n*** Update File: f\n@@\n first\n-second\n+second updated\n*** End of File\n*** End Patch";
        assert_eq!(apply(patch, "first\nsecond\n"), "first\nsecond updated\n");
    }
}
