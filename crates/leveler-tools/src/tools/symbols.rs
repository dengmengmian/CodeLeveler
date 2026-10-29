//! Bounded source scanning, authorized reads and symbol presentation.
//!
//! The language-server sessions themselves belong to
//! [`leveler_lsp::LspSessions`], which every one of these tools is constructed
//! with. Shared scans resolve file reads through the call's frozen workspace
//! authority, and presentation bounds the definition block and display paths.
//!
//! `find_symbol` used to carry its own copy of the session start + locate
//! logic, and three of these tools carried three drifting copies of the source
//! walk — one keyed on `repo_map::is_source`, two on a shorter hardcoded
//! extension list, so the same repository had two different ideas of what
//! counts as source. There is one of each now.

use crate::tool::ToolError;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Cap on the files a dependency-free scan will walk.
pub(crate) const MAX_SCAN_FILES: usize = 2000;

/// Directories no source scan enters.
const IGNORED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "dist",
    "vendor",
    ".leveler",
];

/// Workspace-relative source files under `dir`, bounded by [`MAX_SCAN_FILES`].
///
/// "Source" means whatever [`leveler_context::repo_map::is_source`] says, so
/// the scan and the repository map agree about the same tree.
#[cfg(test)]
pub(crate) fn collect_source_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    scan(
        root,
        dir,
        out,
        &mut std::collections::HashSet::new(),
        &CancellationToken::new(),
    )
    .unwrap();
}

/// Directory traversal is blocking; cancellation is checked at each entry.
pub(crate) async fn source_files(
    root: PathBuf,
    cancellation: CancellationToken,
) -> Result<Vec<String>, ToolError> {
    tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        let mut visited = std::collections::HashSet::new();
        scan(&root, &root, &mut files, &mut visited, &cancellation)?;
        Ok(files)
    })
    .await
    .map_err(|error| ToolError::Io(error.to_string()))?
}

fn scan(
    root: &Path,
    dir: &Path,
    files: &mut Vec<String>,
    visited: &mut std::collections::HashSet<PathBuf>,
    cancellation: &CancellationToken,
) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        return Err(ToolError::Io("symbol scan cancelled".into()));
    }
    if files.len() >= MAX_SCAN_FILES {
        return Ok(());
    }
    let canonical = dir
        .canonicalize()
        .map_err(|error| ToolError::Io(error.to_string()))?;
    if !visited.insert(canonical) {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).map_err(|error| ToolError::Io(error.to_string()))? {
        if cancellation.is_cancelled() {
            return Err(ToolError::Io("symbol scan cancelled".into()));
        }
        if files.len() >= MAX_SCAN_FILES {
            break;
        }
        let entry = entry.map_err(|error| ToolError::Io(error.to_string()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !name.starts_with('.') && !IGNORED_DIRS.contains(&name.as_str()) {
                scan(root, &path, files, visited, cancellation)?;
            }
        } else if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if leveler_context::repo_map::is_source(&rel) {
                files.push(rel);
            }
        }
    }
    Ok(())
}

