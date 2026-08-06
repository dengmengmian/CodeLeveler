//! `read_symbol` — read a symbol's definition body by name, without loading the
//! whole file. Precise via a language server; falls back to a definition scan.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use super::symbols::{extract_block, lsp_locate, relativize};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_BLOCK_LINES: usize = 200;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// The symbol name (function, type, class, ...) to read.
    symbol: String,
}

pub struct ReadSymbolTool;

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
        let root = context.workspace.root().to_path_buf();

        // Precise: language-server location, then read the block from the file.
        if let Some((_, matches)) = lsp_locate(&context, &root, &input.symbol).await {
            let mut body = String::new();
            for m in matches.iter().take(3) {
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

const MAX_FILES: usize = 2000;

fn collect_source_files(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
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
