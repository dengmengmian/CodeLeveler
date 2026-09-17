//! Sub-agent delegation policy: roles, limits, nicknames.

pub(crate) use crate::child_profile::{AgentRole, ChildProfile};

/// Max sub-agent nesting depth (a sub-agent may not itself spawn one).
pub(crate) const MAX_SUB_AGENT_DEPTH: u32 = 1;

/// Whether the top-level executor should inject the multi-agent coordination
/// hint: a static capability note, injected once, never re-decided. Children
/// (`depth > 0`) never get it.
pub fn should_inject_delegation_hint(allow_delegation: bool, depth: u32) -> bool {
    allow_delegation && depth == 0
}

/// First line of [`multi_agent_steer_hint`]; the drive loop dedups injection
/// by this exact header, so the two must never drift apart again (review 必改③:
/// a stale needle re-injected the hint into every resumed window).
pub const MULTI_AGENT_HINT_HEADER: &str = "## Multi-agent coordination";

/// One-shot user injection when [`should_inject_delegation_hint`] is true.
/// V2: this is the canonical coordination policy — Main is both the coding
/// agent and the coordinator of this workspace.
pub fn multi_agent_steer_hint() -> String {
    "## Multi-agent coordination\n\
     `spawn_agent` calls emitted in ONE assistant message run concurrently; \
     calls in separate messages run in sequence. A child does not see this \
     conversation, so `task` must be self-contained — it is the only required \
     argument, and the child claims its own bounded write scope with \
     claim_write_scope rather than being handed one.\n\
     A child runs in the background by default: the call returns its id \
     immediately and the runtime tells you when it settles, so there is \
     nothing to poll. The ownership fence refuses your writes to a file a \
     running child owns, by editor tool or by shell. Set \
     run_in_background=false when your next action depends on that child's \
     result."
        .to_string()
}

/// First lines of every notice the runtime writes into a parent's transcript
/// as a user-role message. The runtime authored these words, so it is the one
/// place that can tell a client "this is a notice, not the user" — clients
/// must never infer that from the text themselves.
pub const RUNTIME_NOTICE_HEADERS: &[&str] = &[
    "## Background sub-agent settled",
    "## Sub-agent results re-delivered after restart",
    "## Sub-agents resumed after restart",
    "## Delegations lost at restart",
];

/// The settlement notice injected into the parent's context when a BACKGROUND
/// child finishes. Runtime-owned and unconditional (a child killed by its
/// budget is exactly the child that never got to report); `result_for_parent`
/// is [`ChildResult::for_parent`] output, so the four-way completion truth and
/// partial findings arrive verbatim.
pub(crate) fn settlement_notice(
    nickname: &str,
    id: &str,
    role: AgentRole,
    scope: &[String],
    result_for_parent: &str,
) -> String {
    let scope_line = if scope.is_empty() {
        String::new()
    } else {
        format!("Its exclusive scope ({}) is released.\n", scope.join(", "))
    };
    format!(
        "## Background sub-agent settled\n\
         {nickname} ({id}, role={}) has finished and will do no further work.\n\
         {scope_line}{result_for_parent}\n\
         Inspect and integrate this result where it matters; do not redo work \
         it completed. If this completes or changes your active plan step, \
         synchronize the plan with update_plan before moving on — a child \
         finishing does not advance the plan on its own.",
        role.label()
    )
}

/// A child that durably settled while the previous window ended, before the
/// parent demonstrably acted on its result. The host derives these from the
/// event log at seed time (the `SubAgentFinished` fact) so the resumed parent
/// gets the recorded outcome re-delivered instead of a false "lost" verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettledChildNotice {
    pub id: String,
    pub nickname: String,
    pub role: String,
    pub ok: bool,
    /// The recorded settlement summary (bounded preview persisted on the
    /// terminal event). The full prose result of the dead activation is not
    /// reconstructable — this is the durable truth that remains.
    pub summary: String,
}

/// Re-delivery note for settlements the previous window may not have acted
/// on. Distinct from `lost_children_note`: these children FINISHED — telling
/// the model they were lost would be false, and would invite re-doing work
/// that is already done.
pub(crate) fn settled_children_redelivery_note(children: &[SettledChildNotice]) -> String {
    let mut out = String::from(
        "## Sub-agent results re-delivered after restart\n\
         The previous session window ended after these sub-agents settled, \
         possibly before their results were acted on. Their recorded outcomes \
         follow (re-delivered; one may repeat a notice you already saw). \
         Integrate them — do NOT re-delegate or redo work that is already \
         done:\n",
    );
    for child in children {
        let status = if child.ok {
            "completed"
        } else {
            "stopped incomplete"
        };
        out.push_str(&format!(
            "- {} ({}, role={}) — {status}: {}\n",
            child.nickname, child.id, child.role, child.summary
        ));
    }
    out
}

