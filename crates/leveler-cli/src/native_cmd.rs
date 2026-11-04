//! `leveler native`: deterministic native actions with no model and no daemon.
//!
//! The CLI is the explicit human path. Admission here is the command the user
//! typed; there is no approval gate to reuse because no model chose the
//! action. The process still runs through [`CommandRunner`], so env scrubbing,
//! cancellation and process-tree termination are the same as every other
//! execution.

use std::process::ExitCode;

use tokio_util::sync::CancellationToken;

use leveler_execution::{CommandRunner, ProcessRequest};
use leveler_native::{
    ActionId, ActionStatus, CapabilityReport, CapabilityState, DiscoveryReport, ExecutionEvidence,
    NativeActionRequest, NativeActions, NativeOutcome, PermissionState,
};

use crate::cli::NativeCommand;

const EXIT_OK: u8 = 0;
const EXIT_REQUIRES_ACTION: u8 = 2;
const EXIT_FAILED: u8 = 3;
const EXIT_UNKNOWN: u8 = 4;

pub async fn cmd_native(command: NativeCommand) -> ExitCode {
    let runner = CommandRunner::new();
    let native = NativeActions::new();
    match command {
        NativeCommand::Capabilities { json } => {
            let report = native.discover(&runner).await;
            if json {
                print_json(&report);
            } else {
                print_report(&report);
            }
            ExitCode::SUCCESS
        }
        NativeCommand::Action {
            action,
            to,
            subject,
            body,
            body_file,
            json,
        } => {
            run_action(
                &runner, &native, &action, to, subject, body, body_file, json,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_action(
    runner: &CommandRunner,
    native: &NativeActions,
    action: &str,
    to: String,
    subject: String,
    body: Option<String>,
    body_file: Option<std::path::PathBuf>,
    json: bool,
) -> ExitCode {
    let action = match ActionId::parse(action) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(EXIT_REQUIRES_ACTION);
        }
    };
    let body = match body_file {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(body) => Some(body),
            Err(error) => {
                eprintln!("could not read --body-file {}: {error}", path.display());
                return ExitCode::from(EXIT_FAILED);
            }
        },
        None => body,
    };
    if action != ActionId::MailSentCheck && body.is_none() {
        eprintln!(
            "{} needs --body (an empty body is `--body \"\"`) or --body-file",
            action.as_str()
        );
        return ExitCode::from(EXIT_REQUIRES_ACTION);
    }
    if action == ActionId::MailSentCheck && body.is_some() {
        eprintln!("{} does not take a body", action.as_str());
        return ExitCode::from(EXIT_REQUIRES_ACTION);
    }

    let input = match action {
        ActionId::MailSentCheck => serde_json::json!({ "to": to, "subject": subject }),
        _ => serde_json::json!({
            "to": to,
            "subject": subject,
            "body": body.unwrap_or_default(),
        }),
    };
    let request = NativeActionRequest { action, input };
    let plan = match native.plan(&request) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(EXIT_REQUIRES_ACTION);
        }
    };

    let report = native.discover(runner).await;
    let provider = plan.provider().as_str();
    let Some(capability) = report
        .capabilities
        .iter()
        .find(|capability| capability.provider == provider)
    else {
        eprintln!(
            "{} is not available: this host reported no provider {provider}",
            action.as_str()
        );
        print_report(&report);
        return ExitCode::from(EXIT_REQUIRES_ACTION);
    };
    if capability.state == CapabilityState::Unavailable
        || capability.state == CapabilityState::Unsupported
    {
        eprintln!(
            "{} is not available: {}",
            action.as_str(),
            capability
                .reason
                .as_deref()
                .unwrap_or("the provider is unavailable")
        );
        return ExitCode::from(EXIT_REQUIRES_ACTION);
    }
    if !json {
        eprintln!(
            "{} via {} — discovery says {} (permission: {}, configured: {:?})",
            action.as_str(),
            capability.provider_name,
            state_name(capability.state),
            capability.permission.as_str(),
            capability.configured
        );
    }
    if capability.permission == PermissionState::Unknown {
        eprintln!(
            "{} was not started: {}",
            action.as_str(),
            capability
                .reason
                .as_deref()
                .unwrap_or("automation permission could not be determined")
        );
        return ExitCode::from(EXIT_REQUIRES_ACTION);
    }

    let mut process = ProcessRequest::new(
        plan.program(),
        plan.args().to_vec(),
        plan.cwd().to_path_buf(),
    );
    process.timeout = plan.timeout();
    process.max_output_bytes = 256 * 1024;
    let started = std::time::Instant::now();
    let output = runner.run(process, CancellationToken::new()).await;
    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let evidence = match output {
        Ok(output) => ExecutionEvidence {
            exit_code: output.exit_code,
            timed_out: output.timed_out,
            stdout: output.stdout,
            stderr: output.stderr,
            duration_ms,
        },
        Err(error) => ExecutionEvidence {
            exit_code: None,
            timed_out: false,
            stdout: String::new(),
            stderr: error.to_string(),
            duration_ms,
        },
    };
    let outcome = native.interpret(action, plan.provider(), &evidence);
    if json {
        print_json(&outcome);
    } else {
        print_outcome(&outcome);
    }
    ExitCode::from(match outcome.status {
        ActionStatus::Completed | ActionStatus::Submitted => EXIT_OK,
        ActionStatus::RequiresPermission | ActionStatus::RequiresConfiguration => {
            EXIT_REQUIRES_ACTION
        }
        ActionStatus::Failed | ActionStatus::Unsupported => EXIT_FAILED,
        ActionStatus::Unknown => EXIT_UNKNOWN,
    })
}

