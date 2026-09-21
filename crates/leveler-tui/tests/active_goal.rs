//! Lifecycle and rendering of the header's Active Goal indicator, driven
//! through the public reducer exactly as the event loop drives it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{
    MessageId, ModelRef, PermissionProfile, RuntimeEvent, RuntimeStatus, SessionId,
    UiSessionSnapshot,
};
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use unicode_width::UnicodeWidthStr;

fn state() -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".to_string(),
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

fn snapshot(goal: &str, status: &str) -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: "/repo".to_string(),
        goal: goal.to_string(),
        model: ModelRef::parse("deepseek/v3"),
        mode: PermissionProfile::Assisted,
        branch: Some("main".to_string()),
        status: status.to_string(),
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

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn submit(state: &mut AppState, text: &str) {
    reduce(state, Action::TextInput(text.to_string()));
    reduce(state, key(KeyCode::Enter));
}

fn rendered(state: &mut AppState, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| leveler_tui::render::render(frame, state))
        .unwrap();
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut x = 0;
        while x < buf.area.width {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += sym.width().max(1) as u16;
        }
        out.push('\n');
    }
    out
}

#[test]
fn a_submitted_turn_shows_a_running_goal_with_its_clock() {
    let mut state = state();
    submit(&mut state, "修复断线后任务续接");
    let goal = state
        .active_goal
        .as_ref()
        .expect("goal starts with the turn");
    assert_eq!(goal.title(), "修复断线后任务续接");
    assert_eq!(goal.phase(), leveler_tui::active_goal::GoalPhase::Running);

    let screen = rendered(&mut state, 80, 12);
    let header = screen.lines().nth(1).unwrap();
    assert!(
        header.starts_with(" CodeLeveler"),
        "identity intact: {header:?}"
    );
    assert!(header.contains("◆ 修复断线后任务续接 · 0s"), "{header:?}");
}

#[test]
fn no_goal_means_no_indicator() {
    let mut state = state();
    let screen = rendered(&mut state, 80, 12);
    let header = screen.lines().nth(1).unwrap();
    assert!(!header.contains('◆'));
    assert!(!header.contains('◐'));
    assert!(!header.contains('✓'));
}

#[test]
fn a_reconnect_to_a_running_session_adopts_its_goal() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("调查 Windows CI flaky test", "running"),
        }),
    );
    assert_eq!(state.status, RuntimeStatus::Busy);
    let goal = state.active_goal.as_ref().expect("live turn owns a goal");
    assert_eq!(goal.title(), "调查 Windows CI flaky test");
    let screen = rendered(&mut state, 80, 12);
    assert!(screen.contains("◆ 调查 Windows CI flaky test"), "{screen}");
}

#[test]
fn esc_pauses_the_goal_and_continue_resumes_the_same_one() {
    let mut state = state();
    submit(&mut state, "修复断线后任务续接");
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCancelled));
    let paused = state
        .active_goal
        .as_ref()
        .expect("an interrupted goal waits for the user");
    assert_eq!(paused.phase(), leveler_tui::active_goal::GoalPhase::Paused);
    assert_eq!(paused.title(), "修复断线后任务续接");
    let screen = rendered(&mut state, 80, 12);
    assert!(screen.contains("◐ 修复断线后任务续接"), "{screen}");

    // `继续` re-enters the same goal: same identity, no new goal, and the
    // `↻` mark while the runtime picks the task back up.
    submit(&mut state, "继续");
    let resuming = state.active_goal.as_ref().unwrap();
    assert_eq!(
        resuming.phase(),
        leveler_tui::active_goal::GoalPhase::Resuming
    );
    assert_eq!(resuming.title(), "修复断线后任务续接");
    let screen = rendered(&mut state, 80, 12);
    assert!(screen.contains("↻ 修复断线后任务续接"), "{screen}");

    // The first real model round settles it back to running.
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new("m1"),
        }),
    );
    assert_eq!(
        state.active_goal.as_ref().unwrap().phase(),
        leveler_tui::active_goal::GoalPhase::Running
    );
    let screen = rendered(&mut state, 80, 12);
    assert!(screen.contains("◆ 修复断线后任务续接"), "{screen}");
}

