use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use leveler_client_protocol::UiCompletionReport;

use crate::fold::DisplayMode;
use crate::i18n::{Locale, UiText};
use crate::theme::Theme;
use crate::transcript::{
    AssistantBlock, AssistantKind, ThoughtBlock, ToolStatus, TranscriptItem, TurnEndBlock,
    TurnEndStatus,
};

use super::text::wrap;

/// Render a (possibly streaming) assistant message to bulleted lines, plus the
/// count of leading lines that belong to fully-received Markdown blocks and are
/// therefore safe to commit to scrollback progressively. When the message is
/// done, every line is stable. The live tail (last block + streaming "▌" cursor)
/// is everything at or after the returned index.
pub fn assistant_split(
    block: &AssistantBlock,
    theme: &Theme,
    wrap_width: usize,
) -> (Vec<Line<'static>>, usize) {
    let (lines, stable) = assistant_body(block, theme, wrap_width);
    // Bulleting maps lines 1:1, so the stable boundary is preserved.
    // A final answer is the turn's one primary statement: the accent marker
    // draws the eye to it. Interim narration is process prose and takes a
    // quieter marker, so a long turn's running commentary never competes with
    // the answer for attention. The distinction is structural — nothing here
    // reads the message text.
    let (marker, marker_style) = match block.kind {
        AssistantKind::Final => ("◆", Style::default().fg(theme.accent.primary)),
        AssistantKind::Progress | AssistantKind::Pending => {
            ("●", Style::default().fg(theme.text.secondary))
        }
    };
    let bulleted = bulleted(lines, marker, marker_style);
    (bulleted, stable)
}

/// The message's own wrapped rows, before the bullet gutter: every VISUAL line
/// the text occupies at this width, plus the live "▌" cursor while streaming
/// (which is one of those rows, and so counts against the bound). This is the
/// unit the presentation bound is measured in — never Markdown blocks, never
/// paragraphs, never `\n` counts.
fn assistant_body(
    block: &AssistantBlock,
    theme: &Theme,
    wrap_width: usize,
) -> (Vec<Line<'static>>, usize) {
    // Content is indented two columns under its Assistant marker.
    let inner = wrap_width.saturating_sub(2).max(1);
    // Use the cached parse when done, else parse the partial text this frame so
    // formatting appears as it streams (spec §62).
    let parsed;
    let doc: &crate::markdown::MdDoc = match &block.rendered {
        Some(doc) => doc,
        None => {
            parsed = crate::markdown::MdDoc::parse(&block.text);
            &parsed
        }
    };
    let tone = match block.kind {
        AssistantKind::Final => crate::markdown::AssistantTone::Final,
        AssistantKind::Progress | AssistantKind::Pending => {
            crate::markdown::AssistantTone::Progress
        }
    };
    let (mut lines, mut stable) = doc.to_lines_split_with_tone(inner, theme, tone);
    if block.done {
        // A finished message is fully stable.
        stable = lines.len();
    } else {
        // Streaming cursor is part of the live tail.
        lines.push(Line::from(Span::styled(
            "▌",
            Style::default().fg(theme.text.secondary),
        )));
    }
    (lines, stable)
}

/// Render an assistant message for the conversation.
///
/// Assistant prose is communication, not evidence: interim narration and the
/// final answer both render in full. Only tool output, diffs and other
/// execution detail may sit behind a disclosure. Kind selects the ink, never
/// the amount of text: `Progress` is subordinate to `Final` in tone and marker,
/// and neither is ever truncated.
pub fn assistant_render(
    block: &AssistantBlock,
    theme: &Theme,
    wrap_width: usize,
) -> Vec<Line<'static>> {
    assistant_split(block, theme, wrap_width).0
}

/// A duration for a Thought header, e.g. `2.8s`, `48.0s`, `1m 5s`. A segment
/// too short to round to a tenth reads `<0.1s`: `0.0s` looked like no time at
/// all, which is a claim the measurement does not make.
fn format_thought_duration(ms: u64) -> String {
    if ms < 100 {
        return "<0.1s".to_string();
    }
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let total = ms / 1000;
        format!("{}m {}s", total / 60, total % 60)
    }
}

/// Render one reasoning segment as a Thought.
///
/// The header states the segment's real state — `Thinking…` while streaming,
/// `Thought for 2.8s` when the runtime reported a clean boundary, and
/// `Thought interrupted after 8.2s` when it did not.
///
/// The body follows the entry's own [`DisplayMode`]: `Collapsed` paints the
/// header alone (a finished segment's default), `Truncated` paints a bounded
/// tail (a live segment's preview, and the floor a manual collapse lands on
/// while running), and `Expanded` paints all of it. The body is the provider's
/// own reasoning text under a `│` gutter; the gutter never extends past the
/// body, so the Thought can never look like it owns the entry after it.
/// A provider that returned no text gets a header and no invented prose.
/// Reasoning is not markdown, so the body is wrapped verbatim rather than
/// parsed.
pub fn thought_lines(
    block: &ThoughtBlock,
    theme: &Theme,
    wrap_width: usize,
    t: &crate::i18n::UiText,
) -> Vec<Line<'static>> {
    let header_style = Style::default()
        .fg(theme.text.secondary)
        .add_modifier(Modifier::ITALIC);
    let header = if !block.done {
        t.thought_live.to_string()
    } else if block.interrupted {
        match block.duration_ms {
            Some(ms) => t
                .thought_interrupted
                .replace("{}", &format_thought_duration(ms)),
            None => t.thought_interrupted_bare.to_string(),
        }
    } else {
        t.thought_for.replace(
            "{}",
            &format_thought_duration(block.duration_ms.unwrap_or(0)),
        )
    };
    let mut out = vec![Line::from(vec![
        Span::styled("◆ ", Style::default().fg(theme.text.secondary)),
        Span::styled(header, header_style),
    ])];
    let inner = wrap_width.saturating_sub(2).max(1);
    let body_style = Style::default().fg(theme.text.muted);
    let rail = |text: String| {
        Line::from(vec![
            Span::styled("│ ", body_style),
            Span::styled(text, body_style),
        ])
    };
    // A provider that returned no reasoning gets a header and nothing else.
    if !block.display.is_collapsed() && !block.text.is_empty() {
        let mut body: Vec<Line<'static>> = wrap(&block.text, inner).into_iter().map(rail).collect();
        if block.display != DisplayMode::Expanded {
            crate::fold::truncate_to_tail(&mut body, rail("…".to_string()));
        }
        out.extend(body);
    }
    if !block.done {
        out.push(Line::from(Span::styled(
            "▌",
            Style::default().fg(theme.text.secondary),
        )));
    }
    out
}

/// Render one transcript item to styled lines (no leading separator).
pub fn item_render(
    item: &TranscriptItem,
    theme: &Theme,
    wrap_width: usize,
    tools_expanded: bool,
    t: &crate::i18n::UiText,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    match item {
        TranscriptItem::User(text) => {
            // A continuous heading left-bar + bold text marks a user turn clearly
            // apart from the assistant's "●" bullet + normal-weight prose.
            let bar = Style::default()
                .fg(theme.text.primary)
                .add_modifier(Modifier::BOLD);
            let body = Style::default()
                .fg(theme.text.primary)
                .add_modifier(Modifier::BOLD);
            let inner = wrap_width.saturating_sub(2).max(1);
            let mut wrapped = wrap(text, inner);
            if wrapped.is_empty() {
                wrapped.push(String::new());
            }
            for line in wrapped {
                out.push(Line::from(vec![
                    Span::styled("▌ ", bar),
                    Span::styled(line, body),
                ]));
            }
        }
        TranscriptItem::Assistant(block) => {
            out.extend(assistant_render(block, theme, wrap_width));
        }
        TranscriptItem::Thought(block) => {
            out.extend(thought_lines(block, theme, wrap_width, t));
        }
        TranscriptItem::ToolGroup(group) => {
            // Same product surface as workbench Conversation: Silent tools
            // (update_goal success, ls probes, …) stay out; exploration
            // aggregates; Important edits/runs are one line each.
            // `tools_expanded` is legacy — expand is per-group via Ctrl+O.
            let _ = tools_expanded;
            let locale = locale_from_ui_text(t);
            // Scrollback: finished tools show their final duration, not live (0).
            out.extend(crate::activity_stream::render_activity(
                group,
                theme,
                wrap_width,
                locale,
                t,
                0,
                None,
                None,
                crate::activity_stream::DIFF_PREVIEW_ROWS,
                &mut Vec::new(),
            ));
        }
        // Scrollback: the agent is finished, so no live elapsed is shown (0).
        TranscriptItem::SubAgent(block) => {
            sub_agent_lines(block, theme, wrap_width, &mut out, t, 0)
        }
        TranscriptItem::UserShell(shell) => {
            out.extend(user_shell_lines(shell, theme, wrap_width, t, 0));
        }
        TranscriptItem::Plan(plan) => historical_plan_lines(plan, theme, wrap_width, &mut out, t),
        TranscriptItem::Completion(report) => completion_lines(report, theme, &mut out, t),
        TranscriptItem::Error(text) => {
            push_prefixed(
                &mut out,
                "✗ ",
                text,
                Style::default().fg(theme.status.error),
                wrap_width,
            );
        }
        TranscriptItem::Failure(block) => failure_lines(block, theme, wrap_width, &mut out, t),
        TranscriptItem::Note(text) => {
            push_prefixed(
                &mut out,
                "◆ ",
                text,
                Style::default().fg(theme.text.secondary),
                wrap_width,
            );
        }
        TranscriptItem::MemoryList(block) => {
            let marker = if block.expanded { "▾ " } else { "▸ " };
            push_prefixed(
                &mut out,
                marker,
                &block.summary,
                Style::default().fg(theme.text.secondary),
                wrap_width,
            );
            if block.expanded {
                push_prefixed(
                    &mut out,
                    "  ",
                    &block.details,
                    Style::default().fg(theme.text.secondary),
                    wrap_width,
                );
            }
        }
        TranscriptItem::AwaySummary(text) => {
            push_prefixed(
                &mut out,
                &format!("※ {}: ", t.recap_label),
                text,
                Style::default().fg(theme.text.secondary),
                wrap_width,
            );
        }
        TranscriptItem::TurnEnd(block) => turn_end_lines(block, theme, wrap_width, &mut out, t),
        TranscriptItem::Recap(block) => {
            let text = match (&block.summary, &block.next_step) {
                (Some(summary), Some(next_step)) => {
                    format!("{summary} · {}{next_step}", t.recap_next_step)
                }
                (Some(summary), None) => summary.clone(),
                (None, Some(next_step)) => format!("{}{next_step}", t.recap_next_step),
                (None, None) => return out,
            };
            push_prefixed(
                &mut out,
                &format!("※ {}: ", t.recap_label),
                &text,
                Style::default().fg(theme.text.secondary),
                wrap_width,
            );
        }
        TranscriptItem::GoalRecap(block) => goal_recap_lines(block, theme, wrap_width, &mut out, t),
    }
    out
}

