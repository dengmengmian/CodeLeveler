//! The turn boundary owns the live activity roster.
//!
//! Dogfood: after Turn N ended with `Euclid ✓`, the next question opened Turn
//! N+1 and Euclid was still painted in the activity strip — a terminal child
//! of a finished turn presented as work in flight. The transcript keeps it
//! (history); the live roster must not.
//!
//! Every case here drives the same [`reduce`] the live stream, the replay and
//! the reconnect all use, so one fix covers all three paths.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use leveler_client_protocol::{
    PlanStepStatus, RuntimeEvent, RuntimeStatus, SessionId, UiChildAgent, UiChildState,
    UiHistoryEntry, UiPlan, UiPlanStep, UiSessionSnapshot,
};
use leveler_tui::action::Action;
use leveler_tui::multi_agent::ChildStatus;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use leveler_tui::transcript::TranscriptItem;

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn state() -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "麻凡".to_string(),
            version: "0.1.0".to_string(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    )
}

fn snapshot() -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: "/repo".to_string(),
        goal: "interactive session".to_string(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
        mode: leveler_client_protocol::PermissionProfile::Assisted,
        branch: Some("main".to_string()),
        status: "idle".to_string(),
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        active_background_tasks: Vec::new(),
        plan: None,
        verification: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        reasoning: None,
        work_profile: None,
        collaboration: None,
        children: Vec::new(),
    }
}

fn opened() -> AppState {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(),
        }),
    );
    s
}

fn typed(s: &mut AppState, text: &str) {
    for ch in text.chars() {
        reduce(s, key(KeyCode::Char(ch)));
    }
}

/// Type a message and send it the way the user does.
fn ask(s: &mut AppState, text: &str) {
    typed(s, text);
    reduce(s, key(KeyCode::Enter));
}

/// One `SubAgentUpdated`, as the runtime carries it.
fn child_update(
    id: &str,
    nickname: &str,
    done: bool,
    ok: bool,
    detail: &str,
    stop: Option<leveler_client_protocol::ChildStop>,
) -> RuntimeEvent {
    RuntimeEvent::SubAgentUpdated {
        id: id.into(),
        nickname: nickname.into(),
        role: "explorer".into(),
        title: None,
        done,
        ok,
        detail: detail.into(),
        profile_id: None,
        profile_role: None,
        read_only: true,
        agent: None,
        contribution: None,
        outcome: None,
        stop,
        limit: None,
        background: Some(true),
        scope: Vec::new(),
    }
}

fn history_has_child(s: &AppState, id: &str) -> bool {
    s.transcript
        .items()
        .iter()
        .any(|i| matches!(i, TranscriptItem::SubAgent(b) if b.id == id))
}

fn live_has_child(s: &AppState, id: &str) -> bool {
    s.team.children.iter().any(|c| c.id == id)
}

/// Render the whole screen to text (TestBackend).
fn rendered(state: &mut AppState, w: u16, h: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        out.push('\n');
    }
    out
}

/// A current-activity strip row: `{glyph} {name} · {time} ↗`. Only the
/// activity strip carries the `↗`; the transcript's sub-agent head does not.
fn strip_has_activity_row(frame: &str, name: &str) -> bool {
    frame.lines().any(|l| l.contains(name) && l.contains('↗'))
}

fn plan(steps: &[(&str, PlanStepStatus)]) -> UiPlan {
    UiPlan {
        steps: steps
            .iter()
            .enumerate()
            .map(|(index, (description, status))| UiPlanStep {
                index,
                description: (*description).to_string(),
                status: *status,
            })
            .collect(),
    }
}

#[test]
fn a_terminal_turn_moves_the_exact_partial_plan_into_history() {
    let mut s = opened();
    ask(&mut s, "问题 A");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: plan(&[
                ("步骤一", PlanStepStatus::Done),
                ("步骤二", PlanStepStatus::Done),
                ("步骤三", PlanStepStatus::Pending),
            ]),
        }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    assert!(s.plan.is_none(), "terminal work cannot remain active");
    let terminal = rendered(&mut s, 120, 40);
    assert!(
        terminal.contains("步骤三"),
        "final plan stays in history:\n{terminal}"
    );
    assert!(
        terminal.contains("2/3"),
        "partial progress is not forged:\n{terminal}"
    );

    ask(&mut s, "问题 B");
    assert!(
        s.plan.is_none(),
        "the old plan must not become Turn 2 activity"
    );
    let next = rendered(&mut s, 120, 40);
    assert!(
        next.contains("步骤三"),
        "Turn 1 history remains visible:\n{next}"
    );
}

