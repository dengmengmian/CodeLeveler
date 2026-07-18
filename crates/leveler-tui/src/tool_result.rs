//! Dedicated Conversation components for tool activity and completed output.
//!
//! A completed tool result is not agent prose: its status stays in Summary and
//! its raw preview is only rendered inside the collapsible Details section.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::i18n::{Locale, UiText};
use crate::render::{truncate_display, wrap};
use crate::theme::Theme;
use crate::transcript::{ToolCallBlock, ToolStatus};

#[allow(dead_code)] // alternate running-tool chrome; Conversation uses compact activity lines
pub(crate) fn activity_lines(
    call: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    locale: Locale,
    t: &UiText,
) -> Vec<Line<'static>> {
    debug_assert_eq!(call.status, ToolStatus::Running);
    let action = tool_target(call, locale);
    vec![Line::from(vec![
        Span::styled("⟳ ", Style::default().fg(theme.accent)),
        Span::styled(
            truncate_display(
                &format!("{} · {action}", t.tool_status_running),
                width.saturating_sub(2),
            ),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
    ])]
}

/// Completed Tool Result component: Summary is always visible; Details owns
/// the raw tool output and is only emitted when the owning group is expanded.
pub(crate) fn result_lines(
    call: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    expanded: bool,
    locale: Locale,
    t: &UiText,
) -> Vec<Line<'static>> {
    debug_assert_ne!(call.status, ToolStatus::Running);
    let parsed = ParsedOutput::from_call(call);
    let (glyph, color, status) = match call.status {
        ToolStatus::Ok => ("✓", theme.success, t.tool_status_succeeded),
        ToolStatus::Failed => ("✗", theme.error, t.tool_status_failed),
        ToolStatus::Running => unreachable!("running calls use Tool Activity"),
    };
    let mut summary = format!("{status} · {}", tool_target(call, locale));
    if let Some(exit) = parsed.exit.as_deref() {
        summary.push_str(&format!(" · exit {exit}"));
    }
    if parsed.timed_out {
        summary.push_str(" · timeout");
    }
    if let Some(ms) = call.duration_ms.filter(|ms| *ms >= 100) {
        summary.push_str(&format!(" · {:.1}s", ms as f64 / 1000.0));
    }

    let mut out = vec![Line::from(vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::styled(
            truncate_display(&summary, width.saturating_sub(2)),
            Style::default().fg(theme.text),
        ),
    ])];

    if parsed.content_lines == 0 {
        return out;
    }

    let disclosure = if expanded { "▾" } else { "▸" };
    out.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{disclosure} {}", t.tool_details),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " · {}",
                t.tool_output_lines
                    .replace("{}", &parsed.content_lines.to_string())
            ),
            Style::default().fg(theme.border),
        ),
        Span::styled(
            if expanded { "" } else { " · Ctrl+O" }.to_string(),
            Style::default().fg(theme.border),
        ),
    ]));

    if expanded {
        append_details(&parsed, theme, width, &mut out);
    }
    out
}

fn tool_target(call: &ToolCallBlock, locale: Locale) -> String {
    let action = crate::tool_cell::tool_action_label_for(&call.name, locale);
    let target = crate::tool_cell::tool_summary_pub(&call.name, &call.arguments)
        .replace("**", "")
        .replace('`', "");
    if target.is_empty() || target == "{}" {
        action
    } else {
        format!("{action} {target}")
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedOutput {
    exit: Option<String>,
    timed_out: bool,
    rows: Vec<DetailRow>,
    content_lines: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum DetailRow {
    Section(String),
    Content(String),
}

impl ParsedOutput {
    fn from_call(call: &ToolCallBlock) -> Self {
        let Some(preview) = call.preview.as_deref().filter(|p| !p.trim().is_empty()) else {
            return Self::default();
        };
        if !matches!(call.name.as_str(), "run_command" | "shell_command") {
            let rows = preview
                .lines()
                .map(|line| DetailRow::Content(line.to_string()))
                .collect::<Vec<_>>();
            return Self {
                content_lines: rows.len(),
                rows,
                ..Self::default()
            };
        }

        let mut parsed = Self::default();
        for line in preview.lines() {
            if line == "[timed out]" {
                parsed.timed_out = true;
            } else if let Some(exit) = line.strip_prefix("exit: ") {
                parsed.exit = Some(exit.to_string());
            } else if let Some(section) = line
                .strip_prefix("--- ")
                .and_then(|line| line.strip_suffix(" ---"))
            {
                parsed.rows.push(DetailRow::Section(section.to_string()));
            } else {
                parsed.rows.push(DetailRow::Content(line.to_string()));
                parsed.content_lines += 1;
            }
        }
        parsed
    }
}

fn append_details(
    parsed: &ParsedOutput,
    theme: &Theme,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    const MAX_DETAIL_LINES: usize = 24;
    let inner = width.saturating_sub(6).max(1);
    let mut shown = 0usize;
    for row in &parsed.rows {
        match row {
            DetailRow::Section(label) => out.push(Line::from(Span::styled(
                format!("    {label}"),
                Style::default()
                    .fg(theme.border)
                    .add_modifier(Modifier::BOLD),
            ))),
            DetailRow::Content(content) => {
                for line in wrap(content, inner) {
                    if shown == MAX_DETAIL_LINES {
                        break;
                    }
                    out.push(Line::from(Span::styled(
                        format!("      {line}"),
                        Style::default().fg(theme.muted),
                    )));
                    shown += 1;
                }
                if shown == MAX_DETAIL_LINES {
                    break;
                }
            }
        }
    }
    if parsed.content_lines > shown {
        out.push(Line::from(Span::styled(
            format!("      … {} more lines", parsed.content_lines - shown),
            Style::default().fg(theme.border),
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::ToolCallId;

    fn command(preview: &str) -> ToolCallBlock {
        ToolCallBlock {
            id: ToolCallId::new("command-1"),
            name: "run_command".into(),
            arguments: r#"{"program":"cargo","args":["test"]}"#.into(),
            status: ToolStatus::Ok,
            preview: Some(preview.into()),
            duration_ms: Some(1200),
        }
    }

    #[test]
    fn command_metadata_is_summary_not_detail_content() {
        let parsed = ParsedOutput::from_call(&command(
            "exit: 0\n--- stdout ---\nfirst\nsecond\n--- stderr ---\nwarning\n",
        ));
        assert_eq!(parsed.exit.as_deref(), Some("0"));
        assert_eq!(parsed.content_lines, 3);
        assert!(!parsed.rows.contains(&DetailRow::Content("exit: 0".into())));
    }
}
