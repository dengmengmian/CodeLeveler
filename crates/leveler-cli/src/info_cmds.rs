//! Read-only informational subcommands: doctor, config show, models
//! list/show, and model probe.

use tokio_util::sync::CancellationToken;

use leveler_app::Application;
use leveler_model::ModelRuntime;
use leveler_project::Layout;
use leveler_provider::probe_basic;

use crate::common::parse_model_ref;
use crate::output::{Line, print_check};

pub(crate) fn cmd_doctor(layout: Layout) -> anyhow::Result<std::process::ExitCode> {
    // Load config leniently so doctor can report problems instead of aborting.
    let config = Application::load_config(&layout).unwrap_or_default();
    println!("{}", Line::heading("leveler doctor"));
    println!("  repo:    {}", layout.repo_root.display());
    println!("  config:  {}", layout.config_dir.display());
    println!("  memory:  {}", layout.memory_dir().display());
    println!();

    let results =
        leveler_app::doctor::run_with_memory(&config, Some(layout.memory_dir().as_path()));
    for r in &results {
        print_check(r);
    }

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
            println!("    {}", path.display());
        }
        println!("  Local only — nothing was sent anywhere.");
    }
    println!();
    if leveler_app::doctor::has_failure(&results) {
        println!("{}", Line::warn("Some checks failed. See details above."));
        Ok(std::process::ExitCode::FAILURE)
    } else {
        println!("{}", Line::ok("Environment looks healthy."));
        Ok(std::process::ExitCode::SUCCESS)
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
    let profile = app.registry.profile(&model_ref).await?;
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
    let report = probe_basic(app.registry.as_ref(), &model_ref, cancellation).await;

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
