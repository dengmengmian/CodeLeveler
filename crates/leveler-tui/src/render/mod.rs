//! Draws [`AppState`] to a Ratatui frame: header, transcript, status line, and
//! the composer. Layout degrades on narrow terminals .

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::screen::Screen;
use crate::state::AppState;
use crate::theme::Theme;
use crate::tool_cell::render_tools_screen;

/// Shell Details: one user shell execution — status, live runtime, source,
/// cwd, the user's exact command (never the `sh -c` wrapper), and the bounded
/// output tail. A Secondary Surface like every other Detail Page: Esc backs
/// out, `x` stops a running one, and the body sits on the shared content
/// gutter.
fn render_shell_screen(frame: &mut Frame, area: ratatui::layout::Rect, state: &mut AppState) {
    use crate::transcript::UserShellStatus;
    let theme = &state.theme;
    let t = state.t();
    let hint = if state
        .focused_user_shell()
        .is_some_and(|s| s.status == UserShellStatus::Running)
    {
        t.shell_hint_running
    } else {
        t.shell_hint_done
    };
    let page = crate::secondary::SecondaryPage {
        title: t.shell_details_title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint,
    };
    let layout = crate::secondary::layout(area, 0);
    crate::secondary::draw_header(frame, &layout, &page, theme);
    crate::secondary::draw_footer(frame, &layout, &page, theme);
    let body = layout.content;
    if body.width == 0 || body.height == 0 {
        return;
    }
    let lines = shell_detail_lines(state, body.width as usize, theme, t);
    // Keep the tail visible by default; PgUp/PgDn (screen_scroll) pages back.
    let height = body.height as usize;
    let max_scroll = lines.len().saturating_sub(height);
    let scroll = state.screen_scroll.min(max_scroll);
    let offset = max_scroll.saturating_sub(scroll);
    let visible: Vec<Line<'static>> = lines.into_iter().skip(offset).take(height).collect();
    frame.render_widget(Paragraph::new(visible), body);
}

