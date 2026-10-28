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
            model_notice: None,
            thinking: None,
        },
    )
}

fn snapshot(goal: &str, status: &str) -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: Some("/repo".to_string()),
        task_status: None,
        task_terminal: None,
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
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        thinking: None,
        work_profile: None,
        collaboration: None,
        execution_task: None,
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

fn submit_goal(state: &mut AppState, text: &str) {
    submit(state, &format!("/goal {text}"));
    let command_id = state.pending_submissions.last().unwrap().command_id.clone();
    reduce(
        state,
        Action::EffectCompleted(leveler_tui::action::EffectCompletion::SubmissionDelivered {
            command_id,
            snapshot: None,
        }),
    );
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
    submit_goal(&mut state, "修复断线后任务续接");
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
    assert!(header.contains("◎ /goal 执行中 (0s)"), "{header:?}");
    assert!(
        header.contains("/goal 执行中"),
        "the right-hand goal keeps one column off the terminal edge: {header:?}"
    );
}

#[test]
fn no_goal_means_no_indicator() {
    let mut state = state();
    let screen = rendered(&mut state, 80, 12);
    let header = screen.lines().nth(1).unwrap();
    assert!(!header.contains('◎'));
    assert!(!header.contains('◐'));
    assert!(!header.contains('✓'));
}

#[test]
fn explicit_goal_header_distinguishes_submission_from_runtime_execution() {
    let mut state = state();
    submit(&mut state, "/goal 修复断线后任务续接");
    let pending = state.pending_submissions[0].command_id.clone();
    let screen = rendered(&mut state, 80, 12);
    let header = screen.lines().nth(1).unwrap();
    assert!(
        header.contains("/goal") && header.contains("发送中"),
        "{header}"
    );
    assert!(!header.contains("执行中"), "{header}");

    reduce(
        &mut state,
        Action::EffectCompleted(leveler_tui::action::EffectCompletion::SubmissionDelivered {
            command_id: pending,
            snapshot: None,
        }),
    );
    let screen = rendered(&mut state, 80, 12);
    let header = screen.lines().nth(1).unwrap();
    assert!(
        header.contains("/goal") && header.contains("执行中"),
        "{header}"
    );
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCancelled));
    let screen = rendered(&mut state, 80, 12);
    let header = screen.lines().nth(1).unwrap();
    assert!(
        header.contains("/goal") && header.contains("已暂停"),
        "{header}"
    );
}

#[test]
fn goal_mode_remains_visible_without_an_active_turn_and_clears_explicitly() {
    let mut state = state();
    let mut session = snapshot("修复断线", "idle");
    session.collaboration = Some("goal".into());
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened { session }),
    );
    for width in [40, 80] {
        let screen = rendered(&mut state, width, 12);
        let header = screen.lines().nth(1).unwrap();
        assert!(header.contains("/goal"), "width {width}: {header}");
        assert!(!header.contains("执行中"), "{header}");
    }
    submit(&mut state, "/goal clear");
    let screen = rendered(&mut state, 80, 12);
    assert!(!screen.lines().nth(1).unwrap().contains("/goal"));
}

#[test]
fn ordinary_chat_active_goal_does_not_claim_goal_mode() {
    let mut state = state();
    submit(&mut state, "修复断线");
    let screen = rendered(&mut state, 80, 12);
    let header = screen.lines().nth(1).unwrap();
    assert!(!header.contains("修复断线"), "{header}");
    assert!(!header.contains("/goal"), "{header}");
    assert!(!leveler_tui::active_goal::has_visible_goal(&state));
}

#[test]
fn goal_followups_keep_the_explicit_objective_until_a_new_goal_is_set() {
    let mut state = state();
    submit_goal(&mut state, "打通 MCP 页面操作");
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCancelled));
    submit(&mut state, "继续，按你说的顺序来。直到完成");
    let goal = state.active_goal.as_ref().unwrap();
    assert_eq!(goal.title(), "打通 MCP 页面操作");
    assert_eq!(goal.objective(), "打通 MCP 页面操作");
    let screen = rendered(&mut state, 100, 12);
    let header = screen.lines().nth(1).unwrap();
    assert!(header.contains("/goal"), "{header}");
    assert!(!header.contains("打通 MCP 页面操作"), "{header}");
    assert!(!header.contains("按你说的顺序"), "{header}");

    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCancelled));
    submit_goal(&mut state, "另一个明确目标");
    assert_eq!(
        state.active_goal.as_ref().unwrap().objective(),
        "另一个明确目标"
    );
}

