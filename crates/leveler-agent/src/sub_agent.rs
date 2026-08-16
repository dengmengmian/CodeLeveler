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
        "end-to-end",
        "e2e",
        "across the stack",
        "full stack",
        "multi-step",
        "multi step",
        "several components",
        "multiple packages",
        "多个模块",
        "多个包",
        "端到端",
        "全栈",
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
        "分别改",
        "分模块",
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
        "frontend",
        "backend",
        "cli",
        "tui",
        "api",
    ];
    let facet_hits = facets.iter().filter(|f| t.contains(*f)).count();
    if facet_hits >= 2
        && (t.contains("review")
            || t.contains("investigate")
            || t.contains("audit")
            || t.contains("refactor")
            || t.contains("implement")
            || t.contains("fix")
            || t.contains("审查")
            || t.contains("重构")
            || t.contains("实现"))
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
     complete self-contained `task` in each spawn — children do not see parent \
     tool history. After children return, synthesize one answer yourself; do not \
     re-run the same investigation. Do not spawn for trivial single-step work.\n\
     Long work stays in this direct tool loop: keep calling tools / spawn_agent \
     until the goal is proven; do not stop early with a plan-only summary."
        .to_string()
}

/// A delegated unit must eventually return control to its parent even if a
/// provider or tool keeps making progress without reaching a terminal answer.
/// Parent goal turns are not capped here; only children are, so a hung sub-agent
/// cannot strand the parent forever.
pub(crate) const SUB_AGENT_MAX_DURATION: std::time::Duration =
    std::time::Duration::from_secs(20 * 60);
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

/// How a child's run ended, in the four readings a parent must tell apart.
///
/// R007b N1: a child died on its wall budget and returned only a stop string.
/// The parent read that as "investigated, nothing to report" and closed the task
/// without ever opening the file the child had been reading. "Finished and found
/// nothing" and "stopped before finding anything" are opposite instructions, and
/// a bare text field cannot carry the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStatus {
    /// Ran to a clean end and reported something.
    CompletedWithFindings,
    /// Ran to a clean end with nothing to flag. This IS a result.
    CompletedNoFindings,
    /// Stopped early, but what it had established survives in `findings`.
    IncompletePartial,
    /// Stopped early with nothing to show. NOT the same as "no findings".
    IncompleteNoResult,
}

impl ChildStatus {
    pub fn label(self) -> &'static str {
        match self {
            ChildStatus::CompletedWithFindings => "COMPLETED_WITH_FINDINGS",
            ChildStatus::CompletedNoFindings => "COMPLETED_NO_FINDINGS",
            ChildStatus::IncompletePartial => "INCOMPLETE_PARTIAL",
            ChildStatus::IncompleteNoResult => "INCOMPLETE_NO_RESULT",
        }
    }

    /// Whether the child reached the end of its task.
    pub fn completed(self) -> bool {
        matches!(
            self,
            ChildStatus::CompletedWithFindings | ChildStatus::CompletedNoFindings
        )
    }
}

/// What a child hands back to whoever launched it.
#[derive(Debug, Clone)]
pub struct ChildResult {
    pub status: ChildStatus,
    /// The child's report — empty only for the two "no result / no findings"
    /// statuses, which say so explicitly.
    pub findings: String,
    /// Why the run ended, in plain words (empty when it ended normally).
    pub stop_reason: String,
    /// True when `findings` is what the child had reached, not what it set out
    /// to deliver.
    pub partial: bool,
}

impl ChildResult {
    /// Classify a terminal run: `findings` decides between the "with" and
    /// "without" readings, `completed` between the two pairs.
    pub(crate) fn new(completed: bool, findings: &str, stop_reason: impl Into<String>) -> Self {
        let findings = findings.trim().to_string();
        let status = match (completed, findings.is_empty()) {
            (true, false) => ChildStatus::CompletedWithFindings,
            (true, true) => ChildStatus::CompletedNoFindings,
            (false, false) => ChildStatus::IncompletePartial,
            (false, true) => ChildStatus::IncompleteNoResult,
        };
        Self {
            status,
            findings,
            stop_reason: stop_reason.into(),
            partial: status == ChildStatus::IncompletePartial,
        }
    }

    /// The text the parent model reads. The status line comes first so a
    /// truncated result still says what kind of result it is.
    pub fn for_parent(&self, nickname: &str) -> String {
        let mut out = format!("[sub-agent {nickname}] status: {}", self.status.label());
        if !self.stop_reason.is_empty() {
            out.push_str(&format!(" (stopped: {})", self.stop_reason));
        }
        out.push('\n');
        match self.status {
            ChildStatus::CompletedWithFindings => out.push_str(&self.findings),
            ChildStatus::CompletedNoFindings => out.push_str(
                "The sub-agent finished its task and had nothing to report. This IS its \
                 result — the work was done and turned up nothing to flag.",
            ),
            ChildStatus::IncompletePartial => {
                out.push_str(
                    "PARTIAL: the sub-agent was stopped before finishing. Everything it had \
                     established follows; the rest of the task is NOT done.\n",
                );
                out.push_str(&self.findings);
            }
            ChildStatus::IncompleteNoResult => out.push_str(
                "The sub-agent produced NO result. This is NOT \"nothing to report\": the task \
                 was not carried out. Do it yourself or delegate it again — do not treat the \
                 subject as investigated.",
            ),
        }
        out
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
    /// Read-only independent review of a change the parent already made.
    /// Launched by the harness when policy says the change warrants one, so it
    /// is the one role a model cannot ask for.
    Reviewer,
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
            AgentRole::Reviewer => "reviewer",
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
        assert!(task_suggests_delegation("端到端实现下单与支付"));
        assert!(task_suggests_delegation(
            "refactor frontend and backend validation"
        ));
        assert!(!task_suggests_delegation("fix the typo in main.rs"));
        assert!(!task_suggests_delegation("你好"));
    }

    #[test]
    fn steer_hint_names_spawn_agent() {
        let h = multi_agent_steer_hint();
        assert!(h.contains("## Multi-agent delegation"));
        assert!(h.contains("spawn_agent"));
        assert!(h.contains("direct tool loop"));
    }

    #[test]
    fn nicknames_cycle() {
        assert_eq!(agent_nickname(1), "Euclid");
        assert_eq!(agent_nickname(AGENT_NICKNAMES.len() + 1), "Euclid #2");
    }
}