fn historical_plan_lines(
    plan: &leveler_client_protocol::UiPlan,
    theme: &Theme,
    width: usize,
    out: &mut Vec<Line<'static>>,
    t: &crate::i18n::UiText,
) {
    let (done, total) = crate::workbench::plan_done_total(plan);
    let summary = t
        .plan_last_recorded
        .replace("{done}", &done.to_string())
        .replace("{total}", &total.to_string());
    out.push(Line::from(Span::styled(
        format!("{} · {summary}", t.active_plan),
        Style::default()
            .fg(theme.text.secondary)
            .add_modifier(Modifier::BOLD),
    )));
    let body_width = width.saturating_sub(4).max(1);
    for step in &plan.steps {
        let color = match step.status {
            leveler_client_protocol::PlanStepStatus::Done => theme.status.success,
            leveler_client_protocol::PlanStepStatus::Failed => theme.status.error,
            _ => theme.text.secondary,
        };
        let text = format!("{}. {}", step.index + 1, step.description);
        let mut wrapped = wrap(&text, body_width);
        if wrapped.is_empty() {
            wrapped.push(String::new());
        }
        for (line_index, line) in wrapped.into_iter().enumerate() {
            let prefix = if line_index == 0 {
                format!("  {} ", crate::plan_cell::plan_glyph(step.status))
            } else {
                "    ".to_string()
            };
            out.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(color)),
                Span::styled(line, Style::default().fg(theme.text.secondary)),
            ]));
        }
    }
}

/// Durable goal recap (`✽ 阶段回顾`): compact 1–2 lines by default; expanded,
/// structured sections rendered from the persisted checkpoint fields — never
/// parsed back out of the display summary. Unknown facts stay explicit:
/// unmeasured verification never renders as a pass, unknown findings never
/// render as zero.
fn goal_recap_lines(
    block: &crate::transcript::GoalRecapBlock,
    theme: &Theme,
    wrap_width: usize,
    out: &mut Vec<Line<'static>>,
    t: &crate::i18n::UiText,
) {
    let recap = &block.recap;
    let head = Style::default().fg(theme.text.primary);
    let muted = Style::default().fg(theme.text.secondary);
    let marker = if block.expanded { "▾ " } else { "✽ " };
    push_prefixed(
        out,
        marker,
        &format!("{} · {}", t.goal_recap_label, recap.display_summary),
        head,
        wrap_width,
    );
    if !block.expanded {
        if let Some(next) = &recap.next_action {
            push_prefixed(
                out,
                "  ",
                &format!("{}{next}", t.recap_next_step),
                muted,
                wrap_width,
            );
        }
        return;
    }

    let section = |title: &str, body: &str, out: &mut Vec<Line<'static>>| {
        out.push(Line::from(Span::styled(
            format!("  {title}"),
            muted.add_modifier(Modifier::BOLD),
        )));
        push_prefixed(out, "  ", body, head, wrap_width);
    };
    if let Some(phase) = &recap.phase {
        section(t.goal_recap_phase, phase, out);
    }
    if !recap.completed_milestones.is_empty() {
        out.push(Line::from(Span::styled(
            format!("  {}", t.goal_recap_completed),
            muted.add_modifier(Modifier::BOLD),
        )));
        for item in &recap.completed_milestones {
            push_prefixed(out, "  ✓ ", item, head, wrap_width);
        }
    }
    if let (Some(done), Some(total)) = (recap.plan_completed, recap.plan_total) {
        section(t.goal_recap_plan, &format!("{done}/{total}"), out);
    }
    let findings = match recap.findings_total {
        Some(total) => format!("{total}"),
        // UNKNOWN is a statement, not a zero.
        None => t.goal_recap_findings_unknown.to_string(),
    };
    section(t.goal_recap_findings, &findings, out);
    if !recap.known_limitations.is_empty() {
        out.push(Line::from(Span::styled(
            format!("  {}", t.goal_recap_limitations),
            muted.add_modifier(Modifier::BOLD),
        )));
        for item in &recap.known_limitations {
            push_prefixed(out, "  • ", item, head, wrap_width);
        }
    }
    if !recap.unresolved_work.is_empty() {
        out.push(Line::from(Span::styled(
            format!("  {}", t.goal_recap_unresolved),
            muted.add_modifier(Modifier::BOLD),
        )));
        for item in &recap.unresolved_work {
            push_prefixed(out, "  • ", item, head, wrap_width);
        }
    }
    if let Some(next) = &recap.next_action {
        section(
            t.recap_next_step.trim_end_matches([':', '：', ' ']),
            next,
            out,
        );
    }
}

/// The single terminal-failure block. The primary lines carry product copy and
/// a muted machine subtitle; the vendor's raw detail is shown only when
/// disclosed, so a default view never leaks a payload.
fn failure_lines(
    block: &crate::transcript::FailureBlock,
    theme: &Theme,
    width: usize,
    out: &mut Vec<Line<'static>>,
    t: &crate::i18n::UiText,
) {
    out.push(Line::from(vec![
        Span::styled("✗ ", Style::default().fg(theme.status.error)),
        Span::styled(
            block.title.clone(),
            Style::default()
                .fg(theme.status.error)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if !block.summary.trim().is_empty() {
        for line in wrap(&block.summary, width.saturating_sub(2).max(8)) {
            out.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(theme.text.primary),
            )));
        }
    }
    if let Some(subtitle) = &block.subtitle
        && !subtitle.trim().is_empty()
    {
        out.push(Line::from(Span::styled(
            format!("  {subtitle}"),
            Style::default().fg(theme.text.secondary),
        )));
    }
    if !block.detail.trim().is_empty() {
        if block.expanded {
            out.push(Line::from(Span::styled(
                format!("  {}", t.failure_detail_label),
                Style::default().fg(theme.text.secondary),
            )));
            for line in wrap(&block.detail, width.saturating_sub(4).max(8)) {
                out.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(theme.text.secondary),
                )));
            }
        } else {
            out.push(Line::from(Span::styled(
                format!("  {}", t.failure_detail_hint),
                Style::default().fg(theme.text.secondary),
            )));
        }
    }
}

/// The turn-end marker's label and colour.
///
/// Single source for the wording: the transcript divider and a Secondary
/// Surface header both read it, so the verdict cannot diverge between them.
pub(crate) fn turn_end_marker(
    block: &TurnEndBlock,
    theme: &Theme,
    t: &crate::i18n::UiText,
) -> (String, ratatui::style::Color) {
    match block.status {
        TurnEndStatus::Completed => (format!("✓ {}", t.turn_end_completed), theme.status.success),
        // A clean turn that ended with an answer, not with a completion
        // declaration. Saying "任务已完成" here would assert a goal terminal the
        // runtime never received — the model's own text may still say the work
        // is unfinished.
        TurnEndStatus::Answered => (format!("✓ {}", t.turn_end_answered), theme.status.success),
        TurnEndStatus::CompletedWithWarnings | TurnEndStatus::Truncated => (
            format!("⚠ {}", t.final_completed_warnings),
            theme.status.warning,
        ),
        TurnEndStatus::Incomplete => (format!("⚠ {}", t.final_blocked), theme.status.warning),
        // Not a failure — the run may have done real work — but not a
        // completion either. The wording names exactly what is missing.
        TurnEndStatus::NoFinalAnswer => (
            format!("⚠ {}", t.turn_no_final_answer),
            theme.status.warning,
        ),
        TurnEndStatus::Failed => (format!("✗ {}", t.final_failed), theme.status.error),
        // Cancelled is user-initiated, not a failure: stopped glyph, muted.
        TurnEndStatus::Cancelled => (format!("⊘ {}", t.final_cancelled), theme.text.secondary),
    }
}

fn turn_end_lines(
    block: &TurnEndBlock,
    theme: &Theme,
    width: usize,
    out: &mut Vec<Line<'static>>,
    t: &crate::i18n::UiText,
) {
    // The marker reads as `symbol + status word` (colored by outcome) followed
    // by muted stats.
    let (label, color) = turn_end_marker(block, theme, t);
    let mut stats = String::new();
    if matches!(
        block.status,
        TurnEndStatus::Completed
            | TurnEndStatus::CompletedWithWarnings
            | TurnEndStatus::Answered
            | TurnEndStatus::NoFinalAnswer
    ) {
        if block.tool_calls > 0 {
            stats.push_str(
                &t.tool_calls_n
                    .replacen("{}", &block.tool_calls.to_string(), 1),
            );
        }
        if block.elapsed_secs > 0 {
            stats.push_str(&format!(
                " · {}",
                crate::status_line::fmt_elapsed(block.elapsed_secs)
            ));
        }
        if let Some(summary) = &block.summary {
            stats.push_str(&format!(" · {summary}"));
        }
    }
    // Incomplete reasons stay on the marker only while they fit there.
    // The old rule compared the reason against the full width and so appended
    // a medium one to a line already carrying the tool count and elapsed — the
    // border then clipped it mid-word, with nothing under it to read.
    let lead = "── ";
    let mut detail_below = false;
    if let Some(detail) = &block.detail {
        let d = localized_turn_detail(detail, t);
        if !d.is_empty() {
            // ` · ` plus the reason, and the marker still needs a rule after it.
            let room = width
                .saturating_sub(UnicodeWidthStr::width(lead))
                .saturating_sub(UnicodeWidthStr::width(label.as_str()))
                .saturating_sub(UnicodeWidthStr::width(stats.as_str()))
                .saturating_sub(3 + 4);
            if UnicodeWidthStr::width(d) <= room {
                stats.push_str(" · ");
                stats.push_str(d);
            } else {
                detail_below = true;
            }
        }
    }
    let used = UnicodeWidthStr::width(lead)
        + UnicodeWidthStr::width(label.as_str())
        + UnicodeWidthStr::width(stats.as_str())
        + 1;
    let tail = "─".repeat(width.saturating_sub(used));
    out.push(Line::from(vec![
        Span::styled(lead, Style::default().fg(theme.border.normal)),
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(stats, Style::default().fg(theme.text.secondary)),
        Span::styled(format!(" {tail}"), Style::default().fg(theme.border.normal)),
    ]));
    // A reason the marker had no room for is readable under it, in full.
    if detail_below && let Some(detail) = &block.detail {
        for line in wrap(
            localized_turn_detail(detail, t),
            width.saturating_sub(4).max(8),
        ) {
            out.push(Line::from(Span::styled(
                format!("   {line}"),
                Style::default().fg(theme.text.secondary),
            )));
        }
    }
}

