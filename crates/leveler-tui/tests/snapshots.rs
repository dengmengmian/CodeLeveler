//! Render snapshot tests across terminal sizes (§69.5): the shell draws the
//! welcome header, status line, and composer without panicking, and stays
//! usable at tiny sizes.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use unicode_width::UnicodeWidthStr;

use leveler_client_protocol::{
    ApprovalId, MessageId, PermissionProfile, RuntimeEvent, SessionId, ToolCallId,
    UiApprovalRequest, UiMessage, UiRole, UiSessionSnapshot,
};
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

fn opened_state() -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "麻凡".to_string(),
            version: "0.1.0".to_string(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
        },
    );
    let snap = UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: "~/Develop/codeleveler".to_string(),
        goal: "interactive session".to_string(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
        mode: PermissionProfile::Assisted,
        branch: Some("main".to_string()),
        status: "idle".to_string(),
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        plan: None,
        verification: None,
        diff: None,
        checkpoints: Vec::new(),
        completion_report: None,
    };
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    s
}

fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
    let buf = terminal.backend().buffer();
    let area = buf.area;
    let mut out = String::new();
    for y in 0..area.height {
        let mut x = 0;
        while x < area.width {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            // A double-width grapheme owns the next cell too; skip it so the
            // scan does not inject a phantom space between wide characters.
            x += (sym.width().max(1)) as u16;
        }
        out.push('\n');
    }
    out
}

fn render_at(width: u16, height: u16, state: &mut AppState) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, state)).unwrap();
    buffer_text(&terminal)
}

/// Last `n` buffer lines — sticky footer/input chrome only (splash may use ↑/↓).
fn sticky_chrome(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[test]
fn renders_welcome_header_and_composer_at_standard_sizes() {
    let mut state = opened_state();
    for (w, h) in [(80u16, 24u16), (120, 40), (160, 50)] {
        let text = render_at(w, h, &mut state);
        assert!(text.contains("CodeLeveler"), "header missing at {w}x{h}");
        assert!(
            !text.contains("欢迎回来"),
            "welcome card must be gone at {w}x{h}"
        );
        // Input border: model · auto (English product terms).
        assert!(
            text.contains(" · auto") || text.contains("auto"),
            "permission chip missing at {w}x{h}: {text}"
        );
        assert!(text.contains('›'), "composer prompt missing at {w}x{h}");
        // The bottom strip stays a trust chip, not a shortcut bar — the full
        // key list lives in /help · Ctrl+?. The one hint row under the composer
        // is deliberate and transient (it vanishes as soon as you type; see
        // `the_hints_get_out_of_the_way_once_you_type`), so the assertion is on
        // the sticky strip rather than the whole frame.
        // The last row is the trust strip; the hint row sits above it.
        let bottom = sticky_chrome(&text, 1);
        assert!(
            !bottom.contains("Ctrl+C")
                && !bottom.contains("Ctrl+M")
                && !bottom.contains("Ctrl+Q")
                && !bottom.contains("Ctrl+O")
                && !bottom.contains("Ctrl+?"),
            "shortcuts must not be sticky footer chrome at {w}x{h}: {bottom}"
        );
        // Fresh session: no token/context dump on sticky footer/input chrome.
        // (Splash hint may mention ↑ history — only check the bottom strip.)
        let chrome = sticky_chrome(&text, 6);
        assert!(
            !chrome.contains("↑") && !chrome.contains("↓"),
            "idle token counts should be hidden at {w}x{h}: {chrome}"
        );
        assert!(
            !chrome.contains("Context "),
            "fresh-session context line should be hidden at {w}x{h}: {chrome}"
        );
    }
}

#[test]
fn context_chip_appears_on_footer_after_usage_update() {
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ContextUpdated {
            estimated_tokens: 24_000,
            candidate_files: Vec::new(),
        }),
    );

    let text = render_at(100, 24, &mut state);
    // Footer is context-only: Context 24k/200k (compact, not full commas).
    assert!(
        text.contains("Context 24k/"),
        "footer context line missing: {text}"
    );
    let chrome = sticky_chrome(&text, 6);
    assert!(
        !chrome.contains("Ctrl+C") && !chrome.contains("↑") && !chrome.contains("替我审批"),
        "keys/token/legacy perm must stay off sticky chrome: {chrome}"
    );
}

