//! Interactive approval prompt for risky actions.

use async_trait::async_trait;
use console::style;

use leveler_execution::{ApprovalDecision, ApprovalRequest, Approver};

/// Prompts the user on the terminal to approve/deny risky actions.
pub struct CliApprover;

// One terminal can present only one unambiguous consent question at a time.
static INPUT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

type InputReply = tokio::sync::oneshot::Sender<Result<String, String>>;

#[derive(Default)]
struct InputState {
    pending: Option<InputReply>,
    closed: Option<String>,
}

#[derive(Default)]
struct InputReader {
    state: std::sync::Mutex<InputState>,
    ready: std::sync::Condvar,
}

static INPUT_READER: std::sync::OnceLock<Result<std::sync::Arc<InputReader>, String>> =
    std::sync::OnceLock::new();

struct PendingInput<'a>(&'a InputReader);
impl Drop for PendingInput<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().pending.take();
    }
}

impl InputReader {
    fn start() -> Result<std::sync::Arc<Self>, String> {
        let reader = std::sync::Arc::new(Self::default());
        let worker = reader.clone();
        // A process-owned thread keeps blocking stdin outside Tokio's shutdown
        // barrier. It routes each line to the current waiter, not a cancelled
        // request, and never reads ahead while no approval is pending.
        std::thread::Builder::new()
            .name("approval-input".into())
            .spawn(move || {
                use std::io::{BufRead, Read};
                let stdin = std::io::stdin();
                loop {
                    let mut state = worker.state.lock().unwrap();
                    while state.pending.is_none() {
                        state = worker.ready.wait(state).unwrap();
                    }
                    drop(state);
                    let mut bytes = Vec::new();
                    let mut input = stdin.lock();
                    let result = (&mut input).take(4097).read_until(b'\n', &mut bytes);
                    let result = match result {
                        Ok(0) => Err("stdin closed".to_string()),
                        Ok(_) if bytes.len() > 4096 => {
                            // This reader is permanently closed below; no remaining
                            // bytes can become a later approval.
                            Err("approval reply exceeds 4096 bytes".to_string())
                        }
                        Ok(_) => String::from_utf8(bytes).map_err(|e| e.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    drop(input);
                    let mut state = worker.state.lock().unwrap();
                    if let Err(error) = &result {
                        state.closed = Some(error.clone());
                    }
                    if let Some(sender) = state.pending.take() {
                        let _ = sender.send(result);
                    }
                    if state.closed.is_some() {
                        return;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(reader)
    }
}

#[async_trait]
impl Approver for CliApprover {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let _input = INPUT_LOCK.lock().await;
        let reader = match INPUT_READER.get_or_init(InputReader::start) {
            Ok(reader) => reader,
            Err(error) => {
                eprintln!("approval input failed: {error}");
                return ApprovalDecision::Deny;
            }
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        {
            let mut state = reader.state.lock().unwrap();
            if let Some(error) = &state.closed {
                eprintln!("approval input failed: {error}");
                return ApprovalDecision::Deny;
            }
            state.pending = Some(sender);
        }
        // Drop before INPUT_LOCK: a cancelled waiter must be removed before
        // the next prompt can register, without abandoning the stdin reader.
        let _pending = PendingInput(reader);
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

        reader.ready.notify_one();
        let line = match receiver.await {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => {
                eprintln!("approval input failed: {error}");
                return ApprovalDecision::Deny;
            }
            Err(error) => {
                eprintln!("approval input failed: {error}");
                return ApprovalDecision::Deny;
            }
        };

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
    // The resolved capability/resource bindings are authorization match keys:
    // they carry a project hash, a credential incarnation and an object
    // identity, and the runtime already projects what they mean into the
    // description the prompt opens with. Printing them here showed the user a
    // hashed identity and a serialized resource instead of a decision.
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
        return "[y]es once / this [s]ession / this [p]roject / [N]o (default)".into();
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
        format!("[{}]lways for this project / ", style("w").green())
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
    fn approval_replacement_child() {
        if std::env::var_os("LEVELER_TEST_APPROVAL_REPLACEMENT").is_none() {
            return;
        }
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let request = ApprovalRequest {
                id: leveler_core::ApprovalId::new("replacement"),
                turn_id: None,
                call_id: "replacement".into(),
                agent_id: None,
                action_fingerprint: "replacement".into(),
                tool: "run_command".into(),
                risk: leveler_execution::RiskLevel::Destructive,
                description: "replacement input probe".into(),
                command: None,
                paths: Vec::new(),
                grant: None,
            };
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    CliApprover.decide(&request)
                )
                .await
                .is_err()
            );
            let next = CliApprover.decide(&request);
            tokio::pin!(next);
            assert!(futures::poll!(&mut next).is_pending());
            use std::io::Write;
            println!("READY_FOR_REPLY");
            std::io::stdout().flush().unwrap();
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(2), next)
                    .await
                    .expect("one reply must reach the current approval"),
                ApprovalDecision::ApproveOnce
            );
        });
    }

    #[test]
    fn cancelled_reader_does_not_consume_the_next_approval_reply() {
        use std::io::{BufRead, Write};
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "approver::tests::approval_replacement_child",
                "--nocapture",
            ])
            .env("LEVELER_TEST_APPROVAL_REPLACEMENT", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let mut ready = false;
        let mut output = std::io::BufReader::new(output);
        for line in (&mut output).lines() {
            if line.unwrap() == "READY_FOR_REPLY" {
                ready = true;
                break;
            }
        }
        assert!(ready, "child must register its replacement approval");
        let mut input = child.stdin.take().unwrap();
        input.write_all(b"yes\n").unwrap();
        input.flush().unwrap();
        assert!(child.wait().unwrap().success(), "the first reply was lost");
    }

    #[test]
    fn approval_shutdown_child() {
        if std::env::var_os("LEVELER_TEST_APPROVAL_SHUTDOWN").is_none() {
            return;
        }
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let request = ApprovalRequest {
                id: leveler_core::ApprovalId::new("probe"),
                turn_id: None,
                call_id: "probe".into(),
                agent_id: None,
                action_fingerprint: "probe".into(),
                tool: "run_command".into(),
                risk: leveler_execution::RiskLevel::Destructive,
                description: "cancelled input probe".into(),
                command: None,
                paths: Vec::new(),
                grant: None,
            };
            let task = tokio::spawn(async move { CliApprover.decide(&request).await });
            // This is a shutdown timing test: let the OS input read begin.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            task.abort();
            let _ = task.await;
        });
        drop(runtime);
    }

    #[test]
    fn cancelling_approval_does_not_block_runtime_shutdown() {
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "approver::tests::approval_shutdown_child",
                "--nocapture",
            ])
            .env("LEVELER_TEST_APPROVAL_SHUTDOWN", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("cancelled approval kept the runtime alive with stdin still open");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// A resource-bound request offers exactly the resource scopes, and the
    /// prompt prints no part of the binding it matches on: the project identity,
    /// the capability name and the serialized resource are authorization data,
    /// not a decision a person can read.
    #[test]
    fn bound_resource_offers_only_resource_scopes_and_prints_no_identity() {
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
        assert!(details.contains("push origin"), "{details}");
        for leaked in [
            "project-a",
            "remote.mutate",
            "remote_mutate",
            "repo-a",
            "example.test",
            "\"kind\"",
            "{",
        ] {
            assert!(!details.contains(leaked), "{leaked} leaked: {details}");
        }
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
