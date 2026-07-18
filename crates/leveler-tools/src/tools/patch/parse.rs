//! Parser for the `*** Begin Patch` envelope.

use super::PatchError;

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const EOF: &str = "*** End of File";

/// One file-level change in a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<UpdateChunk>,
    },
}

/// A single hunk within an Update File section.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateChunk {
    /// Optional `@@ <label>` locator line to seek before matching.
    pub context_label: Option<String>,
    /// Lines to locate in the existing file (context + removed).
    pub old_lines: Vec<String>,
    /// Replacement lines (context + added).
    pub new_lines: Vec<String>,
    /// Whether this hunk is anchored to the end of the file.
    pub is_eof: bool,
}

fn is_file_header(line: &str) -> bool {
    // Tolerate leading whitespace: weak models intermittently indent the
    // `*** ...` section headers, and one such line must not wreck the parse.
    let t = line.trim_start();
    t.starts_with(ADD) || t.starts_with(DELETE) || t.starts_with(UPDATE) || is_end(line)
}

/// Whether a line closes the envelope. Tolerates a dropped `*** ` prefix
/// ("End Patch"), which shows up when a model glues a fence onto the footer.
fn is_end(line: &str) -> bool {
    let t = line.trim();
    t == END || t == "End Patch"
}

/// Strip markdown code-fence noise: a bare ```/```lang line is dropped, and a
/// fence glued onto real content ("``` End Patch") keeps the content. Only
/// column-0 fences qualify — a fence inside hunk content carries a marker or
/// leading space and is untouched.
fn normalize_line(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("```") {
        if rest.trim().chars().all(|c| c.is_ascii_alphanumeric()) {
            return None; // pure fence, with or without a language tag
        }
        return Some(rest);
    }
    Some(line)
}