#[test]
fn a_reconnecting_runtime_shows_the_resuming_mark() {
    let mut state = state();
    submit(&mut state, "调查 Windows CI flaky test");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ModelRetrying {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 1000,
        }),
    );
    let screen = rendered(&mut state, 80, 12);
    assert!(screen.contains("↻ 调查 Windows CI flaky test"), "{screen}");
}

#[test]
fn completion_turns_green_and_fails_stay() {
    let mut completed_state = state();
    submit(&mut completed_state, "调查 Windows CI flaky test");
    // A real completion has an answer behind it; without one the runtime
    // rewrites the terminal to NoFinalAnswer (rendered as a failure).
    let id = MessageId::new("m1");
    reduce(
        &mut completed_state,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: id.clone(),
        }),
    );
    reduce(
        &mut completed_state,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: "done".into(),
        }),
    );
    reduce(
        &mut completed_state,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: id }),
    );
    reduce(
        &mut completed_state,
        Action::Runtime(RuntimeEvent::TurnCompleted),
    );
    let completed = completed_state.active_goal.as_ref().unwrap();
    assert_eq!(
        completed.phase(),
        leveler_tui::active_goal::GoalPhase::Completed
    );
    // Green, frozen, and not yet expired.
    let now = std::time::Instant::now();
    assert!(!completed.is_expired(now));
    assert!(!completed.is_expired(now + std::time::Duration::from_secs(3)));
    assert!(completed.is_expired(now + std::time::Duration::from_secs(60)));
    let screen = rendered(&mut completed_state, 80, 12);
    assert!(screen.contains("✓ 调查 Windows CI flaky test"), "{screen}");

    // A hard failure never times out.
    let mut failed = state();
    submit(&mut failed, "调查 Windows CI flaky test");
    reduce(
        &mut failed,
        Action::Runtime(RuntimeEvent::TurnFailed {
            error: "internal invariant violated".into(),
            failure: None,
        }),
    );
    let goal = failed.active_goal.as_ref().unwrap();
    assert_eq!(goal.phase(), leveler_tui::active_goal::GoalPhase::Failed);
    assert!(!goal.is_expired(std::time::Instant::now() + std::time::Duration::from_secs(3600)));
    let screen = rendered(&mut failed, 80, 12);
    assert!(screen.contains("! 调查 Windows CI flaky test"), "{screen}");
}

#[test]
fn a_new_goal_replaces_a_completed_one() {
    let mut state = state();
    submit(&mut state, "goal a");
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCompleted));
    submit(&mut state, "goal b");
    let goal = state.active_goal.as_ref().unwrap();
    assert_eq!(goal.title(), "goal b");
    assert_eq!(goal.phase(), leveler_tui::active_goal::GoalPhase::Running);
    let screen = rendered(&mut state, 80, 12);
    assert!(screen.contains("◆ goal b"), "{screen}");
    assert!(!screen.contains("goal a"), "{screen}");
}

#[test]
fn a_narrow_terminal_keeps_the_glyph_and_clock_and_never_overlaps() {
    let mut state = state();
    submit(
        &mut state,
        "修复断线 / ESC 后任务无法正确续接，需要调查 continuation runtime",
    );
    // Wide: full title.
    let wide = rendered(&mut state, 100, 12);
    assert!(wide.contains("修复断线"), "{wide}");
    // Medium: title truncated, clock intact, still one row.
    let medium = rendered(&mut state, 52, 12);
    let header = medium.lines().nth(1).unwrap();
    assert!(header.contains('◆'), "{header:?}");
    assert!(header.contains("0s"), "clock survives: {header:?}");
    assert!(header.contains('…'), "title truncated: {header:?}");
    // Narrow: identity and clock only.
    let narrow = rendered(&mut state, 20, 12);
    let header = narrow.lines().nth(1).unwrap();
    assert_eq!(header.width(), 20, "fills the row once: {header:?}");
    assert!(header.starts_with(" CodeLeveler"), "{header:?}");
    assert!(
        header.trim_end().ends_with("s ↗"),
        "clock survives: {header:?}"
    );
    assert!(
        !header.contains('…'),
        "title dropped, not clipped: {header:?}"
    );
    // Row count never changes with the goal present.
    assert_eq!(medium.lines().count(), wide.lines().count());
}

