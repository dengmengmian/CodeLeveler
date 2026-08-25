//! Multi-agent view model.
//!
//! One typed projection between runtime facts and the renderer. The renderer
//! reads this; it never derives a fact from prose.
//!
//! The product question this answers is not "how many agents ran" but "why was
//! the parallel work worth waiting for": what each child was for, what the
//! parent did with what it produced, and what it cost.

use leveler_client_protocol::ChildContribution;

/// Where a child is. Distinct from the outcome of what it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStatus {
    /// Launched, not yet doing visible work.
    Waiting,
    /// Actively spending model calls.
    Running,
    Completed,
    Failed,
}

/// What the parent did with what one child produced.
///
/// Three outcomes, deliberately separate. Collapsing `NothingToFlag` and
/// `NotMeasured` into "0 findings" is the defect that made an eval report
/// claim five reviewers found nothing when all five had reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Contribution {
    /// The child has not finished.
    Pending,
    /// It finished and the runtime produced no projection. Unknown, not zero.
    NotMeasured,
    /// It finished and reported nothing. A real answer.
    NothingToFlag,
    /// It did NOT finish. Whatever it reported is a partial measurement, and
    /// zero findings from a child that was cut off certifies nothing — it only
    /// says the review never got there.
    Incomplete { reported: u32 },
    /// It reported, and this is what became of it.
    Reported {
        total: u32,
        accepted: u32,
        verified: u32,
        rejected: u32,
        open_blocking: u32,
    },
}

impl Contribution {
    /// Did the parent act on anything? A rejection counts — it looked and
    /// decided. A finding nobody judged does not.
    pub fn engaged(&self) -> bool {
        matches!(
            self,
            Contribution::Reported {
                accepted, rejected, ..
            } if *accepted > 0 || *rejected > 0
        )
    }

    /// Findings that still gate a verified closure.
    pub fn open_blocking(&self) -> u32 {
        match self {
            Contribution::Reported { open_blocking, .. } => *open_blocking,
            _ => 0,
        }
    }
}

/// One child, as the user should understand it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildAgentView {
    pub id: String,
    pub nickname: String,
    pub role: String,
    /// Built-in capability contract. `None` means the runtime recorded none.
    pub profile_id: Option<String>,
    pub capabilities: Vec<String>,
    /// What it was asked to do. Leads the running line: a user watching a
    /// spinner needs the reason, not the state.
    pub purpose: String,
    pub status: ChildStatus,
    pub contribution: Contribution,
    /// Latest tool step, when the runtime reported one.
    pub recent_step: Option<String>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub started_elapsed_secs: u64,
    /// Findings loaded on demand by the Contribution Inspector.
    ///
    /// `None` means nobody has asked yet — not that there are none. The
    /// inspector queries when the user opens a detail view, because findings
    /// are ledger facts and streaming them would duplicate the record.
    pub detail: Option<leveler_client_protocol::UiChildContribution>,
}

impl ChildAgentView {
    /// True when this child can only be described by its capabilities, not by
    /// what it produced — the honest state for a failed or unmeasured child.
    pub fn contribution_unknown(&self) -> bool {
        matches!(self.contribution, Contribution::NotMeasured)
    }

    /// Read-only children can be stated as such rather than implied.
    pub fn is_read_only(&self) -> bool {
        !self.capabilities.is_empty()
            && !self
                .capabilities
                .iter()
                .any(|c| matches!(c.as_str(), "write" | "edit" | "apply_patch" | "mutation"))
    }
}

/// One `SubAgentUpdated`, as the view model consumes it.
///
/// A struct rather than ten positional parameters: the call site passes three
/// `Option<String>`-shaped things and two bools, and a transposed pair there
/// would compile and be wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildUpdate {
    pub id: String,
    pub nickname: String,
    pub role: String,
    /// false while running; true once the child finished.
    pub done: bool,
    /// Whether it finished successfully. Only meaningful when `done`, and
    /// load-bearing: zero findings from `ok == false` certifies nothing.
    pub ok: bool,
    /// The task while running; a short result summary once done.
    pub detail: String,
    pub profile_id: Option<String>,
    pub capabilities: Vec<String>,
    /// `None` means the runtime produced no projection — not measured.
    pub contribution: Option<ChildContribution>,
    pub started_elapsed_secs: u64,
}

/// The team working on one task.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskTeamView {
    pub children: Vec<ChildAgentView>,
}