#[test]
fn footer_shows_cache_hit_rate_when_provider_reports_it() {
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::TokenUsage {
            input_tokens: 1000,
            output_tokens: 50,
            cached_input_tokens: 700,
        }),
    );
    // token_input drives used when context_tokens is still low.
    state.context_window_tokens = 200_000;
    state.context_tokens = 1000;
    let text = render_at(100, 24, &mut state);
    assert!(
        text.contains("Context") && text.contains("cache 70%"),
        "footer should show Context + cache: {text}"
    );
}

#[test]
fn empty_session_shows_splash_with_logo() {
    let mut state = opened_state();
    let text = render_at(100, 24, &mut state);
    assert!(
        text.contains("CodeLeveler") && text.contains('█'),
        "empty session splash missing brand/logo: {text}"
    );
}

/// End-to-end through the real render path: a repository shipping ignored
/// `.leveler` config must be visible on screen, because the CLI's stderr notice
/// is swallowed by the alternate screen. The splash carries the detail and the
/// composer border carries a marker that outlives the splash.
#[test]
fn ignored_in_repo_config_is_visible_on_screen() {
    let mut state = opened_state();
    let clean = render_at(100, 24, &mut state);
    assert!(!clean.contains('⚠'), "clean repo shows no warning: {clean}");

    state.untrusted_config = vec![".leveler/hooks.yaml".to_string()];
    let text = render_at(100, 24, &mut state);
    assert!(
        text.contains("hooks.yaml"),
        "splash must name the file: {text}"
    );
    assert!(
        text.contains("leveler trust"),
        "splash must name the fix: {text}"
    );

    // Once the conversation has content the splash is gone, but the composer
    // border marker must remain — the condition still holds.
    state.transcript.push_user("开始干活".into());
    let after = render_at(100, 24, &mut state);
    assert!(
        after.contains('⚠'),
        "the marker must outlive the splash: {after}"
    );
}

#[test]
fn token_usage_does_not_spam_input_or_footer() {
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::TokenUsage {
            input_tokens: 1200,
            output_tokens: 300,
            cached_input_tokens: 0,
        }),
    );

    let text = render_at(120, 24, &mut state);
    let chrome = sticky_chrome(&text, 6);
    // ↑↓ call stats are not sticky chrome; model · auto stays on the input border.
    assert!(
        !chrome.contains('↑') && !chrome.contains('↓'),
        "↑↓ token stats must not sticky-spam: {chrome}"
    );
    assert!(
        text.contains("auto") || text.contains("deepseek"),
        "model/permission chip missing: {text}"
    );
    assert!(
        !chrome.contains("Ctrl+C") && !chrome.contains("Ctrl+M"),
        "shortcuts must not reappear on footer: {chrome}"
    );
}

#[test]
fn shows_typed_and_streamed_text() {
    let mut state = opened_state();
    // Type into the composer.
    for ch in "hello".chars() {
        reduce(
            &mut state,
            Action::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(ch),
                crossterm::event::KeyModifiers::empty(),
            )),
        );
    }
    // Stream an assistant reply.
    let id = leveler_client_protocol::MessageId::new("m1");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: id.clone(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: "答复内容".into(),
        }),
    );

    let text = render_at(100, 20, &mut state);
    assert!(text.contains("hello"), "composer text not rendered");
    assert!(text.contains("答复内容"), "assistant stream not rendered");
}

#[test]
fn conversation_messages_use_prompt_and_bullet_without_role_labels() {
    let mut state = opened_state();
    state.transcript.push_user("请检查项目".into());
    let id = leveler_client_protocol::MessageId::new("message-style");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: id.clone(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: "开始检查".into(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: id }),
    );

    let text = render_at(100, 24, &mut state);
    assert!(text.contains("▌ 请检查项目"), "{text}");
    assert!(text.contains("● 开始检查"), "{text}");
    assert!(!text.lines().any(|line| line.trim() == "User"), "{text}");
    assert!(
        !text.lines().any(|line| line.trim() == "Agent Message"),
        "{text}"
    );
}

