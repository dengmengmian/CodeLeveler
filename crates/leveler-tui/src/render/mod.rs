//! Draws [`AppState`] to a Ratatui frame: header, transcript, status line, and
//! the composer. Layout degrades on narrow terminals .

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::plan_cell::{render_agents_screen, render_plan_screen};
use crate::screen::Screen;
use crate::state::AppState;
use crate::status_line::{header_line, status_line_content};
use crate::tool_cell::render_tools_screen;

/// Shell Details: one user shell execution — status, live runtime, source,
/// cwd, the user's exact command (never the `sh -c` wrapper), and the
/// bounded output tail. Esc backs out; `x` stops a running one.
fn render_shell_screen(frame: &mut Frame, area: ratatui::layout::Rect, state: &mut AppState) {
    use crate::transcript::UserShellStatus;
    let theme = &state.theme;
    let t = state.t();
    let dim = Style::default().fg(theme.text.muted);
    let text_style = Style::default().fg(theme.text.primary);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        t.shell_details_title.to_string(),
        Style::default()
            .fg(theme.text.primary)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    let Some(shell) = state.focused_user_shell() else {
        lines.push(Line::from(Span::styled(t.shell_no_output.to_string(), dim)));
        frame.render_widget(Paragraph::new(lines), area);
        return;
    };
    let (status_text, status_style) = match shell.status {
        UserShellStatus::Running => (
            t.shell_status_running,
            Style::default().fg(theme.accent.primary),
        ),
        UserShellStatus::Success => (
            t.shell_status_success,
            Style::default().fg(theme.status.success),
        ),
        UserShellStatus::Failed => (
            t.shell_status_failed,
            Style::default().fg(theme.status.warning),
        ),
        UserShellStatus::Cancelled => (t.shell_status_cancelled, dim),
    };
    let runtime_secs = match shell.duration_ms {
        Some(ms) => ms / 1000,
        None => (state.elapsed_secs as i64 - shell.started_elapsed_secs).max(0) as u64,
    };
    let runtime = if runtime_secs >= 60 {
        format!("{}m {:02}s", runtime_secs / 60, runtime_secs % 60)
    } else {
        format!("{runtime_secs}s")
    };
    let field = |label: &str, value: Span<'static>| {
        Line::from(vec![Span::styled(format!("{label:<10}"), dim), value])
    };
    lines.push(field(
        t.shell_status_label,
        Span::styled(status_text.to_string(), status_style),
    ));
    lines.push(field(
        t.shell_runtime_label,
        Span::styled(runtime, text_style),
    ));
    lines.push(field(
        t.shell_source_label,
        Span::styled(t.shell_source_user.to_string(), text_style),
    ));
    lines.push(field(
        t.shell_cwd_label,
        Span::styled(shell.cwd.clone(), text_style),
    ));
    lines.push(field(
        t.shell_command_label,
        Span::styled(shell.command.clone(), text_style),
    ));
    if let Some(code) = shell.exit_code {
        lines.push(field(
            t.shell_exit_label,
            Span::styled(
                code.to_string(),
                if code == 0 {
                    Style::default().fg(theme.status.success)
                } else {
                    Style::default().fg(theme.status.warning)
                },
            ),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("{}:", t.shell_output_label),
        dim,
    )));
    if shell.output_truncated {
        lines.push(Line::from(Span::styled(t.shell_truncated.to_string(), dim)));
    }
    if shell.output.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", t.shell_no_output),
            dim,
        )));
    } else {
        let width = area.width.saturating_sub(2) as usize;
        for raw in shell.output.lines() {
            let line = sanitize_terminal_line(raw);
            lines.push(Line::from(Span::styled(
                truncate_display(&format!("  {line}"), width.max(4)),
                Style::default().fg(theme.text.secondary),
            )));
        }
    }
    lines.push(Line::from(""));
    let hint = if shell.status == UserShellStatus::Running {
        t.shell_hint_running
    } else {
        t.shell_hint_done
    };
    lines.push(Line::from(Span::styled(hint.to_string(), dim)));

    // Keep the tail visible by default; PgUp/PgDn (screen_scroll) pages back.
    let height = area.height as usize;
    let max_scroll = lines.len().saturating_sub(height);
    let scroll = state.screen_scroll.min(max_scroll);
    let offset = max_scroll.saturating_sub(scroll);
    let visible: Vec<Line<'static>> = lines.into_iter().skip(offset).take(height).collect();
    frame.render_widget(Paragraph::new(visible), area);
}