impl TaskTeamView {
    /// Children still working.
    pub fn active(&self) -> impl Iterator<Item = &ChildAgentView> {
        self.children
            .iter()
            .filter(|c| matches!(c.status, ChildStatus::Running | ChildStatus::Waiting))
    }

    /// Findings across the team that still gate a verified closure. Surfaced
    /// at task level because a block discovered at the end is a block the user
    /// should have seen coming.
    pub fn open_blocking(&self) -> u32 {
        self.children
            .iter()
            .map(|c| c.contribution.open_blocking())
            .sum()
    }

    /// Apply one `SubAgentUpdated`. Upserts by id so a child transitions in
    /// place rather than appearing twice.
    pub fn apply_update(&mut self, update: ChildUpdate) {
        let ChildUpdate {
            id,
            nickname,
            role,
            done,
            ok,
            detail,
            profile_id,
            capabilities,
            contribution,
            started_elapsed_secs,
        } = update;
        let contribution = if !done {
            Contribution::Pending
        } else {
            project(contribution.as_ref(), ok)
        };
        let status = match (done, ok) {
            (false, _) => ChildStatus::Waiting,
            (true, true) => ChildStatus::Completed,
            (true, false) => ChildStatus::Failed,
        };
        if let Some(existing) = self.children.iter_mut().find(|c| c.id == id) {
            // A finish event carries no purpose; keep the one from the spawn.
            if !done {
                existing.purpose = detail;
            }
            existing.status = status;
            existing.contribution = contribution;
            if !role.is_empty() {
                existing.role = role;
            }
            if profile_id.is_some() {
                existing.profile_id = profile_id;
            }
            if !capabilities.is_empty() {
                existing.capabilities = capabilities;
            }
            return;
        }
        self.children.push(ChildAgentView {
            id,
            nickname,
            role,
            profile_id,
            capabilities,
            purpose: detail,
            status,
            contribution,
            recent_step: None,
            input_tokens: 0,
            output_tokens: 0,
            started_elapsed_secs,
            detail: None,
        });
    }

    /// Live execution state. `active` separates "spending model calls" from
    /// "launched and queued", which is what makes waiting explicable.
    pub fn apply_progress(&mut self, id: &str, active: bool, input: u32, output: u32) {
        if let Some(c) = self.children.iter_mut().find(|c| c.id == id) {
            c.input_tokens = input;
            c.output_tokens = output;
            if matches!(c.status, ChildStatus::Waiting | ChildStatus::Running) {
                c.status = if active {
                    ChildStatus::Running
                } else {
                    ChildStatus::Waiting
                };
            }
        }
    }

    /// Store a loaded contribution detail. Late or duplicate responses are
    /// harmless: the ledger snapshot is the truth and overwriting is idempotent.
    pub fn apply_detail(&mut self, detail: leveler_client_protocol::UiChildContribution) {
        if let Some(c) = self.children.iter_mut().find(|c| c.id == detail.child_id) {
            c.detail = Some(detail);
        }
    }

    pub fn apply_activity(&mut self, id: &str, tool: &str) {
        if let Some(c) = self.children.iter_mut().find(|c| c.id == id) {
            c.recent_step = Some(tool.to_string());
        }
    }
}

