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

/// Whether the top-level executor should inject the keep-vs-delegate planning
/// hint. Keyword heuristics (`task_suggests_delegation`) do **not** gate this:
/// ordinary implementation goals must still see the policy. Children (`depth > 0`)
/// never get it.
pub fn should_inject_delegation_hint(allow_delegation: bool, depth: u32) -> bool {
    allow_delegation && depth == 0
}

/// One-shot user injection when [`should_inject_delegation_hint`] is true.
pub fn multi_agent_steer_hint() -> String {
    "## Multi-agent delegation\n\
     Before substantial implementation, decide whether any bounded subtask \
     should leave this trajectory. Prefer `spawn_agent` with role='worker' when \
     the subtask has a clear file/module/symbol scope, can be independently \
     verified, needs little shared mutable context, can produce a concrete \
     implementation, and would reduce main-trajectory context or latency. A \
     worker performs scoped writes inside `files` and can run targeted tests; \
     put a complete self-contained `task` in the spawn — children do not see \
     parent tool history. After a child returns, inspect and integrate; do not \
     silently redo the same writes.\n\
     Keep work on the main trajectory when delegation would introduce more \
     coordination cost than value, or the step is a single trivial edit. If \
     several subtasks are independent, emit several `spawn_agent` calls in one \
     turn. Do not spawn for its own sake.\n\
     Long work stays in this direct tool loop: keep calling tools / spawn_agent \
     until the goal is proven; do not stop early with a plan-only summary."
        .to_string()
}

/// What the drive loop must do at a round boundary for the delegation
/// decision point (MA-WA1). Ordering: facts (`RecordDelegated` / `RecordKept`)
/// come before `Offer`, so the offer is never emitted after the decision it
/// would ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DelegationRoundAction {
    /// Inject the one-shot keep-vs-delegate request. `trigger` (`plan` |
    /// `first_mutation`) labels the durable `offered` fact; `steps` carries the
    /// model's own open plan steps (empty on the mutation fallback) so the
    /// request is anchored to the registered decomposition, item by item.
    Offer {
        trigger: &'static str,
        steps: Vec<String>,
    },
    /// First mutation after a visible offer with no Worker: durable `kept`.
    RecordKept,
    /// First Worker admitted: durable `delegated` with its file scope.
    RecordDelegated(String),
}

/// One-shot keep-vs-delegate decision point (MA-WA1).
///
/// The prior architecture had no decision layer at all: the only delegation
/// input was a round-1 hint, never re-anchored at the moment the model's own
/// decomposition made the judgment possible. This struct guarantees exactly one
/// neutral decision point per goal epoch — at plan registration (decomposition
/// written down) or, for plan-less runs, at the first mutation — and derives an
/// observable disposition from behavior, never from self-report or hidden
/// reasoning. It never blocks a call, never repeats, and treats KEEP as a
/// first-class outcome.
#[derive(Debug, Clone)]
pub(crate) struct DelegationDecisionPoint {
    /// depth == 0 ∧ allow_delegation. Children never see any of this.
    eligible: bool,
    /// The offer was injected (this window or a prior one, via ProgressLedger).
    offered: bool,
    /// The model has had a full round to read the offer; only mutations after
    /// that count as a KEEP commitment.
    offer_visible: bool,
    kept_recorded: bool,
    delegated_recorded: bool,
    // Round-local facts, cleared by `end_round`.
    plan_open_steps: Vec<String>,
    mutated_this_round: bool,
    worker_scope_this_round: Option<String>,
}

impl DelegationDecisionPoint {
    pub(crate) fn new(eligible: bool, already_offered: bool) -> Self {
        Self {
            eligible,
            offered: already_offered,
            offer_visible: already_offered,
            kept_recorded: false,
            delegated_recorded: false,
            plan_open_steps: Vec::new(),
            mutated_this_round: false,
            worker_scope_this_round: None,
        }
    }

    /// A ModelExplicit plan was registered/replaced; `open_steps` are the step
    /// texts still pending/in_progress, in plan order.
    pub(crate) fn note_plan_registered(&mut self, open_steps: &[String]) {
        if open_steps.len() > self.plan_open_steps.len() {
            self.plan_open_steps = open_steps.to_vec();
        }
    }

    /// A tool call successfully mutated the workspace this round.
    pub(crate) fn note_mutation(&mut self) {
        self.mutated_this_round = true;
    }

    /// A Worker child passed spawn admission this round.
    pub(crate) fn note_worker_admitted(&mut self, files: &[String]) {
        if self.worker_scope_this_round.is_none() {
            self.worker_scope_this_round = Some(files.join(", "));
        }
    }