#[test]
fn renders_approval_overlay_with_deny_visible() {
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ApprovalRequested {
            request: UiApprovalRequest {
                id: ApprovalId::new("r1"),
                tool: "run_command".into(),
                summary: "run git push".into(),
                command: Some("git push".into()),
                risks: vec!["将访问网络".into()],
            },
        }),
    );
    let text = render_at(100, 24, &mut state);
    assert!(text.contains("需要权限"), "approval title missing");
    assert!(text.contains("git push"), "command missing");
    assert!(text.contains("拒绝"), "deny option missing");
    assert!(
        text.contains("始终允许") || text.contains("项目规则"),
        "always option missing: {text}"
    );
}

#[test]
fn renders_failed_tool_inline_and_tools_screen() {
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("t1"),
            name: "run_command".into(),
            arguments: "cargo test".into(),
            parallel: false,
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("t1"),
            ok: false,
            preview: "exit: 101\n--- stderr ---\ncompiler error".into(),
            duration_ms: 13000,
        }),
    );
    // Failed Important tools: compact activity line + first error line only.
    let conv = render_at(100, 24, &mut state);
    assert!(
        conv.contains('✗') || conv.contains("!"),
        "failed activity glyph missing: {conv}"
    );
    assert!(
        conv.contains("cargo test"),
        "collapsed failed tool should show its target: {conv}"
    );
    assert!(
        conv.contains("exit") && conv.contains("101"),
        "collapsed failed tool should keep the first error line: {conv}"
    );
    assert!(
        !conv.contains("compiler error"),
        "collapsed stderr tail must not flood Conversation: {conv}"
    );
    assert!(
        !conv.contains("工具调用"),
        "group summary should not replace the activity stream: {conv}"
    );

    // Open the Tools screen.
    reduce(
        &mut state,
        Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('t'),
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
    let tools = render_at(100, 24, &mut state);
    assert!(tools.contains("工具"), "tools screen title missing");
    assert!(
        tools.contains("run_command · cargo test"),
        "tools screen list should include the tool target"
    );
    assert!(tools.contains("Esc 返回"), "tools screen footer missing");

    let narrow_tools = render_at(80, 24, &mut state);
    assert!(
        narrow_tools.contains("Esc 返回"),
        "80-column tools footer should preserve the full return hint"
    );
}

#[test]
fn ok_tool_output_folds_then_expands_with_ctrl_o() {
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("t1"),
            name: "run_command".into(),
            arguments: r#"{"program":"cargo","args":["test"]}"#.into(),
            parallel: false,
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "line-one\nline-two\nline-three".into(),
            duration_ms: 10,
        }),
    );

    // Folded by default: Important activity line only; raw output stays hidden.
    let folded = render_at(100, 24, &mut state);
    assert!(
        folded.contains('✓') && folded.contains("cargo"),
        "activity line missing: {folded}"
    );
    assert!(!folded.contains("line-one"), "collapsed output leaked");
    assert!(
        !folded.contains("line-three"),
        "later lines should be hidden"
    );

    // Ctrl+O expands the current (latest) tool group only.
    reduce(
        &mut state,
        Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('o'),
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
    let expanded = render_at(100, 24, &mut state);
    assert!(
        expanded.contains("line-three"),
        "expanded output should show all lines: {expanded}"
    );
    if let Some(leveler_tui::transcript::TranscriptItem::ToolGroup(g)) =
        state.transcript.items().last()
    {
        assert!(g.expanded, "latest group must be expanded");
    }
}

