//! `leveler agents list|show` — the resolved agent registry without a UI.

use leveler_app::Application;
use leveler_client_protocol::UiAgentEntry;
use leveler_project::Layout;

use crate::common::resolve_model;
use crate::output::Line;

fn source(entry: &UiAgentEntry) -> String {
    serde_json::to_value(entry.source)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn wire<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "?".to_string())
}

pub(crate) async fn list(layout: Layout, json: bool) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let model = resolve_model(&app, None).ok();
    let (agents, problems) = app.list_agents(model.as_ref()).await;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "agents": agents,
                "problems": problems,
            }))?
        );
        return Ok(std::process::ExitCode::SUCCESS);
    }
    println!("{}", Line::heading("Agents"));
    for entry in &agents {
        let mut line = format!(
            "  {:<24} {:<13} {:<8} {}",
            entry.name,
            entry
                .capability
                .map(|c| wire(&c))
                .unwrap_or_else(|| "?".into()),
            source(entry),
            wire(&entry.status),
        );
        if entry.harness_only {
            line.push_str(" (harness only)");
        }
        for shadow in &entry.shadowed {
            line.push_str(&format!(" · shadows {}", wire(&shadow.source)));
        }
        println!("{line}");
        match (&entry.reason, &entry.description) {
            (Some(reason), _) => println!("      {reason}"),
            (None, Some(description)) => println!("      {description}"),
            _ => {}
        }
    }
    if !problems.is_empty() {
        println!("{}", Line::warn("Not loaded"));
        for problem in &problems {
            println!("  {}: {}", problem.location, problem.error);
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

pub(crate) async fn show(layout: Layout, name: &str) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let model = resolve_model(&app, None).ok();
    let detail = match app.get_agent(name, model.as_ref()).await {
        Ok(detail) => detail,
        Err(error) => {
            eprintln!("{}", Line::fail(&error));
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    let entry = &detail.entry;
    println!("{}", Line::heading(&format!("Agent {}", entry.name)));
    println!("  source:       {}", source(entry));
    if let Some(location) = &entry.location {
        println!("  location:     {location}");
    }
    println!("  status:       {}", wire(&entry.status));
    if let Some(reason) = &entry.reason {
        println!("  reason:       {reason}");
    }
    if let Some(description) = &entry.description {
        println!("  description:  {description}");
    }
    if let Some(capability) = entry.capability {
        println!("  capability:   {}", wire(&capability));
    }
    if !entry.write_roots.is_empty() {
        println!("  write roots:  {}", entry.write_roots.join(", "));
    }
    if let Some(tools) = &entry.tools {
        println!("  tools:        {}", tools.join(", "));
    }
    if let Some(model) = &entry.model {
        println!("  model:        {model}");
    }
    if let Some(effort) = &entry.reasoning_effort {
        println!("  effort:       {effort}");
    }
    if !entry.skills.is_empty() {
        println!("  skills:       {}", entry.skills.join(", "));
    }
    if let Some(fingerprint) = &entry.fingerprint {
        println!("  fingerprint:  {fingerprint}");
    }
    if let Some(instructions) = &detail.instructions {
        println!();
        println!("{instructions}");
    }
    Ok(std::process::ExitCode::SUCCESS)
}
