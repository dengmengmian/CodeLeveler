//! `read_symbol` — read a symbol's definition body by name, without loading the
//! whole file. Precise via a language server; falls back to a definition scan.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_lsp::LspSessions;

use super::symbols::{collect_source_files, extract_block, relativize};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_BLOCK_LINES: usize = 200;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// The symbol name (function, type, class, ...) to read.
    symbol: String,
}

/// Constructed with the language-server sessions it uses, and nothing else.
pub struct ReadSymbolTool {
    lsp: Arc<LspSessions>,
}

impl ReadSymbolTool {
    pub fn new(lsp: Arc<LspSessions>) -> Self {
        Self { lsp }
    }
}

#[async_trait]
impl Tool for ReadSymbolTool {
    fn name(&self) -> &'static str {
        "read_symbol"
    }

    fn description(&self) -> &'static str {
        "Read the DEFINITION body of a symbol (function/type/class/etc) by name, \
         without loading the whole file. Precise via a language server when \
         available. Prefer this over read_file when you only need one symbol."
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

        // Precise: language-server location, then read the block from the file.
        if let Some(located) = self.lsp.locate(&root, &input.symbol).await {
            let mut body = String::new();
            for m in located.definitions.iter().take(3) {
                if let Ok(text) = std::fs::read_to_string(&m.path) {
                    let block = extract_block(&text, m.line as usize, MAX_BLOCK_LINES);
                    body.push_str(&format!(
                        "{}:{}\n{}\n\n",
                        relativize(&m.path, &root),
                        m.line + 1,
                        block
                    ));
                }
            }
            if !body.is_empty() {
                return Ok(ToolOutput::ok(body));
            }
        }

        // Fallback: find a defining file by scan, then extract the block there.
        let mut files = Vec::new();
        collect_source_files(&root, &root, &mut files);
        for rel in files {
            let Ok(text) = std::fs::read_to_string(root.join(&rel)) else {
                continue;
            };
            if !leveler_context::defines(&text, &input.symbol) {
                continue;
            }
            if let Some(line) = definition_line(&text, &input.symbol) {
                let block = extract_block(&text, line, MAX_BLOCK_LINES);
                return Ok(ToolOutput::ok(format!("{rel}:{}\n{block}\n", line + 1)));
            }
        }

        Ok(ToolOutput::ok(format!(
            "(no definition of `{}` found)\n",
            input.symbol
        )))
    }
}

/// The line index where `symbol` is defined. Prefers a line that actually
/// *declares* it (`fn`/`struct`/`def`/`class`/…, via [`leveler_context::defines`])
/// so an earlier `use`/import or a call site above the definition doesn't send
/// the reader to the wrong block. Falls back to the first whole-word mention
/// when no single line reads as a declaration.
fn definition_line(text: &str, symbol: &str) -> Option<usize> {
    if let Some(i) = text
        .lines()
        .position(|line| leveler_context::defines(line, symbol))
    {
        return Some(i);
    }
    text.lines().position(|line| {
        line.split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|w| w == symbol)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_line_finds_first_occurrence() {
        let text = "fn other() {}\nfn target() {}\nfn target() {}\n";
        assert_eq!(definition_line(text, "target"), Some(1));
    }

    #[test]
    fn definition_line_ignores_partial_word_matches() {
        let text = "fn targetable() {}\nfn target() {}\n";
        assert_eq!(definition_line(text, "target"), Some(1));
    }

    #[test]
    fn definition_line_returns_none_when_missing() {
        let text = "fn other() {}\n";
        assert_eq!(definition_line(text, "target"), None);
    }

    #[test]
    fn definition_line_prefers_the_declaration_over_an_earlier_use_or_call() {
        // A `use` import and a call site both mention `foo` before its real
        // definition; the reader must land on the `fn foo` line, not line 0.
        let text = "use crate::foo;\nfn bar() { foo(); }\nfn foo() {}\n";
        assert_eq!(definition_line(text, "foo"), Some(2));
    }
}
