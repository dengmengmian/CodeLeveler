//! Sub-agent delegation policy: roles, limits, nicknames.

/// Max sub-agent nesting depth (a sub-agent may not itself spawn one).
pub(crate) const MAX_SUB_AGENT_DEPTH: u32 = 1;

/// Host-side heuristic: user text that benefits from concurrent `spawn_agent`.
/// Conservative — misses some multi-part tasks rather than over-spawning.
pub fn task_suggests_delegation(task: &str) -> bool {
    let t = task.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "parallel",
        "concurrent",
        "multi-agent",
        "multi agent",
        "spawn_agent",
        "sub-agent",
        "subagent",
        "fan-out",
        "fan out",
        "in parallel",
        "separately investigate",
        "divide and",
        "split the work",
        "分头",
        "并行",
        "多 agent",
        "多agent",
        "子 agent",
        "子agent",
        "同时查",
        "分三路",
        "分两路",
        "分别调查",
        "分别审查",
    ];
    if MARKERS.iter().any(|m| t.contains(m)) {
        return true;
    }
    // English multi-facet review phrasing (architecture + security + …).
    let facets = [
        "architecture",
        "security",
        "stability",
        "tools",
        "performance",
    ];
    let facet_hits = facets.iter().filter(|f| t.contains(*f)).count();
    if facet_hits >= 2 && (t.contains("review") || t.contains("investigate") || t.contains("audit"))
    {
        return true;
    }
    false
}

/// One-shot user injection when [`task_suggests_delegation`] is true.
pub fn multi_agent_steer_hint() -> String {
    "## Multi-agent delegation\n\
     This request looks multi-part or parallel. Prefer emitting several \
     `spawn_agent` calls **in the same assistant turn** so they run concurrently \
     (explorer for investigation, worker + disjoint `files` for edits). Put a \
     complete self-contained `task` in each spawn. Do not spawn for trivial \
     single-step work; synthesize the child reports yourself afterward."
        .to_string()
}
/// A delegated unit must eventually return control to its parent even if a
/// provider or tool keeps making progress without reaching a terminal answer.
pub(crate) const SUB_AGENT_MAX_DURATION: std::time::Duration =
    std::time::Duration::from_secs(15 * 60);
/// Default cap on concurrently-running sub-agents (including within one batch).
pub(crate) const DEFAULT_MAX_CONCURRENT_AGENTS: usize = 4;
/// Default cap on total sub-agents spawned across one top-level run.
pub(crate) const DEFAULT_MAX_TOTAL_AGENTS: usize = 6;

/// Display names assigned to sub-agents in spawn order, so the UI can show
/// "Newton is investigating…" instead of an opaque id. Recycled with an ordinal
/// suffix once exhausted.
pub(crate) const AGENT_NICKNAMES: &[&str] = &[
    "Euclid", "Newton", "Curie", "Turing", "Lovelace", "Hopper", "Darwin", "Tesla", "Bohr",
    "Fermi", "Gauss", "Noether",
];

/// The nickname for the `seq`-th sub-agent (1-based).
pub(crate) fn agent_nickname(seq: usize) -> String {
    let i = seq.saturating_sub(1);
    let base = AGENT_NICKNAMES[i % AGENT_NICKNAMES.len()];
    let cycle = i / AGENT_NICKNAMES.len();
    if cycle == 0 {
        base.to_string()
    } else {
        format!("{base} #{}", cycle + 1)
    }
}

/// A sub-agent's role: its toolset and how it is prompted. Delegation is CC-style
/// star topology — the parent spawns focused workers/explorers and collects their
/// reports; sub-agents don't talk to each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentRole {
    /// Full toolset (default when unspecified).
    Default,
    /// Read-only: investigates and reports; cannot modify the workspace.
    Explorer,
    /// Writes code, pinned to an explicit set of owned files.
    Worker,
}

impl AgentRole {
    pub(crate) fn parse(s: Option<&str>) -> Self {
        match s.map(str::trim) {
            Some("explorer") => AgentRole::Explorer,
            Some("worker") => AgentRole::Worker,
            _ => AgentRole::Default,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            AgentRole::Default => "default",
            AgentRole::Explorer => "explorer",
            AgentRole::Worker => "worker",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_delegation_for_parallel_and_chinese_markers() {
        assert!(task_suggests_delegation("请并行 review 架构 and security"));
        assert!(task_suggests_delegation("分头调查 tools 和 stability"));
        assert!(task_suggests_delegation(
            "review architecture and performance audit of the stack"
        ));
        assert!(!task_suggests_delegation("fix the typo in main.rs"));
        assert!(!task_suggests_delegation("你好"));
    }

    #[test]
    fn steer_hint_names_spawn_agent() {
        let h = multi_agent_steer_hint();
        assert!(h.contains("## Multi-agent delegation"));
        assert!(h.contains("spawn_agent"));
    }
}
