//! What an explicit client exit would do to work that is still running.
//!
//! This is a projection over the runtime's own structured snapshot — never a
//! reading of task text, a UI counter, or a process name. It answers three
//! questions the exit decision needs:
//!
//! 1. Which execution entities are affected by THIS client exiting?
//! 2. Which of them can really keep running after the client is gone?
//! 3. What is the stable identity of each, so a later confirmation cannot act
//!    on a list that has moved?
//!
//! The product contract (`stop_session_before_quit`) already distinguishes
//! lifetimes: a client exit owns `goal` work and leaves hosted
//! `runtime`/`session`/`persistent` services alone. This projection uses the
//! same predicate, so the dialog and the destructive path cannot disagree.

use leveler_client_protocol::{UiSessionSnapshot, UiUserShell};

use crate::tool_cell::background_label;

/// The lifetime identity of the work a dialog row names. Also part of the
/// execution identity: a task id and a shell id are different namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExitTaskKind {
    /// The live agent turn. Subagents and in-flight tool calls are part of it;
    /// listing them separately would double-count one execution.
    AgentTurn,
    /// A registry-backed background process that the client exit would stop.
    BackgroundTask,
    /// A foreground user shell (`!command`) owned by this client's runtime.
    UserShell,
}

/// Why one affected execution cannot be left running after the client exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitContinueBlock {
    /// The runtime lives in this client process, so exiting ends the session
    /// that manages its work. Even a process owned by a separate substrate
    /// would lose the lifecycle that can query, stop and settle it.
    RuntimeEndsWithClient,
    /// This turn already received a cancel request. Choosing "continue" now
    /// would not save it, so the option must not claim that it would.
    TurnAlreadyStopping,
}

/// One execution entity the exit would affect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitTask {
    pub kind: ExitTaskKind,
    /// Stable execution identity: a registry task id, a user-shell execution
    /// id, or the single turn slot. Never a pid, a label, or a display order.
    pub id: String,
    /// Readable description from the runtime's own facts. Empty means the
    /// presenter supplies the kind's own label.
    pub label: String,
    /// Runtime-observed age, when the snapshot carries one.
    pub elapsed_ms: Option<u64>,
    /// A reason this exact entity cannot continue after the client exits.
    pub block: Option<ExitContinueBlock>,
}

impl ExitTask {
    pub fn same_execution(&self, other: &ExitTask) -> bool {
        self.kind == other.kind && self.id == other.id
    }
}

/// The complete exit impact, in the terms the dialog renders and the
/// destructive path verifies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitImpact {
    /// The runtime is a separate process and keeps running after this client
    /// exits. A transport fact supplied by the launcher, never inferred from
    /// task state.
    pub detachable: bool,
    /// Affected executions, oldest-first as far as the snapshot orders them.
    pub tasks: Vec<ExitTask>,
}

impl ExitImpact {
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Whether "continue in background and exit" can be honored for EVERY
    /// affected entity. A partial answer is never offered: a batch option must
    /// not silently end some of the work it claims to continue.
    pub fn can_continue(&self) -> bool {
        self.detachable && self.tasks.iter().all(|task| task.block.is_none())
    }

    /// The dominant reason continuation is unavailable, if any.
    pub fn continue_block(&self) -> Option<ExitContinueBlock> {
        if !self.detachable {
            return Some(ExitContinueBlock::RuntimeEndsWithClient);
        }
        self.tasks.iter().find_map(|task| task.block)
    }

    /// Work present now that the user did not see when they confirmed. A task
    /// that finished while the dialog was open is not a change: only a NEW
    /// execution makes the earlier decision incomplete.
    pub fn has_new_work(&self, confirmed: &ExitImpact) -> bool {
        self.tasks
            .iter()
            .any(|task| !confirmed.tasks.iter().any(|old| old.same_execution(task)))
    }
}

