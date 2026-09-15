//! Built-in agents, projected into the same resolved view as file-based ones.
//!
//! Two kinds: the runtime's structural roles, whose contract is the compiled
//! [`ChildProfile`], and shipped personas, which are ordinary `agent.yaml` +
//! `instructions.md` pairs compiled into the binary and validated by the same
//! parser as a user's.

use super::{
    AgentCapability, AgentDefinition, AgentKind, AgentSource, fingerprint, parse_manifest,
    resolve_manifest,
};
use crate::child_profile::{AgentRole, ChildProfile, ToolAccess};

/// Shipped personas as `(name, agent.yaml, instructions.md)`.
const PERSONAS: &[(&str, &str, &str)] = &[
    (
        "code-architect",
        include_str!("../../prompts/agents/code-architect/agent.yaml"),
        include_str!("../../prompts/agents/code-architect/instructions.md"),
    ),
    (
        "code-explorer",
        include_str!("../../prompts/agents/code-explorer/agent.yaml"),
        include_str!("../../prompts/agents/code-explorer/instructions.md"),
    ),
    (
        "code-reviewer",
        include_str!("../../prompts/agents/code-reviewer/agent.yaml"),
        include_str!("../../prompts/agents/code-reviewer/instructions.md"),
    ),
];

pub(super) fn definitions() -> Vec<AgentDefinition> {
    let mut out: Vec<AgentDefinition> = [
        (
            AgentRole::Default,
            "General child: starts read-capable and claims its own write scope",
        ),
        (
            AgentRole::Explorer,
            "Read-only repository investigation; holds no mutating tool",
        ),
        (
            AgentRole::Worker,
            "Scoped implementation over files it is given exclusively at spawn",
        ),
        (
            AgentRole::Reviewer,
            "Independent read-only review, launched by the harness only",
        ),
    ]
    .into_iter()
    .map(|(role, description)| structural(role, description))
    .collect();
    for (name, manifest, instructions) in PERSONAS {
        // A shipped persona that fails its own schema is a build defect the
        // unit tests catch; at runtime it is simply absent rather than a panic.
        if let Ok(def) = parse_manifest(manifest)
            .and_then(|m| resolve_manifest(&m, instructions, name, AgentSource::Builtin, None))
        {
            out.push(def);
        }
    }
    out
}

fn structural(role: AgentRole, description: &str) -> AgentDefinition {
    let profile = ChildProfile::resolve(role);
    let capability = match profile.tool_policy.access {
        ToolAccess::ReadSearch | ToolAccess::ReadTest => AgentCapability::ReadOnly,
        ToolAccess::WriteScoped => AgentCapability::ScopedWriter,
        ToolAccess::Inherit => AgentCapability::Writer,
    };
    let mut def = AgentDefinition {
        name: role.label().to_string(),
        description: description.to_string(),
        source: AgentSource::Builtin,
        location: None,
        capability,
        model: None,
        reasoning_effort: None,
        skills: Vec::new(),
        tools: None,
        write_roots: Vec::new(),
        max_rounds: profile.max_rounds(),
        max_duration_secs: None,
        instructions: String::new(),
        fingerprint: String::new(),
        kind: AgentKind::Structural(role),
    };
    def.fingerprint = fingerprint(&def);
    def
}

#[cfg(test)]
pub(super) fn persona_names() -> Vec<&'static str> {
    PERSONAS.iter().map(|(n, _, _)| *n).collect()
}

#[cfg(test)]
pub(super) fn persona_sources() -> &'static [(&'static str, &'static str, &'static str)] {
    PERSONAS
}

/// Whether `name` is shipped with the binary (a structural role or a persona).
pub(super) fn is_builtin_name(name: &str) -> bool {
    super::is_reserved_agent_name(name) || PERSONAS.iter().any(|(n, _, _)| *n == name)
}