#[test]
fn command_result_renders_as_important_activity_not_file_list() {
    let mut state = opened_state();
    let message_id = leveler_client_protocol::MessageId::new("agent-1");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: message_id.clone(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: message_id.clone(),
            delta: "正在检查项目".into(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("command-1"),
            name: "run_command".into(),
            arguments: r#"{"program":"cargo","args":["test","-p","leveler-tui"]}"#.into(),
            parallel: false,
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("command-1"),
            ok: true,
            preview: "exit: 0\n--- stdout ---\nraw-shell-output\nsecond-line\n".into(),
            duration_ms: 1250,
        }),
    );

    let collapsed = render_at(120, 30, &mut state);
    assert!(
        !collapsed.lines().any(|line| line.trim() == "Agent Message"),
        "{collapsed}"
    );
    assert!(!collapsed.contains("工具输出"), "{collapsed}");
    assert!(!collapsed.contains("摘要:"), "{collapsed}");
    assert!(collapsed.contains('✓'), "{collapsed}");
    assert!(
        collapsed.contains("执行") && collapsed.contains("cargo"),
        "important run must show verb + target: {collapsed}"
    );
    assert!(
        !collapsed.contains("raw-shell-output"),
        "collapsed stdout must not enter the conversation text flow: {collapsed}"
    );

    reduce(
        &mut state,
        Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('o'),
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
    let expanded = render_at(120, 30, &mut state);
    assert!(expanded.contains("详情"), "{expanded}");
    assert!(expanded.contains("raw-shell-output"), "{expanded}");
    assert!(expanded.contains("second-line"), "{expanded}");
}

#[test]
fn running_command_renders_as_progress_activity() {
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("command-running"),
            name: "run_command".into(),
            arguments: r#"{"program":"cargo","args":["check"]}"#.into(),
            parallel: false,
        }),
    );

    let text = render_at(100, 24, &mut state);
    assert!(
        text.contains('◌') && text.contains("执行") && text.contains("cargo"),
        "running important tool must show progress: {text}"
    );
    assert!(!text.contains("工具活动"), "{text}");
    // Internal exploration noise must not appear as a path dump.
    assert!(!text.contains("✓ ."), "{text}");
}

#[test]
fn list_files_scan_stays_out_of_conversation() {
    let mut state = opened_state();
    for (id, path) in [
        ("l1", "."),
        ("l2", "cmd"),
        ("l3", "internal/admin"),
        ("l4", "Makefile"),
    ] {
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::ToolCallStarted {
                id: ToolCallId::new(id),
                name: "list_files".into(),
                arguments: format!(r#"{{"path":"{path}"}}"#),
                parallel: false,
            }),
        );
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::ToolCallCompleted {
                id: ToolCallId::new(id),
                ok: true,
                preview: format!("{path}\nPROJECT_RULES.md\nMakefile"),
                duration_ms: 3,
            }),
        );
    }
    // A real edit should still surface.
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("edit"),
            name: "apply_patch".into(),
            arguments: r#"{"patch":"*** Begin Patch\n*** Update File: internal/admin/web/web.go\n*** End Patch"}"#.into(),
            parallel: false,
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("edit"),
            ok: true,
            preview: "ok".into(),
            duration_ms: 8,
        }),
    );

    let text = render_at(100, 24, &mut state);
    assert!(
        !text.contains("PROJECT_RULES.md"),
        "file-list probe content must not enter Conversation: {text}"
    );
    assert!(
        !text.contains("✓ .") && !text.contains("列目录"),
        "silent list_files activity must stay hidden: {text}"
    );
    assert!(
        text.contains("web.go") || text.contains("编辑"),
        "important edit must remain visible: {text}"
    );
}

#[test]
fn renders_plan_chrome_and_diff_screen() {
    use leveler_client_protocol::{PlanStepStatus, UiDiff, UiDiffFile, UiPlan, UiPlanStep};
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: UiPlan {
                steps: vec![UiPlanStep {
                    index: 0,
                    description: "调整页面布局".into(),
                    status: PlanStepStatus::Running,
                }],
            },
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::DiffUpdated {
            diff: UiDiff {
                files: vec![UiDiffFile {
                    path: "src/login.rs".into(),
                    added: 12,
                    removed: 4,
                    patch: Some("+added line\n-removed line".into()),
                }],
            },
        }),
    );

    // Plan has no dedicated slash/screen anymore — it lives in conversation chrome.
    let conv = render_at(100, 24, &mut state);
    assert!(
        conv.contains("调整页面布局") || conv.contains("1/1") || conv.contains("计划"),
        "plan chrome missing: {conv}"
    );

    let ctrl_key = |c: char| {
        Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::CONTROL,
        ))
    };
    reduce(&mut state, ctrl_key('d')); // open diff
    let diff = render_at(100, 24, &mut state);
    assert!(diff.contains("src/login.rs"), "diff file missing");
    assert!(diff.contains("+12"), "diff added count missing");
}

