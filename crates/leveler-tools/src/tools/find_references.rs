//! `find_references` — find where a symbol is USED. Precise via a language
//! server (`textDocument/references`); falls back to a whole-word scan.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_lsp::{Located, LspSessions};

use super::symbols::{collect_source_files, column_of, relativize};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_HITS: usize = 200;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// The symbol name whose references to find.
    symbol: String,
}

/// Constructed with the language-server sessions it uses, and nothing else.
pub struct FindReferencesTool {
    lsp: Arc<LspSessions>,
}

impl FindReferencesTool {
    pub fn new(lsp: Arc<LspSessions>) -> Self {
        Self { lsp }
    }
}

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
        let root = context.execution.workspace.root().to_path_buf();

        // Precise: locate the definition, then ask the server for references.
        if let Some(located) = self.lsp.locate(&root, &input.symbol).await
            && let Some(body) = lsp_references(&located, &input.symbol, &root).await
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
        let capped = hits.len() >= MAX_HITS;
        let mut body = format!("References to `{}` (via scan):\n", input.symbol);
        for h in &hits {
            body.push_str(&format!("- {h}\n"));
        }
        if capped {
            body.push_str(&format!(
                "… [references capped at {MAX_HITS}; narrow with `grep` on a \
                 subdirectory]\n"
            ));
        }
        Ok(ToolOutput::ok(body))
    }
}

/// Query the language server for references to the located symbol.
///
/// Reuses the session that answered the definition query — a second lookup
/// could race a restart and answer about a different generation of the index.
async fn lsp_references(located: &Located, symbol: &str, root: &Path) -> Option<String> {
    let def = located.definitions.first()?;
    let def_path = Path::new(&def.path);
    let line_text = std::fs::read_to_string(def_path)
        .ok()?
        .lines()
        .nth(def.line as usize)
        .unwrap_or("")
        .to_string();
    let character = column_of(&line_text, symbol);

    // References need the document open.
    let _ = located
        .client
        .open(def_path, &located.spec.language_id)
        .await;
    let refs = located
        .client
        .references(def_path, def.line, character, false)
        .await
        .ok()?;
    if refs.is_empty() {
        return None;
    }
    let mut body = format!("References to `{symbol}` (via {}):\n", located.spec.program);
    for r in refs.iter().take(MAX_HITS) {
        body.push_str(&format!("- {}:{}\n", relativize(&r.path, root), r.line + 1));
    }
    if refs.len() > MAX_HITS {
        body.push_str(&format!(
            "… [showing {MAX_HITS} of {} references; narrow with `grep` on a \
             subdirectory]\n",
            refs.len()
        ));
    }
    Some(body)
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
        let out = FindReferencesTool::new(crate::tools::test_capabilities().lsp)
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
    async fn fallback_scan_marks_capped_results() {
        // More references than MAX_HITS must produce a marker, not a silently
        // complete-looking list.
        let dir =
            std::env::temp_dir().join(format!("leveler-refs-cap-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = "target();\n".repeat(MAX_HITS + 5);
        std::fs::write(dir.join("lib.rs"), body).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = FindReferencesTool::new(crate::tools::test_capabilities().lsp)
            .execute(
                serde_json::json!({"symbol": "target"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.content.contains(&format!("capped at {MAX_HITS}")),
            "capped scan must carry a marker: {}",
            &out.content[out.content.len().saturating_sub(200)..]
        );
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
        let out = FindReferencesTool::new(crate::tools::test_capabilities().lsp)
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
