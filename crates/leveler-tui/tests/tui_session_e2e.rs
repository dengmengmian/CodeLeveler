//! Headless "TUI session" tests: open a session, type slash commands, feed
//! runtime events, and assert what the screen would show (TestBackend).
//!
//! This is the closest automated stand-in for "start the TUI and click around"
//! without a real PTY or live model.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{
    ClientCommand, CommandId, MessageId, ObservationClass, PermissionProfile, PlanStepStatus,
    RuntimeEvent, SessionId, ToolCallId, UiDiff, UiDiffFile, UiObservabilityLoaded, UiPlan,
    UiPlanStep, UiRecoveryObservation, UiSessionObservation, UiSessionSnapshot, UiToolAggregate,
};
use leveler_tui::action::{Action, Effect};
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn typed(s: &mut AppState, text: &str) {
    for ch in text.chars() {
        reduce(s, key(KeyCode::Char(ch)));
    }
}

fn enter(s: &mut AppState) -> Vec<Effect> {
    reduce(s, key(KeyCode::Enter))
}

fn opened() -> AppState {
    let mut s = AppState::new(
        Theme::dark(),
        Boot {
            session_id: SessionId::new("e2e"),
            user: "tester".into(),
            version: "0.1.0".into(),
            show_welcome: true,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    );
    s.size = (100, 32);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: UiSessionSnapshot {
                id: SessionId::new("e2e"),
                repository: "~/Develop/demo".into(),
                goal: "e2e".into(),
                model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
                mode: PermissionProfile::Assisted,
                branch: Some("main".into()),
                status: "idle".into(),
                finalization_stage: None,
                messages: Vec::new(),
                pending_interactions: Vec::new(),
                available_models: vec![
                    leveler_client_protocol::ModelRef::parse("deepseek/v3").unwrap(),
                    leveler_client_protocol::ModelRef::parse("glm/5").unwrap(),
                ],
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
                reasoning: None,
                work_profile: None,
                collaboration: None,
                children: Vec::new(),
            },
        }),
    );
    s
}

fn screen(state: &mut AppState) -> String {
    let (w, h) = state.size;
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        // trim trailing spaces per row for readable asserts
        while out.ends_with(' ') {
            out.pop();
        }
        out.push('\n');
    }
    out
}

/// Write a multi-screen dump for human review.
fn dump_all(path: &str, pages: &[(&str, String)]) {
    let mut body = String::new();
    for (title, page) in pages {
        body.push_str(&format!("======== {title} ========\n"));
        body.push_str(page);
        body.push_str("\n\n");
    }
    std::fs::write(path, body).unwrap();
}

