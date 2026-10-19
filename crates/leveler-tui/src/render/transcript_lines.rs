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

/// Render a (possibly streaming) assistant message to full prose lines, plus the
/// count of leading lines that belong to fully-received Markdown blocks and are
/// therefore safe to commit to scrollback progressively. When the message is
/// done, every line is stable. The live tail (last block + streaming "▌" cursor)
/// is everything at or after the returned index.
pub fn assistant_split(
    block: &AssistantBlock,
    theme: &Theme,
    wrap_width: usize,
) -> (Vec<Line<'static>>, usize) {
    let (lines, stable, _) = assistant_body_with_copy(block, theme, wrap_width);
    (lines, stable)
}

/// The message's own wrapped rows, before the bullet gutter: every VISUAL line
/// the text occupies at this width, plus the live "▌" cursor while streaming
/// (which is one of those rows, and so counts against the bound). This is the
/// unit the presentation bound is measured in — never Markdown blocks, never
/// paragraphs, never `\n` counts. The Markdown layout also supplies the
/// semantic copy policy in the same pass; the live cursor row is display-only.
fn assistant_body_with_copy(
    block: &AssistantBlock,
    theme: &Theme,
    wrap_width: usize,
) -> (Vec<Line<'static>>, usize, Vec<crate::selection::PlainRow>) {
    // Prose uses the full conversation width without a role gutter.
    let inner = wrap_width.max(1);
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
    let (mut lines, mut copy, mut stable) = doc.to_lines_with_copy(inner, theme, tone);
    if block.done {
        // A finished message is fully stable.
        stable = lines.len();
    } else {
        // Streaming cursor is part of the live tail.
        lines.push(Line::from(Span::styled(
            "▌",
            Style::default().fg(theme.text.secondary),
        )));
        copy.push(crate::selection::PlainRow {
            skip: true,
            ..Default::default()
        });
    }
    (lines, stable, copy)
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

/// [`assistant_render`] plus the copy policy produced by the same layout pass.
pub(crate) fn assistant_render_with_copy(
    block: &AssistantBlock,
    theme: &Theme,
    wrap_width: usize,
) -> (Vec<Line<'static>>, Vec<crate::selection::PlainRow>) {
    let (lines, _, copy) = assistant_body_with_copy(block, theme, wrap_width);
    (lines, copy)
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

/// Display rows retained in a live reasoning preview (header excluded).
const THOUGHT_PREVIEW_LINES: usize = 3;

/// Render one reasoning segment as a Thought.
///
/// The header states the segment's real state — `Thinking…` while streaming,
/// `Thought · 2.8s` when the runtime reported a clean boundary, and
/// `Thought interrupted after 8.2s` when it did not.
///
/// The body follows the entry's own [`DisplayMode`]: `Collapsed` paints the
/// header alone (a finished segment's default), `Truncated` paints a bounded
/// tail (a live segment's preview, and the floor a manual collapse lands on
/// while running), and `Expanded` paints all of it. The body is the provider's
/// own reasoning text under a `│` gutter; the gutter never extends past the
/// body, so the Thought can never look like it owns the entry after it.
/// A live segment without text gets a header; a settled empty segment is hidden.
/// Reasoning is not markdown, so the body is wrapped verbatim rather than
/// parsed.
///
/// A LIVE segment wears the live ink on its bullet and header
/// ([`thinking_ink`]); a settled one recedes to plain secondary ink. The
/// reasoning BODY keeps its own muted ink either way, and the rail's moving
/// highlight is painted later, over the frame's window — see
/// [`paint_reasoning_rail`].
pub fn thought_lines(
    block: &ThoughtBlock,
    theme: &Theme,
    wrap_width: usize,
    t: &crate::i18n::UiText,
) -> Vec<Line<'static>> {
    thought_lines_with_copy(block, theme, wrap_width, t).0
}

/// [`thought_lines`] plus the copy policy for every painted row: the header
/// (state + duration) is display metadata, the `│ ` rail is a gutter, and the
/// reasoning text itself is the semantic content.
pub(crate) fn thought_lines_with_copy(
    block: &ThoughtBlock,
    theme: &Theme,
    wrap_width: usize,
    t: &crate::i18n::UiText,
) -> (Vec<Line<'static>>, Vec<crate::selection::PlainRow>) {
    if block.done && block.text.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }
    let header_style = Style::default()
        .fg(thinking_ink(block.done, theme))
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
        Span::styled("◆ ", Style::default().fg(thinking_ink(block.done, theme))),
        Span::styled(header, header_style),
    ])];
    let mut copy = vec![crate::selection::PlainRow {
        skip: true,
        ..Default::default()
    }];
    let inner = wrap_width.saturating_sub(2).max(1);
    let body_style = Style::default().fg(theme.text.muted);
    // A live segment without reasoning gets a header and nothing else.
    if !block.display.is_collapsed() && !block.text.is_empty() {
        let mut body: Vec<(String, Option<String>)> =
            crate::render::text::wrap_with_joiners(&block.text, inner);
        if block.display != DisplayMode::Expanded {
            let omitted = body.len().saturating_sub(THOUGHT_PREVIEW_LINES);
            body.drain(..omitted);
        }
        for (text, joiner) in body {
            out.push(Line::from(vec![
                Span::styled(REASONING_RAIL_GUTTER, body_style),
                Span::styled(text, body_style),
            ]));
            copy.push(crate::selection::PlainRow {
                indent: 2,
                joiner: joiner.map(String::into_boxed_str),
                ..Default::default()
            });
        }
    }
    (out, copy)
}

// ───────────────────────────────────────────────────────────────────────────
// Live rail animation
// ───────────────────────────────────────────────────────────────────────────
//
// A running Thought's rail carries a short highlight that sweeps from the top
// of the body to the bottom and loops. It is a STYLE-ONLY overlay over lines
// that were already wrapped: no span is added, removed, reordered or
// re-texted, so the block's height, its wrapping, the scroll anchor and every
// downstream hit row are untouched — and because it runs on the painted window
// rather than inside the memoized projection, history never re-wraps and no
// cache entry is invalidated.

/// The gutter a Thought body rides. Its presence as a line's leading span is
/// what identifies a rail row; nothing else paints `│ ` at line start.
pub const REASONING_RAIL_GUTTER: &str = "│ ";

/// Rows of a live Thought's rail the moving highlight covers.
pub const REASONING_RAIL_SEGMENT_ROWS: usize = 2;

/// One full top-to-bottom sweep. Inside the 1.2–1.8s band the motion was
/// specified in: slow enough to read as "still working", never a flicker.
pub const REASONING_RAIL_PERIOD: std::time::Duration = std::time::Duration::from_millis(1500);

/// The live ink a running Thought wears: its bullet, its header and the rail's
/// moving highlight. It is the app's existing "work in flight" accent — the
/// same token a running tool action and the status line's live label use — so
/// a live Thought cannot introduce a second, competing blue.
fn thinking_ink(done: bool, theme: &Theme) -> ratatui::style::Color {
    if done {
        theme.text.secondary
    } else {
        theme.accent.secondary
    }
}

/// Whether this line carries a Thought rail.
pub fn is_reasoning_rail_row(line: &Line<'static>) -> bool {
    line.spans
        .first()
        .is_some_and(|span| span.content.as_ref() == REASONING_RAIL_GUTTER)
}

