//! Sub-agent delegation policy: roles, limits, nicknames.

/// Max sub-agent nesting depth (a sub-agent may not itself spawn one).
pub(crate) const MAX_SUB_AGENT_DEPTH: u32 = 1;
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