fn localized_turn_detail<'a>(detail: &'a str, t: &'a crate::i18n::UiText) -> &'a str {
    let d = detail.trim();
    match d {
        // Executor machine tokens + long defaults → short product copy. The
        // "observe thrash" and "continue suppressed" tokens are replay-only:
        // the semantic watchdogs that wrote them are deleted, and a session
        // recorded before that must still render as what it said.
        s if s.contains("observe thrash") && s.contains("plan complete") => t.turn_plan_thrash,
        s if s.contains("observe thrash")
            || s.starts_with("no-progress streak")
            || s.contains("continue suppressed: no-progress cap") =>
        {
            t.turn_observe_thrash
        }
        s if s.contains("预算已耗尽")
            || s.starts_with("轮次或资源预算")
            // The executor emits the machine token `budget_exhausted
            // dimension=… spent=… cap=…` (underscore) — the human-spaced
            // spelling never matched and users saw the raw token.
            || s.contains("budget_exhausted")
            || s.contains("budget exhausted")
            || s.contains("model token budget")
            || s.contains("model cost budget") =>
        {
            t.turn_budget_exhausted
        }
        s if s.contains("update_goal")
            || s.contains("goal 模式未调用")
            || s.contains("goal 未确认") =>
        {
            t.turn_stalled_goal
        }
        // The label already says "验证未通过"; drop the redundant English prefix
        // and keep just the failing gate name(s) as the detail.
        s if s.starts_with("failed gate(s): ") => s.strip_prefix("failed gate(s): ").unwrap_or(s),
        other => other,
    }
}

/// Whether a transcript item is finalized and safe to commit to scrollback
/// (streaming assistants and running tools are not yet final).
pub fn item_is_final(item: &TranscriptItem) -> bool {
    match item {
        TranscriptItem::Assistant(b) => b.done,
        TranscriptItem::Thought(b) => b.done,
        TranscriptItem::ToolGroup(group) => {
            !group.open
                && group
                    .calls
                    .iter()
                    .all(|call| call.status != ToolStatus::Running)
        }
        TranscriptItem::SubAgent(b) => b.status != ToolStatus::Running,
        // Sealed = another item followed. Never a completion claim; it only
        // says this text will not grow, which is all scrollback needs.
        _ => true,
    }
}

/// Infer locale from the static UiText table (En vs Zh).
fn locale_from_ui_text(t: &UiText) -> Locale {
    if std::ptr::eq(t, Locale::En.text()) {
        Locale::En
    } else {
        Locale::Zh
    }
}
pub(crate) fn sub_agent_display_name(
    block: &crate::transcript::SubAgentBlock,
    t: &crate::i18n::UiText,
) -> String {
    let role = match block.role.as_str() {
        "explorer" => t.sub_agent_explorer,
        "worker" => t.sub_agent_worker,
        "reviewer" => t.sub_agent_reviewer,
        _ => t.sub_agent_default,
    };
    match block.id.strip_prefix("agent-").filter(|n| !n.is_empty()) {
        Some(number) => format!("{role} {number}"),
        None => block.nickname.clone(),
    }
}

pub(crate) fn sub_agent_status(
    block: &crate::transcript::SubAgentBlock,
    t: &crate::i18n::UiText,
) -> &'static str {
    match block.status {
        _ if block.unreported => t.sub_agent_unreported,
        _ if block.interrupted => t.sub_agent_interrupted,
        ToolStatus::Running if block.progress.active => t.sub_agent_running,
        ToolStatus::Running => t.sub_agent_waiting,
        ToolStatus::Ok => t.sub_agent_completed,
        ToolStatus::Failed => {
            crate::multi_agent::stop_label(block.stop, t).unwrap_or(t.sub_agent_incomplete)
        }
        ToolStatus::Cancelled | ToolStatus::Unknown => t.sub_agent_unreported,
    }
}

pub(crate) fn sub_agent_usage(
    block: &crate::transcript::SubAgentBlock,
    t: &crate::i18n::UiText,
) -> String {
    let usage = &block.progress;
    if usage.input_tokens == 0 && usage.output_tokens == 0 {
        return String::new();
    }
    let mut text = format!(
        "↑ {} · ↓ {}",
        crate::status_line::fmt_tokens(usage.input_tokens),
        crate::status_line::fmt_tokens(usage.output_tokens)
    );
    if usage.cached_input_tokens > 0 && usage.input_tokens > 0 {
        let pct = (usage.cached_input_tokens as u64 * 100) / usage.input_tokens as u64;
        text.push_str(&format!(" · {} {pct}%", t.sub_agent_cached));
    }
    text
}

pub(crate) fn sub_agent_detail(detail: &str, t: &crate::i18n::UiText) -> String {
    const ROUND_PREFIX: &str = "Reached the ";
    const ROUND_SUFFIX: &str = "-round limit before finishing.";
    let mut displayed = if let Some(rest) = detail.strip_prefix(ROUND_PREFIX) {
        if let Some((rounds, tail)) = rest.split_once(ROUND_SUFFIX) {
            format!("{}{}", t.sub_agent_round_limit.replace("{}", rounds), tail)
        } else {
            detail.to_string()
        }
    } else {
        detail.to_string()
    };
    displayed = displayed.replace("Latest note: ", t.sub_agent_latest_note);
    displayed
}

/// A spawned sub-agent block: a clear role/ordinal + execution state, then the
/// task while running or its result summary once done.
fn sub_agent_lines(
    block: &crate::transcript::SubAgentBlock,
    theme: &Theme,
    wrap_width: usize,
    out: &mut Vec<Line<'static>>,
    t: &crate::i18n::UiText,
    now_elapsed_secs: u64,
) {
    let (glyph, color) = match block.status {
        _ if block.unreported => ("?", theme.status.warning),
        _ if block.interrupted => ("⏸", theme.status.warning),
        ToolStatus::Running => ("◌", theme.accent.primary),
        ToolStatus::Ok => ("✓", theme.status.success),
        ToolStatus::Failed => ("✗", theme.status.error),
        ToolStatus::Cancelled | ToolStatus::Unknown => ("?", theme.status.warning),
    };
    let mut head_spans = vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::styled(
            crate::multi_agent::child_label(
                &sub_agent_display_name(block, t),
                block.agent_name.as_deref(),
            ),
            Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    head_spans.push(Span::styled(
        format!(" · {}", sub_agent_status(block, t)),
        Style::default().fg(theme.text.secondary),
    ));
    // Contribution, not volume. "reviewed, nothing to flag" is a result and
    // gets a line; an unmeasured child says so rather than showing a zero.
    if let Some(line) = crate::multi_agent::contribution_line_for_block(block, t) {
        head_spans.push(Span::styled(
            format!(" · {line}"),
            Style::default().fg(theme.text.secondary),
        ));
    }
    let elapsed = sub_agent_elapsed(block, now_elapsed_secs);
    if !elapsed.is_empty() {
        head_spans.push(Span::styled(
            format!(" · {elapsed}"),
            Style::default().fg(theme.text.secondary),
        ));
    }
    if let Some(step) = block.recent_step.as_deref().filter(|s| !s.is_empty()) {
        head_spans.push(Span::styled(
            format!(" · {step}"),
            Style::default().fg(theme.accent.primary),
        ));
    }
    let usage = sub_agent_usage(block, t);
    if !usage.is_empty() {
        head_spans.push(Span::styled(
            format!(" · {usage}"),
            Style::default().fg(theme.text.secondary),
        ));
    }
    let head = Line::from(head_spans);
    out.push(head);
    let detail_label = if block.status == ToolStatus::Running {
        t.sub_agent_task
    } else {
        t.sub_agent_result
    };
    let displayed_detail = format!("{detail_label}{}", sub_agent_detail(block.detail.trim(), t));
    let detail = displayed_detail.trim();
    if !detail.is_empty() {
        let inner = wrap_width.saturating_sub(2).max(1);
        let rows = wrap(detail, inner);
        // The runtime hands back whatever the agent wrote. Rendering all of it
        // lets one delegated task push the conversation off screen; dropping it
        // silently would be worse. Bound it, mark the cut, and let Ctrl+O read
        // the rest.
        const MAX_ROWS: usize = 4;
        let cut = !block.expanded && rows.len() > MAX_ROWS;
        let shown = if cut { MAX_ROWS } else { rows.len() };
        for line in rows.iter().take(shown) {
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(line.clone(), Style::default().fg(theme.text.secondary)),
            ]));
        }
        if cut {
            out.push(Line::from(Span::styled(
                format!(
                    "  {}",
                    t.fold_more_lines
                        .replace("{}", &(rows.len() - shown).to_string())
                ),
                Style::default().fg(theme.text.muted),
            )));
        }
    }
}

/// Render a run of consecutive sub-agent blocks as one inline tree: an
/// aggregate header (◌ running / ✓ all done / ⚠ ended with failures) plus one
/// durable task summary. A lone agent keeps the classic single-block
/// rendering. Only data the blocks actually carry is shown; details remain on
/// the live roster and its drill-down page.
pub fn sub_agent_tree_lines(
    blocks: &[&crate::transcript::SubAgentBlock],
    theme: &Theme,
    wrap_width: usize,
    t: &crate::i18n::UiText,
    now_elapsed_secs: u64,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    match blocks {
        [] => {}
        [single] => sub_agent_lines(single, theme, wrap_width, &mut out, t, now_elapsed_secs),
        many => sub_agent_tree_group_lines(many, theme, wrap_width, &mut out, t, now_elapsed_secs),
    }
    out
}

