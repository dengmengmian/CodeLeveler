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

pub fn listing_note(
    agents: &[UiAgentEntry],
    problems: &[UiAgentProblem],
    t: &crate::i18n::UiText,
) -> String {
    let mut lines = vec![format!("{} ({})", t.agents_title, agents.len())];
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
        lines.push(format!("{} ({})", t.agents_not_loaded, problems.len()));
        for problem in problems {
            lines.push(format!("  {}: {}", problem.location, problem.error));
        }
    }
    lines.push(t.agents_hint.to_string());
    lines.join("\n")
}

pub fn detail_note(detail: &UiAgentDetail, t: &crate::i18n::UiText) -> String {
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
        lines.push(format!("  {}: {location}", t.agents_location));
    }
    if !entry.write_roots.is_empty() {
        lines.push(format!(
            "  {}: {}",
            t.agents_write_roots,
            entry.write_roots.join(", ")
        ));
    }
    lines.push(format!(
        "  {}: {}",
        t.agents_tools,
        entry
            .tools
            .as_ref()
            .map(|t| t.join(", "))
            .unwrap_or_else(|| t.agents_tools_all.to_string())
    ));
    lines.push(format!(
        "  {}: {}",
        t.agents_model,
        entry.model.as_deref().unwrap_or(t.agents_model_parent)
    ));
    if let Some(effort) = &entry.reasoning_effort {
        lines.push(format!("  {}: {effort}", t.agents_effort));
    }
    if !entry.skills.is_empty() {
        lines.push(format!(
            "  {}: {}",
            t.agents_skills,
            entry.skills.join(", ")
        ));
    }
    if let Some(rounds) = entry.max_rounds {
        lines.push(format!("  {}: {rounds}", t.agents_max_rounds));
    }
    if let Some(secs) = entry.max_duration_secs {
        lines.push(format!("  {}: {secs}s", t.agents_max_duration));
    }
    if let Some(fingerprint) = &entry.fingerprint {
        lines.push(format!("  {}: {fingerprint}", t.agents_fingerprint));
    }
    for shadow in &entry.shadowed {
        lines.push(format!(
            "  {} {}{}",
            t.agents_shadows,
            source(shadow.source),
            shadow
                .location
                .as_deref()
                .map(|l| format!(" at {l}"))
                .unwrap_or_default()
        ));
    }
    if let Some(instructions) = &detail.instructions {
        lines.push(format!("  {}:", t.agents_instructions));
        for line in instructions.lines() {
            lines.push(format!("    {line}"));
        }
    } else if entry.structural {
        lines.push(format!("  {}", t.agents_structural));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;

    fn entry() -> UiAgentEntry {
        UiAgentEntry {
            name: "code-reviewer".into(),
            description: Some("审查改动".into()),
            capability: Some(UiAgentCapability::ReadOnly),
            source: UiAgentSource::Builtin,
            status: UiAgentStatus::Available,
            harness_only: true,
            reason: None,
            shadowed: Vec::new(),
            structural: true,
            location: Some("/repo/.leveler/agents/x".into()),
            write_roots: Vec::new(),
            tools: None,
            model: None,
            reasoning_effort: None,
            skills: Vec::new(),
            max_rounds: None,
            max_duration_secs: None,
            fingerprint: None,
        }
    }

    /// §12 of the IA doc: chrome follows the locale, identifiers never do.
    /// This screen was the one place a Chinese session still read as English —
    /// every label on it ("hint", "location", "tools", "Not loaded") is Class A.
    #[test]
    fn the_listing_chrome_follows_the_locale_and_the_identifiers_do_not() {
        let agents = [entry()];
        let zh = listing_note(&agents, &[], Locale::Zh.text());
        let en = listing_note(&agents, &[], Locale::En.text());
        assert!(!zh.contains("hint:"), "zh kept English chrome:\n{zh}");
        assert!(en.contains("hint:"), "en lost its chrome:\n{en}");
        for id in ["code-reviewer", "read_only", "builtin"] {
            assert!(zh.contains(id), "zh rewrote {id}:\n{zh}");
            assert!(en.contains(id), "en rewrote {id}:\n{en}");
        }
    }

    #[test]
    fn the_detail_chrome_follows_the_locale_and_the_paths_do_not() {
        let detail = UiAgentDetail {
            entry: entry(),
            instructions: None,
        };
        let zh = detail_note(&detail, Locale::Zh.text());
        assert!(!zh.contains("location:"), "zh kept English chrome:\n{zh}");
        assert!(
            zh.contains("/repo/.leveler/agents/x"),
            "the path is data:\n{zh}"
        );
    }
}
