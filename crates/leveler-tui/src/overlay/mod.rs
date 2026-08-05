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
    inner_width: usize,
) -> (String, Vec<Line<'static>>, Option<(usize, usize)>) {
    let (title, lines, cursor) = build_content(overlay, theme);
    let (lines, cursor) = wrap_to_width(lines, cursor, inner_width);
    (title, lines, cursor)
}

/// Re-flow `lines` so none is wider than `width`, keeping each span's style.
///
/// Overlays carry things the user must read in full — a command they are about
/// to approve, the key that confirms it. Clipping those to the border turns a
/// decision prompt into a guess. Breaking is by display width rather than at
/// word boundaries so a long command wraps exactly where the box ends.
fn wrap_to_width(
    lines: Vec<Line<'static>>,
    cursor: Option<(usize, usize)>,
    width: usize,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    if width == 0 {
        return (lines, cursor);
    }
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    let mut cursor_out = cursor;
    for (idx, line) in lines.into_iter().enumerate() {
        let start = out.len();
        // Flatten to styled characters so a break can be chosen by looking at
        // the whole row rather than one span at a time.
        let cells: Vec<(char, Style)> = line
            .spans
            .iter()
            .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
            .collect();

        let mut row_start = 0usize;
        while row_start < cells.len() {
            let mut used = 0usize;
            let mut end = row_start;
            let mut last_space: Option<usize> = None;
            while end < cells.len() {
                let w = unicode_width::UnicodeWidthChar::width(cells[end].0).unwrap_or(0);
                if used + w > width {
                    break;
                }
                used += w;
                end += 1;
                if cells[end - 1].0 == ' ' {
                    last_space = Some(end);
                }
            }
            // Prefer breaking after a space: splitting "Enter" into "En"/"ter"
            // is technically lossless and practically unreadable. A token
            // longer than the box still breaks hard — losing it is worse.
            if end < cells.len()
                && cells[end].0 != ' '
                && let Some(sp) = last_space
                && sp > row_start
            {
                end = sp;
            }
            out.push(spans_of(&cells[row_start..end]));
            row_start = end.max(row_start + 1);
            // Leading spaces from a word break would indent the next row.
            while row_start < cells.len() && cells[row_start].0 == ' ' && used >= width {
                row_start += 1;
            }
        }
        if out.len() == start {
            out.push(Line::from(""));
        }
        // The cursor is addressed by source row; move it onto the wrapped row
        // that now holds it.
        if let Some((crow, ccol)) = cursor
            && crow == idx
        {
            cursor_out = Some((start + ccol / width, ccol % width));
        }
    }
    (out, cursor_out)
}

/// Rebuild spans from styled characters, merging runs that share a style.
fn spans_of(cells: &[(char, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (ch, style) in cells {
        match spans.last_mut() {
            Some(last) if last.style == *style => last.content.to_mut().push(*ch),
            _ => spans.push(Span::styled(ch.to_string(), *style)),
        }
    }
    Line::from(spans)
}

fn build_content(
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

/// How many rows the overlay needs, including its border.
///
/// The workbench reserves this instead of drawing the overlay on top: a
/// decision belongs where the input box was, and painting over the transcript
/// hides the very message the user is judging.
pub fn overlay_height(overlay: &Overlay, theme: &Theme, width: u16) -> u16 {
    let inner = modal_width(overlay, width).saturating_sub(2);
    let (_, lines, _) = content_lines(overlay, theme, inner as usize);
    (lines.len() as u16).saturating_add(2)
}

/// The modal's outer width for a given available width.
fn modal_width(overlay: &Overlay, available: u16) -> u16 {
    let max_w = match overlay {
        Overlay::Approval(_) | Overlay::Clarification(_) => 68,
        _ => 64,
    };
    max_w.min(available.saturating_sub(4)).max(20)
}

/// Draw the active overlay centered over `area` (modal form, used on
/// non-conversation screens).
pub fn render_overlay(frame: &mut Frame, area: Rect, overlay: &Overlay, theme: &Theme) {
    let w = modal_width(overlay, area.width);
    let (title, lines, _) = content_lines(overlay, theme, w.saturating_sub(2) as usize);
    render_modal(frame, area, &title, lines, theme, w);
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
fn modal_rect(area: Rect, w: u16, content_h: u16) -> Rect {
    // Fill the height it was given. The workbench reserves exactly this box's
    // rows in the composer's slot, so any flex here would leave a gap between
    // the transcript and the decision. On other screens `area` is the whole
    // frame and the box still grows only to its content.
    let h = (content_h + 2).min(area.height).max(3);
    let [row] = Layout::vertical([Constraint::Length(h)])
        .flex(Flex::End)
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
    // A label with nothing after it is noise in a prompt people must read.
    if !req.summary.trim().is_empty() {
        lines.push(section(theme, "说明", &req.summary));
    }
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
    width: u16,
) {
    let rect = modal_rect(area, width, lines.len() as u16);
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

#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::overlay::approval::ApprovalOverlay;
    use leveler_client_protocol::{ApprovalId, UiApprovalRequest};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn overlay(summary: &str) -> Overlay {
        Overlay::Approval(Box::new(ApprovalOverlay::new(UiApprovalRequest {
            id: ApprovalId::new("r1"),
            tool: "shell_command".into(),
            summary: summary.into(),
            command: Some("rm -rf src/main.rs && ls src/".into()),
            risks: vec!["可能造成破坏性变更".into()],
        })))
    }

    fn frame_of(ov: &Overlay, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let theme = Theme::default();
        term.draw(|f| render_overlay(f, f.area(), ov, &theme))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = Vec::new();
        for y in 0..h {
            let mut line = String::new();
            let mut x = 0;
            while x < w {
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                line.push_str(sym);
                // A double-width grapheme owns the next cell; skip it.
                x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            }
            out.push(line.trim_end().to_string());
        }
        out
    }

    #[test]
    fn no_overlay_line_is_cut_off_by_the_border() {
        // A hint that ends in "↑↓/En" is worse than no hint: the user cannot
        // tell whether the key is Enter or something else. Long content must
        // wrap inside the box, never get clipped by it.
        let lines = frame_of(&overlay(""), 110, 32);
        let screen = lines.join("\n");
        assert!(
            screen.contains("默认拒绝") && screen.contains("Enter"),
            "the key hint is clipped:\n{screen}"
        );
    }

    #[test]
    fn a_long_command_is_never_clipped() {
        // Approving a command you cannot fully see is approving blind.
        let long = "rm -rf ".to_string() + &"a/very/deeply/nested/path/".repeat(4) + "target";
        let ov = Overlay::Approval(Box::new(ApprovalOverlay::new(UiApprovalRequest {
            id: ApprovalId::new("r1"),
            tool: "shell_command".into(),
            summary: String::new(),
            command: Some(long.clone()),
            risks: vec![],
        })));
        let screen = frame_of(&ov, 110, 40).join("\n");
        let flat: String = screen
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '│')
            .collect();
        let want: String = long.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(flat.contains(&want), "command was clipped:\n{screen}");
    }

    #[test]
    fn an_empty_summary_leaves_no_empty_row() {
        let lines = frame_of(&overlay(""), 110, 32);
        assert!(
            !lines.iter().any(|l| l.contains("说明")),
            "an empty summary must not render a label with nothing after it"
        );
    }

    #[test]
    fn a_real_summary_still_shows() {
        let lines = frame_of(&overlay("删除源文件"), 110, 32);
        assert!(
            lines.iter().any(|l| l.contains("说明") && l.contains("删除源文件")),
            "frame:\n{}",
            lines.join("\n")
        );
    }
}
