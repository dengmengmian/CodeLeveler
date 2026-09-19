//! `leveler skills list|show` — the resolved skill registry without a UI.

use leveler_app::Application;
use leveler_client_protocol::{UiSkillDetail, UiSkillScope, UiSkillSource};
use leveler_project::Layout;

use crate::output::Line;

fn scope(scope: UiSkillScope) -> &'static str {
    match scope {
        UiSkillScope::Project => "project",
        UiSkillScope::User => "user",
        UiSkillScope::Builtin => "builtin",
    }
}

fn source(source: UiSkillSource) -> &'static str {
    match source {
        UiSkillSource::Native => "CodeLeveler",
        UiSkillSource::Codex => "Codex compatible",
        UiSkillSource::AgentSkills => "Agent Skills",
        UiSkillSource::Claude => "Claude compatible",
        UiSkillSource::Builtin => "Built-in",
    }
}

fn wire<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "?".to_string())
}

pub(crate) async fn list(layout: Layout, json: bool) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let (skills, problems) = app.list_skills();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "skills": skills,
                "problems": problems,
            }))?
        );
        return Ok(std::process::ExitCode::SUCCESS);
    }
    println!("{}", Line::heading("Skills"));
    for entry in &skills {
        let mut line = format!(
            "  ${:<22} {:<9} {:<18} {}",
            entry.name,
            scope(entry.scope),
            source(entry.source),
            wire(&entry.status),
        );
        for shadow in &entry.shadowed {
            line.push_str(&format!(
                " · shadows {} {}",
                scope(shadow.scope),
                source(shadow.source)
            ));
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

pub(crate) async fn show(
    layout: Layout,
    name: &str,
    json: bool,
) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let detail: UiSkillDetail = match app.get_skill(name) {
        Ok(detail) => detail,
        Err(error) => {
            eprintln!("{}", Line::fail(&error));
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&detail)?);
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let entry = &detail.entry;
    println!("{}", Line::heading(&format!("Skill {}", entry.name)));
    println!("  scope:        {}", scope(entry.scope));
    println!("  source:       {}", source(entry.source));
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
    for shadow in &entry.shadowed {
        println!(
            "  shadowed:     {} {}{}",
            scope(shadow.scope),
            source(shadow.source),
            shadow
                .location
                .as_ref()
                .map(|l| format!(" ({l})"))
                .unwrap_or_default()
        );
    }
    for (label, files) in [
        ("scripts", &detail.scripts),
        ("references", &detail.references),
        ("files", &detail.other_files),
    ] {
        if !files.is_empty() {
            println!("  {label}: {}", files.join(", "));
        }
    }
    if let Some(body) = &detail.body {
        println!();
        println!("{body}");
    }
    Ok(std::process::ExitCode::SUCCESS)
}