#[cfg(test)]
pub(crate) use crate::tool_cell::tool_action_label;
pub(crate) use crate::tool_cell::tool_summary;

mod footer;
mod panes;
mod screens;
pub(crate) mod text;
mod transcript_lines;

pub(crate) use footer::key_hint_line;
pub(crate) use footer::user_turn_summaries;
pub use transcript_lines::{
    assistant_split, item_is_final, item_render, items_need_gap, sub_agent_tree_lines,
};
pub(crate) use transcript_lines::{
    btw_card_lines, sub_agent_detail, sub_agent_display_name, sub_agent_status, sub_agent_usage,
    user_shell_lines,
};

pub(crate) use panes::{render_list_focused, render_scrolled};
pub(crate) use screens::screen_title;
pub(crate) use text::{sanitize_terminal_line, truncate_display, wrap};

pub(crate) use footer::{
    COMPOSER_MAX_ROWS, composer_box_lines, composer_visible_rows, render_attachments,
    render_composer, render_slash_popup,
};
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
    state.theme.paint_canvas(frame, area);

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
        Screen::Remote => render_remote_screen(frame, chunks[1], state),
        Screen::Shell => render_shell_screen(frame, chunks[1], state),
        Screen::Help => render_help_screen(frame, chunks[1], state),
        Screen::Trace => crate::observability::render_trace_screen(frame, chunks[1], state),
    }
    frame.render_widget(
        Paragraph::new(status_line_content(state, area.width as usize)),
        chunks[2],
    );
    render_attachments(frame, chunks[3], state);
    {
        let input_bg = state.theme.surface.input;
        state.theme.paint_surface(frame, chunks[4], input_bg);
    }
    render_composer(frame, chunks[4], state);

    if let Some(overlay) = &state.overlay {
        crate::overlay::render_overlay(frame, area, overlay, &state.theme, state.locale);
    }
}

/// The `/remote` invite.
///
/// Title + subtitle, a centered QR, labeled address/fingerprint, then either
/// the waiting/paste copy or the pending y/n decision. Footer always reminds
/// Esc returns to the conversation without cancelling the invite.
fn render_remote_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let t = state.t();
    let width = area.width as usize;

    let Some(remote) = &state.remote else {
        let lines = vec![
            screens::screen_title(t.screen_remote, theme),
            Line::from(""),
            Line::from(Span::styled(
                t.remote_preparing.to_string(),
                Style::default().fg(theme.text.secondary),
            )),
            Line::from(""),
            Line::from(Span::styled(
                t.remote_footer.to_string(),
                Style::default().fg(theme.text.muted),
            )),
        ];
        render_scrolled(frame, area, state, lines);
        return;
    };

    let lines = remote_screen_lines(remote, width, theme, t);
    render_scrolled(frame, area, state, lines);
}