#[test]
fn ordinary_followup_does_not_reopen_a_terminal_goal() {
    let mut state = state();
    submit_goal(&mut state, "已结算的明确目标");
    state
        .active_goal
        .as_mut()
        .unwrap()
        .complete(std::time::Instant::now());
    state.status = RuntimeStatus::Idle;
    submit(&mut state, "补充说明一下");
    let goal = state.active_goal.as_ref().unwrap();
    assert_eq!(goal.objective(), "已结算的明确目标");
    assert_eq!(goal.phase(), leveler_tui::active_goal::GoalPhase::Completed);
}

#[test]
fn a_reconnect_to_a_running_session_adopts_its_goal() {
    let mut state = state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: {
                let mut session = snapshot("调查 Windows CI flaky test", "running");
                session.collaboration = Some("goal".into());
                session
            },
        }),
    );
    assert_eq!(state.status, RuntimeStatus::Busy);
    let goal = state.active_goal.as_ref().expect("live turn owns a goal");
    assert_eq!(goal.title(), "调查 Windows CI flaky test");
    let screen = rendered(&mut state, 80, 12);
    assert!(screen.contains("◎ /goal 执行中 (0s)"), "{screen}");
}

#[test]
fn esc_pauses_the_goal_and_continue_resumes_the_same_one() {
    let mut state = state();
    submit_goal(&mut state, "修复断线后任务续接");
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCancelled));
    let paused = state
        .active_goal
        .as_ref()
        .expect("an interrupted goal waits for the user");
    assert_eq!(paused.phase(), leveler_tui::active_goal::GoalPhase::Paused);
    assert_eq!(paused.title(), "修复断线后任务续接");
    let screen = rendered(&mut state, 80, 12);
    assert!(screen.contains("◎ /goal 已暂停"), "{screen}");

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
    assert!(screen.contains("◎ /goal 发送中"), "{screen}");
    let command_id = state.pending_submissions.last().unwrap().command_id.clone();
    reduce(
        &mut state,
        Action::EffectCompleted(leveler_tui::action::EffectCompletion::SubmissionDelivered {
            command_id,
            snapshot: None,
        }),
    );
    let screen = rendered(&mut state, 80, 12);
    assert!(screen.contains("◎ /goal 正在恢复"), "{screen}");

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
    assert!(screen.contains("◎ /goal 执行中"), "{screen}");
}

#[test]
fn a_reconnecting_runtime_shows_the_resuming_mark() {
    let mut state = state();
    submit_goal(&mut state, "调查 Windows CI flaky test");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ModelRetrying {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 1000,
        }),
    );
    let screen = rendered(&mut state, 80, 12);
    assert!(screen.contains("◎ /goal 正在恢复"), "{screen}");
}

#[test]
fn completion_turns_green_and_fails_stay() {
    let mut completed_state = state();
    submit_goal(&mut completed_state, "调查 Windows CI flaky test");
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
    assert!(screen.contains("◎ /goal 已完成"), "{screen}");

    // A hard failure never times out.
    let mut failed = state();
    submit_goal(&mut failed, "调查 Windows CI flaky test");
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
    // The failure indicator stays (it is actionable) but must never read as a
    // live turn: it states the phase and drops the live marker.
    assert!(screen.contains("◎ /goal 失败"), "{screen}");
    assert!(
        screen.contains("失败"),
        "a failed turn says so in words: {screen}"
    );
    assert!(
        !screen.contains('↗'),
        "a failed turn never wears the live marker: {screen}"
    );
}

/// A resumable failure pauses the goal: the header must say `已暂停` instead of
/// leaving `◐ … ↗`, which is what the audit read as "still running".
#[test]
fn a_paused_turn_states_the_phase_and_drops_the_live_marker() {
    let mut state = state();
    submit_goal(&mut state, "跑完整测试");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::TurnFailed {
            error: "provider unavailable".into(),
            failure: Some(leveler_client_protocol::UiFailure {
                category: leveler_client_protocol::FailureCategory::Provider,
                source: leveler_client_protocol::FailureSource::Provider,
                provider: None,
                model: None,
                provider_code: None,
                request_id: None,
                status: Some(503),
                retries: Some(3),
                retryability: leveler_client_protocol::FailureRetryability::Safe,
                delivery: leveler_client_protocol::FailureDelivery::Responded,
                summary: "模型服务暂时不可用。".to_string(),
                detail: String::new(),
            }),
        }),
    );
    let goal = state.active_goal.as_ref().unwrap();
    assert_eq!(goal.phase(), leveler_tui::active_goal::GoalPhase::Paused);
    let screen = rendered(&mut state, 80, 12);
    assert!(screen.contains("◎ /goal 已暂停"), "{screen}");
    assert!(screen.contains("已暂停"), "the phase is stated: {screen}");
    assert!(
        !screen.contains('↗'),
        "a paused turn never wears the live marker: {screen}"
    );
}

