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
    /// Its activation died with a runtime window; the runtime continues it or
    /// settles it as lost. Not running, not finished.
    Interrupted,
    /// Its turn ended and no terminal reached this view. The UI holds no fact
    /// about how it ended; a later terminal or snapshot says.
    Unreported,
    Completed,
    Failed,
}

impl ChildStatus {
    /// Whether the child has reached an outcome. A terminal child is history
    /// once its turn ends; an open one (`Running`, `Waiting`, `Interrupted`,
    /// `Unreported`) may still be continued or settled by a later turn.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
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
    /// It reported this many findings. What the parent made of them is in
    /// the transcript, in the parent's own words — the runtime no longer
    /// tracks a per-finding verdict, so neither does this.
    Reported { total: u32 },
}

/// One child, as the user should understand it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildAgentView {
    pub id: String,
    pub nickname: String,
    pub role: String,
    /// Built-in capability contract. `None` means the runtime recorded none.
    pub profile_id: Option<String>,
    /// The declarative agent it was spawned from (`security-reviewer`), when
    /// it was. `None` for a built-in role spawn and for older records.
    pub agent_name: Option<String>,
    /// Whether this child holds a physically read-only toolset.
    pub read_only: bool,
    /// The child's short task title, fixed at spawn. `None` for children
    /// recorded before titles existed; renderers project `purpose` then.
    pub title: Option<String>,
    /// What it was asked to do. Leads the running line: a user watching a
    /// spinner needs the reason, not the state.
    pub purpose: String,
    pub status: ChildStatus,
    pub contribution: Contribution,
    /// Latest tool step, when the runtime reported one.
    pub recent_step: Option<String>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// How the activation ended, once it has. `None` while running and for
    /// terminals recorded before it was typed.
    pub stop: Option<leveler_client_protocol::ChildStop>,
    /// Which bound fired when `stop` is a budget stop, once the runtime typed
    /// one. Read, never inferred from the summary text.
    pub limit: Option<leveler_client_protocol::ChildLimit>,
    pub started_elapsed_secs: u64,
    /// Elapsed when this child settled, once it has. A finished row shows the
    /// time it took; without the stamp it followed the turn clock, so a
    /// hundred-second child read "7m 10s" ten minutes later, beside its own ✓.
    pub settled_elapsed_secs: Option<u64>,
    /// Findings loaded on demand by the Contribution Inspector.
    ///
    /// `None` means nobody has asked yet — not that there are none. The
    /// inspector queries when the user opens a detail view, because findings
    /// are ledger facts and streaming them would duplicate the record.
    pub detail: Option<leveler_client_protocol::UiChildContribution>,
    /// Bounded projection of [`SubAgentActivity`] calls, oldest first.
    ///
    /// The child protocol carries a reduced shape (tool name, an arguments
    /// preview, a result preview and an error bit) rather than a full
    /// [`crate::transcript::ToolCallBlock`]; this is the typed read model the
    /// activity presenter consumes. It is a projection of runtime events, not
    /// a second lifecycle.
    pub activity: Vec<ChildActivityCall>,
}

impl ChildAgentView {
    /// True when this child can only be described by its bounds, not by what
    /// it produced — the honest state for a failed or unmeasured child.
    pub fn contribution_unknown(&self) -> bool {
        matches!(self.contribution, Contribution::NotMeasured)
    }

    /// Read-only children can be stated as such rather than implied.
    ///
    /// This used to scan a list of semantic capability labels for
    /// "write"/"edit"/"apply_patch" — none of which a label ever was, so every
    /// profiled child, Workers included, read as read-only. The runtime now
    /// sends the bound it enforces.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }
}

/// One delegated tool invocation, as the runtime reported it over
/// `SubAgentActivity`.
///
/// `tool_started` opens the call with its arguments preview; the matching
/// `tool_finished` closes it with the result preview and the error bit. A
/// child's tool calls are sequential, so the close pairs with the most recent
/// still-running call of the same tool — order is the pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildActivityCall {
    pub tool: String,
    /// The `tool_started` arguments preview (the runtime may have truncated
    /// it). Empty for a call first seen on `tool_finished` (a reconnect).
    pub arguments: String,
    /// The `tool_finished` result preview, once it arrived.
    pub preview: Option<String>,
    pub status: crate::transcript::ToolStatus,
}

/// Cap on the per-child activity projection. Bounded so a long-running child
/// cannot grow the view model without bound; the newest calls are kept.
const CHILD_ACTIVITY_CAP: usize = 48;

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
    /// The child's short task title, carried on the start only.
    pub title: Option<String>,
    pub profile_id: Option<String>,
    pub agent_name: Option<String>,
    pub read_only: bool,
    /// `None` means the runtime produced no projection — not measured.
    pub contribution: Option<ChildContribution>,
    /// How the activation ended, when it did. Read, never inferred.
    pub stop: Option<leveler_client_protocol::ChildStop>,
    /// Which bound fired when `stop` is a budget stop. Distinguishes the
    /// wall-clock cap from a spent token or cost budget, so the surface can
    /// say the actual reason.
    pub limit: Option<leveler_client_protocol::ChildLimit>,
    pub started_elapsed_secs: u64,
}

/// The team working on one task.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskTeamView {
    pub children: Vec<ChildAgentView>,
    /// Turn-elapsed seconds at the moment the LAST active child settled.
    /// Presentation-only, and deliberately not persisted anywhere: it exists
    /// so the runtime surface can show a brief terminal summary and then get
    /// out of the way. Cleared whenever a child becomes active again.
    pub settled_at_elapsed: Option<u64>,
}