/// A running sub-agent's own elapsed time (`now - started`), formatted, or empty
/// when not running / start unknown. Lets a live view show per-agent runtime.
fn sub_agent_elapsed(block: &crate::transcript::SubAgentBlock, now_elapsed_secs: u64) -> String {
    if block.status != ToolStatus::Running {
        return String::new();
    }
    crate::status_line::fmt_elapsed(now_elapsed_secs.saturating_sub(block.started_elapsed_secs))
}

/// The multi-agent tree (two or more consecutive blocks).
fn sub_agent_tree_group_lines(
    blocks: &[&crate::transcript::SubAgentBlock],
    theme: &Theme,
    wrap_width: usize,
    out: &mut Vec<Line<'static>>,
    t: &crate::i18n::UiText,
    now_elapsed_secs: u64,
) {
    let n = blocks.len();
    let any_running = blocks.iter().any(|b| b.status == ToolStatus::Running);
    let all_ok = blocks.iter().all(|b| b.status == ToolStatus::Ok);

    let count = n.to_string();
    let (glyph, color, header) = if any_running {
        (
            "◌",
            theme.accent.primary,
            t.agents_running_header.replacen("{}", &count, 1),
        )
    } else if all_ok {
        (
            "✓",
            theme.status.success,
            t.agents_done_header.replacen("{}", &count, 1),
        )
    } else {
        (
            "⚠",
            theme.status.warning,
            t.agents_ended_header.replacen("{}", &count, 1),
        )
    };

    // Aggregate the token usage the runtime actually reports. The transcript
    // keeps one durable step summary; the live roster owns per-agent state.
    let sum_in = blocks
        .iter()
        .fold(0u64, |acc, b| acc + u64::from(b.progress.input_tokens));
    let sum_out = blocks
        .iter()
        .fold(0u64, |acc, b| acc + u64::from(b.progress.output_tokens));
    let mut stats = String::new();
    if sum_in > 0 || sum_out > 0 {
        let total = sum_in.saturating_add(sum_out);
        stats.push_str(&format!(
            " · {} tokens",
            crate::multi_agent::fmt_tokens_compact(u32::try_from(total).unwrap_or(u32::MAX))
        ));
    }
    let started = blocks
        .iter()
        .map(|block| block.started_elapsed_secs)
        .min()
        .unwrap_or(now_elapsed_secs);
    let settled = blocks
        .iter()
        .filter_map(|block| block.settled_elapsed_secs)
        .max();
    let end = if any_running {
        now_elapsed_secs
    } else {
        settled.unwrap_or(now_elapsed_secs)
    };
    if end >= started && (any_running || settled.is_some()) {
        stats.push_str(&format!(
            " · {}",
            crate::status_line::fmt_elapsed(end.saturating_sub(started))
        ));
    }
    // A finished-but-not-clean run breaks down how each agent ended.
    if !any_running && !all_ok {
        let completed = blocks.iter().filter(|b| b.status == ToolStatus::Ok).count();
        let timeout = blocks
            .iter()
            .filter(|b| b.status == ToolStatus::Failed && sub_agent_timed_out(b, t))
            .count();
        let failed = n - completed - timeout;
        let mut parts: Vec<String> = Vec::new();
        if completed > 0 {
            parts.push(format!("{completed} {}", t.agent_status_completed));
        }
        if timeout > 0 {
            parts.push(format!("{timeout} {}", t.agent_status_timeout));
        }
        if failed > 0 {
            parts.push(format!("{failed} {}", t.sub_agent_incomplete));
        }
        stats.push_str(" · ");
        stats.push_str(&parts.join(" · "));
    }
    out.push(Line::from(vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::styled(
            header,
            Style::default()
                .fg(theme.text.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(stats, Style::default().fg(theme.text.muted)),
    ]));

    let mut tasks: Vec<String> = Vec::new();
    for block in blocks {
        let task = block.task.trim();
        if !task.is_empty() && !tasks.iter().any(|seen| seen == task) {
            tasks.push(task.to_string());
        }
    }
    if !tasks.is_empty() {
        let separator = if std::ptr::eq(t, crate::i18n::Locale::En.text()) {
            " · "
        } else {
            "、"
        };
        for row in wrap(&tasks.join(separator), wrap_width.saturating_sub(2).max(1)) {
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(row, Style::default().fg(theme.text.secondary)),
            ]));
        }
    }
}

/// True when the runtime typed this child's stop as the wall-clock bound, the
/// only budget stop that is a timeout. Reads the same `ChildStop + ChildLimit`
/// mapping the shared label uses, so the transcript cannot drift from it.
fn sub_agent_timed_out(block: &crate::transcript::SubAgentBlock, t: &crate::i18n::UiText) -> bool {
    crate::multi_agent::child_stop_label(block.stop, block.limit, t) == Some(t.agent_status_timeout)
}

/// Status word for one tree child in a non-all-success batch. Running agents
/// keep the waiting/running distinction; finished ones carry a ✓/✗ glyph.
#[cfg(test)]
fn sub_agent_tree_child_status(
    block: &crate::transcript::SubAgentBlock,
    theme: &Theme,
    t: &crate::i18n::UiText,
) -> (String, ratatui::style::Color) {
    match block.status {
        ToolStatus::Running if block.progress.active => {
            (t.agent_status_running.to_string(), theme.accent.primary)
        }
        ToolStatus::Running => (t.sub_agent_waiting.to_string(), theme.text.secondary),
        ToolStatus::Ok => (
            format!("✓ {}", t.agent_status_completed),
            theme.status.success,
        ),
        _ if block.unreported => (
            format!("? {}", t.sub_agent_unreported),
            theme.status.warning,
        ),
        _ if block.interrupted => (
            format!("⏸ {}", t.sub_agent_interrupted),
            theme.status.warning,
        ),
        ToolStatus::Failed => (
            format!(
                "✗ {}",
                crate::multi_agent::child_stop_label(block.stop, block.limit, t)
                    .unwrap_or(t.sub_agent_incomplete)
            ),
            theme.status.error,
        ),
        ToolStatus::Cancelled | ToolStatus::Unknown => (
            format!("? {}", t.sub_agent_unreported),
            theme.status.warning,
        ),
    }
}

/// Prefix a block of lines with a colored bullet on the first line and a
/// two-column hanging indent on the remaining lines.
fn bulleted(lines: Vec<Line<'static>>, bullet: &str, bullet_style: Style) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len().max(1));
    for (i, line) in lines.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 1);
        if i == 0 {
            spans.push(Span::styled(format!("{bullet} "), bullet_style));
        } else {
            spans.push(Span::raw("  "));
        }
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(format!("{bullet} "), bullet_style)));
    }
    out
}

/// Whether a blank separator belongs between two adjacent transcript items.
/// Consecutive tool calls form one visual group — a gap between each would
/// stretch a burst of quick reads across half a screen of whitespace.
pub fn items_need_gap(prev: &TranscriptItem, next: &TranscriptItem) -> bool {
    // Consecutive sub-agents form one visual batch (the workbench renders the
    // whole run as one tree via `sub_agent_tree_lines`; this rule covers the
    // per-item footer path). Tool calls are already a single ToolGroup item.
    let both_agents =
        matches!(prev, TranscriptItem::SubAgent(_)) && matches!(next, TranscriptItem::SubAgent(_));
    !both_agents
}

/// The completion report block (spec §23).
fn completion_lines(
    report: &UiCompletionReport,
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
    t: &crate::i18n::UiText,
) {
    let (glyph, color) = if report.success {
        ("✓", theme.status.success)
    } else {
        ("✗", theme.status.warning)
    };
    out.push(Line::from(Span::styled(
        format!("{glyph} {}", t.turn_end_completed),
        Style::default().fg(color),
    )));
    out.push(Line::from(Span::styled(
        format!(
            "  {}  +{} / -{}",
            t.completion_files_changed
                .replacen("{}", &report.files_changed.to_string(), 1),
            report.added,
            report.removed
        ),
        Style::default().fg(theme.text.secondary),
    )));
    out.push(Line::from(Span::styled(
        format!("  {}", t.completion_diff_hint),
        Style::default().fg(theme.text.secondary),
    )));
}

/// Push `text` wrapped, with `prefix` on the first line and blank alignment on
/// continuations.
fn push_prefixed(
    out: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    style: Style,
    width: usize,
) {
    // Prefixes such as `※ 回顾: ` are wider than their char count. The wrap
    // budget and the continuation indent both have to use that display width,
    // or the first painted row overruns and the terminal clips a wide glyph.
    let prefix_cols = UnicodeWidthStr::width(prefix);
    let indent = " ".repeat(prefix_cols);
    let inner = width.saturating_sub(prefix_cols).max(1);
    let wrapped = wrap(text, inner);
    for (i, line) in wrapped.into_iter().enumerate() {
        let lead = if i == 0 {
            prefix.to_string()
        } else {
            indent.clone()
        };
        out.push(Line::from(vec![
            Span::styled(lead, style),
            Span::styled(line, style),
        ]));
    }
}

