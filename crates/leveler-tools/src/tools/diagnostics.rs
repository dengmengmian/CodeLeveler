//! `diagnostics` — compiler/linter diagnostics for a file via its language
//! server (`textDocument/publishDiagnostics`). The LSP client already collects
//! them; this tool opens the file, waits briefly for the server to publish, and
//! renders them. Complements a full build: faster, file-scoped.

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_lsp::Diagnostic;
use leveler_project::Language;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// How long to wait for the server to publish diagnostics after opening a file.
const WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// File to report diagnostics for, relative to the workspace root.
    path: String,
}

pub struct DiagnosticsTool;

#[async_trait]
impl Tool for DiagnosticsTool {
    fn name(&self) -> &'static str {
        "diagnostics"
    }

    fn description(&self) -> &'static str {
        "Report compiler/linter diagnostics (errors, warnings) for a single file \
         via its language server. Use after editing to see problems without a full \
         build. Requires a language server for the file's language to be installed; \
         degrades to a clear message when none is available."
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
        let abs = context.workspace.resolve_read(&input.path)?;
        let root = context.workspace.root().to_path_buf();

        let Some(language) = Language::from_path(&abs) else {
            return Ok(ToolOutput::ok(format!(
                "(no recognized language for `{}`; diagnostics need a language server)\n",
                input.path
            )));
        };
        if !leveler_lsp::server_available_with_environment(language, &context.environment) {
            return Ok(ToolOutput::ok(format!(
                "(no language server available for {}; diagnostics unavailable — a full \
                 build via run_command still works)\n",
                language.as_str()
            )));
        }
        let Some(spec) = leveler_lsp::server_for(language) else {
            return Ok(ToolOutput::ok(format!(
                "(no language server configured for {})\n",
                language.as_str()
            )));
        };

        // Ensure a session, mirroring the code-intelligence tools. Clone the Arc
        // out and drop the lock before `wait_for_diagnostics` (up to WAIT
        // seconds) so a diagnostics call doesn't block every other LSP tool.
        let key = language.as_str().to_string();
        let client = match super::symbols::get_or_start_lsp(
            &context,
            &key,
            &spec.program,
            &spec.args,
            &root,
        )
        .await
        {
            Ok(client) => client,
            Err(error) => {
                return Ok(ToolOutput::ok(format!(
                    "(could not start {} language server: {error})\n",
                    spec.program
                )));
            }
        };
        let _ = client.open(&abs, &spec.language_id).await;
        let diags = client.wait_for_diagnostics(&abs, WAIT).await;

        // Distinguish "analyzed and clean" from "the server never reported":
        // an empty result with no publish is a timeout, not a clean bill.
        if diags.is_empty() && !client.diagnostics_reported(&abs).await {
            return Ok(ToolOutput::ok(format!(
                "(no diagnostics received for `{}` within {}s — the language server may \
                 still be analyzing; re-run to confirm before trusting a clean result)\n",
                input.path,
                WAIT.as_secs()
            )));
        }

        Ok(ToolOutput::ok(format_diagnostics(&input.path, &diags)))
    }
}

/// LSP DiagnosticSeverity codes (1=Error … 4=Hint).
fn severity_label(severity: i64) -> &'static str {
    match severity {
        1 => "error",
        2 => "warning",
        3 => "info",
        4 => "hint",
        _ => "diagnostic",
    }
}

/// Render diagnostics as a header line plus one `path:line: [severity] message`
/// per entry, sorted by line then severity.
fn format_diagnostics(path: &str, diags: &[Diagnostic]) -> String {
    if diags.is_empty() {
        return format!("(no diagnostics for `{path}`)\n");
    }
    let mut sorted: Vec<&Diagnostic> = diags.iter().collect();
    sorted.sort_by_key(|d| (d.line, d.severity));
    let errors = diags.iter().filter(|d| d.severity == 1).count();
    let warnings = diags.iter().filter(|d| d.severity == 2).count();
    let mut body = format!("{path}: {errors} error(s), {warnings} warning(s)\n");
    for d in sorted {
        body.push_str(&format!(
            "- {path}:{}: [{}] {}\n",
            d.line + 1,
            severity_label(d.severity),
            d.message
        ));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_empty_diagnostics() {
        let out = format_diagnostics("src/lib.rs", &[]);
        assert!(out.contains("(no diagnostics for `src/lib.rs`)"));
    }

    #[test]
    fn formats_errors_and_warnings_sorted_by_line() {
        let diags = vec![
            Diagnostic {
                line: 9,
                severity: 2,
                message: "unused import".into(),
            },
            Diagnostic {
                line: 2,
                severity: 1,
                message: "mismatched types".into(),
            },
        ];
        let out = format_diagnostics("src/lib.rs", &diags);
        assert!(out.contains("1 error(s), 1 warning(s)"), "got: {out}");
        // The line-2 error must render before the line-9 warning (1-based).
        let err_at = out.find("src/lib.rs:3:").expect("error line");
        let warn_at = out.find("src/lib.rs:10:").expect("warning line");
        assert!(err_at < warn_at, "got: {out}");
        assert!(out.contains("[error] mismatched types"));
        assert!(out.contains("[warning] unused import"));
    }

    #[test]
    fn severity_labels_map_lsp_codes() {
        assert_eq!(severity_label(1), "error");
        assert_eq!(severity_label(2), "warning");
        assert_eq!(severity_label(3), "info");
        assert_eq!(severity_label(4), "hint");
        assert_eq!(severity_label(99), "diagnostic");
    }
}