#[test]
fn tui_session_commands_ui_and_logic() {
    let mut s = opened();
    let mut pages: Vec<(&str, String)> = Vec::new();

    // --- idle conversation: welcome + composer ---
    let idle = screen(&mut s);
    assert!(
        idle.contains("CodeLeveler") || idle.contains("欢迎") || idle.contains("tester"),
        "welcome missing: {idle}"
    );
    assert!(
        idle.contains('›') || idle.contains(">"),
        "composer prompt: {idle}"
    );
    pages.push(("01-idle", idle));

    // --- /mode opens permission picker ---
    typed(&mut s, "/mode");
    enter(&mut s);
    assert!(s.overlay.is_some(), "mode picker open");
    let mode_ui = screen(&mut s);
    assert!(
        mode_ui.contains("只读") || mode_ui.contains("权限"),
        "mode overlay labels: {mode_ui}"
    );
    assert!(
        !mode_ui.contains("Workspace Write")
            || mode_ui.contains("可写")
            || mode_ui.contains("只读"),
        "should use permission wording: {mode_ui}"
    );
    pages.push(("03-mode-picker", mode_ui));
    reduce(&mut s, key(KeyCode::Esc));
    assert!(s.overlay.is_none());

    // --- /model opens model picker ---
    typed(&mut s, "/model");
    enter(&mut s);
    assert!(s.overlay.is_some());
    let model_ui = screen(&mut s);
    assert!(
        model_ui.contains("deepseek") || model_ui.contains("模型") || model_ui.contains("glm"),
        "model picker: {model_ui}"
    );
    pages.push(("04-model-picker", model_ui));
    reduce(&mut s, key(KeyCode::Esc));

    // --- slash arg ghost (composer on the workbench screen) ---
    typed(&mut s, "/btw ");
    let ghost_ui = screen(&mut s);
    assert!(
        ghost_ui.contains("<问题>") || ghost_ui.contains("<question>"),
        "ghost missing in composer: {ghost_ui}"
    );
    assert_eq!(s.composer.text(), "/btw ", "ghost must not enter buffer");
    pages.push(("05-btw-ghost-composer", ghost_ui));
    // clear composer
    while !s.composer.is_empty() {
        reduce(&mut s, key(KeyCode::Backspace));
    }

    typed(&mut s, "/goal ");
    let ghost_ui = screen(&mut s);
    assert!(
        ghost_ui.contains("<任务") || ghost_ui.contains("<goal>"),
        "goal ghost: {ghost_ui}"
    );
    while !s.composer.is_empty() {
        reduce(&mut s, key(KeyCode::Backspace));
    }

    // --- /btw side thread: its own surface, not a card in the main history ---
    typed(&mut s, "/btw 还完事了吗？");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(
        s.surface,
        leveler_tui::btw::SurfaceFocus::Btw,
        "submitting /btw must land on the side thread"
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BtwStarted {
            question: "还完事了吗？".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::BtwTextDelta {
            delta: "审查完成。**没有明显问题**。\n\n- 编译通过\n- 测试通过".into(),
        }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::BtwCompleted));
    let btw_ui = screen(&mut s);
    assert!(
        btw_ui.contains("返回主线程") || btw_ui.contains("Main"),
        "side-thread header: {btw_ui}"
    );
    assert!(btw_ui.contains("还完事了吗？"), "side question: {btw_ui}");
    assert!(btw_ui.contains("没有明显问题"), "side answer: {btw_ui}");
    assert!(
        !btw_ui.contains("**"),
        "side thread must render markdown, not raw **: {btw_ui}"
    );
    pages.push(("06-btw-surface", btw_ui));
    // Esc returns to the main surface; it is navigation, not a cancel.
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.surface, leveler_tui::btw::SurfaceFocus::Main);
    // `/btw` alone re-opens the same thread without retyping the question.
    typed(&mut s, "/btw");
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.surface, leveler_tui::btw::SurfaceFocus::Btw);
    assert_eq!(
        s.btw.turns.len(),
        1,
        "re-entry must not create a second thread"
    );
    reduce(&mut s, key(KeyCode::Esc));

    // Plan updates still land on state (shown in conversation chrome).
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: UiPlan {
                steps: vec![
                    UiPlanStep {
                        index: 0,
                        description: "定位相关代码".into(),
                        status: PlanStepStatus::Done,
                    },
                    UiPlanStep {
                        index: 1,
                        description: "修复两个 bug".into(),
                        status: PlanStepStatus::Running,
                    },
                ],
            },
        }),
    );
    assert!(
        s.plan.as_ref().is_some_and(|p| p
            .steps
            .iter()
            .any(|st| st.description.contains("定位相关代码"))),
        "plan should stay on state for chrome"
    );

    // --- /diff screen ---
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::DiffUpdated {
            diff: UiDiff {
                files: vec![UiDiffFile {
                    path: "src/main.rs".into(),
                    added: 3,
                    removed: 1,
                    patch: Some("@@\n-old line\n+new line\n+another\n+third\n context line".into()),
                }],
            },
        }),
    );
    typed(&mut s, "/diff");
    enter(&mut s);
    assert_eq!(s.active_screen, Screen::Diff);
    let diff_ui = screen(&mut s);
    assert!(
        diff_ui.contains("src/main.rs") || diff_ui.contains("main.rs"),
        "diff files: {diff_ui}"
    );
    assert!(
        diff_ui.contains("+3") || diff_ui.contains("+"),
        "diff stats: {diff_ui}"
    );
    pages.push(("08-diff", diff_ui));
    reduce(&mut s, key(KeyCode::Esc));

    // --- tool preview ANSI stripped ---
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("t-ansi"),
            name: "run_command".into(),
            arguments: r#"{"program":"vitest"}"#.into(),
            parallel: false,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: None,
            id: ToolCallId::new("t-ansi"),
            ok: true,
            preview: "\u{1b}[32m✓\u{1b}[39m all tests passed".into(),
            duration_ms: 1200,
            applied_diff: None,
        }),
    );
    let preview = s
        .transcript
        .tool_calls()
        .iter()
        .find(|t| t.id.as_str() == "t-ansi")
        .and_then(|t| t.preview.clone())
        .unwrap_or_default();
    assert!(
        preview.contains('✓') && preview.contains("all tests passed"),
        "kept: {preview}"
    );
    assert!(
        !preview.contains('\u{1b}') && !preview.contains("[32m"),
        "ANSI leak: {preview}"
    );

    // --- streaming assistant (no panic render) ---
    let mid = MessageId::new("a1");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: mid.clone(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: mid.clone(),
            delta: "正在修复 **两个** bug。".into(),
        }),
    );
    let stream_ui = screen(&mut s);
    assert!(!stream_ui.is_empty(), "streaming frame should paint");
    pages.push(("09-streaming", stream_ui));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: mid }),
    );

    // --- incomplete turn shows reason ---
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnIncomplete {
            reason: "轮次或资源预算已耗尽".into(),
        }),
    );
    let incomplete_ui = screen(&mut s);
    assert!(
        incomplete_ui.contains("未完成")
            || incomplete_ui.contains("incomplete")
            || incomplete_ui.contains("预算"),
        "incomplete marker: {incomplete_ui}"
    );
    pages.push(("10-incomplete", incomplete_ui));

    // --- /help ---
    typed(&mut s, "/help");
    enter(&mut s);
    assert_eq!(s.active_screen, Screen::Help);
    let help = screen(&mut s);
    assert!(
        help.contains("/goal") || help.contains("/model") || help.contains("帮助"),
        "help lists commands: {help}"
    );
    assert!(
        !help.contains("/workflow") && !help.contains("/steps") && !help.contains("/confirm-plan"),
        "removed commands must not appear in help: {help}"
    );
    pages.push(("11-help", help));
    reduce(&mut s, key(KeyCode::Esc));

    // --- permission label on status after mode Write ---
    assert_eq!(s.mode_label, "Assisted");

    // Persist dump for manual review
    let dump_path = std::env::temp_dir().join("leveler-tui-session-e2e.txt");
    dump_all(dump_path.to_str().unwrap(), &pages);
    eprintln!("TUI e2e dump written to {}", dump_path.display());
}

