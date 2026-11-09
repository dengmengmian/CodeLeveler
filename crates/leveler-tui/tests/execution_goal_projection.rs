//! The durable goal a session owes, as the TUI projects it.
//!
//! A goal committed from a Chat turn outlives the turn window. The client must
//! keep presenting the goal's own state — including the pause the user set —
//! across ordinary questions that do not touch it, and it must never read a
//! different goal's history as this goal's authorization.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{
    ClientCommand, ModelRef, PermissionProfile, PlanStepStatus, RuntimeEvent, SessionId,
    UiExecutionTask, UiPlan, UiPlanStep, UiSessionSnapshot,
};
use leveler_tui::action::{Action, Effect};
use leveler_tui::active_goal::GoalPhase;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use leveler_tui::transcript::TranscriptItem;

fn state() -> AppState {
    let mut state = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s-goal"),
            user: "u".to_string(),
            version: "0.1.0".to_string(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    state.size = (100, 40);
    state
}

fn paused_goal(objective: &str) -> UiExecutionTask {
    UiExecutionTask {
        goal_id: "g-1".into(),
        objective: objective.into(),
        running: false,
        user_paused: true,
    }
}

fn running_goal(objective: &str) -> UiExecutionTask {
    UiExecutionTask {
        goal_id: "g-1".into(),
        objective: objective.into(),
        running: true,
        user_paused: false,
    }
}

fn snapshot(status: &str, task: Option<UiExecutionTask>) -> UiSessionSnapshot {
    UiSessionSnapshot {
        history: None,
        id: SessionId::new("s-goal"),
        repository: Some("/repo".to_string()),
        task_status: None,
        task_terminal: None,
        goal: "会话标题".to_string(),
        model: ModelRef::parse("deepseek/v3"),
        mode: PermissionProfile::Assisted,
        branch: None,
        status: status.to_string(),
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        latest_turn_ordinal: None,
        active_tools: Vec::new(),
        interrupted_tool_calls: Vec::new(),
        active_background_tasks: Vec::new(),
        background_tasks: Vec::new(),
        plan: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        thinking: None,
        work_profile: None,
        collaboration: Some("chat".to_string()),
        execution_task: task,
        children: Vec::new(),
        foreign_origin: None,
    }
}

fn header(state: &AppState) -> String {
    leveler_tui::active_goal::header(state, 80)
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn render(state: &mut AppState) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(100, 40)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..40 {
        let mut x = 0u16;
        while x < 100 {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        out.push('\n');
    }
    out
}

/// Chat → `commit_task` → user pause → ordinary question: the paused goal is
/// still owed and still paused. The turn that answered the question did not
/// settle the goal, so the badge may not disappear with that turn.
#[test]
fn an_ordinary_question_does_not_drop_a_paused_chat_goal() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(
                "interrupted",
                Some(paused_goal("实现数据导出，阶段 1 后暂停")),
            ),
        }),
    );
    assert!(
        header(&state).contains("已暂停"),
        "a restored paused goal says so: {}",
        header(&state)
    );

    state.composer.replace("刚才阶段 1 改了哪些文件？");
    let effects = reduce(&mut state, key(KeyCode::Enter));
    assert!(
        matches!(
            &effects[..],
            [Effect::Submit {
                command: ClientCommand::SubmitMessage { .. },
                ..
            }]
        ),
        "an ordinary question is a Chat turn, not a goal continuation: {effects:?}"
    );

    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnAnswered));

    assert!(
        state.execution_task_active,
        "the goal still owes work after the question"
    );
    assert_eq!(
        state.active_goal.as_ref().map(|goal| goal.phase()),
        Some(GoalPhase::Paused),
        "answering a question is not the goal settling"
    );
    assert!(
        header(&state).contains("已暂停"),
        "the paused goal stays visible: {}",
        header(&state)
    );

    // The runtime republishes the durable row at the terminal; the projection
    // the client reads is the same one it had before the question.
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionUpdated {
            session: snapshot("idle", Some(paused_goal("实现数据导出，阶段 1 后暂停"))),
        }),
    );
    assert!(
        header(&state).contains("已暂停"),
        "the durable projection outranks the settled turn: {}",
        header(&state)
    );
}

