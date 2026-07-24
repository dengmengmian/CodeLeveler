use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use leveler_client_protocol::UiCompletionReport;

use crate::i18n::{Locale, UiText};
use crate::theme::Theme;
use crate::transcript::{AssistantBlock, ToolStatus, TranscriptItem, TurnEndBlock, TurnEndStatus};

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
    // Content is indented two columns under a leading "●" bullet.
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
    let (mut lines, mut stable) = doc.to_lines_split(inner, theme);
    if block.done {
        // A finished message is fully stable.
        stable = lines.len();
    } else {
        // Streaming cursor is part of the live tail.
        lines.push(Line::from(Span::styled(
            "▌",
            Style::default().fg(theme.muted),
        )));
    }
    // Bulleting maps lines 1:1, so the stable boundary is preserved.
    let bulleted = bulleted(lines, "●", Style::default().fg(theme.accent));
    (bulleted, stable)
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
        // Welcome card retired — workbench Header + Input replace it.
        TranscriptItem::Welcome(_) => {}
        TranscriptItem::User(text) => {
            // A continuous heading left-bar + bold text marks a user turn clearly
            // apart from the assistant's "●" bullet + normal-weight prose.
            let bar = Style::default()
                .fg(theme.heading)
                .add_modifier(Modifier::BOLD);
            let body = Style::default()
                .fg(theme.user_message)
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
            out.extend(assistant_split(block, theme, wrap_width).0);
        }
        TranscriptItem::ToolGroup(group) => {
            // Same product surface as workbench Conversation: Silent tools
            // (update_goal success, ls probes, …) stay out; exploration
            // aggregates; Important edits/runs are one line each.
            // `tools_expanded` is legacy — expand is per-group via Ctrl+O.
            let _ = tools_expanded;
            let locale = locale_from_ui_text(t);
            out.extend(crate::activity_stream::render_group(
                group, theme, wrap_width, locale, t,
            ));
        }
        // Scrollback: the agent is finished, so no live elapsed is shown (0).
        TranscriptItem::SubAgent(block) => {
            sub_agent_lines(block, theme, wrap_width, &mut out, t, 0)
        }
        TranscriptItem::Completion(report) => completion_lines(report, theme, &mut out, t),
        TranscriptItem::Error(text) => {
            push_prefixed(
                &mut out,
                "✗ ",
                text,
                Style::default().fg(theme.error),
                wrap_width,
            );
        }
        TranscriptItem::Note(text) => {
            push_prefixed(
                &mut out,
                "◆ ",
                text,
                Style::default().fg(theme.muted),
                wrap_width,
            );
        }
        TranscriptItem::TurnEnd(block) => turn_end_lines(block, theme, wrap_width, &mut out, t),
        TranscriptItem::Recap(block) => {
            let text = match &block.summary {
                Some(summary) => format!("{summary} · {}{}", t.recap_next_step, block.next_step),
                None => format!("{}{}", t.recap_next_step, block.next_step),
            };
            push_prefixed(
                &mut out,
                &format!("※ {}: ", t.recap_label),
                &text,
                Style::default().fg(theme.muted),
                wrap_width,
            );
        }
        TranscriptItem::Btw(block) => out.extend(btw_card_lines(block, theme, wrap_width, t)),
    }
    out
}

