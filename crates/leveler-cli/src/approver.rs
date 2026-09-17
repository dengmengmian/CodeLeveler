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
        let persists = request.always_persists();
        eprint!("  Approve? {}: ", choices_prompt(persists));

        let line = tokio::task::spawn_blocking(|| {
            use std::io::BufRead;
            let mut s = String::new();
            let stdin = std::io::stdin();
            let _ = stdin.lock().read_line(&mut s);
            s
        })
        .await
        .unwrap_or_default();

        parse_answer(&line, persists)
    }
}

/// The answers offered. "Always" only when the runtime would persist a rule
/// for it — otherwise the prompt would promise an effect that never happens.
fn choices_prompt(always_persists: bool) -> String {
    let always = if always_persists {
        format!("[{}]lways (project rule) / ", style("w").green())
    } else {
        String::new()
    };
    format!(
        "[{}]es once / this [{}]urn / {always}[{}]o (default)",
        style("y").green(),
        style("t").green(),
        style("N").red()
    )
}

fn parse_answer(line: &str, always_persists: bool) -> ApprovalDecision {
    match line.trim().to_lowercase().as_str() {
        "y" | "yes" | "once" => ApprovalDecision::ApproveOnce,
        // Lasts the rest of this turn. `a` and `s` are kept for muscle memory;
        // the prompt offers `t`.
        "t" | "turn" | "a" | "all" | "s" | "session" => ApprovalDecision::ApproveSession,
        "w" | "always" | "forever" if always_persists => ApprovalDecision::ApproveAlways,
        _ => ApprovalDecision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A consent tool cannot become a project rule: the prompt does not offer
    /// "always", and typing `w` anyway is not an approval.
    #[test]
    fn always_is_offered_and_accepted_only_when_a_rule_would_be_written() {
        assert!(choices_prompt(true).contains("lways"));
        assert!(!choices_prompt(false).contains("lways"));
        assert_eq!(parse_answer("w", true), ApprovalDecision::ApproveAlways);
        assert_eq!(parse_answer("always", false), ApprovalDecision::Deny);
        assert_eq!(parse_answer("s", false), ApprovalDecision::ApproveSession);
        assert_eq!(parse_answer("y", false), ApprovalDecision::ApproveOnce);
    }

    /// The runtime keeps that grant for the rest of the turn, not the session.
    #[test]
    fn the_wider_approval_names_the_turn_it_lasts_for() {
        let prompt = console::strip_ansi_codes(&choices_prompt(false)).to_string();
        assert!(prompt.contains("this [t]urn"), "{prompt}");
        assert!(!prompt.contains("ession"), "{prompt}");
        assert_eq!(parse_answer("t", false), ApprovalDecision::ApproveSession);
    }
}