/// User shell presentation adapter: one execution block → the shared
/// disclosure language. Clearly marked as USER-initiated ("Run (user)"), so
/// it can never read as an agent tool call; running shows a live `◌` row.
pub(crate) fn user_shell_lines(
    shell: &crate::transcript::UserShellBlock,
    theme: &Theme,
    wrap_width: usize,
    t: &crate::i18n::UiText,
    now_elapsed_secs: u64,
) -> Vec<Line<'static>> {
    use crate::transcript::UserShellStatus;
    let mut out = Vec::new();
    let label = format!("{} {}", t.user_shell_run_label, shell.command);
    if shell.status == UserShellStatus::Running {
        let elapsed = (now_elapsed_secs as i64 - shell.started_elapsed_secs).max(0);
        let suffix = if elapsed > 0 {
            format!(" · {elapsed}s")
        } else {
            String::new()
        };
        out.push(Line::from(vec![
            Span::styled("◌ ".to_string(), Style::default().fg(theme.accent.primary)),
            Span::styled(
                crate::render::truncate_display(&label, wrap_width.saturating_sub(8)),
                Style::default().fg(theme.text.primary),
            ),
            Span::styled(suffix, Style::default().fg(theme.text.muted)),
        ]));
        return out;
    }
    let failed = shell.status == UserShellStatus::Failed;
    let label = match shell.status {
        UserShellStatus::Cancelled => format!("{label} · {}", t.user_shell_cancelled),
        UserShellStatus::Unknown => format!("{label} · {}", t.shell_status_unknown),
        _ => label,
    };
    let presentation = crate::presentation::disclosure::DisclosurePresentation {
        label,
        failed: usize::from(failed),
        failed_suffix: shell
            .exit_code
            .filter(|code| failed && *code != 0)
            .map(|code| format!("exit {code}")),
        needs_permission_suffix: None,
        // A user shell is never a batch: it has no siblings to summarize.
        ok_suffix: None,
        expanded: shell.expanded,
        // A user shell is a direct invocation, never a running stage header.
        running: false,
        // A user shell row opens Shell Details (its own screen) rather than
        // expanding inline, so it carries the drill-down marker, not the
        // inline `▸/▾`.
        drill_down: true,
        duration_ms: shell.duration_ms,
        first_error: (!shell.expanded && failed)
            .then(|| {
                shell
                    .output
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .map(str::to_string)
            })
            .flatten(),
    };
    if !shell.expanded {
        return crate::presentation::disclosure::collapsed_lines(&presentation, theme, wrap_width);
    }
    out.push(crate::presentation::disclosure::header_line(
        &presentation,
        theme,
        wrap_width,
    ));
    if shell.output_truncated {
        out.push(Line::from(Span::styled(
            format!("  {}", t.shell_truncated),
            Style::default().fg(theme.text.muted),
        )));
    }
    for raw in shell.output.lines() {
        let line = crate::render::sanitize_terminal_line(raw);
        out.push(Line::from(Span::styled(
            crate::render::truncate_display(&format!("  {line}"), wrap_width),
            Style::default().fg(theme.text.secondary),
        )));
    }
    out
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::i18n::Locale;
    use crate::theme::Theme;
    use crate::transcript::{AssistantKind, ToolCallBlock, ToolGroupBlock};
    use leveler_client_protocol::ToolCallId;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// A user shell whose stop could not be confirmed reads as unknown —
    /// never failed, never cancelled.
    #[test]
    fn a_user_shell_with_an_unconfirmed_stop_reads_as_unknown() {
        use crate::transcript::{UserShellBlock, UserShellStatus};
        let status = UserShellStatus::from_wire("unknown");
        let shell = UserShellBlock {
            id: leveler_core::UserShellId::new("ush-1"),
            command: "sleep 30".into(),
            cwd: "/repo".into(),
            status,
            output: String::new(),
            output_truncated: false,
            exit_code: None,
            duration_ms: Some(2_000),
            started_elapsed_secs: 0,
            expanded: false,
        };
        let row: String = user_shell_lines(&shell, &Theme::no_color(), 80, Locale::Zh.text(), 0)
            .iter()
            .map(line_text)
            .collect();
        assert!(row.contains("状态未知"), "{row:?}");
        assert!(!row.contains('✗') && !row.contains("已取消"), "{row:?}");
    }

    /// A user shell row opens its own Details screen, so it carries the
    /// drill-down marker `↗`, not the inline `▸/▾` a tool group expands with.
    /// The same glyph must not mean two different things.
    #[test]
    fn a_user_shell_row_advertises_drill_down_not_inline_expand() {
        use crate::transcript::{UserShellBlock, UserShellStatus};
        let shell = UserShellBlock {
            id: leveler_core::UserShellId::new("ush-2"),
            command: "cargo test".into(),
            cwd: "/repo".into(),
            status: UserShellStatus::Success,
            output: "ok".into(),
            output_truncated: false,
            exit_code: Some(0),
            duration_ms: Some(1_000),
            started_elapsed_secs: 0,
            expanded: false,
        };
        let row: String = user_shell_lines(&shell, &Theme::no_color(), 80, Locale::Zh.text(), 0)
            .iter()
            .map(line_text)
            .collect();
        assert!(row.contains('↗'), "{row:?}");
        assert!(!row.contains('▸') && !row.contains('▾'), "{row:?}");
    }

    // ---- Assistant prose is communication: never folded ----

    fn block(text: &str, kind: AssistantKind) -> AssistantBlock {
        AssistantBlock {
            id: leveler_client_protocol::MessageId::new("m1"),
            text: text.to_string(),
            done: true,
            rendered: Some(crate::markdown::MdDoc::parse(text)),
            kind,
        }
    }

    fn rows(block: &AssistantBlock, width: usize) -> Vec<String> {
        assistant_render(block, &Theme::no_color(), width)
            .iter()
            .map(line_text)
            .collect()
    }

    // ---- Reasoning is a Thought: real text, honest state, never invented ----

    fn thought(
        text: &str,
        done: bool,
        duration_ms: Option<u64>,
        interrupted: bool,
    ) -> ThoughtBlock {
        ThoughtBlock {
            text: text.to_string(),
            done,
            duration_ms,
            interrupted,
            live_since: None,
            display: crate::fold::DisplayMode::Expanded,
            display_pinned: true,
        }
    }

    fn thought_rows(block: &ThoughtBlock, width: usize) -> Vec<String> {
        thought_lines(block, &Theme::no_color(), width, Locale::Zh.text())
            .iter()
            .map(line_text)
            .collect()
    }

    /// REASONING-8: a provider that returned no reasoning text gets a header
    /// and NO body — the harness never invents a Thought.
    #[test]
    fn a_thought_without_provider_text_has_no_invented_body() {
        let lines = thought_rows(&thought("", true, Some(1200), false), 60);
        assert!(lines[0].contains("已思考"), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains('│')),
            "no body without provider text: {lines:?}"
        );
    }

    /// A segment too short to round to a tenth says `<0.1s`, not `0.0s`: a
    /// duration that reads as no time at all is a claim the measurement does
    /// not make.
    #[test]
    fn a_sub_tenth_thought_does_not_read_as_zero() {
        let lines = thought_rows(&thought("快", true, Some(40), false), 60);
        assert!(lines[0].contains("<0.1s"), "{lines:?}");
        assert!(!lines[0].contains("0.0s"), "{lines:?}");
    }

    /// A cut Thought whose elapsed could not be measured says so without
    /// inventing a time.
    #[test]
    fn an_unmeasured_interrupted_thought_shows_no_duration() {
        let lines = thought_rows(&thought("半句", true, None, true), 60);
        assert!(lines[0].contains("思考中断"), "{lines:?}");
        assert!(!lines[0].contains('s'), "no invented duration: {lines:?}");
    }

    /// A completed segment states the runtime's own measurement and shows the
    /// provider's words verbatim under the gutter.
    #[test]
    fn a_completed_thought_shows_its_measured_duration() {
        let lines = thought_rows(&thought("先查 catalog。", true, Some(2800), false), 60);
        assert!(lines[0].contains("已思考 2.8s"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("先查 catalog。")),
            "{lines:?}"
        );
    }

    /// A segment that never reached a clean boundary is never labelled as a
    /// finished one.
    #[test]
    fn an_interrupted_thought_is_not_labelled_finished() {
        let lines = thought_rows(&thought("半句话", true, None, true), 60);
        assert!(lines[0].contains("思考中断"), "{lines:?}");
        assert!(
            !lines[0].contains("已思考"),
            "interrupted is not completed: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("半句话")), "{lines:?}");
    }

    /// REASONING-narrow: a meaningful label survives a narrow terminal instead
    /// of degrading to a bare spinner glyph.
    #[test]
    fn a_thought_keeps_a_meaningful_label_when_narrow() {
        let lines = thought_rows(
            &thought("正在检查当前 provider catalog。", false, None, false),
            12,
        );
        assert!(lines[0].contains("思考中"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains('▌')), "{lines:?}");
    }

    /// The two kinds differ in marker GLYPH and in marker INK: a faint `●` for
    /// process narration, the accent `◆` for the answer. A reader scanning a
    /// long turn never has to compare glyph shapes to find the answer.
    #[test]
    fn progress_and_final_use_distinct_markers() {
        let theme = Theme::dark();
        let progress = assistant_render(&block("working", AssistantKind::Progress), &theme, 40);
        let final_answer = assistant_render(&block("done", AssistantKind::Final), &theme, 40);

        assert!(line_text(&progress[0]).starts_with("● "));
        assert!(line_text(&final_answer[0]).starts_with("◆ "));
        assert_eq!(progress[0].spans[0].style.fg, Some(theme.text.secondary));
        assert_eq!(
            final_answer[0].spans[0].style.fg,
            Some(theme.accent.primary)
        );
        assert_ne!(
            progress[0].spans[0].style.fg,
            final_answer[0].spans[0].style.fg
        );
    }

    /// The hierarchy invariant the presentation rests on: for the SAME prose,
    /// every progress row renders in lighter ink than the final answer's body,
    /// so a long turn's narration cannot outweigh the answer.
    #[test]
    fn progress_prose_is_subordinate_to_the_final_answer_body() {
        let theme = Theme::dark();
        // Two paragraphs: the final answer's first is its lead, its second is a
        // body paragraph — both must stay heavier than progress prose.
        let text = "第一段结论。\n\n第二段正文。";
        let progress = assistant_render(&block(text, AssistantKind::Progress), &theme, 80);
        let final_answer = assistant_render(&block(text, AssistantKind::Final), &theme, 80);
        let ink = |lines: &[Line<'static>], needle: &str| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .find(|span| span.content.contains(needle))
                .map(|span| span.style.fg)
                .expect("span")
        };

        assert_eq!(ink(&progress, "第一段"), Some(theme.text.secondary));
        assert_eq!(ink(&progress, "第二段"), Some(theme.text.secondary));
        assert_eq!(ink(&final_answer, "第一段"), Some(theme.text.final_lead));
        assert_eq!(ink(&final_answer, "第二段"), Some(theme.text.primary));
        assert_ne!(ink(&progress, "第一段"), ink(&final_answer, "第一段"));
        assert_ne!(ink(&progress, "第二段"), ink(&final_answer, "第二段"));
    }

    #[test]
    fn final_answer_hierarchy_without_overriding_markdown_roles() {
        let theme = Theme::dark();
        let progress = assistant_render(&block("plain body", AssistantKind::Progress), &theme, 80);
        let final_answer = assistant_render(
            &block(
                "plain body with `cargo test` and [docs](https://example.com)\n\n## Result\n\n- first push succeeded\n- worktree is clean",
                AssistantKind::Final,
            ),
            &theme,
            80,
        );

        let progress_body = progress[0]
            .spans
            .iter()
            .find(|span| span.content.contains("plain"))
            .expect("progress body span");
        let lead = final_answer[0]
            .spans
            .iter()
            .find(|span| span.content.contains("plain"))
            .expect("final lead span");
        let code = final_answer
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("cargo"))
            .expect("inline code span");
        let link = final_answer
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("docs"))
            .expect("link span");
        let heading = final_answer
            .iter()
            .find(|line| line_text(line).contains("Result"))
            .expect("heading line");
        let list_item = final_answer
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "first")
            .expect("list item body span");
        let list_marker = final_answer
            .iter()
            .find(|line| line_text(line).contains("first push"))
            .and_then(|line| line.spans.iter().find(|span| span.content.as_ref() == "• "))
            .expect("list item marker span");

        // Progress prose is process narration: subordinate ink, not the
        // answer's body ink.
        assert_eq!(progress_body.style.fg, Some(theme.text.secondary));
        // The opening paragraph is the lead; the list marker is the scan point;
        // the item text is body.
        assert_eq!(lead.style.fg, Some(theme.text.final_lead));
        assert_eq!(list_marker.style.fg, Some(theme.accent.secondary));
        assert_eq!(list_item.style.fg, Some(theme.text.primary));
        // Local semantics still win.
        assert_eq!(code.style.fg, Some(theme.text.code));
        assert_eq!(link.style.fg, Some(theme.accent.primary));
        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(
            heading
                .spans
                .iter()
                .filter(|span| !span.content.trim().is_empty())
                .all(|span| span.style.fg == Some(theme.accent.secondary))
        );
    }

    /// The opening paragraph is the conclusion and gets its own ink; every later
    /// paragraph is ordinary body. This is the internal hierarchy that keeps a
    /// final answer from reading as one flat block of color.
    #[test]
    fn a_final_answer_leads_with_its_first_paragraph_and_bodies_the_rest() {
        let theme = Theme::dark();
        let lines = assistant_render(
            &block("任务已经完成。\n\n这里是详细说明。", AssistantKind::Final),
            &theme,
            80,
        );

        let lead = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("任务"))
            .expect("lead span");
        let body = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("详细说明"))
            .expect("body span");

        assert_eq!(lead.style.fg, Some(theme.text.final_lead));
        assert!(lead.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(body.style.fg, Some(theme.text.primary));
        assert_ne!(theme.text.final_lead, theme.text.primary);
    }

    /// A heading before the opening paragraph is NOT the lead: the heading keeps
    /// its accent and the first paragraph after it becomes the lead.
    #[test]
    fn a_final_heading_does_not_consume_the_lead() {
        let theme = Theme::dark();
        let lines = assistant_render(
            &block(
                "## 结果\n\n任务已经完成。\n\n详细说明。",
                AssistantKind::Final,
            ),
            &theme,
            80,
        );
        let heading = lines
            .iter()
            .find(|line| line_text(line).contains("结果"))
            .expect("heading line");
        assert!(
            heading
                .spans
                .iter()
                .filter(|span| span.content.contains("结果") || span.content.contains('▎'))
                .all(|span| span.style.fg == Some(theme.accent.secondary))
        );
        let lead = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("任务"))
            .expect("lead span");
        let body = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("详细说明"))
            .expect("body span");
        assert_eq!(lead.style.fg, Some(theme.text.final_lead));
        assert_eq!(body.style.fg, Some(theme.text.primary));
    }

    /// A list's marker carries the accent; the item text stays body ink, so a
    /// list reads as a list rather than a block of blue.
    #[test]
    fn a_final_list_marks_with_accent_and_bodies_the_text() {
        let theme = Theme::dark();
        let lines = assistant_render(
            &block("- 第一项\n- 第二项", AssistantKind::Final),
            &theme,
            80,
        );
        let marker = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "• ")
            .expect("bullet");
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "第一项")
            .expect("item text");
        assert_eq!(marker.style.fg, Some(theme.accent.secondary));
        assert_eq!(text.style.fg, Some(theme.text.primary));
    }

    /// `**strong**` is brighter than body — real emphasis, not just a weight.
    #[test]
    fn strong_prose_is_brighter_than_body() {
        let theme = Theme::dark();
        let lines = assistant_render(
            &block("这里有 **重要信息** 结束。", AssistantKind::Final),
            &theme,
            80,
        );
        let strong = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("重要"))
            .expect("strong span");
        assert_eq!(strong.style.fg, Some(theme.text.strong));
        assert!(strong.style.add_modifier.contains(Modifier::BOLD));
        assert_ne!(theme.text.strong, theme.text.primary);
    }

    /// A blockquote is the one place a hand-written secondary tone comes from
    /// structure, not from scanning the words.
    #[test]
    fn a_final_blockquote_reads_as_secondary_prose() {
        let theme = Theme::dark();
        let lines = assistant_render(&block("> 补充说明", AssistantKind::Final), &theme, 80);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("补充"))
            .expect("quote span");
        assert_eq!(text.style.fg, Some(theme.text.secondary));
    }

    /// Progress lists carry process ink, not the final answer's accent: the
    /// subordination holds inside a block, not only at the block's marker.
    #[test]
    fn progress_lists_use_process_ink_not_the_final_accent() {
        let theme = Theme::dark();
        let lines = assistant_render(&block("- 过程项", AssistantKind::Progress), &theme, 80);
        let marker = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "• ")
            .expect("bullet");
        assert_eq!(marker.style.fg, Some(theme.text.secondary));
        assert_ne!(marker.style.fg, Some(theme.accent.secondary));
        let body = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("过程项"))
            .expect("item body");
        assert_eq!(body.style.fg, Some(theme.text.secondary));
    }

    #[test]
    fn streaming_and_completed_final_answers_share_the_same_wrapped_body_tone() {
        let theme = Theme::light();
        let text = "A final answer long enough to wrap over several visual rows consistently.";
        let complete = assistant_render(&block(text, AssistantKind::Final), &theme, 24);
        let mut streaming = block(text, AssistantKind::Final);
        streaming.done = false;
        streaming.rendered = None;
        let streaming = assistant_render(&streaming, &theme, 24);

        let body_colors = |lines: &[Line<'_>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .filter(|span| {
                    let text = span.content.as_ref();
                    !text.trim().is_empty() && text != "◆ " && text != "▌"
                })
                .map(|span| span.style.fg)
                .collect::<Vec<_>>()
        };
        assert!(!body_colors(&complete).is_empty());
        assert_eq!(body_colors(&complete), body_colors(&streaming));
    }

    #[test]
    fn no_color_keeps_final_structure_without_adding_color() {
        let lines = assistant_render(
            &block("final answer", AssistantKind::Final),
            &Theme::no_color(),
            40,
        );

        assert!(line_text(&lines[0]).starts_with("◆ "));
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .all(|span| span.style.fg == Some(ratatui::style::Color::Reset)
                    || span.style.fg.is_none())
        );
    }

    const LONG_PROGRESS: &str = "全部验证通过：