/// Whether the snapshot describes a live agent turn.
///
/// `status == "running"` is the runtime's own session-level fact and already
/// excludes zombies whose owning boot is gone (the runtime projects those as
/// `interrupted`). A turn that has emitted its final message but is still
/// finalizing remains live and is still affected by an exit.
pub fn has_live_turn(snapshot: &UiSessionSnapshot) -> bool {
    snapshot.status == "running" || snapshot.finalization_stage.is_some()
}

/// Whether an active registry task is one a client exit owns.
///
/// `None` is an older runtime without the lifetime field; the existing cleanup
/// treats it as goal-scoped, so the projection and the destructive path share
/// this one predicate.
pub(crate) fn affected_background_lifetime(lifetime: Option<&str>) -> bool {
    matches!(lifetime, None | Some("goal"))
}

/// Whether a user shell is the live foreground execution.
///
/// A shell already moved to the background carries a `task_id`; its process
/// then belongs to a lifetime that outlives this client and is not this exit's
/// work.
fn foreground_shell(shell: &UiUserShell) -> bool {
    shell.status == "running" && shell.task_id.is_none()
}

/// Project the exit impact from one authoritative snapshot.
pub fn project(
    snapshot: &UiSessionSnapshot,
    scope: ClientExitScope,
    turn_cancel_requested: bool,
) -> ExitImpact {
    let mut tasks = Vec::new();
    if has_live_turn(snapshot) {
        tasks.push(ExitTask {
            kind: ExitTaskKind::AgentTurn,
            // The turn ordinal is the runtime's durable identity for "which
            // turn is this". A fixed slot id would make a NEW turn after the
            // first one ended look like the same execution, so a continuing
            // client could leave unseen work behind.
            id: snapshot
                .latest_turn_ordinal
                .map(|ordinal| format!("turn-{ordinal}"))
                .unwrap_or_else(|| "agent-turn".to_string()),
            label: turn_label(snapshot),
            elapsed_ms: None,
            block: turn_cancel_requested.then_some(ExitContinueBlock::TurnAlreadyStopping),
        });
    }
    for task in &snapshot.active_background_tasks {
        if !affected_background_lifetime(task.lifetime.as_deref()) {
            continue;
        }
        tasks.push(ExitTask {
            kind: ExitTaskKind::BackgroundTask,
            id: task.task_id.clone(),
            label: background_label(&task.program, &task.args),
            elapsed_ms: Some(task.elapsed_ms),
            block: None,
        });
    }
    for shell in &snapshot.user_shells {
        if !foreground_shell(shell) {
            continue;
        }
        tasks.push(ExitTask {
            kind: ExitTaskKind::UserShell,
            id: shell.id.as_str().to_string(),
            label: shell.command.trim().to_string(),
            elapsed_ms: Some(shell.elapsed_secs.saturating_mul(1000)),
            block: None,
        });
    }
    ExitImpact {
        detachable: scope == ClientExitScope::DetachesFromRuntime,
        tasks,
    }
}

