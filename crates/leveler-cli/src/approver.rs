//! Interactive approval prompt for risky actions.

use async_trait::async_trait;
use console::style;

use leveler_execution::{ApprovalDecision, ApprovalRequest, Approver};

/// Prompts the user on the terminal to approve/deny risky actions.
pub struct CliApprover;

#[async_trait]
impl Approver for CliApprover {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        eprintln!();
        eprintln!(
            "{} {} wants to run a {:?} action:",
            style("⚠ approval needed").yellow().bold(),
            request.tool,
            request.risk
        );
        if let Some(cmd) = &request.command {
            eprintln!("    {}", style(cmd).bold());
        }
        if !request.paths.is_empty() {
            for p in &request.paths {
                eprintln!("    path: {}", p.display());
            }
        }
        eprint!(
            "  Approve? [{}]es once / [{}]ession / [{}]lways (project rule) / [{}]o (default): ",
            style("y").green(),
            style("s").green(),
            style("w").green(),
            style("N").red()
        );

        let line = tokio::task::spawn_blocking(|| {
            use std::io::BufRead;
            let mut s = String::new();
            let stdin = std::io::stdin();
            let _ = stdin.lock().read_line(&mut s);
            s
        })
        .await
        .unwrap_or_default();

        match line.trim().to_lowercase().as_str() {
            "y" | "yes" | "once" => ApprovalDecision::ApproveOnce,
            // `a` kept as session for muscle memory; prefer `s` in the prompt.
            "a" | "all" | "s" | "session" => ApprovalDecision::ApproveSession,
            "w" | "always" | "forever" => ApprovalDecision::ApproveAlways,
            _ => ApprovalDecision::Deny,
        }
    }
}