/// `None` is the runtime saying "not measured". It is not a zero, and the
/// difference is the whole reason this function exists.
fn project(c: Option<&ChildContribution>, ok: bool) -> Contribution {
    let Some(c) = c else {
        return Contribution::NotMeasured;
    };
    if !ok {
        // A child that was stopped reports whatever it had. That is a partial
        // measurement, never a clean bill of health.
        return Contribution::Incomplete {
            reported: c.findings_total,
        };
    }
    if c.findings_total == 0 {
        return Contribution::NothingToFlag;
    }
    Contribution::Reported {
        total: c.findings_total,
        accepted: c.findings_accepted,
        verified: c.findings_verified,
        rejected: c.findings_rejected,
        open_blocking: c.findings_open_blocking,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contribution(total: u32, accepted: u32, verified: u32, rejected: u32) -> ChildContribution {
        ChildContribution {
            role: "reviewer".into(),
            profile_id: Some("reviewer".into()),
            profile_role: Some("reviewer".into()),
            capabilities: vec!["code_review".into()],
            source: Some("independent_reviewer".into()),
            findings_total: total,
            findings_acknowledged: total,
            findings_accepted: accepted,
            findings_verified: verified,
            findings_rejected: rejected,
            findings_open_blocking: 0,
        }
    }

    fn started(team: &mut TaskTeamView, id: &str, role: &str, purpose: &str) {
        team.apply_update(crate::multi_agent::ChildUpdate {
            id: id.into(),
            nickname: "Newton".into(),
            role: role.into(),
            done: false,
            ok: false,
            detail: purpose.into(),
            profile_id: Some(role.into()),
            capabilities: vec!["read_file".into()],
            contribution: None,
            started_elapsed_secs: 0,
        });
    }

    fn finished(team: &mut TaskTeamView, id: &str, ok: bool, c: Option<ChildContribution>) {
        team.apply_update(crate::multi_agent::ChildUpdate {
            id: id.into(),
            nickname: "Newton".into(),
            role: "reviewer".into(),
            done: true,
            ok,
            detail: "summary".into(),
            profile_id: None,
            capabilities: Vec::new(),
            contribution: c,
            started_elapsed_secs: 0,
        });
    }

    #[test]
    fn a_running_child_leads_with_its_purpose_not_its_state() {
        let mut team = TaskTeamView::default();
        started(
            &mut team,
            "a1",
            "explorer",
            "analyzing repository structure",
        );
        let c = &team.children[0];
        assert_eq!(c.purpose, "analyzing repository structure");
        assert_eq!(c.status, ChildStatus::Waiting);
        assert_eq!(c.contribution, Contribution::Pending);
    }

    #[test]
    fn progress_separates_running_from_queued() {
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "look");
        team.apply_progress("a1", true, 100, 20);
        assert_eq!(team.children[0].status, ChildStatus::Running);
        team.apply_progress("a1", false, 100, 20);
        assert_eq!(team.children[0].status, ChildStatus::Waiting);
    }

    #[test]
    fn a_child_transitions_in_place_rather_than_appearing_twice() {
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "look");
        finished(&mut team, "a1", true, Some(contribution(2, 1, 1, 0)));
        assert_eq!(team.children.len(), 1);
        assert_eq!(team.children[0].status, ChildStatus::Completed);
    }

    #[test]
    fn a_finish_event_does_not_erase_the_purpose() {
        let mut team = TaskTeamView::default();
        started(
            &mut team,
            "a1",
            "explorer",
            "analyzing repository structure",
        );
        finished(&mut team, "a1", true, Some(contribution(1, 1, 0, 0)));
        assert_eq!(
            team.children[0].purpose, "analyzing repository structure",
            "the finish summary is not the purpose"
        );
    }

    #[test]
    fn explorer_contribution_reports_what_the_parent_accepted() {
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "look");
        finished(&mut team, "a1", true, Some(contribution(7, 5, 3, 1)));
        match &team.children[0].contribution {
            Contribution::Reported {
                total,
                accepted,
                verified,
                rejected,
                ..
            } => {
                assert_eq!((*total, *accepted, *verified, *rejected), (7, 5, 3, 1));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(team.children[0].contribution.engaged());
    }

    #[test]
    fn a_reviewer_that_found_nothing_is_a_result_not_an_empty_state() {
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review the diff");
        finished(&mut team, "r1", true, Some(contribution(0, 0, 0, 0)));
        assert_eq!(team.children[0].contribution, Contribution::NothingToFlag);
        assert!(!team.children[0].contribution_unknown());
    }

    #[test]
    fn an_unmeasured_contribution_is_not_a_zero() {
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review the diff");
        finished(&mut team, "r1", true, None);
        assert_eq!(team.children[0].contribution, Contribution::NotMeasured);
        assert!(team.children[0].contribution_unknown());
        assert_ne!(
            team.children[0].contribution,
            Contribution::NothingToFlag,
            "not measured and nothing to flag are different facts"
        );
    }

    #[test]
    fn a_failed_child_is_failed_even_with_a_projection() {
        let mut team = TaskTeamView::default();
        started(&mut team, "w1", "worker", "implement");
        finished(&mut team, "w1", false, Some(contribution(0, 0, 0, 0)));
        assert_eq!(team.children[0].status, ChildStatus::Failed);
    }

    #[test]
    fn findings_nobody_judged_are_not_engagement() {
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        finished(&mut team, "r1", true, Some(contribution(3, 0, 0, 0)));
        assert!(
            !team.children[0].contribution.engaged(),
            "reported but unjudged is noise, not contribution"
        );
    }

    #[test]
    fn a_rejection_counts_as_engagement() {
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        finished(&mut team, "r1", true, Some(contribution(2, 0, 0, 2)));
        assert!(
            team.children[0].contribution.engaged(),
            "the parent read it and decided"
        );
    }

    #[test]
    fn read_only_is_stated_from_the_capability_contract() {
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        assert!(team.children[0].is_read_only());

        let mut team2 = TaskTeamView::default();
        team2.apply_update(crate::multi_agent::ChildUpdate {
            id: "w1".into(),
            nickname: "Worker".into(),
            role: "worker".into(),
            done: false,
            ok: false,
            detail: "implement".into(),
            profile_id: Some("worker".into()),
            capabilities: vec!["read_file".into(), "apply_patch".into()],
            contribution: None,
            started_elapsed_secs: 0,
        });
        assert!(!team2.children[0].is_read_only());
    }

    #[test]
    fn open_blocking_findings_surface_at_team_level() {
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        let mut c = contribution(2, 1, 0, 0);
        c.findings_open_blocking = 1;
        finished(&mut team, "r1", true, Some(c));
        assert_eq!(team.open_blocking(), 1);
    }

    #[test]
    fn the_role_from_spawn_survives_a_finish_without_one() {
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "look");
        team.apply_update(crate::multi_agent::ChildUpdate {
            id: "a1".into(),
            nickname: "Newton".into(),
            role: String::new(),
            done: true,
            ok: true,
            detail: "done".into(),
            profile_id: None,
            capabilities: Vec::new(),
            contribution: None,
            started_elapsed_secs: 0,
        });
        assert_eq!(team.children[0].role, "explorer");
    }

    #[test]
    fn a_clean_review_reads_as_a_result_not_a_blank() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review the diff");
        finished(&mut team, "r1", true, Some(contribution(0, 0, 0, 0)));
        let line = contribution_line(&team.children[0], t).expect("a result line");
        assert!(line.contains("nothing to flag"), "{line}");
        assert!(!line.contains('0'), "zero must not be the headline: {line}");
    }

    #[test]
    fn an_unmeasured_child_says_so_rather_than_showing_zero() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        finished(&mut team, "r1", true, None);
        let line = contribution_line(&team.children[0], t).expect("a result line");
        assert!(line.contains("not measured"), "{line}");
        assert!(!line.contains('0'), "{line}");
    }

    #[test]
    fn a_reported_contribution_leads_with_what_the_parent_did() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "look");
        finished(&mut team, "a1", true, Some(contribution(7, 5, 3, 0)));
        let line = contribution_line(&team.children[0], t).expect("a result line");
        assert!(line.contains("5 accepted"), "{line}");
        assert!(line.contains("3 verified"), "{line}");
    }

    #[test]
    fn unjudged_findings_are_not_dressed_up_as_contribution() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        finished(&mut team, "r1", true, Some(contribution(3, 0, 0, 0)));
        let line = contribution_line(&team.children[0], t).expect("a result line");
        assert!(line.contains("none judged"), "{line}");
        assert!(!line.contains("accepted"), "{line}");
    }

    #[test]
    fn a_running_child_has_no_contribution_line_yet() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "look");
        assert!(contribution_line(&team.children[0], t).is_none());
    }

    #[test]
    fn waiting_explains_itself_with_the_purpose() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(
            &mut team,
            "a1",
            "explorer",
            "analyzing repository structure",
        );
        let line = running_line(&team.children[0], t);
        assert_eq!(line, "analyzing repository structure");
        assert_ne!(line, "waiting", "a bare status word explains nothing");
    }

    #[test]
    fn a_purposeless_child_falls_back_to_the_status_word() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "   ");
        assert_eq!(running_line(&team.children[0], t), "waiting");
        team.apply_progress("a1", true, 1, 1);
        assert_eq!(running_line(&team.children[0], t), "running");
    }

    use leveler_client_protocol::{UiChildContribution, UiFinding};

    fn finding(id: &str, state: &str, blocking: bool, reason: Option<&str>) -> UiFinding {
        UiFinding {
            id: id.into(),
            kind: "correctness".into(),
            summary: format!("summary {id}"),
            file: Some("src/auth.rs".into()),
            symbol: None,
            state: state.into(),
            resolution_reason: reason.map(|r| r.into()),
            blocking,
        }
    }

    fn detail(measured: bool, findings: Vec<UiFinding>) -> UiChildContribution {
        UiChildContribution {
            child_id: "r1".into(),
            role: "reviewer".into(),
            profile_id: Some("reviewer".into()),
            capabilities: vec!["code_review".into()],
            findings,
            measured,
        }
    }

    #[test]
    fn the_inspector_shows_nothing_until_something_is_loaded() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review the diff");
        assert!(
            inspector_rows(&team.children[0], t).is_none(),
            "an empty list is a claim; not-asked-yet is not"
        );
    }

    #[test]
    fn an_explorer_with_findings_shows_file_state_and_summary() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "explorer", "repository analysis");
        team.apply_detail(detail(
            true,
            vec![
                finding("f-1", "accepted", false, None),
                finding("f-2", "verified", false, None),
            ],
        ));
        let rows = inspector_rows(&team.children[0], t).expect("loaded");
        let joined = rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("src/auth.rs"), "{joined}");
        assert!(joined.contains("[accepted]"), "{joined}");
        assert!(
            joined.contains("2 accepted · 1 verified · 0 rejected"),
            "{joined}"
        );
    }

    #[test]
    fn a_reviewer_with_no_findings_reads_as_a_clean_review() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review the diff");
        team.apply_detail(detail(true, Vec::new()));
        let rows = inspector_rows(&team.children[0], t).expect("loaded");
        let joined = rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("nothing to flag"), "{joined}");
        assert!(
            !joined.contains("0 accepted"),
            "a clean review is not a tally: {joined}"
        );
    }

    #[test]
    fn a_rejected_finding_carries_the_reason_it_was_declined() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        team.apply_detail(detail(
            true,
            vec![finding(
                "f-1",
                "rejected",
                false,
                Some("covered by the existing guard"),
            )],
        ));
        let rows = inspector_rows(&team.children[0], t).expect("loaded");
        let joined = rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("covered by the existing guard"), "{joined}");
        assert!(
            joined.contains("0 accepted · 0 verified · 1 rejected"),
            "{joined}"
        );
    }

    #[test]
    fn an_unmeasured_detail_says_so_rather_than_listing_nothing() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        team.apply_detail(detail(false, Vec::new()));
        let rows = inspector_rows(&team.children[0], t).expect("loaded");
        let joined = rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("not measured"), "{joined}");
        assert!(!joined.contains("nothing to flag"), "{joined}");
    }

    #[test]
    fn an_open_blocking_finding_is_marked_but_a_resolved_one_is_not() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        team.apply_detail(detail(
            true,
            vec![
                finding("f-1", "acknowledged", true, None),
                finding("f-2", "verified", true, None),
            ],
        ));
        let rows = inspector_rows(&team.children[0], t).expect("loaded");
        let marked: Vec<_> = rows.iter().filter(|r| r.blocking).collect();
        assert_eq!(marked.len(), 1, "only the still-open one gates closure");
        assert!(marked[0].text.contains("[acknowledged]"), "{:?}", marked[0]);
    }

    #[test]
    fn unjudged_findings_are_called_out_separately() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        team.apply_detail(detail(
            true,
            vec![
                finding("f-1", "acknowledged", false, None),
                finding("f-2", "accepted", false, None),
            ],
        ));
        let rows = inspector_rows(&team.children[0], t).expect("loaded");
        let joined = rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("1 not judged"), "{joined}");
    }

    #[test]
    fn the_inspector_states_read_only_from_the_contract() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        team.apply_detail(detail(true, Vec::new()));
        let rows = inspector_rows(&team.children[0], t).expect("loaded");
        let joined = rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("read-only"), "{joined}");
    }

    #[test]
    fn a_late_response_for_an_unknown_child_is_ignored() {
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        let mut d = detail(true, Vec::new());
        d.child_id = "someone-else".into();
        team.apply_detail(d);
        assert!(team.children[0].detail.is_none());
    }

    #[test]
    fn one_child_is_not_a_team() {
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "look");
        assert!(
            !team_panel_should_show(&team),
            "a panel that says \"1 agent\" costs a line to say nothing"
        );
    }

    #[test]
    fn a_lone_reviewer_still_earns_the_panel() {
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review the diff");
        assert!(
            team_panel_should_show(&team),
            "an independent review is the thing the user most needs told about"
        );
    }

    #[test]
    fn a_working_child_shows_its_purpose_not_a_status_word() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(
            &mut team,
            "a1",
            "explorer",
            "analyzing repository structure",
        );
        started(&mut team, "w1", "worker", "implementing the change");
        let lines = team_lines(&team, t);
        assert_eq!(lines[0].glyph, "○");
        assert_eq!(lines[0].detail, "analyzing repository structure");
        assert_ne!(lines[0].detail, "waiting");
    }

    #[test]
    fn a_finished_child_shows_what_it_contributed() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "look");
        started(&mut team, "w1", "worker", "implement");
        finished(&mut team, "a1", true, Some(contribution(7, 5, 3, 0)));
        let lines = team_lines(&team, t);
        assert_eq!(lines[0].glyph, "✓");
        assert!(lines[0].detail.contains("5 accepted"), "{:?}", lines[0]);
    }

    #[test]
    fn a_clean_reviewer_reads_as_a_result_in_the_panel() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "w1", "worker", "implement");
        started(&mut team, "r1", "reviewer", "review");
        finished(&mut team, "r1", true, Some(contribution(0, 0, 0, 0)));
        let lines = team_lines(&team, t);
        let reviewer = lines
            .iter()
            .find(|l| l.status == ChildStatus::Completed)
            .unwrap();
        assert!(reviewer.detail.contains("nothing to flag"), "{reviewer:?}");
    }

    #[test]
    fn a_failed_child_is_marked_and_not_dressed_up() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "w1", "worker", "implement");
        started(&mut team, "a1", "explorer", "look");
        finished(&mut team, "w1", false, None);
        let lines = team_lines(&team, t);
        let failed = lines
            .iter()
            .find(|l| l.status == ChildStatus::Failed)
            .unwrap();
        assert_eq!(failed.glyph, "✗");
        assert!(!failed.detail.contains("not measured"), "{failed:?}");
    }

    #[test]
    fn the_title_leads_with_a_block_when_one_exists() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "w1", "worker", "implement");
        started(&mut team, "r1", "reviewer", "review");
        let mut c = contribution(2, 1, 0, 0);
        c.findings_open_blocking = 1;
        finished(&mut team, "r1", true, Some(c));
        finished(&mut team, "w1", true, Some(contribution(0, 0, 0, 0)));
        assert!(
            team_panel_title(&team, t).contains("1 blocking"),
            "a block found at the end is a block the user should have seen coming"
        );
    }

    #[test]
    fn the_title_never_leads_with_a_head_count() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        for (i, role) in ["explorer", "worker", "reviewer"].iter().enumerate() {
            started(&mut team, &format!("c{i}"), role, "work");
        }
        let title = team_panel_title(&team, t);
        assert!(!title.contains('3'), "count is not the headline: {title}");
    }

    /// Dogfood, taskC: the panel said "AI team · done" above a child marked
    /// failed. Nothing was done; something broke.
    #[test]
    fn a_failed_child_is_not_a_finished_team() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "w1", "worker", "implement");
        started(&mut team, "r1", "reviewer", "review");
        finished(&mut team, "w1", true, Some(contribution(0, 0, 0, 0)));
        finished(&mut team, "r1", false, Some(contribution(0, 0, 0, 0)));
        let title = team_panel_title(&team, t);
        assert!(
            !title.contains("done"),
            "a team with a failed child has not finished: {title}"
        );
    }

    /// Dogfood, taskC: a reviewer stopped mid-review reported zero findings and
    /// rendered as "reviewed, nothing to flag". It had not reviewed anything —
    /// it was cut off. Zero findings from an interrupted child is not a clean
    /// bill of health, it is an unfinished measurement.
    #[test]
    fn zero_findings_from_a_failed_child_is_not_a_clean_review() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review the diff");
        finished(&mut team, "r1", false, Some(contribution(0, 0, 0, 0)));
        assert_ne!(
            team.children[0].contribution,
            Contribution::NothingToFlag,
            "an interrupted reviewer did not certify anything"
        );
        let line = contribution_line(&team.children[0], t).unwrap_or_default();
        assert!(
            !line.contains("nothing to flag"),
            "a stopped reviewer must not read as a clean review: {line}"
        );
    }

    #[test]
    fn a_successful_child_with_zero_findings_still_reads_as_clean() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review");
        finished(&mut team, "r1", true, Some(contribution(0, 0, 0, 0)));
        let line = contribution_line(&team.children[0], t).unwrap_or_default();
        assert!(line.contains("nothing to flag"), "{line}");
    }

    #[test]
    fn active_lists_only_children_still_working() {
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "look");
        started(&mut team, "w1", "worker", "implement");
        finished(&mut team, "a1", true, Some(contribution(1, 1, 0, 0)));
        let active: Vec<_> = team.active().map(|c| c.id.clone()).collect();
        assert_eq!(active, vec!["w1"]);
    }
}

