//! Overlays: modal decision surfaces layered over the conversation .
//!
//! An overlay captures key input while it is open, but the background keeps
//! processing runtime events (the reducer's `apply_runtime` runs regardless).
//! Dismissal never approves anything.
//!
//! On the conversation screen an overlay renders INLINE in the footer (in place
//! of the composer) so the transcript stays visible; on other screens it draws
//! as a centered modal. Both share the same content builder.

pub mod approval;
pub mod clarification;
pub mod selection;

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

pub use approval::{ApprovalOutcome, ApprovalOverlay};
pub use clarification::{ClarificationOutcome, ClarificationOverlay};
pub use selection::{SelectionModel, SelectionOption, SelectionOutcome};

/// An open overlay. Model and mode pickers share the [`SelectionModel`]; the
/// reducer distinguishes them by variant to build the right command.
#[derive(Debug, Clone)]
pub enum Overlay {
    ModelPicker(Box<SelectionModel>),
    ModePicker(Box<SelectionModel>),
    /// Named TUI palettes (`ion` / `night` / `day`).
    ThemePicker(Box<SelectionModel>),
    Approval(Box<ApprovalOverlay>),
    /// The agent asked the user a question mid-task (spec §35).
    Clarification(Box<ClarificationOverlay>),
    /// Shown when attachments are present but the model has no vision (spec §42).
    UnsupportedMedia(Box<SelectionModel>),
    /// Pick a conversation checkpoint to restore (spec §68).
    CheckpointPicker(Box<SelectionModel>),
}

/// A short label for the status line while an overlay is open.
impl Overlay {
    pub fn status_hint(&self, t: &crate::i18n::UiText) -> &'static str {
        match self {
            Overlay::Approval(_) => t.overlay_approval,
            Overlay::Clarification(_) => t.overlay_clarify,
            Overlay::ModelPicker(_) => t.overlay_model,
            Overlay::ModePicker(_) => t.overlay_mode,
            Overlay::ThemePicker(_) => t.overlay_theme,
            Overlay::UnsupportedMedia(_) => t.overlay_media,
            Overlay::CheckpointPicker(_) => t.overlay_checkpoint,
        }
    }
}

/// The overlay's title, content lines, and — when it has a text input — the
/// cursor position as `(row, display_col)` within those content lines.
pub fn content_lines(
    overlay: &Overlay,
    theme: &Theme,
) -> (String, Vec<Line<'static>>, Option<(usize, usize)>) {
    match overlay {
        Overlay::ModelPicker(model)
        | Overlay::ModePicker(model)
        | Overlay::ThemePicker(model)
        | Overlay::UnsupportedMedia(model)
        | Overlay::CheckpointPicker(model) => {
            let (lines, cursor) = selection_content(model, theme);
            (model.title.clone(), lines, cursor)
        }
        Overlay::Approval(ov) => ("需要权限".to_string(), approval_content(ov, theme), None),
        Overlay::Clarification(ov) => {
            let (lines, cursor) = clarification_content(ov, theme);
            ("需要澄清".to_string(), lines, cursor)
        }
    }
}

/// Draw the active overlay centered over `area` (modal form, used on
/// non-conversation screens).
pub fn render_overlay(frame: &mut Frame, area: Rect, overlay: &Overlay, theme: &Theme) {
    let (title, lines, _) = content_lines(overlay, theme);
    let max_w = match overlay {
        Overlay::Approval(_) | Overlay::Clarification(_) => 68,
        _ => 64,
    };
    render_modal(frame, area, &title, lines, theme, max_w);
}

fn clarification_content(
    ov: &ClarificationOverlay,
    theme: &Theme,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::raw(ov.request.question.clone())));
    lines.push(Line::from(""));
    for (i, opt) in ov.request.options.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!("  {}. ", i + 1), Style::default().fg(theme.accent)),
            Span::raw(opt.clone()),
        ]));
    }
    if !ov.request.options.is_empty() {
        lines.push(Line::from(""));
    }
    let input_row = lines.len();
    let input_col = 2 + UnicodeWidthStr::width(ov.input());
    lines.push(Line::from(vec![
        Span::styled("› ", Style::default().fg(theme.accent)),
        Span::raw(ov.input().to_string()),
    ]));
    lines.push(Line::from(""));
    lines.push(help_line(
        theme,
        "1-9 选项 · 输入自定义 · Enter 提交 · Esc 跳过",
    ));
    (lines, Some((input_row, input_col)))
}