/// How long the terminal collaboration summary stays on screen after the last
/// child settles, before the surface disappears entirely.
pub const COLLABORATION_TERMINAL_SECS: u64 = 6;

impl TaskTeamView {
    /// Children still working.
    pub fn active(&self) -> impl Iterator<Item = &ChildAgentView> {
        self.children
            .iter()
            .filter(|c| matches!(c.status, ChildStatus::Running | ChildStatus::Waiting))
    }

    /// Apply one `SubAgentUpdated`. Upserts by id so a child transitions in
    /// place rather than appearing twice.
    /// Re-derive the terminal stamp after any child-state change.
    ///
    /// Active again → the surface is live, so clear the stamp. Just went
    /// fully settled → stamp it so the terminal summary can age out. Already
    /// stamped → leave it, or the summary would restart on every event.
    fn restamp_settlement(&mut self, now_elapsed: u64) {
        if self.children.is_empty() {
            self.settled_at_elapsed = None;
            return;
        }
        if self.active().next().is_some() {
            self.settled_at_elapsed = None;
        } else if self.settled_at_elapsed.is_none() {
            self.settled_at_elapsed = Some(now_elapsed);
        }
    }

    /// Whether the runtime surface should be on screen at all.
    ///
    /// This is an ACTIVITY question, not a composition one: a team that
    /// merely exists is history, and history belongs to Task Detail. The
    /// surface is live while anyone is working or blocking, stays briefly
    /// after the last child settles, and then goes away.
    pub fn surface_visible(&self, now_elapsed: u64) -> bool {
        if self.children.is_empty() {
            return false;
        }
        if self.active().next().is_some() {
            return true;
        }
        match self.settled_at_elapsed {
            None => true,
            // A clock that went BACKWARDS (the turn clock resets when the
            // session goes idle) means the turn that stamped this has ended:
            // the linger is over, not restarted.
            Some(at) => now_elapsed >= at && now_elapsed - at < COLLABORATION_TERMINAL_SECS,
        }
    }

    /// Whether the surface is showing its brief post-settlement summary.
    pub fn surface_is_terminal(&self, now_elapsed: u64) -> bool {
        self.surface_visible(now_elapsed) && self.active().next().is_none()
    }

    pub fn apply_update(&mut self, update: ChildUpdate) {
        let ChildUpdate {
            id,
            nickname,
            role,
            done,
            ok,
            detail,
            title,
            profile_id,
            agent_name,
            read_only,
            contribution,
            stop,
            limit,
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
            // A finish event carries no purpose or title; keep the ones from
            // the spawn.
            if !done {
                existing.purpose = detail;
            }
            if title.is_some() {
                existing.title = title;
            }
            existing.status = status;
            // The first settlement owns the clock: a repeated finish event
            // must not re-stamp it later.
            if done {
                existing
                    .settled_elapsed_secs
                    .get_or_insert(started_elapsed_secs);
            }
            existing.contribution = contribution;
            existing.stop = stop;
            if limit.is_some() {
                existing.limit = limit;
            }
            if !role.is_empty() {
                existing.role = role;
            }
            if profile_id.is_some() {
                existing.profile_id = profile_id;
            }
            if agent_name.is_some() {
                existing.agent_name = agent_name;
            }
            existing.read_only = read_only;
            self.restamp_settlement(started_elapsed_secs);
            return;
        }
        self.children.push(ChildAgentView {
            id,
            nickname,
            role,
            profile_id,
            agent_name,
            read_only,
            title,
            purpose: detail,
            status,
            contribution,
            recent_step: None,
            input_tokens: 0,
            output_tokens: 0,
            stop,
            limit,
            started_elapsed_secs,
            settled_elapsed_secs: done.then_some(started_elapsed_secs),
            detail: None,
            activity: Vec::new(),
        });
        self.restamp_settlement(started_elapsed_secs);
    }

    /// Take the children the runtime recorded (a snapshot on open or
    /// reconnect). Each is upserted by id: a child this view already follows
    /// keeps its live-only detail (steps, loaded findings) and takes the
    /// recorded state; one it never saw is added as recorded.
    pub fn restore(
        &mut self,
        children: &[leveler_client_protocol::UiChildAgent],
        now_elapsed: u64,
    ) {
        use leveler_client_protocol::{ChildOutcome, UiChildState};
        for recorded in children {
            let status = match recorded.state {
                UiChildState::Running => ChildStatus::Waiting,
                UiChildState::Interrupted => ChildStatus::Interrupted,
                UiChildState::Settled => match recorded.outcome {
                    Some(
                        ChildOutcome::CompletedWithFindings | ChildOutcome::CompletedNoFindings,
                    ) => ChildStatus::Completed,
                    Some(_) => ChildStatus::Failed,
                    // Settled before the outcome was typed: only the ok bit.
                    None if recorded.ok => ChildStatus::Completed,
                    None => ChildStatus::Failed,
                },
            };
            let input = u32::try_from(recorded.input_tokens).unwrap_or(u32::MAX);
            let output = u32::try_from(recorded.output_tokens).unwrap_or(u32::MAX);
            if let Some(existing) = self.children.iter_mut().find(|c| c.id == recorded.id) {
                // A terminal already applied is final: a snapshot taken before
                // it still says running and must not reopen the child. And a
                // live Running is finer than the record's Running.
                let settled_here = existing.status.is_terminal();
                let finer_live =
                    status == ChildStatus::Waiting && existing.status == ChildStatus::Running;
                if !settled_here && !finer_live {
                    existing.status = status;
                    existing.stop = recorded.stop;
                    existing.limit = recorded.limit;
                }
                existing.input_tokens = existing.input_tokens.max(input);
                existing.output_tokens = existing.output_tokens.max(output);
                continue;
            }
            // Settled history belongs to the transcript, not the live team:
            // only children the runtime still has open join it.
            if recorded.state == UiChildState::Settled {
                continue;
            }
            self.children.push(ChildAgentView {
                id: recorded.id.clone(),
                nickname: recorded.nickname.clone(),
                role: recorded.role.clone(),
                profile_id: recorded.profile_id.clone(),
                agent_name: recorded.agent.as_ref().map(|a| a.name.clone()),
                read_only: recorded.read_only,
                title: recorded.title.clone(),
                purpose: recorded.purpose.clone(),
                status,
                contribution: Contribution::Pending,
                recent_step: None,
                input_tokens: input,
                output_tokens: output,
                stop: None,
                // The runtime holds no bound for a child still open.
                limit: None,
                started_elapsed_secs: now_elapsed,
                // Restored children are still open by the branch above; a
                // settled one never joins the live team.
                settled_elapsed_secs: None,
                detail: None,
                activity: Vec::new(),
            });
        }
        self.restamp_settlement(now_elapsed);
    }