    /// Resolve this round's facts into actions. At most one `Offer` per epoch;
    /// `kept`/`delegated` each at most once.
    pub(crate) fn end_round(&mut self) -> Vec<DelegationRoundAction> {
        let mut actions = Vec::new();
        if !self.eligible {
            self.clear_round();
            return actions;
        }
        if let Some(scope) = self.worker_scope_this_round.take()
            && !self.delegated_recorded
        {
            self.delegated_recorded = true;
            actions.push(DelegationRoundAction::RecordDelegated(scope));
        }
        if self.mutated_this_round
            && self.offer_visible
            && !self.kept_recorded
            && !self.delegated_recorded
        {
            self.kept_recorded = true;
            actions.push(DelegationRoundAction::RecordKept);
        }
        if !self.offered && !self.delegated_recorded {
            if self.plan_open_steps.len() >= 2 {
                self.offered = true;
                actions.push(DelegationRoundAction::Offer {
                    trigger: "plan",
                    steps: std::mem::take(&mut self.plan_open_steps),
                });
            } else if self.mutated_this_round {
                self.offered = true;
                actions.push(DelegationRoundAction::Offer {
                    trigger: "first_mutation",
                    steps: Vec::new(),
                });
            }
        }
        // Anything offered up to and including this round is readable next round.
        self.offer_visible = self.offered;
        self.clear_round();
        actions
    }

    fn clear_round(&mut self) {
        self.plan_open_steps.clear();
        self.mutated_this_round = false;
        self.worker_scope_this_round = None;
    }

    #[cfg(test)]
    pub(crate) fn offered(&self) -> bool {
        self.offered
    }
}

/// How many registered plan steps the decision request enumerates at most.
const DECISION_REQUEST_MAX_STEPS: usize = 8;

/// The one-shot decision-point message. Deliberately neutral: it names both
/// outcomes as valid, directs nothing globally, and says it will not repeat.
///
/// When the trigger was plan registration, the request enumerates the model's
/// OWN open steps and asks for a per-step disposition. Probe evidence (miller
/// head/tail): a generic "any bounded piece?" invitation lets the model judge
/// the whole plan by its most coupled steps and keep everything — including
/// the separable mechanical tail (doc/golden updates) that then exhausts its
/// round budget. Anchoring the choice to each registered item is decision
/// scaffolding, not steering: the criteria stay symmetric and KEEP-everything
/// stays a first-class outcome.
pub(crate) fn delegation_decision_request(steps: &[String]) -> String {
    let mut out = String::from("## Delegation disposition (one-time)\n");
    if steps.is_empty() {
        out.push_str(
            "Your task decomposition is on record. Decide once, before continuing \
             implementation: does any remaining piece form a bounded independent \
             workstream — a clear file/module scope, independently verifiable, needing \
             little of your in-flight context?\n\
             - If yes: you may delegate it now via `spawn_agent` with role='worker', a \
             complete self-contained `task`, and the exact `files` (or directories) it \
             will own; inspect and integrate its result when it returns.\n\
             - If no: keep all the work and continue exactly as you were. KEEP is a \
             fully valid outcome.\n",
        );
    } else {
        out.push_str(
            "Your plan is on record. Decide once, for EACH open step below: KEEP \
             (do it on this trajectory) or DELEGATE (`spawn_agent` role='worker' \
             with a complete self-contained `task` and the exact `files` or \
             directories that step owns).\n",
        );
        for (i, step) in steps.iter().take(DECISION_REQUEST_MAX_STEPS).enumerate() {
            out.push_str(&format!("{}. {}\n", i + 1, step));
        }
        if steps.len() > DECISION_REQUEST_MAX_STEPS {
            out.push_str(&format!(
                "… and {} more step(s), same choice each.\n",
                steps.len() - DECISION_REQUEST_MAX_STEPS
            ));
        }
        out.push_str(
            "A step that needs your in-flight context, shares edits with other \
             steps, or is trivial: KEEP it. A step that is bounded, independently \
             verifiable, and does not need what is in your head — for example a \
             self-contained module, or a broad mechanical update of docs/goldens/\
             fixtures — is a candidate for DELEGATE: the worker runs concurrently \
             and returns a result you inspect and integrate.\n\
             Answer with one short KEEP/DELEGATE line per step, then proceed \
             accordingly. KEEP for every step is a valid outcome.\n",
        );
    }
    out.push_str("This is the only time the harness raises this; you will not be asked again.");
    out
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

/// The capability contract of one child role, resolved in exactly one place.
///
/// This formalises what was previously scattered across the drive-loop spawn
/// admission, `child_for_role_on`, and `run_reviewer_child`: which registry a
/// role gets, whether it must own an explicit file scope, whether its
/// `report_finding(blocking=true)` is honoured, and how long it may run.
/// Capability is expressed per semantic class, never per role × tool name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChildProfile {
    pub role: AgentRole,
    /// Physically read-only toolset (`read_only_subset`): no mutating tool is
    /// even advertised, so denial is structural, not behavioral.
    pub read_only: bool,
    /// Spawn admission requires a non-empty exclusive `files` scope.
    pub requires_scope: bool,
    /// `report_finding(blocking=true)` is honoured; other roles are recorded
    /// non-blocking so an explorer observation can never gate closure.
    pub may_report_blocking: bool,
    /// Hard round bound (None = run until terminal within budgets).
    pub max_rounds: Option<u32>,
    /// Force serial tool execution (a writer sharing the workspace).
    pub serial_tools: bool,
}