/// The turn's own readable label: the objective it is executing, then the
/// session's display title, then its original request. Empty when the snapshot
/// carries none; the presenter then names the kind itself.
fn turn_label(snapshot: &UiSessionSnapshot) -> String {
    snapshot
        .execution_task
        .as_ref()
        .map(|task| task.objective.trim())
        .filter(|text| !text.is_empty())
        .or_else(|| {
            snapshot
                .title
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .or_else(|| {
            let goal = snapshot.goal.trim();
            (!goal.is_empty()).then_some(goal)
        })
        .unwrap_or_default()
        .to_string()
}

/// Where the runtime this client is attached to lives.
///
/// This is a composition-root fact: the launcher knows whether it connected to
/// a separate runtime process or constructed one inside this process. It is
/// never derived from task state or a probe performed by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientExitScope {
    /// A separate runtime process owns the work and survives this client.
    DetachesFromRuntime,
    /// The runtime is inside this process; exiting ends everything it owns.
    EndsWithClient,
}

/// Test-only convenience: a task with no item-specific block.
#[cfg(test)]
fn task(kind: ExitTaskKind, id: &str, label: &str) -> ExitTask {
    ExitTask {
        kind,
        id: id.to_string(),
        label: label.to_string(),
        elapsed_ms: Some(1),
        block: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::InteractiveRuntimeClient;
    use leveler_client_protocol::UiActiveBackgroundTask;
    use leveler_client_protocol::mock::MockRuntimeClient;
    use leveler_core::{SessionId, UserShellId};

    fn snapshot() -> UiSessionSnapshot {
        let mock = MockRuntimeClient::new(SessionId::new("s1"));
        futures::executor::block_on(mock.snapshot(&SessionId::new("s1"))).unwrap()
    }

    fn background(id: &str, lifetime: Option<&str>) -> UiActiveBackgroundTask {
        UiActiveBackgroundTask {
            task_id: id.to_string(),
            program: "cargo".to_string(),
            args: vec!["test".to_string(), "--workspace".to_string()],
            elapsed_ms: 4_000,
            pid: Some(42),
            ready_evidence: None,
            lifetime: lifetime.map(str::to_string),
        }
    }

    fn shell(id: &str, status: &str, task_id: Option<&str>) -> UiUserShell {
        UiUserShell {
            id: UserShellId::new(id),
            origin_command_id: None,
            command: "npm run dev".to_string(),
            cwd: "/repo".to_string(),
            status: status.to_string(),
            elapsed_secs: 3,
            exit_code: None,
            output_tail: String::new(),
            output_truncated: false,
            full_output: None,
            task_id: task_id.map(str::to_string),
        }
    }

    #[test]
    fn an_idle_session_has_nothing_affected() {
        let impact = project(&snapshot(), ClientExitScope::DetachesFromRuntime, false);
        assert!(impact.is_empty());
    }

    #[test]
    fn terminal_background_records_do_not_trigger_exit_protection() {
        let mut snap = snapshot();
        snap.background_tasks = vec![leveler_client_protocol::UiBackgroundTaskRecord {
            task_id: "bg-done".into(),
            call_id: Some("call-1".into()),
            program: "cargo".into(),
            args: vec!["test".into()],
            lifetime: Some("goal".into()),
            state: leveler_client_protocol::UiBackgroundTaskState::Completed,
            exit_code: Some(0),
            output: "ok".into(),
            output_truncated: false,
            duration_ms: Some(1_000),
        }];
        assert!(
            project(&snap, ClientExitScope::DetachesFromRuntime, false).is_empty(),
            "a completed task is history, not exit-protected work"
        );
    }

    #[test]
    fn a_live_turn_is_one_affected_execution() {
        let mut snap = snapshot();
        snap.status = "running".into();
        let impact = project(&snap, ClientExitScope::DetachesFromRuntime, false);
        assert_eq!(impact.tasks.len(), 1);
        assert_eq!(impact.tasks[0].kind, ExitTaskKind::AgentTurn);
    }

    #[test]
    fn finalizing_turn_is_still_live() {
        let mut snap = snapshot();
        snap.finalization_stage =
            Some(leveler_client_protocol::FinalizationStage::SettlingDependencies);
        assert!(has_live_turn(&snap));
    }

    #[test]
    fn only_goal_lifetime_background_tasks_are_affected() {
        let mut snap = snapshot();
        snap.active_background_tasks = vec![
            background("bg-goal", Some("goal")),
            background("bg-legacy", None),
            background("bg-runtime", Some("runtime")),
            background("bg-session", Some("session")),
            background("bg-persistent", Some("persistent")),
        ];
        let impact = project(&snap, ClientExitScope::DetachesFromRuntime, false);
        let ids: Vec<_> = impact.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids, ["bg-goal", "bg-legacy"]);
    }

    #[test]
    fn foreground_shells_are_affected_but_backgrounded_ones_are_not() {
        let mut snap = snapshot();
        snap.user_shells = vec![
            shell("ush-live", "running", None),
            shell("ush-moved", "background", Some("bg-1")),
            shell("ush-done", "success", None),
        ];
        let impact = project(&snap, ClientExitScope::DetachesFromRuntime, false);
        assert_eq!(impact.tasks.len(), 1);
        assert_eq!(impact.tasks[0].kind, ExitTaskKind::UserShell);
        assert_eq!(impact.tasks[0].id, "ush-live");
        assert_eq!(impact.tasks[0].label, "npm run dev");
    }

    #[test]
    fn subagents_are_not_counted_beside_their_turn() {
        let mut snap = snapshot();
        snap.status = "running".into();
        snap.children = vec![leveler_client_protocol::UiChildAgent {
            id: "child-1".into(),
            nickname: "reviewer".into(),
            role: "review".into(),
            profile_id: None,
            read_only: true,
            agent: None,
            title: None,
            purpose: "inspect".into(),
            state: leveler_client_protocol::UiChildState::Running,
            ok: false,
            background: false,
            scope: Vec::new(),
            resumes: 0,
            outcome: None,
            stop: None,
            limit: None,
            summary: None,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd_micros: None,
        }];
        let impact = project(&snap, ClientExitScope::DetachesFromRuntime, false);
        assert_eq!(impact.tasks.len(), 1, "one execution, not one per subagent");
        assert_eq!(impact.tasks[0].kind, ExitTaskKind::AgentTurn);
    }

    #[test]
    fn an_embedded_runtime_cannot_continue_anything() {
        let mut snap = snapshot();
        snap.status = "running".into();
        snap.active_background_tasks = vec![background("bg-1", Some("goal"))];
        let impact = project(&snap, ClientExitScope::EndsWithClient, false);
        assert_eq!(impact.tasks.len(), 2);
        assert!(!impact.can_continue());
        assert_eq!(
            impact.continue_block(),
            Some(ExitContinueBlock::RuntimeEndsWithClient)
        );
    }

    #[test]
    fn a_detachable_runtime_can_continue_untouched_work() {
        let mut snap = snapshot();
        snap.active_background_tasks = vec![background("bg-1", Some("goal"))];
        let impact = project(&snap, ClientExitScope::DetachesFromRuntime, false);
        assert!(impact.can_continue());
        assert_eq!(impact.continue_block(), None);
    }

    #[test]
    fn a_turn_already_cancelling_cannot_be_presented_as_continuable() {
        let mut snap = snapshot();
        snap.status = "running".into();
        let impact = project(&snap, ClientExitScope::DetachesFromRuntime, true);
        assert!(!impact.can_continue());
        assert_eq!(
            impact.continue_block(),
            Some(ExitContinueBlock::TurnAlreadyStopping)
        );
    }

    #[test]
    fn a_new_turn_after_the_first_ended_is_new_work() {
        let mut snap = snapshot();
        snap.status = "running".into();
        snap.latest_turn_ordinal = Some(4);
        let first = project(&snap, ClientExitScope::DetachesFromRuntime, false);
        snap.latest_turn_ordinal = Some(5);
        let second = project(&snap, ClientExitScope::DetachesFromRuntime, false);
        assert_ne!(first.tasks[0].id, second.tasks[0].id);
        assert!(
            second.has_new_work(&first),
            "a different turn is a different execution"
        );
    }

    #[test]
    fn a_task_started_during_the_dialog_is_new_work() {
        let confirmed = ExitImpact {
            detachable: true,
            tasks: vec![task(ExitTaskKind::BackgroundTask, "bg-1", "cargo test")],
        };
        let mut current = confirmed.clone();
        current
            .tasks
            .push(task(ExitTaskKind::UserShell, "ush-1", "npm run dev"));
        assert!(current.has_new_work(&confirmed));
        let mut finished = confirmed.clone();
        finished.tasks.clear();
        assert!(
            !finished.has_new_work(&confirmed),
            "a task that ended is not a reason to re-ask"
        );
    }

    #[test]
    fn identity_is_kind_and_id_never_label_or_order() {
        let a = task(ExitTaskKind::BackgroundTask, "bg-1", "old label");
        let b = task(ExitTaskKind::BackgroundTask, "bg-1", "new label");
        assert!(a.same_execution(&b));
        let c = task(ExitTaskKind::UserShell, "bg-1", "same id, other namespace");
        assert!(!a.same_execution(&c));
    }
}