    /// The runtime moved a child without a start or terminal: its activation
    /// died (interrupted) or a new one began (running). A settled child stays
    /// settled — a terminal is final.
    pub fn apply_state(
        &mut self,
        id: &str,
        state: leveler_client_protocol::UiChildState,
        now_elapsed: u64,
    ) {
        use leveler_client_protocol::UiChildState;
        if let Some(c) = self.children.iter_mut().find(|c| c.id == id) {
            match (state, c.status) {
                (
                    UiChildState::Interrupted,
                    ChildStatus::Waiting | ChildStatus::Running | ChildStatus::Unreported,
                ) => {
                    c.status = ChildStatus::Interrupted;
                }
                (UiChildState::Running, ChildStatus::Interrupted | ChildStatus::Unreported) => {
                    c.status = ChildStatus::Waiting;
                }
                _ => return,
            }
        }
        self.restamp_settlement(now_elapsed);
    }

    /// Retire the previous turn's settled children when a new turn begins.
    ///
    /// A terminal child ([`ChildStatus::is_terminal`]) belongs to the turn that
    /// owned it: once that turn is over the child is history. The transcript
    /// keeps it, and the live team must not — [`Self::restore`] refuses to
    /// restore a settled child for the same reason. A child still open at the
    /// boundary stays: the runtime continues or settles it in a later turn, so
    /// it is cross-turn background activity, not history.
    ///
    /// Returns whether anything was retired, so the caller never pays for a
    /// second look at an unchanged roster.
    pub fn retire_settled(&mut self, now_elapsed: u64) -> bool {
        let before = self.children.len();
        self.children.retain(|c| !c.status.is_terminal());
        if self.children.len() == before {
            return false;
        }
        self.restamp_settlement(now_elapsed);
        true
    }

    /// A turn ended: a child this view still shows as working got no terminal
    /// here. Say exactly that — not failed, not interrupted.
    pub fn mark_unreported_at_turn_end(&mut self, now_elapsed: u64) {
        let mut changed = false;
        for c in &mut self.children {
            if matches!(c.status, ChildStatus::Running | ChildStatus::Waiting) {
                c.status = ChildStatus::Unreported;
                changed = true;
            }
        }
        if changed {
            self.restamp_settlement(now_elapsed);
        }
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

    /// Record one `SubAgentActivity` event for a running sub-agent.
    ///
    /// A `tool_started` opens a call with its arguments preview; the matching
    /// `tool_finished` closes the most recent still-running call of the same
    /// tool with the result preview and the error bit. `recent_step` keeps the
    /// compact one-word line the transcript head already uses, so this is
    /// purely an enrichment of the same event — never a second lifecycle.
    pub fn apply_activity(
        &mut self,
        id: &str,
        phase: &str,
        tool: &str,
        preview: &str,
        is_error: bool,
    ) {
        use crate::transcript::ToolStatus;
        let Some(c) = self.children.iter_mut().find(|c| c.id == id) else {
            return;
        };
        if phase == "tool_finished" {
            // A call that ran is not an outcome that succeeded: the terminal
            // carries its own status. `recent_step` reads it back to the head.
            c.recent_step = Some(if is_error {
                format!("{tool} ✗")
            } else {
                format!("{tool} ✓")
            });
            let closed = c
                .activity
                .iter_mut()
                .rev()
                .find(|call| call.status == ToolStatus::Running && call.tool == tool);
            if let Some(call) = closed {
                call.status = if is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Ok
                };
                call.preview = Some(preview.to_string());
                return;
            }
            // No live start for this terminal (a reconnect): keep the fact
            // instead of dropping it. It has no arguments to show.
            c.activity.push(ChildActivityCall {
                tool: tool.to_string(),
                arguments: String::new(),
                preview: Some(preview.to_string()),
                status: if is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Ok
                },
            });
        } else {
            c.recent_step = Some(tool.to_string());
            c.activity.push(ChildActivityCall {
                tool: tool.to_string(),
                arguments: preview.to_string(),
                preview: None,
                status: ToolStatus::Running,
            });
        }
        if c.activity.len() > CHILD_ACTIVITY_CAP {
            let excess = c.activity.len() - CHILD_ACTIVITY_CAP;
            c.activity.drain(0..excess);
        }
    }
}