/// The Shell Details body, without chrome.
fn shell_detail_lines(
    state: &AppState,
    width: usize,
    theme: &Theme,
    t: &crate::i18n::UiText,
) -> Vec<Line<'static>> {
    use crate::transcript::UserShellStatus;
    let dim = Style::default().fg(theme.text.muted);
    let text_style = Style::default().fg(theme.text.primary);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let Some(shell) = state.focused_user_shell() else {
        lines.push(Line::from(Span::styled(t.shell_no_output.to_string(), dim)));
        return lines;
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
        UserShellStatus::Unknown => (
            t.shell_status_unknown,
            Style::default().fg(theme.status.warning),
        ),
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
    const FIELD_LABEL_WIDTH: usize = 12;
    let field = |label: &str, value: Span<'static>| {
        Line::from(vec![
            Span::styled(
                crate::render::text::pad_display(label, FIELD_LABEL_WIDTH),
                dim,
            ),
            value,
        ])
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
    // Both fields are one unwrapped row, so anything past the edge is gone.
    // Cut them ourselves and mark it: a value that merely ends mid-path reads
    // as the whole value. A path keeps its tail — that is what names it — and
    // a command keeps its head, which is what it does.
    let value_width = width.saturating_sub(FIELD_LABEL_WIDTH + 1).max(4);
    lines.push(field(
        t.shell_cwd_label,
        Span::styled(text::elide_head(&shell.cwd, value_width), text_style),
    ));
    lines.push(field(
        t.shell_command_label,
        Span::styled(truncate_display(&shell.command, value_width), text_style),
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
    lines.push(section_line(t.shell_output_label, theme));
    if shell.output_truncated {
        lines.push(detail_line(t.shell_truncated.to_string(), dim, width));
    }
    if shell.output.trim().is_empty() {
        lines.push(detail_line(t.shell_no_output.to_string(), dim, width));
    } else {
        for raw in shell.output.lines() {
            let line = sanitize_terminal_line(raw);
            lines.push(detail_line(
                line,
                Style::default().fg(theme.text.secondary),
                width,
            ));
        }
    }
    lines
}
/// Whether the child behind this activity was stopped at a bound rather than
/// failing. Reads the typed stop, never the summary prose: `Incomplete`,
/// `Budget`, `Cancelled` and `Lost` are all "did not finish", and only a
/// runtime-typed `Failed` (or an untyped terminal) is a failure.
fn child_stopped_at_a_bound(state: &AppState, id: &crate::activity::ActivityId) -> bool {
    use leveler_client_protocol::ChildStop;
    let crate::activity::ActivityId::Child(child_id) = id else {
        return false;
    };
    state
        .team
        .children
        .iter()
        .find(|c| &c.id == child_id)
        .is_some_and(|c| {
            matches!(
                c.stop,
                Some(
                    ChildStop::Incomplete
                        | ChildStop::Budget
                        | ChildStop::Cancelled
                        | ChildStop::Lost
                )
            )
        })
}

/// Activity Detail: a Secondary Surface over the conversation.
///
/// Observational: Esc backs out without cancelling anything, and `x` stops a
/// running task only through the runtime's own cancellation path. The page owns
/// no lifecycle — it renders the same [`crate::activity::summaries`] projection
/// the status strip does, inside the same shell every other Secondary Surface
/// uses.
fn render_activity_screen(frame: &mut Frame, area: ratatui::layout::Rect, state: &mut AppState) {
    use crate::activity::ActivityKind;
    let t = state.t();
    let Some(id) = state.activity_open.clone() else {
        render_activity_stale(frame, area, state);
        return;
    };
    let Some(summary) = crate::activity::summaries(state)
        .into_iter()
        .find(|s| s.id == id)
    else {
        render_activity_stale(frame, area, state);
        return;
    };
    let detail_title = match summary.kind {
        ActivityKind::BackgroundTask => t.activity_title_background,
        ActivityKind::ChildAgent => t.activity_title_child,
    };
    let layout = crate::secondary::layout(area, 0);
    let body = layout.content;
    if body.width == 0 || body.height == 0 {
        return;
    }
    // Measure the body first: the follow chip in the footer reads the viewport
    // state this frame publishes, so the hint and the body agree on which frame
    // the user is looking at.
    let lines = activity_screen_lines(state, &id, &summary, body.width as usize, &state.theme, t);
    let height = body.height as usize;
    crate::activity::sync_view(state, lines.len(), height);
    let theme = &state.theme;
    let hint = activity_footer_hint(state, &id, &summary, t);
    let page = crate::secondary::SecondaryPage {
        title: detail_title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: &hint,
    };
    crate::secondary::draw_header(frame, &layout, &page, theme);
    crate::secondary::draw_footer(frame, &layout, &page, theme);
    let offset = state
        .activity_view
        .scroll
        .min(state.activity_view.max_scroll);
    let visible: Vec<Line<'static>> = lines.into_iter().skip(offset).take(height).collect();
    frame.render_widget(Paragraph::new(visible), body);
}

/// A closed / retired activity: still inside the shared shell, so the chrome
/// never flickers between two layouts.
fn render_activity_stale(frame: &mut Frame, area: ratatui::layout::Rect, state: &AppState) {
    let theme = &state.theme;
    let t = state.t();
    let page = crate::secondary::SecondaryPage {
        title: t.activity_title_background,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: t.activity_esc,
    };
    let layout = crate::secondary::layout(area, 0);
    crate::secondary::draw_header(frame, &layout, &page, theme);
    crate::secondary::draw_footer(frame, &layout, &page, theme);
    let lines = vec![Line::from(Span::styled(
        t.activity_stale.to_string(),
        Style::default().fg(theme.text.muted),
    ))];
    frame.render_widget(Paragraph::new(lines), layout.content);
}

/// The Background Jobs list page: running tasks first, then recently finished
/// ones. Enter opens the selected task's Detail Page, `x` stops a selected
/// running task, Esc returns to the conversation. Acknowledging the current
/// failures happens when the page opens (`activity::open_background_list`), so
/// this renderer stays pure.
fn render_background_list_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let theme = &state.theme;
    let t = state.t();
    let list = crate::activity::background_job_list(state);
    let hint = if list.running.is_empty() {
        t.background_hint_list_idle
    } else {
        t.background_hint_list_running
    };
    let page = crate::secondary::SecondaryPage {
        title: t.background_list_title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint,
    };
    let layout = crate::secondary::layout(area, 0);
    crate::secondary::draw_header(frame, &layout, &page, theme);
    crate::secondary::draw_footer(frame, &layout, &page, theme);
    let body = layout.content;
    if body.width == 0 || body.height == 0 {
        return;
    }
    let (lines, selected_line) = background_list_body(state, &list, body.width as usize, theme, t);
    let height = body.height as usize;
    // Keep the selection visible without a second scroll owner: the offset is
    // derived from the selection, so the list needs no scroll state and a
    // resize can never strand the highlighted row.
    let max_offset = lines.len().saturating_sub(height);
    let offset = selected_line
        .map(|line| line.saturating_sub(height.saturating_sub(1)))
        .unwrap_or(0)
        .min(max_offset);
    let visible: Vec<Line<'static>> = lines.into_iter().skip(offset).take(height).collect();
    frame.render_widget(Paragraph::new(visible), body);
}

/// The list body plus the line index of the selected row, or `None` when
/// nothing is selected (or the list is empty).
fn background_list_body(
    state: &AppState,
    list: &crate::activity::BackgroundJobList,
    width: usize,
    theme: &Theme,
    t: &crate::i18n::UiText,
) -> (Vec<Line<'static>>, Option<usize>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut selected_line = None;
    let selected = state.background_list_selected.as_deref();
    if list.is_empty() {
        lines.push(Line::from(Span::styled(
            t.background_list_empty.to_string(),
            Style::default().fg(theme.text.muted),
        )));
        return (lines, None);
    }
    if !list.running.is_empty() {
        lines.push(section_line(
            &format!("{} · {}", t.background_list_running, list.running.len()),
            theme,
        ));
        for summary in &list.running {
            let on = selected == Some(summary.id.as_key());
            if on {
                selected_line = Some(lines.len());
            }
            lines.push(background_list_row(summary, on, width, theme));
        }
    }
    if !list.finished.is_empty() {
        lines.push(Line::from(""));
        lines.push(section_line(
            &format!("{} · {}", t.background_list_finished, list.finished.len()),
            theme,
        ));
        for summary in &list.finished {
            let on = selected == Some(summary.id.as_key());
            if on {
                selected_line = Some(lines.len());
            }
            lines.push(background_list_row(summary, on, width, theme));
        }
    }
    (lines, selected_line)
}

/// One job row: `→ ● label … duration`. The glyph carries the status ink, the
/// duration is right-aligned in muted grey, and the label truncates first so a
/// long command can never push the duration off the edge.
fn background_list_row(
    summary: &crate::activity::ActivitySummary,
    selected: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    use crate::activity::ActivityStatus;
    let indent = crate::layout::DETAIL_BODY_INDENT;
    let prefix = if selected { "→ " } else { "  " };
    let glyph = crate::activity::activity_glyph(summary.status);
    let dur = crate::status_line::fmt_elapsed(summary.duration_secs);
    let accent = Style::default().fg(theme.accent.primary);
    let glyph_style = match summary.status {
        ActivityStatus::Running | ActivityStatus::Waiting => accent,
        ActivityStatus::Completed => Style::default().fg(theme.status.success),
        ActivityStatus::Failed => Style::default().fg(theme.status.error),
        ActivityStatus::Stopped => Style::default().fg(theme.text.muted),
        ActivityStatus::Interrupted | ActivityStatus::Unreported => {
            Style::default().fg(theme.status.warning)
        }
    };
    let label_style = Style::default().fg(if selected {
        theme.text.primary
    } else {
        theme.text.secondary
    });
    let head = format!("{indent}{prefix}{glyph} ");
    let head_w = unicode_width::UnicodeWidthStr::width(head.as_str());
    let dur_w = unicode_width::UnicodeWidthStr::width(dur.as_str());
    const GAP: usize = 2;
    let label_budget = width.saturating_sub(head_w + GAP + dur_w).max(4);
    let label = truncate_display(&summary.title, label_budget);
    let label_w = unicode_width::UnicodeWidthStr::width(label.as_str());
    let pad = width.saturating_sub(head_w + label_w + dur_w);
    Line::from(vec![
        Span::styled(head, glyph_style),
        Span::styled(label, label_style),
        Span::raw(" ".repeat(pad)),
        Span::styled(dur, Style::default().fg(theme.text.muted)),
    ])
}

/// The Detail Page's interaction hint. Every Detail Page reads `Esc 返回`; a
/// task the runtime can still stop adds `x 停止`; the background follow chip
/// lives here rather than in the body so it never scrolls away from the output
/// it describes.
fn activity_footer_hint(
    state: &AppState,
    id: &crate::activity::ActivityId,
    summary: &crate::activity::ActivitySummary,
    t: &crate::i18n::UiText,
) -> String {
    use crate::activity::{ActivityId, ActivityStatus};
    let running = matches!(
        summary.status,
        ActivityStatus::Running | ActivityStatus::Waiting
    );
    // A child can always be cancelled. A background task only advertises the
    // stop while the runtime still holds it running — a terminal one has
    // nothing to stop, and the footer must not promise a key that does nothing.
    let stoppable = running
        && match id {
            ActivityId::Child(_) => true,
            ActivityId::Background(task_id) => state
                .background_task_labels
                .get(task_id)
                .is_some_and(|c| c.is_running()),
        };
    let mut hint = t.activity_esc.to_string();
    if stoppable {
        hint.push_str(" · ");
        hint.push_str(t.activity_stop);
    }
    if let ActivityId::Background(task_id) = id
        && state
            .background_task_labels
            .get(task_id)
            .is_some_and(|c| !c.output.trim().is_empty())
    {
        let chip = if state.activity_view.follow {
            t.activity_follow_on.to_string()
        } else if state.activity_view.unread > 0 {
            format!(
                "{} · {}",
                t.activity_follow_paused,
                t.activity_new_lines
                    .replace("{}", &state.activity_view.unread.to_string())
            )
        } else {
            t.activity_follow_paused.to_string()
        };
        hint.push_str(" · ");
        hint.push_str(&chip);
    }
    hint.push_str(" · ");
    hint.push_str(t.activity_hint_scroll);
    hint
}

/// The Detail Page body, without chrome. Pure (no Frame, no scroll state) so
/// renderer tests assert the exact content at a given width.
fn activity_screen_lines(
    state: &AppState,
    id: &crate::activity::ActivityId,
    summary: &crate::activity::ActivitySummary,
    width: usize,
    theme: &Theme,
    t: &crate::i18n::UiText,
) -> Vec<Line<'static>> {
    use crate::activity::ActivityId;
    let mut lines: Vec<Line<'static>> = Vec::new();
    // The object itself owns the strongest ink: the page exists to name it.
    lines.push(Line::from(Span::styled(
        truncate_display(&summary.title, width.max(8)),
        Style::default()
            .fg(theme.text.primary)
            .add_modifier(Modifier::BOLD),
    )));
    let (status_text, status_style) = activity_status_text(state, id, summary, t, theme);
    let glyph = crate::activity::activity_glyph(summary.status);
    lines.push(Line::from(Span::styled(
        format!(
            "{glyph} {status_text} · {}",
            crate::status_line::fmt_elapsed(summary.duration_secs)
        ),
        status_style,
    )));
    match id {
        ActivityId::Background(task_id) => {
            background_detail_body(state, task_id, summary, width, theme, t, &mut lines)
        }
        ActivityId::Child(child_id) => {
            child_detail_body(state, child_id, summary, width, theme, t, &mut lines)
        }
    }
    lines
}

/// Status word + ink for one activity, including the typed stop reason and a
/// background task's exit code. Shared by every Detail Page so the wording can
/// never drift from the summary projection.
fn activity_status_text(
    state: &AppState,
    id: &crate::activity::ActivityId,
    summary: &crate::activity::ActivitySummary,
    t: &crate::i18n::UiText,
    theme: &Theme,
) -> (String, Style) {
    use crate::activity::{ActivityId, ActivityStatus};
    let mut text = match summary.status {
        ActivityStatus::Running => t.wait_target_running.to_string(),
        ActivityStatus::Waiting => t.sub_agent_waiting.to_string(),
        ActivityStatus::Completed => t.title_completed.to_string(),
        // A child stopped at a bound did not FAIL — it did not finish.
        ActivityStatus::Failed if child_stopped_at_a_bound(state, id) => {
            t.sub_agent_incomplete.to_string()
        }
        ActivityStatus::Failed => t.title_failed.to_string(),
        // A stopped task did not fail: user/agent cancel or session cleanup.
        // Its own word, never the failure branch.
        ActivityStatus::Stopped => t.background_status_stopped.to_string(),
        ActivityStatus::Interrupted => t.sub_agent_interrupted.to_string(),
        ActivityStatus::Unreported => t.sub_agent_unreported.to_string(),
    };
    let style = match summary.status {
        ActivityStatus::Running | ActivityStatus::Waiting => {
            Style::default().fg(theme.accent.primary)
        }
        ActivityStatus::Completed => Style::default().fg(theme.status.success),
        ActivityStatus::Failed => Style::default().fg(theme.status.error),
        // Stopped is neutral, not alarming: it yields to failed and to a live
        // running accent.
        ActivityStatus::Stopped => Style::default().fg(theme.text.secondary),
        ActivityStatus::Interrupted | ActivityStatus::Unreported => {
            Style::default().fg(theme.status.warning)
        }
    };
    match id {
        ActivityId::Background(task_id) => {
            if let Some(chrome) = crate::activity::background_chrome(state, task_id)
                && let Some(code) = chrome.exit_code
            {
                text.push_str(&format!(" · exit {code}"));
            }
        }
        // How it ended is the question the detail page exists to answer.
        ActivityId::Child(child_id) => {
            if let Some(reason) = state
                .team
                .children
                .iter()
                .find(|c| &c.id == child_id)
                .and_then(|c| crate::multi_agent::child_stop_label(c.stop, c.limit, t))
            {
                text.push_str(&format!(" · {reason}"));
            }
        }
    }
    (text, style)
}