/// No user text — however clearly it reads — releases a pause on the client.
/// The message goes out as an ordinary submission; the runtime's projection of
/// the goal (after the model declares the continuation) is what clears the
/// badge.
#[test]
fn an_authorized_continuation_releases_the_pause_on_the_runtimes_projection() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(
                "interrupted",
                Some(paused_goal("实现数据导出，阶段 1 后暂停")),
            ),
        }),
    );

    state.composer.replace("继续，把阶段 2 做完并验收。");
    let effects = reduce(&mut state, key(KeyCode::Enter));
    assert!(
        matches!(
            &effects[..],
            [Effect::Submit {
                command: ClientCommand::SubmitMessage { .. },
                ..
            }]
        ),
        "the client submits the message; it does not classify it: {effects:?}"
    );
    let phase = state.active_goal.as_ref().map(|goal| goal.phase());
    assert_eq!(
        phase,
        Some(GoalPhase::Paused),
        "the pause is durable truth; the client cannot read the user's words as its release"
    );
    assert!(
        header(&state).contains("已暂停"),
        "the paused badge stays until the runtime publishes the change: {}",
        header(&state)
    );

    // The runtime projected the declaration: the same goal is running now.
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionUpdated {
            session: snapshot("running", Some(running_goal("实现数据导出，阶段 1 后暂停"))),
        }),
    );
    let phase = state.active_goal.as_ref().map(|goal| goal.phase());
    assert!(
        matches!(phase, Some(GoalPhase::Running | GoalPhase::Resuming)),
        "the same goal resumes: {phase:?}"
    );
    assert!(
        !header(&state).contains("已暂停"),
        "the pause badge is gone once the runtime projects the release: {}",
        header(&state)
    );
}

/// The polite form takes the same path: one ordinary submission, no client-side
/// reading, and the runtime's projection is what changes the display.
#[test]
fn a_polite_continuation_is_routed_by_the_runtime_not_the_display() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(
                "interrupted",
                Some(paused_goal("实现数据导出，阶段 1 后暂停")),
            ),
        }),
    );
    state.composer.replace("确认，继续阶段 2，可以吗？");
    let effects = reduce(&mut state, key(KeyCode::Enter));
    assert!(
        matches!(
            &effects[..],
            [Effect::Submit {
                command: ClientCommand::SubmitMessage { .. },
                ..
            }]
        ),
        "the runtime owns the routing decision: {effects:?}"
    );
    let phase = state.active_goal.as_ref().map(|goal| goal.phase());
    assert_eq!(
        phase,
        Some(GoalPhase::Paused),
        "a question form is not an authorization the display may infer"
    );

    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionUpdated {
            session: snapshot("running", Some(running_goal("实现数据导出，阶段 1 后暂停"))),
        }),
    );
    let phase = state.active_goal.as_ref().map(|goal| goal.phase());
    assert!(
        matches!(phase, Some(GoalPhase::Running | GoalPhase::Resuming)),
        "the same goal resumes: {phase:?}"
    );
    assert!(
        !header(&state).contains("已暂停"),
        "the pause badge is gone once the runtime projects the release: {}",
        header(&state)
    );
}

/// Only a real goal terminal settles the task. A completed goal leaves no
/// owed-task badge, and a cancelled one leaves no stale pause.
#[test]
fn a_settled_or_cancelled_goal_leaves_no_pause_badge() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("interrupted", Some(paused_goal("实现数据导出"))),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::TurnPaused {
            reason: "目标已暂停 · 等待你授权继续".into(),
        }),
    );
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCompleted));
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionUpdated {
            session: snapshot("completed", None),
        }),
    );
    assert!(state.execution_goal.is_none(), "the goal settled");
    assert!(!state.execution_task_active);
    assert!(header(&state).is_empty(), "{}", header(&state));

    let mut state = self::state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("interrupted", Some(paused_goal("实现数据导出"))),
        }),
    );
    reduce(&mut state, Action::Runtime(RuntimeEvent::TaskCancelled));
    assert!(state.execution_goal.is_none(), "a cancelled goal is gone");
    assert!(!state.execution_task_active);
    assert!(header(&state).is_empty(), "{}", header(&state));
}

/// A session with no goal never grows a goal badge from a question.
#[test]
fn a_plain_chat_session_shows_no_goal_badge() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("idle", None),
        }),
    );
    state.composer.replace("测试通过了吗？");
    reduce(&mut state, key(KeyCode::Enter));
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnAnswered));
    assert!(header(&state).is_empty(), "{}", header(&state));
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
        state: Default::default(),
    }
}

/// Goal A's pause is not Goal B's authorization.
///
/// A completed goal A leaves its own plan in the conversation. Goal B's own
/// instruction was "finish it in one go", so the archived plan may not claim
/// the user is still holding anything back — the boundary belongs to A, and A
/// is over.
#[test]
fn another_goals_pause_does_not_mark_this_goal_as_waiting() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("idle", None),
        }),
    );
    // Goal A: the user asked for a pause, and its plan is archived history.
    state
        .transcript
        .push_user("现在先暂停，等我确认。".to_string());
    state.transcript.push_plan(plan(&[
        ("阶段 1：实现接口", PlanStepStatus::Done),
        ("阶段 2：写测试", PlanStepStatus::Pending),
    ]));
    // Goal B is the session's current, unpaused task.
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionUpdated {
            session: snapshot(
                "idle",
                Some(UiExecutionTask {
                    goal_id: "g-2".into(),
                    objective: "实现新的数据导出功能，一次完成。".into(),
                    running: false,
                    user_paused: false,
                }),
            ),
        }),
    );

    let screen = render(&mut state);
    assert!(
        !screen.contains("等待用户确认"),
        "goal A's pause is over and goal B never set one:\n{screen}"
    );
}