/// One line describing what a child contributed, for the renderer.
///
/// Deliberately here and not in the renderer: "reviewed, nothing to flag" and
/// "not measured" are product statements, and putting them next to the type
/// that distinguishes them keeps them from drifting apart.
pub fn contribution_line(view: &ChildAgentView, t: &crate::i18n::UiText) -> Option<String> {
    match &view.contribution {
        Contribution::Pending => None,
        Contribution::NotMeasured => Some(t.child_contribution_unmeasured.to_string()),
        Contribution::NothingToFlag => Some(t.child_contribution_clean.to_string()),
        Contribution::Incomplete { reported } => Some(
            t.child_contribution_incomplete
                .replace("{n}", &reported.to_string()),
        ),
        Contribution::Reported {
            total,
            accepted,
            verified,
            rejected,
            ..
        } => {
            // Lead with what the parent did, not with volume. A finding nobody
            // judged is the one number that does not belong in front.
            let judged = accepted + rejected;
            if judged == 0 {
                return Some(
                    t.child_contribution_unjudged
                        .replace("{n}", &total.to_string()),
                );
            }
            Some(
                t.child_contribution_reported
                    .replace("{n}", &total.to_string())
                    .replace("{accepted}", &accepted.to_string())
                    .replace("{verified}", &verified.to_string()),
            )
        }
    }
}