/// A background task's body: the command it runs and its retained output, each
/// under its own section. Output is real (the task projection's retained tail);
/// when none has arrived the section says so in place rather than leaving the
/// page's left edge bare.
fn background_detail_body(
    state: &AppState,
    task_id: &str,
    summary: &crate::activity::ActivitySummary,
    width: usize,
    theme: &Theme,
    t: &crate::i18n::UiText,
    lines: &mut Vec<Line<'static>>,
) {
    let dim = Style::default().fg(theme.text.muted);
    let text = Style::default().fg(theme.text.primary);
    lines.push(Line::from(""));
    lines.push(section_line(t.activity_command_label, theme));
    lines.push(detail_line(format!("$ {}", summary.title), text, width));
    lines.push(Line::from(""));
    lines.push(section_line(t.activity_output_label, theme));
    let chrome = crate::activity::background_chrome(state, task_id);
    let output = chrome.map(|c| c.output.as_str()).unwrap_or("");
    if output.trim().is_empty() {
        lines.push(detail_line(t.activity_no_output.to_string(), dim, width));
        return;
    }
    for raw in output.lines() {
        let line = sanitize_terminal_line(raw);
        lines.push(detail_line(
            line,
            Style::default().fg(theme.text.secondary),
            width,
        ));
    }
}

/// A sub-agent's body. The semantic task, then either the running call or the
/// settled result, then the folded activity. Capability facts (profile,
/// read-only, usage) are demoted to one muted metadata line: they are useful,
/// but they are not the answer the reader came for.
fn child_detail_body(
    state: &AppState,
    child_id: &str,
    summary: &crate::activity::ActivitySummary,
    width: usize,
    theme: &Theme,
    t: &crate::i18n::UiText,
    lines: &mut Vec<Line<'static>>,
) {
    use crate::activity::ActivityStatus;
    let dim = Style::default().fg(theme.text.muted);
    let text = Style::default().fg(theme.text.primary);
    let Some(child) = state.team.children.iter().find(|c| c.id == child_id) else {
        lines.push(Line::from(""));
        lines.push(detail_line(t.activity_stale.to_string(), dim, width));
        return;
    };
    if let Some(meta) = child_metadata_line(child, t) {
        lines.push(Line::from(Span::styled(meta, dim)));
    }
    // The goal: the semantic title the runtime fixed at spawn, else the
    // purpose (the same text the transcript head already names).
    let goal = child
        .title
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| (!child.purpose.trim().is_empty()).then_some(child.purpose.as_str()));
    if let Some(goal) = goal {
        lines.push(Line::from(""));
        lines.push(section_line(t.activity_goal, theme));
        lines.push(detail_line(goal.to_string(), text, width));
    }
    let settled = !matches!(
        summary.status,
        ActivityStatus::Running | ActivityStatus::Waiting
    );
    let block = state.transcript.items().iter().find_map(|item| match item {
        crate::transcript::TranscriptItem::SubAgent(b) if b.id == child_id => Some(b),
        _ => None,
    });
    if settled {
        // Prefer the runtime's own final message. A settled child with none
        // falls back to its contribution projection — never an invented
        // "result" harvested from tool logs.
        let result = block
            .filter(|b| !b.detail.trim().is_empty())
            .map(|b| sub_agent_detail(b.detail.trim(), t))
            .or_else(|| crate::multi_agent::contribution_line(child, t));
        if let Some(result) = result.filter(|s| !s.trim().is_empty()) {
            lines.push(Line::from(""));
            lines.push(section_line(t.activity_result, theme));
            for row in wrap(&result, content_width(width)) {
                lines.push(detail_line(row, text, width));
            }
        }
    } else if let Some(current) = child_current_line(child, state.locale, t) {
        lines.push(Line::from(""));
        lines.push(section_line(t.activity_current, theme));
        lines.push(detail_line(current, text, width));
    }
    lines.push(Line::from(""));
    lines.push(section_line(t.activity_steps, theme));
    let avail = content_width(width);
    let activity = crate::activity_stream::child_activity_lines(
        &child.activity,
        theme,
        state.locale,
        t,
        !settled,
        avail,
    );
    for line in activity {
        lines.push(prefix_indent(line, crate::layout::DETAIL_BODY_INDENT));
    }
}

/// The current call of a running child, in the same user language the main
/// activity stream uses. Falls back to the raw `recent_step` only when no typed
/// call has arrived.
fn child_current_line(
    child: &crate::multi_agent::ChildAgentView,
    locale: crate::i18n::Locale,
    t: &crate::i18n::UiText,
) -> Option<String> {
    if let Some(call) = child.activity.last() {
        let action = crate::tool_cell::tool_action_label_for(&call.tool, locale);
        let summary = crate::tool_cell::tool_summary_for(&call.tool, &call.arguments, t);
        return Some(if summary.is_empty() {
            action
        } else {
            format!("{action} {summary}")
        });
    }
    child.recent_step.clone().filter(|s| !s.trim().is_empty())
}