/// Build the `/remote` screen content (testable without a Frame).
fn remote_screen_lines(
    remote: &crate::state::RemoteState,
    width: usize,
    theme: &crate::theme::Theme,
    t: &crate::i18n::UiText,
) -> Vec<Line<'static>> {
    let accent = Style::default().fg(theme.accent.primary);
    let muted = Style::default().fg(theme.text.secondary);
    let dim = Style::default().fg(theme.text.muted);
    let text = Style::default().fg(theme.text.primary);
    let bold_accent = Style::default()
        .fg(theme.accent.primary)
        .add_modifier(Modifier::BOLD);
    let bold_text = Style::default()
        .fg(theme.text.primary)
        .add_modifier(Modifier::BOLD);
    let success = Style::default()
        .fg(theme.status.success)
        .add_modifier(Modifier::BOLD);
    let warning = Style::default()
        .fg(theme.status.warning)
        .add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line<'static>> = vec![
        screens::screen_title(t.screen_remote, theme),
        Line::from(""),
        Line::from(Span::styled(t.remote_scan_heading.to_string(), bold_text)),
        Line::from(Span::styled(t.remote_scan_sub.to_string(), muted)),
        Line::from(""),
    ];

    // Quiet frame around the QR so it reads as a card, not a wall of blocks.
    let qr_inner = remote
        .invite
        .qr
        .iter()
        .map(|r| unicode_width::UnicodeWidthStr::width(r.as_str()))
        .max()
        .unwrap_or(0);
    let frame_w = (qr_inner + 4).min(width.saturating_sub(2).max(1));
    let top = format!("┌{}┐", "─".repeat(frame_w.saturating_sub(2)));
    let bot = format!("└{}┘", "─".repeat(frame_w.saturating_sub(2)));
    lines.push(Line::from(Span::styled(center_display(&top, width), dim)));
    for row in &remote.invite.qr {
        let padded = format!("│ {} │", pad_to_width(row, qr_inner));
        lines.push(Line::from(Span::styled(
            center_display(&padded, width),
            text,
        )));
    }
    lines.push(Line::from(Span::styled(center_display(&bot, width), dim)));
    lines.push(Line::from(""));

    lines.push(labeled_row(
        t.remote_label_address,
        &remote.invite.relay_url,
        accent,
        crate::url_link::link_style(theme.accent.primary),
        width,
    ));
    lines.push(labeled_row(
        t.remote_label_host_fp,
        &remote.invite.host_fingerprint,
        accent,
        text,
        width,
    ));
    lines.push(Line::from(""));

    match (&remote.pending, &remote.outcome) {
        (Some(pending), _) => {
            let title = t
                .remote_wants_connect
                .replacen("{}", &pending.device_name, 1)
                .replacen("{}", &pending.platform, 1);
            lines.push(Line::from(Span::styled(title, warning)));
            lines.push(labeled_row(
                t.remote_label_phone_fp,
                &pending.fingerprint,
                accent,
                bold_text,
                width,
            ));
            lines.push(Line::from(Span::styled(
                t.remote_compare_hint.to_string(),
                muted,
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                t.remote_yn.to_string(),
                bold_accent,
            )));
        }
        (None, Some(outcome)) => {
            let style =
                if outcome.contains("拒绝") || outcome.to_ascii_lowercase().contains("reject") {
                    warning
                } else {
                    success
                };
            lines.push(Line::from(Span::styled(outcome.clone(), style)));
        }
        (None, None) => {
            lines.push(Line::from(Span::styled(
                t.remote_waiting.to_string(),
                muted,
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                t.remote_paste_hint.to_string(),
                muted,
            )));
            // Payload can be very long — wrap by display width, keep dim so it
            // does not compete with the QR.
            for chunk in wrap_display(&remote.invite.payload, width.saturating_sub(2).max(8)) {
                lines.push(Line::from(Span::styled(format!("  {chunk}"), dim)));
            }
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "─".repeat(width.clamp(8, 48)),
        dim,
    )));
    lines.push(Line::from(Span::styled(
        t.remote_footer.to_string(),
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    )));
    lines
}

fn labeled_row(
    label: &str,
    value: &str,
    label_style: Style,
    value_style: Style,
    width: usize,
) -> Line<'static> {
    // Fixed label column so address / fingerprints align.
    let col = 10usize;
    let label_w = unicode_width::UnicodeWidthStr::width(label);
    let pad = " ".repeat(col.saturating_sub(label_w));
    let avail = width.saturating_sub(col + 1).max(8);
    let val = truncate_display(value, avail);
    Line::from(vec![
        Span::styled(format!("{label}{pad}"), label_style),
        Span::styled(format!(" {val}"), value_style),
    ])
}

pub(crate) fn center_display(s: &str, width: usize) -> String {
    let w = unicode_width::UnicodeWidthStr::width(s);
    if w >= width || width == 0 {
        return s.to_string();
    }
    let pad = (width - w) / 2;
    format!("{}{s}", " ".repeat(pad))
}