fn print_json<T: serde::Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(json) => println!("{json}"),
        Err(error) => eprintln!("could not render JSON: {error}"),
    }
}

fn print_report(report: &DiscoveryReport) {
    println!(
        "{} native actions — backend {}",
        report.platform,
        if report.supported {
            "supported"
        } else {
            "unsupported"
        }
    );
    if let Some(handler) = report.default_mail_handler.as_deref() {
        println!("default mail handler: {handler}");
    }
    for tool in &report.system_tools {
        println!(
            "tool {}: {}",
            tool.id,
            if tool.installed {
                tool.path.as_deref().unwrap_or("installed")
            } else {
                "missing"
            }
        );
    }
    for capability in &report.capabilities {
        print_capability(capability);
    }
    for note in &report.notes {
        println!("note: {note}");
    }
}

fn print_capability(capability: &CapabilityReport) {
    println!(
        "\n{} via {} ({}): {}",
        capability.id,
        capability.provider_name,
        capability.provider,
        state_name(capability.state)
    );
    println!(
        "  installed={} automation_permission={} configured={:?} interactive_session={}",
        capability.installed,
        capability.permission.as_str(),
        capability.configured,
        capability.requires_interactive_session
    );
    if let Some(reason) = capability.reason.as_deref() {
        println!("  reason: {reason}");
    }
    for action in &capability.actions {
        println!(
            "  action {:<20} supported={} side_effects={} gui_only={} unattended={} sent_state={} delivery={}",
            action.id,
            action.supported,
            action.external_side_effects,
            action.gui_only,
            unattended_name(action.unattended),
            action.sent_state_confirmable,
            action.delivery_confirmable,
        );
        if let Some(reason) = action.reason.as_deref() {
            println!("      {reason}");
        }
    }
    for evidence in &capability.evidence {
        println!(
            "  evidence {}={} ({})",
            evidence.fact, evidence.value, evidence.source
        );
    }
}

fn print_outcome(outcome: &NativeOutcome) {
    println!(
        "{} via {}: {}",
        outcome.action.as_str(),
        outcome.provider.as_str(),
        outcome.status.as_str()
    );
    println!(
        "  execution_id={} exit_code={:?} timed_out={}",
        outcome.execution_id, outcome.exit_code, outcome.timed_out
    );
    println!(
        "  sent_state={:?} delivery={:?}",
        outcome.sent_state, outcome.delivery
    );
    println!("  detail: {}", outcome.detail);
    for evidence in &outcome.evidence {
        println!("  evidence: {evidence}");
    }
    if outcome.status == ActionStatus::Unknown {
        println!("  The effect is unknown. Do not retry automatically; verify first.");
    }
}

fn state_name(state: CapabilityState) -> &'static str {
    match state {
        CapabilityState::Installed => "installed",
        CapabilityState::Available => "available",
        CapabilityState::RequiresPermission => "requires_permission",
        CapabilityState::RequiresConfiguration => "requires_configuration",
        CapabilityState::Unavailable => "unavailable",
        CapabilityState::Unsupported => "unsupported",
        CapabilityState::Unknown => "unknown",
    }
}

fn unattended_name(support: leveler_native::UnattendedSupport) -> &'static str {
    match support {
        leveler_native::UnattendedSupport::Yes => "yes",
        leveler_native::UnattendedSupport::Conditional => "conditional",
        leveler_native::UnattendedSupport::No => "no",
        leveler_native::UnattendedSupport::Unknown => "unknown",
    }
}