#[test]
fn a_new_goal_replaces_a_completed_one() {
    let mut state = state();
    submit_goal(&mut state, "goal a");
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCompleted));
    submit_goal(&mut state, "goal b");
    let goal = state.active_goal.as_ref().unwrap();
    assert_eq!(goal.title(), "goal b");
    assert_eq!(goal.phase(), leveler_tui::active_goal::GoalPhase::Running);
    let screen = rendered(&mut state, 80, 12);
    assert!(screen.contains("◎ /goal 执行中"), "{screen}");
    assert!(!screen.contains("goal a"), "{screen}");
}

#[test]
fn a_narrow_terminal_keeps_the_glyph_and_clock_and_never_overlaps() {
    let mut state = state();
    submit_goal(
        &mut state,
        "修复断线 / ESC 后任务无法正确续接，需要调查 continuation runtime",
    );
    // Wide: compact mode, state and clock.
    let wide = rendered(&mut state, 100, 12);
    assert!(
        wide.lines().nth(1).unwrap().contains("/goal 执行中 (0s)"),
        "{wide}"
    );
    // Medium: the same badge and clock fit on one row.
    let medium = rendered(&mut state, 52, 12);
    let header = medium.lines().nth(1).unwrap();
    assert!(header.contains('◎'), "{header:?}");
    assert!(header.contains("0s"), "clock survives: {header:?}");
    assert!(
        !header.contains("修复断线"),
        "objective stays in detail: {header:?}"
    );
    // Narrow: identity and clock only.
    let narrow = rendered(&mut state, 20, 12);
    let header = narrow.lines().nth(1).unwrap();
    assert_eq!(header.width(), 20, "fills the row once: {header:?}");
    assert!(header.starts_with(" CodeLeveler"), "{header:?}");
    // At 20 columns the header keeps the product identity and the branch
    // glyph; the goal segment is dropped WHOLE (never a clipped `…`). The
    // mode/clock have no room here — the state strip owns them in this width.
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
    state.repository = Some("/workspace/CodeLeveler".into());
    state.branch = Some("main".into());
    submit_goal(
        &mut state,
        "不要参考之前的文档了。我感觉之前的文档不对。我们要重新提出一模型。看要怎么做。",
    );

    let screen = rendered(&mut state, 120, 12);
    let header = screen.lines().nth(1).unwrap();
    assert!(
        header.contains("/workspace/CodeLeveler"),
        "a long goal must leave the repository context readable: {header:?}"
    );
    let goal = header.split('◎').nth(1).expect("running goal in header");
    assert!(
        UnicodeWidthStr::width(goal.trim()) <= 48,
        "goal chrome must have a fixed display-width ceiling: {header:?}"
    );
    assert!(
        !header.contains("不要参考"),
        "the objective belongs in the detail page: {header:?}"
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
    submit_goal(&mut state, &objective);

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
    submit_goal(&mut state, "检查完整目标的点击入口");
    let workbench = rendered(&mut state, 80, 12);
    let header = workbench.lines().nth(1).expect("header row");
    let goal_byte = header.find('◎').expect("goal affordance");
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
    submit_goal(&mut state, "🚀 修复断线后任务续接");
    let screen = rendered(&mut state, 40, 12);
    let header = screen.lines().nth(1).unwrap();
    assert!(header.contains("/goal"), "{header:?}");
    assert!(!header.contains('\n'));
    assert_eq!(
        header.width(),
        40,
        "no wrap on a narrow-ish row: {header:?}"
    );
}

#[test]
fn compact_goal_badge_supports_both_interface_languages() {
    for (locale, expected) in [
        (leveler_tui::Locale::Zh, "◎ /goal 执行中 (0s)"),
        (leveler_tui::Locale::En, "◎ /goal active (0s)"),
    ] {
        let mut state = state();
        state.locale = locale;
        submit_goal(
            &mut state,
            "a long objective that belongs in the detail page",
        );
        for width in [40, 80] {
            let screen = rendered(&mut state, width, 12);
            let header = screen.lines().nth(1).unwrap();
            assert!(header.contains(expected), "{header}");
            assert!(!header.contains("a long objective"), "{header}");
        }
    }
}