/// The words for a non-completed stop the runtime typed. `None` for a stop
/// that has no more specific word than "incomplete".
pub fn stop_label(
    stop: Option<leveler_client_protocol::ChildStop>,
    t: &crate::i18n::UiText,
) -> Option<&'static str> {
    use leveler_client_protocol::ChildStop;
    match stop? {
        ChildStop::Cancelled => Some(t.sub_agent_cancelled),
        ChildStop::Lost => Some(t.sub_agent_lost),
        ChildStop::Budget => Some(t.sub_agent_budget),
        ChildStop::Completed | ChildStop::Incomplete | ChildStop::Failed => None,
    }
}

/// The same words, using the bound the runtime typed beside the stop.
///
/// A wall-clock cap and a spent token budget are different events: the child
/// that ran out of time is `超时`/`timeout`, the one that ran out of tokens is
/// `预算耗尽`/`budget exhausted`. The generic label is only for terminals that
/// carry no bound (older records, or a stop the runtime did not type).
pub fn child_stop_label(
    stop: Option<leveler_client_protocol::ChildStop>,
    limit: Option<leveler_client_protocol::ChildLimit>,
    t: &crate::i18n::UiText,
) -> Option<&'static str> {
    use leveler_client_protocol::{ChildLimit, ChildStop};
    if stop == Some(ChildStop::Budget) && limit == Some(ChildLimit::Duration) {
        return Some(t.agent_status_timeout);
    }
    stop_label(stop, t)
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
    }
}