/// An interrupted child rebuilt from its durable session, ready for a new
/// activation under the same identity.
#[derive(Debug, Clone)]
pub(crate) struct ResumableChild {
    pub id: String,
    pub nickname: String,
    pub role: AgentRole,
    pub spec: leveler_lifecycle::ChildSpawnSpec,
    /// The child's own transcript, as persisted.
    pub prior: Vec<leveler_model::Message>,
    /// The host-authored recovery note, appended (and persisted) before the
    /// child is asked anything.
    pub note: leveler_model::Message,
}

/// What the parent reads when its interrupted children continue.
pub(crate) fn resumed_children_note(children: &[ResumableChild]) -> String {
    let mut out = String::from(
        "## Sub-agents resumed after restart\n\
         The previous session window ended while these sub-agents were still \
         working. Each continues its own task in the background under the same \
         id, from its saved progress; its settlement will arrive as usual. Do not \
         re-delegate their work:\n",
    );
    for child in children {
        if child.spec.files.is_empty() {
            out.push_str(&format!(
                "- {} ({}, role={})\n",
                child.nickname,
                child.id,
                child.role.label()
            ));
        } else {
            out.push_str(&format!(
                "- {} ({}, role={}, scope: {})\n",
                child.nickname,
                child.id,
                child.role.label(),
                child.spec.files.join(", ")
            ));
        }
    }
    out
}

/// Truthful note for a resumed run whose previous window still had background
/// children running: in-process children do not survive a restart.
pub(crate) fn lost_children_note(outstanding: &[String]) -> String {
    let mut out = String::from(
        "## Delegations lost at restart\n\
         The previous session window ended while these background sub-agents \
         were still running. They did NOT survive the restart — their work is \
         NOT done, their exclusive scopes are released, and no settlement will \
         arrive. Re-delegate or do the work yourself if it is still needed:\n",
    );
    for entry in outstanding {
        // id|nickname|role|scope
        let mut parts = entry.splitn(4, '|');
        let id = parts.next().unwrap_or("?");
        let nickname = parts.next().unwrap_or("?");
        let role = parts.next().unwrap_or("?");
        let scope = parts.next().unwrap_or("");
        if scope.is_empty() {
            out.push_str(&format!("- {nickname} ({id}, role={role})\n"));
        } else {
            out.push_str(&format!(
                "- {nickname} ({id}, role={role}, scope: {scope})\n"
            ));
        }
    }
    out
}

/// A delegated unit must eventually return control to its parent even if a
/// provider or tool keeps making progress without reaching a terminal answer.
/// Parent goal turns are not capped here; only children are, so a hung sub-agent
/// cannot strand the parent forever.
pub(crate) const SUB_AGENT_MAX_DURATION: std::time::Duration =
    std::time::Duration::from_secs(crate::child_profile::CHILD_MAX_DURATION_SECS);
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

/// Durable identity for one `spawn_agent` child. Unique across turns of the
/// same session. Not a [`leveler_core::SessionId`] and not a run-local
/// `agent-N` ordinal.
pub(crate) fn new_delegated_agent_id() -> String {
    leveler_core::AgentId::generate().into_inner()
}

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

pub use leveler_lifecycle::ChildStatus;

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