/// Floating `/btw` card (overlay on Conversation). Full border, question on top,
/// answer or Answering… / Esc status on the bottom — not mixed into main history.
pub(crate) fn btw_card_lines(
    block: &crate::transcript::BtwBlock,
    theme: &Theme,
    wrap_width: usize,
    t: &crate::i18n::UiText,
) -> Vec<Line<'static>> {
    let border = Style::default().fg(theme.attachment);
    let muted = Style::default().fg(theme.muted);
    let text = Style::default().fg(theme.text);
    let err = Style::default().fg(theme.error);
    let accent = Style::default().fg(theme.attachment);

    let box_w = wrap_width.max(16);
    let inner_w = box_w.saturating_sub(2).max(8);

    let framed = |payload: Vec<Span<'static>>| -> Line<'static> {
        let used: usize = payload
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let pad = inner_w.saturating_sub(used);
        let mut spans = Vec::with_capacity(payload.len() + 3);
        spans.push(Span::styled("│", border));
        spans.extend(payload);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
        spans.push(Span::styled("│", border));
        Line::from(spans)
    };

    let mut out = vec![Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(inner_w)),
        border,
    ))];

    // Question: `/btw <question>`
    let q_prefix = "/btw ";
    let q_budget = inner_w
        .saturating_sub(1 + UnicodeWidthStr::width(q_prefix))
        .max(4);
    let q_lines = wrap(&block.question, q_budget);
    for (i, line) in q_lines.into_iter().enumerate() {
        if i == 0 {
            out.push(framed(vec![
                Span::raw(" "),
                Span::styled(q_prefix.to_string(), accent.add_modifier(Modifier::BOLD)),
                Span::styled(line, text),
            ]));
        } else {
            out.push(framed(vec![
                Span::raw(" "),
                Span::raw(" ".repeat(UnicodeWidthStr::width(q_prefix))),
                Span::styled(line, text),
            ]));
        }
    }

    // Blank separator row.
    out.push(framed(vec![Span::raw(" ".repeat(inner_w))]));

    // Answer body or status row with Esc hint on the right.
    let status_right = "[Esc]";
    let status_right_w = UnicodeWidthStr::width(status_right);

    if block.answer.is_empty() && !block.done {
        let left = if block.failed {
            t.btw_failed
        } else {
            t.btw_answering
        };
        let left_style = if block.failed { err } else { muted };
        let gap = inner_w
            .saturating_sub(1 + UnicodeWidthStr::width(left) + status_right_w)
            .max(1);
        out.push(framed(vec![
            Span::raw(" "),
            Span::styled(left.to_string(), left_style),
            Span::raw(" ".repeat(gap)),
            Span::styled(status_right.to_string(), muted),
        ]));
    } else if block.failed && block.answer.is_empty() {
        let left = t.btw_failed;
        let gap = inner_w
            .saturating_sub(1 + UnicodeWidthStr::width(left) + status_right_w)
            .max(1);
        out.push(framed(vec![
            Span::raw(" "),
            Span::styled(left.to_string(), err),
            Span::raw(" ".repeat(gap)),
            Span::styled(status_right.to_string(), muted),
        ]));
    } else {
        let doc = crate::markdown::MdDoc::parse(&block.answer);
        let mut answer_lines = doc.to_lines(inner_w.saturating_sub(2).max(4), theme);
        const MAX_A: usize = 8;
        let truncated = answer_lines.len() > MAX_A;
        if truncated {
            answer_lines.truncate(MAX_A);
        }
        for line in answer_lines {
            let mut spans = vec![Span::raw(" ")];
            let mut used = 1usize;
            for sp in line.spans {
                used += UnicodeWidthStr::width(sp.content.as_ref());
                spans.push(sp);
            }
            let _ = used;
            out.push(framed(spans));
        }
        if truncated {
            out.push(framed(vec![Span::styled(" …", muted)]));
        }
        // Status footer with Esc when finished.
        if block.done || block.failed {
            let left = if block.failed {
                t.btw_failed
            } else {
                t.btw_dismiss
            };
            let left_style = if block.failed { err } else { muted };
            let gap = inner_w
                .saturating_sub(1 + UnicodeWidthStr::width(left) + status_right_w)
                .max(1);
            out.push(framed(vec![
                Span::raw(" "),
                Span::styled(left.to_string(), left_style),
                Span::raw(" ".repeat(gap)),
                Span::styled(status_right.to_string(), muted),
            ]));
        }
    }

    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner_w)),
        border,
    )));
    out
}

