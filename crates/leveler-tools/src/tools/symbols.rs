//! Shared LSP-backed symbol location for the code-intelligence tools
//! (`read_symbol`, `find_references`). Precise where a language server is
//! available; the individual tools carry their own dependency-free fallbacks.

use std::path::Path;

use leveler_lsp::SymbolLocation;
use leveler_project::Language;

use crate::tool::ToolContext;

pub(crate) fn remove_if_same<T>(
    sessions: &mut std::collections::HashMap<String, std::sync::Arc<T>>,
    key: &str,
    expected: &std::sync::Arc<T>,
) {
    if sessions
        .get(key)
        .is_some_and(|current| std::sync::Arc::ptr_eq(current, expected))
    {
        sessions.remove(key);
    }
}

pub(crate) async fn get_or_start_lsp(
    context: &ToolContext,
    key: &str,
    program: &str,
    args: &[String],
    root: &Path,
) -> Result<std::sync::Arc<leveler_lsp::LspClient>, String> {
    if let Some(client) = context.lsp_sessions.lock().await.get(key).cloned() {
        return Ok(client);
    }
    let start_lock = {
        let mut locks = context.lsp_start_locks.lock().await;
        locks
            .entry(key.to_string())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _starting = start_lock.lock().await;
    if let Some(client) = context.lsp_sessions.lock().await.get(key).cloned() {
        return Ok(client);
    }

    // Only this language's startup lock is held across process launch. Other
    // languages and already-running sessions remain available concurrently.
    let client = std::sync::Arc::new(
        leveler_lsp::LspClient::start(program, args, root)
            .await
            .map_err(|error| error.to_string())?,
    );
    context
        .lsp_sessions
        .lock()
        .await
        .insert(key.to_string(), client.clone());
    Ok(client)
}

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

        let key = language.as_str().to_string();
        // Clone the session's Arc out under the lock, then release it before the
        // request/retry loop — holding the global lock across `sleep` would
        // serialize every LSP tool and stall them for seconds.
        let client = match get_or_start_lsp(context, &key, &spec.program, &spec.args, root).await {
            Ok(client) => client,
            Err(_) => continue,
        };

        // The server may still be indexing on first use; retry briefly.
        let mut located = Vec::new();
        let mut server_died = false;
        for _ in 0..6 {
            match client.workspace_symbols(symbol).await {
                Ok(found) if !found.is_empty() => {
                    located = found;
                    break;
                }
                Ok(_) => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
                // The server crashed or timed out. Evict the dead client so the
                // NEXT call restarts it — otherwise a single crash/timeout pins a
                // corpse in the map and this language degrades to scan forever.
                Err(_) => {
                    server_died = true;
                    break;
                }
            }
        }
        if server_died {
            // Re-acquire only to evict, and only if it's still the same dead
            // client (a concurrent call may have already restarted it).
            let mut sessions = context.lsp_sessions.lock().await;
            remove_if_same(&mut sessions, &key, &client);
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
    fn eviction_never_removes_a_concurrently_restarted_session() {
        let stale = std::sync::Arc::new(1_u8);
        let restarted = std::sync::Arc::new(2_u8);
        let mut sessions = std::collections::HashMap::new();
        sessions.insert("rust".to_string(), restarted.clone());

        remove_if_same(&mut sessions, "rust", &stale);

        assert!(std::sync::Arc::ptr_eq(
            sessions.get("rust").unwrap(),
            &restarted
        ));
        remove_if_same(&mut sessions, "rust", &restarted);
        assert!(!sessions.contains_key("rust"));
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
}
