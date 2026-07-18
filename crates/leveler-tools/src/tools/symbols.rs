//! Shared LSP-backed symbol location for the code-intelligence tools
//! (`read_symbol`, `find_references`). Precise where a language server is
//! available; the individual tools carry their own dependency-free fallbacks.

use std::path::Path;

use leveler_lsp::SymbolLocation;
use leveler_project::Language;

use crate::tool::ToolContext;

/// Locate a symbol's definitions via a language server, returning the language
/// whose server answered and the matching locations. `None` if no server is
/// available or the symbol is not found.
pub(crate) async fn lsp_locate(
    context: &ToolContext,
    root: &Path,
    symbol: &str,
) -> Option<(Language, Vec<SymbolLocation>)> {
    for language in leveler_project::detect_languages(root) {
        if !leveler_lsp::server_available_with_environment(language, &context.environment) {
            continue;
        }
        let Some(spec) = leveler_lsp::server_for(language) else {
            continue;
        };

        let mut sessions = context.lsp_sessions.lock().await;
        let key = language.as_str().to_string();
        if !sessions.contains_key(&key) {
            match leveler_lsp::LspClient::start(&spec.program, &spec.args, root).await {
                Ok(client) => {
                    sessions.insert(key.clone(), client);
                }
                Err(_) => continue,
            }
        }
        let client = sessions.get(&key)?;

        // The server may still be indexing on first use; retry briefly.
        let mut located = Vec::new();
        for _ in 0..6 {
            match client.workspace_symbols(symbol).await {
                Ok(found) if !found.is_empty() => {
                    located = found;
                    break;
                }
                Ok(_) => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
                Err(_) => break,
            }
        }
        let matches: Vec<_> = located
            .into_iter()
            .filter(|s| s.name.eq_ignore_ascii_case(symbol))
            .collect();
        if !matches.is_empty() {
            return Some((language, matches));
        }
    }
    None
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
    lines[line..=end].join("\n")
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
}
