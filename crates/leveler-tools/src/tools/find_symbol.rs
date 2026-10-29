//! `find_symbol` — locate where a symbol is defined (spec §26). Precise
//! through a language server; otherwise a dependency-free definition scan that
//! says which files define the name and labels itself as a scan.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_lsp::LspSessions;

use super::symbols::{read_source, source_files};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_SCAN_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// The symbol name to locate (function, type, class, ...).
    symbol: String,
}

/// Constructed with the language-server sessions it uses, and nothing else.
pub struct FindSymbolTool {
    lsp: Arc<LspSessions>,
}

impl FindSymbolTool {
    pub fn new(lsp: Arc<LspSessions>) -> Self {
        Self { lsp }
    }
}

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
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let workspace = context.execution_workspace()?;
        let root = workspace.root().to_path_buf();

        // Precise path: ask a language server (session reused across calls).
        if let Some(located) = self.lsp.locate(&root, &input.symbol).await {
            let mut body = format!(
                "`{}` is defined at (via {}):\n",
                input.symbol, located.spec.program
            );
            for definition in &located.definitions {
                body.push_str(&format!(
                    "- {}:{}\n",
                    super::symbols::relativize(&definition.path, &root),
                    definition.line + 1
                ));
            }
            return Ok(ToolOutput::ok(body));
        }

        // Fallback: dependency-free definition scan (files only). Labelled as a
        // scan in the result, because it answers a weaker question than the
        // server does — which files define the name, not where.
        let files = source_files(root.clone(), cancellation.clone()).await?;
        let mut hits = Vec::new();
        for rel in files {
            let text = read_source(
                &workspace,
                &root.join(&rel),
                &cancellation,
                Some(MAX_SCAN_BYTES),
            )
            .await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> FindSymbolTool {
        FindSymbolTool::new(Arc::new(LspSessions::new(Arc::new(
            leveler_core::environment().clone(),
        ))))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symbol_scan_obeys_sensitive_path_authority() {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join(".env");
        std::fs::write(&secret, "fn private_marker() {}\n").unwrap();
        std::os::unix::fs::symlink(&secret, repo.path().join("alias.rs")).unwrap();
        for (mode, allowed) in [
            (leveler_execution::PermissionProfile::RequestApproval, false),
            (leveler_execution::PermissionProfile::FullAccess, true),
        ] {
            let ctx = ToolContext::new(
                leveler_execution::Workspace::new(repo.path()).unwrap(),
                mode,
            );
            let result = tool()
                .execute(
                    serde_json::json!({"symbol":"private_marker"}),
                    ctx,
                    CancellationToken::new(),
                )
                .await;
            if allowed {
                assert!(result.unwrap().content.contains("alias.rs"));
            } else {
                assert!(
                    result.is_err(),
                    "sensitive symlink must require authority: {result:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn finds_definition_site() {
        let dir =
            std::env::temp_dir().join(format!("leveler-findsym-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "pub fn cancel_order() {}\n").unwrap();
        std::fs::write(dir.join("src/b.rs"), "fn caller() { cancel_order(); }\n").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = tool()
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
