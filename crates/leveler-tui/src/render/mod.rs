//! Draws [`AppState`] to a Ratatui frame: header, transcript, status line, and
//! the composer. Layout degrades on narrow terminals .

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::Paragraph;

use crate::plan_cell::{render_agents_screen, render_plan_screen};
use crate::screen::Screen;
use crate::state::AppState;
use crate::status_line::{header_line, status_line_content};
use crate::tool_cell::render_tools_screen;
#[cfg(test)]
pub(crate) use crate::tool_cell::tool_action_label;
pub(crate) use crate::tool_cell::tool_summary;

mod footer;
mod panes;
mod screens;
mod text;
mod transcript_lines;

pub use footer::conversation_footer;
pub(crate) use footer::user_turn_summaries;
pub use transcript_lines::{
    assistant_split, item_is_final, item_render, items_need_gap, sub_agent_tree_lines,
};
pub(crate) use transcript_lines::{
    btw_card_lines, sub_agent_detail, sub_agent_display_name, sub_agent_status, sub_agent_usage,
};

pub(crate) use panes::{render_list_focused, render_scrolled};
pub(crate) use screens::screen_title;
pub(crate) use text::{truncate_display, wrap};

pub(crate) use footer::{
    COMPOSER_MAX_ROWS, composer_box_lines, composer_visible_rows, render_attachments,
    render_composer, render_slash_popup,
};
#[cfg(test)]
use ratatui::text::Line;
use screens::{
    render_context_screen, render_diff_screen, render_help_screen, render_sessions_screen,
    render_verification_screen,
};

/// Render the whole screen.
///
/// Conversation uses the workbench layout (Header / Conversation viewport /
/// Plan / Input / Footer). Other screens keep the classic full-screen panes.
pub fn render(frame: &mut Frame, state: &mut AppState) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }

    if state.active_screen == Screen::Conversation {
        crate::workbench::render_workbench(frame, state);
        return;
    }

    // Non-conversation screens: header + body + status + bordered composer.
    let composer_rows =
        composer_visible_rows(state, area.width as usize).clamp(3, COMPOSER_MAX_ROWS + 2) as u16;
    let attach_rows = if state.pending_attachments.is_empty() {
        0
    } else {
        1
    };

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(attach_rows),
        Constraint::Length(composer_rows),
    ])
    .split(area);

    frame.render_widget(
        Paragraph::new(header_line(state, area.width as usize)),
        chunks[0],
    );
    match state.active_screen {
        Screen::Conversation => unreachable!(),
        Screen::Tools => render_tools_screen(frame, chunks[1], state),
        Screen::Plan => render_plan_screen(frame, chunks[1], state),
        Screen::Diff => render_diff_screen(frame, chunks[1], state),
        Screen::Verification => render_verification_screen(frame, chunks[1], state),
        Screen::Sessions => render_sessions_screen(frame, chunks[1], state),
        Screen::Context => render_context_screen(frame, chunks[1], state),
        Screen::Agents => render_agents_screen(frame, chunks[1], state),
        Screen::Help => render_help_screen(frame, chunks[1], state),
    }
    frame.render_widget(
        Paragraph::new(status_line_content(state, area.width as usize)),
        chunks[2],
    );
    render_attachments(frame, chunks[3], state);
    render_composer(frame, chunks[4], state);

    if let Some(overlay) = &state.overlay {
        crate::overlay::render_overlay(frame, area, overlay, &state.theme);
    }
}

#[cfg(test)]
mod tests {
    use super::panes::pad_line_to_width;
    use super::text::sanitize_terminal_line;
    use super::{
        assistant_split, item_render, render, render_list_focused, render_scrolled,
        tool_action_label, tool_summary, truncate_display,
    };
    use crate::screen::Screen;
    use crate::state::{AppState, Boot};
    use crate::theme::Theme;
    use crate::transcript::{
        AssistantBlock, RecapBlock, ToolCallBlock, ToolGroupBlock, ToolStatus, TranscriptItem,
    };
    use leveler_client_protocol::{MessageId, SessionId, ToolCallId};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::text::Line;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn truncate_display_measures_by_width_not_char_count() {
        // 5 CJK chars = 10 cells. A budget of 6 must not return all 5 chars.
        let out = truncate_display("你好世界啊", 6);
        assert!(UnicodeWidthStr::width(out.as_str()) <= 6, "got {out:?}");
        assert!(out.ends_with('…'));
        // Fits exactly (no ellipsis).
        assert_eq!(truncate_display("abc", 3), "abc");
        assert_eq!(truncate_display("你好", 4), "你好");
    }

    #[test]
    fn partial_pane_rendering_clears_stale_line_tails() {
        let mut terminal = Terminal::new(TestBackend::new(24, 4)).unwrap();
        let state = test_state();
        let area = Rect::new(0, 0, 24, 2);

        terminal
            .draw(|frame| {
                render_scrolled(
                    frame,
                    area,
                    &state,
                    vec![Line::from("very very long stale tail")],
                )
            })
            .unwrap();
        terminal
            .draw(|frame| render_scrolled(frame, area, &state, vec![Line::from("short")]))
            .unwrap();

        let first = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .take(24)
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            !first.contains("stale") && !first.contains("tail"),
            "scrolled pane left stale text: {first:?}"
        );
        assert!(first.trim_end().ends_with("short"), "short row: {first:?}");