#[test]
fn tui_slash_popup_lists_renamed_commands() {
    let mut s = opened();
    typed(&mut s, "/");
    let matches = leveler_tui::screen::visible_slash_popup(&s);
    let names: Vec<_> = matches.iter().map(|(n, _)| n.as_str()).collect();
    assert!(!names.contains(&"/workflow"), "removed: {names:?}");
    assert!(!names.contains(&"/wf"), "removed alias: {names:?}");
    assert!(!names.contains(&"/agent"), "old /agent must not appear");
    assert!(!names.contains(&"/steps"), "removed: {names:?}");
    // `/agents` is back as a read-only registry view, but searchable only:
    // the empty-`/` popup stays the high-frequency core.
    assert!(
        !names.contains(&"/agents"),
        "searchable, not core: {names:?}"
    );
    assert!(!names.contains(&"/context"), "removed: {names:?}");
    assert!(!names.contains(&"/verify"), "removed: {names:?}");
    assert!(!names.contains(&"/confirm-plan"), "removed: {names:?}");
    assert!(
        !names.contains(&"/plan"),
        "plan is `/collab plan`, not a second entry: {names:?}"
    );
    assert!(
        !names.contains(&"/work-mode"),
        "work-mode is searchable, not on empty /: {names:?}"
    );
    assert!(names.contains(&"/collab"), "got {names:?}");
    assert!(names.contains(&"/goal"), "got {names:?}");
    assert!(names.contains(&"/new"), "got {names:?}");
    assert!(
        !names.contains(&"/fork"),
        "fork is searchable, not on empty /: {names:?}"
    );

    let popup_ui = screen(&mut s);
    assert!(
        popup_ui.contains("/goal") || popup_ui.contains("/help"),
        "popup on screen: {popup_ui}"
    );
}

#[test]
fn tui_esc_dismisses_slash_popup() {
    let mut s = opened();
    typed(&mut s, "/");
    assert!(!leveler_tui::screen::visible_slash_popup(&s).is_empty());
    reduce(&mut s, key(KeyCode::Esc));
    assert!(leveler_tui::screen::visible_slash_popup(&s).is_empty());
    assert_eq!(s.composer.text(), "/");
}