impl ChildProfile {
    pub(crate) fn resolve(role: AgentRole) -> Self {
        match role {
            AgentRole::Default => Self {
                role,
                read_only: false,
                requires_scope: false,
                may_report_blocking: false,
                max_rounds: None,
                serial_tools: false,
            },
            AgentRole::Explorer => Self {
                role,
                read_only: true,
                requires_scope: false,
                may_report_blocking: false,
                max_rounds: None,
                serial_tools: false,
            },
            AgentRole::Worker => Self {
                role,
                read_only: false,
                requires_scope: true,
                may_report_blocking: false,
                max_rounds: None,
                serial_tools: true,
            },
            // R013r: an unbounded reviewer burned the parent's whole round
            // ceiling reading a repo it was only asked to judge. Reading a
            // diff is a bounded job.
            AgentRole::Reviewer => Self {
                role,
                read_only: true,
                requires_scope: false,
                may_report_blocking: true,
                max_rounds: Some(20),
                serial_tools: false,
            },
        }
    }

    /// Minimal capability negotiation at spawn admission: the requested
    /// capabilities (role + file scope) against the role's profile.
    /// `Err` is an honest denial fed back to the model — never a silent
    /// downgrade.
    pub(crate) fn admit(role: AgentRole, files: &[String]) -> Result<Self, String> {
        let profile = Self::resolve(role);
        if profile.requires_scope && files.is_empty() {
            return Err(format!(
                "role='{}' requires a non-empty `files` list naming the files it \
                 exclusively owns; an unscoped writer is not admitted.",
                role.label()
            ));
        }
        if profile.read_only && !files.is_empty() {
            return Err(format!(
                "role='{}' is read-only and cannot take a `files` write scope. \
                 Use role='worker' for edits, or drop `files` to investigate.",
                role.label()
            ));
        }
        Ok(profile)
    }
}

