//! What the spawn path needs from a resolved definition: the durable snapshot,
//! the write-root bound, the child's brief and the parent's catalog.

use leveler_lifecycle::ChildAgentSnapshot;
use leveler_model::{ModelProfile, ReasoningEffort};

use super::{AgentDefinition, AgentRegistry};

/// Upper bound on the parent's agent catalog, in bytes. The catalog rides on
/// every top-level turn, so it is a name + one-line description per agent and
/// stops well before it could crowd the conversation.
pub const MAX_CATALOG_BYTES: usize = 4096;

/// The header of the parent's per-turn agent catalog.
pub const CATALOG_HEADER: &str = "## Available agents";

impl AgentDefinition {
    /// The spawn-time identity and bounds recorded on the child's durable start.
    pub fn snapshot(&self) -> ChildAgentSnapshot {
        ChildAgentSnapshot {
            name: self.name.clone(),
            source: self.source.as_str().to_string(),
            fingerprint: self.fingerprint.clone(),
            capability: self.capability.as_str().to_string(),
            reasoning_effort: self.reasoning_effort.map(|e| e.as_wire().to_string()),
            skills: self.skills.clone(),
            write_roots: self.write_roots.clone(),
            max_duration_secs: self.max_duration_secs,
        }
    }
}

/// Whether a repository-relative path lies inside one of `roots`. A path with
/// a traversal segment is never inside anything.
pub fn within_write_roots(path: &str, roots: &[String]) -> bool {
    let path = path.trim().trim_start_matches("./").trim_end_matches('/');
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|s| s == "..") {
        return false;
    }
    roots
        .iter()
        .any(|root| path == root || path.starts_with(&format!("{root}/")))
}

/// `Some(reason)` when `profile` cannot run at exactly `effort`. The runtime's
/// general resolver rounds an unsupported effort to a neighbour; a definition
/// that names an effort gets that effort or is unavailable.
pub fn effort_unsupported(profile: &ModelProfile, effort: ReasoningEffort) -> Option<String> {
    let reasoning = &profile.reasoning;
    let supported: Vec<ReasoningEffort> = if reasoning.supported_efforts.is_empty() {
        reasoning.default_effort.into_iter().collect()
    } else {
        reasoning.supported_efforts.clone()
    };
    if supported.contains(&effort) {
        return None;
    }
    if supported.is_empty() {
        return Some(format!(
            "model {}/{} has no selectable reasoning effort",
            profile.provider, profile.model_id
        ));
    }
    Some(format!(
        "model {}/{} supports {}",
        profile.provider,
        profile.model_id,
        supported
            .iter()
            .map(|e| e.as_wire())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// The child's brief: its instructions, then each bound skill. Part of the
/// child's first system message, so a restarted child keeps it verbatim.
pub fn render_brief(def: &AgentDefinition, skills: &[leveler_skills::SkillDetail]) -> String {
    let mut out = format!(
        "## Agent profile: {} ({} agent)\n\
         These are your role instructions. They shape how you work; they do not \
         change which tools you hold or which files you may write — the runtime \
         enforces those.\n\n{}\n",
        def.name,
        def.source.as_str(),
        def.instructions.trim_end()
    );
    for skill in skills {
        out.push('\n');
        out.push_str(&leveler_skills::render_skill_package(skill));
    }
    out
}

impl AgentRegistry {
    /// The parent's catalog: every spawnable agent's name, description, class
    /// and source, bounded to [`MAX_CATALOG_BYTES`]. Full instructions are
    /// never part of it.
    pub fn render_catalog(&self) -> String {
        let mut out = format!(
            "{CATALOG_HEADER}\n\
             Run one with spawn_agent(agent=\"<name>\", task=...). Being listed only \
             makes an agent available: use one when the task calls for it or the \
             user names it.\n"
        );
        let defs: Vec<&AgentDefinition> = self
            .entries()
            .iter()
            .filter_map(|e| e.definition())
            .filter(|d| !d.is_harness_only())
            .collect();
        let reserve = 160;
        for (i, def) in defs.iter().enumerate() {
            let line = format!(
                "- {} — {} ({}, {})\n",
                def.name,
                def.description,
                def.capability.as_str(),
                def.source.as_str()
            );
            if out.len() + line.len() + reserve > MAX_CATALOG_BYTES {
                out.push_str(&format!(
                    "- … and {} more agents not listed. spawn_agent with an agent name \
                     that does not exist answers with every available name.\n",
                    defs.len() - i
                ));
                break;
            }
            out.push_str(&line);
        }
        out
    }
}