#[test]
fn renders_completion_block() {
    use leveler_client_protocol::UiCompletionReport;
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionCompleted {
            report: UiCompletionReport {
                files_changed: 3,
                added: 86,
                removed: 31,
                checks_passed: 4,
                checks_total: 4,
                success: true,
            },
        }),
    );
    let text = render_at(100, 24, &mut state);
    assert!(text.contains("任务已完成"), "completion header missing");
    assert!(text.contains("修改 3 个文件"), "completion summary missing");
}

#[test]
fn second_render_reflects_new_content_not_a_stale_cache() {
    let mut state = opened_state();

    let m1 = leveler_client_protocol::MessageId::new("m1");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: m1.clone(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: m1.clone(),
            delta: "first answer".into(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: m1 }),
    );
    let first = render_at(100, 24, &mut state);
    assert!(first.contains("first answer"), "first render: {first}");

    // Add a second message and re-render the SAME state: the conversation-line
    // cache must invalidate so the new content shows and the old one stays.
    let m2 = leveler_client_protocol::MessageId::new("m2");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: m2.clone(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: m2.clone(),
            delta: "second answer".into(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: m2 }),
    );
    let second = render_at(100, 24, &mut state);
    assert!(
        second.contains("second answer"),
        "cache must not hide new content: {second}"
    );
    assert!(
        second.contains("first answer"),
        "prior content must remain: {second}"
    );
}

#[test]
fn completed_turn_omits_recap_and_does_not_guess_input_suggestion() {
    let mut state = opened_state();
    let id = leveler_client_protocol::MessageId::new("handoff");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: id.clone(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: "权限检查已经完成。\n\n下一步：运行标签预置脚本。".into(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: id }),
    );
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCompleted));

    assert!(
        state.composer.is_empty(),
        "freeform next-step prose must not prefill the composer"
    );
    let text = render_at(100, 24, &mut state);
    assert!(
        !text.contains("recap:"),
        "freeform answer must not become a recap: {text}"
    );
    // Once the conversation has real turns, the empty-composer hint no longer
    // repeats — it is a first-run cue only.
    assert!(
        !text.contains("输入消息") && !text.contains("Type a message"),
        "hint should not repeat after a completed turn: {text}"
    );
}

#[test]
fn recap_does_not_render_raw_markdown_markers() {
    let mut state = opened_state();
    let id = ToolCallId::new("recap-md");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: id.clone(),
            name: "update_goal".into(),
            arguments: serde_json::json!({
                "status": "complete",
                "summary": "**构建和测试已经完成。**",
                "next_step": "运行 `release` 发布流程"
            })
            .to_string(),
            parallel: false,
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id,
            ok: true,
            preview: "目标已完成".into(),
            duration_ms: 10,
        }),
    );
    reduce(&mut state, Action::Runtime(RuntimeEvent::TurnCompleted));

    let text = render_at(100, 24, &mut state);
    assert!(text.contains("回顾:"), "localized recap missing: {text}");
    assert!(!text.contains("**"), "raw markdown leaked: {text}");
    assert!(text.contains("release"), "next step missing: {text}");
}

#[test]
fn completed_markdown_message_is_rendered_not_raw() {
    let mut state = opened_state();
    let id = leveler_client_protocol::MessageId::new("m1");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: id.clone(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: "## 标题\n\n这是 **加粗** 文本。\n\n```rust\nfn a() {}\n```".into(),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: id }),
    );

    let text = render_at(100, 24, &mut state);
    assert!(text.contains("加粗"), "bold text content present");
    assert!(!text.contains("**加粗**"), "raw ** markers must be gone");
    assert!(text.contains("标题"), "heading text present");
    assert!(text.contains("fn a()"), "code block content present");
}

#[test]
fn tiny_terminal_does_not_panic() {
    let mut state = opened_state();
    // Well below the 80x24 target: must degrade, not crash (§65).
    let _ = render_at(20, 5, &mut state);
    let _ = render_at(1, 1, &mut state);
}