/// The same plan under its OWN goal's pause still says the user is holding the
/// next step. The label is bound to the goal, not deleted outright.
#[test]
fn the_paused_goals_own_plan_still_names_the_wait() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(
                "interrupted",
                Some(paused_goal("实现数据导出，阶段 1 后暂停")),
            ),
        }),
    );
    state
        .transcript
        .push_user("实现数据导出，阶段 1 后暂停。".to_string());
    state.transcript.push_plan(plan(&[
        ("阶段 1：实现接口", PlanStepStatus::Done),
        ("阶段 2：写测试", PlanStepStatus::Pending),
    ]));

    let screen = render(&mut state);
    assert!(
        screen.contains("等待用户确认"),
        "the paused goal's own plan names the wait:\n{screen}"
    );
    assert!(
        screen.contains("已暂停"),
        "and the goal header agrees:\n{screen}"
    );
}

/// A resume rebuilds the same projection from the durable snapshot, so the
/// isolation survives reopening the session.
#[test]
fn a_reopened_session_keeps_the_pause_bound_to_its_goal() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(
                "interrupted",
                Some(paused_goal("实现数据导出，阶段 1 后暂停")),
            ),
        }),
    );
    let messages = |state: &AppState| {
        state
            .transcript
            .items()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::User(_)))
            .count()
    };
    assert_eq!(messages(&state), 0);
    assert!(header(&state).contains("已暂停"), "{}", header(&state));

    // Reopening the same session (a reconnect) must not resurrect a different
    // goal's boundary: the projection is the only source.
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(
                "idle",
                Some(UiExecutionTask {
                    goal_id: "g-2".into(),
                    objective: "实现新的数据导出功能，一次完成。".into(),
                    running: false,
                    user_paused: false,
                }),
            ),
        }),
    );
    assert_eq!(
        state.active_goal.as_ref().map(|goal| goal.objective()),
        Some("实现新的数据导出功能，一次完成。"),
        "the header names the goal the runtime is actually holding"
    );
    assert!(
        !state
            .execution_goal
            .as_ref()
            .is_some_and(|goal| goal.user_paused),
        "goal B carries no pause of its own"
    );
}

/// Goal A's pause is A's fact.
///
/// When the session owes a DIFFERENT goal B, the header's active goal is B: a
/// live window on B, no badge from A, and no continuation phase inherited from
/// A's pause. Adopting a different goal is not "resuming the same goal", which
/// is what a continuation of A would be.
#[test]
fn a_running_second_goal_does_not_inherit_the_first_goals_pause() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("interrupted", Some(paused_goal("Goal A：实现数据导出"))),
        }),
    );
    assert_eq!(
        state.active_goal.as_ref().map(|goal| goal.phase()),
        Some(GoalPhase::Paused),
        "A is the paused goal"
    );
    assert!(header(&state).contains("已暂停"), "{}", header(&state));

    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionUpdated {
            session: snapshot(
                "running",
                Some(UiExecutionTask {
                    goal_id: "g-2".into(),
                    objective: "Goal B：修正导出边界".into(),
                    running: true,
                    user_paused: false,
                }),
            ),
        }),
    );

    let projection = state.execution_goal.clone().expect("B is the owed goal");
    assert_eq!(projection.goal_id, "g-2", "the projection names B");
    assert!(!projection.user_paused, "B carries no pause of its own");
    let goal = state.active_goal.as_ref().expect("an active goal");
    assert_eq!(
        goal.phase(),
        GoalPhase::Running,
        "B starts running; it is not a continuation of A"
    );
    assert_eq!(goal.title(), "Goal B：修正导出边界");
    assert!(
        !header(&state).contains("已暂停"),
        "A's pause is not B's state: {}",
        header(&state)
    );

    // A's own history is still readable as A: the durable row is what a client
    // re-reading the session sees.
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionUpdated {
            session: snapshot("idle", Some(paused_goal("Goal A：实现数据导出"))),
        }),
    );
    assert_eq!(
        state.active_goal.as_ref().map(|goal| goal.phase()),
        Some(GoalPhase::Paused),
        "A's pause is intact when the runtime projects A again"
    );
}