/// Whether two worker scopes overlap (equal path, or one is a directory
/// prefix of the other), after `./` normalization. Used to refuse same-batch
/// workers whose exclusive scopes are not actually exclusive.
pub(crate) fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    // Trailing slashes stripped for the same reason as the write allowlist:
    // the schema's directory example is spelled `src/output/`.
    let norm = |p: &String| {
        p.trim()
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string()
    };
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
    fn child_wall_clock_matches_the_profile_budget() {
        assert_eq!(
            SUB_AGENT_MAX_DURATION.as_secs(),
            crate::child_profile::CHILD_MAX_DURATION_SECS
        );
        assert_eq!(
            ChildProfile::default_profile()
                .budget_policy
                .max_duration_secs,
            SUB_AGENT_MAX_DURATION.as_secs()
        );
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
        // The schema's directory example carries a trailing slash; overlap
        // detection must see through it, or two same-batch workers could hold
        // "src/output/" and "src/output/foo.rs" as disjoint scopes.
        assert!(scopes_overlap(
            &["src/output/".to_string()],
            &["src/output/foo.rs".to_string()]
        ));
        assert!(scopes_overlap(
            &["src/output".to_string()],
            &["src/output/".to_string()]
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn looks_like_run_ordinal(id: &str) -> bool {
        id.strip_prefix("agent-")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
    }

    #[test]
    fn delegated_agent_ids_are_unique_and_not_run_ordinals() {
        let a = new_delegated_agent_id();
        let b = new_delegated_agent_id();
        assert_ne!(a, b);
        assert!(!looks_like_run_ordinal(&a), "{a}");
        assert!(!looks_like_run_ordinal(&b), "{b}");
    }

    #[test]
    fn nickname_stays_a_display_label_not_an_id() {
        assert_eq!(agent_nickname(1), "Euclid");
        assert_eq!(agent_nickname(2), "Newton");
    }

    #[test]
    fn steer_hint_names_spawn_agent() {
        let h = multi_agent_steer_hint();
        // The dedup needle must be the hint's actual first line (必改③).
        assert!(h.starts_with(MULTI_AGENT_HINT_HEADER));
        assert!(h.contains("spawn_agent"));
    }

    /// The hint states the delegation MECHANICS and nothing else: how
    /// concurrency is expressed, what a child can see, who owns which files,
    /// and when a background call blocks. When to delegate, and what is worth
    /// delegating, is the model's judgement (`docs/ARCHITECTURE.md` §1.1).
    #[test]
    fn the_steer_hint_states_mechanics_not_strategy() {
        let h = multi_agent_steer_hint();
        let lower = h.to_ascii_lowercase();
        // Mechanics the model cannot derive from the tool schema alone.
        assert!(lower.contains("one assistant message run concurrently"));
        assert!(lower.contains("does not see this conversation"));
        assert!(lower.contains("claim_write_scope"));
        assert!(lower.contains("ownership fence"));
        assert!(lower.contains("run_in_background=false"));
        // Strategy that used to ride along with them.
        for coaching in [
            "continue useful work",
            "simply stays here",
            "do not stop early",
            "always spawn",
            "you are both the coding agent",
        ] {
            assert!(!lower.contains(coaching), "coaching left in: {coaching:?}");
        }
    }

    #[test]
    fn keep_vs_delegate_hint_is_offered_on_ordinary_implementation_goals() {
        assert!(should_inject_delegation_hint(true, 0));
        assert!(!should_inject_delegation_hint(true, 1));
        assert!(!should_inject_delegation_hint(false, 0));
    }

    /// Every notice the runtime writes into a parent transcript opens with a
    /// header the client classifier knows; a new notice that forgot to
    /// register would render as something the user typed.
    #[test]
    fn every_runtime_notice_opens_with_a_registered_header() {
        let settled = SettledChildNotice {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            ok: true,
            summary: "done".into(),
        };
        for note in [
            settlement_notice("Euclid", "c1", AgentRole::Explorer, &[], "result"),
            settled_children_redelivery_note(&[settled]),
            lost_children_note(&["c1|Euclid|explorer|".to_string()]),
            resumed_children_note(&[ResumableChild {
                id: "c2".into(),
                nickname: "Newton".into(),
                role: AgentRole::Explorer,
                spec: leveler_lifecycle::ChildSpawnSpec::default(),
                prior: Vec::new(),
                note: leveler_model::Message::text(leveler_model::Role::User, "note"),
            }]),
        ] {
            assert!(
                RUNTIME_NOTICE_HEADERS.iter().any(|h| note.starts_with(h)),
                "{note}"
            );
        }
    }

    /// Restart notes are prose a model reads. A mangled line continuation
    /// once left ten-space gaps and stray line breaks mid-sentence in them.
    #[test]
    fn restart_notes_read_as_clean_prose() {
        let settled = settled_children_redelivery_note(&[SettledChildNotice {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            ok: true,
            summary: "done".into(),
        }]);
        let resumed = resumed_children_note(&[ResumableChild {
            id: "c2".into(),
            nickname: "Newton".into(),
            role: AgentRole::Worker,
            spec: leveler_lifecycle::ChildSpawnSpec {
                files: vec!["src/a.rs".into()],
                ..Default::default()
            },
            prior: Vec::new(),
            note: leveler_model::Message::text(leveler_model::Role::User, "note"),
        }]);
        for note in [settled, resumed] {
            assert!(!note.contains("  "), "a gap inside the prose: {note:?}");
            let header_end = note.find('\n').expect("a header line");
            assert!(
                !note[header_end + 1..].starts_with(' '),
                "the body starts indented: {note:?}"
            );
        }
    }

    #[test]
    fn nicknames_cycle() {
        assert_eq!(agent_nickname(1), "Euclid");
        assert_eq!(agent_nickname(AGENT_NICKNAMES.len() + 1), "Euclid #2");
    }
}