/// How a child is named on screen: its nickname, and the declared agent it
/// runs as when it has one (`Euclid · rust-reviewer`).
pub(crate) fn child_label(nickname: &str, agent_name: Option<&str>) -> String {
    match agent_name.filter(|a| !a.trim().is_empty()) {
        Some(agent) => format!("{nickname} · {agent}"),
        None => nickname.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three children spawned into one turn printed "子 Agent" three times in
    /// the roster — the same generic role — while the activity lane above named
    /// every one of them. A row nobody can identify cannot say whose command
    /// just failed.
    #[test]
    fn the_roster_names_each_child_the_way_the_lane_does() {
        let mut team = TaskTeamView::default();
        for (id, nickname) in [("c1", "Euclid"), ("c2", "Newton"), ("c3", "Curie")] {
            team.apply_update(ChildUpdate {
                id: id.into(),
                nickname: nickname.into(),
                role: "default".into(),
                done: false,
                ok: false,
                detail: "任务".into(),
                title: None,
                profile_id: None,
                agent_name: None,
                read_only: false,
                contribution: None,
                stop: None,
                limit: None,
                started_elapsed_secs: 0,
            });
        }
        let t = crate::i18n::Locale::Zh.text();
        let labels: Vec<String> = roster_rows(&team, 10, t)
            .into_iter()
            .map(|r| r.label)
            .collect();
        assert_eq!(labels, vec!["Euclid", "Newton", "Curie"], "{labels:?}");

        // One child of a role needs no disambiguation.
        let mut single = TaskTeamView::default();
        single.apply_update(ChildUpdate {
            id: "only".into(),
            nickname: "Euclid".into(),
            role: "reviewer".into(),
            done: false,
            ok: false,
            detail: "复核".into(),
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: true,
            contribution: None,
            stop: None,
            limit: None,
            started_elapsed_secs: 0,
        });
        let labels: Vec<String> = roster_rows(&single, 10, t)
            .into_iter()
            .map(|r| r.label)
            .collect();
        assert_eq!(labels, vec!["Euclid"], "{labels:?}");
    }

    /// A settled team that lost a child must not wear the success mark.
    /// The corpus replay caught eight real frames saying `✓ … 1 项未完成`.
    #[test]
    fn a_team_that_lost_a_child_does_not_wear_the_success_mark() {
        let lost = team_with(&[ChildStatus::Completed, ChildStatus::Failed]);
        assert_eq!(
            collaboration_glyph(&lost, true),
            "\u{26a0}",
            "settled with a loss"
        );
        let clean = team_with(&[ChildStatus::Completed, ChildStatus::Completed]);
        assert_eq!(
            collaboration_glyph(&clean, true),
            "\u{2713}",
            "settled, nothing lost"
        );
        let working = team_with(&[ChildStatus::Running, ChildStatus::Failed]);
        assert_eq!(
            collaboration_glyph(&working, false),
            "\u{25c9}",
            "still working"
        );
    }

    /// The spawn title is task identity: it arrives on the start event, a
    /// terminal carries none and must not erase it, and a snapshot restores it.
    #[test]
    fn a_childs_spawn_title_is_kept_across_its_terminal() {
        let mut team = TaskTeamView::default();
        team.apply_update(ChildUpdate {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            title: Some("调查 Windows CI 两个 flaky tests".into()),
            done: false,
            ok: false,
            detail: "the full instructions".into(),
            profile_id: None,
            agent_name: None,
            read_only: true,
            contribution: None,
            stop: None,
            limit: None,
            started_elapsed_secs: 0,
        });
        assert_eq!(
            team.children[0].title.as_deref(),
            Some("调查 Windows CI 两个 flaky tests")
        );
        assert_eq!(team.children[0].purpose, "the full instructions");

        // A terminal carries no title; it must keep the spawn's.
        team.apply_update(ChildUpdate {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: String::new(),
            title: None,
            done: true,
            ok: true,
            detail: String::new(),
            profile_id: None,
            agent_name: None,
            read_only: true,
            contribution: None,
            stop: None,
            limit: None,
            started_elapsed_secs: 5,
        });
        assert_eq!(
            team.children[0].title.as_deref(),
            Some("调查 Windows CI 两个 flaky tests")
        );

        // A snapshot (replay) restores it from the runtime's own record.
        let mut restored = TaskTeamView::default();
        let mut child = recorded("c2", leveler_client_protocol::UiChildState::Running, false);
        child.title = Some("audit the refund path".into());
        restored.restore(&[child], 0);
        assert_eq!(
            restored.children[0].title.as_deref(),
            Some("audit the refund path")
        );
    }

    fn recorded(
        id: &str,
        state: leveler_client_protocol::UiChildState,
        ok: bool,
    ) -> leveler_client_protocol::UiChildAgent {
        leveler_client_protocol::UiChildAgent {
            id: id.into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            profile_id: None,
            read_only: true,
            title: None,
            purpose: "look".into(),
            agent: None,
            state,
            ok,
            background: true,
            scope: Vec::new(),
            resumes: 0,
            outcome: None,
            stop: None,
            limit: None,
            summary: None,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd_micros: None,
        }
    }

    /// MA3 review M1: a terminal already applied is final; a snapshot taken
    /// before it (which still says running) must not reopen the child.
    #[test]
    fn a_stale_snapshot_does_not_reopen_a_settled_child() {
        let mut team = team_with(&[ChildStatus::Completed]);
        let id = team.children[0].id.clone();
        team.restore(
            &[recorded(
                &id,
                leveler_client_protocol::UiChildState::Running,
                false,
            )],
            5,
        );
        assert_eq!(team.children[0].status, ChildStatus::Completed);
    }

    /// MA3 review L1 + L4: a settled record restores nothing into the live
    /// team (history is the transcript's), and an old settled row without an
    /// outcome is read by its ok bit when it updates a child this view holds.
    #[test]
    fn restore_brings_back_open_children_only_and_reads_ok_for_old_rows() {
        use leveler_client_protocol::UiChildState;
        let mut team = TaskTeamView::default();
        team.restore(
            &[
                recorded("old", UiChildState::Settled, true),
                recorded("open", UiChildState::Interrupted, false),
            ],
            0,
        );
        assert_eq!(team.children.len(), 1, "settled history is not live team");
        assert_eq!(team.children[0].id, "open");

        let mut live = team_with(&[ChildStatus::Running]);
        let id = live.children[0].id.clone();
        live.restore(&[recorded(&id, UiChildState::Settled, true)], 5);
        assert_eq!(
            live.children[0].status,
            ChildStatus::Completed,
            "ok=true without outcome is completed"
        );
    }

    /// U2: the turn clock resets to 0 when the session goes idle. A settled
    /// team stamped at 10s must not read "0 - 10 saturates to 0, still under
    /// the linger" and stay on screen for the whole idle period.
    #[test]
    fn a_settled_team_leaves_the_surface_when_the_turn_clock_resets() {
        let mut team = team_with(&[ChildStatus::Completed]);
        team.settled_at_elapsed = Some(10);
        assert!(team.surface_visible(12), "inside the linger");
        assert!(
            !team.surface_visible(10 + COLLABORATION_TERMINAL_SECS),
            "aged out"
        );
        assert!(
            !team.surface_visible(0),
            "the clock reset: the linger is over"
        );
    }

    fn team_with(statuses: &[ChildStatus]) -> TaskTeamView {
        let mut team = TaskTeamView::default();
        for (i, st) in statuses.iter().enumerate() {
            team.children.push(ChildAgentView {
                id: format!("c{i}"),
                nickname: String::new(),
                role: if i == 0 {
                    "探索 Agent".into()
                } else {
                    "审查 Agent".into()
                },
                profile_id: None,
                agent_name: None,
                read_only: false,
                title: None,
                purpose: "look around".into(),
                status: *st,
                contribution: Contribution::Pending,
                recent_step: None,
                input_tokens: 0,
                output_tokens: 0,
                started_elapsed_secs: 0,
                settled_elapsed_secs: None,
                detail: None,
                activity: Vec::new(),
                stop: None,
                limit: None,
            });
        }
        team
    }

    /// The runtime surface answers "who is working NOW". A team that merely
    /// exists is history: visible while active, visible briefly as a terminal
    /// summary after the last child settles, gone afterwards — and back the
    /// moment a child becomes active again.
    #[test]
    fn collaboration_surface_follows_activity_not_existence() {
        let mut team = team_with(&[ChildStatus::Running, ChildStatus::Waiting]);
        assert!(team.surface_visible(100), "active children → visible");
        assert!(!team.surface_is_terminal(100));

        team.children[0].status = ChildStatus::Completed;
        team.children[1].status = ChildStatus::Completed;
        team.restamp_settlement(120);
        assert!(
            team.surface_visible(121),
            "terminal summary lingers briefly"
        );
        assert!(team.surface_is_terminal(121));
        assert!(
            !team.surface_visible(120 + COLLABORATION_TERMINAL_SECS),
            "then the surface leaves — history belongs to Task Detail"
        );

        // A new child re-activates the surface and clears the stamp.
        team.children[0].status = ChildStatus::Running;
        team.restamp_settlement(130);
        assert_eq!(team.settled_at_elapsed, None);
        assert!(team.surface_visible(500));

        assert!(
            !TaskTeamView::default().surface_visible(0),
            "no children, no surface"
        );
    }

    /// A new turn owns the live roster: the previous turn's settled children
    /// are history once it begins, while a child still open at the boundary
    /// stays as cross-turn background activity.
    #[test]
    fn a_new_turn_retires_settled_children_and_keeps_open_ones() {
        let mut team = team_with(&[
            ChildStatus::Completed,
            ChildStatus::Failed,
            ChildStatus::Running,
            ChildStatus::Waiting,
            ChildStatus::Interrupted,
            ChildStatus::Unreported,
        ]);
        assert!(team.retire_settled(0));
        let kept: Vec<ChildStatus> = team.children.iter().map(|c| c.status).collect();
        assert_eq!(
            kept,
            vec![
                ChildStatus::Running,
                ChildStatus::Waiting,
                ChildStatus::Interrupted,
                ChildStatus::Unreported
            ],
            "terminal outranks nothing: an open child is background, not history"
        );
        assert_eq!(
            team.settled_at_elapsed, None,
            "an open child re-opens the surface"
        );

        let mut all_settled = team_with(&[ChildStatus::Completed, ChildStatus::Failed]);
        assert!(all_settled.retire_settled(10));
        assert!(all_settled.children.is_empty());
        assert_eq!(all_settled.settled_at_elapsed, None);

        // Nothing settled → nothing changes, and the caller is told so.
        let mut open = team_with(&[ChildStatus::Running]);
        assert!(!open.retire_settled(10));
        assert_eq!(open.children.len(), 1);
    }

    /// The compact row never words a lost child as success.
    #[test]
    fn compact_line_is_truthful_about_failure() {
        let t = crate::i18n::Locale::Zh.text();
        let mut team = team_with(&[ChildStatus::Running, ChildStatus::Waiting]);
        let row = collaboration_compact_line(&team, t);
        assert!(row.contains("2 个 Agent"), "{row}");
        assert!(row.contains("探索 Agent"), "{row}");

        team.children[0].status = ChildStatus::Completed;
        team.children[1].status = ChildStatus::Completed;
        let row = collaboration_compact_line(&team, t);
        assert!(row.contains("已完成"), "{row}");

        team.children[1].status = ChildStatus::Failed;
        let row = collaboration_compact_line(&team, t);
        assert!(
            !row.contains("已完成"),
            "a failed child is not 已完成: {row}"
        );
        assert!(row.contains("未完成"), "{row}");
    }

    fn contribution(total: u32) -> ChildContribution {
        ChildContribution {
            role: "reviewer".into(),
            profile_id: Some("reviewer".into()),
            profile_role: Some("reviewer".into()),
            read_only: true,
            findings_total: total,
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
            title: None,
            profile_id: Some(role.into()),
            agent_name: None,
            read_only: true,
            contribution: None,
            started_elapsed_secs: 0,
            stop: None,
            limit: None,
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
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: false,
            contribution: c,
            started_elapsed_secs: 0,
            stop: None,
            limit: None,
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
        finished(&mut team, "a1", true, Some(contribution(2)));
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
        finished(&mut team, "a1", true, Some(contribution(1)));
        assert_eq!(
            team.children[0].purpose, "analyzing repository structure",
            "the finish summary is not the purpose"
        );
    }

    #[test]
    fn a_reporting_child_carries_its_finding_count() {
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "look");
        finished(&mut team, "a1", true, Some(contribution(7)));
        assert_eq!(
            team.children[0].contribution,
            Contribution::Reported { total: 7 }
        );
    }

    #[test]
    fn a_reviewer_that_found_nothing_is_a_result_not_an_empty_state() {
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review the diff");
        finished(&mut team, "r1", true, Some(contribution(0)));
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
        finished(&mut team, "w1", false, Some(contribution(0)));
        assert_eq!(team.children[0].status, ChildStatus::Failed);
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
            title: None,
            profile_id: Some("worker".into()),
            agent_name: None,
            read_only: false,
            contribution: None,
            started_elapsed_secs: 0,
            stop: None,
            limit: None,
        });
        assert!(!team2.children[0].is_read_only());
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
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: false,
            contribution: None,
            started_elapsed_secs: 0,
            stop: None,
            limit: None,
        });
        assert_eq!(team.children[0].role, "explorer");
    }

    #[test]
    fn a_clean_review_reads_as_a_result_not_a_blank() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "r1", "reviewer", "review the diff");
        finished(&mut team, "r1", true, Some(contribution(0)));
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

    /// The third reading a contribution can have, beside "nothing to flag"
    /// and "not measured" above: a measured, non-empty result states its own
    /// count. The three must stay distinguishable — a child that found seven
    /// things, a child that found none, and a child nobody measured are three
    /// different facts, and only the first is a tally.
    #[test]
    fn a_measured_contribution_states_the_count_it_measured() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "look");
        finished(&mut team, "a1", true, Some(contribution(7)));
        let line = contribution_line(&team.children[0], t).expect("a result line");
        assert!(line.contains('7'), "{line}");
        assert!(!line.contains("nothing to flag"), "{line}");
        assert!(!line.contains("not measured"), "{line}");
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

    fn finding(id: &str) -> UiFinding {
        UiFinding {
            id: id.into(),
            kind: "correctness".into(),
            summary: format!("summary {id}"),
            file: Some("src/auth.rs".into()),
            symbol: None,
        }
    }

    fn detail(measured: bool, findings: Vec<UiFinding>) -> UiChildContribution {
        UiChildContribution {
            child_id: "r1".into(),
            role: "reviewer".into(),
            profile_id: Some("reviewer".into()),
            read_only: true,
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
        team.apply_detail(detail(true, vec![finding("f-1"), finding("f-2")]));
        let rows = inspector_rows(&team.children[0], t).expect("loaded");
        let joined = rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("src/auth.rs"), "{joined}");
        assert!(joined.contains("[correctness]"), "{joined}");
        assert!(joined.contains("2 findings"), "{joined}");
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
            !joined.contains('0'),
            "a clean review is a sentence, not a tally that happens to be zero: \
             {joined}"
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
        finished(&mut team, "a1", true, Some(contribution(7)));
        let lines = team_lines(&team, t);
        assert_eq!(lines[0].glyph, "✓");
        assert!(lines[0].detail.contains("7 findings"), "{:?}", lines[0]);
    }

    #[test]
    fn a_clean_reviewer_reads_as_a_result_in_the_panel() {
        let t = crate::i18n::Locale::En.text();
        let mut team = TaskTeamView::default();
        started(&mut team, "w1", "worker", "implement");
        started(&mut team, "r1", "reviewer", "review");
        finished(&mut team, "r1", true, Some(contribution(0)));
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
        finished(&mut team, "w1", true, Some(contribution(0)));
        finished(&mut team, "r1", false, Some(contribution(0)));
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
        finished(&mut team, "r1", false, Some(contribution(0)));
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
        finished(&mut team, "r1", true, Some(contribution(0)));
        let line = contribution_line(&team.children[0], t).unwrap_or_default();
        assert!(line.contains("nothing to flag"), "{line}");
    }

    #[test]
    fn active_lists_only_children_still_working() {
        let mut team = TaskTeamView::default();
        started(&mut team, "a1", "explorer", "look");
        started(&mut team, "w1", "worker", "implement");
        finished(&mut team, "a1", true, Some(contribution(1)));
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
        Contribution::Reported { total } => Some(
            t.child_contribution_reported
                .replace("{n}", &total.to_string()),
        ),
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
        Contribution::Reported { total } => Some(
            t.child_contribution_reported
                .replace("{n}", &total.to_string()),
        ),
    }
}

