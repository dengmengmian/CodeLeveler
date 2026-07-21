//! Headless "TUI session" tests: open a session, type slash commands, feed
//! runtime events, and assert what the screen would show (TestBackend).
//!
//! This is the closest automated stand-in for "start the TUI and click around"
//! without a real PTY or live model.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{
    MessageId, PermissionProfile, PlanStepStatus, RuntimeEvent, SessionId, ToolCallId, UiDiff,
    UiDiffFile, UiPlan, UiPlanStep, UiSessionSnapshot,
};
use leveler_tui::action::{Action, Effect};
use leveler_tui::reducer::reduce;
use leveler_tui::render::{conversation_footer, render};
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn ctrl(c: char) -> Action {
    Action::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
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
                messages: Vec::new(),
                pending_interactions: Vec::new(),
                available_models: vec![
                    leveler_client_protocol::ModelRef::parse("deepseek/v3").unwrap(),
                    leveler_client_protocol::ModelRef::parse("glm/5").unwrap(),
                ],
                vision: false,
                last_sequence: None,
                active_tools: Vec::new(),
                plan: None,
                verification: None,
                diff: None,
                checkpoints: Vec::new(),
                completion_report: None,
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

fn footer_text(state: &AppState) -> String {
    let (lines, _) = conversation_footer(state, state.size.0 as usize, 0, 0, false);
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
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

    // --- /workflow toggle logic + notification ---
    assert!(!s.orchestrate);
    typed(&mut s, "/workflow");
    let effects = enter(&mut s);
    assert!(s.orchestrate, "workflow should enable orchestrate");
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(
                leveler_client_protocol::ClientCommand::SetAgentMode {
                    orchestrate: true,
                    ..
                }
            )]
        ),
        "must send SetAgentMode: {effects:?}"
    );
    assert!(
        s.notification
            .as_ref()
            .is_some_and(|n| n.message.contains("编排") || n.message.contains("workflow")),
        "notify: {:?}",
        s.notification
    );
    pages.push(("02-workflow-on", screen(&mut s)));

    typed(&mut s, "/wf");
    enter(&mut s);
    assert!(!s.orchestrate, "/wf toggles back to direct");

    // --- /mode opens permission picker (not Plan steps) ---
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

    // --- slash arg ghost (footer / conversation path) ---
    typed(&mut s, "/btw ");
    let foot = footer_text(&s);
    assert!(
        foot.contains("<问题>") || foot.contains("<question>"),
        "ghost missing in footer: {foot}"
    );
    assert_eq!(s.composer.text(), "/btw ", "ghost must not enter buffer");
    pages.push(("05-btw-ghost-footer", foot));
    // clear composer
    while !s.composer.is_empty() {
        reduce(&mut s, key(KeyCode::Backspace));
    }

    typed(&mut s, "/goal ");
    let foot = footer_text(&s);
    assert!(
        foot.contains("<任务") || foot.contains("<goal>"),
        "goal ghost: {foot}"
    );
    while !s.composer.is_empty() {
        reduce(&mut s, key(KeyCode::Backspace));
    }

    // --- /btw side card with markdown answer ---
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
    let btw_foot = footer_text(&s);
    assert!(
        btw_foot.contains("临时提问") || btw_foot.contains("btw"),
        "btw card title: {btw_foot}"
    );
    assert!(
        btw_foot.contains("没有明显问题"),
        "btw answer text: {btw_foot}"
    );
    assert!(
        !btw_foot.contains("**"),
        "btw must render markdown, not raw **: {btw_foot}"
    );
    pages.push(("06-btw-card", btw_foot));

    // --- /steps (plan) screen ---
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
    typed(&mut s, "/steps");
    enter(&mut s);
    assert_eq!(s.active_screen, Screen::Plan);
    let steps_ui = screen(&mut s);
    assert!(
        steps_ui.contains("任务步骤")
            || steps_ui.contains("Task steps")
            || steps_ui.contains("步骤"),
        "steps title: {steps_ui}"
    );
    assert!(
        steps_ui.contains("定位相关代码") && steps_ui.contains("修复两个 bug"),
        "steps body: {steps_ui}"
    );
    pages.push(("07-steps", steps_ui));
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);

    // Ctrl+P still opens steps
    reduce(&mut s, ctrl('p'));
    assert_eq!(s.active_screen, Screen::Plan);
    reduce(&mut s, key(KeyCode::Esc));

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
            id: ToolCallId::new("t-ansi"),
            ok: true,
            preview: "\u{1b}[32m✓\u{1b}[39m all tests passed".into(),
            duration_ms: 1200,
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
    let foot = footer_text(&s);
    assert!(
        foot.contains("未完成") || foot.contains("incomplete") || foot.contains("预算"),
        "incomplete marker: {foot}"
    );
    pages.push(("10-incomplete", foot));

    // --- /help ---
    typed(&mut s, "/help");
    enter(&mut s);
    assert_eq!(s.active_screen, Screen::Help);
    let help = screen(&mut s);
    assert!(
        help.contains("/workflow") || help.contains("workflow") || help.contains("编排"),
        "help lists workflow: {help}"
    );
    assert!(
        help.contains("/steps") || help.contains("步骤"),
        "help lists steps: {help}"
    );
    pages.push(("11-help", help));
    reduce(&mut s, key(KeyCode::Esc));

    // --- /agents screen ---
    typed(&mut s, "/agents");
    enter(&mut s);
    assert_eq!(s.active_screen, Screen::Agents);
    pages.push(("12-agents", screen(&mut s)));
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
    let names: Vec<_> = matches.iter().map(|(n, _)| *n).collect();
    assert!(names.contains(&"/workflow"), "got {names:?}");
    assert!(names.contains(&"/steps"), "got {names:?}");
    assert!(!names.contains(&"/agent"), "old /agent must not appear");
    // /steps = task plan screen; /plan = collaboration Plan mode (not a legacy alias).
    assert!(names.contains(&"/steps"));
    assert!(
        names.contains(&"/plan"),
        "menu should list collab /plan alongside /steps: {names:?}"
    );
    assert!(names.contains(&"/work-mode"), "got {names:?}");
    assert!(names.contains(&"/collab"), "got {names:?}");
    assert!(names.contains(&"/confirm-plan"), "got {names:?}");

    let foot = footer_text(&s);
    assert!(
        foot.contains("/workflow") || foot.contains("编排") || foot.contains("workflow"),
        "popup in footer: {foot}"
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
