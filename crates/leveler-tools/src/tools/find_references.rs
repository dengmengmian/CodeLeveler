//! `find_references` — find where a symbol is USED. Precise via a language
//! server (`textDocument/references`); falls back to a whole-word scan.

use std::path::Path;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use super::symbols::{column_of, lsp_locate, relativize};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_HITS: usize = 200;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// The symbol name whose references to find.
    symbol: String,
}

pub struct FindReferencesTool;

#[async_trait]
impl Tool for FindReferencesTool {
    fn name(&self) -> &'static str {
        "find_references"
    }

    fn description(&self) -> &'static str {
        "Find every place a symbol is USED (call sites, mentions), not just its \
         definition. Precise via a language server when available, else a \
         whole-word scan. Complements find_symbol (which locates definitions)."
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

        // Precise: locate the definition, then ask the server for references.
        if let Some((language, matches)) = lsp_locate(&context, &root, &input.symbol).await
            && let Some(m) = matches.first()
            && let Some(body) = lsp_references(&context, language, m, &input.symbol, &root).await
        {
            return Ok(ToolOutput::ok(body));
        }

        // Fallback: whole-word scan across source files.
        let mut files = Vec::new();
        collect_source_files(&root, &root, &mut files);
        let mut hits = Vec::new();
        for rel in &files {
            let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                let is_ref = line
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|w| w == input.symbol);
                if is_ref {
                    hits.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                    if hits.len() >= MAX_HITS {
                        break;
                    }
                }
            }
            if hits.len() >= MAX_HITS {
                break;
            }
        }
        if hits.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "(no references to `{}` found)\n",
                input.symbol
            )));
        }
        let mut body = format!("References to `{}` (via scan):\n", input.symbol);
        for h in &hits {
            body.push_str(&format!("- {h}\n"));
        }
        Ok(ToolOutput::ok(body))
    }
}

/// Query the language server for references to the located symbol.
async fn lsp_references(
    context: &ToolContext,
    language: leveler_project::Language,
    def: &leveler_lsp::SymbolLocation,
    symbol: &str,
    root: &Path,
) -> Option<String> {
    let spec = leveler_lsp::server_for(language)?;
    let def_path = Path::new(&def.path);
    let line_text = std::fs::read_to_string(def_path)
        .ok()?
        .lines()
        .nth(def.line as usize)
        .unwrap_or("")
        .to_string();
    let character = column_of(&line_text, symbol);

    let sessions = context.lsp_sessions.lock().await;
    let client = sessions.get(language.as_str())?;
    // References need the document open.
    let _ = client.open(def_path, &spec.language_id).await;
    let refs = client
        .references(def_path, def.line, character, false)
        .await
        .ok()?;
    if refs.is_empty() {
        return None;
    }
    let mut body = format!("References to `{symbol}` (via {}):\n", spec.program);
    for r in refs.iter().take(MAX_HITS) {
        body.push_str(&format!("- {}:{}\n", relativize(&r.path, root), r.line + 1));
    }
    Some(body)
}

const MAX_FILES: usize = 2000;

fn collect_source_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    const IGNORED: &[&str] = &[
        "target",
        "node_modules",
        ".git",
        "dist",
        "vendor",
        ".leveler",
    ];
    const EXTS: &[&str] = &[
        "rs", "go", "ts", "tsx", "js", "jsx", "py", "java", "c", "h", "cpp",
    ];
    if out.len() >= MAX_FILES {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !IGNORED.contains(&name.as_str()) && !name.starts_with('.') {
                collect_source_files(root, &path, out);
            }
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| EXTS.contains(&e))
            .unwrap_or(false)
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fallback_scan_finds_references() {
        let dir =
            std::env::temp_dir().join(format!("leveler-refs-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("lib.rs"),
            "fn target() {}\nfn caller() { target(); }\n",
        )
        .unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = FindReferencesTool
            .execute(
                serde_json::json!({"symbol": "target"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("References to `target`"));
        assert!(out.content.contains("lib.rs:1"));
        assert!(out.content.contains("lib.rs:2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn fallback_scan_reports_no_references() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-refs-empty-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.rs"), "fn other() {}\n").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = FindReferencesTool
            .execute(
                serde_json::json!({"symbol": "missing"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("(no references to `missing` found)"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