/// Whether two worker scopes overlap (equal path, or one is a directory
/// prefix of the other), after `./` normalization. Used to refuse same-batch
/// workers whose exclusive scopes are not actually exclusive.
pub(crate) fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    let norm = |p: &String| p.trim().trim_start_matches("./").to_string();
    let covers = |x: &str, y: &str| x == y || y.starts_with(&format!("{x}/"));
    a.iter().map(&norm).any(|pa| {
        b.iter()
            .map(&norm)
            .any(|pb| covers(&pa, &pb) || covers(&pb, &pa))
    })
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn explorer_and_reviewer_are_structurally_read_only() {
        for role in [AgentRole::Explorer, AgentRole::Reviewer] {
            let p = ChildProfile::resolve(role);
            assert!(p.read_only, "{role:?} must hold no write tools");
            assert!(!p.requires_scope);
            assert!(!p.serial_tools);
        }
    }

    #[test]
    fn only_the_reviewer_may_raise_blocking_findings() {
        for role in [AgentRole::Default, AgentRole::Explorer, AgentRole::Worker] {
            assert!(!ChildProfile::resolve(role).may_report_blocking, "{role:?}");
        }
        assert!(ChildProfile::resolve(AgentRole::Reviewer).may_report_blocking);
    }

    #[test]
    fn the_reviewer_is_bounded_and_the_worker_is_serial() {
        assert_eq!(
            ChildProfile::resolve(AgentRole::Reviewer).max_rounds,
            Some(20)
        );
        let worker = ChildProfile::resolve(AgentRole::Worker);
        assert!(worker.serial_tools);
        assert!(!worker.read_only);
        assert!(worker.requires_scope);
    }

    #[test]
    fn a_worker_without_a_scope_is_refused_not_unleashed() {
        // Before this contract an empty `files` silently produced a worker
        // with UNRESTRICTED write access — the opposite of what the schema
        // promises ("MUST be given files it exclusively owns").
        let err = ChildProfile::admit(AgentRole::Worker, &[]).unwrap_err();
        assert!(
            err.contains("files"),
            "denial must name the missing scope: {err}"
        );
        assert!(ChildProfile::admit(AgentRole::Worker, &["src/a.rs".into()]).is_ok());
    }

    #[test]
    fn a_read_only_role_asking_for_write_scope_is_refused() {
        let err = ChildProfile::admit(AgentRole::Explorer, &["src/a.rs".into()]).unwrap_err();
        assert!(
            err.contains("read-only"),
            "denial must say why, not silently ignore the request: {err}"
        );
        assert!(ChildProfile::admit(AgentRole::Explorer, &[]).is_ok());
        assert!(ChildProfile::admit(AgentRole::Default, &[]).is_ok());
    }

    #[test]
    fn overlapping_scopes_are_detected_by_path_and_directory_prefix() {
        let a = vec!["src/auth.rs".to_string()];
        assert!(scopes_overlap(&a, &["src/auth.rs".to_string()]));
        assert!(scopes_overlap(&a, &["./src/auth.rs".to_string()]));
        assert!(scopes_overlap(&["src".to_string()], &a));
        assert!(scopes_overlap(&a, &["src".to_string()]));
        assert!(!scopes_overlap(&a, &["src/config.rs".to_string()]));
        // Prefix means DIRECTORY prefix, not string prefix.
        assert!(!scopes_overlap(
            &["src/auth.rs".to_string()],
            &["src/auth.rs.bak".to_string()]
        ));
        assert!(!scopes_overlap(&a, &[]));
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
    fn steer_hint_is_keep_vs_delegate_not_always_spawn() {
        let h = multi_agent_steer_hint();
        let lower = h.to_ascii_lowercase();
        assert!(lower.contains("bounded"));
        assert!(lower.contains("independently verif") || lower.contains("independent verif"));
        assert!(lower.contains("scoped") && (lower.contains("write") || lower.contains("edit")));
        assert!(lower.contains("coordination") || lower.contains("keep work on the main"));
        assert!(!lower.contains("always spawn"));
        assert!(!lower.contains("must spawn"));
    }

    #[test]
    fn keep_vs_delegate_hint_is_offered_on_ordinary_implementation_goals() {
        assert!(should_inject_delegation_hint(true, 0));
        assert!(!should_inject_delegation_hint(true, 1));
        assert!(!should_inject_delegation_hint(false, 0));
        // A real GitHub-issue goal without parallel/spawn words still gets the hint.
        let goal = "Add `--export-toml FILE`. When that flag is passed, write \
                    the timing summary as TOML. Existing export flags must keep working.";
        assert!(
            !task_suggests_delegation(goal),
            "ordinary implementation goals must stay unsteered by the keyword heuristic"
        );
        assert!(should_inject_delegation_hint(true, 0));
    }

    #[test]
    fn nicknames_cycle() {
        assert_eq!(agent_nickname(1), "Euclid");
        assert_eq!(agent_nickname(AGENT_NICKNAMES.len() + 1), "Euclid #2");
    }
}

#[cfg(test)]
mod decision_point_tests {
    use super::*;

