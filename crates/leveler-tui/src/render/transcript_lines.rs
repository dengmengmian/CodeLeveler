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
            // A continuous accent left-bar + bold text marks a user turn clearly
            // apart from the assistant's "●" bullet + normal-weight prose.
            let bar = Style::default()
                .fg(theme.accent)
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
        TranscriptItem::SubAgent(block) => sub_agent_lines(block, theme, wrap_width, &mut out, t),
        TranscriptItem::Completion(report) => completion_lines(report, theme, &mut out),
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
    // Soft unverified (no gate / no edits) is a calm finish, not a warning.
    let (mut label, color) = match block.status {
        TurnEndStatus::Completed => (t.turn_completed.to_string(), theme.success),
        TurnEndStatus::Answered => (t.turn_answered.to_string(), theme.success),
        TurnEndStatus::Truncated => (t.turn_truncated.to_string(), theme.warning),
        TurnEndStatus::Incomplete => (t.turn_incomplete.to_string(), theme.warning),
        TurnEndStatus::Unverified if no_code_changes => {
            (t.turn_no_code_changes.to_string(), theme.success)
        }
        TurnEndStatus::Unverified if no_auto_verify => {
            (t.turn_unverified.to_string(), theme.success)
        }
        TurnEndStatus::Unverified => (t.turn_unverified.to_string(), theme.muted),
        TurnEndStatus::Failed => (t.turn_failed.to_string(), theme.error),
        TurnEndStatus::Cancelled => (t.turn_cancelled.to_string(), theme.warning),
    };
    if matches!(
        block.status,
        TurnEndStatus::Completed | TurnEndStatus::Answered | TurnEndStatus::Unverified
    ) {
        if block.tool_calls > 0 {
            label.push_str(
                &t.tool_calls_n
                    .replacen("{}", &block.tool_calls.to_string(), 1),
            );
        }
        if block.elapsed_secs > 0 {
            label.push_str(&format!(
                " · {}",
                crate::status_line::fmt_elapsed(block.elapsed_secs)
            ));
        }
        if let Some(summary) = &block.summary {
            label.push_str(&format!(" · {summary}"));
        }
    }
    // Soft machine tokens are already folded into the label — do not re-append.
    // Other incomplete reasons stay on the marker, but keep them short.
    let folded = no_code_changes || no_auto_verify;
    if let Some(detail) = &block.detail {
        let d = localized_turn_detail(detail, t);
        if !folded && !d.is_empty() {
            label.push_str(" · ");
            label.push_str(d);
        }
    }
    let lead = "── ";
    let used = UnicodeWidthStr::width(lead) + UnicodeWidthStr::width(label.as_str()) + 1;
    let tail = "─".repeat(width.saturating_sub(used));
    out.push(Line::from(vec![
        Span::styled(lead, Style::default().fg(theme.border)),
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
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
) {
    let (glyph, color) = match block.status {
        ToolStatus::Running => ("↗", theme.accent),
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
    // Consecutive sub-agents form one visual batch. Tool calls are already
    // represented by a single ToolGroup transcript item.
    let both_agents =
        matches!(prev, TranscriptItem::SubAgent(_)) && matches!(next, TranscriptItem::SubAgent(_));
    !both_agents
}

/// The completion report block (spec §23).
fn completion_lines(report: &UiCompletionReport, theme: &Theme, out: &mut Vec<Line<'static>>) {
    let (glyph, color) = if report.success {
        ("✓", theme.success)
    } else {
        ("✗", theme.warning)
    };
    out.push(Line::from(Span::styled(
        format!("{glyph} 任务已完成"),
        Style::default().fg(color),
    )));
    out.push(Line::from(Span::styled(
        format!(
            "  修改 {} 个文件  +{} / -{}",
            report.files_changed, report.added, report.removed
        ),
        Style::default().fg(theme.muted),
    )));
    if report.checks_total > 0 {
        out.push(Line::from(Span::styled(
            format!(
                "  验证 {}/{} 通过",
                report.checks_passed, report.checks_total
            ),
            Style::default().fg(theme.muted),
        )));
    }
    out.push(Line::from(Span::styled(
        "  /diff 查看改动",
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
}
