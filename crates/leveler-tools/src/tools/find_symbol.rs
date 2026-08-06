//! `find_symbol` — locate where a symbol is defined (spec §26). Uses the
//! dependency-free symbol scan; complements `grep` by matching *definitions*,
//! not every mention.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_FILES: usize = 2000;
const MAX_SCAN_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// The symbol name to locate (function, type, class, ...).
    symbol: String,
}

pub struct FindSymbolTool;

#[async_trait]
impl Tool for FindSymbolTool {
    fn name(&self) -> &'static str {
        "find_symbol"
    }

    fn description(&self) -> &'static str {
        "Find where a symbol (function/type/class/etc) is DEFINED. Uses a language \
         server for precise `path:line` when one is available, else a fast scan \
         returning the defining files. Complements `grep`, which matches every \
         mention."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let root = context.workspace.root().to_path_buf();

        // Precise path: ask a language server (reused across calls).
        if let Some(body) = lsp_lookup(&context, &root, &input.symbol).await {
            return Ok(ToolOutput::ok(body));
        }

        // Fallback: dependency-free definition scan (files only).
        let mut files = Vec::new();
        collect_source_files(&root, &root, &mut files);
        let mut hits = Vec::new();
        for rel in files {
            let Ok(bytes) = std::fs::read(root.join(&rel)) else {
                continue;
            };
            let slice = &bytes[..bytes.len().min(MAX_SCAN_BYTES)];
            let text = String::from_utf8_lossy(slice);
            if leveler_context::defines(&text, &input.symbol) {
                hits.push(rel);
            }
        }
        hits.sort();

        if hits.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "(no definition of `{}` found)\n",
                input.symbol
            )));
        }
        let mut body = format!("`{}` is defined in (via scan):\n", input.symbol);
        for h in &hits {
            body.push_str(&format!("- {h}\n"));
        }
        Ok(ToolOutput::ok(body))
    }
}

/// Try each detected language's server (starting/caching it), query
/// `workspace/symbol`, and return `path:line` for exact-name definitions.
/// Returns `None` on any failure so the caller falls back to the scan.
async fn lsp_lookup(context: &ToolContext, root: &std::path::Path, symbol: &str) -> Option<String> {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    for language in leveler_project::detect_languages(root) {
        if !leveler_lsp::server_available_with_environment(language, &context.environment) {
            continue;
        }
        let Some(spec) = leveler_lsp::server_for(language) else {
            continue;
        };

        let key = language.as_str().to_string();
        // Clone the session Arc out under the lock, then drop it before the
        // request/retry loop so LSP tools don't serialize on the global lock.
        let client =
            match super::symbols::get_or_start_lsp(context, &key, &spec.program, &spec.args, root)
                .await
            {
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
                // Evict a crashed/timed-out server so the next call restarts it
                // instead of reusing a corpse and degrading to scan forever.
                Err(_) => {
                    server_died = true;
                    break;
                }
            }
        }
        if server_died {
            let mut sessions = context.lsp_sessions.lock().await;
            super::symbols::remove_if_same(&mut sessions, &key, &client);
        }

        let matches: Vec<_> = located
            .into_iter()
            .filter(|s| s.name.eq_ignore_ascii_case(symbol))
            .collect();
        if matches.is_empty() {
            continue;
        }
        let mut body = format!("`{symbol}` is defined at (via {}):\n", spec.program);
        for m in matches {
            let rel = std::path::Path::new(&m.path)
                .strip_prefix(&canonical_root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| m.path.clone());
            body.push_str(&format!("- {rel}:{}\n", m.line + 1));
        }
        return Some(body);
    }
    None
}

fn collect_source_files(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    const IGNORED: &[&str] = &[
        "target",
        "node_modules",
        ".git",
        "dist",
        "vendor",
        ".leveler",
    ];
    if out.len() >= MAX_FILES {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_source_files(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if leveler_context::repo_map::is_source(&rel) {
                out.push(rel);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_definition_site() {
        let dir =
            std::env::temp_dir().join(format!("leveler-findsym-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "pub fn cancel_order() {}\n").unwrap();
        std::fs::write(dir.join("src/b.rs"), "fn caller() { cancel_order(); }\n").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = FindSymbolTool
            .execute(
                serde_json::json!({ "symbol": "cancel_order" }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("src/a.rs"));
        assert!(
            !out.content.contains("src/b.rs"),
            "b.rs only calls it: {}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