/// One rendered row of the Contribution Inspector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectorRow {
    pub text: String,
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
    });
    if let Some(profile) = view.agent_name.as_deref().or(detail.profile_id.as_deref()) {
        let access = if view.is_read_only() {
            t.inspector_read_only
        } else {
            t.inspector_can_write
        };
        rows.push(InspectorRow {
            text: format!("{}: {profile} · {access}", t.inspector_profile),
        });
    }

    if !detail.measured {
        // The question could not be answered. Saying so beats an empty list,
        // which would read as "found nothing".
        rows.push(InspectorRow {
            text: t.child_contribution_unmeasured.to_string(),
        });
        return Some(rows);
    }

    if detail.findings.is_empty() {
        // A clean review is a result. It gets a sentence, not blank space.
        rows.push(InspectorRow {
            text: t.child_contribution_clean.to_string(),
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
        text.push_str(&format!(" [{}]", f.kind));
        rows.push(InspectorRow { text });
    }

    rows.push(InspectorRow {
        text: t
            .inspector_summary
            .replace("{n}", &detail.findings.len().to_string()),
    });
    Some(rows)
}

/// Visual tone of one agent-roster row; the renderer maps it to theme
/// colors. Presentation-only — never persisted, never a lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterTone {
    /// A child actively working (or queued to work).
    Active,
    /// A child that settled successfully.
    Done,
    /// A child that ended without completing. Never rendered as success.
    Failed,
}