#[test]
fn tui_trace_queries_durable_observatory_and_esc_returns() {
    let mut s = opened();
    typed(&mut s, "/trace");
    let effects = enter(&mut s);
    assert_eq!(s.active_screen, Screen::Trace);
    let query_id = sent_query_id(&effects);
    assert!(
        query_id.is_some(),
        " /trace must query the runtime, not SQLite: {effects:?}"
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id,
            observation: UiObservabilityLoaded {
                session: UiSessionObservation {
                    session_id: SessionId::new("e2e"),
                    goal: "e2e".into(),
                    repository: "/repo".into(),
                    created_at: "t".into(),
                    updated_at: "t".into(),
                    status: "completed".into(),
                    model: "glm-5.2".into(),
                    work_profile: "balanced".into(),
                    collaboration: "goal".into(),
                    last_sequence: Some(12),
                    request_count: 3,
                    input_tokens: 1000,
                    output_tokens: 40,
                    avg_latency_ms: Some(2000),
                    last_latency_ms: Some(1800),
                    request_failures: 0,
                    request_retries: 0,
                    tool_started: 5,
                    tool_finished: 5,
                    compact_count: 0,
                    subagent_started: 0,
                    duration_ms: None,
                    cached_input_tokens: None,
                    cost_usd_micros: None,
                    lanes: Vec::new(),
                },
                window: Vec::new(),
                window_from: 1,
                window_to: 12,
                requests: Vec::new(),
                tools: vec![UiToolAggregate {
                    name: "read_file".into(),
                    class: ObservationClass::Read,
                    calls: 40,
                    succeeded: 40,
                    failed: 0,
                    unfinished: 0,
                    total_ms: Some(1840),
                    avg_ms: Some(46),
                }],
                agents: Vec::new(),
                recovery: UiRecoveryObservation {
                    interrupted_turns: 0,
                    workspace_snapshots: 0,
                    review_stages: Vec::new(),
                },
                relations: Vec::new(),
            },
        }),
    );
    let ui = screen(&mut s);
    assert!(
        ui.contains("Observatory") || ui.contains("MODEL") || ui.contains("glm"),
        "trace screen: {ui}"
    );
    reduce(&mut s, key(KeyCode::Char('4')));
    let tools_ui = screen(&mut s);
    assert!(
        tools_ui.contains("全会话汇总")
            && tools_ui.contains("read_file")
            && tools_ui.contains("40"),
        "Tools tab is session-wide, not the event window: {tools_ui}"
    );
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
}

fn sent_query_id(effects: &[Effect]) -> Option<CommandId> {
    effects.iter().find_map(|e| match e {
        Effect::Send(ClientCommand::QueryObservability {
            query_id: Some(id), ..
        }) => Some(id.clone()),
        _ => None,
    })
}

fn trace_observation(window_from: i64, window_to: i64) -> UiObservabilityLoaded {
    UiObservabilityLoaded {
        session: UiSessionObservation {
            session_id: SessionId::new("e2e"),
            goal: "e2e".into(),
            repository: "/repo".into(),
            created_at: "t".into(),
            updated_at: "t".into(),
            status: "completed".into(),
            model: "glm-5.2".into(),
            work_profile: "balanced".into(),
            collaboration: "goal".into(),
            last_sequence: Some(100),
            request_count: 3,
            input_tokens: 1000,
            output_tokens: 40,
            avg_latency_ms: Some(2000),
            last_latency_ms: Some(1800),
            request_failures: 0,
            request_retries: 0,
            tool_started: 5,
            tool_finished: 5,
            compact_count: 0,
            subagent_started: 0,
            duration_ms: None,
            cached_input_tokens: None,
            cost_usd_micros: None,
            lanes: Vec::new(),
        },
        window: Vec::new(),
        window_from,
        window_to,
        requests: Vec::new(),
        tools: Vec::new(),
        agents: Vec::new(),
        recovery: UiRecoveryObservation {
            interrupted_turns: 0,
            workspace_snapshots: 0,
            review_stages: Vec::new(),
        },
        relations: Vec::new(),
    }
}

#[test]
fn tui_trace_ignores_observability_loaded_for_a_foreign_query() {
    let mut s = opened();
    typed(&mut s, "/trace");
    let effects = enter(&mut s);
    let owned = sent_query_id(&effects).expect("trace query");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id: Some(CommandId::new("foreign-historical")),
            observation: trace_observation(20, 60),
        }),
    );
    assert!(
        s.trace.loaded.is_none(),
        "foreign query must not populate /trace"
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id: Some(owned),
            observation: trace_observation(21, 100),
        }),
    );
    assert_eq!(s.trace.loaded.as_ref().map(|l| l.window_from), Some(21));
    assert_eq!(s.trace.loaded.as_ref().map(|l| l.window_to), Some(100));
}