#[test]
fn a_fully_done_plan_is_kept_until_terminal_then_archived_verbatim() {
    let mut s = opened();
    ask(&mut s, "问题 A");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: plan(&[
                ("步骤一", PlanStepStatus::Done),
                ("步骤二", PlanStepStatus::Done),
                ("步骤三", PlanStepStatus::Done),
            ]),
        }),
    );
    assert!(
        s.plan.is_some(),
        "the reducer retains lifecycle truth until terminal"
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert!(s.plan.is_none());
    let frame = rendered(&mut s, 120, 40);
    assert!(
        frame.contains("3/3"),
        "the historical snapshot is exact:\n{frame}"
    );
}

#[test]
fn replay_archives_a_terminal_plan_without_making_it_live() {
    let mut s = opened();
    let query_id = leveler_client_protocol::CommandId::generate();
    s.history_query = Some(query_id.clone());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id: Some(query_id),
            session_id: SessionId::new("s1"),
            omitted_turns: 0,
            entries: vec![
                UiHistoryEntry {
                    turn_elapsed_ms: 0,
                    turn_start: true,
                    event: RuntimeEvent::PlanUpdated {
                        plan: plan(&[("历史步骤", PlanStepStatus::Running)]),
                    },
                },
                UiHistoryEntry {
                    turn_elapsed_ms: 10,
                    turn_start: false,
                    event: RuntimeEvent::TurnIncomplete {
                        reason: "blocked".into(),
                    },
                },
            ],
        }),
    );

    assert!(
        s.plan.is_none(),
        "history replay cannot populate active plan"
    );
    let frame = rendered(&mut s, 120, 40);
    assert!(
        frame.contains("历史步骤"),
        "replay reconstructs plan history:\n{frame}"
    );
}

#[test]
fn failed_turn_archives_its_exact_plan_without_leaving_active_work() {
    let mut s = opened();
    ask(&mut s, "问题 A");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: plan(&[
                ("已完成", PlanStepStatus::Done),
                ("失败步骤", PlanStepStatus::Failed),
                ("未开始", PlanStepStatus::Pending),
            ]),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnFailed {
            error: "boom".into(),
            failure: None,
        }),
    );

    assert!(s.plan.is_none(), "a failed turn is terminal");
    let frame = rendered(&mut s, 120, 40);
    assert!(
        frame.contains("1/3"),
        "failure does not forge progress:\n{frame}"
    );
    assert!(
        frame.contains("失败步骤") && frame.contains("未开始"),
        "the exact declaration remains historical:\n{frame}"
    );
}

#[test]
fn historical_and_current_plans_render_on_separate_surfaces() {
    let mut s = opened();
    ask(&mut s, "问题 A");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: plan(&[("旧计划步骤", PlanStepStatus::Pending)]),
        }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCancelled));
    ask(&mut s, "问题 B");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: plan(&[("当前计划步骤", PlanStepStatus::Running)]),
        }),
    );

    assert_eq!(
        s.plan.as_ref().unwrap().steps[0].description,
        "当前计划步骤"
    );
    let frame = rendered(&mut s, 120, 40);
    assert!(
        frame.contains("旧计划步骤"),
        "history plan missing:\n{frame}"
    );
    assert!(
        frame.contains("当前计划步骤"),
        "active plan missing:\n{frame}"
    );
}

/// The core regression: a child that reached its terminal in Turn 1 is not
/// Turn 2's activity, while Turn 1's transcript still shows it.
#[test]
fn a_settled_child_leaves_the_live_roster_when_the_next_turn_begins() {
    let mut s = opened();
    ask(&mut s, "问题 A");
    reduce(
        &mut s,
        Action::Runtime(child_update(
            "c1",
            "Euclid",
            false,
            false,
            "你在 CodeLeveler 仓库中加固 extract 模块。",
            None,
        )),
    );
    reduce(
        &mut s,
        Action::Runtime(child_update("c1", "Euclid", true, true, "完成", None)),
    );
    assert!(
        live_has_child(&s, "c1"),
        "a child is live during its own turn"
    );
    assert!(history_has_child(&s, "c1"));
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert_eq!(s.status, RuntimeStatus::Idle);

    ask(&mut s, "问题 B");
    assert_eq!(s.status, RuntimeStatus::Busy, "a new turn began");
    assert!(
        !live_has_child(&s, "c1"),
        "Turn 1's terminal child must not be Turn 2's current activity"
    );
    assert!(
        s.team.children.is_empty(),
        "no child belongs to Turn 2: {:?}",
        s.team.children
    );
    assert!(
        history_has_child(&s, "c1"),
        "Turn 1's transcript must keep the child"
    );
}

