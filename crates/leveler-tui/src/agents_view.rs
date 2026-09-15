//! `/agents` — the agent registry as transcript notes.
//!
//! A listing names every agent with its class, source and status, and says
//! what is wrong with an unusable one: "failed to load agent" alone would leave
//! the user guessing which file and why.

use leveler_client_protocol::{
    UiAgentCapability, UiAgentDetail, UiAgentEntry, UiAgentProblem, UiAgentSource, UiAgentStatus,
};

fn source(source: UiAgentSource) -> &'static str {
    match source {
        UiAgentSource::Project => "project",
        UiAgentSource::User => "user",
        UiAgentSource::Builtin => "builtin",
    }
}

fn capability(capability: Option<UiAgentCapability>) -> &'static str {
    match capability {
        Some(UiAgentCapability::ReadOnly) => "read_only",
        Some(UiAgentCapability::Writer) => "writer",
        Some(UiAgentCapability::ScopedWriter) => "scoped_writer",
        None => "?",
    }
}

fn status_suffix(entry: &UiAgentEntry) -> String {
    match entry.status {
        UiAgentStatus::Available if entry.harness_only => " · harness only".to_string(),
        UiAgentStatus::Available => String::new(),
        UiAgentStatus::Unavailable => format!(
            " · unavailable: {}",
            entry.reason.as_deref().unwrap_or("unknown")
        ),
        UiAgentStatus::Invalid => format!(
            " · INVALID: {}",
            entry.reason.as_deref().unwrap_or("unknown")
        ),
    }
}

pub fn listing_note(agents: &[UiAgentEntry], problems: &[UiAgentProblem]) -> String {
    let mut lines = vec![format!("Agents ({})", agents.len())];
    for entry in agents {
        let mut line = format!(
            "  {} — {} [{} · {}]{}",
            entry.name,
            entry.description.as_deref().unwrap_or("—"),
            capability(entry.capability),
            source(entry.source),
            status_suffix(entry),
        );
        for shadow in &entry.shadowed {
            line.push_str(&format!(" · shadows {}", source(shadow.source)));
        }
        lines.push(line);
    }
    if !problems.is_empty() {
        lines.push(format!("Not loaded ({})", problems.len()));
        for problem in problems {
            lines.push(format!("  {}: {}", problem.location, problem.error));
        }
    }
    lines.push("hint: /agents <name> shows one definition".to_string());
    lines.join("\n")
}

pub fn detail_note(detail: &UiAgentDetail) -> String {
    let entry = &detail.entry;
    let mut lines = vec![format!(
        "Agent {} [{} · {}]{}",
        entry.name,
        capability(entry.capability),
        source(entry.source),
        status_suffix(entry)
    )];
    if let Some(description) = &entry.description {
        lines.push(format!("  {description}"));
    }
    if let Some(location) = &entry.location {
        lines.push(format!("  location: {location}"));
    }
    if !entry.write_roots.is_empty() {
        lines.push(format!(
            "  writes only under: {}",
            entry.write_roots.join(", ")
        ));
    }
    lines.push(format!(
        "  tools: {}",
        entry
            .tools
            .as_ref()
            .map(|t| t.join(", "))
            .unwrap_or_else(|| "all of its capability".to_string())
    ));
    lines.push(format!(
        "  model: {}",
        entry.model.as_deref().unwrap_or("the parent's model")
    ));
    if let Some(effort) = &entry.reasoning_effort {
        lines.push(format!("  reasoning effort: {effort}"));
    }
    if !entry.skills.is_empty() {
        lines.push(format!("  skills: {}", entry.skills.join(", ")));
    }
    if let Some(rounds) = entry.max_rounds {
        lines.push(format!("  max rounds: {rounds}"));
    }
    if let Some(secs) = entry.max_duration_secs {
        lines.push(format!("  max duration: {secs}s"));
    }
    if let Some(fingerprint) = &entry.fingerprint {
        lines.push(format!("  fingerprint: {fingerprint}"));
    }
    for shadow in &entry.shadowed {
        lines.push(format!(
            "  shadows a {} definition{}",
            source(shadow.source),
            shadow
                .location
                .as_deref()
                .map(|l| format!(" at {l}"))
                .unwrap_or_default()
        ));
    }
    if let Some(instructions) = &detail.instructions {
        lines.push("  instructions:".to_string());
        for line in instructions.lines() {
            lines.push(format!("    {line}"));
        }
    } else if entry.structural {
        lines.push("  a built-in role: its behaviour is the runtime's contract".to_string());
    }
    lines.join("\n")
}