/// Parse a full patch into an ordered list of file changes. All-or-nothing:
/// parsing never touches the filesystem and fails before any change is applied.
pub fn parse_patch(input: &str) -> Result<Vec<FileChange>, PatchError> {
    // Keep each surviving line's ORIGINAL 1-based number alongside its text, so
    // parse errors can point the model at the real line even when fence lines
    // were dropped by `normalize_line`.
    let mut lines: Vec<&str> = Vec::new();
    let mut orig_no: Vec<usize> = Vec::new();
    for (n, raw) in input.lines().enumerate() {
        if let Some(t) = normalize_line(raw) {
            lines.push(t);
            orig_no.push(n + 1);
        }
    }
    let mut idx = 0;

    while idx < lines.len() && lines[idx].trim().is_empty() {
        idx += 1;
    }
    if idx >= lines.len() || lines[idx].trim() != BEGIN {
        if looks_like_unified_diff(&lines[idx.min(lines.len())..]) {
            return parse_unified_diff(&lines[idx..]);
        }
        return Err(PatchError::Parse("missing `*** Begin Patch` header".into()));
    }
    idx += 1;

    let mut changes = Vec::new();

    while idx < lines.len() {
        // Match section headers after leading whitespace so an indented
        // `  *** Update File:` is still recognized (see `is_file_header`).
        let line = lines[idx].trim_start();

        if is_end(line) {
            return Ok(changes);
        }
        if line.trim().is_empty() {
            idx += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix(ADD) {
            idx += 1;
            let mut body = Vec::new();
            while idx < lines.len() && !is_file_header(lines[idx]) {
                let l = lines[idx];
                if let Some(rest) = l.strip_prefix('+') {
                    body.push(rest.to_string());
                } else if l.trim().is_empty() {
                    body.push(String::new());
                } else if l.trim_start().starts_with("*** ") {
                    // Looks like a (misspelled?) section header — swallowing it
                    // as content would silently corrupt the new file.
                    return Err(PatchError::Parse(format!(
                        "unrecognized section header inside Add File: {l:?}"
                    )));
                } else {
                    // A bare line missing its '+' escape: the body of an Add
                    // File section IS the file content, so this is unambiguous
                    // — accept it rather than burn a retry round-trip.
                    body.push(l.to_string());
                }
                idx += 1;
            }
            let mut content = body.join("\n");
            if !content.is_empty() {
                content.push('\n');
            }
            changes.push(FileChange::Add {
                path: path.trim().to_string(),
                content,
            });
            continue;
        }

        if let Some(path) = line.strip_prefix(DELETE) {
            changes.push(FileChange::Delete {
                path: path.trim().to_string(),
            });
            idx += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix(UPDATE) {
            let header_no = orig_no[idx];
            idx += 1;
            let mut move_to = None;
            if idx < lines.len()
                && let Some(m) = lines[idx].strip_prefix(MOVE)
            {
                move_to = Some(m.trim().to_string());
                idx += 1;
            }

            let mut chunks: Vec<UpdateChunk> = Vec::new();
            let mut cur: Option<UpdateChunk> = None;

            while idx < lines.len() && !is_file_header(lines[idx]) {
                let l = lines[idx];

                if l.trim() == EOF {
                    cur.get_or_insert_with(UpdateChunk::default).is_eof = true;
                    idx += 1;
                    continue;
                }
                if let Some(ctx) = l.strip_prefix("@@") {
                    if let Some(done) = cur.take() {
                        chunks.push(done);
                    }
                    let label = ctx.trim();
                    cur = Some(UpdateChunk {
                        context_label: (!label.is_empty()).then(|| label.to_string()),
                        ..Default::default()
                    });
                    idx += 1;
                    continue;
                }

                let chunk = cur.get_or_insert_with(UpdateChunk::default);
                if l.is_empty() {
                    chunk.old_lines.push(String::new());
                    chunk.new_lines.push(String::new());
                } else {
                    let marker = l.chars().next().unwrap();
                    let rest = &l[marker.len_utf8()..];
                    match marker {
                        ' ' => {
                            chunk.old_lines.push(rest.to_string());
                            chunk.new_lines.push(rest.to_string());
                        }
                        '-' => chunk.old_lines.push(rest.to_string()),
                        '+' => chunk.new_lines.push(rest.to_string()),
                        _ => {
                            let t = l.trim_start();
                            if t.starts_with("=======")
                                || t.starts_with("<<<<<<<")
                                || t.starts_with(">>>>>>>")
                            {
                                return Err(PatchError::Parse(format!(
                                    "line {}: found a merge-conflict / search-replace marker \
                                     ({l:?}); this tool does not use that format. Rewrite the hunk \
                                     with one marker per line — ' ' to keep a line, '-' to remove, \
                                     '+' to add — and no '=======', '<<<<<<<' or '>>>>>>>' \
                                     separators.",
                                    orig_no[idx]
                                )));
                            }
                            return Err(PatchError::Parse(format!(
                                "line {}: update hunk line must start with a marker — ' ' keeps a \
                                 line, '-' removes, '+' adds: {l:?}",
                                orig_no[idx]
                            )));
                        }
                    }
                }
                idx += 1;
            }

            if let Some(done) = cur.take() {
                chunks.push(done);
            }
            if chunks.is_empty() {
                return Err(PatchError::Parse(format!(
                    "line {header_no}: Update File `{}` has no hunks",
                    path.trim()
                )));
            }
            changes.push(FileChange::Update {
                path: path.trim().to_string(),
                move_to,
                chunks,
            });
            continue;
        }

        return Err(PatchError::Parse(format!(
            "line {}: unexpected line: {line:?}",
            orig_no[idx]
        )));
    }

    Err(PatchError::Parse("missing `*** End Patch` footer".into()))
}

fn looks_like_unified_diff(lines: &[&str]) -> bool {
    lines
        .iter()
        .any(|line| line.starts_with("--- ") || line.starts_with("diff --git "))
}

fn parse_unified_diff(lines: &[&str]) -> Result<Vec<FileChange>, PatchError> {
    let mut idx = 0usize;
    let mut changes = Vec::new();

    while idx < lines.len() {
        while idx < lines.len()
            && (lines[idx].trim().is_empty() || lines[idx].starts_with("diff --git "))
        {
            idx += 1;
        }
        if idx >= lines.len() {
            break;
        }
        if !lines[idx].starts_with("--- ") {
            return Err(PatchError::Parse(format!(
                "unified diff expected old-file header (`---`), got {:?}",
                lines[idx]
            )));
        }
        let old_path = unified_path(lines[idx].trim_start_matches("--- ").trim());
        idx += 1;
        if idx >= lines.len() || !lines[idx].starts_with("+++ ") {
            return Err(PatchError::Parse(
                "unified diff missing new-file header (`+++`)".into(),
            ));
        }
        let new_path = unified_path(lines[idx].trim_start_matches("+++ ").trim());
        idx += 1;

        let path = new_path
            .as_ref()
            .or(old_path.as_ref())
            .cloned()
            .ok_or_else(|| PatchError::Parse("unified diff has no real file path".into()))?;
        let mut chunks = Vec::new();
        let mut add_body = Vec::new();
        let is_add = old_path.is_none();
        let is_delete = new_path.is_none();

        while idx < lines.len()
            && !lines[idx].starts_with("--- ")
            && !lines[idx].starts_with("diff --git ")
        {
            let line = lines[idx];
            if line.starts_with("@@") {
                idx += 1;
                let mut chunk = UpdateChunk::default();
                while idx < lines.len()
                    && !lines[idx].starts_with("@@")
                    && !lines[idx].starts_with("--- ")
                    && !lines[idx].starts_with("diff --git ")
                {
                    let l = lines[idx];
                    if l.starts_with("\\ ") {
                        idx += 1;
                        continue;
                    }
                    let Some(marker) = l.chars().next() else {
                        chunk.old_lines.push(String::new());
                        chunk.new_lines.push(String::new());
                        idx += 1;
                        continue;
                    };
                    let rest = &l[marker.len_utf8()..];
                    match marker {
                        ' ' => {
                            chunk.old_lines.push(rest.to_string());
                            chunk.new_lines.push(rest.to_string());
                        }
                        '-' => {
                            if !is_add {
                                chunk.old_lines.push(rest.to_string());
                            }
                        }
                        '+' => {
                            if is_add {
                                add_body.push(rest.to_string());
                            } else {
                                chunk.new_lines.push(rest.to_string());
                            }
                        }
                        _ => {
                            return Err(PatchError::Parse(format!(
                                "unified diff hunk line must start with ' ', '-' or '+': {l:?}"
                            )));
                        }
                    }
                    idx += 1;
                }
                if !is_add && !is_delete {
                    chunks.push(chunk);
                }
                continue;
            }
            idx += 1;
        }

        if is_add {
            let mut content = add_body.join("\n");
            if !content.is_empty() {
                content.push('\n');
            }
            changes.push(FileChange::Add { path, content });
        } else if is_delete {
            changes.push(FileChange::Delete { path });
        } else {
            if chunks.is_empty() {
                return Err(PatchError::Parse(format!(
                    "unified diff for `{path}` has no hunks"
                )));
            }
            changes.push(FileChange::Update {
                path,
                move_to: None,
                chunks,
            });
        }
    }

    if changes.is_empty() {
        Err(PatchError::Parse("unified diff has no file changes".into()))
    } else {
        Ok(changes)
    }
}

fn unified_path(raw: &str) -> Option<String> {
    let path = raw.split_whitespace().next().unwrap_or(raw);
    if path == "/dev/null" {
        return None;
    }
    Some(
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_add_file() {
        let patch =
            "*** Begin Patch\n*** Add File: src/new.rs\n+fn a() {}\n+fn b() {}\n*** End Patch";
        let changes = parse_patch(patch).unwrap();
        assert_eq!(
            changes,
            vec![FileChange::Add {
                path: "src/new.rs".into(),
                content: "fn a() {}\nfn b() {}\n".into(),
            }]
        );
    }

    #[test]
    fn parses_markdown_fenced_patch() {
        let patch = "```patch\n*** Begin Patch\n*** Update File: src/lib.rs\n-old\n+new\n*** End Patch\n```";
        let changes = parse_patch(patch).unwrap();
        assert!(matches!(
            &changes[0],
            FileChange::Update { path, .. } if path == "src/lib.rs"
        ));
    }

    #[test]
    fn parses_unified_diff_update() {
        let patch = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
 keep
-old
+new
 end
";
        let changes = parse_patch(patch).unwrap();
        assert_eq!(
            changes,
            vec![FileChange::Update {
                path: "src/lib.rs".to_string(),
                move_to: None,
                chunks: vec![UpdateChunk {
                    context_label: None,
                    old_lines: vec!["keep".to_string(), "old".to_string(), "end".to_string()],
                    new_lines: vec!["keep".to_string(), "new".to_string(), "end".to_string()],
                    is_eof: false,
                }],
            }]
        );
    }

    #[test]
    fn parses_unified_diff_add_file() {
        let patch = "\
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,2 @@
+fn a() {}
+fn b() {}
";
        let changes = parse_patch(patch).unwrap();
        assert_eq!(
            changes,
            vec![FileChange::Add {
                path: "src/new.rs".to_string(),
                content: "fn a() {}\nfn b() {}\n".to_string(),
            }]
        );
    }

    #[test]
    fn parses_delete_file() {
        let patch = "*** Begin Patch\n*** Delete File: old.rs\n*** End Patch";
        let changes = parse_patch(patch).unwrap();
        assert_eq!(
            changes,
            vec![FileChange::Delete {
                path: "old.rs".into()
            }]
        );
    }

    #[test]
    fn parses_update_with_hunk() {
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@ fn main\n context\n-old\n+new\n*** End Patch";
        let changes = parse_patch(patch).unwrap();
        let FileChange::Update { chunks, .. } = &changes[0] else {
            panic!("expected update");
        };
        assert_eq!(chunks[0].context_label.as_deref(), Some("fn main"));
        assert_eq!(chunks[0].old_lines, vec!["context", "old"]);
        assert_eq!(chunks[0].new_lines, vec!["context", "new"]);
    }

    #[test]
    fn rejects_missing_begin() {
        assert!(parse_patch("*** Add File: x\n*** End Patch").is_err());
    }

    #[test]
    fn merge_conflict_marker_gives_actionable_error() {
        // Weaker models often reach for a search/replace / merge-conflict layout
        // ("=======" separator) instead of the diff-marker hunk format. The error
        // must name the mistake and say how to fix it, not just "must start with".
        let patch =
            "*** Begin Patch\n*** Update File: src/lib.rs\n old\n=======\n new\n*** End Patch";
        let msg = parse_patch(patch).unwrap_err().to_string();
        assert!(
            msg.contains("======="),
            "should quote the offending marker: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("merge") || msg.to_lowercase().contains("search"),
            "should explain it is a merge/search-replace marker: {msg}"
        );
        assert!(
            msg.contains("'+'") && msg.contains("'-'"),
            "should point back to the correct hunk markers: {msg}"
        );
    }

    #[test]
    fn empty_update_section_error_reports_the_header_line() {
        // An Update section with no hunk lines must point at ITS header, not
        // just name the path — in a multi-file patch the model needs to know
        // where the empty section sits.
        let patch = "*** Begin Patch\n*** Update File: a.txt\n*** End Patch\n";
        let msg = parse_patch(patch).unwrap_err().to_string();
        assert!(msg.contains("no hunks"), "names the failure: {msg}");
        assert!(msg.contains("line 2"), "points at the header line: {msg}");
    }

    #[test]
    fn hunk_marker_error_reports_line_number() {
        // A malformed hunk line must tell the model WHICH line is wrong so it
        // can fix it in one retry instead of re-diffing the whole file.
        let patch = "*** Begin Patch\n*** Update File: f\n a\nbad line\n+c\n*** End Patch";
        let msg = parse_patch(patch).unwrap_err().to_string();
        assert!(
            msg.contains("line 4"),
            "should name the offending line: {msg}"
        );
    }

    #[test]
    fn tolerates_indented_section_headers() {
        // A weak model indented the section header; the patch must still parse.
        let patch = "*** Begin Patch\n  *** Update File: src/lib.rs\n a\n-b\n+B\n*** End Patch";
        let changes = parse_patch(patch).unwrap();
        let FileChange::Update { path, chunks, .. } = &changes[0] else {
            panic!("expected update, got {changes:?}");
        };
        assert_eq!(path, "src/lib.rs");
        assert_eq!(chunks[0].old_lines, vec!["a", "b"]);
    }

    #[test]
    fn strips_markdown_code_fences_around_the_patch() {
        // Models intermittently wrap the whole patch in a markdown code block.
        let patch = "```\n*** Begin Patch\n*** Add File: a.rs\n+x\n*** End Patch\n```";
        assert!(parse_patch(patch).is_ok(), "fenced patch must parse");
        let patch = "```diff\n*** Begin Patch\n*** Delete File: a.rs\n*** End Patch\n```";
        assert!(parse_patch(patch).is_ok(), "language-tagged fence too");
    }

    #[test]
    fn fence_glued_to_end_patch_is_recognized() {
        // Seen in the wild: the closing line came out as "``` End Patch".
        let patch = "*** Begin Patch\n*** Update File: a.rs\n-x\n+y\n``` End Patch";
        let changes = parse_patch(patch).unwrap();
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn add_file_bare_lines_are_taken_as_content() {
        // Seen in the wild: "Add File line must start with '+'". The body of an
        // Add File section IS the new file's content, so a bare line is
        // unambiguous — accept it instead of forcing a retry round-trip.
        let patch = "*** Begin Patch\n*** Add File: a.rs\n//! doc line\n+fn x() {}\n*** End Patch";
        let FileChange::Add { content, .. } = &parse_patch(patch).unwrap()[0] else {
            panic!("expected add");
        };
        assert_eq!(content, "//! doc line\nfn x() {}\n");
    }

    #[test]
    fn add_file_misspelled_section_header_still_errors() {
        // A '*** ' line that matches no known header is a structural mistake,
        // not file content — swallowing it would corrupt the new file.
        let patch =
            "*** Begin Patch\n*** Add File: a.rs\n+x\n*** Updte File: b.rs\n+y\n*** End Patch";
        assert!(parse_patch(patch).is_err());
    }

    #[test]
    fn parses_move_to() {
        let patch =
            "*** Begin Patch\n*** Update File: a.rs\n*** Move to: b.rs\n-x\n+y\n*** End Patch";
        let FileChange::Update { move_to, .. } = &parse_patch(patch).unwrap()[0] else {
            panic!();
        };
        assert_eq!(move_to.as_deref(), Some("b.rs"));
    }
}