#[test]
fn a_wide_header_caps_the_goal_instead_of_sacrificing_repository_context() {
    let mut state = state();
    state.repository = "/workspace/CodeLeveler".into();
    state.branch = Some("main".into());
    submit(
        &mut state,
        "不要参考之前的文档了。我感觉之前的文档不对。我们要重新提出一模型。看要怎么做。",
    );

    let screen = rendered(&mut state, 120, 12);
    let header = screen.lines().nth(1).unwrap();
    assert!(
        header.contains("/workspace/CodeLeveler"),
        "a long goal must leave the repository context readable: {header:?}"
    );
    let goal = header.split('◆').nth(1).expect("running goal in header");
    assert!(
        UnicodeWidthStr::width(goal.trim()) <= 48,
        "goal chrome must have a fixed display-width ceiling: {header:?}"
    );
    assert!(
        header.contains('…'),
        "the capped goal should be explicit: {header:?}"
    );
}

#[test]
fn the_header_goal_opens_its_full_multiline_objective_with_keyboard() {
    let mut state = state();
    let objective = format!(
        "重新设计报价模型\n{}",
        (1..=20)
            .map(|index| format!("完整目标详情第{index}行：保留全部约束和验收条件"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    submit(&mut state, &objective);

    reduce(&mut state, key(KeyCode::Tab));
    reduce(&mut state, key(KeyCode::Tab));
    reduce(&mut state, key(KeyCode::Enter));

    let detail = rendered(&mut state, 80, 16);
    assert!(detail.contains("当前目标"), "{detail}");
    assert!(detail.contains("重新设计报价模型"), "{detail}");
    assert!(
        detail.contains("完整目标详情第1行：保留全部约束和验收条件"),
        "the detail page must retain the unabridged objective: {detail}"
    );
    reduce(&mut state, key(KeyCode::PageDown));
    let later = rendered(&mut state, 80, 16);
    assert!(later.contains("完整目标详情第20行"), "{later}");
    reduce(&mut state, key(KeyCode::Esc));
    let workbench = rendered(&mut state, 80, 16);
    assert!(workbench.contains("CodeLeveler"), "{workbench}");
    assert!(!workbench.contains("← 当前目标"), "{workbench}");
}

#[test]
fn clicking_the_header_goal_opens_the_same_detail_page() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut state = state();
    submit(&mut state, "检查完整目标的点击入口");
    let workbench = rendered(&mut state, 80, 12);
    let header = workbench.lines().nth(1).expect("header row");
    let goal_byte = header.find('◆').expect("goal affordance");
    let goal_column = UnicodeWidthStr::width(&header[..goal_byte]) as u16 + 2;

    reduce(
        &mut state,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: goal_column,
            row: 1,
            modifiers: KeyModifiers::empty(),
        }),
    );

    let detail = rendered(&mut state, 80, 12);
    assert!(detail.contains("← 当前目标"), "{detail}");
    assert!(detail.contains("检查完整目标的点击入口"), "{detail}");
}

#[test]
fn an_emoji_or_cjk_title_stays_on_one_row() {
    let mut state = state();
    submit(&mut state, "🚀 修复断线后任务续接");
    let screen = rendered(&mut state, 40, 12);
    let header = screen.lines().nth(1).unwrap();
    assert!(header.contains("🚀"), "{header:?}");
    assert!(!header.contains('\n'));
    assert_eq!(
        header.width(),
        40,
        "no wrap on a narrow-ish row: {header:?}"
    );
}
