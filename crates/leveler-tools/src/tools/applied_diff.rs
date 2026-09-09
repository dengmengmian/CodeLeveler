//! Canonical unified diffs for edits that were actually applied.
//!
//! A tool's arguments say what the model WANTED to change; only the execution
//! layer knows where the change landed. That difference is why an inline diff
//! must not derive line numbers from the request: `replace` carries no line
//! numbers at all, and an `apply_patch` hunk is located by content, so its
//! position is discovered, not declared.
//!
//! Everything here is string-in / string-out and numbers only what it can
//! prove. A hunk whose location is unknown is not emitted rather than guessed.

/// One located change: the lines it replaced and the lines it wrote, each with
/// the 1-based start line on its own side of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedHunk {
    pub old_start: usize,
    pub old_lines: Vec<String>,
    pub new_start: usize,
    pub new_lines: Vec<String>,
}

/// Upper bound on a published applied diff. It travels through the durable
/// event log and into the UI, and a patch past this size is not something a
/// conversation renders anyway — so an oversized one publishes nothing and the
/// UI falls back to an unnumbered diff, rather than bloating every reader.
const MAX_APPLIED_DIFF_BYTES: usize = 64 * 1024;

/// Render located hunks as a unified diff with numeric headers.
///
/// Returns `None` when nothing was located, so a caller never publishes an
/// empty diff that a reader would take for "no change".
pub fn unified_diff(path: &str, hunks: &[AppliedHunk]) -> Option<String> {
    if hunks.is_empty() || hunks.iter().all(|h| h.old_lines == h.new_lines) {
        return None;
    }
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    for h in hunks {
        if h.old_lines == h.new_lines {
            continue;
        }
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            h.old_start,
            h.old_lines.len(),
            h.new_start,
            h.new_lines.len()
        ));
        // An `apply_patch` chunk carries the context lines the model wrote
        // around its change. Emitting those as removed-and-re-added would
        // claim untouched lines changed, so the run of identical lines at each
        // end of the hunk is written as context.
        let (head, tail) = shared_ends(&h.old_lines, &h.new_lines);
        for line in &h.old_lines[..head] {
            out.push(' ');
            out.push_str(line);
            out.push('\n');
        }
        for line in &h.old_lines[head..h.old_lines.len() - tail] {
            out.push('-');
            out.push_str(line);
            out.push('\n');
        }
        for line in &h.new_lines[head..h.new_lines.len() - tail] {
            out.push('+');
            out.push_str(line);
            out.push('\n');
        }
        for line in &h.old_lines[h.old_lines.len() - tail..] {
            out.push(' ');
            out.push_str(line);
            out.push('\n');
        }
    }
    (out.len() <= MAX_APPLIED_DIFF_BYTES).then_some(out)
}

/// How many lines at the start and at the end are identical on both sides.
/// The two runs never overlap, so a hunk always splits into context / change /
/// context without a line being counted twice.
fn shared_ends(old: &[String], new: &[String]) -> (usize, usize) {
    let limit = old.len().min(new.len());
    let head = (0..limit).take_while(|&i| old[i] == new[i]).count();
    let tail = (0..limit - head)
        .take_while(|&i| old[old.len() - 1 - i] == new[new.len() - 1 - i])
        .count();
    (head, tail)
}

/// Locate every occurrence a literal `old` → `new` replacement changed.
///
/// `replace` rewrites a substring, which may sit mid-line, so each hunk covers
/// the WHOLE lines the match touched — that is the unit a line number can
/// describe. The new-side start accounts for the line delta of every earlier
/// occurrence, so the second hunk of a `replace_all` is numbered against the
/// file as it will be, not as it was.
pub fn literal_replacement_hunks(
    before: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Vec<AppliedHunk> {
    if old.is_empty() {
        return Vec::new();
    }
    let mut hunks = Vec::new();
    let mut delta: i64 = 0;
    let mut from = 0usize;
    while let Some(rel) = before[from..].find(old) {
        let start = from + rel;
        let end = start + old.len();
        let line_start = before[..start].rfind('\n').map_or(0, |i| i + 1);
        let line_end = before[end..].find('\n').map_or(before.len(), |i| end + i);
        let old_block = &before[line_start..line_end];
        let new_block = format!(
            "{}{}{}",
            &before[line_start..start],
            new,
            &before[end..line_end]
        );
        let old_start = before[..line_start].matches('\n').count() + 1;
        let old_lines: Vec<String> = split_block(old_block);
        let new_lines: Vec<String> = split_block(&new_block);
        let new_start = (old_start as i64 + delta).max(1) as usize;
        delta += new_lines.len() as i64 - old_lines.len() as i64;
        hunks.push(AppliedHunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
        });
        if !replace_all {
            break;
        }
        from = line_end.max(end).max(start + 1);
        if from >= before.len() {
            break;
        }
    }
    hunks
}