    fn steps(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plan_registration_triggers_one_offer_and_mutations_then_record_keep() {
        let mut dp = DelegationDecisionPoint::new(true, false);
        dp.note_plan_registered(&steps(&["edit head", "edit tail"]));
        assert_eq!(
            dp.end_round(),
            vec![DelegationRoundAction::Offer {
                trigger: "plan",
                steps: steps(&["edit head", "edit tail"]),
            }],
            "decomposition on record → one offer carrying the model's own steps"
        );
        // Same round as the offer: the model has not read it yet, so a
        // mutation that raced it in a later round is the KEEP commitment.
        dp.note_mutation();
        assert_eq!(dp.end_round(), vec![DelegationRoundAction::RecordKept]);
        dp.note_mutation();
        assert!(
            dp.end_round().is_empty(),
            "keep is recorded once, never nagged"
        );
    }

    #[test]
    fn first_mutation_is_the_fallback_trigger_for_plan_less_runs() {
        let mut dp = DelegationDecisionPoint::new(true, false);
        dp.note_mutation();
        assert_eq!(
            dp.end_round(),
            vec![DelegationRoundAction::Offer {
                trigger: "first_mutation",
                steps: Vec::new(),
            }]
        );
        dp.note_mutation();
        assert_eq!(dp.end_round(), vec![DelegationRoundAction::RecordKept]);
    }

    #[test]
    fn a_single_step_plan_waits_for_the_mutation_fallback() {
        let mut dp = DelegationDecisionPoint::new(true, false);
        dp.note_plan_registered(&steps(&["do it"]));
        assert!(dp.end_round().is_empty(), "one step is not a decomposition");
        dp.note_mutation();
        assert_eq!(
            dp.end_round(),
            vec![DelegationRoundAction::Offer {
                trigger: "first_mutation",
                steps: Vec::new(),
            }]
        );
    }

    #[test]
    fn worker_admission_records_delegated_and_suppresses_offer_and_keep() {
        let mut dp = DelegationDecisionPoint::new(true, false);
        dp.note_worker_admitted(&["src/a.rs".into(), "src/b.rs".into()]);
        dp.note_mutation();
        assert_eq!(
            dp.end_round(),
            vec![DelegationRoundAction::RecordDelegated(
                "src/a.rs, src/b.rs".into()
            )],
            "a model that already decided is not offered and not marked kept"
        );
        dp.note_worker_admitted(&["src/c.rs".into()]);
        assert!(dp.end_round().is_empty(), "delegated is recorded once");
    }

    #[test]
    fn delegation_after_keep_is_still_recorded_as_a_fact() {
        let mut dp = DelegationDecisionPoint::new(true, false);
        dp.note_mutation();
        dp.end_round();
        dp.note_mutation();
        assert_eq!(dp.end_round(), vec![DelegationRoundAction::RecordKept]);
        dp.note_worker_admitted(&["src/a.rs".into()]);
        assert_eq!(
            dp.end_round(),
            vec![DelegationRoundAction::RecordDelegated("src/a.rs".into())]
        );
    }

    #[test]
    fn children_and_disabled_delegation_are_fully_silent() {
        for mut dp in [
            DelegationDecisionPoint::new(false, false),
            DelegationDecisionPoint::new(false, true),
        ] {
            dp.note_plan_registered(&steps(&["a", "b", "c"]));
            dp.note_mutation();
            dp.note_worker_admitted(&["a".into()]);
            assert!(dp.end_round().is_empty());
        }
    }

    #[test]
    fn a_prior_window_offer_is_never_re_asked() {
        // Resume: ProgressLedger says the offer already happened. The next
        // mutation records KEEP; no second offer this epoch.
        let mut dp = DelegationDecisionPoint::new(true, true);
        assert!(dp.offered());
        dp.note_plan_registered(&steps(&["a", "b", "c", "d"]));
        dp.note_mutation();
        assert_eq!(dp.end_round(), vec![DelegationRoundAction::RecordKept]);
    }

    fn assert_request_is_neutral(text: &str) {
        let lower = text.to_ascii_lowercase();
        assert!(lower.contains("## delegation disposition"));
        assert!(lower.contains("spawn_agent") && lower.contains("worker"));
        assert!(lower.contains("valid outcome"));
        assert!(lower.contains("not be asked again"));
        // Forbidden directive framings (gate §5): the decision point asks,
        // it never steers.
        for banned in [
            "you should delegate",
            "prefer worker",
            "prefer delegation",
            "delegate when possible",
            "always",
            "parallelize aggressively",
            "for complex tasks",
        ] {
            assert!(
                !lower.contains(banned),
                "banned phrase `{banned}` in: {text}"
            );
        }
    }

    #[test]
    fn the_generic_request_is_neutral_names_both_outcomes_and_promises_no_repeat() {
        assert_request_is_neutral(&delegation_decision_request(&[]));
    }

    /// Iteration 2 (miller probe evidence): the plan-triggered request must
    /// enumerate the model's own open steps and ask per-step, with symmetric
    /// KEEP/DELEGATE criteria — never a global directive.
    #[test]
    fn the_plan_request_enumerates_the_registered_steps_per_item() {
        let items = steps(&["给 head.go 加 --filename", "更新 docs 与 goldens"]);
        let text = delegation_decision_request(&items);
        assert_request_is_neutral(&text);
        assert!(text.contains("1. 给 head.go 加 --filename"), "{text}");
        assert!(text.contains("2. 更新 docs 与 goldens"), "{text}");
        assert!(text.contains("EACH open step"), "{text}");
        assert!(
            text.contains("KEEP for every step is a valid outcome"),
            "{text}"
        );
        // Long plans stay bounded.
        let many: Vec<String> = (0..12).map(|i| format!("step {i}")).collect();
        let long = delegation_decision_request(&many);
        assert!(long.contains("… and 4 more step(s)"), "{long}");
        assert!(!long.contains("step 9"), "{long}");
    }
}