/// An interrupt nobody can find is not an interrupt: while a turn runs, the key
/// that stops it has to be on screen. These drive the real `render`, not the
/// footer helper — a hint that only exists in a helper is a hint nobody sees.
#[test]
fn a_running_turn_shows_how_to_interrupt_it() {
    let mut state = opened_state();
    state.status = leveler_client_protocol::RuntimeStatus::Busy;
    let text = render_at(100, 24, &mut state);
    assert!(
        text.contains("Esc"),
        "no way to discover the interrupt:\n{text}"
    );
}

#[test]
fn an_untouched_composer_points_at_the_openers() {
    let mut state = opened_state();
    let text = render_at(100, 24, &mut state);
    assert!(text.contains("Shift+Tab"), "{text}");
    assert!(text.contains('@'), "{text}");
}

#[test]
fn the_hints_get_out_of_the_way_once_you_type() {
    let mut state = opened_state();
    state.composer.replace("修一下登录");
    let text = render_at(100, 24, &mut state);
    assert!(
        !text.contains("Shift+Tab"),
        "hints must not nag while composing:\n{text}"
    );
}

/// The layout must not jump when the hints appear and disappear: a row that
/// steals height from the conversation on every keystroke would reflow the
/// transcript under the user's eyes.
#[test]
fn showing_and_hiding_the_hints_keeps_the_conversation_the_same_height() {
    let mut state = opened_state();
    let empty = render_at(100, 24, &mut state);
    state.composer.replace("修一下登录");
    let typing = render_at(100, 24, &mut state);
    assert_eq!(
        empty.lines().count(),
        typing.lines().count(),
        "frame height changed with the hint row"
    );
}

/// A short conversation must sit against the composer, the way terminal output
/// does — not pin to the top of the viewport and leave a band of dead space
/// between the last line and the input box.
#[test]
fn a_short_conversation_sits_against_the_composer() {
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "改个字".into(),
            },
        }),
    );

    let text = render_at(100, 30, &mut state);
    let lines: Vec<&str> = text.lines().collect();
    let content = lines
        .iter()
        .position(|l| l.contains("改个字"))
        .expect("the message must be on screen");
    let composer = lines
        .iter()
        .position(|l| l.contains('›'))
        .expect("composer must be on screen");

    // The filler belongs ABOVE the content: several blank rows between the
    // header rule and the first message, and none of consequence below it.
    let blank_above = lines[..content]
        .iter()
        .rev()
        .take_while(|l| l.trim().is_empty())
        .count();
    let blank_below = lines[content + 1..composer]
        .iter()
        .filter(|l| l.trim().is_empty())
        .count();
    assert!(
        blank_above >= 3,
        "a short transcript must be pushed down by filler, got {blank_above} blank rows above\n{text}"
    );
    assert!(
        blank_below <= 2,
        "content must sit against the composer, got {blank_below} blank rows below\n{text}"
    );
}

/// A tool group whose tools are all Silent (probe runs like `ls`) renders
/// nothing — and must not leave the separator behind either. Otherwise the
/// reader sees an unexplained hole between their prompt and the answer.
#[test]
fn a_silent_tool_group_leaves_no_hole() {
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "看看有多少文件".into(),
            },
        }),
    );
    // A probe run: Silent by taxonomy, so it contributes no visible line.
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("t1"),
            name: "run_command".into(),
            arguments: r#"{"program":"ls","args":["-la"]}"#.into(),
            parallel: false,
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "924 entries".into(),
            duration_ms: 120,
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new("a1"),
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: MessageId::new("a1"),
            delta: "924 个文件".into(),
        }),
    );

    let text = render_at(100, 30, &mut state);
    let lines: Vec<&str> = text.lines().collect();
    let prompt = lines
        .iter()
        .position(|l| l.contains("看看有多少文件"))
        .expect("prompt on screen");
    let answer = lines
        .iter()
        .position(|l| l.contains("924 个文件"))
        .expect("answer on screen");
    let blanks = lines[prompt + 1..answer]
        .iter()
        .filter(|l| l.trim().is_empty())
        .count();
    assert!(
        blanks <= 1,
        "a silent tool must not leave {blanks} blank rows between prompt and answer\n{text}"
    );
}