/// A block of text as its lines. An empty block is zero lines, not one empty
/// line — a zero-length side is how a unified diff spells "nothing here".
fn split_block(block: &str) -> Vec<String> {
    if block.is_empty() {
        Vec::new()
    } else {
        block.split('\n').map(str::to_string).collect()
    }
}

/// The whole file as one added hunk, numbered from line 1.
pub fn whole_file_hunk(content: &str, added: bool) -> Option<AppliedHunk> {
    let lines = split_block(content.strip_suffix('\n').unwrap_or(content));
    if lines.is_empty() {
        return None;
    }
    Some(if added {
        AppliedHunk {
            old_start: 0,
            old_lines: Vec::new(),
            new_start: 1,
            new_lines: lines,
        }
    } else {
        AppliedHunk {
            old_start: 1,
            old_lines: lines,
            new_start: 0,
            new_lines: Vec::new(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_line_replacement_is_numbered_where_it_landed() {
        let before = "a\nb\nold\nc\n";
        let hunks = literal_replacement_hunks(before, "old", "new", false);
        assert_eq!(
            hunks,
            vec![AppliedHunk {
                old_start: 3,
                old_lines: vec!["old".into()],
                new_start: 3,
                new_lines: vec!["new".into()],
            }]
        );
    }

    /// Two old lines becoming three: the old side keeps its own numbering and
    /// the new side keeps its own, which is what a single cursor could not do.
    #[test]
    fn a_growing_replacement_numbers_each_side_separately() {
        let before = "x\nold A\nold B\ny\n";
        let hunks = literal_replacement_hunks(before, "old A\nold B", "new A\nnew B\nnew C", false);
        assert_eq!(hunks[0].old_start, 2);
        assert_eq!(hunks[0].old_lines.len(), 2);
        assert_eq!(hunks[0].new_start, 2);
        assert_eq!(hunks[0].new_lines.len(), 3);
        let diff = unified_diff("a.rs", &hunks).unwrap();
        assert!(diff.contains("@@ -2,2 +2,3 @@"), "{diff}");
    }

    /// A mid-line replacement still describes whole lines — that is the only
    /// unit a line number can honestly name.
    #[test]
    fn a_mid_line_replacement_covers_the_whole_line() {
        let before = "fn a() { old() }\n";
        let hunks = literal_replacement_hunks(before, "old", "new", false);
        assert_eq!(hunks[0].old_lines, vec!["fn a() { old() }".to_string()]);
        assert_eq!(hunks[0].new_lines, vec!["fn a() { new() }".to_string()]);
    }

    /// replace_all: the second hunk is numbered against the file the first
    /// hunk already changed.
    #[test]
    fn later_occurrences_account_for_the_lines_earlier_ones_added() {
        let before = "t\nt\n";
        let hunks = literal_replacement_hunks(before, "t", "u\nv", true);
        assert_eq!(hunks.len(), 2);
        assert_eq!((hunks[0].old_start, hunks[0].new_start), (1, 1));
        assert_eq!((hunks[1].old_start, hunks[1].new_start), (2, 3));
    }

    /// A patch hunk's surrounding context stays context: writing it as
    /// removed-and-re-added would claim untouched lines changed.
    #[test]
    fn shared_lines_at_a_hunks_edges_are_written_as_context() {
        let h = AppliedHunk {
            old_start: 10,
            old_lines: vec!["keep".into(), "old".into(), "tail".into()],
            new_start: 10,
            new_lines: vec!["keep".into(), "new".into(), "tail".into()],
        };
        let diff = unified_diff("a.rs", &[h]).unwrap();
        assert!(diff.contains("@@ -10,3 +10,3 @@"), "{diff}");
        assert!(diff.contains("\n keep\n"), "{diff}");
        assert!(diff.contains("\n-old\n"), "{diff}");
        assert!(diff.contains("\n+new\n"), "{diff}");
        assert!(diff.contains("\n tail\n"), "{diff}");
        assert!(!diff.contains("-keep"), "{diff}");
    }

    #[test]
    fn a_no_op_replacement_produces_no_diff() {
        let hunks = literal_replacement_hunks("a\n", "a", "a", false);
        assert!(unified_diff("f", &hunks).is_none());
    }

    #[test]
    fn a_new_file_is_one_hunk_numbered_from_one() {
        let h = whole_file_hunk("one\ntwo\n", true).unwrap();
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_lines, vec!["one".to_string(), "two".to_string()]);
        assert!(h.old_lines.is_empty());
        let diff = unified_diff("n.rs", &[h]).unwrap();
        assert!(diff.contains("@@ -0,0 +1,2 @@"), "{diff}");
    }

    #[test]
    fn a_deleted_file_is_numbered_off_the_old_side() {
        let h = whole_file_hunk("one\ntwo\n", false).unwrap();
        assert_eq!(h.old_start, 1);
        assert!(h.new_lines.is_empty());
        let diff = unified_diff("d.rs", &[h]).unwrap();
        assert!(diff.contains("@@ -1,2 +0,0 @@"), "{diff}");
    }
}