pub(crate) fn pad_to_width(s: &str, width: usize) -> String {
    let w = unicode_width::UnicodeWidthStr::width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// Greedy wrap on display columns (not bytes) for long paste payloads.
fn wrap_display(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch)
            .unwrap_or(1)
            .max(1);
        if cur_w + cw > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += cw;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
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
                    &Theme::dark(),
                )
            })
            .unwrap();
        terminal
            .draw(|frame| {
                render_list_focused(frame, area, vec![Line::from("row")], 0, &Theme::dark())
            })
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
                untrusted_config: Vec::new(),
                reasoning_effort: None,
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
                untrusted_config: Vec::new(),
                reasoning_effort: None,
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
            expanded: false,
            id: "a1".into(),
            nickname: "Newton".into(),
            role: String::new(),
            status: ToolStatus::Ok,
            detail: "done".into(),
            progress: Default::default(),
            recent_step: None,
            started_elapsed_secs: 0,
            contribution: crate::multi_agent::Contribution::Pending,
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
            expanded: false,
            id: "agent-1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            status: ToolStatus::Failed,
            detail:
                "Reached the 6-round limit before finishing.\n\nLatest note: inspecting providers"
                    .into(),
            progress: Default::default(),
            recent_step: None,
            started_elapsed_secs: 0,
            contribution: crate::multi_agent::Contribution::Pending,
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
            expanded: false,
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
            started_elapsed_secs: 0,
            contribution: crate::multi_agent::Contribution::Pending,
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
                    profile_id: None,
                    profile_role: None,
                    capabilities: Vec::new(),
                    contribution: None,
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
                profile_id: None,
                profile_role: None,
                capabilities: Vec::new(),
                contribution: None,
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
                started_elapsed_secs: 0,
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
    fn workbench_renders_open_overlay_inline_instead_of_composer() {
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
        // A user turn suppresses the splash card so only overlay chrome remains.
        s.transcript.push_user("推送一下".into());
        let text = render_text(&mut s, 80, 30);
        assert!(text.contains("等待授权"), "{text}");
        assert!(text.contains("拒绝"), "{text}");
        assert!(text.contains("git push"), "{text}");
        // The overlay takes the composer's slot: no composer prompt on screen.
        assert!(
            !text.contains('›'),
            "composer must be hidden while an overlay is open: {text}"
        );
    }

    #[test]
    fn clarification_overlay_places_cursor_on_its_input() {
        let s = test_state();
        let overlay = crate::overlay::Overlay::Clarification(Box::new(
            crate::overlay::ClarificationOverlay::new(
                leveler_client_protocol::UiClarificationRequest {
                    id: leveler_client_protocol::ClarificationId::new("c1"),
                    question: "选哪个？".into(),
                    options: vec!["A".into()],
                },
            ),
        ));
        // The shared overlay content builder feeds both the workbench inline
        // box and the modal path; the cursor must land on the text input.
        let (title, lines, cursor) =
            crate::overlay::content_lines(&overlay, &s.theme, 76, s.locale);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(
            title.contains("选哪个？") || joined.contains("选哪个？"),
            "{title} / {joined}"
        );
        assert!(
            cursor.is_some(),
            "clarification input needs a visible cursor"
        );
        // The explicit waiting-state copy is asserted on the live status line in
        // status_line::tests::clarification_overlay_is_awaiting_user.
    }

    #[test]
    fn completed_turn_renders_a_summary_divider() {
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

        let rendered: Vec<String> = crate::conversation::build::build_conversation_lines(&s, 80)
            .iter()
            .map(line_str)
            .collect();
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
        let joined: String = crate::conversation::build::build_conversation_lines(&s, 80)
            .iter()
            .map(line_str)
            .collect::<Vec<_>>()
            .join("\n");
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
            0,
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
            0,
        );
        s.transcript
            .complete_tool(&second, false, "grep failed loudly".into(), 20);

        // A finished batch states that it happened and names its failures; the
        // error text itself is one Ctrl+O away, so a run full of broken calls
        // stays readable.
        let auto: String = crate::conversation::build::build_conversation_lines(&s, 100)
            .iter()
            .map(line_str)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            auto.contains("失败"),
            "a batch that broke must say so: {auto}"
        );
        if let Some(TranscriptItem::ToolGroup(group)) = s.transcript.items().last() {
            assert!(!group.expanded, "failed groups must not auto-expand");
        }
        assert!(
            !auto.contains("rest of the readme body"),
            "collapsed group must not leak tool output: {auto}"
        );

        // Opened, both calls and the error come back.
        if let Some(TranscriptItem::ToolGroup(group)) = s.transcript.items_mut().last_mut() {
            group.expanded = true;
        }
        let open: String = crate::conversation::build::build_conversation_lines(&s, 100)
            .iter()
            .map(line_str)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            open.contains("grep failed loudly"),
            "expanding must expose the error: {open}"
        );

        // Expand the current (only) group via its own flag — not a global blast.
        if let Some(TranscriptItem::ToolGroup(group)) = s.transcript.items_mut().last_mut() {
            group.expanded = true;
        }
        let expanded: String = crate::conversation::build::build_conversation_lines(&s, 100)
            .iter()
            .map(line_str)
            .collect::<Vec<_>>()
            .join("\n");
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
    fn active_structured_plan_is_visible_in_the_plan_panel() {
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

        let text = render_text(&mut s, 100, 32);
        assert!(text.contains("计划"), "{text}");
        assert!(text.contains("读取约束与现状"), "{text}");
        assert!(text.contains("修复运行链"), "{text}");
    }

    #[test]
    fn long_plan_panel_caps_at_six_rows_showing_leading_steps() {
        // The workbench plan panel is chrome, not a scroll surface: it takes at
        // most 6 rows (title + 5 steps) so a long plan never squeezes out the
        // conversation. Leading steps stay visible; the title carries progress.
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

        let text = render_text(&mut s, 100, 40);
        for index in 1..=5 {
            assert!(text.contains(&format!("计划步骤{index}")), "{text}");
        }
        assert!(
            !text.contains("计划步骤8"),
            "panel is capped chrome, trailing steps stay off-screen: {text}"
        );
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
            started_elapsed_secs: 0,
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
            transcript.push_tool_started(id.clone(), "read_file".into(), "{}".into(), false, 0);
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
            (joined.contains("- old")
                || joined.contains("-old")
                || joined.lines().any(|l| l.contains('-') && l.contains("old")))
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
        assert!(!text.contains("line 30"), "folded diff is capped: {text}");
        // Click-first: the fold names the interaction, not a shortcut.
        assert!(
            text.contains("点击展开完整 Diff") && text.contains("…"),
            "must hint how to see the full diff: {text}"
        );
        assert!(
            !text.contains("Ctrl+O"),
            "shortcuts are not advertised: {text}"
        );

        let expanded = tool_render(&item, true);
        let text = expanded.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(text.contains("line 30"), "Ctrl+O expands the diff: {text}");
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
            started_elapsed_secs: 0,
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
            started_elapsed_secs: 0,
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
            started_elapsed_secs: 0,
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
            started_elapsed_secs: 0,
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

    fn sample_remote() -> crate::state::RemoteState {
        crate::state::RemoteState {
            invite: crate::action::RemoteInvite {
                qr: vec!["█▀▀█".into(), "█  █".into(), "▀▀▀▀".into()],
                payload: "LV1|rt_test|payload".into(),
                host_fingerprint: "af0f 3032 701b de9e".into(),
                relay_url: "http://172.20.54.97:18443".into(),
            },
            pending: None,
            outcome: None,
        }
    }

    #[test]
    fn remote_screen_shows_title_qr_meta_and_esc_footer() {
        let theme = Theme::dark();
        let t = crate::i18n::Locale::Zh.text();
        let lines = super::remote_screen_lines(&sample_remote(), 72, &theme, t);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains(t.screen_remote), "title: {text}");
        assert!(text.contains(t.remote_scan_heading), "heading: {text}");
        assert!(
            text.contains("http://172.20.54.97:18443"),
            "address: {text}"
        );
        assert!(text.contains("af0f 3032 701b de9e"), "host fp: {text}");
        assert!(
            text.contains("LV1|rt_test|payload"),
            "paste payload: {text}"
        );
        assert!(text.contains("Esc"), "Esc footer required: {text}");
        assert!(
            text.contains(t.remote_footer) || text.contains("返回对话"),
            "footer must tell the user Esc returns to chat: {text}"
        );
        // QR card frame
        assert!(text.contains('┌') && text.contains('└'), "QR frame: {text}");
    }

    #[test]
    fn remote_screen_pending_prompts_yn() {
        let theme = Theme::dark();
        let t = crate::i18n::Locale::Zh.text();
        let mut remote = sample_remote();
        remote.pending = Some(crate::action::PairingRequest {
            device_name: "iPhone".into(),
            platform: "iOS".into(),
            fingerprint: "bb11 cc22".into(),
        });
        let lines = super::remote_screen_lines(&remote, 72, &theme, t);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("iPhone") && text.contains("iOS"), "{text}");
        assert!(text.contains("bb11 cc22"), "{text}");
        assert!(
            text.contains("y ") || text.contains("y接受") || text.contains(t.remote_yn),
            "{text}"
        );
        assert!(text.contains("Esc"), "{text}");
    }
}
