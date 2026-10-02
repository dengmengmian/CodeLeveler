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
        let details = request_details(request);
        if !details.is_empty() {
            eprintln!("{details}");
        }
        eprint!("  Approve? {}: ", request_choices_prompt(request));

        let line = tokio::task::spawn_blocking(|| {
            use std::io::BufRead;
            let mut s = String::new();
            let stdin = std::io::stdin();
            let _ = stdin.lock().read_line(&mut s);
            s
        })
        .await
        .unwrap_or_default();

        request_answer(&line, request)
    }
}

fn request_details(request: &ApprovalRequest) -> String {
    let mut lines = Vec::new();
    // Escalation approvals can carry their scope, operation and reason in the
    // description with no separate command. Omitting it makes consent blind.
    if !request.description.trim().is_empty() {
        lines.push(format!(
            "    {}",
            leveler_core::sanitize_terminal_output(&request.description)
        ));
    }
    if let Some(cmd) = &request.command {
        lines.push(format!("    {}", style(cmd).bold()));
    }
    if let Some(grant) = &request.grant {
        lines.push(format!("    Project: {}", grant.project_identity));
        for binding in &grant.bindings {
            let capability =
                serde_json::to_string(&binding.capability).expect("typed capability serializes");
            lines.push(format!(
                "    Capability: {}",
                capability.trim_matches('"').replace('_', ".")
            ));
            lines.push(format!(
                "    Resource: {}",
                serde_json::to_string(&binding.resource).expect("typed resource serializes")
            ));
        }
    }
    for path in &request.paths {
        lines.push(format!("    path: {}", path.display()));
    }
    lines.join("\n")
}

fn request_choices_prompt(request: &ApprovalRequest) -> String {
    if request.requires_human_consent() {
        return "[y]es once / [N]o (default)".into();
    }
    if request.project_available() {
        return "[y]es once / this [s]ession (resource grant) / this [p]roject (resource grant) / [N]o (default)".into();
    }
    choices_prompt(request.always_persists())
}

fn request_answer(line: &str, request: &ApprovalRequest) -> ApprovalDecision {
    let answer = line.trim().to_ascii_lowercase();
    let decision = if request.grant.is_some() {
        match answer.as_str() {
            "y" | "yes" | "once" => ApprovalDecision::ApproveOnce,
            "s" | "session" => ApprovalDecision::ApproveSession,
            "p" | "project" => ApprovalDecision::ApproveProject,
            _ => ApprovalDecision::Deny,
        }
    } else {
        parse_answer(line, request.always_persists())
    };
    if request.decisions().contains(&decision) {
        decision
    } else {
        ApprovalDecision::Deny
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

    #[test]
    fn bound_resource_offers_only_resource_scopes_and_renders_identity() {
        let mut request = ApprovalRequest {
            id: leveler_core::ApprovalId::new("grant-test"),
            turn_id: None,
            call_id: "call".into(),
            agent_id: None,
            action_fingerprint: "fp".into(),
            tool: "run_command".into(),
            risk: leveler_execution::RiskLevel::Destructive,
            description: "push origin".into(),
            command: Some("git push origin main".into()),
            paths: Vec::new(),
            grant: Some(leveler_core::GrantRequest {
                project_identity: "project-a".into(),
                bindings: vec![leveler_core::GrantBinding {
                    capability: leveler_core::Capability::RemoteMutate,
                    resource: leveler_core::ResourceIdentity::ConfiguredRemote {
                        repository: "repo-a".into(),
                        remote_name: "origin".into(),
                        canonical_url: "https://example.test/a.git".into(),
                        transport: "https".into(),
                    },
                }],
            }),
        };
        let details = request_details(&request);
        assert!(
            details.contains("remote.mutate")
                && details.contains("repo-a")
                && details.contains("https://example.test/a.git"),
            "{details}"
        );
        assert!(request_choices_prompt(&request).contains("[p]roject"));
        assert_eq!(
            request_answer("p", &request),
            ApprovalDecision::ApproveProject
        );
        assert_eq!(request_answer("w", &request), ApprovalDecision::Deny);
        assert_eq!(request_answer("t", &request), ApprovalDecision::Deny);
        request.tool = "save_agent".into();
        assert!(!request_choices_prompt(&request).contains("ession"));
        assert_eq!(request_answer("s", &request), ApprovalDecision::Deny);
        assert_eq!(request_answer("p", &request), ApprovalDecision::Deny);
        assert_eq!(request_answer("y", &request), ApprovalDecision::ApproveOnce);
    }

    #[test]
    fn privileged_approval_displays_scope_command_and_reason_from_description() {
        let mut request = ApprovalRequest {
            grant: None,
            id: leveler_core::ApprovalId::new("test"),
            turn_id: None,
            call_id: "call".into(),
            agent_id: None,
            action_fingerprint: "fingerprint".into(),
            tool: "shell_command".into(),
            risk: leveler_execution::RiskLevel::Privileged,
            description: "Unrestricted filesystem · git add taskbox/ · reason: local commit".into(),
            command: None,
            paths: Vec::new(),
        };
        let details = request_details(&request);
        assert!(details.contains(&request.description), "{details}");
        request.command = Some("git add taskbox/".into());
        request.paths.push("taskbox/".into());
        let details = request_details(&request);
        assert!(details.contains(&request.description));
        assert!(details.contains("path: taskbox/"));
        assert!(details.contains("git add taskbox/"));
        assert!(
            console::strip_ansi_codes(&details)
                .lines()
                .any(|line| line.trim() == "git add taskbox/")
        );
        request.command = None;
        request.paths.clear();
        request.description = "\u{1b}[2Jgit add taskbox/\rreason: local commit".into();
        let details = request_details(&request);
        assert!(!details.contains('\u{1b}') && !details.contains('\r'));
        assert!(details.contains("git add taskbox/") && details.contains("reason: local commit"));
    }

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