/// A centered modal rect, at most `max_w` wide and `content_h`+chrome tall.
fn modal_rect(area: Rect, max_w: u16, content_h: u16) -> Rect {
    let w = max_w.min(area.width.saturating_sub(4)).max(20);
    let h = (content_h + 2).min(area.height.saturating_sub(2)).max(3);
    let [row] = Layout::vertical([Constraint::Length(h)])
        .flex(Flex::Center)
        .areas(area);
    let [col] = Layout::horizontal([Constraint::Length(w)])
        .flex(Flex::Center)
        .areas(row);
    col
}

fn selection_content(
    model: &SelectionModel,
    theme: &Theme,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let mut lines: Vec<Line> = Vec::new();
    let mut cursor = None;
    if let Some(desc) = &model.description {
        lines.push(Line::from(Span::styled(
            desc.clone(),
            Style::default().fg(theme.muted),
        )));
        lines.push(Line::from(""));
    }
    if model.is_searchable() {
        cursor = Some((lines.len(), 8 + UnicodeWidthStr::width(model.query())));
        lines.push(Line::from(vec![
            Span::styled("Search: ", Style::default().fg(theme.muted)),
            Span::raw(model.query().to_string()),
        ]));
        lines.push(Line::from(""));
    }

    for (pos, (_, opt, is_cursor)) in model.visible_rows().into_iter().enumerate() {
        let prefix = if is_cursor { "› " } else { "  " };
        let number = if model.is_searchable() {
            String::new()
        } else {
            format!("{}. ", pos + 1)
        };
        let base = if opt.is_enabled() {
            if is_cursor {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            }
        } else {
            Style::default().fg(theme.muted)
        };
        let mut spans = vec![Span::styled(format!("{prefix}{number}{}", opt.label), base)];
        if opt.recommended {
            spans.push(Span::styled(
                "  Recommended",
                Style::default().fg(theme.success),
            ));
        }
        if opt.current {
            spans.push(Span::styled(" (current)", Style::default().fg(theme.muted)));
        }
        lines.push(Line::from(spans));
        if let Some(desc) = &opt.description {
            lines.push(Line::from(Span::styled(
                format!("     {desc}"),
                Style::default().fg(theme.muted),
            )));
        }
        if let Some(reason) = &opt.disabled_reason {
            lines.push(Line::from(Span::styled(
                format!("     × {reason}"),
                Style::default().fg(theme.muted),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(help_line(theme, "↑↓ 移动  Enter 确认  Esc 返回"));
    (lines, cursor)
}

fn approval_content(ov: &ApprovalOverlay, theme: &Theme) -> Vec<Line<'static>> {
    let req = &ov.request;
    let mut lines: Vec<Line> = Vec::new();
    lines.push(section(theme, "工具", &req.tool));
    lines.push(section(theme, "说明", &req.summary));
    if let Some(cmd) = &req.command {
        lines.push(section(theme, "命令", cmd));
    }
    if !req.risks.is_empty() {
        lines.push(Line::from(Span::styled(
            "风险",
            Style::default().fg(theme.muted),
        )));
        for risk in &req.risks {
            lines.push(Line::from(Span::styled(
                format!("  • {risk}"),
                Style::default().fg(theme.warning),
            )));
        }
    }
    lines.push(Line::from(""));
    for (label, is_cursor) in ov.options() {
        let prefix = if is_cursor { "› " } else { "  " };
        let style = if is_cursor {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(format!("{prefix}{label}"), style)));
    }
    lines.push(Line::from(""));
    lines.push(help_line(
        theme,
        "默认拒绝 · y 本次 · s 会话 · w 始终(项目规则) · d/Esc 拒绝 · ↑↓/Enter",
    ));
    lines
}

fn render_modal(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: Vec<Line>,
    theme: &Theme,
    max_w: u16,
) {
    let rect = modal_rect(area, max_w, lines.len() as u16);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn section(theme: &Theme, label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}  "), Style::default().fg(theme.muted)),
        Span::raw(value.to_string()),
    ])
}

fn help_line(theme: &Theme, text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(theme.muted),
    ))
}