/// Why this child is running, for the line the user watches. Falls back to the
/// status word only when the runtime gave no purpose.
pub fn running_line(view: &ChildAgentView, t: &crate::i18n::UiText) -> String {
    if view.purpose.trim().is_empty() {
        return match view.status {
            ChildStatus::Running => t.sub_agent_running.to_string(),
            _ => t.sub_agent_waiting.to_string(),
        };
    }
    view.purpose.trim().to_string()
}

/// Contribution line for a transcript block.
///
/// The block carries the projection directly, so this is the same statement as
/// [`contribution_line`] without needing the whole team view.
pub fn contribution_line_for_block(
    block: &crate::transcript::SubAgentBlock,
    t: &crate::i18n::UiText,
) -> Option<String> {
    match &block.contribution {
        Contribution::Pending => None,
        Contribution::NotMeasured => Some(t.child_contribution_unmeasured.to_string()),
        Contribution::NothingToFlag => Some(t.child_contribution_clean.to_string()),
        Contribution::Incomplete { reported } => Some(
            t.child_contribution_incomplete
                .replace("{n}", &reported.to_string()),
        ),
        Contribution::Reported {
            total,
            accepted,
            verified,
            rejected,
            ..
        } => {
            if accepted + rejected == 0 {
                return Some(
                    t.child_contribution_unjudged
                        .replace("{n}", &total.to_string()),
                );
            }
            Some(
                t.child_contribution_reported
                    .replace("{n}", &total.to_string())
                    .replace("{accepted}", &accepted.to_string())
                    .replace("{verified}", &verified.to_string()),
            )
        }
    }
}

