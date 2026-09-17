//! Terminal output helpers. Rendering only — no business logic.

use console::style;

use leveler_app::doctor::{CheckResult, CheckStatus};

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

/// Render a doctor check result.
pub fn print_check(result: &CheckResult) {
    let mark = match result.status {
        CheckStatus::Ok => style("✓").green(),
        CheckStatus::Warn => style("!").yellow(),
        CheckStatus::Fail => style("✗").red(),
    };
    println!(
        "  {mark} {:<22} {}",
        result.name,
        style(&result.detail).dim()
    );
}
