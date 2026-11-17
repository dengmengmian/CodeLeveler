//! Terminal output helpers. Rendering only — no business logic.

use console::style;

use leveler_app::doctor;
use leveler_client_protocol::{DiagnosticFinding, DiagnosticStatus};

/// A styled output line builder.
pub struct Line;

impl Line {
    pub fn heading(text: &str) -> String {
        style(text).bold().cyan().to_string()
    }
    pub fn ok(text: &str) -> String {
        format!("{} {}", style("✓").green().bold(), text)
    }
    pub fn warn(text: &str) -> String {
        format!("{} {}", style("!").yellow().bold(), text)
    }
    pub fn fail(text: &str) -> String {
        format!("{} {}", style("✗").red().bold(), text)
    }
}

/// Error banner prefix for the top-level handler.
pub fn error_prefix() -> String {
    style("error:").red().bold().to_string()
}

/// Render one structured diagnostic finding as a text line.
pub fn print_finding(finding: &DiagnosticFinding) {
    let (mark, word) = match finding.status {
        DiagnosticStatus::Pass => (style("✓").green(), "ok"),
        DiagnosticStatus::Warn => (style("!").yellow(), "warn"),
        DiagnosticStatus::Fail => (style("✗").red(), "fail"),
        DiagnosticStatus::Unknown => (style("?").dim(), "unknown"),
        DiagnosticStatus::NotChecked => (style("-").dim(), "not checked"),
        DiagnosticStatus::NotApplicable => (style("-").dim(), "n/a"),
    };
    let name = match &finding.subject {
        Some(subject) => format!("{} {subject}", doctor::label(&finding.id)),
        None => doctor::label(&finding.id).to_string(),
    };
    let evidence = finding.evidence.join("; ");
    println!("  {mark} {:<24} [{word}] {}", name, style(&evidence).dim());
    if let Some(action) = finding.action.as_deref() {
        let sentence = doctor::action_label(action);
        if !sentence.is_empty() {
            println!("      {}", style(format!("→ {sentence}")).dim());
        }
    }
}