/// One row of the active agent runtime roster: who, doing what, for how
/// long, at what accumulated usage. A pure render projection over existing
/// TUI state — structurally ready for future activation states (a child
/// that EXISTS is not assumed ACTIVE; tone follows current status), but no
/// new state is added before the runtime supports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRosterRow {
    /// Localized identity: 主 Agent / 探索 Agent / …
    pub label: String,
    /// Current human-readable activity — structured runtime facts only
    /// (tool/background labels, task text), never reasoning.
    pub activity: String,
    /// Right-aligned elapsed time, or absent.
    pub meta: Option<String>,
    pub tone: RosterTone,
}

/// The same text with the workspace's absolute path written as its name.
///
/// A spawn brief opens with "你在仓库 <absolute path>（工作区根目录）中…", so
/// three children in one repository painted three rows of the same hundred
/// characters and the reason never reached the screen. The path is the one
/// thing the reader already knows; its name is what identifies it.
pub fn name_the_workspace(text: &str, repository: &str) -> String {
    let repo = repository.trim_end_matches('/');
    let name = repo.rsplit('/').next().unwrap_or("");
    if repo.is_empty() || name.is_empty() || !text.contains(repo) {
        return text.to_string();
    }
    text.replace(repo, name)
}

/// Compact token figure for the team summary: `168k`, `29.9k`, `312`.
/// No fake precision — one decimal only below 100k.
pub fn fmt_tokens_compact(n: u32) -> String {
    if n >= 100_000 {
        format!("{}k", n / 1000)
    } else if n >= 1000 {
        let whole = n / 1000;
        let frac = (n % 1000) / 100;
        if frac == 0 {
            format!("{whole}k")
        } else {
            format!("{whole}.{frac}k")
        }
    } else {
        n.to_string()
    }
}