/// Narrow terminals must stay *usable*, not merely survive. A split pane at 60
/// columns is normal; if the answer or the composer is gone at that width the
/// pane is useless even though nothing crashed.
#[test]
fn a_narrow_terminal_keeps_the_answer_and_the_composer() {
    for width in [60u16, 70, 80] {
        let mut state = opened_state();
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::UserMessageAdded {
                message: UiMessage {
                    id: MessageId::new("u1"),
                    role: UiRole::User,
                    text: "问题".into(),
                },
            }),
        );
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::AssistantMessageStarted {
                message_id: MessageId::new("a1"),
            }),
        );
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::AssistantTextDelta {
                message_id: MessageId::new("a1"),
                delta: "这是答案".into(),
            }),
        );
        let text = render_at(width, 24, &mut state);
        assert!(text.contains("这是答案"), "answer lost at {width} cols:\n{text}");
        assert!(text.contains('›'), "composer lost at {width} cols:\n{text}");
    }
}

/// Nothing may print past the terminal edge: an over-wide line wraps in the
/// user's terminal and shears the whole frame.
#[test]
fn no_rendered_line_exceeds_the_terminal_width() {
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                // No spaces: a naive wrapper has nowhere to break.
                text: "验证一个非常长的不带空格的中文串".repeat(6),
            },
        }),
    );
    for (w, h) in [(60u16, 24u16), (100, 30), (120, 40)] {
        let text = render_at(w, h, &mut state);
        for (i, line) in text.lines().enumerate() {
            let printed = UnicodeWidthStr::width(line.trim_end());
            assert!(
                printed <= w as usize,
                "row {i} is {printed} cols wide at {w}x{h}:\n{line}"
            );
        }
    }
}

/// The approval overlay replaces the composer, so it must not also leave the
/// conversation stranded at the top with dead space — the decision should sit
/// where the input box was, right under the work that prompted it.
#[test]
fn the_approval_overlay_sits_where_the_composer_was() {
    let mut state = opened_state();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "推一下代码".into(),
            },
        }),
    );
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ApprovalRequested {
            request: UiApprovalRequest {
                id: ApprovalId::new("r1"),
                tool: "run_command".into(),
                summary: "run git push".into(),
                command: Some("git push".into()),
                risks: vec!["将访问网络".into()],
            },
        }),
    );
    let text = render_at(100, 30, &mut state);
    let lines: Vec<&str> = text.lines().collect();
    let prompt = lines
        .iter()
        .position(|l| l.contains("推一下代码"))
        .expect("the prompt must stay visible while deciding");
    let overlay = lines
        .iter()
        .position(|l| l.contains("需要权限"))
        .expect("the approval must be on screen");
    assert!(
        overlay > prompt,
        "the decision belongs below the work — prompt at {prompt}, overlay at {overlay}:\n{text}"
    );
    let blanks = lines[prompt + 1..overlay]
        .iter()
        .filter(|l| l.trim().is_empty())
        .count();
    assert!(
        blanks <= 3,
        "{blanks} blank rows between the prompt and the approval:\n{text}"
    );
}

#[test]
fn the_splash_sits_in_the_middle_of_an_empty_session() {
    // Bottom-aligning a transcript is right: output grows upward from the
    // composer. An empty session has no transcript — it has a title card, and a
    // title card shoved against the input box with a screenful of void above it
    // reads as a layout bug, not a welcome.
    let mut state = opened_state();
    let text = render_at(120, 40, &mut state);
    let lines: Vec<&str> = text.lines().collect();

    // The composer box starts at column 0; the splash card is indented.
    let composer = lines
        .iter()
        .position(|l| l.starts_with('╭'))
        .expect("composer must be on screen");
    // The conversation area begins under the header rule.
    let rule = lines
        .iter()
        .position(|l| l.starts_with('─'))
        .expect("header rule must be on screen");

    let card_top = (rule + 1..composer)
        .find(|&i| !lines[i].trim().is_empty())
        .expect("splash card must be on screen");
    let card_bottom = (rule + 1..composer)
        .rev()
        .find(|&i| !lines[i].trim().is_empty())
        .unwrap();

    let above = card_top - (rule + 1);
    let below = composer - (card_bottom + 1);
    assert!(
        above.abs_diff(below) <= 2,
        "splash is not centred: {above} rows above, {below} below\n{text}"
    );
}