/// Completed, failed and cancelled are all terminal: none of them leaks into
/// the next turn.
#[test]
fn every_terminal_outcome_of_a_finished_turn_leaves_the_live_roster() {
    use leveler_client_protocol::ChildStop;
    let mut s = opened();
    ask(&mut s, "问题 A");
    for (id, nickname, ok, stop) in [
        ("c1", "Euclid", true, ChildStop::Completed),
        ("c2", "Newton", false, ChildStop::Cancelled),
        ("c3", "Curie", false, ChildStop::Failed),
    ] {
        reduce(
            &mut s,
            Action::Runtime(child_update(id, nickname, false, false, "task", None)),
        );
        reduce(
            &mut s,
            Action::Runtime(child_update(id, nickname, true, ok, "settled", Some(stop))),
        );
    }
    assert_eq!(s.team.children.len(), 3, "all three are live in their turn");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    ask(&mut s, "问题 B");
    assert!(
        s.team.children.is_empty(),
        "terminal children must leave: {:?}",
        s.team.children
    );
    for id in ["c1", "c2", "c3"] {
        assert!(history_has_child(&s, id), "{id} must stay in history");
    }
}

/// A child still open when the turn ended may be continued or settled by a
/// later turn, so it stays as cross-turn background activity — the boundary
/// must not delete it.
#[test]
fn a_child_still_open_at_the_turn_boundary_stays_as_background_activity() {
    let mut s = opened();
    ask(&mut s, "问题 A");
    reduce(
        &mut s,
        Action::Runtime(child_update("c1", "Euclid", false, false, "还在跑", None)),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    assert_eq!(
        s.team.children[0].status,
        ChildStatus::Unreported,
        "the turn ended with no terminal for this child"
    );

    ask(&mut s, "问题 B");
    assert!(
        live_has_child(&s, "c1"),
        "an open cross-turn child is background activity, not history"
    );
}

/// A child the current turn spawned is live activity.
#[test]
fn the_current_turns_child_is_live_activity() {
    let mut s = opened();
    ask(&mut s, "问题 A");
    reduce(
        &mut s,
        Action::Runtime(child_update("c1", "Euclid", false, false, "task A", None)),
    );
    reduce(
        &mut s,
        Action::Runtime(child_update("c1", "Euclid", true, true, "done", None)),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    ask(&mut s, "问题 B");
    assert!(s.team.children.is_empty());
    reduce(
        &mut s,
        Action::Runtime(child_update("d1", "Newton", false, false, "task B", None)),
    );
    assert!(live_has_child(&s, "d1"), "the current turn's child is live");
    assert!(!live_has_child(&s, "c1"), "the previous turn's is not");
    assert!(s.team.surface_visible(s.elapsed_secs));
}

/// What the user sees: the activity strip is the current execution area. It
/// offers details while the child is running, collapses once the team settles,
/// and leaves the durable history in the transcript for the next turn.
#[test]
fn the_activity_strip_shows_the_child_in_its_turn_and_not_in_the_next() {
    let mut s = opened();
    ask(&mut s, "问题 A");
    reduce(
        &mut s,
        Action::Runtime(child_update(
            "c1",
            "Euclid",
            false,
            false,
            "你在 CodeLeveler 仓库中加固 extract 模块。",
            None,
        )),
    );
    let running = rendered(&mut s, 120, 40);
    assert!(
        strip_has_activity_row(&running, "Euclid"),
        "the strip offers child details while it is running:\n{running}"
    );

    reduce(
        &mut s,
        Action::Runtime(child_update("c1", "Euclid", true, true, "完成", None)),
    );
    let settled = rendered(&mut s, 120, 40);
    assert!(
        settled.replace(' ', "").contains("1个Agent已完成"),
        "a settled team collapses to one terminal line:\n{settled}"
    );
    assert!(
        !strip_has_activity_row(&settled, "Euclid"),
        "settled children no longer advertise a live detail action:\n{settled}"
    );

    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    ask(&mut s, "问题 B");
    let turn2 = rendered(&mut s, 120, 40);
    assert!(
        !strip_has_activity_row(&turn2, "Euclid"),
        "the previous turn's terminal child must leave the strip:\n{turn2}"
    );
    assert!(
        turn2.contains("Euclid"),
        "the transcript keeps the history:\n{turn2}"
    );
}

/// A background shell task is a different lifecycle: it may genuinely
/// outlive the turn that started it, so the boundary must not touch it.
#[test]
fn a_cross_turn_background_task_keeps_running_into_the_next_turn() {
    let mut s = opened();
    ask(&mut s, "问题 A");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: "bg1".into(),
            program: "cargo".into(),
            args: vec!["test".into()],
        }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));

    ask(&mut s, "问题 B");
    assert!(
        s.background_task_labels.contains_key("bg1"),
        "a running background task is not the previous turn's history"
    );
    let frame = rendered(&mut s, 120, 40);
    assert!(
        strip_has_activity_row(&frame, "cargo test"),
        "the background task still shows as live activity:\n{frame}"
    );
}