/// One rendered row of the Contribution Inspector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectorRow {
    pub text: String,
    /// Findings that still gate closure are marked so the eye finds them.
    pub blocking: bool,
}

/// Inspector body for one child.
///
/// Returns `None` when nothing has been loaded yet — the caller shows a
/// loading state rather than an empty list, because an empty list is a claim
/// and "not asked yet" is not one.
pub fn inspector_rows(view: &ChildAgentView, t: &crate::i18n::UiText) -> Option<Vec<InspectorRow>> {
    let detail = view.detail.as_ref()?;
    let mut rows = Vec::new();

    rows.push(InspectorRow {
        text: format!("{}: {}", t.inspector_purpose, running_line(view, t)),
        blocking: false,
    });
    if let Some(profile) = detail.profile_id.as_deref() {
        let access = if view.is_read_only() {
            t.inspector_read_only
        } else {
            t.inspector_can_write
        };
        rows.push(InspectorRow {
            text: format!("{}: {profile} · {access}", t.inspector_profile),
            blocking: false,
        });
    }

    if !detail.measured {
        // The question could not be answered. Saying so beats an empty list,
        // which would read as "found nothing".
        rows.push(InspectorRow {
            text: t.child_contribution_unmeasured.to_string(),
            blocking: false,
        });
        return Some(rows);
    }

    if detail.findings.is_empty() {
        // A clean review is a result. It gets a sentence, not blank space.
        rows.push(InspectorRow {
            text: t.child_contribution_clean.to_string(),
            blocking: false,
        });
        return Some(rows);
    }

    for f in &detail.findings {
        let mut text = String::new();
        if let Some(file) = f.file.as_deref() {
            text.push_str(file);
            if let Some(sym) = f.symbol.as_deref() {
                text.push_str("::");
                text.push_str(sym);
            }
            text.push_str(" — ");
        }
        text.push_str(&f.summary);
        text.push_str(&format!(" [{}]", f.state));
        // A rejection is a judgement, and a judgement without its reason is
        // indistinguishable from being ignored.
        if let Some(reason) = f.resolution_reason.as_deref() {
            text.push_str(&format!(" · {reason}"));
        }
        rows.push(InspectorRow {
            text,
            blocking: f.blocking && f.state != "verified" && f.state != "rejected",
        });
    }

    rows.push(InspectorRow {
        text: t
            .inspector_summary
            .replace("{accepted}", &detail.accepted().to_string())
            .replace("{verified}", &detail.verified().to_string())
            .replace("{rejected}", &detail.rejected().to_string()),
        blocking: false,
    });
    let unjudged = detail.unjudged();
    if unjudged > 0 {
        rows.push(InspectorRow {
            text: t.inspector_unjudged.replace("{n}", &unjudged.to_string()),
            blocking: false,
        });
    }
    Some(rows)
}