- ping → 200
- MCP 路径返回 404 是因为入口路径可能不同，但 HTTPS 层已通

最后确认续期配置没问题。certbot 自动装的 timer 指向标准路径，但我们的 hook 在非标准位置。

检查 renewal conf：";

    /// Interim prose is the agent talking to the user, not evidence: however
    /// long, every row is on screen and nothing offers to hide it.
    #[test]
    fn long_progress_prose_renders_in_full_with_no_disclosure() {
        let lines = rows(&block(LONG_PROGRESS, AssistantKind::Progress), 60);
        for fragment in ["全部验证通过", "ping → 200", "certbot", "检查 renewal conf"] {
            assert!(
                lines.iter().any(|l| l.contains(fragment)),
                "{fragment} missing: {lines:?}"
            );
        }
        assert!(
            !lines
                .iter()
                .any(|l| l.contains('▸') || l.contains("过程说明")),
            "{lines:?}"
        );
    }

    /// Streaming prose grows in place with its live cursor; it is not held to
    /// a row bound either.
    #[test]
    fn streaming_prose_renders_in_full_with_its_cursor() {
        let mut streaming = block(LONG_PROGRESS, AssistantKind::Pending);
        streaming.done = false;
        streaming.rendered = None;
        let lines = rows(&streaming, 60);
        assert!(
            lines.iter().any(|l| l.contains("检查 renewal conf")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains('▌')), "{lines:?}");
    }

    /// The answer is never compacted, however long it is.
    #[test]
    fn a_long_final_answer_is_never_folded() {
        let text = (1..=20)
            .map(|i| format!("第 {i} 行结论"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let lines = rows(&block(&text, AssistantKind::Final), 60);
        assert!(
            lines.iter().any(|l| l.contains("第 20 行结论")),
            "{lines:?}"
        );
    }

    #[test]
    fn item_render_hides_successful_update_goal() {
        let theme = Theme::no_color();
        let t = Locale::Zh.text();
        let item = TranscriptItem::ToolGroup(ToolGroupBlock {
            calls: vec![ToolCallBlock {
                exit_code: None,
                output: String::new(),
                output_truncated: false,
                expanded: false,
                stop: Default::default(),
                id: ToolCallId::new("g1"),
                name: "update_goal".into(),
                arguments: r#"{"status":"complete","summary":"用户询问这是什么项目，已回答"}"#
                    .into(),
                status: ToolStatus::Ok,
                preview: Some("Goal resolved.".into()),
                duration_ms: Some(1),
                parallel: false,
                batch: None,
                started_elapsed_secs: 0,
                applied_diff: None,
            }],
            open: false,
            display: crate::fold::DisplayMode::Collapsed,
            round: None,
        });
        let lines = item_render(&item, &theme, 120, false, t);
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            text.trim().is_empty(),
            "complete update_goal must not appear in Conversation: {text:?}"
        );
        assert!(!text.contains("目标收尾"), "{text:?}");
        assert!(!text.contains("完成："), "{text:?}");
    }

    fn turn_end_text(status: TurnEndStatus, detail: Option<&str>) -> String {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let item = TranscriptItem::TurnEnd(TurnEndBlock {
            status,
            tool_calls: 0,
            elapsed_secs: 0,
            summary: None,
            detail: detail.map(str::to_string),
        });
        item_render(&item, &theme, 120, false, t)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A reason is appended to the marker line only if it fits there. It was
    /// appended whatever its length and only wrapped below when the reason
    /// ALONE was near the full width — so a medium reason on a line that
    /// already carried tool count, elapsed and verification was clipped at the
    /// border, with nothing under it: "· develop: stopped before Review passed
    /// — Review produced no ver".
    #[test]
    fn a_reason_that_does_not_fit_the_marker_is_shown_under_it() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let reason =
            "develop: stopped before Review passed — Review produced no verdict for the change";
        let item = TranscriptItem::TurnEnd(TurnEndBlock {
            status: TurnEndStatus::CompletedWithWarnings,
            tool_calls: 14,
            elapsed_secs: 439,
            summary: Some("验证 ✓".into()),
            detail: Some(reason.to_string()),
        });
        let lines: Vec<String> = item_render(&item, &theme, 120, false, t)
            .iter()
            .map(line_text)
            .collect();
        for line in &lines {
            assert!(
                unicode_width::UnicodeWidthStr::width(line.as_str()) <= 120,
                "no line overruns the width: {line:?}"
            );
        }
        let below = lines[1..].join(" ");
        assert!(
            below.contains("Review produced no verdict"),
            "the whole reason is readable: {lines:#?}"
        );
    }

    #[test]
    fn turn_end_marker_distinguishes_all_terminal_states() {
        let completed = turn_end_text(TurnEndStatus::Completed, None);
        assert!(completed.contains("✓ 任务已完成"), "{completed}");
        let answered = turn_end_text(TurnEndStatus::Answered, None);
        assert!(
            answered.contains("✓ 回答结束"),
            "an answered turn is not a completion claim: {answered}"
        );
        assert!(!answered.contains("任务已完成"), "{answered}");
        let truncated = turn_end_text(TurnEndStatus::Truncated, Some("context limit"));
        assert!(truncated.contains("⚠ 已完成，但有警告"), "{truncated}");
        // Budget/loop/stall incompletes read as "未完成", not the old "被阻塞".
        let incomplete = turn_end_text(TurnEndStatus::Incomplete, Some("预算已耗尽"));
        assert!(incomplete.contains("⚠ 未完成"), "{incomplete}");
        let failed = turn_end_text(TurnEndStatus::Failed, Some("boom"));
        assert!(failed.contains("✗ 失败"), "{failed}");
        let cancelled = turn_end_text(TurnEndStatus::Cancelled, None);
        assert!(cancelled.contains("⊘ 已取消"), "{cancelled}");
    }

    #[test]
    fn turn_end_marker_colors_only_symbol_and_word_not_stats() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let item = TranscriptItem::TurnEnd(TurnEndBlock {
            status: TurnEndStatus::Failed,
            tool_calls: 2,
            elapsed_secs: 5,
            summary: None,
            detail: Some("boom".into()),
        });
        let lines = item_render(&item, &theme, 80, false, t);
        assert_eq!(lines.len(), 1);
        // lead border / label / stats / tail: stats must not take the status color.
        let stats = &lines[0].spans[2];
        assert!(stats.content.contains("boom"), "{stats:?}");
        assert_eq!(stats.style.fg, Some(theme.text.secondary), "{stats:?}");
        assert_eq!(lines[0].spans[1].style.fg, Some(theme.status.error));
    }

    fn sub_agent(id: &str, nickname: &str, status: ToolStatus) -> crate::transcript::SubAgentBlock {
        crate::transcript::SubAgentBlock {
            id: id.into(),
            nickname: nickname.into(),
            agent_name: None,
            role: "explorer".into(),
            status,
            task: "task".into(),
            detail: if status == ToolStatus::Failed {
                "Reached the 6-round limit before finishing.".into()
            } else {
                "done".into()
            },
            progress: Default::default(),
            recent_step: None,
            started_elapsed_secs: 0,
            settled_elapsed_secs: (status != ToolStatus::Running).then_some(0),
            expanded: false,
            contribution: crate::multi_agent::Contribution::Pending,
            // A failed fixture child was stopped by its wall clock — typed, as
            // the runtime sends it; the summary text above is only prose. The
            // bound is what makes this fixture a timeout and not a spent
            // token budget.
            stop: (status == ToolStatus::Failed)
                .then_some(leveler_client_protocol::ChildStop::Budget),
            limit: (status == ToolStatus::Failed)
                .then_some(leveler_client_protocol::ChildLimit::Duration),
            interrupted: false,
            unreported: false,
        }
    }

    #[test]
    fn a_long_sub_agent_result_is_bounded_and_says_so() {
        // The runtime hands back whatever the agent wrote. Rendering all of it
        // lets one delegated task push the conversation off screen; dropping it
        // silently would be worse. Bound it and mark the cut.
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let mut a = sub_agent("agent-1", "Euclid", ToolStatus::Ok);
        a.detail = (1..=20)
            .map(|i| format!("结论第 {i} 条，这一行足够长以至于会占满整行宽度不会被合并"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = sub_agent_tree_lines(&[&a], &theme, 80, t, 0);
        assert!(
            lines.len() <= 6,
            "a finished sub-agent must not flood the transcript, got {} rows",
            lines.len()
        );
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        // The honest count is the cut marker; expansion is click / the
        // undocumented Ctrl+O fallback, neither advertised inline.
        assert!(
            text.contains("还有 16 行"),
            "the cut must be named truthfully: {text}"
        );
        assert!(
            !text.contains("Ctrl+O"),
            "shortcuts are not advertised: {text}"
        );
    }

    #[test]
    fn expanding_a_sub_agent_shows_its_whole_result() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let mut a = sub_agent("agent-1", "Euclid", ToolStatus::Ok);
        a.detail = (1..=20)
            .map(|i| format!("结论第 {i} 条"))
            .collect::<Vec<_>>()
            .join("\n");
        a.expanded = true;
        let lines = sub_agent_tree_lines(&[&a], &theme, 80, t, 0);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            text.contains("结论第 20 条"),
            "expanded must be complete: {text}"
        );
    }

    #[test]
    fn a_finished_explorer_shows_its_finding_count() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let mut a = sub_agent("agent-1", "Euclid", ToolStatus::Ok);
        a.contribution = crate::multi_agent::Contribution::Reported { total: 3 };
        let lines = sub_agent_tree_lines(&[&a], &theme, 80, t, 0);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            text.contains("3 项发现"),
            "finding count must be on the head line: {text}"
        );
    }

    #[test]
    fn a_short_sub_agent_result_is_untouched() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let a = sub_agent("agent-1", "Euclid", ToolStatus::Ok);
        let lines = sub_agent_tree_lines(&[&a], &theme, 80, t, 0);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(!text.contains("Ctrl+O"), "no cut, no hint: {text}");
    }

    #[test]
    fn sub_agent_tree_keeps_tasks_instead_of_live_tool_steps() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let mut a = sub_agent("agent-1", "Euclid", ToolStatus::Running);
        a.progress.active = true;
        a.recent_step = Some("list_files".into());
        let mut b = sub_agent("agent-2", "Newton", ToolStatus::Running);
        b.progress.active = true;
        b.recent_step = Some("grep ✓".into());
        let lines = sub_agent_tree_lines(&[&a, &b], &theme, 100, t, 0);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("task"), "{text}");
        assert!(
            !text.contains("list_files") && !text.contains("grep ✓"),
            "{text}"
        );
    }

    #[test]
    fn sub_agent_tree_aggregates_running_agents() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let mut a = sub_agent("agent-1", "Euclid", ToolStatus::Running);
        a.progress.active = true;
        a.progress.input_tokens = 1_200;
        a.progress.output_tokens = 80;
        let mut b = sub_agent("agent-2", "Newton", ToolStatus::Running);
        b.progress.input_tokens = 2_400;
        b.progress.output_tokens = 160;
        let lines = sub_agent_tree_lines(&[&a, &b], &theme, 100, t, 0);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("◌ 2 个 agents 正在运行"), "{text}");
        assert!(text.contains("3.8k tokens"), "{text}");
        assert!(text.contains("task"), "{text}");
        assert!(
            !text.contains("├─ Euclid") && !text.contains("└─ Newton"),
            "conversation keeps one durable step summary, not the live roster: {text}"
        );
    }

    #[test]
    fn running_sub_agents_show_the_batch_elapsed_time() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let mut a = sub_agent("agent-1", "Euclid", ToolStatus::Running);
        a.started_elapsed_secs = 3; // started 3s into the turn
        let mut b = sub_agent("agent-2", "Newton", ToolStatus::Running);
        b.started_elapsed_secs = 10;
        // Turn is now 15s in: Euclid has run 12s, Newton 5s.
        let lines = sub_agent_tree_lines(&[&a, &b], &theme, 100, t, 15);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("12s"), "batch elapsed missing: {text}");
        assert!(
            !text.contains("5s"),
            "per-agent time belongs to the live panel: {text}"
        );
    }

    #[test]
    fn sub_agent_tree_all_done_shows_stats_not_status_words() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let mut a = sub_agent("agent-1", "Euclid", ToolStatus::Ok);
        a.progress.input_tokens = 87_800;
        let b = sub_agent("agent-2", "Newton", ToolStatus::Ok);
        let lines = sub_agent_tree_lines(&[&a, &b], &theme, 100, t, 0);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("✓ 2 个 agents 完成"), "{text}");
        assert!(text.contains("87.8k tokens"), "{text}");
        assert!(!text.contains("├─ Euclid"), "{text}");
    }

    #[test]
    fn sub_agent_tree_with_failure_breaks_down_outcomes() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let a = sub_agent("agent-1", "Euclid", ToolStatus::Ok);
        let b = sub_agent("agent-2", "Newton", ToolStatus::Failed);
        let lines = sub_agent_tree_lines(&[&a, &b], &theme, 100, t, 0);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("⚠ 2 个 agents 结束"), "{text}");
        assert!(text.contains("1 已完成 · 1 超时"), "{text}");
        assert!(
            !text.contains("├─ Euclid") && !text.contains("✗ 超时"),
            "{text}"
        );
    }

    /// The termination label is the shared `ChildStop + ChildLimit` mapping,
    /// not a transcript-local budget rule. Duration is the wall clock; every
    /// other budget stop is a spent budget; a missing bound is the honest
    /// generic word, never a guessed timeout.
    #[test]
    fn a_failed_childs_termination_label_reads_the_typed_bound() {
        use leveler_client_protocol::{ChildLimit, ChildStop};
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let cases: &[(Option<ChildStop>, Option<ChildLimit>, &str)] = &[
            (Some(ChildStop::Budget), Some(ChildLimit::Duration), "超时"),
            (
                Some(ChildStop::Budget),
                Some(ChildLimit::ModelTokens),
                "预算耗尽",
            ),
            (Some(ChildStop::Budget), Some(ChildLimit::Cost), "预算耗尽"),
            (
                Some(ChildStop::Budget),
                Some(ChildLimit::Commands),
                "预算耗尽",
            ),
            (
                Some(ChildStop::Budget),
                Some(ChildLimit::ModifiedFiles),
                "预算耗尽",
            ),
            (
                Some(ChildStop::Budget),
                Some(ChildLimit::RoundWindow),
                "预算耗尽",
            ),
            (
                Some(ChildStop::Budget),
                Some(ChildLimit::RoundCeiling),
                "预算耗尽",
            ),
            // Old record / replay: no typed bound must not become a timeout.
            (Some(ChildStop::Budget), None, "预算耗尽"),
            // Other termination states keep their existing words.
            (Some(ChildStop::Cancelled), None, "已取消"),
            (Some(ChildStop::Lost), None, "已丢失"),
            (Some(ChildStop::Failed), None, "未完成"),
            (Some(ChildStop::Incomplete), None, "未完成"),
        ];
        for (stop, limit, expected) in cases {
            let mut child = sub_agent("agent-1", "Euclid", ToolStatus::Failed);
            child.stop = *stop;
            child.limit = *limit;
            let (label, _) = sub_agent_tree_child_status(&child, &theme, t);
            assert_eq!(
                label,
                format!("✗ {expected}"),
                "stop={stop:?} limit={limit:?}"
            );
            // Cross-surface contract: the transcript must be the shared
            // mapping's own label, so Activity Detail can never disagree.
            let shared = crate::multi_agent::child_stop_label(*stop, *limit, t)
                .unwrap_or(t.sub_agent_incomplete);
            assert_eq!(
                label,
                format!("✗ {shared}"),
                "transcript and the shared mapping drifted: stop={stop:?} limit={limit:?}"
            );
        }
    }

    /// A completed child keeps its success word; the typed bound only changes
    /// the failed/budget branch.
    #[test]
    fn a_completed_child_still_reads_as_completed() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let child = sub_agent("agent-1", "Euclid", ToolStatus::Ok);
        let (label, _) = sub_agent_tree_child_status(&child, &theme, t);
        assert_eq!(label, "✓ 已完成");
    }

    /// The group breakdown is the same truth: only the wall clock is a
    /// timeout. A spent token budget is not.
    #[test]
    fn the_group_breakdown_counts_only_the_wall_clock_as_timeout() {
        use leveler_client_protocol::{ChildLimit, ChildStop};
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let ok = sub_agent("agent-1", "Euclid", ToolStatus::Ok);

        let mut token_budget = sub_agent("agent-2", "Newton", ToolStatus::Failed);
        token_budget.stop = Some(ChildStop::Budget);
        token_budget.limit = Some(ChildLimit::ModelTokens);
        let text = sub_agent_tree_lines(&[&ok, &token_budget], &theme, 100, t, 0)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("1 已完成"), "{text}");
        assert!(
            !text.contains("超时"),
            "a spent token budget is not a timeout:\n{text}"
        );
        assert!(text.contains("未完成"), "{text}");

        let mut timed_out = sub_agent("agent-2", "Newton", ToolStatus::Failed);
        timed_out.stop = Some(ChildStop::Budget);
        timed_out.limit = Some(ChildLimit::Duration);
        let text = sub_agent_tree_lines(&[&ok, &timed_out], &theme, 100, t, 0)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("1 超时"), "{text}");
        assert!(
            !text.contains("✗ 超时"),
            "per-agent outcome belongs to the live panel: {text}"
        );
    }

    #[test]
    fn single_sub_agent_keeps_classic_rendering() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let a = sub_agent("agent-1", "Euclid", ToolStatus::Running);
        let lines = sub_agent_tree_lines(&[&a], &theme, 100, t, 0);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("◌ 探索 Agent 1"), "{text}");
        assert!(!text.contains("├─"), "{text}");
    }

    #[test]
    fn completion_report_is_localized() {
        let theme = Theme::default();
        let report = UiCompletionReport {
            files_changed: 3,
            added: 86,
            removed: 31,
            success: true,
        };
        let item = TranscriptItem::Completion(report);
        let zh = item_render(&item, &theme, 120, false, Locale::Zh.text())
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(zh.contains("✓ 任务已完成"), "{zh}");
        assert!(zh.contains("修改 3 个文件  +86 / -31"), "{zh}");
        assert!(zh.contains("/diff 查看改动"), "{zh}");

        let en = item_render(&item, &theme, 120, false, Locale::En.text())
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(en.contains("✓ Task completed"), "{en}");
        assert!(en.contains("3 files changed"), "{en}");
        assert!(en.contains("/diff to view changes"), "{en}");
    }

    fn recap_lines(body: &str, width: usize) -> Vec<String> {
        let item = TranscriptItem::Recap(crate::transcript::RecapBlock {
            summary: Some(body.to_string()),
            next_step: None,
        });
        item_render(&item, &Theme::default(), width, false, Locale::Zh.text())
            .iter()
            .map(line_text)
            .collect()
    }

    /// Continuation rows are indented with spaces. The wrapped piece itself
    /// does not start with a space, so stripping the indent recovers the text.
    fn recover_prefixed(lines: &[String], prefix: &str) -> String {
        let mut body = String::new();
        for (index, line) in lines.iter().enumerate() {
            if index == 0 {
                assert!(line.starts_with(prefix), "{line}");
                body.push_str(&line[prefix.len()..]);
            } else {
                body.push_str(line.trim_start_matches(' '));
            }
        }
        body
    }

    /// The dogfood recap lost 两 at 40 columns because the prefix was measured
    /// in characters while the row was painted in display columns.
    #[test]
    fn a_cjk_recap_keeps_every_wide_glyph() {
        let body = "三句话说明 yq 仓库用途，仅读两个文件，未改文件";
        let prefix = format!("※ {}: ", Locale::Zh.text().recap_label);
        for width in [80usize, 48, 40, 32] {
            let lines = recap_lines(body, width);
            assert_eq!(recover_prefixed(&lines, &prefix), body, "width {width}");
            for line in &lines {
                let painted = UnicodeWidthStr::width(line.as_str());
                assert!(
                    painted <= width,
                    "width {width} painted {painted}: {line:?}"
                );
            }
            assert!(
                lines.iter().any(|line| line.contains('两')),
                "width {width} dropped 两: {lines:?}"
            );
        }
    }

    /// The first visual row ends on a width-2 glyph that still fits. A short
    /// prefix budget pushes that glyph past the terminal edge and the cell clips it.
    #[test]
    fn a_prefixed_row_ends_on_a_wide_glyph_without_overflow() {
        let prefix = format!("※ {}: ", Locale::Zh.text().recap_label);
        let prefix_cols = UnicodeWidthStr::width(prefix.as_str());
        let width = 40usize;
        let glyphs = width.saturating_sub(prefix_cols) / 2;
        let mut body = "仅".repeat(glyphs);
        body.push('两');
        let lines = recap_lines(&body, width);
        assert_eq!(recover_prefixed(&lines, &prefix), body, "{lines:?}");
        let first = UnicodeWidthStr::width(lines[0].as_str());
        assert!(
            first <= width,
            "first row overflowed: {first} > {width}: {lines:?}"
        );
        let last = lines[0].chars().last().unwrap();
        assert_eq!(
            unicode_width::UnicodeWidthChar::width(last),
            Some(2),
            "the row must end on a whole wide glyph: {lines:?}"
        );
        assert!(
            lines.get(1).is_some_and(|line| line.contains('两')),
            "两 must wrap onto a following row instead of being clipped: {lines:?}"
        );
    }

    #[test]
    fn an_ascii_error_prefix_still_wraps_inside_the_width() {
        // No spaces: a word-break would drop the break space, which is wrap's
        // existing behavior and not a lost glyph.
        let body = "x".repeat(70);
        let width = 40usize;
        let item = TranscriptItem::Error(body);
        let lines: Vec<String> =
            item_render(&item, &Theme::default(), width, false, Locale::En.text())
                .iter()
                .map(line_text)
                .collect();
        assert_eq!(recover_prefixed(&lines, "✗ "), "x".repeat(70));
        for line in &lines {
            assert!(UnicodeWidthStr::width(line.as_str()) <= width, "{line:?}");
        }
    }

    #[test]
    fn a_recap_with_an_emoji_keeps_the_wide_glyph() {
        // Sixteen width-2 hanzi fill the correct 40-column body. The emoji is
        // the next glyph, so a character-counted prefix pushes it past the edge.
        let mut body = "仅".repeat(16);
        body.push('😀');
        body.push_str("未改");
        let prefix = format!("※ {}: ", Locale::Zh.text().recap_label);
        let lines = recap_lines(&body, 40);
        assert_eq!(recover_prefixed(&lines, &prefix), body);
        assert!(lines.iter().any(|line| line.contains('😀')), "{lines:?}");
        for line in &lines {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 40, "{line:?}");
        }
    }
}