#[test]
fn reconnect_rebuilds_only_registry_active_background_tasks() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: "terminal-before-reconnect".into(),
            program: "false".into(),
            args: vec![],
        }),
    );
    let mut snap = snapshot();
    snap.active_background_tasks = vec![leveler_client_protocol::UiActiveBackgroundTask {
        task_id: "still-running".into(),
        program: "cargo".into(),
        args: vec!["test".into()],
        elapsed_ms: 4_000,
    }];

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );

    assert!(
        !s.background_task_labels
            .contains_key("terminal-before-reconnect")
    );
    assert!(
        s.background_task_labels
            .get("still-running")
            .is_some_and(|task| task.is_running())
    );
}

#[test]
fn lifecycle_lag_reconciliation_replaces_stale_active_background_tasks() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: "missed-terminal".into(),
            program: "false".into(),
            args: vec![],
        }),
    );
    s.activity_selected = Some(leveler_tui::activity::ActivityId::Background(
        "missed-terminal".into(),
    ));
    s.activity_open = Some(leveler_tui::activity::ActivityId::Background(
        "missed-terminal".into(),
    ));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTasksReconciled {
            tasks: vec![leveler_client_protocol::UiActiveBackgroundTask {
                task_id: "still-running".into(),
                program: "cargo".into(),
                args: vec!["check".into()],
                elapsed_ms: 2_000,
            }],
        }),
    );

    assert!(!s.background_task_labels.contains_key("missed-terminal"));
    assert!(
        s.background_task_labels
            .get("still-running")
            .is_some_and(|task| task.is_running())
    );
    assert!(s.activity_selected.is_none());
    assert!(s.activity_open.is_none());
}

/// Reopening a session must not rebuild a settled child as current activity,
/// and replaying its history must still show it.
#[test]
fn reopening_and_replaying_never_make_a_settled_child_current_activity() {
    let mut s = state();
    let mut snap = snapshot();
    snap.children = vec![UiChildAgent {
        id: "c1".into(),
        nickname: "Euclid".into(),
        role: "explorer".into(),
        profile_id: None,
        read_only: true,
        agent: None,
        title: None,
        purpose: "你在 CodeLeveler 仓库中加固 extract 模块。".into(),
        state: UiChildState::Settled,
        ok: true,
        background: true,
        scope: Vec::new(),
        resumes: 0,
        outcome: None,
        stop: None,
        limit: None,
        summary: Some("完成".into()),
        input_tokens: 10,
        output_tokens: 5,
        cost_usd_micros: None,
    }];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    assert!(
        !live_has_child(&s, "c1"),
        "settled history belongs to the transcript, not the live team"
    );

    // The durable history replay (the real gate: this client's own query) shows
    // the child without reopening it as activity.
    let query_id = leveler_client_protocol::CommandId::generate();
    s.history_query = Some(query_id.clone());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id: Some(query_id),
            session_id: SessionId::new("s1"),
            omitted_turns: 0,
            entries: vec![
                UiHistoryEntry {
                    turn_elapsed_ms: 0,
                    turn_start: true,
                    event: child_update("c1", "Euclid", false, false, "task", None),
                },
                UiHistoryEntry {
                    turn_elapsed_ms: 1500,
                    turn_start: false,
                    event: child_update("c1", "Euclid", true, true, "done", None),
                },
            ],
        }),
    );
    assert!(
        history_has_child(&s, "c1"),
        "the replayed history keeps the child"
    );
    assert!(
        !live_has_child(&s, "c1"),
        "replay must not turn history into current activity"
    );
}

/// A session switch must not carry the previous session's open Activity Detail
/// into a session that never had that task.
#[test]
fn switching_sessions_drops_the_previous_activity_detail() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BackgroundTaskStarted {
            task_id: "bg-2".into(),
            program: "cargo".into(),
            args: vec!["test".into()],
        }),
    );
    s.activity_selected = Some(leveler_tui::activity::ActivityId::Background("bg-2".into()));
    s.activity_open = Some(leveler_tui::activity::ActivityId::Background("bg-2".into()));
    s.active_screen = leveler_tui::screen::Screen::Activity;

    let mut other = snapshot();
    other.id = SessionId::new("s2");
    other.active_background_tasks = vec![leveler_client_protocol::UiActiveBackgroundTask {
        task_id: "bg-2".into(),
        program: "cargo".into(),
        args: vec!["test".into()],
        elapsed_ms: 1_000,
    }];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: other }),
    );

    assert_eq!(s.session_id, SessionId::new("s2"));
    assert!(
        s.activity_open.is_none(),
        "the previous session's detail must not stay open"
    );
    assert!(
        s.activity_selected.is_none(),
        "the previous session's selection must not stay"
    );
}