/// One line of the Task Team header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamLine {
    pub glyph: &'static str,
    pub role: String,
    /// Purpose while working, contribution once finished. Never a bare status
    /// word — the whole point of the panel is that waiting is explicable.
    pub detail: String,
    pub status: ChildStatus,
}

/// Should the Task Team panel appear at all?
///
/// One child is not a team, and a panel that says "1 agent" costs a line of a
/// terminal to tell the user something the transcript already shows.
pub fn team_panel_should_show(team: &TaskTeamView) -> bool {
    team.children.len() >= 2 || team.children.iter().any(|c| c.role == "reviewer")
}

/// Compact team summary for the header.
///
/// Deliberately not a dashboard: role, state and *why*. No token counts, no
/// event stream, no agent graph. The task is the primary object; this is a
/// caption on it.
pub fn team_lines(team: &TaskTeamView, t: &crate::i18n::UiText) -> Vec<TeamLine> {
    team.children
        .iter()
        .map(|c| {
            let glyph = match c.status {
                ChildStatus::Waiting => "○",
                ChildStatus::Running => "⟳",
                ChildStatus::Completed => "✓",
                ChildStatus::Failed => "✗",
            };
            let detail = match c.status {
                ChildStatus::Waiting | ChildStatus::Running => running_line(c, t),
                ChildStatus::Failed => t.sub_agent_incomplete.to_string(),
                ChildStatus::Completed => {
                    contribution_line(c, t).unwrap_or_else(|| t.sub_agent_completed.to_string())
                }
            };
            TeamLine {
                glyph,
                role: display_role(&c.role, t),
                detail,
                status: c.status,
            }
        })
        .collect()
}

/// Title for the team panel. Names what the team is doing, not how many of
/// them there are — "3 agents" is the count the product deliberately does not
/// lead with.
pub fn team_panel_title(team: &TaskTeamView, t: &crate::i18n::UiText) -> String {
    let blocking = team.open_blocking();
    if blocking > 0 {
        return t.team_panel_blocking.replace("{n}", &blocking.to_string());
    }
    if team.active().next().is_some() {
        return t.team_panel_working.to_string();
    }
    // A team with a failed child has not finished. Saying "done" over a ✗ is
    // the panel contradicting the line directly beneath it.
    if team
        .children
        .iter()
        .any(|c| c.status == ChildStatus::Failed)
    {
        return t.team_panel_incomplete.to_string();
    }
    t.team_panel_done.to_string()
}

fn display_role(role: &str, t: &crate::i18n::UiText) -> String {
    match role {
        "explorer" => t.sub_agent_explorer,
        "worker" => t.sub_agent_worker,
        "reviewer" => t.sub_agent_reviewer,
        _ => t.sub_agent_default,
    }
    .to_string()
}