/// Window-relative indices of the rail rows among `lines`, in paint order.
pub fn reasoning_rail_rows(lines: &[Line<'static>]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_reasoning_rail_row(line))
        .map(|(index, _)| index)
        .collect()
}

/// The moving highlight as a half-open row range over a body of `rows` rail
/// rows, at motion-clock time `phase`.
///
/// The range sweeps the whole rail exactly once per [`REASONING_RAIL_PERIOD`],
/// spending one slot on each legal head position so the bottom is reached
/// before the loop restarts. A body no taller than the segment has nowhere to
/// travel and holds a still highlight rather than flickering.
pub fn reasoning_rail_segment(rows: usize, phase: std::time::Duration) -> std::ops::Range<usize> {
    if rows == 0 {
        return 0..0;
    }
    let height = REASONING_RAIL_SEGMENT_ROWS.min(rows);
    let travel = rows - height;
    if travel == 0 {
        return 0..height;
    }
    let period = REASONING_RAIL_PERIOD.as_millis().max(1) as usize;
    let at = (phase.as_millis() as usize) % period;
    let head = at * (travel + 1) / period;
    head..head + height
}

/// Repaint the highlight over a Thought's rail.
///
/// `rail_rows` are that rail's rows as ABSOLUTE line indices, in paint order —
/// the WHOLE rail, not just its visible part, so the sweep is defined by the
/// rail itself and scrolling never moves the highlight along it. A covered row
/// that `scroll` puts outside `lines` is simply not painted. Only each covered
/// row's rail span is recoloured: the reasoning body keeps its own ink, and no
/// other row moves.
pub fn paint_reasoning_rail(
    lines: &mut [Line<'static>],
    scroll: usize,
    rail_rows: &[usize],
    theme: &Theme,
    phase: std::time::Duration,
) {
    let segment = reasoning_rail_segment(rail_rows.len(), phase);
    let Some(covered) = rail_rows.get(segment) else {
        return;
    };
    for &row in covered {
        let Some(offset) = row.checked_sub(scroll) else {
            continue;
        };
        if let Some(span) = lines
            .get_mut(offset)
            .and_then(|line| line.spans.first_mut())
        {
            span.style = span.style.fg(theme.accent.secondary);
        }
    }
}

/// Columns reserved before every prompt row: the `❯` marker and its gap.
/// The marker itself is painted by the conversation viewport, not written
/// into the line, so copied text never contains it.
pub(crate) const USER_PROMPT_GUTTER: usize = 2;
/// One column kept clear on the right so wrapped glyphs never touch the band.
const USER_PROMPT_RIGHT_PAD: usize = 1;
/// Blank rows painted inside the band above and below the body.
pub(crate) const USER_PROMPT_PAD_ROWS: usize = 1;
/// The prompt's leading icon.
pub(crate) const USER_PROMPT_MARKER: &str = "❯";
const USER_PROMPT_INDENT: &str = "  ";

/// The user's own turn: a marker gutter, one blank row of band padding above
/// and below, and the user's text at primary ink.
///
/// The `❯` marker and the background band are painted over this projection by
/// the conversation viewport. Keeping them out of the line means wrapping,
/// selection columns, URL hit-testing and clipboard text all see exactly the
/// user's own bytes — decoration can never be copied.
///
/// One implementation on purpose. The conversation paints this path, and
/// `item_render` (driven by the presentation tests) paints it too; two copies
/// would drift and the tests would keep passing on the copy nobody runs.
pub(crate) fn push_user_turn(
    text: &str,
    theme: &Theme,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let ink = Style::default().fg(theme.text.primary);
    for _ in 0..USER_PROMPT_PAD_ROWS {
        out.push(Line::from(""));
    }
    for (line, _) in user_wrapped(text, width) {
        out.push(Line::from(vec![
            Span::raw(USER_PROMPT_INDENT),
            Span::styled(line, ink),
        ]));
    }
    for _ in 0..USER_PROMPT_PAD_ROWS {
        out.push(Line::from(""));
    }
}

/// The prompt body's visual rows and their soft-wrap joiners. Shared by the
/// painter and the copy policy so the two cannot drift.
fn user_wrapped(text: &str, width: usize) -> Vec<(String, Option<String>)> {
    let inner = width
        .saturating_sub(USER_PROMPT_GUTTER + USER_PROMPT_RIGHT_PAD)
        .max(1);
    let mut wrapped = crate::render::text::wrap_with_joiners(text, inner);
    if wrapped.is_empty() {
        wrapped.push((String::new(), None));
    }
    wrapped
}

/// Copy policy for [`push_user_turn`]: band padding rows are display-only and
/// the marker gutter is decoration; every body row is the user's own text.
pub(crate) fn user_copy_rows(text: &str, width: usize) -> Vec<crate::selection::PlainRow> {
    let mut rows = Vec::new();
    for _ in 0..USER_PROMPT_PAD_ROWS {
        rows.push(crate::selection::PlainRow {
            skip: true,
            ..Default::default()
        });
    }
    for (_, joiner) in user_wrapped(text, width) {
        rows.push(crate::selection::PlainRow {
            indent: USER_PROMPT_GUTTER as u16,
            joiner: joiner.map(String::into_boxed_str),
            ..Default::default()
        });
    }
    for _ in 0..USER_PROMPT_PAD_ROWS {
        rows.push(crate::selection::PlainRow {
            skip: true,
            ..Default::default()
        });
    }
    rows
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
        TranscriptItem::User(text) => push_user_turn(text, theme, wrap_width, &mut out),
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
                &mut Vec::new(),
                None,
                // Scrollback is a finished session: no card is open over it.
                false,
            ));
        }
        // Scrollback: the agent is finished, so no live elapsed is shown (0).
        TranscriptItem::SubAgent(block) => {
            sub_agent_lines(block, theme, wrap_width, &mut out, t, 0)
        }
        TranscriptItem::UserShell(shell) => {
            out.extend(user_shell_lines(
                shell,
                theme,
                wrap_width,
                t,
                std::time::Duration::ZERO,
                false,
            ));
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
/// a muted machine subtitle; the safe technical projection is always visible.
fn failure_lines(
    block: &crate::transcript::FailureBlock,
    theme: &Theme,
    width: usize,
    out: &mut Vec<Line<'static>>,
    t: &crate::i18n::UiText,
) {
    out.push(Line::from(vec![
        Span::styled(
            if block.display.is_expanded() {
                "▾ ✗ "
            } else {
                "▸ ✗ "
            },
            Style::default().fg(theme.status.error),
        ),
        Span::styled(
            block.title.clone(),
            Style::default()
                .fg(theme.status.error)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if !block.display.is_expanded() {
        return;
    }
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
        // declaration. The status names the turn's own end state — the answer
        // arrived — so it leads the receipt while still not claiming that the
        // task is done.
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

/// This turn's own receipt, joined with ` · `.
///
/// Clock, duration, model steps, tool count, and the provider usage summed
/// for this turn. Nothing here is the footer window gauge (`N%/1M`) or the
/// latest-prompt cache chip. A zero count is omitted: it is not a measurement.
///
/// The tool-count string in the catalog already carries a leading separator,
/// because other rows append it to a label. Here it may stand alone.
fn turn_receipt_stats(block: &TurnEndBlock, t: &crate::i18n::UiText) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(hm) = &block.ended_hm {
        let hm = hm.trim();
        if !hm.is_empty() {
            parts.push(hm.to_string());
        }
    }
    if block.elapsed_secs > 0 {
        parts.push(crate::status_line::fmt_elapsed(block.elapsed_secs));
    }
    if block.rounds > 0 {
        parts.push(t.turn_rounds.replacen("{}", &block.rounds.to_string(), 1));
    }
    if block.tool_calls > 0 {
        let raw = t
            .tool_calls_n
            .replacen("{}", &block.tool_calls.to_string(), 1);
        let piece = raw.trim_start_matches([' ', '·']).trim();
        if !piece.is_empty() {
            parts.push(piece.to_string());
        }
    }
    let tokens = block.input_tokens.saturating_add(block.output_tokens);
    if tokens > 0 {
        parts.push(t.turn_tokens.replacen(
            "{}",
            &crate::status_line::fmt_tokens_compact(tokens),
            1,
        ));
    }
    if block.input_tokens > 0 && block.cached_tokens > 0 {
        let pct = (u64::from(block.cached_tokens) * 100 / u64::from(block.input_tokens)).min(100);
        parts.push(t.turn_cache.replacen("{}", &pct.to_string(), 1));
    }
    if matches!(
        block.status,
        TurnEndStatus::Completed
            | TurnEndStatus::CompletedWithWarnings
            | TurnEndStatus::Answered
            | TurnEndStatus::NoFinalAnswer
    ) && let Some(summary) = &block.summary
        && !summary.is_empty()
    {
        parts.push(summary.clone());
    }
    parts.join(" · ")
}

fn turn_end_lines(
    block: &TurnEndBlock,
    theme: &Theme,
    width: usize,
    out: &mut Vec<Line<'static>>,
    t: &crate::i18n::UiText,
) {
    // A per-turn receipt, marked like the reference closer. Every terminal
    // state leads with its own status word; an answered turn's word says only
    // that the answer arrived, not that the task is done. Neither stretches a
    // rule to the edge of the terminal.
    let (label, color) = turn_end_marker(block, theme, t);
    let mut stats = turn_receipt_stats(block, t);
    let star = "* ";
    let mut detail_below = false;
    if let Some(detail) = &block.detail {
        let d = localized_turn_detail(detail, t);
        if !d.is_empty() {
            let prefix = UnicodeWidthStr::width(star)
                + if label.is_empty() {
                    UnicodeWidthStr::width(stats.as_str())
                } else if stats.is_empty() {
                    UnicodeWidthStr::width(label.as_str())
                } else {
                    UnicodeWidthStr::width(label.as_str())
                        + 3
                        + UnicodeWidthStr::width(stats.as_str())
                };
            let room = width.saturating_sub(prefix).saturating_sub(3);
            if UnicodeWidthStr::width(d) <= room {
                if !stats.is_empty() {
                    stats.push_str(" · ");
                }
                stats.push_str(d);
            } else {
                detail_below = true;
            }
        }
    }
    let secondary = Style::default().fg(theme.text.secondary);
    if label.is_empty() {
        if !stats.is_empty() {
            out.push(Line::from(Span::styled(
                format!("{star}{stats}"),
                secondary,
            )));
        }
    } else {
        let mut spans = vec![
            Span::styled(star.to_string(), secondary),
            Span::styled(
                label,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ];
        if !stats.is_empty() {
            spans.push(Span::styled(format!(" · {stats}"), secondary));
        }
        out.push(Line::from(spans));
    }
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
    // The role says what the child is doing; the nickname or its ordinal says
    // which one it is. A bare nickname leaves the reader with a name and no job.
    match block.id.strip_prefix("agent-").filter(|n| !n.is_empty()) {
        Some(number) => format!("{role} {number}"),
        None => format!("{role} · {}", block.nickname),
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
/// short title while running or its result summary once done. The prompt is
/// not shown.
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
    // While running, `detail` is still the prompt. Show the short title.
    // After settlement, `detail` is the result summary the parent received.
    let displayed_detail = if block.status == ToolStatus::Running {
        block
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string()
    } else {
        let summary = sub_agent_detail(block.detail.trim(), t);
        let summary = summary.trim();
        if summary.is_empty() {
            String::new()
        } else {
            format!("{}{summary}", t.sub_agent_result)
        }
    };
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
/// aggregate header (◌ running / ✓ all done / ⚠ ended with failures) plus the
/// short spawn titles. The child's prompt stays off this surface. A lone
/// agent keeps the classic single-block rendering.
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

    // `task` is the prompt the child runs on. The conversation shows the short
    // titles fixed at spawn, never that prompt.
    let mut titles: Vec<String> = Vec::new();
    for block in blocks {
        let Some(title) = block
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if titles.iter().all(|seen| seen != title) {
            titles.push(title.to_string());
        }
    }
    if titles.is_empty() {
        return;
    }
    let separator = if std::ptr::eq(t, crate::i18n::Locale::En.text()) {
        " · "
    } else {
        "、"
    };
    for row in wrap(&titles.join(separator), wrap_width.saturating_sub(2).max(1)) {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(row, Style::default().fg(theme.text.secondary)),
        ]));
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
/// Copy rows for one [`push_prefixed`] block: the glyph is a gutter, the
/// wrapped text is semantic.
pub(crate) fn prefixed_copy_rows(
    prefix: &str,
    text: &str,
    width: usize,
) -> Vec<crate::selection::PlainRow> {
    let prefix_cols = UnicodeWidthStr::width(prefix);
    let inner = width.saturating_sub(prefix_cols).max(1);
    crate::render::text::wrap_with_joiners(text, inner)
        .into_iter()
        .map(|(_, joiner)| crate::selection::PlainRow {
            indent: prefix_cols as u16,
            joiner: joiner.map(String::into_boxed_str),
            ..Default::default()
        })
        .collect()
}

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

/// Rows of a local command kept visible before its result is folded.
const USER_SHELL_PREVIEW_ROWS: usize = 6;
/// Left status line, two cells on every row of one execution record.
pub(crate) const USER_SHELL_RAIL: &str = "│ ";
/// Body indent after the rail.
const USER_SHELL_INDENT: &str = "  ";
/// A running expansion is a controlled preview; finishing removes the cap.
const USER_SHELL_RUNNING_EXPANDED_ROWS: usize = 2000;
/// Bytes of raw output scanned for a live/collapsed preview.
const USER_SHELL_PREVIEW_BYTES: usize = 16 * 1024;
/// Rightmost cells of a running header that act as the clickable stop.
pub(crate) const USER_SHELL_STOP_ZONE: usize = 10;

/// User shell execution record.
///
/// ```text
/// │ ◆ Running (user) go test · 2s                    ✕ 停止
/// │   $ go test
/// │
/// │   running tests...
/// │   ok
/// ```
///
/// The left rail spans the whole record and carries the execution status; the
/// header names the execution subject (`(user)`), and the exact command follows
/// on its own `$` line. Output keeps the terminal's line structure, with a
/// subtle background difference between stdout and stderr.
pub(crate) fn user_shell_lines(
    shell: &crate::transcript::UserShellBlock,
    theme: &Theme,
    wrap_width: usize,
    t: &crate::i18n::UiText,
    now: std::time::Duration,
    focused: bool,
) -> Vec<Line<'static>> {
    use crate::transcript::UserShellStatus;
    let width = wrap_width.max(1);
    let mut out = Vec::new();
    let status_color = shell_color(shell.status, theme);
    let rail_style = Style::default().fg(status_color);
    let rail = if focused { "→ " } else { USER_SHELL_RAIL };

    let elapsed = matches!(
        shell.status,
        UserShellStatus::Running | UserShellStatus::Background
    );
    let suffix = if elapsed {
        let elapsed_ms = (now.as_millis() as i64 - shell.started_motion_ms).max(0);
        let secs = (elapsed_ms / 1000) as u64;
        if secs == 0 {
            String::new()
        } else {
            format!(" · {secs}s")
        }
    } else {
        shell
            .duration_ms
            .map(|ms| {
                format!(
                    " · {}",
                    crate::presentation::disclosure::format_duration(ms)
                )
            })
            .unwrap_or_default()
    };

    let (glyph, title) = match shell.status {
        UserShellStatus::Running => ("◆ ", t.user_shell_running_label),
        UserShellStatus::Background => ("↗ ", t.user_shell_background_label),
        UserShellStatus::Success => ("◆ ", t.user_shell_ran_label),
        UserShellStatus::Failed => ("✗ ", t.user_shell_ran_label),
        UserShellStatus::Cancelled => ("⊘ ", t.user_shell_cancelled_label),
        UserShellStatus::Unknown => ("? ", t.user_shell_ran_label),
    };
    let command_line = shell.command.replace('\n', " ");
    let mut header = vec![
        Span::styled(rail.to_string(), rail_style),
        Span::styled(glyph.to_string(), Style::default().fg(status_color)),
        Span::styled(
            format!("{title} "),
            Style::default()
                .fg(
                    if matches!(
                        shell.status,
                        UserShellStatus::Running | UserShellStatus::Background
                    ) {
                        theme.text.primary
                    } else {
                        theme.text.secondary
                    },
                )
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let mut facts = suffix.clone();
    if shell.status == UserShellStatus::Failed
        && let Some(code) = shell.exit_code
        && code != 0
    {
        facts.push_str(&format!(
            " · {}",
            t.user_shell_exit_code.replace("{}", &code.to_string())
        ));
    }
    if shell.status == UserShellStatus::Cancelled {
        facts.push_str(&format!(" · {}", t.user_shell_cancelled));
    }
    if shell.status == UserShellStatus::Unknown {
        facts.push_str(&format!(" · {}", t.shell_status_unknown));
    }
    let running = shell.status == UserShellStatus::Running;
    let stop = running.then_some(t.user_shell_stop);
    let background = running.then_some(t.user_shell_background);
    let stop_label = stop.map(|s| format!(" · ✕ {s}")).unwrap_or_default();
    let background_label = background.map(|s| format!(" · ↗ {s}")).unwrap_or_default();
    let controls_w = UnicodeWidthStr::width(stop_label.as_str())
        + UnicodeWidthStr::width(background_label.as_str());
    let command_room = width.saturating_sub(
        UnicodeWidthStr::width(rail)
            + UnicodeWidthStr::width(glyph)
            + UnicodeWidthStr::width(title)
            + 1
            + UnicodeWidthStr::width(facts.as_str())
            + controls_w,
    );
    header.push(Span::styled(
        crate::render::truncate_display(&command_line, command_room.max(1)),
        Style::default().fg(theme.text.primary),
    ));
    header.push(Span::styled(facts, Style::default().fg(theme.text.muted)));
    if running {
        let used: usize = header
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let pad = width.saturating_sub(used + controls_w);
        if pad > 0 {
            header.push(Span::raw(" ".repeat(pad)));
        }
        header.push(Span::styled(
            background_label,
            Style::default().fg(theme.text.secondary),
        ));
        header.push(Span::styled(
            stop_label,
            Style::default().fg(theme.text.secondary),
        ));
    }
    out.push(clip_spans(header, width));

    // The exact command always has its own `$` line, wrapped, so the header
    // never has to be the place the full command lives.
    push_shell_command_rows(&mut out, shell, rail_style, theme, width);
    if !shell.output.is_empty() {
        out.push(Line::from(Span::styled(rail.to_string(), rail_style)));
        push_shell_output_rows(&mut out, shell, rail_style, theme, t, width);
    }
    if shell.status == UserShellStatus::Failed {
        // The failure summary stays visible even when the (folded) output is
        // longer than the preview and the error sits outside it.
        if !shell.expanded
            && let Some(error) = crate::activity_stream::shell_failure_line(&shell.output)
        {
            push_shell_body(
                &mut out,
                rail,
                rail_style,
                &error,
                Style::default().fg(theme.shell.error),
                width,
                theme,
            );
        }
        if let Some(code) = shell.exit_code {
            push_shell_body(
                &mut out,
                rail,
                rail_style,
                &format!("exit code {code}"),
                Style::default()
                    .fg(theme.shell.error)
                    .add_modifier(Modifier::BOLD),
                width,
                theme,
            );
        }
    }
    out
}

fn shell_color(status: crate::transcript::UserShellStatus, theme: &Theme) -> ratatui::style::Color {
    use crate::transcript::UserShellStatus;
    match status {
        UserShellStatus::Running => theme.shell.running,
        // The same ink the background Activity rows use for a running task.
        UserShellStatus::Background => theme.accent.primary,
        UserShellStatus::Success => theme.shell.success,
        UserShellStatus::Failed => theme.shell.error,
        UserShellStatus::Cancelled => theme.shell.cancelled,
        UserShellStatus::Unknown => theme.shell.cancelled,
    }
}

fn push_shell_command_rows(
    out: &mut Vec<Line<'static>>,
    shell: &crate::transcript::UserShellBlock,
    rail_style: Style,
    theme: &Theme,
    width: usize,
) {
    let room = width
        .saturating_sub(UnicodeWidthStr::width(USER_SHELL_RAIL) + USER_SHELL_INDENT.len())
        .max(1);
    let command = if shell.command.trim().is_empty() {
        "…".to_string()
    } else {
        shell.command.clone()
    };
    let mut first = true;
    for logical in command.split('\n') {
        for row in crate::shell_render::wrap_terminal_line(logical, room) {
            let lead = if first { "$ " } else { "  " };
            first = false;
            out.push(Line::from(vec![
                Span::styled(USER_SHELL_RAIL.to_string(), rail_style),
                Span::styled(
                    format!("{USER_SHELL_INDENT}{lead}{row}"),
                    Style::default().fg(theme.text.secondary),
                ),
            ]));
        }
    }
}

fn push_shell_output_rows(
    out: &mut Vec<Line<'static>>,
    shell: &crate::transcript::UserShellBlock,
    rail_style: Style,
    theme: &Theme,
    t: &crate::i18n::UiText,
    width: usize,
) {
    use crate::transcript::UserShellStatus;
    let running = shell.status == UserShellStatus::Running;
    let source = if shell.expanded {
        shell.output.as_str()
    } else {
        crate::shell_render::tail_source(&shell.output, USER_SHELL_PREVIEW_BYTES)
    };
    let mut lines = crate::shell_render::render_output(source, &shell.output_marks);
    let hidden_source = !shell.expanded && source.len() < shell.output.len();
    let cap = if shell.expanded {
        if running {
            USER_SHELL_RUNNING_EXPANDED_ROWS
        } else {
            usize::MAX
        }
    } else {
        USER_SHELL_PREVIEW_ROWS
    };
    let hidden_rows = lines.len().saturating_sub(cap);
    if hidden_rows > 0 {
        lines.drain(..hidden_rows);
    }
    if hidden_source || hidden_rows > 0 {
        let marker = if running {
            t.shell_running_expanded_hidden
                .replace("{}", &lines.len().to_string())
        } else {
            t.shell_output_earlier_hidden.to_string()
        };
        push_shell_body(
            out,
            USER_SHELL_RAIL,
            rail_style,
            &marker,
            Style::default().fg(theme.text.muted),
            width,
            theme,
        );
    }
    if shell.output_truncated {
        push_shell_body(
            out,
            USER_SHELL_RAIL,
            rail_style,
            t.shell_truncated,
            Style::default().fg(theme.text.muted),
            width,
            theme,
        );
    }
    let room = width
        .saturating_sub(UnicodeWidthStr::width(USER_SHELL_RAIL) + USER_SHELL_INDENT.len())
        .max(1);
    for line in lines {
        let text = crate::render::sanitize_terminal_line(&line.text);
        let bg = if line.stderr {
            theme.surface.elevated
        } else {
            theme.surface.input
        };
        let style = Style::default().fg(theme.text.secondary).bg(bg);
        for row in crate::shell_render::wrap_terminal_line(&text, room) {
            let line = if row.is_empty() {
                Line::from(Span::styled(USER_SHELL_RAIL.to_string(), rail_style))
            } else {
                Line::from(vec![
                    Span::styled(USER_SHELL_RAIL.to_string(), rail_style),
                    Span::styled(format!("{USER_SHELL_INDENT}{row}"), style),
                ])
            };
            out.push(line);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_shell_body(
    out: &mut Vec<Line<'static>>,
    rail: &str,
    rail_style: Style,
    text: &str,
    style: Style,
    width: usize,
    _theme: &Theme,
) {
    let room = width
        .saturating_sub(UnicodeWidthStr::width(rail) + USER_SHELL_INDENT.len())
        .max(1);
    for row in crate::shell_render::wrap_terminal_line(text, room) {
        out.push(Line::from(vec![
            Span::styled(rail.to_string(), rail_style),
            Span::styled(format!("{USER_SHELL_INDENT}{row}"), style),
        ]));
    }
}

/// Cut a span list at `width` display cells so a wide header can never paint
/// past the conversation's right edge.
fn clip_spans(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut used = 0usize;
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        let span_w = UnicodeWidthStr::width(span.content.as_ref());
        if used + span_w <= width {
            used += span_w;
            out.push(span);
            continue;
        }
        let mut cut = String::new();
        for ch in span.content.chars() {
            let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + ch_w > width {
                break;
            }
            used += ch_w;
            cut.push(ch);
        }
        if !cut.is_empty() {
            out.push(Span::styled(cut, span.style));
        }
        break;
    }
    Line::from(out)
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::i18n::Locale;
    use crate::theme::Theme;
    use crate::transcript::{AssistantKind, ToolCallBlock, ToolGroupBlock, UserShellStatus};
    use leveler_client_protocol::ToolCallId;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The prompt owns a marker gutter and one blank row above and below.
    /// The `❯` itself is paint-time decoration: line content is only the
    /// indent plus the user's own text, so copying can never pick it up.
    #[test]
    fn a_user_turn_reserves_a_marker_gutter_and_vertical_padding() {
        let theme = Theme::dark();
        let t = Locale::Zh.text();
        let lines = item_render(&TranscriptItem::User("hello".into()), &theme, 40, false, t);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["", "  hello", ""], "prompt rows");
        assert!(
            !text.iter().any(|row| row.contains('❯')),
            "the marker is painted, never line content: {text:?}"
        );
    }

    /// Every visual row starts after the same gutter, including an empty
    /// logical line and wrapped continuations.
    #[test]
    fn every_user_row_starts_after_the_marker_gutter() {
        let theme = Theme::dark();
        let t = Locale::Zh.text();
        let lines = item_render(
            &TranscriptItem::User("alpha\n\nbeta".into()),
            &theme,
            20,
            false,
            t,
        );
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["", "  alpha", "  ", "  beta", ""], "prompt rows");
    }

    /// The body wraps inside the band: the gutter is reserved and one column
    /// stays clear on the right so glyphs never touch the band edge.
    #[test]
    fn a_wrapped_prompt_keeps_its_right_padding() {
        let theme = Theme::dark();
        let t = Locale::Zh.text();
        let text = "这是一段足够长的用户输入，用来确认窄终端下自动换行仍然留出右边距。";
        let lines = item_render(&TranscriptItem::User(text.into()), &theme, 20, false, t);
        for row in &lines {
            let width = unicode_width::UnicodeWidthStr::width(line_text(row).as_str());
            assert!(
                width <= 19,
                "row must stay inside the band: {:?}",
                line_text(row)
            );
        }
    }

    #[test]
    fn terminal_failure_displays_technical_error_without_disclosure() {
        let mut block = crate::transcript::FailureBlock {
            display: crate::fold::DisplayMode::Collapsed,
            title: "请求失败".into(),
            summary: "无法连接模型服务。".into(),
            subtitle: Some("network · 可继续".into()),
            detail: "原因        connection closed".into(),
        };
        let mut lines = Vec::new();
        failure_lines(&block, &Theme::dark(), 80, &mut lines, Locale::Zh.text());
        assert_eq!(lines.len(), 1);
        assert!(!line_text(&lines[0]).contains("connection closed"));
        block.display = crate::fold::DisplayMode::Expanded;
        lines.clear();
        failure_lines(&block, &Theme::dark(), 80, &mut lines, Locale::Zh.text());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            text.contains("connection closed"),
            "the error must be immediately readable: {text}"
        );
        assert!(
            !text.contains("Ctrl+O"),
            "failure has no disclosure action: {text}"
        );
    }

    /// A user shell whose stop could not be confirmed reads as unknown —
    /// never failed, never cancelled.
    #[test]
    fn a_user_shell_with_an_unconfirmed_stop_reads_as_unknown() {
        use crate::transcript::{UserShellBlock, UserShellStatus};
        let status = UserShellStatus::from_wire("unknown");
        let shell = UserShellBlock {
            id: leveler_core::UserShellId::new("ush-1"),
            task_id: None,
            command: "sleep 30".into(),
            cwd: "/repo".into(),
            status,
            output: String::new(),
            output_marks: Vec::new(),
            output_truncated: false,
            exit_code: None,
            duration_ms: Some(2_000),
            started_motion_ms: 0,
            expanded: false,
            expanded_pinned: false,
        };
        let row: String = user_shell_lines(
            &shell,
            &Theme::no_color(),
            80,
            Locale::Zh.text(),
            std::time::Duration::ZERO,
            false,
        )
        .iter()
        .map(line_text)
        .collect();
        assert!(row.contains("状态未知"), "{row:?}");
        assert!(!row.contains('✗') && !row.contains("已取消"), "{row:?}");
    }

    /// A user shell is an inline execution record: a status line, the
    /// execution subject `(user)`, and the exact command on its own `$` row.
    #[test]
    fn a_user_shell_record_names_its_subject_and_command() {
        let shell = shell_block(UserShellStatus::Success, "ok", false);
        let rows = shell_rows(&shell, 80);
        assert!(rows[0].contains('│'), "left status rail: {rows:?}");
        assert!(rows[0].contains("Ran (user)"), "{rows:?}");
        assert!(rows[1].contains("$ git status"), "{rows:?}");
        assert!(!rows[0].contains('↗'), "{rows:?}");
    }

    /// Every finished local command carries the runtime's measured duration,
    /// including one below the shared header's 100ms noise floor.
    #[test]
    fn a_fast_user_shell_still_reports_its_measured_duration() {
        let mut shell = shell_block(UserShellStatus::Success, "ok\n", false);
        shell.duration_ms = Some(99);
        let rows = shell_rows(&shell, 80);
        assert!(rows[0].contains("0.1s"), "{rows:?}");
    }

    /// A local command is user-authored output, not agent exploration: its
    /// result is visible in the conversation without opening anything.
    #[test]
    fn a_finished_user_shell_shows_its_output_inline_by_default() {
        let shell = shell_block(
            UserShellStatus::Success,
            "On branch main\nYour branch is up to date.\n\nnothing to commit, working tree clean\n",
            false,
        );
        let rows = shell_rows(&shell, 80);
        let text = rows.join("\n");
        assert!(text.contains("On branch main"), "{text}");
        assert!(
            text.contains("nothing to commit, working tree clean"),
            "{text}"
        );
    }

    /// While the command runs, its live tail is in the conversation — the row
    /// updates in place instead of accumulating a second result elsewhere.
    #[test]
    fn a_running_user_shell_shows_a_live_tail() {
        let mut shell = shell_block(
            UserShellStatus::Running,
            "Compiling leveler-runtime\nCompiling leveler-tui\n\nrunning 128 tests\n",
            false,
        );
        shell.duration_ms = None;
        let rows = shell_rows_at(&shell, 80, std::time::Duration::from_secs(3));
        let text = rows.join("\n");
        assert!(text.contains("running 128 tests"), "{text}");
        assert!(text.contains("3s"), "live elapsed is shown: {text}");
    }

    /// A long result keeps its tail visible by default and says how many rows
    /// are folded away — the cut is disclosed, never silent.
    #[test]
    fn a_collapsed_user_shell_marks_the_lines_it_is_not_showing() {
        let output: String = (1..=20).map(|n| format!("line-{n:02}\n")).collect();
        let shell = shell_block(UserShellStatus::Success, &output, false);
        let rows = shell_rows(&shell, 80);
        let text = rows.join("\n");
        assert!(text.contains("line-20"), "tail stays visible: {text}");
        assert!(
            text.contains("更早的输出已折叠"),
            "the folded rows are named: {text}"
        );
    }

    /// Expanded is the full retained record: every line, and long lines wrap
    /// instead of being cut at the right edge.
    #[test]
    fn expanding_a_user_shell_keeps_every_retained_line_and_wraps_long_ones() {
        let long = "字".repeat(120);
        let mut output: String = (1..=30).map(|n| format!("row-{n:02}\n")).collect();
        output.push_str(&long);
        output.push('\n');
        let mut shell = shell_block(UserShellStatus::Success, &output, true);
        shell.duration_ms = Some(1_500);
        let rows = shell_rows(&shell, 48);
        let text = rows.join("\n");
        for n in 1..=30 {
            assert!(text.contains(&format!("row-{n:02}")), "missing row-{n:02}");
        }
        // Rows carry the status rail and body indent; the content itself must
        // survive whole across the wraps.
        let joined: String = rows
            .iter()
            .map(|row| row.trim_start_matches('│').trim_start())
            .collect::<String>()
            .replace(' ', "");
        assert!(
            joined.contains(&long),
            "the long line is wrapped, not truncated: {rows:?}"
        );
        assert!(
            rows.iter()
                .all(|r| UnicodeWidthStr::width(r.as_str()) <= 48),
            "a row overruns the terminal: {rows:?}"
        );
    }

    /// A failed command states its exit code and the line that says what broke
    /// without requiring the reader to expand it first.
    #[test]
    fn a_failed_user_shell_shows_exit_and_error_without_expanding() {
        let mut shell = shell_block(
            UserShellStatus::Failed,
            "running 128 tests\nassertion failed: expected Completed\n",
            false,
        );
        shell.exit_code = Some(101);
        let rows = shell_rows(&shell, 80);
        let text = rows.join("\n");
        assert!(text.contains("exit code 101"), "{text}");
        assert!(text.contains("assertion failed"), "{text}");
    }

    /// The header is a one-line summary; expanding reveals the exact command,
    /// wrapped, so a long or multi-line command is never cut off for good.
    #[test]
    fn a_long_command_is_fully_visible_when_expanded() {
        let command = format!(
            "cargo test --package leveler-tui --test reducer -- --nocapture {}",
            "arg".repeat(40)
        );
        let mut shell = shell_block(UserShellStatus::Success, "ok\n", true);
        shell.command = command.clone();
        let rows = shell_rows(&shell, 48);
        let joined: String = rows
            .iter()
            .map(|row| row.trim_start_matches('│').trim_start())
            .collect::<String>()
            .replace(' ', "");
        assert!(
            joined.contains(&command.replace(' ', "")),
            "the exact command is not reachable: {rows:?}"
        );
    }

    /// A command that printed nothing is its header alone: no invented marker
    /// row, no empty output section.
    #[test]
    fn a_user_shell_with_no_output_is_only_its_header() {
        let shell = shell_block(UserShellStatus::Success, "", false);
        let rows = shell_rows(&shell, 80);
        assert_eq!(rows.len(), 2, "status row + `$ command`: {rows:?}");
        assert!(rows[1].contains("$ git status"), "{rows:?}");
    }

    #[test]
    fn a_running_shell_shows_background_and_stop_controls_in_order() {
        let rows = shell_rows(&shell_block(UserShellStatus::Running, "", false), 100);
        let header = &rows[0];
        let background = header.find("↗ 转后台").expect("background control");
        let stop = header.find("✕ 停止").expect("stop control");
        assert!(
            background < stop,
            "background precedes stop on the right: {header}"
        );
        assert!(header.starts_with("│ "), "{header}");
    }

    #[test]
    fn a_background_shell_has_no_foreground_controls() {
        let rows = shell_rows(&shell_block(UserShellStatus::Background, "", false), 100);
        let header = &rows[0];
        assert!(header.contains("Background (user)"), "{header}");
        assert!(!header.contains("✕ 停止"), "{header}");
        assert!(!header.contains("↗ 转后台"), "{header}");
    }

    fn shell_block(
        status: UserShellStatus,
        output: &str,
        expanded: bool,
    ) -> crate::transcript::UserShellBlock {
        crate::transcript::UserShellBlock {
            id: leveler_core::UserShellId::new("ush-render"),
            task_id: None,
            command: "git status".into(),
            cwd: "/repo".into(),
            status,
            output: output.into(),
            output_marks: Vec::new(),
            output_truncated: false,
            exit_code: Some(0),
            duration_ms: Some(200),
            started_motion_ms: 0,
            expanded,
            expanded_pinned: false,
        }
    }

    fn shell_rows(shell: &crate::transcript::UserShellBlock, width: usize) -> Vec<String> {
        shell_rows_at(shell, width, std::time::Duration::ZERO)
    }

    fn shell_rows_at(
        shell: &crate::transcript::UserShellBlock,
        width: usize,
        now: std::time::Duration,
    ) -> Vec<String> {
        user_shell_lines(
            shell,
            &Theme::no_color(),
            width,
            Locale::Zh.text(),
            now,
            false,
        )
        .iter()
        .map(line_text)
        .collect()
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

    // ---- The live rail: a style-only sweep, never a layout change ----

    /// A running Thought announces itself in the app's live accent, so its
    /// bullet and header match the rail highlight that is moving under it. A
    /// settled Thought recedes to plain secondary ink.
    #[test]
    fn a_live_thought_header_wears_the_live_accent_and_a_settled_one_does_not() {
        let theme = Theme::dark();
        let live = thought("查 catalog", false, None, false);
        let settled = thought("查 catalog", true, Some(2800), false);
        for (block, expected, label) in [
            (&live, theme.accent.secondary, "live"),
            (&settled, theme.text.secondary, "settled"),
        ] {
            let lines = thought_lines(block, &theme, 60, Locale::Zh.text());
            assert_eq!(
                lines[0].spans[0].content.as_ref(),
                "◆ ",
                "{label}: bullet first"
            );
            assert_eq!(lines[0].spans[0].style.fg, Some(expected), "{label} bullet");
            assert_eq!(lines[0].spans[1].style.fg, Some(expected), "{label} header");
        }
    }

    /// The highlight sweeps the whole rail once per period, top to bottom,
    /// and its segment is never taller than the rail it rides.
    #[test]
    fn the_rail_highlight_sweeps_top_to_bottom_once_per_period() {
        let rows = 11;
        let period = REASONING_RAIL_PERIOD;
        let head =
            |ms: u64| reasoning_rail_segment(rows, std::time::Duration::from_millis(ms)).start;
        assert_eq!(head(0), 0, "starts at the top");
        assert_eq!(
            reasoning_rail_segment(rows, period - std::time::Duration::from_millis(1)),
            rows - REASONING_RAIL_SEGMENT_ROWS..rows,
            "reaches the bottom before the loop restarts"
        );
        assert_eq!(head(period.as_millis() as u64), 0, "and loops");
        let mut previous = 0;
        for ms in (0..period.as_millis() as u64).step_by(10) {
            let at = head(ms);
            assert!(at >= previous, "never moves backwards at {ms}ms");
            previous = at;
        }
    }

    /// A rail no taller than the highlight has nowhere to travel: it holds
    /// still rather than flickering on and off.
    #[test]
    fn a_rail_no_taller_than_the_highlight_holds_still() {
        for ms in [0, 750, 1499, 1500, 3000] {
            let phase = std::time::Duration::from_millis(ms);
            assert_eq!(reasoning_rail_segment(2, phase), 0..2, "two rows at {ms}ms");
            assert_eq!(reasoning_rail_segment(1, phase), 0..1, "one row at {ms}ms");
            assert_eq!(reasoning_rail_segment(0, phase), 0..0, "no rail at {ms}ms");
        }
    }

    /// The overlay recolours the rail gutter only. The reasoning body keeps its
    /// own ink, and the block's line count and every line's width are exactly
    /// what the wrap produced — which is what keeps the scroll anchor still.
    #[test]
    fn the_highlight_touches_the_rail_only_and_moves_nothing() {
        let theme = Theme::dark();
        let block = thought(
            "第一行推理。第二行推理。第三行推理。第四行推理。第五行推理。",
            false,
            None,
            false,
        );
        let mut lines = thought_lines(&block, &theme, 24, Locale::Zh.text());
        let rail_rows = reasoning_rail_rows(&lines);
        assert!(
            rail_rows.len() > REASONING_RAIL_SEGMENT_ROWS,
            "a body long enough to show movement: {rail_rows:?}"
        );
        let widths: Vec<usize> = lines.iter().map(Line::width).collect();

        paint_reasoning_rail(&mut lines, 0, &rail_rows, &theme, REASONING_RAIL_PERIOD / 2);

        assert_eq!(
            lines.iter().map(Line::width).collect::<Vec<_>>(),
            widths,
            "no line grew or shrank"
        );
        let covered = reasoning_rail_segment(rail_rows.len(), REASONING_RAIL_PERIOD / 2);
        for (position, &row) in rail_rows.iter().enumerate() {
            let line = &lines[row];
            let expected = if covered.contains(&position) {
                theme.accent.secondary
            } else {
                theme.text.muted
            };
            assert_eq!(line.spans[0].content.as_ref(), REASONING_RAIL_GUTTER);
            assert_eq!(line.spans[0].style.fg, Some(expected), "rail row {row}");
            assert_eq!(
                line.spans[1].style.fg,
                Some(theme.text.muted),
                "the reasoning body keeps its own ink on row {row}"
            );
        }
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(theme.accent.secondary),
            "the header is not part of the sweep"
        );
    }

    /// A settled empty segment has no detail to reveal or history row to paint.
    #[test]
    fn a_settled_thought_without_provider_text_is_hidden() {
        for text in ["", " \n\t"] {
            for interrupted in [false, true] {
                assert!(thought_rows(&thought(text, true, Some(1200), interrupted), 60).is_empty());
            }
        }
        let live = thought_rows(&thought("", false, None, false), 60);
        assert_eq!(live.len(), 1);
        assert!(live[0].contains("思考中"));
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
        assert!(lines[0].contains("思考 · 2.8s"), "{lines:?}");
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
            !lines[0].contains("思考 ·"),
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
    }

    /// THOUGHT-IA-1: while the model reasons, its body is on screen. The
    /// running Thought is the ONE reasoning entry that defaults to showing text.
    #[test]
    fn a_running_thought_paints_its_body() {
        let mut block = thought("正在检查 pricing.rs", false, None, false);
        block.display = crate::fold::DisplayMode::Truncated;
        block.display_pinned = false;
        let lines = thought_rows(&block, 60);
        assert!(lines[0].contains("思考中"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("检查 pricing.rs")),
            "the live body is visible: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains('\u{2502}')),
            "the body rides its own rail: {lines:?}"
        );
    }

    /// THOUGHT-IA-2/3: a completed Thought defaults to its header alone; the
    /// reasoning body is not painted until the reader opens it.
    #[test]
    fn a_finished_thought_defaults_to_a_bare_header() {
        let mut block = thought("body that must not show", true, Some(4100), false);
        block.display = crate::fold::collapse_target(false);
        block.display_pinned = false;
        let lines = thought_rows(&block, 60);
        assert_eq!(lines.len(), 1, "header only: {lines:?}");
        assert!(lines[0].contains("思考 · 4.1s"), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains("body that must not show")),
            "the body is collapsed: {lines:?}"
        );
    }

    /// THOUGHT-IA-4: opening the Thought paints the whole reasoning body.
    #[test]
    fn an_opened_thought_paints_the_whole_body() {
        let mut block = thought(
            "first line\nsecond line\nthird line",
            true,
            Some(2000),
            false,
        );
        block.display = crate::fold::DisplayMode::Expanded;
        block.display_pinned = true;
        let lines = thought_rows(&block, 60);
        for want in ["first line", "second line", "third line"] {
            assert!(lines.iter().any(|l| l.contains(want)), "{want}: {lines:?}");
        }
    }

    /// The two kinds differ in marker GLYPH and in marker INK: a faint `●` for
    /// process narration, the accent `◆` for the answer. A reader scanning a
    /// long turn never has to compare glyph shapes to find the answer.
    #[test]
    fn progress_and_final_use_distinct_markers() {
        let theme = Theme::dark();
        let progress = assistant_render(&block("working", AssistantKind::Progress), &theme, 40);
        let final_answer = assistant_render(&block("done", AssistantKind::Final), &theme, 40);

        assert_eq!(line_text(&progress[0]), "working");
        assert_eq!(line_text(&final_answer[0]), "done");
        assert_eq!(progress[0].spans[0].style.fg, Some(theme.text.secondary));
        assert_eq!(
            final_answer[0].spans[0].style.fg,
            Some(theme.text.final_lead)
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
        // The conclusion is marked by its own ink, not by weight: body text is
        // regular everywhere, so a long answer reads as prose.
        assert!(!lead.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(body.style.fg, Some(theme.text.primary));
        assert_ne!(theme.text.final_lead, theme.text.primary);
    }

    /// Body text is regular, while the things that must stand out still do:
    /// a heading keeps its weight, an emphasised phrase keeps its weight, and
    /// code keeps its own ink. Bold prose is what costs legibility; a heading
    /// that is not marked stops reading as a heading.
    #[test]
    fn headings_and_emphasis_stay_marked_while_body_text_is_regular() {
        let theme = Theme::dark();
        let lines = assistant_render(
            &block(
                "## 小节标题\n\n普通正文，其中 **强调短语** 要突出，还有 `inline_code`。",
                AssistantKind::Final,
            ),
            &theme,
            100,
        );
        let find = |needle: &str| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .find(|span| span.content.contains(needle))
                .unwrap_or_else(|| panic!("{needle} not rendered"))
                .clone()
        };
        let heading = find("小节标题");
        assert!(
            heading.style.add_modifier.contains(Modifier::BOLD),
            "a heading stays a heading: {heading:?}"
        );
        let emphasis = find("强调短语");
        assert!(
            emphasis.style.add_modifier.contains(Modifier::BOLD),
            "explicit emphasis stays emphasised: {emphasis:?}"
        );
        let code = find("inline_code");
        assert_ne!(
            code.style.fg,
            Some(theme.text.primary),
            "inline code keeps its own ink: {code:?}"
        );
        // And the plain prose between them is neither bold nor tinted.
        let prose = find("普通正文");
        assert!(
            !prose.style.add_modifier.contains(Modifier::BOLD),
            "body text is regular: {prose:?}"
        );
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

        assert_eq!(line_text(&lines[0]), "final answer");
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
            ended_hm: None,
            rounds: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
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
            ended_hm: None,
            rounds: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
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
            "an answered turn leads with its own status: {answered}"
        );
        assert!(
            !answered.contains("任务已完成"),
            "an answered turn names the turn's end, not a task verdict: {answered}"
        );
        assert!(
            !answered.contains('─'),
            "the receipt is not a section rule: {answered}"
        );
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
            ended_hm: None,
            rounds: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
        });
        let lines = item_render(&item, &theme, 80, false, t);
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(
            unicode_width::UnicodeWidthStr::width(text.as_str()) < 80,
            "the receipt does not fill the row: {text:?}"
        );
        assert!(!text.contains('─'), "{text}");
        // Verdict keeps the status color. Stats, including the reason, do not.
        let stats = &lines[0].spans[2];
        assert!(stats.content.contains("boom"), "{stats:?}");
        assert!(stats.content.contains("2 次工具"), "{stats:?}");
        assert_eq!(stats.style.fg, Some(theme.text.secondary), "{stats:?}");
        assert_eq!(lines[0].spans[1].style.fg, Some(theme.status.error));
        assert_eq!(lines[0].spans[0].content.as_ref(), "* ");
    }

    #[test]
    fn an_answered_turn_leads_with_its_status_and_then_the_stats() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let item = TranscriptItem::TurnEnd(TurnEndBlock {
            status: TurnEndStatus::Answered,
            tool_calls: 13,
            elapsed_secs: 486,
            summary: None,
            detail: None,
            ended_hm: Some("14:04".into()),
            rounds: 2,
            input_tokens: 3_000,
            output_tokens: 530,
            // 2970 / 3000 = 99%. This is the turn's own cache, not N%/1M.
            cached_tokens: 2_970,
        });
        let lines = item_render(&item, &theme, 120, false, t);
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert_eq!(
            text.trim(),
            "* ✓ 回答结束 · 14:04 · 8m 06s · 2 轮 · 13 次工具 · 3.5k tokens · 本轮缓存 99%"
        );
        assert!(!text.contains("%/"), "not the window gauge: {text}");
        // The status word takes the verdict accent; the receipt stays secondary.
        assert_eq!(lines[0].spans[0].content.as_ref(), "* ");
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.text.secondary));
        assert_eq!(lines[0].spans[1].content.as_ref(), "✓ 回答结束");
        assert_eq!(lines[0].spans[1].style.fg, Some(theme.status.success));
        assert_eq!(lines[0].spans[2].style.fg, Some(theme.text.secondary));
        assert!(lines[0].spans[2].content.starts_with(" · "), "{text}");
    }

    /// A short chat still closes on this turn's clock, duration and steps.
    /// A bare "1s" is not a separator, and a window percentage is not this turn.
    #[test]
    fn a_short_answer_closes_on_this_turn_not_the_window() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let item = TranscriptItem::TurnEnd(TurnEndBlock {
            status: TurnEndStatus::Answered,
            tool_calls: 0,
            elapsed_secs: 1,
            summary: None,
            detail: None,
            ended_hm: Some("14:24".into()),
            rounds: 1,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
        });
        let lines = item_render(&item, &theme, 80, false, t);
        let text = line_text(&lines[0]);
        assert_eq!(text.trim(), "* ✓ 回答结束 · 14:24 · 1s · 1 轮");
        assert!(!text.contains("缓存"), "{text}");
        assert!(!text.contains("%/"), "{text}");
    }

    fn sub_agent(id: &str, nickname: &str, status: ToolStatus) -> crate::transcript::SubAgentBlock {
        crate::transcript::SubAgentBlock {
            id: id.into(),
            nickname: nickname.into(),
            agent_name: None,
            role: "explorer".into(),
            status,
            task: "task".into(),
            title: None,
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
    fn sub_agent_tree_keeps_titles_instead_of_the_prompt_or_live_tool_steps() {
        let theme = Theme::default();
        let t = Locale::Zh.text();
        let mut a = sub_agent("agent-1", "Euclid", ToolStatus::Running);
        a.progress.active = true;
        a.recent_step = Some("list_files".into());
        a.title = Some("摸清事件入口".into());
        a.task = "你是只读探查。目标仓库：/tmp/repo\n1. HTTP 路由在哪里".into();
        let mut b = sub_agent("agent-2", "Newton", ToolStatus::Running);
        b.progress.active = true;
        b.recent_step = Some("grep ✓".into());
        b.title = Some("摸清审查链路".into());
        b.task = "你是只读探查。把完整提示词写进对话".into();
        let lines = sub_agent_tree_lines(&[&a, &b], &theme, 100, t, 0);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("摸清事件入口、摸清审查链路"), "{text}");
        assert!(
            !text.contains("你是只读探查") && !text.contains("目标仓库"),
            "the agent prompt is not conversation text: {text}"
        );
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
        a.title = Some("摸清事件入口".into());
        a.task = "你是只读探查。不要把这段提示词画出来".into();
        let mut b = sub_agent("agent-2", "Newton", ToolStatus::Running);
        b.progress.input_tokens = 2_400;
        b.progress.output_tokens = 160;
        b.title = Some("摸清审查链路".into());
        let lines = sub_agent_tree_lines(&[&a, &b], &theme, 100, t, 0);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("◌ 2 个 agents 正在运行"), "{text}");
        assert!(text.contains("3.8k tokens"), "{text}");
        assert!(text.contains("摸清事件入口"), "{text}");
        assert!(!text.contains("你是只读探查"), "{text}");
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