fn turn_end_lines(
    block: &TurnEndBlock,
    theme: &Theme,
    width: usize,
    out: &mut Vec<Line<'static>>,
    t: &crate::i18n::UiText,
) {
    let detail_token = block.detail.as_deref().map(str::trim);
    let no_code_changes = block.status == TurnEndStatus::Unverified
        && detail_token == Some(leveler_client_protocol::REASON_NO_CODE_CHANGES);
    let no_auto_verify = block.status == TurnEndStatus::Unverified
        && detail_token == Some(leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION);
    // The marker reads as `symbol + status word` (colored by outcome) followed
    // by muted stats. Soft unverified (no gate / no edits) is a calm finish,
    // not a warning, so it keeps its own low-key wording.
    let (label, color) = match block.status {
        TurnEndStatus::Completed | TurnEndStatus::Answered => {
            (format!("✓ {}", t.turn_end_completed), theme.success)
        }
        TurnEndStatus::Truncated => (format!("⚠ {}", t.final_completed_warnings), theme.warning),
        // Incomplete = the run stopped mid-task on its own (budget / loop guard /
        // failed verification gate / model gave up). None of these are a *system*
        // block, so the honest word is "未完成"; the detail says how to continue.
        // A failed verification gate gets its own accurate word ("验证未通过")
        // instead of the misleading "被阻塞" — that was the false-blocked UX.
        TurnEndStatus::Incomplete => {
            let is_gate_failure = detail_token.is_some_and(|d| d.starts_with("failed gate(s)"));
            let word = if is_gate_failure {
                t.final_verification_failed
            } else {
                t.final_blocked
            };
            (format!("⚠ {word}"), theme.warning)
        }
        TurnEndStatus::Unverified if no_code_changes => {
            (t.turn_no_code_changes.to_string(), theme.success)
        }
        TurnEndStatus::Unverified if no_auto_verify => {
            (t.turn_unverified.to_string(), theme.success)
        }
        TurnEndStatus::Unverified => (format!("⚠ {}", t.final_completed_warnings), theme.warning),
        TurnEndStatus::Failed => (format!("✗ {}", t.final_failed), theme.error),
        // Cancelled is user-initiated, not a failure: stopped glyph, muted.
        TurnEndStatus::Cancelled => (format!("⊘ {}", t.final_cancelled), theme.muted),
    };
    let mut stats = String::new();
    if matches!(
        block.status,
        TurnEndStatus::Completed | TurnEndStatus::Answered | TurnEndStatus::Unverified
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
    // Soft machine tokens are already folded into the label — do not re-append.
    // Other incomplete reasons stay on the marker, but keep them short.
    let folded = no_code_changes || no_auto_verify;
    if let Some(detail) = &block.detail {
        let d = localized_turn_detail(detail, t);
        if !folded && !d.is_empty() {
            stats.push_str(" · ");
            stats.push_str(d);
        }
    }
    let lead = "── ";
    let used = UnicodeWidthStr::width(lead)
        + UnicodeWidthStr::width(label.as_str())
        + UnicodeWidthStr::width(stats.as_str())
        + 1;
    let tail = "─".repeat(width.saturating_sub(used));
    out.push(Line::from(vec![
        Span::styled(lead, Style::default().fg(theme.border)),
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(stats, Style::default().fg(theme.muted)),
        Span::styled(format!(" {tail}"), Style::default().fg(theme.border)),
    ]));
    // Very long residual reasons wrap onto a second dim line.
    if !folded && let Some(detail) = &block.detail {
        let d = localized_turn_detail(detail, t);
        if UnicodeWidthStr::width(d) > width.saturating_sub(6) {
            for line in wrap(d, width.saturating_sub(4).max(8)) {
                out.push(Line::from(Span::styled(
                    format!("   {line}"),
                    Style::default().fg(theme.muted),
                )));
            }
        }
    }
}

fn localized_turn_detail<'a>(detail: &'a str, t: &'a crate::i18n::UiText) -> &'a str {
    let d = detail.trim();
    match d {
        leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION => {
            t.turn_no_automatic_verification
        }
        // Executor machine tokens + long defaults → short product copy.
        s if s.contains("observe thrash") && s.contains("plan complete") => t.turn_plan_thrash,
        s if s.contains("observe thrash") || s.starts_with("no-progress streak") => {
            t.turn_observe_thrash
        }
        s if s.contains("预算已耗尽")
            || s.starts_with("轮次或资源预算")
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
        s if s.starts_with("failed gate(s): ") => {
            s.strip_prefix("failed gate(s): ").unwrap_or(s)
        }
        other => other,
    }
}

/// Whether a transcript item is finalized and safe to commit to scrollback
/// (streaming assistants and running tools are not yet final).
pub fn item_is_final(item: &TranscriptItem) -> bool {
    match item {
        TranscriptItem::Assistant(b) => b.done,
        TranscriptItem::ToolGroup(group) => {
            !group.open
                && group
                    .calls
                    .iter()
                    .all(|call| call.status != ToolStatus::Running)
        }
        TranscriptItem::SubAgent(b) => b.status != ToolStatus::Running,
        TranscriptItem::Btw(b) => b.done,
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
        ToolStatus::Running if block.progress.active => t.sub_agent_running,
        ToolStatus::Running => t.sub_agent_waiting,
        ToolStatus::Ok => t.sub_agent_completed,
        ToolStatus::Failed => t.sub_agent_incomplete,
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
        ToolStatus::Running => ("◌", theme.accent),
        ToolStatus::Ok => ("✓", theme.success),
        ToolStatus::Failed => ("✗", theme.error),
    };
    let mut head_spans = vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::styled(
            sub_agent_display_name(block, t),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    head_spans.push(Span::styled(
        format!(" · {}", sub_agent_status(block, t)),
        Style::default().fg(theme.muted),
    ));
    let elapsed = sub_agent_elapsed(block, now_elapsed_secs);
    if !elapsed.is_empty() {
        head_spans.push(Span::styled(
            format!(" · {elapsed}"),
            Style::default().fg(theme.muted),
        ));
    }
    if let Some(step) = block.recent_step.as_deref().filter(|s| !s.is_empty()) {
        head_spans.push(Span::styled(
            format!(" · {step}"),
            Style::default().fg(theme.accent),
        ));
    }
    let usage = sub_agent_usage(block, t);
    if !usage.is_empty() {
        head_spans.push(Span::styled(
            format!(" · {usage}"),
            Style::default().fg(theme.muted),
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
        for line in wrap(detail, inner) {
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(line, Style::default().fg(theme.muted)),
            ]));
        }
    }
}

/// Render a run of consecutive sub-agent blocks as one inline tree: an
/// aggregate header (◌ running / ✓ all done / ⚠ ended with failures) plus one
/// `├─/└─` child per agent. A lone agent keeps the classic single-block
/// rendering. Only data the blocks actually carry is shown (token usage);
/// per-agent tool counts / wall time are not tracked, so they are not faked.
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
        many => sub_agent_tree_group_lines(many, theme, &mut out, t, now_elapsed_secs),
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
            theme.accent,
            t.agents_running_header.replacen("{}", &count, 1),
        )
    } else if all_ok {
        (
            "✓",
            theme.success,
            t.agents_done_header.replacen("{}", &count, 1),
        )
    } else {
        (
            "⚠",
            theme.warning,
            t.agents_ended_header.replacen("{}", &count, 1),
        )
    };

    // Aggregate the token usage the runtime actually reports.
    let sum_in = blocks
        .iter()
        .fold(0u64, |acc, b| acc + u64::from(b.progress.input_tokens));
    let sum_out = blocks
        .iter()
        .fold(0u64, |acc, b| acc + u64::from(b.progress.output_tokens));
    let mut stats = String::new();
    if sum_in > 0 || sum_out > 0 {
        stats.push_str(&format!(
            " · ↑ {} · ↓ {}",
            crate::status_line::fmt_tokens_compact(u32::try_from(sum_in).unwrap_or(u32::MAX)),
            crate::status_line::fmt_tokens_compact(u32::try_from(sum_out).unwrap_or(u32::MAX))
        ));
    }
    // A finished-but-not-clean run breaks down how each agent ended.
    if !any_running && !all_ok {
        let completed = blocks.iter().filter(|b| b.status == ToolStatus::Ok).count();
        let timeout = blocks
            .iter()
            .filter(|b| b.status == ToolStatus::Failed && sub_agent_hit_round_limit(b))
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
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(stats, Style::default().fg(theme.dim)),
    ]));

    // Children: nickname first, then the localized role name. Stats/status in
    // a right column aligned on the widest name.
    let names: Vec<String> = blocks
        .iter()
        .map(|b| {
            if b.nickname.trim().is_empty() {
                sub_agent_display_name(b, t)
            } else {
                b.nickname.clone()
            }
        })
        .collect();
    let name_w = names
        .iter()
        .map(|name| UnicodeWidthStr::width(name.as_str()))
        .max()
        .unwrap_or(0);
    for (i, (block, name)) in blocks.iter().zip(&names).enumerate() {
        let branch = if i + 1 == blocks.len() {
            "└─"
        } else {
            "├─"
        };
        let mut spans = vec![
            Span::styled(format!("  {branch} "), Style::default().fg(theme.border)),
            Span::styled(name.clone(), Style::default().fg(theme.text)),
        ];
        // Only an imperfect run spells out each child's outcome; a fully
        // successful batch shows usage stats instead of repeating "completed".
        // While running, prefer the real recent tool/step when present.
        let (right, right_color) = if block.status == ToolStatus::Running {
            // Running children lead with their own elapsed time so the user can
            // see each agent is alive and how long it has worked, then the real
            // recent tool/step (or a plain running status when none yet).
            let elapsed = sub_agent_elapsed(block, now_elapsed_secs);
            let detail = block
                .recent_step
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| sub_agent_tree_child_status(block, theme, t).0);
            let text = match (elapsed.is_empty(), detail.is_empty()) {
                (false, false) => format!("{elapsed} · {detail}"),
                (false, true) => elapsed,
                (true, false) => detail,
                (true, true) => String::new(),
            };
            (text, theme.accent)
        } else if all_ok {
            (sub_agent_tree_child_usage(block), theme.dim)
        } else {
            sub_agent_tree_child_status(block, theme, t)
        };
        if !right.is_empty() {
            let pad = name_w.saturating_sub(UnicodeWidthStr::width(name.as_str())) + 2;
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(right, Style::default().fg(right_color)));
        }
        out.push(Line::from(spans));
    }
}

