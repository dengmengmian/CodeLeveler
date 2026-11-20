//! Read-only informational subcommands: doctor, config show, models
//! list/show, and model probe.

use tokio_util::sync::CancellationToken;

use leveler_app::Application;
use leveler_model::ModelRuntime;
use leveler_project::Layout;
use leveler_provider::probe_basic;

use crate::common::parse_model_ref;
use crate::output::{Line, print_finding};
use leveler_client_protocol::{
    DiagnosticCategory, DiagnosticFinding, DiagnosticReport, DiagnosticSeverity, DiagnosticStatus,
    finding_id,
};

pub(crate) fn cmd_doctor(layout: Layout, json: bool) -> anyhow::Result<std::process::ExitCode> {
    // Load config leniently so doctor can report problems instead of aborting.
    let config = Application::load_config(&layout).unwrap_or_default();

    let mut findings = leveler_app::doctor::run(&config);
    if let Some(root) = layout.primary_workspace() {
        let home = leveler_core::LevelerHome::resolve(leveler_core::environment());
        let trust = leveler_definition_trust::DefinitionTrustView::load(home.root(), Some(root));
        findings.push(leveler_app::doctor::agents_check(
            &leveler_agent::agent_registry::AgentRoots::for_project(root),
            &trust,
        ));
        findings.push(leveler_app::doctor::definitions_check(
            &leveler_definition_trust::DiscoveryRoots::resolve(
                leveler_core::environment(),
                Some(root.to_path_buf()),
            ),
            &trust,
        ));
    }
    // Terminal observations are real local facts; the CLI can collect them
    // without a runtime. The live runtime checks exist only in the TUI's
    // `/doctor`, and this finding says so instead of guessing.
    let terminal = leveler_tui::terminal_diagnostics::TerminalDiagnostics::detect();
    findings.push(DiagnosticFinding {
        id: finding_id::TERMINAL_CAPABILITY.into(),
        subject: None,
        category: DiagnosticCategory::Terminal,
        severity: DiagnosticSeverity::Info,
        status: DiagnosticStatus::Pass,
        evidence: terminal.describe().lines().map(str::to_string).collect(),
        action: None,
        checked_at: leveler_core::now().to_rfc3339(),
    });
    findings.push(DiagnosticFinding {
        id: finding_id::RUNTIME_CONNECTION.into(),
        subject: None,
        category: DiagnosticCategory::Runtime,
        severity: DiagnosticSeverity::Info,
        status: DiagnosticStatus::NotChecked,
        evidence: vec![
            "no live runtime was consulted; open the TUI and run /doctor for runtime checks".into(),
        ],
        action: None,
        checked_at: leveler_core::now().to_rfc3339(),
    });

    if json {
        let report = DiagnosticReport {
            generated_at: leveler_core::now().to_rfc3339(),
            findings: findings.clone(),
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(exit_for(&findings));
    }

    println!("{}", Line::heading("leveler doctor"));
    println!(
        "  repo:    {}",
        layout
            .primary_workspace()
            .map(|p| leveler_core::path::display_path(p).into_owned())
            .unwrap_or_else(|| "No Workspace".to_string())
    );
    println!(
        "  config:  {}",
        leveler_core::path::display_path(&layout.config_dir)
    );
    println!();

    for finding in &findings {
        // The text view prints the full terminal observations once, in their
        // own section; only the JSON report needs the terminal finding.
        if finding.id == finding_id::TERMINAL_CAPABILITY {
            continue;
        }
        print_finding(finding);
    }
    println!();
    println!("{}", Line::heading("Terminal observations"));
    println!("{}", terminal.describe());

    // Crashes are otherwise invisible: the TUI tears the screen down and the
    // message scrolls away. Point at the records rather than reprinting them.
    let crashes = crate::crash::recent(3);
    if !crashes.is_empty() {
        println!();
        println!(
            "{}",
            Line::warn(&format!("{} recent crash record(s):", crashes.len()))
        );
        for path in &crashes {
            println!("    {}", leveler_core::path::display_path(path));
        }
        println!("  Local only — nothing was sent anywhere.");
    }
    println!();
    if leveler_app::doctor::has_failure(&findings) {
        println!("{}", Line::warn("Some checks failed. See details above."));
    } else {
        println!("{}", Line::ok("No failed checks."));
    }
    Ok(exit_for(&findings))
}

fn exit_for(findings: &[DiagnosticFinding]) -> std::process::ExitCode {
    if leveler_app::doctor::has_failure(findings) {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

pub(crate) fn cmd_config_show(layout: Layout) -> anyhow::Result<std::process::ExitCode> {
    let config = Application::load_config(&layout)?;
    println!("{}", Line::heading("Providers"));
    for p in &config.providers {
        println!(
            "  {} [{:?}]  {}  (key: ${})",
            p.id, p.protocol, p.base_url, p.api_key_env
        );
    }
    println!("\n{}", Line::heading("Models"));
    for m in &config.models {
        println!(
            "  {}/{}  -> model_id={}",
            m.profile.provider, m.profile.id, m.profile.model_id
        );
    }
    println!("\n{}", Line::heading("VCS"));
    println!(
        "  co-author trailer: {}",
        if config.vcs_co_author {
            "enabled"
        } else {
            "disabled"
        }
    );
    Ok(std::process::ExitCode::SUCCESS)
}

/// Print a semantic-token preview. No live TUI session.
pub(crate) fn cmd_theme_preview(id: Option<String>) -> anyhow::Result<std::process::ExitCode> {
    use std::io::IsTerminal;

    use leveler_tui::{Theme, ThemeId, preview_theme};

    let color = std::io::stdout().is_terminal();
    let ids = match id.as_deref() {
        None => vec![ThemeId::Dark, ThemeId::Light, ThemeId::HighContrast],
        Some(raw) => {
            let Some(parsed) = ThemeId::parse(raw) else {
                anyhow::bail!(
                    "unknown theme `{raw}`; expected auto | dark | light | high-contrast"
                );
            };
            vec![parsed]
        }
    };
    for (i, theme_id) in ids.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print!("{}", preview_theme(&Theme::named(*theme_id), color));
    }
    Ok(std::process::ExitCode::SUCCESS)
}

pub(crate) fn cmd_models_list(layout: Layout) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let mut refs = app.model_refs();
    refs.sort_by_key(|r| r.to_string());
    if refs.is_empty() {
        println!("{}", Line::warn("No models configured."));
        println!(
            "  Add providers/models in ~/.leveler/config.toml \
             (or $LEVELER_HOME/config.toml), then re-run `leveler doctor`."
        );
    } else {
        println!("{}", Line::heading("Models"));
        for r in refs {
            println!("  {r}");
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

pub(crate) async fn cmd_models_show(
    layout: Layout,
    model: &str,
) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let model_ref = parse_model_ref(model)?;
    let profile = app.registry().profile(&model_ref).await?;
    println!("{}", Line::heading(&format!("{model_ref}")));
    println!("  provider:  {}", profile.provider);
    println!("  model_id:  {}", profile.model_id);
    println!("  protocol:  {:?}", profile.protocol);
    println!("  streaming: {}", profile.capabilities.streaming);
    println!("  tools:     {}", profile.capabilities.tool_calling);
    println!(
        "  context:   {} (reliable {})",
        profile.limits.context_window, profile.limits.reliable_context
    );
    println!();
    println!("{}", Line::heading("Reasoning"));
    if !profile.capabilities.reasoning {
        println!("  capable:    false");
    } else {
        let resolved = leveler_model::resolve_reasoning_effort(None, &profile.reasoning);
        let supported = if profile.reasoning.supported_efforts.is_empty() {
            "(none)".to_string()
        } else {
            profile
                .reasoning
                .supported_efforts
                .iter()
                .map(|e| e.as_wire())
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!("  capable:    true");
        println!("  style:      {:?}", profile.reasoning.style);
        println!("  supported:  {supported}");
        println!(
            "  default:    {}",
            profile
                .reasoning
                .default_effort
                .map(|e| e.as_wire().to_string())
                .unwrap_or_else(|| "n/a".into())
        );
        println!("  requested:  (none)");
        println!(
            "  effective:  {}",
            resolved
                .effective
                .map(|e| e.as_wire().to_string())
                .unwrap_or_else(|| "n/a".into())
        );
    }
    Ok(std::process::ExitCode::SUCCESS)
}

pub(crate) async fn cmd_model_probe(
    layout: Layout,
    model: &str,
) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let model_ref = parse_model_ref(model)?;

    println!("{}", Line::heading(&format!("Probing {model_ref}")));
    let cancellation = CancellationToken::new();
    let report = probe_basic(app.registry().as_ref(), &model_ref, cancellation).await;

    print_bool("text response", report.text_ok);
    print_bool("streaming", report.stream_ok);
    if !report.streamed_text.trim().is_empty() {
        println!(
            "  streamed text: {:?}",
            truncate(report.streamed_text.trim(), 80)
        );
    }
    if !report.streamed_reasoning.trim().is_empty() {
        println!(
            "  reasoning:     {:?}",
            truncate(report.streamed_reasoning.trim(), 80)
        );
    }
    if let Some(reason) = report.finish_reason {
        println!("  finish reason: {reason:?}");
    }
    if report.usage.total() > 0 {
        println!(
            "  usage: {} in / {} out",
            report.usage.input_tokens, report.usage.output_tokens
        );
        // The provider's reasoning breakdown, when it reported one. Showing it
        // here is what makes "the wire said X" checkable without a database.
        if let Some(reasoning) = report.usage.reasoning_tokens {
            let visible = report
                .usage
                .visible_output_tokens()
                .map(|visible| visible.to_string())
                .unwrap_or_else(|| "n/a".to_string());
            println!("         reasoning: {reasoning} (visible out: {visible})");
        }
    }
    if let Some(err) = &report.error {
        println!("{}", Line::fail(&format!("error: {err}")));
    }

    if report.healthy() {
        println!("\n{}", Line::ok("Model is reachable and behaving."));
        Ok(std::process::ExitCode::SUCCESS)
    } else {
        println!("\n{}", Line::warn("Probe did not fully succeed."));
        Ok(std::process::ExitCode::FAILURE)
    }
}

fn print_bool(label: &str, value: bool) {
    if value {
        println!("  {} {label}", console::style("✓").green());
    } else {
        println!("  {} {label}", console::style("✗").red());
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}
