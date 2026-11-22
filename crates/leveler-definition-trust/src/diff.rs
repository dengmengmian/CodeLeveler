//! A compact line diff for a human reviewing a definition change.
//!
//! This is a review aid, not a patch format: unchanged runs are reduced to
//! three lines of context around each change. The trust decision never depends
//! on it; it only helps a person decide.

/// Longest edit script a definition diff will render. Beyond this the caller
/// shows the two versions are too large for a line diff.
pub const MAX_DIFF_CELLS: usize = 4_000_000;

/// Render a line diff as `- old`, `+ new`, `  context` rows.
///
/// Returns `None` when the inputs are too large for the quadratic edit script;
/// the caller must say so rather than display a partial comparison as complete.
pub fn unified_diff(old: &str, new: &str, max_rows: usize) -> Option<Vec<String>> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines.len().saturating_mul(new_lines.len()) > MAX_DIFF_CELLS {
        return None;
    }
    let mut dp = vec![vec![0usize; new_lines.len() + 1]; old_lines.len() + 1];
    for i in (0..old_lines.len()).rev() {
        for j in (0..new_lines.len()).rev() {
            dp[i][j] = if old_lines[i] == new_lines[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut script: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < old_lines.len() || j < new_lines.len() {
        if i < old_lines.len() && j < new_lines.len() && old_lines[i] == new_lines[j] {
            script.push((' ', old_lines[i]));
            i += 1;
            j += 1;
        } else if j < new_lines.len() && (i == old_lines.len() || dp[i][j + 1] >= dp[i + 1][j]) {
            script.push(('+', new_lines[j]));
            j += 1;
        } else if i < old_lines.len() {
            script.push(('-', old_lines[i]));
            i += 1;
        }
    }
    let mut out = Vec::new();
    let mut last = 0usize;
    for (index, (op, line)) in script.iter().enumerate() {
        if *op == ' ' {
            continue;
        }
        let start = index.saturating_sub(3);
        if start > last {
            out.push("…".to_string());
        }
        for (context_index, (context_op, context_line)) in
            script.iter().enumerate().take(index).skip(start)
        {
            if context_index >= last {
                out.push(format!("{context_op} {}", sanitize_line(context_line)));
            }
        }
        out.push(format!("{op} {}", sanitize_line(line)));
        last = index + 1;
        if out.len() >= max_rows {
            out.push("… (diff truncated)".to_string());
            return Some(out);
        }
    }
    if out.is_empty() {
        out.push("(no textual difference; only the digest changed)".to_string());
    }
    Some(out)
}

/// Definition bodies are untrusted terminal input. Keep tabs and printable
/// text; neutralize every other control character so a rule cannot drive the
/// terminal while a person reviews a diff.
pub fn sanitize_line(line: &str) -> String {
    line.chars()
        .map(|ch| {
            if ch.is_control() && ch != '\t' {
                '?'
            } else {
                ch
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_diff_marks_removed_and_added_lines() {
        let rows = unified_diff("one\ntwo\nthree\n", "one\nTWO\nthree\nfour\n", 400).unwrap();
        assert!(rows.iter().any(|row| row == "- two"), "{rows:?}");
        assert!(rows.iter().any(|row| row == "+ TWO"), "{rows:?}");
        assert!(rows.iter().any(|row| row == "+ four"), "{rows:?}");
    }

    #[test]
    fn control_characters_cannot_drive_the_terminal() {
        let rows = unified_diff("a\nb\n", "a\nb\x1b[2J\n", 10).unwrap();
        assert!(rows.iter().any(|row| row.contains("?[2J")), "{rows:?}");
        assert!(!rows.iter().any(|row| row.contains('\u{1b}')), "{rows:?}");
    }

    #[test]
    fn too_large_a_comparison_is_reported_as_unavailable() {
        let old = "x\n".repeat(3000);
        let new = "y\n".repeat(3000);
        assert!(unified_diff(&old, &new, 400).is_none());
    }
}
