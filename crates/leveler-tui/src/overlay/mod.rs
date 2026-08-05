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
use ratatui::widgets::{Clear, Paragraph};
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
    /// Status-strip copy, or `None` when the overlay speaks for itself.
    ///
    /// Only overlays that interrupt say anything here — they explain why the
    /// task stopped. A picker the user just opened already carries its title
    /// as its first row, and repeating it in the status strip prints the same
    /// sentence twice, one line apart.
    pub fn status_hint(&self, t: &crate::i18n::UiText) -> Option<&'static str> {
        match self {
            Overlay::Approval(_) => Some(t.overlay_approval),
            Overlay::Clarification(_) => Some(t.overlay_clarify),
            Overlay::UnsupportedMedia(_) => Some(t.overlay_media),
            Overlay::ModelPicker(_)
            | Overlay::ModePicker(_)
            | Overlay::ThemePicker(_)
            | Overlay::CheckpointPicker(_) => None,
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
    let (title, lines, cursor) = build_content(overlay, theme, inner_width);
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
        // Continuation rows inherit the row's own indent, so a wrapped
        // description stays under the option it belongs to instead of falling
        // back to column 0 where it reads as a new entry.
        let indent: String = line
            .spans
            .first()
            .map(|s| s.content.chars().take_while(|c| *c == ' ').collect())
            .unwrap_or_default();
        let indent = if indent.len() + 8 < width { indent } else { String::new() };
        // Flatten to styled characters so a break can be chosen by looking at
        // the whole row rather than one span at a time.
        let cells: Vec<(char, Style)> = line
            .spans
            .iter()
            .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
            .collect();

        let mut row_start = 0usize;
        while row_start < cells.len() {
            let mut used = if row_start > 0 { indent.len() } else { 0 };
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
            let mut row_line = spans_of(&cells[row_start..end]);
            if row_start > 0 && !indent.is_empty() {
                row_line.spans.insert(0, Span::raw(indent.clone()));
            }
            out.push(row_line);
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
    width: usize,
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
        Overlay::Approval(ov) => ("需要权限".to_string(), approval_content(ov, theme, width), None),
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
    let (_, lines, _) = content_lines(overlay, theme, width as usize);
    lines.len() as u16
}

/// Draw the active overlay centered over `area` (modal form, used on
/// non-conversation screens).
pub fn render_overlay(frame: &mut Frame, area: Rect, overlay: &Overlay, theme: &Theme) {
    // No frame, for any of them. These surfaces sit in the composer's slot with
    // the conversation directly above; a box around them adds a border to
    // parse and a column of padding to nothing, and having some framed and
    // some bare made the same keys feel like different modes.
    let (_, lines, _) = content_lines(overlay, theme, area.width as usize);
    let h = (lines.len() as u16).min(area.height);
    let [row] = Layout::vertical([Constraint::Length(h)])
        .flex(Flex::End)
        .areas(area);
    frame.render_widget(Clear, row);
    frame.render_widget(Paragraph::new(lines), row);
}

fn clarification_content(
    ov: &ClarificationOverlay,
    theme: &Theme,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "需要澄清",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )));
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