/// Whether a failed sub-agent hit its round limit (the "timeout" outcome).
fn sub_agent_hit_round_limit(block: &crate::transcript::SubAgentBlock) -> bool {
    block.detail.starts_with("Reached the ")
        && block.detail.contains("-round limit before finishing.")
}

/// Compact usage stats for one fully-succeeded tree child (`↑ 87.8k · ↓ 45.8k`).
fn sub_agent_tree_child_usage(block: &crate::transcript::SubAgentBlock) -> String {
    let usage = &block.progress;
    if usage.input_tokens == 0 && usage.output_tokens == 0 {
        return String::new();
    }
    format!(
        "↑ {} · ↓ {}",
        crate::status_line::fmt_tokens_compact(usage.input_tokens),
        crate::status_line::fmt_tokens_compact(usage.output_tokens)
    )
}

/// Status word for one tree child in a non-all-success batch. Running agents
/// keep the waiting/running distinction; finished ones carry a ✓/✗ glyph.
fn sub_agent_tree_child_status(
    block: &crate::transcript::SubAgentBlock,
    theme: &Theme,
    t: &crate::i18n::UiText,
) -> (String, ratatui::style::Color) {
    match block.status {
        ToolStatus::Running if block.progress.active => {
            (t.agent_status_running.to_string(), theme.accent)
        }
        ToolStatus::Running => (t.sub_agent_waiting.to_string(), theme.muted),
        ToolStatus::Ok => (format!("✓ {}", t.agent_status_completed), theme.success),
        ToolStatus::Failed if sub_agent_hit_round_limit(block) => {
            (format!("✗ {}", t.agent_status_timeout), theme.error)
        }
        ToolStatus::Failed => (format!("✗ {}", t.sub_agent_incomplete), theme.error),
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
        ("✓", theme.success)
    } else {
        ("✗", theme.warning)
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
        Style::default().fg(theme.muted),
    )));
    if report.checks_total > 0 {
        out.push(Line::from(Span::styled(
            format!(
                "  {}",
                t.completion_verified
                    .replacen("{}", &report.checks_passed.to_string(), 1)
                    .replacen("{}", &report.checks_total.to_string(), 1)
            ),
            Style::default().fg(theme.muted),
        )));
    }
    out.push(Line::from(Span::styled(
        format!("  {}", t.completion_diff_hint),
        Style::default().fg(theme.muted),
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
    let indent = " ".repeat(prefix.chars().count());
    let inner = width.saturating_sub(prefix.chars().count()).max(1);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;
    use crate::theme::Theme;
    use crate::transcript::{ToolCallBlock, ToolGroupBlock};
    use leveler_client_protocol::ToolCallId;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn item_render_hides_successful_update_goal() {
        let theme = Theme::no_color();
        let t = Locale::Zh.text();
        let item = TranscriptItem::ToolGroup(ToolGroupBlock {
            calls: vec![ToolCallBlock {
                id: ToolCallId::new("g1"),
                name: "update_goal".into(),
                arguments: r#"{"status":"complete","summary":"用户询问这是什么项目，已回答"}"#
                    .into(),
                status: ToolStatus::Ok,
                preview: Some("Goal resolved.".into()),
                duration_ms: Some(1),
                parallel: false,
            }],
            open: false,
            expanded: false,
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

    #[test]
    fn no_code_changes_marker_is_calm_and_omits_machine_token() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let item = TranscriptItem::TurnEnd(TurnEndBlock {
            status: TurnEndStatus::Unverified,
            tool_calls: 20,
            elapsed_secs: 88,
            summary: None,
            detail: Some(leveler_client_protocol::REASON_NO_CODE_CHANGES.into()),
        });
        let lines = item_render(&item, &theme, 80, false, t);
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(
            text.contains("◇ 结束 · 未改仓库 · 20 次工具 · 1m 28s"),
            "unexpected marker: {text}"
        );
        assert!(!text.contains("未验证"), "{text}");
        assert!(!text.contains("no_code_changes"), "{text}");
    }

    #[test]
    fn soft_unverified_is_calm_and_does_not_lecture() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let item = TranscriptItem::TurnEnd(TurnEndBlock {
            status: TurnEndStatus::Unverified,
            tool_calls: 3,
            elapsed_secs: 12,
            summary: None,
            detail: Some(leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION.into()),
        });
        let lines = item_render(&item, &theme, 100, false, t);
        assert_eq!(lines.len(), 1, "soft unverified stays one line");
        let text = line_text(&lines[0]);
        assert!(text.contains("完成 · 未自动验证"), "{text}");
        assert!(text.contains("3 次工具"), "{text}");
        // Product philosophy / browser disclaimer must not flood the marker.
        assert!(!text.contains("浏览器预览"), "{text}");
        assert!(!text.contains("未配置适用的自动验证命令"), "{text}");
    }

    #[test]
    fn automatic_verification_marker_is_short_in_english() {
        let theme = Theme::default();
        let t = Locale::En.text();
        let item = TranscriptItem::TurnEnd(TurnEndBlock {
            status: TurnEndStatus::Unverified,
            tool_calls: 0,
            elapsed_secs: 0,
            summary: None,
            detail: Some(leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION.into()),
        });
        let lines = item_render(&item, &theme, 140, false, t);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("done · not auto-verified"), "{text}");
        assert!(!text.contains("browser preview"), "{text}");
        assert!(
            !text.contains("no applicable automatic verification"),
            "{text}"
        );
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

    #[test]
    fn turn_end_marker_distinguishes_all_terminal_states() {
        let completed = turn_end_text(TurnEndStatus::Completed, None);
        assert!(completed.contains("✓ 任务已完成"), "{completed}");
        let answered = turn_end_text(TurnEndStatus::Answered, None);
        assert!(answered.contains("✓ 任务已完成"), "{answered}");
        let truncated = turn_end_text(TurnEndStatus::Truncated, Some("context limit"));
        assert!(truncated.contains("⚠ 已完成，但有警告"), "{truncated}");
        // Budget/loop/stall incompletes read as "未完成", not the old "被阻塞".
        let incomplete = turn_end_text(TurnEndStatus::Incomplete, Some("预算已耗尽"));
        assert!(incomplete.contains("⚠ 未完成"), "{incomplete}");
        // A failed verification gate reads as "验证未通过" with just the gate name,
        // NOT "被阻塞 · failed gate(s): cargo test" (the false-blocked UX).
        let gate = turn_end_text(TurnEndStatus::Incomplete, Some("failed gate(s): cargo test"));
        assert!(gate.contains("⚠ 验证未通过"), "{gate}");
        assert!(gate.contains("cargo test"), "{gate}");
        assert!(!gate.contains("被阻塞"), "{gate}");
        assert!(!gate.contains("failed gate(s)"), "{gate}");
        let unverified = turn_end_text(TurnEndStatus::Unverified, Some("verify failed"));
        assert!(unverified.contains("⚠ 已完成，但有警告"), "{unverified}");
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
        assert_eq!(stats.style.fg, Some(theme.muted), "{stats:?}");
        assert_eq!(lines[0].spans[1].style.fg, Some(theme.error));
    }

    fn sub_agent(id: &str, nickname: &str, status: ToolStatus) -> crate::transcript::SubAgentBlock {
        crate::transcript::SubAgentBlock {
            id: id.into(),
            nickname: nickname.into(),
            role: "explorer".into(),
            status,
            detail: if status == ToolStatus::Failed {
                "Reached the 6-round limit before finishing.".into()
            } else {
                "done".into()
            },
            progress: Default::default(),
            recent_step: None,
            started_elapsed_secs: 0,
        }
    }

    #[test]
    fn sub_agent_tree_shows_recent_tool_step_while_running() {
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
        assert!(text.contains("list_files"), "{text}");
        assert!(text.contains("grep ✓"), "{text}");
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
        assert!(text.contains("↑ 3.6k · ↓ 240"), "{text}");
        assert!(text.contains("├─ Euclid"), "{text}");
        assert!(text.contains("└─ Newton"), "{text}");
        assert!(text.contains("进行中"), "{text}");
        assert!(text.contains("等待执行"), "{text}");
    }

    #[test]
    fn running_sub_agents_show_their_own_elapsed_time() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let mut a = sub_agent("agent-1", "Euclid", ToolStatus::Running);
        a.started_elapsed_secs = 3; // started 3s into the turn
        let mut b = sub_agent("agent-2", "Newton", ToolStatus::Running);
        b.started_elapsed_secs = 10;
        // Turn is now 15s in: Euclid has run 12s, Newton 5s.
        let lines = sub_agent_tree_lines(&[&a, &b], &theme, 100, t, 15);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("12s"), "Euclid elapsed missing: {text}");
        assert!(text.contains("5s"), "Newton elapsed missing: {text}");
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
        assert!(text.contains("├─ Euclid"), "{text}");
        assert!(text.contains("↑ 87k"), "{text}");
        assert!(!text.contains("└─ Newton  已完成"), "{text}");
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
        assert!(text.contains("├─ Euclid"), "{text}");
        assert!(text.contains("✓ 已完成"), "{text}");
        assert!(text.contains("✗ 超时"), "{text}");
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
            checks_passed: 4,
            checks_total: 5,
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
        assert!(zh.contains("验证 4/5 通过"), "{zh}");
        assert!(zh.contains("/diff 查看改动"), "{zh}");

        let en = item_render(&item, &theme, 120, false, Locale::En.text())
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(en.contains("✓ Task completed"), "{en}");
        assert!(en.contains("3 files changed"), "{en}");
        assert!(en.contains("verification 4/5 passed"), "{en}");
        assert!(en.contains("/diff to view changes"), "{en}");
    }
}