/// One muted metadata line for a child: the capability bounds and usage. The
/// declared agent already rides on the identity line, so only a profile with no
/// agent name is repeated here.
fn child_metadata_line(
    child: &crate::multi_agent::ChildAgentView,
    t: &crate::i18n::UiText,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if child.agent_name.is_none()
        && let Some(profile) = child.profile_id.as_deref().filter(|p| !p.trim().is_empty())
    {
        parts.push(profile.to_string());
    }
    if child.read_only {
        parts.push(t.inspector_read_only.to_string());
    }
    if child.input_tokens > 0 || child.output_tokens > 0 {
        parts.push(format!(
            "↑ {} · ↓ {}",
            crate::status_line::fmt_tokens(child.input_tokens),
            crate::status_line::fmt_tokens(child.output_tokens)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// A section title at the page gutter.
fn section_line(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default()
            .fg(theme.text.secondary)
            .add_modifier(Modifier::BOLD),
    ))
}

/// A section body row at [`crate::layout::DETAIL_BODY_INDENT`], truncated to
/// the section's own width so a long value never overruns the right gutter.
fn detail_line(value: impl Into<String>, style: Style, width: usize) -> Line<'static> {
    let value = value.into();
    let avail = content_width(width);
    Line::from(Span::styled(
        format!(
            "{}{}",
            crate::layout::DETAIL_BODY_INDENT,
            truncate_display(&value, avail)
        ),
        style,
    ))
}

/// Prefix an already-built line with the shared detail indent.
fn prefix_indent(line: Line<'static>, indent: &str) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(indent.to_string()));
    spans.extend(line.spans);
    Line::from(spans)
}

/// Columns available to a section body once the detail indent is spent.
fn content_width(width: usize) -> usize {
    width
        .saturating_sub(crate::layout::DETAIL_BODY_INDENT.len())
        .max(4)
}

#[cfg(test)]
pub(crate) use crate::tool_cell::tool_action_label;
#[cfg(test)]
pub(crate) use crate::tool_cell::tool_summary;
pub(crate) use crate::tool_cell::tool_summary_for;

mod footer;
mod panes;
mod screens;
pub(crate) mod text;
mod transcript_lines;

pub(crate) use footer::background_footer_spans;
pub(crate) use footer::key_hint_line;
pub(crate) use footer::user_turn_summaries;
pub use transcript_lines::{
    assistant_render, assistant_split, item_is_final, item_render, items_need_gap,
    sub_agent_tree_lines,
};
pub(crate) use transcript_lines::{sub_agent_detail, turn_end_marker, user_shell_lines};

pub(crate) use panes::pad_line_to_width;
pub(crate) use panes::{render_list_focused, render_scrolled};
pub(crate) use screens::screen_title;
pub(crate) use text::{pad_display, sanitize_terminal_line, truncate_display, wrap};

pub(crate) use footer::{
    COMPOSER_MAX_ROWS, composer_box_lines, composer_visible_rows, render_attachments,
    render_slash_popup,
};
use screens::{render_diff_screen, render_help_screen, render_sessions_screen};

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
        // The side thread is a surface of its own, not a card over the main
        // workbench: it owns the viewport and the composer while focused.
        if state.surface == crate::btw::SurfaceFocus::Btw {
            crate::btw::render(frame, area, state);
        } else {
            crate::workbench::render_workbench(frame, state);
        }
        // A `/update` in flight owns the foreground until it finishes; drawn
        // over the workbench so the user can still see the task behind it.
        if let Some(view) = &state.update {
            crate::update::render_panel(frame, area, view, &state.theme, state.t());
        }
        return;
    }

    // Non-conversation screens own their own chrome over the full area — the
    // shared Secondary Surface shell, or the legacy app frame for views that do
    // not fit it yet. No composer: these views answer every key themselves
    // (Esc, x, j/k), so an input box here would take a whole typed task with no
    // echo and no submit. The draft is kept and comes back with the
    // conversation.
    match state.active_screen {
        Screen::Conversation => unreachable!(),
        Screen::Tools => render_tools_screen(frame, area, state),
        Screen::Diff => render_diff_screen(frame, area, state),
        Screen::Sessions => render_sessions_screen(frame, area, state),
        Screen::Remote => render_remote_screen(frame, area, state),
        Screen::Shell => render_shell_screen(frame, area, state),
        Screen::Activity => render_activity_screen(frame, area, state),
        Screen::ActivityList => render_background_list_screen(frame, area, state),
        Screen::Help => render_help_screen(frame, area, state),
        Screen::Trace => crate::observability::render_trace_screen(frame, area, state),
        Screen::Context => crate::context::render_context_screen(frame, area, state),
    }
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
    let area = crate::secondary::legacy_frame(frame, area, state);
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
                kind: crate::transcript::AssistantKind::Final,
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
            kind: crate::transcript::AssistantKind::Final,
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
            kind: crate::transcript::AssistantKind::Final,
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
            kind: crate::transcript::AssistantKind::Final,
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
            r#"{"program":"cargo","args":["check","-p","example-core"]}"#,
        );
        assert_eq!(s, "cargo check -p example-core");
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
        // `tool_summary` is the Chinese-pinned form, so the joining word is
        // Chinese too — it used to be a hardcoded English " in " in both.
        let s = tool_summary("grep", r#"{"pattern":"TODO","path":"crates"}"#);
        assert_eq!(s, "\"TODO\" 于 crates");
        let en = crate::tool_cell::tool_summary_for(
            "grep",
            r#"{"pattern":"TODO","path":"crates"}"#,
            crate::i18n::Locale::En.text(),
        );
        assert_eq!(en, "\"TODO\" in crates");
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

    /// Shell Details answers "where did this run" and "what exactly ran". Both
    /// fields were pushed as one unwrapped span, so ratatui clipped them at the
    /// right edge with no mark — and for a path the clipped-off tail is the
    /// part that identifies it. A real `!git diff --stat` showed
    /// `目录  /private/tmp/.../b6daaffb-de00-45ce-bd4d-8fcd3`, which reads as a
    /// complete directory and is not one.
    #[test]
    fn shell_details_marks_a_cut_path_and_keeps_its_tail() {
        let mut state = test_state();
        let cwd = "/private/tmp/agent-501/-Users-example-Develop-app-codeleveler/\
                   b6daaffb-de00-45ce-bd4d-b457-f1fd58344a9c/scratchpad/h-inv";
        crate::reducer::reduce(
            &mut state,
            crate::action::Action::Runtime(
                leveler_client_protocol::RuntimeEvent::UserShellStarted {
                    execution_id: leveler_core::UserShellId::new("ush-1"),
                    command: format!("git {} --stat", "verylongsubcommand".repeat(6)),
                    cwd: cwd.into(),
                },
            ),
        );
        state.active_screen = Screen::Shell;
        let screen = render_text(&mut state, 80, 30);
        let dir = screen
            .lines()
            .find(|l| l.contains("目录"))
            .expect("the cwd field is on screen")
            .to_string();
        assert!(dir.contains('…'), "the cut is unmarked: {dir:?}");
        assert!(
            dir.contains("h-inv"),
            "the tail is what names the directory: {dir:?}"
        );
        let cmd = screen
            .lines()
            .find(|l| l.contains("命令"))
            .expect("the command field is on screen")
            .to_string();
        assert!(cmd.contains('…'), "the cut is unmarked: {cmd:?}");
    }

    /// A full-screen view answers every keystroke itself — Esc backs out, `x`
    /// stops a shell, j/k scroll — so nothing typed reaches the composer. It
    /// was still drawn under them: on Shell Details a whole task can be typed
    /// with no echo and Enter does nothing, because the box is not an input
    /// there. An overlay already takes the composer's slot for this reason.
    #[test]
    fn a_full_screen_view_does_not_draw_an_input_it_will_not_accept() {
        for screen in [
            Screen::Shell,
            Screen::Tools,
            Screen::Diff,
            Screen::Help,
            Screen::Sessions,
            Screen::Activity,
        ] {
            let mut state = test_state();
            state.composer.replace("DRAFTMARKER");
            state.active_screen = screen;
            let text = render_text(&mut state, 80, 30);
            assert!(
                !text.contains("DRAFTMARKER"),
                "{screen:?} draws a composer that swallows every key:\n{text}"
            );
        }
    }

    /// An empty output section is still a section: the muted state sits at the
    /// section body indent, not drifting on the page's left edge.
    #[test]
    fn background_detail_empty_output_is_an_indented_section_state() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-empty".into(),
            crate::state::BackgroundTaskChrome::running("make up", 0),
        );
        state.activity_open = Some(crate::activity::ActivityId::Background("bg-empty".into()));
        state.active_screen = Screen::Activity;
        let text = render_text(&mut state, 90, 24);
        assert!(text.contains("  输出"), "section present: {text}");
        assert!(
            text.lines().any(|l| l.trim_end() == "    暂无输出"),
            "empty state is indented under its section:\n{text}"
        );
    }

    /// A background Activity Detail shows the real command, its retained
    /// output, and the viewport's follow state — the same surface the child
    /// detail uses, not a second background-specific screen.
    #[test]
    fn background_detail_shows_command_output_and_follow_state() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-1".into(),
            crate::state::BackgroundTaskChrome {
                label: "cargo test --workspace".into(),
                started_elapsed_secs: 0,
                ok: None,
                stopped: false,
                exit_code: None,
                duration_ms: None,
                output: "Compiling leveler-core ...\ntest result: ok\n".into(),
            },
        );
        state.activity_open = Some(crate::activity::ActivityId::Background("bg-1".into()));
        state.active_screen = Screen::Activity;
        let text = render_text(&mut state, 90, 30);
        assert!(text.contains("Background Task"), "{text}");
        assert!(text.contains("$ cargo test --workspace"), "{text}");
        assert!(text.contains("Compiling leveler-core"), "{text}");
        assert!(text.contains("Follow ON"), "{text}");

        // Scrolling back pauses follow and says so; the content stays put.
        crate::activity::scroll_lines(&mut state, -1);
        let text = render_text(&mut state, 90, 30);
        assert!(text.contains("Follow PAUSED"), "{text}");
        assert!(!text.contains("Follow ON"), "{text}");
    }

    /// A terminal background task keeps its detail: the row is not deletable
    /// just because the process exited, and the page switches to the terminal
    /// status instead of closing.
    #[test]
    fn a_finished_background_detail_stays_readable() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-1".into(),
            crate::state::BackgroundTaskChrome {
                label: "cargo test --workspace".into(),
                started_elapsed_secs: 0,
                ok: Some(true),
                stopped: false,
                exit_code: Some(0),
                duration_ms: Some(133_000),
                output: "test result: ok. 428 passed\n".into(),
            },
        );
        state.activity_open = Some(crate::activity::ActivityId::Background("bg-1".into()));
        state.active_screen = Screen::Activity;
        let text = render_text(&mut state, 90, 30);
        assert!(text.contains("Background Task"), "{text}");
        assert!(text.contains("test result: ok"), "{text}");
        assert!(text.contains("exit 0"), "{text}");
        assert!(
            !text.contains("x 停止"),
            "a terminal task has nothing left to stop:\n{text}"
        );
    }

    /// A running child with a known activity log: six reads, nine commands and
    /// a live `grep`. Builds the read model directly so the page can be asserted
    /// without a runtime.
    fn running_child_with_activity() -> AppState {
        use crate::multi_agent::ChildUpdate;
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        state.team.apply_update(ChildUpdate {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            done: false,
            ok: false,
            detail: "调查 portal-web 的选择器归属".into(),
            title: Some("调查选择器归属".into()),
            profile_id: None,
            agent_name: None,
            read_only: true,
            contribution: None,
            stop: None,
            limit: None,
            started_elapsed_secs: 0,
        });
        for _ in 0..6 {
            state.team.apply_activity(
                "c1",
                "tool_started",
                "read_file",
                r#"{"path":"public-order.css"}"#,
                false,
            );
            state
                .team
                .apply_activity("c1", "tool_finished", "read_file", "ok", false);
        }
        for _ in 0..9 {
            state.team.apply_activity(
                "c1",
                "tool_started",
                "run_command",
                r#"{"program":"rg","args":["selector","."]}"#,
                false,
            );
            state
                .team
                .apply_activity("c1", "tool_finished", "run_command", "match", false);
        }
        state.team.apply_activity(
            "c1",
            "tool_started",
            "grep",
            r#"{"pattern":"public-order","path":"web"}"#,
            false,
        );
        state.activity_open = Some(crate::activity::ActivityId::Child("c1".into()));
        state.active_screen = Screen::Activity;
        state
    }

    /// A Detail Page is not a bare renderer: the navigation owns the edge, the
    /// page body sits on the shared content gutter, and a section's body is one
    /// level deeper. The spacing comes from the shared tokens, so both detail
    /// pages line up at the same columns.
    #[test]
    fn detail_pages_share_one_content_gutter() {
        let mut state = test_state();
        state.background_task_labels.insert(
            "bg-1".into(),
            crate::state::BackgroundTaskChrome {
                label: "cargo test --workspace".into(),
                started_elapsed_secs: 0,
                ok: None,
                stopped: false,
                exit_code: None,
                duration_ms: None,
                output: "Compiling leveler-tui\n".into(),
            },
        );
        state.activity_open = Some(crate::activity::ActivityId::Background("bg-1".into()));
        state.active_screen = Screen::Activity;
        let text = render_text(&mut state, 90, 30);
        let lines: Vec<&str> = text.lines().collect();
        let header = lines
            .iter()
            .find(|l| l.contains("Background Task"))
            .expect("identity header");
        assert!(
            header.starts_with('←'),
            "back nav owns the edge: {header:?}"
        );
        for expected in [
            "  cargo test --workspace",
            "  命令",
            "    $ cargo test --workspace",
            "  输出",
            "    Compiling leveler-tui",
        ] {
            assert!(
                lines.iter().any(|l| l.trim_end() == expected),
                "missing {expected:?} in:\n{text}"
            );
        }
    }

    /// The Sub-agent page is not a tool trace. Repeated calls fold into one
    /// semantic line, the live call is the only expanded one, and no raw tool
    /// name leaks.
    #[test]
    fn sub_agent_detail_folds_tool_runs_and_never_shows_raw_tool_names() {
        let mut state = running_child_with_activity();
        let text = render_text(&mut state, 90, 40);
        assert!(text.contains("读取 6 个文件"), "{text}");
        assert!(text.contains("执行了 9 个命令"), "{text}");
        assert!(
            !text.contains("run_command"),
            "raw tool name leaked:\n{text}"
        );
        assert!(!text.contains("read_file"), "raw tool name leaked:\n{text}");
        // The live call is expanded: its action and its target are both shown.
        assert!(text.contains("● 搜索代码"), "{text}");
        assert!(text.contains("public-order"), "{text}");
        // No row may claim two contradictory states at once.
        for line in text.lines() {
            assert!(
                !(line.contains('●') && line.contains('✓')),
                "conflicting status glyphs: {line:?}"
            );
        }
    }

    /// The footer is the same language on every Detail Page: a running task can
    /// be stopped, a settled one cannot, and `Esc 返回` is always present.
    #[test]
    fn detail_footer_says_stop_only_while_stoppable() {
        let mut running = running_child_with_activity();
        let run_text = render_text(&mut running, 90, 30);
        assert!(run_text.contains("Esc 返回"), "{run_text}");
        assert!(run_text.contains("x 停止"), "{run_text}");

        let mut settled = running_child_with_activity();
        settled.team.apply_update(crate::multi_agent::ChildUpdate {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            done: true,
            ok: true,
            detail: "调查完成".into(),
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: true,
            contribution: None,
            stop: None,
            limit: None,
            started_elapsed_secs: 0,
        });
        let settled_text = render_text(&mut settled, 90, 30);
        assert!(settled_text.contains("Esc 返回"), "{settled_text}");
        assert!(
            !settled_text.contains("x 停止"),
            "a settled child has nothing to stop:\n{settled_text}"
        );
    }

    /// Every Detail Page survives a terminal too narrow for its own columns.
    #[test]
    fn detail_pages_do_not_panic_on_a_narrow_terminal() {
        for (w, h) in [(12u16, 5u16), (20, 6), (30, 5), (40, 24)] {
            let mut bg = test_state();
            bg.background_task_labels.insert(
                "bg-1".into(),
                crate::state::BackgroundTaskChrome {
                    label: "a-very-long-command --with a-very-long-argument".into(),
                    started_elapsed_secs: 0,
                    ok: None,
                    stopped: false,
                    exit_code: None,
                    duration_ms: None,
                    output: "a line of output that is much wider than the terminal\n".into(),
                },
            );
            bg.activity_open = Some(crate::activity::ActivityId::Background("bg-1".into()));
            bg.active_screen = Screen::Activity;
            let _ = render_text(&mut bg, w, h);

            let mut child = running_child_with_activity();
            let _ = render_text(&mut child, w, h);
        }
    }

    /// Resizing the terminal re-lays out every frame; the page must keep its
    /// identity, gutter and footer at each size and leave no ghost of the
    /// previous geometry.
    #[test]
    fn detail_page_relayouts_cleanly_across_resizes() {
        let mut state = running_child_with_activity();
        for (w, h) in [(100u16, 40u16), (40, 12), (80, 24), (30, 8)] {
            let text = render_text(&mut state, w, h);
            assert!(text.starts_with('←'), "w={w}: nav at the edge:\n{text}");
            assert!(
                text.contains("Sub-agent"),
                "w={w}: page identity survives resize:\n{text}"
            );
            assert!(
                text.contains("Esc 返回") && text.contains("x 停止"),
                "w={w}: footer stays fixed:\n{text}"
            );
            for line in text.lines() {
                assert!(
                    line.chars().count() as u16 <= w,
                    "w={w}: a row overran the terminal: {line:?}"
                );
            }
        }
    }

    /// A child that was stopped and a child that finished are not the same
    /// thing, and the detail page is where a reader goes to find out which.
    /// It said only "失败", while the runtime had typed the reason — the same
    /// reason `stop_label` prints everywhere else.
    #[test]
    fn a_stopped_child_detail_says_how_it_was_stopped() {
        let mut state = stopped_child_state();
        let text = render_text(&mut state, 90, 30);
        assert!(
            text.contains("预算耗尽"),
            "the detail must name the stop the runtime typed:\n{text}"
        );
    }

    /// A child the wall clock stopped is not a child that "failed on its
    /// budget". The runtime typed the bound; the surface must say it: the
    /// detail reads unfinished, and the reason is the timeout, not the
    /// generic budget word that also covers a spent token budget.
    #[test]
    fn a_wall_clock_timeout_detail_reads_as_unfinished_timeout() {
        let mut state =
            stopped_child_state_hit(Some(leveler_client_protocol::ChildLimit::Duration));
        let text = render_text(&mut state, 90, 30);
        let meta = text
            .lines()
            .find(|l| l.contains("超时"))
            .unwrap_or_else(|| panic!("the detail must name the wall-clock bound:\n{text}"));
        assert!(
            meta.contains("未完成") && !meta.contains("失败"),
            "a stopped child did not fail; it did not finish: {meta:?}"
        );
        assert!(
            !meta.contains("预算耗尽"),
            "the duration bound must not read as a spent budget: {meta:?}"
        );
    }

    /// The distinction is the typed limit, not the wording: a token-budget
    /// stop keeps the budget label.
    #[test]
    fn a_token_budget_stop_keeps_the_budget_word() {
        let mut state =
            stopped_child_state_hit(Some(leveler_client_protocol::ChildLimit::ModelTokens));
        let text = render_text(&mut state, 90, 30);
        assert!(text.contains("预算耗尽"), "{text}");
        assert!(!text.contains("超时"), "{text}");
    }

    /// Cross-feature: the two concurrent closures meet on one row. A child that
    /// timed out keeps the semantic task title from its spawn — not the full
    /// purpose — and its detail reads the wall-clock bound, not a generic
    /// budget word. Neither feature may overwrite the other's fact.
    #[test]
    fn a_timed_out_child_keeps_its_task_title_and_reads_as_timeout() {
        use crate::multi_agent::ChildUpdate;
        let mut state = test_state();
        let update = |done: bool, stop, limit| ChildUpdate {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            done,
            ok: false,
            detail: "你在 CodeLeveler 仓库里做一次只读调查，解释 Windows CI 的两个 flaky tests。"
                .into(),
            title: Some("调查 Windows CI 两个 flaky tests".into()),
            profile_id: None,
            agent_name: None,
            read_only: true,
            contribution: None,
            stop,
            limit,
            started_elapsed_secs: 0,
        };
        state.team.apply_update(update(false, None, None));
        state.team.apply_update(update(
            true,
            Some(leveler_client_protocol::ChildStop::Budget),
            Some(leveler_client_protocol::ChildLimit::Duration),
        ));

        let summary = crate::activity::summaries(&state)
            .into_iter()
            .find(|s| s.id == crate::activity::ActivityId::Child("c1".into()))
            .expect("the child row");
        assert_eq!(
            summary.task_title.as_deref(),
            Some("调查 Windows CI 两个 flaky tests"),
            "the semantic title must survive the terminal and must not fall back to the purpose"
        );

        state.activity_open = Some(crate::activity::ActivityId::Child("c1".into()));
        state.active_screen = Screen::Activity;
        let text = render_text(&mut state, 90, 30);
        assert!(text.contains("未完成") && text.contains("超时"), "{text}");
        assert!(!text.contains("预算耗尽"), "{text}");
    }

    /// Same rule `collaboration_glyph` already enforces one panel over: a call
    /// that ran is not an outcome that succeeded. Every step of an aborted
    /// child carried a success check, so a reviewer that was cut off mid-way
    /// read as one that finished every step it started.
    #[test]
    fn the_steps_of_a_stopped_child_are_not_marked_done() {
        let mut state = stopped_child_state();
        let text = render_text(&mut state, 90, 30);
        // The activity rows carry a status glyph and a user-language label.
        let row = |needle: &str| {
            text.lines()
                .filter(|l| l.starts_with("  ") && l.contains(needle))
                .find(|l| {
                    let g = l.trim_start().chars().next();
                    matches!(g, Some('✓') | Some('●') | Some('·'))
                })
                .unwrap_or_else(|| panic!("the fixture must list {needle} as a step:\n{text}"))
                .to_string()
        };
        // It was on `grep` when it was stopped: that step never finished, so
        // it must not wear a check.
        assert!(
            !row("搜索").contains('✓'),
            "the step it died on is not a completed one: {:?}",
            row("搜索")
        );
        // It had already moved past `read_file`, so that one did finish —
        // marking it unknown would lose true information.
        assert!(
            row("读取").contains('✓'),
            "a step the child moved past did finish: {:?}",
            row("读取")
        );
    }

    fn stopped_child_state() -> AppState {
        stopped_child_state_hit(None)
    }

    fn stopped_child_state_hit(limit: Option<leveler_client_protocol::ChildLimit>) -> AppState {
        use crate::multi_agent::ChildUpdate;
        let mut state = test_state();
        let update = |done: bool, stop| ChildUpdate {
            id: "c1".into(),
            nickname: "Euclid".into(),
            role: "code-reviewer".into(),
            done,
            ok: false,
            detail: "审查持久化改动".into(),
            title: None,
            profile_id: None,
            agent_name: None,
            read_only: true,
            contribution: None,
            stop,
            limit,
            started_elapsed_secs: 0,
        };
        state.team.apply_update(update(false, None));
        state
            .team
            .apply_activity("c1", "tool_started", "read_file", "{}", false);
        state
            .team
            .apply_activity("c1", "tool_started", "grep", "{}", false);
        state.team.apply_update(update(
            true,
            Some(leveler_client_protocol::ChildStop::Budget),
        ));
        state.activity_open = Some(crate::activity::ActivityId::Child("c1".into()));
        state.active_screen = Screen::Activity;
        state
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
            agent_name: None,
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
            stop: None,
            limit: None,
            interrupted: false,
            unreported: false,
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
            agent_name: None,
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
            stop: None,
            limit: None,
            interrupted: false,
            unreported: false,
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
            agent_name: None,
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
            stop: None,
            limit: None,
            interrupted: false,
            unreported: false,
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

    fn spawn_call_then(state: &mut crate::state::AppState, ok: bool, preview: &str) {
        use crate::action::Action;
        use crate::reducer::reduce;
        use leveler_client_protocol::{RuntimeEvent, ToolCallId};
        reduce(
            state,
            Action::Runtime(RuntimeEvent::ToolCallStarted {
                id: ToolCallId::new("s1"),
                name: "spawn_agent".into(),
                arguments: serde_json::json!({"task": "SPAWNED survey", "role": "explorer"})
                    .to_string(),
                parallel: false,
            }),
        );
        if ok {
            reduce(
                state,
                Action::Runtime(RuntimeEvent::SubAgentUpdated {
                    id: "agent-1".into(),
                    nickname: "Euclid".into(),
                    role: "explorer".into(),
                    title: None,
                    done: false,
                    ok: false,
                    detail: "SPAWNED survey".into(),
                    profile_id: None,
                    profile_role: None,
                    read_only: true,
                    agent: None,
                    contribution: None,
                    outcome: None,
                    stop: None,
                    limit: None,
                    background: Some(true),
                    scope: Vec::new(),
                }),
            );
        }
        reduce(
            state,
            Action::Runtime(RuntimeEvent::ToolCallCompleted {
                exit_code: None,
                stop: None,
                id: ToolCallId::new("s1"),
                ok,
                preview: preview.into(),
                duration_ms: 3,
                applied_diff: None,
            }),
        );
    }

    fn child_event(
        id: &str,
        done: bool,
        ok: bool,
        stop: Option<leveler_client_protocol::ChildStop>,
    ) -> leveler_client_protocol::RuntimeEvent {
        leveler_client_protocol::RuntimeEvent::SubAgentUpdated {
            id: id.into(),
            nickname: "Euclid".into(),
            role: "explorer".into(),
            title: None,
            done,
            ok,
            detail: if done {
                "result".into()
            } else {
                "survey".into()
            },
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

    /// U4: how a child ended is read off its typed terminal, not guessed from
    /// its summary text and not collapsed into one "incomplete".
    #[test]
    fn a_child_terminal_reads_its_typed_stop() {
        use crate::action::Action;
        use crate::reducer::reduce;
        use leveler_client_protocol::ChildStop;
        for (stop, expected) in [
            (ChildStop::Cancelled, "已取消"),
            (ChildStop::Lost, "已丢失"),
            (ChildStop::Budget, "预算耗尽"),
        ] {
            let mut state = test_state();
            state.status = leveler_client_protocol::RuntimeStatus::Busy;
            reduce(
                &mut state,
                Action::Runtime(child_event("agent-1", false, false, None)),
            );
            reduce(
                &mut state,
                Action::Runtime(child_event("agent-1", true, false, Some(stop))),
            );
            let text = render_text(&mut state, 100, 30);
            assert!(text.contains(expected), "{stop:?}: {text}");
        }
    }

    /// U4 + MA3 review L3: a turn that ends with a child still drawn as
    /// running does not rewrite it to failed or interrupted — the UI holds
    /// neither fact. It says what it knows: no terminal reached this view.
    #[test]
    fn a_child_left_running_at_turn_end_reads_unreported_not_failed() {
        use crate::action::Action;
        use crate::reducer::reduce;
        use leveler_client_protocol::RuntimeEvent;
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        reduce(
            &mut state,
            Action::Runtime(child_event("agent-1", false, false, None)),
        );
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::TurnFailed {
                error: "boom".into(),
                failure: None,
            }),
        );
        let text = render_text(&mut state, 100, 30);
        assert!(text.contains("未收到结束状态"), "{text}");
        assert!(
            !text.contains("未完成") && !text.contains("已中断"),
            "{text}"
        );
        assert_eq!(
            state.team.children[0].status,
            crate::multi_agent::ChildStatus::Unreported,
            "the team says the same as the transcript"
        );
    }

    /// The runtime's own lifecycle move reaches the child: interrupted, then
    /// running again under the same id.
    #[test]
    fn a_child_state_change_moves_the_child() {
        use crate::action::Action;
        use crate::reducer::reduce;
        use leveler_client_protocol::{RuntimeEvent, UiChildState};
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        reduce(
            &mut state,
            Action::Runtime(child_event("agent-1", false, false, None)),
        );
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::SubAgentStateChanged {
                id: "agent-1".into(),
                state: UiChildState::Interrupted,
            }),
        );
        assert!(render_text(&mut state, 100, 30).contains("已中断"));
        assert_eq!(
            state.team.children[0].status,
            crate::multi_agent::ChildStatus::Interrupted
        );
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::SubAgentStateChanged {
                id: "agent-1".into(),
                state: UiChildState::Running,
            }),
        );
        assert!(!render_text(&mut state, 100, 30).contains("已中断"));
        assert_eq!(
            state.team.children[0].status,
            crate::multi_agent::ChildStatus::Waiting
        );
    }

    /// U1: an accepted spawn is ONE entry — the child block. The spawn call's
    /// own tool cell said the same thing a second time.
    #[test]
    fn an_accepted_spawn_is_one_entry_not_a_tool_cell_and_a_child() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        spawn_call_then(
            &mut state,
            true,
            "[sub-agent Euclid (agent-1, role=explorer)] started in the background.",
        );
        let text = render_text(&mut state, 100, 30);
        assert!(text.contains("探索 Agent 1"), "{text}");
        assert!(
            !text.contains("子 Agent ·"),
            "the spawn call rendered as its own cell: {text}"
        );
    }

    /// A refused spawn has no child block, so its failure stays visible.
    #[test]
    fn a_refused_spawn_still_shows_its_failure() {
        let mut state = test_state();
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        spawn_call_then(&mut state, false, "Unknown role `explorr`.");
        let text = render_text(&mut state, 100, 30);
        assert!(text.contains("子 Agent"), "{text}");
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
                    title: None,
                    done: false,
                    ok: false,
                    detail: task.into(),
                    profile_id: None,
                    profile_role: None,
                    read_only: false,
                    agent: None,
                    contribution: None,
                    outcome: None,
                    stop: None,
                    limit: None,
                    background: None,
                    scope: Vec::new(),
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
    fn english_locale_covers_recap_and_unsupported_delegation() {
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
                exit_code: None,
                output: String::new(),
                output_truncated: false,
                expanded: false,
                stop: Default::default(),
                id: ToolCallId::new("task-1"),
                name: "task".into(),
                arguments: r#"{"description":"Inspect provider architecture"}"#.into(),
                status: ToolStatus::Failed,
                preview: Some("tool error: unknown tool `task`; use `spawn_agent`".into()),
                duration_ms: None,
                parallel: false,
                batch: None,
                started_elapsed_secs: 0,
                applied_diff: None,
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
                call_id: None,
                always_persists: true,
            }),
        )));
        // A user turn suppresses the splash card so only overlay chrome remains.
        s.transcript.push_user("推送一下".into());
        let text = render_text(&mut s, 80, 30);
        assert!(text.contains("等待审批"), "{text}");
        assert!(text.contains("拒绝"), "{text}");
        assert!(text.contains("git push"), "{text}");
        // The overlay takes the composer's slot: no composer prompt on screen.
        assert!(
            !text.contains('›'),
            "composer must be hidden while an overlay is open: {text}"
        );
    }

    /// A text question puts the terminal cursor in its input; a choice list
    /// does not, because its focus marker is a cell in the list.
    #[test]
    fn clarification_overlay_places_cursor_on_its_input() {
        let s = test_state();
        let overlay = crate::overlay::Overlay::Clarification(Box::new(
            crate::overlay::ClarificationOverlay::new(
                leveler_client_protocol::UiClarificationRequest::single(
                    leveler_client_protocol::ClarificationId::new("c1"),
                    "补充要求？",
                    vec![],
                ),
            ),
        ));
        // The shared overlay content builder feeds both the workbench inline
        // box and the modal path; the cursor must land on the text input.
        let (title, lines, cursor) =
            crate::overlay::content_lines(&overlay, &s.theme, 76, s.locale);
        let joined = lines.iter().map(line_str).collect::<Vec<_>>().join("\n");
        assert!(
            title.contains("补充要求？") || joined.contains("补充要求？"),
            "{title} / {joined}"
        );
        assert!(
            cursor.is_some(),
            "clarification input needs a visible cursor"
        );

        let choice = crate::overlay::Overlay::Clarification(Box::new(
            crate::overlay::ClarificationOverlay::new(
                leveler_client_protocol::UiClarificationRequest::single(
                    leveler_client_protocol::ClarificationId::new("c2"),
                    "选哪个？",
                    vec!["A".into(), "B".into()],
                ),
            ),
        ));
        let (_, lines, cursor) = crate::overlay::content_lines(&choice, &s.theme, 76, s.locale);
        assert!(cursor.is_none(), "a list needs no terminal cursor");
        let rows: Vec<String> = lines.iter().map(line_str).collect();
        assert!(
            rows.iter().any(|r| r.starts_with('❯')),
            "the list marks focus in a cell instead: {rows:#?}"
        );
        // The explicit waiting-state copy is asserted on the live status line in
        // status_line::tests::clarification_overlay_is_awaiting_user.
    }

    /// The focused option is marked with a cursor, not with a digit typed
    /// into the answer field: the answer is the option the arrows are on.
    #[test]
    fn the_focused_option_carries_the_cursor_marker() {
        let s = test_state();
        let mut ov = crate::overlay::ClarificationOverlay::new(
            leveler_client_protocol::UiClarificationRequest::single(
                leveler_client_protocol::ClarificationId::new("c1"),
                "选哪个？",
                vec!["保留".into(), "替换".into()],
            ),
        );
        // Arrows, not digits: the second option takes the cursor.
        ov.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::empty(),
        ));
        let overlay = crate::overlay::Overlay::Clarification(Box::new(ov));
        let (_, lines, _) = crate::overlay::content_lines(&overlay, &s.theme, 76, s.locale);
        let rows: Vec<String> = lines.iter().map(line_str).collect();
        let marked: Vec<&String> = rows.iter().filter(|r| r.starts_with('❯')).collect();
        assert_eq!(marked.len(), 1, "exactly one row is focused: {rows:#?}");
        assert!(
            marked[0].starts_with("❯ 2. 替换"),
            "the cursor moved to the second numbered row: {rows:#?}"
        );
        assert!(
            rows.iter().any(|r| r.starts_with("  1. 保留")),
            "the first row keeps its number and focus column: {rows:#?}"
        );
        assert!(
            !rows
                .iter()
                .any(|r| r.starts_with("  1. 保留") && r.contains('❯')),
            "focus is not color-only: {rows:#?}"
        );
    }

    #[test]
    fn an_answered_single_choice_marks_the_option_it_recorded() {
        let s = test_state();
        let mut ov = crate::overlay::ClarificationOverlay::new(
            leveler_client_protocol::UiClarificationRequest::single(
                leveler_client_protocol::ClarificationId::new("c1"),
                "选哪个？",
                vec!["保留".into(), "替换".into()],
            ),
        );
        ov.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::empty(),
        ));
        ov.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::empty(),
        ));
        let overlay = crate::overlay::Overlay::Clarification(Box::new(ov));
        let (_, lines, _) = crate::overlay::content_lines(&overlay, &s.theme, 76, s.locale);
        let rows: Vec<String> = lines.iter().map(line_str).collect();
        let chosen: Vec<&String> = rows.iter().filter(|r| r.contains('✓')).collect();
        assert_eq!(chosen.len(), 1, "one recorded pick is marked: {rows:#?}");
        assert!(chosen[0].contains("替换"), "{rows:#?}");
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
                        exit_code: None,
                        stop: None,
                        id,
                        ok: true,
                        preview: "ok".into(),
                        duration_ms: 20,
                        applied_diff: None,
                    },
                ),
            );
        }
        // A completion footer is a claim about an answer, so the turn has to
        // have committed one (§12) before it may show.
        for event in [
            leveler_client_protocol::RuntimeEvent::AssistantMessageStarted {
                message_id: leveler_client_protocol::MessageId::new("m-final"),
            },
            leveler_client_protocol::RuntimeEvent::AssistantTextDelta {
                message_id: leveler_client_protocol::MessageId::new("m-final"),
                delta: "两个文件都读过了。".into(),
            },
            leveler_client_protocol::RuntimeEvent::AssistantMessageCompleted {
                message_id: leveler_client_protocol::MessageId::new("m-final"),
            },
            leveler_client_protocol::RuntimeEvent::TurnCompleted,
        ] {
            crate::reducer::reduce(&mut s, crate::action::Action::Runtime(event));
        }

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
                failure: None,
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
    fn a_structured_failure_hides_the_raw_detail_until_disclosed() {
        use leveler_client_protocol::{
            FailureCategory, FailureDelivery, FailureRetryability, FailureSource, UiFailure,
        };
        let mut s = test_state();
        s.status = leveler_client_protocol::RuntimeStatus::Busy;
        let raw = r#"{"error":{"message":"tools.function.parameters is not a valid moonshot flavored json schema"}}"#;
        crate::reducer::reduce(
            &mut s,
            crate::action::Action::Runtime(leveler_client_protocol::RuntimeEvent::TurnFailed {
                error: format!("execution error: model error [InvalidRequest]: {raw}"),
                failure: Some(UiFailure {
                    category: FailureCategory::InvalidRequest,
                    source: FailureSource::Provider,
                    provider: Some("moonshot".into()),
                    model: None,
                    provider_code: None,
                    request_id: None,
                    status: Some(400),
                    retries: None,
                    retryability: FailureRetryability::Never,
                    delivery: FailureDelivery::Responded,
                    summary: "模型服务拒绝了当前请求。".into(),
                    detail: raw.into(),
                }),
            }),
        );
        // ONE terminal failure → ONE primary block.
        let failures = s
            .transcript
            .items()
            .iter()
            .filter(|i| matches!(i, crate::transcript::TranscriptItem::Failure(_)))
            .count();
        assert_eq!(failures, 1, "one failure must be presented once");
        let joined: String = crate::conversation::build::build_conversation_lines(&s, 120)
            .iter()
            .map(line_str)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("模型服务拒绝了当前请求。"), "{joined}");
        assert!(joined.contains("moonshot · invalid_request"), "{joined}");
        assert!(
            !joined.contains("flavored json schema"),
            "the raw detail must not leak into the default view:\n{joined}"
        );
        // Disclosure reveals the raw technical detail.
        let _ = s.transcript.toggle_last_collapsible();
        let expanded: String = crate::conversation::build::build_conversation_lines(&s, 120)
            .iter()
            .map(line_str)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            expanded.contains("flavored json schema"),
            "the raw detail must be readable once disclosed:\n{expanded}"
        );
    }

    /// The disclosure names every machine fact the runtime proved, so a user
    /// report carries the provider, model, status, vendor code and request id
    /// without anyone parsing a payload.
    #[test]
    fn a_failure_disclosure_names_the_provider_facts() {
        use leveler_client_protocol::{
            FailureCategory, FailureDelivery, FailureRetryability, FailureSource, UiFailure,
        };
        let mut s = test_state();
        s.status = leveler_client_protocol::RuntimeStatus::Busy;
        crate::reducer::reduce(
            &mut s,
            crate::action::Action::Runtime(leveler_client_protocol::RuntimeEvent::TurnFailed {
                error: "legacy".into(),
                failure: Some(UiFailure {
                    category: FailureCategory::InvalidRequest,
                    source: FailureSource::Provider,
                    provider: Some("kimi".into()),
                    model: Some("k3".into()),
                    provider_code: Some("invalid_request".into()),
                    request_id: Some("req_live_1".into()),
                    status: Some(400),
                    retries: None,
                    retryability: FailureRetryability::Never,
                    delivery: FailureDelivery::Responded,
                    summary: "模型服务拒绝了当前请求。".into(),
                    detail: "tools.function.parameters is not a valid json schema".into(),
                }),
            }),
        );
        let default: String = crate::conversation::build::build_conversation_lines(&s, 120)
            .iter()
            .map(line_str)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            default.contains("kimi · invalid_request · HTTP 400"),
            "the status must ride the always-visible subtitle:\n{default}"
        );
        assert!(
            !default.contains("not a valid json schema"),
            "the reason stays behind the disclosure:\n{default}"
        );

        let _ = s.transcript.toggle_last_collapsible();
        let expanded: String = crate::conversation::build::build_conversation_lines(&s, 120)
            .iter()
            .map(line_str)
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "kimi",
            "k3",
            "invalid_request",
            "400",
            "req_live_1",
            "not a valid json schema",
        ] {
            assert!(
                expanded.contains(needle),
                "missing {needle} in:\n{expanded}"
            );
        }
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
            None,
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
            .complete_tool(&second, false, "grep failed loudly".into(), 20, None);
        // Close the group: only a CLOSED settled batch is history.
        s.transcript.begin_assistant(MessageId::new("m-done"));

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

        // Opened, both calls and the error come back. (The assistant item that
        // closed the group sits after it, so find the group by type.)
        for item in s.transcript.items_mut() {
            if let TranscriptItem::ToolGroup(group) = item {
                group.expanded = true;
            }
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

        // Expand the (only) group via its own flag — not a global blast.
        for item in s.transcript.items_mut() {
            if let TranscriptItem::ToolGroup(group) = item {
                group.expanded = true;
            }
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
        assert!(
            text.contains("修复运行链"),
            "the running step stays on the panel: {text}"
        );
        assert!(
            !text.contains("读取约束与现状"),
            "a settled step no longer claims a row: {text}"
        );
    }

    #[test]
    fn a_long_plan_hides_finished_steps_and_shows_the_open_ones() {
        // The panel is a summary, not a full checklist: finished steps drop
        // out, so an eight-step plan with two open steps shows those two
        // instead of one row per completed step.
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
        for index in 7..=8 {
            assert!(text.contains(&format!("计划步骤{index}")), "{text}");
        }
        for index in 1..=6 {
            assert!(!text.contains(&format!("计划步骤{index}")), "{text}");
        }
        assert!(
            !text.contains('⋯'),
            "every open step fits, so no count row: {text}"
        );
    }

    #[test]
    fn a_long_plan_in_a_short_terminal_keeps_the_running_step_and_says_what_is_hidden() {
        let mut s = test_state();
        s.status = leveler_client_protocol::RuntimeStatus::Busy;
        s.plan = Some(leveler_client_protocol::UiPlan {
            steps: (0..20)
                .map(|index| leveler_client_protocol::UiPlanStep {
                    index,
                    description: format!("计划步骤{:02}", index + 1),
                    status: match index {
                        0..=5 => leveler_client_protocol::PlanStepStatus::Done,
                        6 => leveler_client_protocol::PlanStepStatus::Running,
                        _ => leveler_client_protocol::PlanStepStatus::Pending,
                    },
                })
                .collect(),
        });

        let text = render_text(&mut s, 100, 22);
        assert!(
            text.contains("计划步骤07"),
            "the running step survives: {text}"
        );
        assert!(
            text.contains('⋯'),
            "the hidden open steps must name their count: {text}"
        );
        for index in 1..=6 {
            assert!(
                !text.contains(&format!("计划步骤{index:02}")),
                "a finished step stays hidden even when the panel is squeezed: {text}"
            );
        }
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
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            expanded: false,
            stop: Default::default(),
            id: ToolCallId::new("t"),
            name: name.into(),
            arguments: args.into(),
            status: ToolStatus::Ok,
            preview: Some("some output".into()),
            duration_ms: Some(ms),
            parallel: false,
            batch: None,
            started_elapsed_secs: 0,
            applied_diff: None,
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
            transcript.complete_tool(&id, true, "ok".into(), 1, None);
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

    /// §6: an applied diff is the change itself, so the disclosure has no say
    /// over it. This test used to assert the opposite — that a 40-line patch
    /// showed 16 rows and a "click for the full diff" hint — which left the
    /// only record of what the agent changed one interaction out of reach.
    #[test]
    fn a_long_inline_diff_is_complete_whether_folded_or_expanded() {
        let body: String = (0..40).map(|i| format!("+line {i}\n")).collect();
        let patch = format!("*** Begin Patch\n*** Update File: src/a.rs\n@@\n{body}*** End Patch");
        let args = serde_json::json!({ "patch": patch }).to_string();
        let item = tool_item("apply_patch", &args, 5);

        for expanded in [false, true] {
            let text = tool_render(&item, expanded)
                .iter()
                .map(line_str)
                .collect::<Vec<_>>()
                .join("\n");
            for i in [0usize, 15, 30, 39] {
                assert!(
                    text.contains(&format!("line {i}")),
                    "expanded={expanded}: {text}"
                );
            }
            assert!(
                !text.contains("完整 Diff"),
                "a complete diff offers nothing to reveal: {text}"
            );
        }
    }

    #[test]
    fn failed_tool_output_is_available_in_expanded_details() {
        let item = ToolCallBlock {
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            expanded: false,
            stop: Default::default(),
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
            batch: None,
            started_elapsed_secs: 0,
            applied_diff: None,
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
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            expanded: false,
            stop: Default::default(),
            id: ToolCallId::new("t1"),
            name: "read_file".into(),
            arguments: r#"{"path":"README.md"}"#.into(),
            status: ToolStatus::Ok,
            preview: Some("     1\t# README\n     2\tlots of content".into()),
            duration_ms: Some(1),
            parallel: false,
            batch: None,
            started_elapsed_secs: 0,
            applied_diff: None,
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
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            expanded: false,
            stop: Default::default(),
            id: ToolCallId::new("g1"),
            name: "update_goal".into(),
            arguments: r#"{"status":"complete","summary":"完成了对示例 CLI 项目的安装验证"}"#
                .into(),
            status: ToolStatus::Ok,
            preview: Some("Goal resolved.".into()),
            duration_ms: Some(1),
            parallel: false,
            batch: None,
            started_elapsed_secs: 0,
            applied_diff: None,
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
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            expanded: false,
            stop: Default::default(),
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
            batch: None,
            started_elapsed_secs: 0,
            applied_diff: None,
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
    fn btw_surface_renders_markdown_bold_not_raw_asterisks() {
        let mut state = test_state();
        state.surface = crate::btw::SurfaceFocus::Btw;
        state.btw.begin("还没有完事吗？".into());
        state
            .btw
            .append("审查完成。**没有发现明显问题**。\n\n- 编译通过\n- 测试通过");
        state.btw.finish(crate::btw::BtwTurnState::Done, None);
        let text = render_text(&mut state, 80, 24);
        assert!(text.contains("没有发现明显问题"), "{text}");
        assert!(!text.contains("**"), "raw markdown markers: {text}");
        assert!(text.contains("编译通过"), "list content: {text}");
        assert!(text.contains("返回主线程"), "side-thread header: {text}");
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