        terminal
            .draw(|frame| {
                render_list_focused(
                    frame,
                    area,
                    vec![Line::from("selected row with stale tail")],
                    0,
                )
            })
            .unwrap();
        terminal
            .draw(|frame| render_list_focused(frame, area, vec![Line::from("row")], 0))
            .unwrap();
        let first = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .take(24)
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            !first.contains("stale") && !first.contains("tail"),
            "focused list left stale text: {first:?}"
        );
        assert!(first.trim_end().ends_with("row"), "row: {first:?}");
    }

    #[test]
    fn scrolled_pane_clears_rows_that_scroll_off() {
        // Reproduces the Diff residual: longer lines leave right-hand fragments
        // of identifiers when a later frame paints shorter content into the
        // same rows (file switch / scroll past dense patch hunks).
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        let mut state = test_state();
        let area = Rect::new(0, 0, 20, 3);
        let dense = vec![
            Line::from("+AuthModule,Middleware"),
            Line::from("+OrgModule,WsModule,"),
            Line::from("+ProjectModule,Roles"),
        ];
        terminal
            .draw(|frame| render_scrolled(frame, area, &state, dense))
            .unwrap();

        // Simulate switching to a file whose patch is just the footer.
        let short = vec![Line::from("help")];
        state.screen_scroll = 0;
        terminal
            .draw(|frame| render_scrolled(frame, area, &state, short))
            .unwrap();
        let view: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        for ghost in ["Auth", "Module", "Middleware", "Org", "Project", "Roles"] {
            assert!(
                !view.contains(ghost),
                "file switch left residual {ghost:?} in {view:?}"
            );
        }
        assert!(view.contains("help"), "expected help in view: {view:?}");

        // Scroll a tall list so early rows leave the viewport entirely.
        let tall: Vec<Line<'static>> = (0..10)
            .map(|i| Line::from(format!("+ModuleName{i:02},tailXXXX")))
            .chain(std::iter::once(Line::from("help")))
            .collect();
        state.screen_scroll = 0;
        terminal
            .draw(|frame| render_scrolled(frame, area, &state, tall.clone()))
            .unwrap();
        state.screen_scroll = 100; // clamp to end
        terminal
            .draw(|frame| render_scrolled(frame, area, &state, tall))
            .unwrap();
        let view: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            !view.contains("ModuleName00") && !view.contains("tailXXXX"),
            "scroll-to-end left early residual: {view:?}"
        );
        assert!(view.contains("help"), "footer should be visible: {view:?}");
    }

    #[test]
    fn sanitize_terminal_line_expands_tabs_and_strips_controls() {
        let out = sanitize_terminal_line("a\tb\r\x1bc");
        assert!(!out.contains('\t'), "tab must expand: {out:?}");
        assert!(!out.contains('\r'), "cr must not reach Print: {out:?}");
        assert!(!out.contains('\u{1b}'), "esc must not reach Print: {out:?}");
        // "a" + 7 spaces to col 8 + "b" + space (for \r) + space (for esc) + "c"
        assert!(out.starts_with("a       b"), "tabstop-8 expand: {out:?}");
        assert_eq!(out, "a       b  c");
    }

    #[test]
    fn pad_line_to_width_fills_and_truncates() {
        let padded = pad_line_to_width(Line::from("hi"), 5);
        let w: usize = padded
            .spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        assert_eq!(w, 5);
        let truncated = pad_line_to_width(Line::from("hello world"), 5);
        let tw: usize = truncated
            .spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        assert_eq!(tw, 5);
    }

    // Replays the run-loop's progressive commit over cumulative streaming
    // snapshots (each a growing prefix; the last is the finished message) and
    // returns the lines committed to scrollback across all frames.
    fn simulate_stream(snapshots: &[&str]) -> Vec<String> {
        let theme = Theme::no_color();
        let width = 40;
        let mut committed: Vec<super::Line<'static>> = Vec::new();
        let mut assistant_lines = 0usize;
        for (i, text) in snapshots.iter().enumerate() {
            let done = i == snapshots.len() - 1;
            let block = AssistantBlock {
                id: MessageId::new("m1"),
                text: text.to_string(),
                done,
                rendered: done.then(|| crate::markdown::MdDoc::parse(text)),
            };
            let (full, stable) = assistant_split(&block, &theme, width);
            let upto = if done { full.len() } else { stable };
            if upto > assistant_lines {
                committed.extend(full[assistant_lines..upto].iter().cloned());
                assistant_lines = upto;
            }
        }
        committed.iter().map(line_text).collect()
    }

    fn line_text(line: &super::Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn progressive_commit_reproduces_full_message_exactly() {
        let theme = Theme::no_color();
        // A message that streams in over several frames, block by block.
        let final_text = "# Overview\n\nfirst point here\n\nsecond point here\n\nthird and last";
        let snapshots = [
            "# Over",
            "# Overview\n\nfirst point",
            "# Overview\n\nfirst point here\n\nsecond point here",
            "# Overview\n\nfirst point here\n\nsecond point here\n\nthird and last",
        ];
        let committed = simulate_stream(&snapshots);

        // The full, finished render (what a one-shot commit would produce).
        let done = AssistantBlock {
            id: MessageId::new("m1"),
            text: final_text.to_string(),
            done: true,
            rendered: Some(crate::markdown::MdDoc::parse(final_text)),
        };
        let full: Vec<String> = assistant_split(&done, &theme, 40)
            .0
            .iter()
            .map(line_text)
            .collect();

        // Progressive commit must equal the whole message exactly: no duplicated
        // lines, no gaps, correct order.
        assert_eq!(committed, full);
    }

    #[test]
    fn progressive_commit_never_freezes_a_partial_markdown_table() {
        let theme = Theme::no_color();
        let final_text = "| 阶段 | 工时 |\n|------|------|\n| 基础设施（model + store + DynamicReader） | 1 天 |\n| Admin 后端 API（repo/service/handler/路由） | 0.5 天 |\n| Admin 前端页面（API 封装 + 表格 + 编辑弹窗） | 0.5 天 |\n| 消费者改造 + 联调 + 测试 | 0.5 天 |";
        let mut snapshots: Vec<&str> = final_text
            .char_indices()
            .skip(1)
            .map(|(index, _)| &final_text[..index])
            .collect();
        snapshots.push(final_text);

        let committed = simulate_stream(&snapshots);
        let done = AssistantBlock {
            id: MessageId::new("m1"),
            text: final_text.to_string(),
            done: true,
            rendered: Some(crate::markdown::MdDoc::parse(final_text)),
        };
        let full: Vec<String> = assistant_split(&done, &theme, 40)
            .0
            .iter()
            .map(line_text)
            .collect();

        assert_eq!(committed, full);
    }

    #[test]
    fn progressive_commit_does_not_freeze_raw_strong_markers() {
        let theme = Theme::no_color();
        let final_text = "## 一句话总结\n\n> **CodeLeveler 是一个用 Rust 写的跨平台编程 Agent，能够理解代码并执行任务。**";
        let snapshots = [
            "## 一句话总结",
            "## 一句话总结\n\n> **CodeLeveler 是一个用 Rust 写的跨平台编程 Agent",
            final_text,
        ];
        let committed = simulate_stream(&snapshots);
        let done = AssistantBlock {
            id: MessageId::new("m1"),
            text: final_text.to_string(),
            done: true,
            rendered: Some(crate::markdown::MdDoc::parse(final_text)),
        };
        let full: Vec<String> = assistant_split(&done, &theme, 40)
            .0
            .iter()
            .map(line_text)
            .collect();

        assert_eq!(committed, full);
        assert!(
            committed.iter().all(|line| !line.contains("**")),
            "raw strong markers leaked into scrollback: {committed:?}"
        );
    }

    #[test]
    fn run_command_shows_the_command() {
        let s = tool_summary(
            "run_command",
            r#"{"program":"cargo","args":["check","-p","atomcode-core"]}"#,
        );
        assert_eq!(s, "cargo check -p atomcode-core");
    }

    #[test]
    fn run_command_hides_duplicate_program_arg() {
        let s = tool_summary(
            "run_command",
            r#"{"program":"pytest","args":["pytest","tests/providers/test_retry_classification.py","-q"]}"#,
        );
        assert_eq!(s, "pytest tests/providers/test_retry_classification.py -q");
    }

    #[test]
    fn read_file_shows_path_and_range() {
        let s = tool_summary(
            "read_file",
            r#"{"path":"src/lib.rs","start_line":1,"end_line":100}"#,
        );
        assert_eq!(s, "src/lib.rs:1-100");
    }

    #[test]
    fn grep_shows_pattern_and_path() {
        let s = tool_summary("grep", r#"{"pattern":"TODO","path":"crates"}"#);
        assert_eq!(s, "\"TODO\" in crates");
    }

    #[test]
    fn apply_patch_shows_touched_files() {
        let s = tool_summary(
            "apply_patch",
            r#"{"patch":"*** Begin Patch\n*** Update File: src/a.rs\n*** End Patch"}"#,
        );
        assert_eq!(s, "src/a.rs");
        // A new file (the screenshot case): show the added path, not raw JSON.
        let s = tool_summary(
            "apply_patch",
            &serde_json::json!({
                "patch": "*** Begin Patch\n*** Add File: crates/api/src/sse/chat.rs\n+//! SSE parsing\n*** End Patch"
            })
            .to_string(),
        );
        assert_eq!(s, "crates/api/src/sse/chat.rs");

        // Some providers stream tool arguments with raw newlines inside a JSON
        // string. That is not valid JSON, but the one-line tool heading should
        // still show the touched file instead of a giant {"patch":... blob.
        let s = tool_summary(
            "apply_patch",
            "{\"patch\":\"*** Begin Patch\n*** Update File: src/cli.ts\n*** End Patch\"}",
        );
        assert_eq!(s, "src/cli.ts");
    }

    #[test]
    fn update_plan_shows_explanation_not_raw_json() {
        let s = tool_summary(
            "update_plan",
            r#"{"explanation":"WireApi 枚举已更新，现在创建 Chat SSE 解析器","plan":[{"step":"x","status":"pending"}]}"#,
        );
        assert!(
            s.contains("WireApi") && !s.contains('{'),
            "explanation, not raw JSON: {s}"
        );
        assert_eq!(tool_action_label("update_plan"), "更新计划");
    }

    #[test]
    fn update_goal_shows_human_resolution_not_raw_json() {
        let s = tool_summary(
            "update_goal",
            r#"{"status":"complete","summary":"完成了对示例 CLI 项目的安装验证"}"#,
        );
        assert_eq!(s, "完成：完成了对示例 CLI 项目的安装验证");
        assert_eq!(tool_action_label("update_goal"), "目标收尾");
        assert_eq!(tool_action_label("request_user_input"), "询问");
        assert_eq!(tool_action_label("ask_user"), "询问");
    }

    #[test]
    fn unknown_or_non_json_falls_back() {
        assert_eq!(tool_summary("run_command", "cargo test"), "cargo test");
    }

    #[test]
    fn shell_command_summary_is_human_readable() {
        let s = tool_summary(
            "shell_command",
            r#"{"cmd":"cd /tmp && cargo test --workspace"}"#,
        );
        assert_eq!(s, "cargo test --workspace");
        assert!(!s.contains("cmd"), "{s}");
        assert!(!s.contains('{'), "{s}");
    }

    fn test_state() -> crate::state::AppState {
        crate::state::AppState::new(
            Theme::no_color(),
            crate::state::Boot {
                session_id: leveler_client_protocol::SessionId::new("s1"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
            },
        )
    }

    fn render_text(state: &mut AppState, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
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
    fn help_first_page_shows_goal_command() {
        let mut state = AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
            },
        );
        state.active_screen = Screen::Help;

        let text = render_text(&mut state, 80, 24);
        assert!(
            text.contains("/goal"),
            "help first page should advertise /goal:\n{text}"
        );
    }

    fn line_str(line: &super::Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn busy_status_shows_real_activity_when_known() {
        let mut s = test_state();
        s.status = leveler_client_protocol::RuntimeStatus::Busy;
        s.activity = Some("运行 cargo test".into());
        let text = line_str(&crate::status_line::status_line_content(&s, 80));
        assert!(
            text.contains("运行 cargo test"),
            "the real activity must be shown, not a whimsy word: {text:?}"
        );
    }

    #[test]
    fn list_scroll_offset_keeps_focus_visible() {
        use super::panes::list_scroll_offset;
        // Focus within the first page: no scroll.
        assert_eq!(list_scroll_offset(20, 10, 0), 0);
        assert_eq!(list_scroll_offset(20, 10, 9), 0);
        // Focus past the fold: scroll just enough to keep it on the last row.
        assert_eq!(list_scroll_offset(20, 10, 10), 1);
        assert_eq!(list_scroll_offset(20, 10, 15), 6);
        // Never scroll past the end (max = 20 - 10 = 10).
        assert_eq!(list_scroll_offset(20, 10, 19), 10);
        // Content shorter than the pane: never scroll.
        assert_eq!(list_scroll_offset(5, 10, 4), 0);
    }

    #[test]
    fn sub_agent_block_without_role_has_no_empty_brackets() {
        use crate::transcript::SubAgentBlock;
        let item = TranscriptItem::SubAgent(SubAgentBlock {
            id: "a1".into(),
            nickname: "Newton".into(),
            role: String::new(),
            status: ToolStatus::Ok,
            detail: "done".into(),
            progress: Default::default(),
            recent_step: None,
        });
        let text: String = item_render(
            &item,
            &Theme::no_color(),
            60,
            false,
            crate::i18n::Locale::Zh.text(),
        )
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
        assert!(
            !text.contains("[]"),
            "empty role must not render as []: {text:?}"
        );
        assert!(text.contains("Newton"));
    }

    #[test]
    fn explorer_failure_is_named_and_explained_for_the_user() {
        use crate::transcript::SubAgentBlock;
        let item = TranscriptItem::SubAgent(SubAgentBlock {
            id: "agent-1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            status: ToolStatus::Failed,
            detail:
                "Reached the 6-round limit before finishing.\n\nLatest note: inspecting providers"
                    .into(),
            progress: Default::default(),
            recent_step: None,
        });
        let text: String = item_render(
            &item,
            &Theme::no_color(),
            80,
            false,
            crate::i18n::Locale::Zh.text(),
        )
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

        assert!(text.contains("探索 Agent 1 · 未完成"), "{text}");
        assert!(text.contains("未在 6 轮内完成"), "{text}");
        assert!(text.contains("最后进展：inspecting providers"), "{text}");
        assert!(!text.contains("Euclid"), "{text}");
        assert!(!text.contains("[explorer]"), "{text}");
        assert!(!text.contains("Reached the"), "{text}");
    }

    #[test]
    fn running_explorer_has_a_clear_execution_label() {
        use crate::transcript::SubAgentBlock;
        let item = TranscriptItem::SubAgent(SubAgentBlock {
            id: "agent-1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            status: ToolStatus::Running,
            detail: "Explore model provider architecture".into(),
            progress: crate::transcript::SubAgentProgress {
                active: true,
                ..Default::default()
            },
            recent_step: None,
        });
        let text = item_render(
            &item,
            &Theme::no_color(),
            80,
            false,
            crate::i18n::Locale::Zh.text(),
        )
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

        assert!(text.contains("探索 Agent 1 · 执行中"), "{text}");
        assert!(
            text.contains("Explore model provider architecture"),
            "{text}"
        );
        assert!(!text.contains("Euclid"), "{text}");
    }

    #[test]
    fn multiple_sub_agents_show_distinct_work_state_purpose_and_usage() {
        use crate::action::Action;
        use crate::reducer::reduce;
        use leveler_client_protocol::RuntimeEvent;

        let mut state = test_state();
        for (id, nickname, task) in [
            ("agent-1", "Euclid", "检查 provider 架构"),
            ("agent-2", "Newton", "检查协议适配层"),
        ] {
            reduce(
                &mut state,
                Action::Runtime(RuntimeEvent::SubAgentUpdated {
                    id: id.into(),
                    nickname: nickname.into(),
                    role: "explorer".into(),
                    done: false,
                    ok: false,
                    detail: task.into(),
                }),
            );
        }

        // Consecutive sub-agents aggregate into one tree: a parent header plus
        // one ├─/└─ child per agent (nickname first).
        let waiting = render_text(&mut state, 100, 28);
        assert!(waiting.contains("2 个 agents 正在运行"), "{waiting}");
        assert!(waiting.contains("├─ Euclid"), "{waiting}");
        assert!(waiting.contains("└─ Newton"), "{waiting}");
        assert!(waiting.contains("等待执行"), "{waiting}");

        for (id, input, output, cached) in
            [("agent-1", 1_200, 80, 600), ("agent-2", 2_400, 160, 1_200)]
        {
            reduce(
                &mut state,
                Action::Runtime(RuntimeEvent::SubAgentProgress {
                    id: id.into(),
                    active: true,
                    input_tokens: input,
                    output_tokens: output,
                    cached_input_tokens: cached,
                }),
            );
        }

        let active = render_text(&mut state, 100, 28);
        assert!(active.contains("进行中"), "{active}");
        assert!(
            active.contains("↑ 3.6k · ↓ 240"),
            "parent aggregates reported usage: {active}"
        );
    }

    #[test]
    fn english_locale_covers_agents_recap_and_unsupported_delegation() {
        use crate::action::Action;
        use crate::reducer::reduce;
        use leveler_client_protocol::RuntimeEvent;

        let mut state = test_state();
        state.locale = crate::i18n::Locale::En;
        state.active_screen = Screen::Agents;
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::SubAgentUpdated {
                id: "agent-1".into(),
                nickname: "Euclid".into(),
                role: "explorer".into(),
                done: false,
                ok: false,
                detail: "Inspect provider architecture".into(),
            }),
        );
        let agents = render_text(&mut state, 100, 28);
        assert!(agents.contains("Sub-agents"), "{agents}");
        assert!(agents.contains("Explorer agent 1 · waiting"), "{agents}");
        assert!(
            agents.contains("Task: Inspect provider architecture"),
            "{agents}"
        );
        assert!(agents.contains("Esc back"), "{agents}");
        assert!(
            !agents.contains("子 Agent") && !agents.contains("返回"),
            "{agents}"
        );

        let recap = TranscriptItem::Recap(RecapBlock {
            summary: Some("Implemented".into()),
            next_step: "Run the app".into(),
        });
        let recap = item_render(
            &recap,
            &Theme::no_color(),
            80,
            false,
            crate::i18n::Locale::En.text(),
        )
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert!(recap.contains("recap:"), "{recap}");

        let task = TranscriptItem::ToolGroup(ToolGroupBlock {
            calls: vec![ToolCallBlock {
                id: ToolCallId::new("task-1"),
                name: "task".into(),
                arguments: r#"{"description":"Inspect provider architecture"}"#.into(),
                status: ToolStatus::Failed,
                preview: Some("tool error: unknown tool `task`; use `spawn_agent`".into()),
                duration_ms: None,
                parallel: false,
            }],
            open: false,
            expanded: false,
        });
        let task = item_render(
            &task,
            &Theme::no_color(),
            100,
            false,
            crate::i18n::Locale::En.text(),
        )
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert!(task.contains("Delegation (unsupported)"), "{task}");
        assert!(
            task.contains("task is unsupported; use spawn_agent"),
            "{task}"
        );
        assert!(
            !task
                .chars()
                .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch)),
            "{task}"
        );
    }

    #[test]
    fn truncate_display_neutralizes_control_chars() {
        // A raw \r/\t/\n in tool output must not survive into a one-line summary.
        let out = truncate_display("a\r\tb\nc", 20);
        assert!(!out.contains('\r') && !out.contains('\t') && !out.contains('\n'));
        assert_eq!(out, "a  b c");
    }

    #[test]
    fn error_status_does_not_reference_missing_command() {
        let mut s = test_state();
        s.status = leveler_client_protocol::RuntimeStatus::Error;
        let text = line_str(&crate::status_line::status_line_content(&s, 80));
        assert!(
            !text.contains("/status"),
            "/status is not implemented; the hint must not point at it: {text:?}"
        );
    }

    #[test]
    fn bottom_bar_has_no_fabricated_info() {
        let mut s = test_state();
        s.model_label = "deepseek/v3".into();
        s.mode_label = "Assisted".into();
        let text: String = crate::status_line::bottom_bar_lines(&s, 120)
            .iter()
            .map(line_str)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("1 shell"), "hardcoded shell count: {text:?}");
        assert!(
            !text.contains("shift+enter"),
            "shift+enter is not handled and must not be advertised: {text:?}"
        );
        // Trust strip: model · auto only — keys live in /help · Ctrl+?.
        assert!(text.contains("deepseek/v3"), "{text:?}");
        assert!(text.contains("auto"), "{text:?}");
        assert!(
            !text.contains("ctrl+j")
                && !text.contains("Ctrl+O")
                && !text.contains('↑')
                && !text.contains("替我审批"),
            "no key/token spam on trust strip: {text:?}"
        );
        assert_eq!(
            crate::status_line::bottom_bar_lines(&s, 120).len(),
            1,
            "single trust strip"
        );
    }

    #[test]
    fn footer_renders_open_overlay_inline_instead_of_composer() {
        let mut s = test_state();
        s.overlay = Some(crate::overlay::Overlay::Approval(Box::new(
            crate::overlay::ApprovalOverlay::new(leveler_client_protocol::UiApprovalRequest {
                id: leveler_client_protocol::ApprovalId::new("r1"),
                tool: "run_command".into(),
                summary: "git push".into(),
                command: Some("git push".into()),
                risks: vec!["将访问网络".into()],
            }),
        )));
        let (lines, _) = super::conversation_footer(&s, 80, 0, 0, false);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("需要权限"), "{joined}");
        assert!(joined.contains("拒绝"), "{joined}");
        assert!(joined.contains("git push"), "{joined}");
        // The overlay replaces the composer: exactly one box in the footer.
        assert_eq!(
            joined.matches('╭').count(),
            1,
            "composer must be hidden while an overlay is open: {joined}"
        );
    }

    #[test]
    fn footer_clarification_overlay_places_cursor_on_its_input() {
        let mut s = test_state();
        s.overlay = Some(crate::overlay::Overlay::Clarification(Box::new(
            crate::overlay::ClarificationOverlay::new(
                leveler_client_protocol::UiClarificationRequest {
                    id: leveler_client_protocol::ClarificationId::new("c1"),
                    question: "选哪个？".into(),
                    options: vec!["A".into()],
                },
            ),
        )));
        let (lines, cursor) = super::conversation_footer(&s, 80, 0, 0, false);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("选哪个？"), "{joined}");
        assert!(
            cursor.is_some(),
            "clarification input needs a visible cursor"
        );
        assert!(
            joined.contains("等待你的回复"),
            "clarification must make the waiting state explicit: {joined}"
        );
    }

    #[test]
    fn completed_turn_renders_a_summary_divider_with_space_before_composer() {
        let mut s = test_state();
        s.status = leveler_client_protocol::RuntimeStatus::Busy;
        s.elapsed_secs = 261;
        for (id, path) in [("t1", "src/lib.rs"), ("t2", "src/main.rs")] {
            let id = ToolCallId::new(id);
            crate::reducer::reduce(
                &mut s,
                crate::action::Action::Runtime(
                    leveler_client_protocol::RuntimeEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({ "path": path }).to_string(),
                        parallel: false,
                    },
                ),
            );
            crate::reducer::reduce(
                &mut s,
                crate::action::Action::Runtime(
                    leveler_client_protocol::RuntimeEvent::ToolCallCompleted {
                        id,
                        ok: true,
                        preview: "ok".into(),
                        duration_ms: 20,
                    },
                ),
            );
        }
        crate::reducer::reduce(
            &mut s,
            crate::action::Action::Runtime(leveler_client_protocol::RuntimeEvent::TurnCompleted),
        );

        let (lines, _) = super::conversation_footer(&s, 80, 0, 0, false);
        let rendered = lines.iter().map(line_str).collect::<Vec<_>>();
        let marker = rendered
            .iter()
            .position(|line| line.contains("已完成"))
            .unwrap_or_else(|| panic!("completion marker missing: {}", rendered.join("\n")));
        assert!(
            rendered[marker].contains("2 次工具"),
            "{:?}",
            rendered[marker]
        );
        assert!(
            rendered[marker].contains("4m 21s"),
            "{:?}",
            rendered[marker]
        );
        assert_eq!(rendered.get(marker + 1).map(String::as_str), Some(""));
        // Composer box (╭) or prompt line (›) follows the blank; metrics/bottom
        // chrome may sit below the box.
        assert!(
            rendered.get(marker + 2).is_some_and(|line| {
                line.starts_with('╭') || line.starts_with('›') || line.contains('›')
            }),
            "blank then composer after turn marker: {}",
            rendered.join("\n")
        );

        s.transcript.push_user("继续".into());
        assert!(matches!(
            &s.transcript.items()[s.transcript.items().len() - 2],
            TranscriptItem::TurnEnd(end)
                if end.status == crate::transcript::TurnEndStatus::Completed
        ));
    }

    #[test]
    fn failed_turn_renders_a_stopped_divider() {
        let mut s = test_state();
        s.status = leveler_client_protocol::RuntimeStatus::Busy;
        crate::reducer::reduce(
            &mut s,
            crate::action::Action::Runtime(leveler_client_protocol::RuntimeEvent::TurnFailed {
                error: "boom".into(),
            }),
        );
        let (lines, _) = super::conversation_footer(&s, 80, 0, 0, false);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("✗ 失败"), "{joined}");
    }

    #[test]
    fn consecutive_tool_calls_collapse_into_one_group_summary() {
        let mut s = test_state();
        let first = ToolCallId::new("t1");
        s.transcript.push_tool_started(
            first.clone(),
            "read_file".into(),
            r#"{"path":"README.md"}"#.into(),
            false,
        );
        s.transcript.complete_tool(
            &first,
            true,
            "README contents\nrest of the readme body".into(),
            10,
        );
        let second = ToolCallId::new("t2");
        s.transcript.push_tool_started(
            second.clone(),
            "grep".into(),
            r#"{"pattern":"TODO","path":"crates"}"#.into(),
            false,
        );
        s.transcript
            .complete_tool(&second, false, "grep failed loudly".into(), 20);

        // Product activity stream: the successful read renders its own compact
        // unit; the failed grep keeps an honest result line with its error.
        let (auto, _) = super::conversation_footer(&s, 100, 0, 0, false);
        let auto = auto.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(
            auto.contains("读取") || auto.contains("检查") || auto.contains("找到"),
            "successful read renders its own unit: {auto}"
        );
        assert!(
            auto.contains("搜索") && auto.contains("grep failed loudly"),
            "collapsed failed call must expose its error: {auto}"
        );
        if let Some(TranscriptItem::ToolGroup(group)) = s.transcript.items().last() {
            assert!(!group.expanded, "failed groups must not auto-expand");
        }
        assert!(
            auto.contains("README contents"),
            "collapsed unit shows the bounded first content line: {auto}"
        );
        assert!(
            !auto.contains("rest of the readme body"),
            "collapsed group must not leak output beyond the first line: {auto}"
        );

        // Expand the current (only) group via its own flag — not a global blast.
        if let Some(TranscriptItem::ToolGroup(group)) = s.transcript.items_mut().last_mut() {
            group.expanded = true;
        }
        let (expanded, _) = super::conversation_footer(&s, 100, 0, 0, false);
        let expanded = expanded.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(
            expanded.contains("README") || expanded.contains("读取") || expanded.contains("检查"),
            "{expanded}"
        );
        assert!(
            expanded.contains("TODO") || expanded.contains("搜索"),
            "{expanded}"
        );
        assert!(expanded.contains("grep failed loudly"), "{expanded}");
    }

    #[test]
    fn active_structured_plan_is_visible_in_the_conversation_footer() {
        let mut s = test_state();
        s.status = leveler_client_protocol::RuntimeStatus::Busy;
        s.plan = Some(leveler_client_protocol::UiPlan {
            steps: vec![
                leveler_client_protocol::UiPlanStep {
                    index: 0,
                    description: "读取约束与现状".into(),
                    status: leveler_client_protocol::PlanStepStatus::Done,
                },
                leveler_client_protocol::UiPlanStep {
                    index: 1,
                    description: "修复运行链".into(),
                    status: leveler_client_protocol::PlanStepStatus::Running,
                },
            ],
        });

        let (lines, _) = super::conversation_footer(&s, 100, 0, 0, false);
        let text = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(text.contains("计划"), "{text}");
        assert!(text.contains("读取约束与现状"), "{text}");
        assert!(text.contains("修复运行链"), "{text}");
    }

    #[test]
    fn structured_plan_does_not_truncate_after_six_steps() {
        let mut s = test_state();
        s.status = leveler_client_protocol::RuntimeStatus::Busy;
        s.plan = Some(leveler_client_protocol::UiPlan {
            steps: (0..8)
                .map(|index| leveler_client_protocol::UiPlanStep {
                    index,
                    description: format!("计划步骤{}", index + 1),
                    status: match index {
                        0..=5 => leveler_client_protocol::PlanStepStatus::Done,
                        6 => leveler_client_protocol::PlanStepStatus::Running,
                        _ => leveler_client_protocol::PlanStepStatus::Pending,
                    },
                })
                .collect(),
        });

        let (lines, _) = super::conversation_footer(&s, 100, 0, 0, false);
        let text = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        for index in 1..=8 {
            assert!(text.contains(&format!("计划步骤{index}")), "{text}");
        }
    }

    #[test]
    fn loaded_project_rules_are_visible_during_the_turn() {
        let mut s = test_state();
        s.status = leveler_client_protocol::RuntimeStatus::Busy;
        let event: leveler_client_protocol::RuntimeEvent =
            serde_json::from_value(serde_json::json!({
                "type": "project_rules_loaded",
                "sources": ["AGENTS.md", "src/AGENTS.md"]
            }))
            .expect("the runtime protocol must carry loaded rule provenance");
        crate::reducer::reduce(&mut s, crate::action::Action::Runtime(event));

        let (lines, _) = super::conversation_footer(&s, 100, 0, 0, false);
        let text = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(text.contains("AGENTS.md"), "{text}");
        assert!(text.contains("src/AGENTS.md"), "{text}");
    }

    #[test]
    fn footer_lists_queued_messages_under_the_composer() {
        let mut s = test_state();
        s.queue_collapsed = false;
        s.input_queues.rejected = vec!["重试".into()];
        s.input_queues.pending = vec!["发送".into()];
        s.input_queues.queued = vec!["第一条".into(), "第二条".into()];
        let (lines, _) = super::conversation_footer(&s, 80, 0, 0, false);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        // Queue panel lists waiting tasks (workbench design).
        assert!(joined.contains("Queue (4)"), "{joined}");
        assert!(joined.contains("重试"), "rejected item visible: {joined}");
        assert!(joined.contains("第一条"), "queued item visible: {joined}");
        assert!(joined.contains('○') || joined.contains('⟳'), "{joined}");
    }

    #[test]
    fn composer_window_follows_cursor_above_the_fold() {
        let mut s = test_state();
        let text = (0..12)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        s.composer.replace(text);
        for _ in 0..12 {
            s.composer.up();
        }
        assert_eq!(s.composer.cursor_row_col_display().0, 0);
        let (lines, (_, cy)) = super::composer_box_lines(&s, 40);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("line0"),
            "the visible window must follow the cursor row: {joined}"
        );
        // With borders, content starts at row 1 (row 0 is ╭──╮).
        assert_eq!(cy, 1, "cursor on first content row under top border");
        // Content + top/bottom borders.
        assert!(
            lines.len() <= super::COMPOSER_MAX_ROWS + 2,
            "composer overflowed max rows: {}",
            lines.len()
        );
        assert!(
            joined.contains('╭') && joined.contains('│') && joined.contains('╰'),
            "bordered input expected: {joined}"
        );
    }

    #[test]
    fn long_single_line_soft_wraps_inside_box_width() {
        let mut s = test_state();
        // Wider than a 40-col box (inner after borders, minus "› ").
        s.composer.replace("1".repeat(80));
        let width = 40;
        let (lines, (cx, _cy)) = super::composer_box_lines(&s, width);
        for line in &lines {
            let w = line.width();
            assert!(
                w <= width,
                "composer row wider than box: width={w} max={width} line={line:?}"
            );
        }
        // Soft wrap + borders → more than 3 lines (top + ≥2 content + bottom).
        assert!(
            lines.len() >= 4,
            "expected soft-wrapped multi-row composer, got {} lines",
            lines.len()
        );
        assert!(
            (cx as usize) < width,
            "cursor col {cx} past box width {width}"
        );
    }

    #[test]
    fn empty_composer_shows_a_visual_hint_without_mutating_the_buffer() {
        let s = test_state();
        let (lines, _) = super::composer_box_lines(&s, 48);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");

        assert!(
            joined.contains("输入消息") || joined.contains("Type a message"),
            "empty composer hint missing: {joined}"
        );
        assert!(
            s.composer.is_empty(),
            "hint must not enter the input buffer"
        );
    }

    #[test]
    fn composer_hint_hidden_once_conversation_has_turns() {
        let mut s = test_state();
        s.transcript.push_user("你好".into());
        let (lines, _) = super::composer_box_lines(&s, 48);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(
            !joined.contains("输入消息") && !joined.contains("Type a message"),
            "hint should not repeat after real turns: {joined}"
        );
    }

    #[test]
    fn composer_shows_slash_arg_ghost_without_mutating_buffer() {
        let mut s = test_state();
        s.composer.replace("/btw ");
        let (lines, _) = super::composer_box_lines(&s, 48);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("<问题>") || joined.contains("<question>"),
            "ghost placeholder missing: {joined}"
        );
        assert_eq!(
            s.composer.text(),
            "/btw ",
            "ghost must not be written into the buffer"
        );

        s.composer.replace("/btw 你好");
        let (lines, _) = super::composer_box_lines(&s, 48);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(
            !joined.contains("<问题>") && !joined.contains("<question>"),
            "ghost must clear once an argument is typed: {joined}"
        );
    }

    fn tool_item(name: &str, args: &str, ms: u64) -> ToolCallBlock {
        ToolCallBlock {
            id: ToolCallId::new("t"),
            name: name.into(),
            arguments: args.into(),
            status: ToolStatus::Ok,
            preview: Some("some output".into()),
            duration_ms: Some(ms),
            parallel: false,
        }
    }

    fn tool_render(call: &ToolCallBlock, expanded: bool) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        crate::tool_cell::tool_lines(
            call,
            &Theme::no_color(),
            100,
            expanded,
            crate::i18n::Locale::Zh.text(),
            &mut lines,
        );
        lines
    }

    #[test]
    fn successful_tool_call_renders_as_one_compact_line() {
        let item = tool_item("read_file", r#"{"path":"src/lib.rs"}"#, 2);
        let lines = tool_render(&item, false);
        assert_eq!(lines.len(), 1, "one line per quiet success: {lines:?}");
        let head = line_str(&lines[0]);
        assert!(
            head.contains("读取") && head.contains("src/lib.rs"),
            "verb + target on the same line: {head}"
        );
        assert!(!head.contains("ms"), "sub-second timing is noise: {head}");
    }

    #[test]
    fn slow_tool_call_shows_duration_in_seconds() {
        let item = tool_item(
            "run_command",
            r#"{"program":"cargo","args":["test"]}"#,
            13000,
        );
        let lines = tool_render(&item, false);
        let head = line_str(&lines[0]);
        assert!(head.contains("13.0s"), "slow call shows seconds: {head}");
    }

    #[test]
    fn consecutive_tool_calls_share_one_open_transcript_group() {
        let mut transcript = crate::transcript::TranscriptState::new();
        for id in ["t1", "t2"] {
            let id = ToolCallId::new(id);
            transcript.push_tool_started(id.clone(), "read_file".into(), "{}".into(), false);
            transcript.complete_tool(&id, true, "ok".into(), 1);
        }
        assert_eq!(transcript.items().len(), 1);
        let TranscriptItem::ToolGroup(group) = &transcript.items()[0] else {
            panic!("tool burst must be stored as one group")
        };
        assert_eq!(group.calls.len(), 2);
        assert!(
            group.open,
            "group stays live until the next transcript block"
        );
        assert!(!super::item_is_final(&transcript.items()[0]));

        transcript.begin_assistant(MessageId::new("m2"));
        let TranscriptItem::ToolGroup(group) = &transcript.items()[0] else {
            unreachable!()
        };
        assert!(!group.open);
        assert!(super::item_is_final(&transcript.items()[0]));
    }

    #[test]
    fn apply_patch_shows_inline_diff_lines() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n context\n-let old = 1;\n+let new = 2;\n*** End Patch";
        let args = serde_json::json!({ "patch": patch }).to_string();
        let item = tool_item("apply_patch", &args, 5);
        let lines = tool_render(&item, false);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("let old = 1;")
                && joined.contains("let new = 2;")
                && joined.contains('-')
                && joined.contains('+'),
            "the edit's diff must be visible inline: {joined}"
        );
        assert!(
            !joined.contains("Begin Patch") && !joined.contains("End Patch"),
            "patch envelope markers are noise: {joined}"
        );
    }

    #[test]
    fn apply_patch_with_raw_newline_json_still_shows_inline_diff() {
        let args = "{\"patch\":\"*** Begin Patch\n*** Update File: src/cli.ts\n@@\n-old\n+new\n*** End Patch\"}";
        let item = tool_item("apply_patch", args, 5);
        let lines = tool_render(&item, false);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");

        assert!(
            joined.contains("src/cli.ts"),
            "file heading missing: {joined}"
        );
        assert!(
            (joined.contains("- old") || joined.contains("-old") || joined.lines().any(|l| l.contains('-') && l.contains("old")))
                && (joined.contains("+ new")
                    || joined.contains("+new")
                    || joined.lines().any(|l| l.contains('+') && l.contains("new"))),
            "raw-newline patch diff missing: {joined}"
        );
        assert!(
            !joined.contains("{\"patch\""),
            "raw JSON wrapper should not leak into patch display: {joined}"
        );
    }

    #[test]
    fn long_inline_diff_is_capped_until_expanded() {
        let body: String = (0..40).map(|i| format!("+line {i}\n")).collect();
        let patch = format!("*** Begin Patch\n*** Update File: src/a.rs\n@@\n{body}*** End Patch");
        let args = serde_json::json!({ "patch": patch }).to_string();
        let item = tool_item("apply_patch", &args, 5);

        let folded = tool_render(&item, false);
        let text = folded.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(
            !text.contains("line 30"),
            "folded diff is capped: {text}"
        );
        assert!(
            text.contains("Ctrl+O") && text.contains("…"),
            "must hint how to see the full diff: {text}"
        );

        let expanded = tool_render(&item, true);
        let text = expanded.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(
            text.contains("line 30"),
            "Ctrl+O expands the diff: {text}"
        );
    }

    #[test]
    fn failed_tool_output_is_available_in_expanded_details() {
        let item = ToolCallBlock {
            id: ToolCallId::new("t"),
            name: "apply_patch".into(),
            arguments: "{}".into(),
            status: ToolStatus::Failed,
            preview: Some(
                "failed to apply hunk: could not find expected lines:\n    let a = 1;\n    let b = 2;"
                    .into(),
            ),
            duration_ms: Some(1),
            parallel: false,
        };
        let lines = tool_render(&item, true);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("let a = 1;"),
            "the error detail must be visible after expanding the group: {joined}"
        );
    }

    #[test]
    fn read_file_success_hides_noisy_preview_until_expanded() {
        let item = ToolCallBlock {
            id: ToolCallId::new("t1"),
            name: "read_file".into(),
            arguments: r#"{"path":"README.md"}"#.into(),
            status: ToolStatus::Ok,
            preview: Some("     1\t# README\n     2\tlots of content".into()),
            duration_ms: Some(1),
            parallel: false,
        };
        let folded = tool_render(&item, false);
        let text = folded
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(text.contains("README.md"));
        assert!(!text.contains("lots of content"));

        let expanded = tool_render(&item, true);
        let text = expanded
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(text.contains("lots of content"));
    }

    #[test]
    fn update_goal_success_hides_internal_preview() {
        let item = ToolCallBlock {
            id: ToolCallId::new("g1"),
            name: "update_goal".into(),
            arguments: r#"{"status":"complete","summary":"完成了对示例 CLI 项目的安装验证"}"#
                .into(),
            status: ToolStatus::Ok,
            preview: Some("Goal resolved.".into()),
            duration_ms: Some(1),
            parallel: false,
        };
        let lines = tool_render(&item, false);
        let text = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(text.contains("目标收尾"));
        assert!(text.contains("完成：完成了对示例 CLI 项目的安装验证"));
        assert!(!text.contains("update_goal"));
        assert!(!text.contains("Goal resolved"));
        assert!(!text.contains('{'));
        // Collapsed: only the compact head row (noisy success).
        assert_eq!(
            lines.len(),
            1,
            "collapsed update_goal is one line: {lines:?}"
        );
    }

    #[test]
    fn update_goal_success_expands_full_summary_not_internal_preview() {
        let long = "已通过阅读 README.md, Cargo.toml, AGENTS.md 和目录结构, 确认这是一个 Rust 多 crate workspace 编程 Agent CLI，默认 Goal 模式需要 update_goal 显式结案。";
        let item = ToolCallBlock {
            id: ToolCallId::new("g2"),
            name: "update_goal".into(),
            arguments: serde_json::json!({
                "status": "complete",
                "summary": long,
            })
            .to_string(),
            status: ToolStatus::Ok,
            preview: Some("Goal resolved.".into()),
            duration_ms: Some(1),
            parallel: false,
        };
        // Collapsed head is width-clipped (may end with …).
        let collapsed = tool_render(&item, false);
        let collapsed_text = collapsed
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(collapsed_text.contains("目标收尾"), "{collapsed_text}");
        assert!(
            !collapsed_text.contains("Goal resolved"),
            "{collapsed_text}"
        );

        // Ctrl+O expanded: full model summary, never the internal ok string.
        let expanded = tool_render(&item, true);
        let text = expanded
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        // Body is wrap()'d, so a mid-phrase line break can insert indent between
        // words — check key fragments rather than one contiguous substring.
        assert!(
            text.contains("AGENTS.md")
                && text.contains("workspace")
                && text.contains("update_goal")
                && text.contains("显式结案"),
            "expanded body must show full summary: {text}"
        );
        assert!(
            !text.contains("Goal resolved"),
            "must not leak internal preview: {text}"
        );
        assert!(
            expanded.len() > 1,
            "expanded update_goal should add body lines: {expanded:?}"
        );
    }

    #[test]
    fn btw_answer_renders_markdown_bold_not_raw_asterisks() {
        let state = test_state();
        let block = crate::transcript::BtwBlock {
            question: "还没有完事吗？".into(),
            answer: "审查完成。**没有发现明显问题**。\n\n- 编译通过\n- 测试通过".into(),
            done: true,
            failed: false,
        };
        let lines = super::btw_card_lines(&block, &state.theme, 60, state.t());
        let text = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(
            text.contains("没有发现明显问题"),
            "bold text should appear: {text:?}"
        );
        assert!(
            !text.contains("**"),
            "raw markdown markers must not remain: {text:?}"
        );
        assert!(
            text.contains("编译通过") || text.contains("•"),
            "list content: {text:?}"
        );
    }
}