#[test]
fn tui_trace_drops_a_stale_owned_query_after_refresh() {
    let mut s = opened();
    typed(&mut s, "/trace");
    let first = enter(&mut s);
    let query_a = sent_query_id(&first).expect("first query");
    let second = reduce(&mut s, key(KeyCode::Char('r')));
    let query_b = sent_query_id(&second).expect("refresh query");
    assert_ne!(query_a, query_b);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id: Some(query_b.clone()),
            observation: trace_observation(21, 100),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id: Some(query_a),
            observation: trace_observation(20, 60),
        }),
    );
    assert_eq!(s.trace.loaded.as_ref().map(|l| l.window_from), Some(21));
    assert_eq!(s.trace.loaded.as_ref().map(|l| l.window_to), Some(100));
}

#[test]
fn tui_trace_ignores_legacy_observability_loaded_without_query_id() {
    let mut s = opened();
    typed(&mut s, "/trace");
    let _ = enter(&mut s);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id: None,
            observation: trace_observation(20, 60),
        }),
    );
    assert!(
        s.trace.loaded.is_none(),
        "1.5 uncorrelated payload must not populate a current /trace"
    );
}

/// A message written while a turn runs is held in 待发送, and the moment that
/// turn reaches its terminal event it becomes the next turn on its own — with
/// no keystroke, and without ever reading as `状态未知` while it only waited.
#[test]
fn a_queued_message_continues_as_the_next_turn_when_the_runtime_is_ready() {
    let mut s = opened();

    // First turn: submitted, optimistically busy, then admitted by the runtime.
    typed(&mut s, "重构 HTTP 层");
    let first: Vec<Effect> = enter(&mut s);
    let first_id = match first.as_slice() {
        [Effect::Submit { command_id, .. }] => command_id.clone(),
        other => panic!("the first message must be submitted: {other:?}"),
    };
    assert!(s.is_busy());
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: leveler_client_protocol::UiMessage {
                id: MessageId::new("u1"),
                role: leveler_client_protocol::UiRole::User,
                text: "重构 HTTP 层".into(),
                ordinal: None,
                kind: None,
                images: 0,
            },
        }),
    );
    reduce(
        &mut s,
        Action::EffectCompleted(leveler_tui::action::EffectCompletion::SubmissionDelivered {
            command_id: first_id,
            snapshot: None,
        }),
    );

    // While it runs, the user writes the follow-up.
    typed(&mut s, "完事之后提交远端。");
    assert!(
        enter(&mut s).is_empty(),
        "a busy turn holds the follow-up, it does not send it"
    );
    let held = screen(&mut s);
    let row = held
        .lines()
        .find(|line| line.contains("完事之后提交远端。"))
        .unwrap_or_else(|| panic!("the queued row is on screen: {held}"));
    assert!(
        !row.contains("状态未知"),
        "a queued item has a known state: {row:?}"
    );

    // The running turn ends. Nothing else happens: the queue advances.
    let advances = reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    let advance_id = match advances.as_slice() {
        [Effect::Submit { command, command_id }] => {
            assert!(
                matches!(command, ClientCommand::SubmitMessage { content, .. } if content == "完事之后提交远端。"),
                "the queued message starts the next turn: {command:?}"
            );
            command_id.clone()
        }
        // The terminal event may also issue a history query; the submission is
        // the effect that matters.
        other => other
            .iter()
            .find_map(|effect| match effect {
                Effect::Submit { command, command_id } => {
                    assert!(
                        matches!(command, ClientCommand::SubmitMessage { content, .. } if content == "完事之后提交远端。"),
                        "the queued message starts the next turn: {command:?}"
                    );
                    Some(command_id.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("the queued message must be submitted: {other:?}")),
    };

    // Admitted: out of 待发送, and it is the newest user turn.
    reduce(
        &mut s,
        Action::EffectCompleted(leveler_tui::action::EffectCompletion::SubmissionDelivered {
            command_id: advance_id,
            snapshot: None,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: leveler_client_protocol::UiMessage {
                id: MessageId::new("u2"),
                role: leveler_client_protocol::UiRole::User,
                text: "完事之后提交远端。".into(),
                ordinal: None,
                kind: None,
                images: 0,
            },
        }),
    );
    assert!(s.pending_inputs.is_empty(), "accepted leaves 待发送");
    let after = screen(&mut s);
    assert!(
        !after.contains("待发送 · 1"),
        "the 待发送 area is gone once admitted: {after}"
    );
    assert!(
        after.contains("完事之后提交远端。"),
        "the message is now a user turn: {after}"
    );
    assert!(s.is_busy(), "its own turn is under way");
}