fn child_meta(view: &ChildAgentView, now_elapsed: u64) -> Option<String> {
    let elapsed = crate::status_line::fmt_elapsed(
        view.settled_elapsed_secs
            .unwrap_or(now_elapsed)
            .saturating_sub(view.started_elapsed_secs),
    );
    Some(elapsed)
}

/// The child's assigned task. Transient tool/step labels stay in the detail
/// page so this summary does not jump from the user's intent to implementation
/// noise such as `list_files`.
fn child_activity(view: &ChildAgentView, t: &crate::i18n::UiText) -> String {
    running_line(view, t)
}

/// Build the active child roster. The aggregate header owns the coordinator
/// state, so rows contain only the clickable children.
pub fn roster_rows(
    team: &TaskTeamView,
    now_elapsed: u64,
    t: &crate::i18n::UiText,
) -> Vec<AgentRosterRow> {
    let mut rows = Vec::with_capacity(team.children.len());
    for child in &team.children {
        let (tone, activity) = match child.status {
            ChildStatus::Waiting | ChildStatus::Running => {
                (RosterTone::Active, child_activity(child, t))
            }
            ChildStatus::Completed => (
                RosterTone::Done,
                contribution_line(child, t).unwrap_or_else(|| t.sub_agent_completed.to_string()),
            ),
            // Truth over comfort: an incomplete child never renders as ✓.
            ChildStatus::Failed => (
                RosterTone::Failed,
                stop_label(child.stop, t)
                    .map(str::to_string)
                    .unwrap_or_else(|| t.sub_agent_ended_incomplete.to_string()),
            ),
            ChildStatus::Interrupted => (RosterTone::Failed, t.sub_agent_interrupted.to_string()),
            ChildStatus::Unreported => (RosterTone::Failed, t.sub_agent_unreported.to_string()),
        };
        rows.push(AgentRosterRow {
            label: if child.nickname.trim().is_empty() {
                child
                    .agent_name
                    .clone()
                    .unwrap_or_else(|| display_role(&child.role, t))
            } else {
                child_label(child.nickname.trim(), child.agent_name.as_deref())
            },
            activity,
            meta: child_meta(child, now_elapsed),
            tone,
        });
    }
    rows
}

/// Live aggregate shown above the clickable child rows.
pub fn collaboration_runtime_line(
    team: &TaskTeamView,
    now_elapsed: u64,
    t: &crate::i18n::UiText,
) -> String {
    let active = team.active().count();
    if active == 0 {
        return collaboration_compact_line(team, t);
    }
    let tokens = team.children.iter().fold(0u32, |sum, child| {
        sum.saturating_add(child.input_tokens.saturating_add(child.output_tokens))
    });
    let started = team
        .active()
        .map(|child| child.started_elapsed_secs)
        .min()
        .unwrap_or(now_elapsed);
    let mut line = t
        .agents_running_header
        .replacen("{}", &active.to_string(), 1);
    if tokens > 0 {
        line.push_str(&format!(" · {} tokens", fmt_tokens_compact(tokens)));
    }
    line.push_str(&format!(
        " · {}",
        crate::status_line::fmt_elapsed(now_elapsed.saturating_sub(started))
    ));
    line
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

/// The one-line runtime row: who is working right now.
///
/// Truth over brevity: a settled team says what it ended as, and a team that
/// lost a child never says "completed".
pub fn collaboration_compact_line(team: &TaskTeamView, t: &crate::i18n::UiText) -> String {
    let n = team.children.len();
    let bad = team
        .children
        .iter()
        .filter(|c| c.status == ChildStatus::Failed)
        .count();
    if team.active().next().is_none() {
        return if bad > 0 {
            t.collaboration_ended_incomplete
                .replace("{n}", &n.to_string())
                .replace("{bad}", &bad.to_string())
        } else {
            t.collaboration_done.replace("{n}", &n.to_string())
        };
    }
    let mut row = t.collaboration_compact.replace("{n}", &n.to_string());
    for c in team.active() {
        let state = match c.status {
            ChildStatus::Running => t.agent_status_running,
            _ => t.sub_agent_waiting,
        };
        row.push_str(&format!(" · {} {}", c.role, state));
    }
    row
}

/// The glyph on the compact collaboration row.
///
/// Found by replaying real sessions: a settled team that lost a child rendered
/// `✓ 2 个 Agent 已结束 · 1 项未完成` — the success mark on the line that
/// announces work which did not finish. The colour already went red; the glyph
/// did not, and the glyph is what a reader takes as the verdict. Same rule the
/// blocked goal row follows (`status_glyph`): a call that ran is not an outcome
/// that succeeded.
///
/// `terminal` is the caller's own settled test, so WHEN the glyph appears does
/// not change — only what it says when the team lost someone.
pub fn collaboration_glyph(team: &TaskTeamView, terminal: bool) -> &'static str {
    if !terminal {
        return "\u{25c9}";
    }
    let lost_one = team
        .children
        .iter()
        .any(|c| c.status == ChildStatus::Failed);
    if lost_one { "\u{26a0}" } else { "\u{2713}" }
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
                ChildStatus::Interrupted => "⏸",
                ChildStatus::Unreported => "?",
            };
            let detail = match c.status {
                ChildStatus::Waiting | ChildStatus::Running => running_line(c, t),
                ChildStatus::Interrupted => t.sub_agent_interrupted.to_string(),
                ChildStatus::Unreported => t.sub_agent_unreported.to_string(),
                ChildStatus::Failed => child_stop_label(c.stop, c.limit, t)
                    .unwrap_or(t.sub_agent_incomplete)
                    .to_string(),
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