fn selection_content(
    model: &SelectionModel,
    theme: &Theme,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let mut lines: Vec<Line> = Vec::new();
    let mut cursor = None;
    // Without a border there is nowhere else for the title to live, and a list
    // of choices with no question above it is a puzzle.
    lines.push(Line::from(Span::styled(
        model.title.clone(),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )));
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
        let prefix = if is_cursor { "▸ " } else { "  " };
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
            spans.push(Span::styled(" 当前", Style::default().fg(theme.muted)));
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

fn approval_content(ov: &ApprovalOverlay, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let req = &ov.request;
    let mut lines: Vec<Line> = Vec::new();
    let action = tool_action(&req.tool);
    let head = format!("允许 {action}");
    // The command shares the headline and is elided rather than wrapped: this
    // prompt appears many times a session, and a growing block of shell text
    // pushes the conversation off screen every time.
    let detail = req.command.clone().unwrap_or_else(|| req.summary.clone());
    let mut spans = vec![Span::styled(
        head.clone(),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )];
    let budget = width.saturating_sub(UnicodeWidthStr::width(head.as_str()) + 4);
    let elided = !detail.trim().is_empty()
        && !ov.expanded()
        && UnicodeWidthStr::width(detail.as_str()) > budget.max(8);
    if !detail.trim().is_empty() && !ov.expanded() {
        spans.push(Span::raw(format!(
            "（{}）",
            crate::render::text::truncate_display(&detail, budget.max(8))
        )));
    }
    lines.push(Line::from(spans));
    // Expanded: the command gets its own rows and `wrap_to_width` keeps every
    // character on screen, because this is the state you enter to read it.
    if ov.expanded() && !detail.trim().is_empty() {
        // Pre-wrap with a hanging indent so continuation rows line up under the
        // command instead of falling back to column 0, where they read as a
        // separate command rather than the rest of this one.
        for piece in crate::render::text::wrap(&detail, width.saturating_sub(2).max(8)) {
            lines.push(Line::from(Span::raw(format!("  {piece}"))));
        }
    }
    for risk in &req.risks {
        lines.push(Line::from(Span::styled(
            format!("  ⚠ {risk}"),
            Style::default().fg(theme.warning),
        )));
    }
    for (i, (label, is_cursor)) in ov.options().into_iter().enumerate() {
        let text = format!("{}. {label}", i + 1);
        if is_cursor {
            // The focused row is reversed end to end, so the eye lands on the
            // choice rather than hunting for a marker.
            let pad = width.saturating_sub(UnicodeWidthStr::width(text.as_str()) + 2);
            lines.push(Line::from(Span::styled(
                format!("▸ {text}{}", " ".repeat(pad)),
                Style::default().add_modifier(Modifier::REVERSED),
            )));
        } else {
            lines.push(Line::from(Span::raw(format!("  {text}"))));
        }
    }
    lines.push(help_line(
        theme,
        if elided {
            "  ↑↓ 选择 · Enter 确认 · Ctrl+O 展开完整命令 · Esc 取消"
        } else if ov.expanded() {
            "  ↑↓ 选择 · Enter 确认 · Ctrl+O 收起 · Esc 取消"
        } else {
            "  ↑↓ 选择 · Enter 确认 · Esc 取消"
        },
    ));
    lines
}

/// The tool's name in the words the transcript already uses, so the prompt
/// reads as part of the conversation rather than as an internal identifier.
fn tool_action(tool: &str) -> &str {
    match tool {
        "run_command" | "shell_command" => "执行命令",
        "apply_patch" | "write_file" => "修改文件",
        "read_file" => "读取文件",
        "remember" => "写入记忆",
        other => other,
    }
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
            command: Some("rm -rf src/main.rs && ls src/ && git status --short && echo done".into()),
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

    fn mode_picker() -> Overlay {
        Overlay::ModePicker(Box::new(SelectionModel::new(
            "选择权限模式",
            vec![
                SelectionOption::new("request_approval", "请求批准")
                    .description("始终询问：外写文件、用网、危险命令都要你点同意"),
                SelectionOption::new("assisted", "替我审批")
                    .description("半自动：读写/联网/shell（含 git push）自动执行，仅删除/提权/打开外部应用询问")
                    .current(true),
                SelectionOption::new("full_access", "完全访问")
                    .description("免审批：读写、危险命令、删除、记忆写入全部自动执行"),
            ],
            false,
        )))
    }

    #[test]
    fn every_overlay_is_a_bare_prompt() {
        // One shape for every decision surface. A framed dialog for some and a
        // bare prompt for others makes the same key mean different things
        // depending on which one happens to be open.
        let screen = frame_of(&mode_picker(), 110, 32).join("\n");
        assert!(
            !screen.contains('┌') && !screen.contains('│') && !screen.contains('╭'),
            "picker should not be boxed:\n{screen}"
        );
    }

    #[test]
    fn a_picker_row_never_spills_past_its_column() {
        // The mode descriptions are long enough to wrap; a wrapped row that
        // falls back to column 0 reads as a new option rather than the rest of
        // this one.
        let lines = frame_of(&mode_picker(), 64, 32);
        for l in lines.iter().filter(|l| !l.trim().is_empty()) {
            assert!(
                unicode_width::UnicodeWidthStr::width(l.as_str()) <= 64,
                "row overflows the terminal: {l:?}"
            );
        }
        // Continuation of a description stays indented under it.
        let wrapped: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("自动执行") || l.contains("询问"))
            .collect();
        for l in &wrapped {
            let indent = l.len() - l.trim_start().len();
            assert!(indent >= 4, "description row is not indented: {l:?}");
        }
    }

    #[test]
    fn the_approval_is_a_compact_prompt_not_a_dialog_box() {
        // A decision the user makes many times a session should read like the
        // next line of the conversation, not interrupt it with a framed dialog
        // that eats a third of the screen.
        let lines = frame_of(&overlay(""), 110, 32);
        let painted: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
        assert!(
            painted.len() <= 10,
            "approval takes {} rows:\n{}",
            painted.len(),
            lines.join("\n")
        );
        let screen = lines.join("\n");
        assert!(
            !screen.contains('┌') && !screen.contains('│'),
            "approval should not be boxed:\n{screen}"
        );
        // Numbered choices: quick to hit, and they read as a question's answers.
        assert!(screen.contains("1."), "options must be numbered:\n{screen}");
        // Left-aligned with the transcript, not centred in the terminal.
        let first = painted.first().unwrap();
        let indent = first.len() - first.trim_start().len();
        assert!(indent <= 2, "approval should be left-aligned: {first:?}");
    }

    #[test]
    fn a_framed_overlay_wraps_instead_of_clipping() {
        // The approval is bare now, but pickers and clarifications still draw a
        // box, and content must wrap inside it rather than be cut by it.
        let long = "请确认这一步该怎么做，".repeat(6);
        let ov = Overlay::Clarification(Box::new(crate::overlay::ClarificationOverlay::new(
            leveler_client_protocol::UiClarificationRequest {
                id: leveler_client_protocol::ClarificationId::new("c1"),
                question: long.clone(),
                options: vec![],
            },
        )));
        let screen = frame_of(&ov, 110, 32).join("\n");
        let flat: String = screen
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '│')
            .collect();
        let want: String = long.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(flat.contains(&want), "question was clipped:\n{screen}");
    }

    #[test]
    fn a_long_command_is_elided_visibly_not_silently() {
        // The prompt keeps the command on one line so it does not push the
        // conversation off screen every time it appears. That trades full
        // visibility for compactness, so the cut must be marked: a command that
        // simply ends mid-path reads as the whole command.
        let long = "rm -rf ".to_string() + &"a/very/deeply/nested/path/".repeat(6) + "target";
        let ov = Overlay::Approval(Box::new(ApprovalOverlay::new(UiApprovalRequest {
            id: ApprovalId::new("r1"),
            tool: "shell_command".into(),
            summary: String::new(),
            command: Some(long),
            risks: vec![],
        })));
        let lines = frame_of(&ov, 110, 40);
        let head = lines
            .iter()
            .find(|l| l.contains("允许"))
            .expect("headline must be on screen");
        assert!(head.contains('…'), "elision is unmarked: {head}");
        let screen = lines.join("\n");
        assert!(
            screen.contains("Ctrl+O"),
            "an elided command must say how to read it in full:\n{screen}"
        );
        // Still one row: the point of eliding is that it does not grow.
        assert_eq!(
            lines.iter().filter(|l| l.contains("rm -rf")).count(),
            1,
            "command must stay on one row:\n{}",
            lines.join("\n")
        );
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
    fn expanding_shows_the_whole_command() {
        let long = "rm -rf ".to_string() + &"a/very/deeply/nested/path/".repeat(6) + "target";
        let mut ap = ApprovalOverlay::new(UiApprovalRequest {
            id: ApprovalId::new("r1"),
            tool: "shell_command".into(),
            summary: String::new(),
            command: Some(long.clone()),
            risks: vec![],
        });
        ap.on_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('o'),
            ratatui::crossterm::event::KeyModifiers::CONTROL,
        ));
        let screen = frame_of(&Overlay::Approval(Box::new(ap)), 110, 40).join("\n");
        let flat: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
        let want: String = long.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(flat.contains(&want), "expanded command is cut:\n{screen}");
    }

    #[test]
    fn a_summary_stands_in_when_there_is_no_command() {
        // Tools without a command line (memory writes, MCP calls) still need a
        // headline that says what is about to happen.
        let ov = Overlay::Approval(Box::new(ApprovalOverlay::new(UiApprovalRequest {
            id: ApprovalId::new("r1"),
            tool: "remember".into(),
            summary: "记住用户偏好".into(),
            command: None,
            risks: vec![],
        })));
        let screen = frame_of(&ov, 110, 32).join("\n");
        assert!(screen.contains("记住用户偏好"), "frame:\n{screen}");
    }
}