/// Resolve every discovered or LSP-provided path through the frozen authority.
pub(crate) async fn read_source(
    workspace: &leveler_execution::Workspace,
    path: &Path,
    cancellation: &CancellationToken,
    max_bytes: Option<usize>,
) -> Result<String, ToolError> {
    use tokio::io::AsyncReadExt;
    let path = workspace.resolve_for_read(path)?;
    let read = async {
        let file = tokio::fs::File::open(&path).await?;
        if !file.metadata().await?.is_file() {
            return Err(std::io::Error::other("symbol source is not a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(max_bytes.map_or(u64::MAX, |limit| limit as u64))
            .read_to_end(&mut bytes)
            .await?;
        if max_bytes.is_some() {
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        } else {
            String::from_utf8(bytes)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        }
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(ToolError::Io("symbol read cancelled".into())),
        result = read => result.map_err(|error| ToolError::Io(error.to_string())),
    }
}

/// Make an absolute path relative to `root` for display, if possible.
pub(crate) fn relativize(path: &str, root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    Path::new(path)
        .strip_prefix(&canonical)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Extract a definition block starting at 0-based `line`: for brace languages,
/// capture until the braces balance; otherwise a bounded window (Python etc.).
pub(crate) fn extract_block(text: &str, line: usize, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if line >= lines.len() {
        return String::new();
    }
    let mut depth: i32 = 0;
    let mut seen_open = false;
    let mut found_end = false;
    let mut end = line;
    for (offset, row) in lines[line..].iter().enumerate().take(max_lines) {
        for ch in row.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    seen_open = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        end = line + offset;
        if seen_open && depth <= 0 {
            found_end = true;
            break;
        }
        // A brace-less declaration terminated by `;` (e.g. `type X = ...;`).
        if !seen_open && row.trim_end().ends_with(';') {
            found_end = true;
            break;
        }
    }
    if !found_end && !seen_open {
        // Indentation-based languages: return a bounded window.
        end = (line + 20).min(lines.len() - 1);
    }
    let mut block = lines[line..=end].join("\n");
    if !found_end && seen_open {
        // The braces never closed within the window: the symbol body continues
        // past the clip. Without a marker the model may treat the clip point as
        // the end of the definition.
        block.push_str(&format!(
            "\n… [symbol body clipped after {max_lines} lines; use read_file \
             with start_line={} for the rest]",
            end + 2
        ));
    }
    block
}

/// The 0-based character offset of `name` on `line_text`, if present.
pub(crate) fn column_of(line_text: &str, name: &str) -> u64 {
    line_text
        .find(name)
        .map(|byte| line_text[..byte].chars().count() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_scan_caps_a_flat_directory() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..MAX_SCAN_FILES + 1 {
            std::fs::write(dir.path().join(format!("file{index}.rs")), "").unwrap();
        }
        let mut files = Vec::new();
        collect_source_files(dir.path(), dir.path(), &mut files);
        assert_eq!(files.len(), MAX_SCAN_FILES);
    }

    #[test]
    fn extract_block_marks_an_unclosed_clip() {
        // A body longer than max_lines is clipped mid-function; without a
        // marker the model may treat the clip point as the end of the symbol.
        let text = format!("fn big() {{\n{}}}\n", "    call();\n".repeat(300));
        let block = extract_block(&text, 0, 200);
        assert!(
            block.contains("clipped"),
            "mid-body clip must be marked: …{}",
            &block[block.len().saturating_sub(120)..]
        );
        assert!(
            block.contains("read_file"),
            "marker should point at the recovery tool: …{}",
            &block[block.len().saturating_sub(120)..]
        );
    }

    #[test]
    fn extract_block_captures_a_braced_definition() {
        let src = "before\nfn target() {\n    let x = 1;\n    x + 1\n}\nafter\n";
        let block = extract_block(src, 1, 50);
        assert!(block.starts_with("fn target() {"));
        assert!(block.trim_end().ends_with('}'));
        assert!(!block.contains("after"));
    }

    #[test]
    fn extract_block_handles_one_line_declaration() {
        let src = "type Alias = Vec<u8>;\nnext\n";
        let block = extract_block(src, 0, 50);
        assert_eq!(block, "type Alias = Vec<u8>;");
    }

    #[test]
    fn column_of_counts_characters_not_bytes() {
        // Leading full-width chars must be counted as chars.
        assert_eq!(column_of("    fn foo", "foo"), 7);
        assert_eq!(column_of("你好 foo", "foo"), 3);
    }

    /// One source walk for every symbol tool, so `find_symbol` and
    /// `read_symbol` cannot disagree about which files exist.
    #[test]
    fn the_source_walk_skips_ignored_trees_and_non_source_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        std::fs::write(root.join("src/notes.md"), "").unwrap();
        std::fs::write(root.join("target/debug/b.rs"), "").unwrap();

        let mut found = Vec::new();
        collect_source_files(root, root, &mut found);
        found.sort();
        assert_eq!(found, vec!["src/a.rs".to_string()]);
    }
}
