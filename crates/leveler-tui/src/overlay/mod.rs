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
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use leveler_client_protocol::ClarificationQuestionKind;

use crate::render::text::truncate_display;
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
    /// Named TUI palettes (`auto` / `dark` / `light` / `high-contrast`).
    ThemePicker(Box<SelectionModel>),
    /// `/work-mode` — economy / balanced.
    WorkModePicker(Box<SelectionModel>),
    /// `/collab` — chat / plan / goal.
    CollabPicker(Box<SelectionModel>),
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
            | Overlay::WorkModePicker(_)
            | Overlay::CollabPicker(_)
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
    locale: crate::i18n::Locale,
) -> (String, Vec<Line<'static>>, Option<(usize, usize)>) {
    let (title, lines, cursor) = build_content(overlay, theme, inner_width, locale);
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
        let indent = if indent.len() + 8 < width {
            indent
        } else {
            String::new()
        };
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
    locale: crate::i18n::Locale,
) -> (String, Vec<Line<'static>>, Option<(usize, usize)>) {
    match overlay {
        Overlay::ModelPicker(model)
        | Overlay::ModePicker(model)
        | Overlay::ThemePicker(model)
        | Overlay::WorkModePicker(model)
        | Overlay::CollabPicker(model)
        | Overlay::UnsupportedMedia(model)
        | Overlay::CheckpointPicker(model) => {
            let (lines, cursor) = selection_content(model, theme, locale);
            (model.title.clone(), lines, cursor)
        }
        Overlay::Approval(ov) => (
            locale.text().overlay_approval.to_string(),
            approval_content(ov, theme, width, locale),
            None,
        ),
        Overlay::Clarification(ov) => {
            let (lines, cursor) = clarification_content(ov, theme, width, locale);
            (locale.text().clarify_title.to_string(), lines, cursor)
        }
    }
}

/// How many rows the overlay needs, including its border.
///
/// The workbench reserves this instead of drawing the overlay on top: a
/// decision belongs where the input box was, and painting over the transcript
/// hides the very message the user is judging.
pub fn overlay_height(
    overlay: &Overlay,
    theme: &Theme,
    width: u16,
    locale: crate::i18n::Locale,
) -> u16 {
    let (_, lines, _) = content_lines(overlay, theme, width as usize, locale);
    lines.len() as u16
}

/// Draw the active overlay centered over `area` (modal form, used on
/// non-conversation screens).
pub fn render_overlay(
    frame: &mut Frame,
    area: Rect,
    overlay: &Overlay,
    theme: &Theme,
    locale: crate::i18n::Locale,
) {
    // No frame, for any of them. These surfaces sit in the composer's slot with
    // the conversation directly above; a box around them adds a border to
    // parse and a column of padding to nothing, and having some framed and
    // some bare made the same keys feel like different modes.
    let (_, lines, cursor) = content_lines(overlay, theme, area.width as usize, locale);
    let h = (lines.len() as u16).min(area.height);
    let [row] = Layout::vertical([Constraint::Length(h)])
        .flex(Flex::End)
        .areas(area);
    theme.paint_surface(frame, row, theme.surface.elevated);
    frame.render_widget(Paragraph::new(lines), row);
    // A text field in an overlay needs a visible insertion point. The
    // composer's cursor is suppressed while an overlay owns the slot, so if
    // this does not place it, a question the user must type into has none.
    if let Some((crow, ccol)) = cursor {
        let x = row.x.saturating_add(ccol as u16);
        let y = row.y.saturating_add(crow as u16);
        if (crow as u16) < row.height && x < row.x.saturating_add(row.width) {
            frame.set_cursor_position(ratatui::layout::Position::new(x, y));
        }
    }
}

/// Gap between two tabs in the strip.
const TAB_SEP: usize = 3;
/// `… ` / ` …` — the marker that says tabs are clipped on that side.
const TAB_ELLIPSIS: usize = 2;

/// One tab, measured once so the strip can be laid out without re-measuring.
struct TabCell {
    marker: &'static str,
    label: String,
    style: Style,
    width: usize,
    active: bool,
}

fn clarification_tabs(
    ov: &ClarificationOverlay,
    theme: &Theme,
    max_label: usize,
    t: &crate::i18n::UiText,
) -> Vec<TabCell> {
    ov.questions()
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let current = i == ov.active();
            // The three states are exclusive: the tab you are on says where
            // you are, and the headline's count already says how far along the
            // interaction is.
            let marker = if current {
                t.clarify_tab_current
            } else if q.answer.is_some() {
                t.clarify_tab_answered
            } else {
                t.clarify_tab_pending
            };
            let label = truncate_display(&q.display_header(), max_label);
            let style = if current {
                Style::default()
                    .fg(theme.accent.primary)
                    .add_modifier(Modifier::BOLD)
            } else if q.answer.is_some() {
                Style::default().fg(theme.status.success)
            } else {
                Style::default().fg(theme.text.muted)
            };
            let width = 2 + UnicodeWidthStr::width(label.as_str());
            TabCell {
                marker,
                label,
                style,
                width,
                active: current,
            }
        })
        .collect()
}

/// The widest window of tabs containing `active` that fits `budget` columns,
/// including the `…` markers for what it clips on each side.
///
/// Keeping the ACTIVE tab visible is the whole contract: an 8-question
/// interaction on an 80-column terminal must never scroll the tab you are on
/// off the strip.
fn tab_window(cells: &[TabCell], active: usize, budget: usize) -> (usize, usize) {
    let n = cells.len();
    if n == 0 {
        return (0, 0);
    }
    let cost = |s: usize, e: usize| -> usize {
        let body: usize = (s..=e).map(|i| cells[i].width).sum::<usize>() + TAB_SEP * (e - s);
        let left = if s > 0 { TAB_ELLIPSIS + TAB_SEP } else { 0 };
        let right = if e + 1 < n { TAB_SEP + TAB_ELLIPSIS } else { 0 };
        body + left + right
    };
    if cost(0, n - 1) <= budget {
        return (0, n - 1);
    }
    let (mut s, mut e) = (active, active);
    let mut prefer_left = active > 0;
    loop {
        let mut grew = false;
        for left in [prefer_left, !prefer_left] {
            if left && s > 0 && cost(s - 1, e) <= budget {
                s -= 1;
                grew = true;
                break;
            }
            if !left && e + 1 < n && cost(s, e + 1) <= budget {
                e += 1;
                grew = true;
                break;
            }
        }
        if !grew {
            break;
        }
        prefer_left = !prefer_left;
    }
    (s, e)
}

/// The tab row and its underline row. Both fit `width` exactly, so the
/// underline stays under the tab it belongs to and `wrap_to_width` never has
/// to break either one.
fn clarification_tab_rows(
    ov: &ClarificationOverlay,
    theme: &Theme,
    width: usize,
    t: &crate::i18n::UiText,
) -> (Line<'static>, Line<'static>) {
    // Markers, separators and a possible ellipsis on each side come out of the
    // label budget before a single label is measured.
    let chrome = 2 + TAB_SEP * 2 + TAB_ELLIPSIS * 2 + 2;
    let max_label = width.saturating_sub(chrome).max(1);
    let cells = clarification_tabs(ov, theme, max_label, t);
    let (start, end) = tab_window(&cells, ov.active(), width);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut active_col = 0usize;
    let mut active_width = 0usize;
    fn push(spans: &mut Vec<Span<'static>>, used: &mut usize, span: Span<'static>) {
        *used += UnicodeWidthStr::width(span.content.as_ref());
        spans.push(span);
    }
    if start > 0 {
        push(
            &mut spans,
            &mut used,
            Span::styled("… ".to_string(), Style::default().fg(theme.text.muted)),
        );
    }
    for (i, cell) in cells.iter().enumerate().take(end + 1).skip(start) {
        if i > start {
            push(&mut spans, &mut used, Span::raw(" ".repeat(TAB_SEP)));
        }
        if cell.active {
            active_col = used;
            active_width = cell.width;
        }
        push(&mut spans, &mut used, Span::styled(cell.marker, cell.style));
        push(
            &mut spans,
            &mut used,
            Span::styled(format!(" {}", cell.label), cell.style),
        );
    }
    if end + 1 < cells.len() {
        push(
            &mut spans,
            &mut used,
            Span::styled(" …".to_string(), Style::default().fg(theme.text.muted)),
        );
    }
    let underline = format!(
        "{}{}",
        " ".repeat(active_col.min(width)),
        "\u{2501}".repeat(active_width.min(width.saturating_sub(active_col)))
    );
    (
        Line::from(spans),
        Line::from(Span::styled(
            underline,
            Style::default().fg(theme.accent.primary),
        )),
    )
}

/// Digit column width of the largest 1-based index in an option list of
/// `total`. Every row reserves this many columns, so `9.` and `10.` keep their
/// labels in one vertical line.
fn option_number_width(total: usize) -> usize {
    total.max(1).to_string().len()
}

/// `" 1."` / `"10."` — the right-aligned, stable number half of an option
/// row. Callers add their own separator and label.
fn option_number(index: usize, total: usize) -> String {
    format!("{:>width$}.", index + 1, width = option_number_width(total))
}

/// The width-stable text columns every numbered option row opens with: focus
/// marker, number, and (for a multi-choice) the selection box.
///
/// The focus column is always reserved, so moving the cursor never shifts the
/// label; the number column is padded from the option count, so 9 and 10 align;
/// the selection box keeps this project's existing `[x]` / `[ ]` vocabulary.
/// Styles are the row's own: the focus glyph wears the accent, the focused
/// number brightens with its label, and an unfocused number sits one step below
/// the label instead of competing with it.
fn option_prefix_spans(
    index: usize,
    total: usize,
    focused: bool,
    marker: &str,
    state: Option<&str>,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let focus = if focused {
        marker.to_string()
    } else {
        " ".repeat(UnicodeWidthStr::width(marker))
    };
    let focus_style = if focused {
        Style::default().fg(theme.accent.primary)
    } else {
        Style::default()
    };
    let number_style = if focused {
        Style::default()
            .fg(theme.text.primary)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text.secondary)
    };
    let mut spans = vec![
        Span::styled(format!("{focus} "), focus_style),
        Span::styled(format!("{} ", option_number(index, total)), number_style),
    ];
    if let Some(state) = state {
        spans.push(Span::styled(
            format!("{state} "),
            Style::default().fg(theme.text.secondary),
        ));
    }
    spans
}

/// Push one list row, wrapping its label to the space left after `prefix` and
/// indenting the continuation under the label rather than under the margin.
fn push_wrapped_row(
    lines: &mut Vec<Line<'static>>,
    prefix: Vec<Span<'static>>,
    label: &str,
    label_style: Style,
    suffix: Option<Span<'static>>,
    width: usize,
) {
    let prefix_width: usize = prefix
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let indent = " ".repeat(prefix_width);
    let suffix_width = suffix
        .as_ref()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .unwrap_or(0);
    let first_room = width.saturating_sub(prefix_width + suffix_width).max(1);
    let cont_room = width.saturating_sub(prefix_width + suffix_width).max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut rest = label;
    while !rest.is_empty() {
        let room = if chunks.is_empty() {
            first_room
        } else {
            cont_room
        };
        let (piece, tail) = crate::render::text::take_display_prefix(rest, room);
        // Prefer breaking after a space: splitting a word is lossless and
        // unreadable. A token longer than the row still breaks hard.
        if !tail.is_empty()
            && !tail.starts_with(' ')
            && let Some(cut) = piece.rfind(' ')
            && cut > 0
        {
            let kept = piece[..cut].to_string();
            chunks.push(kept);
            rest = rest[cut..].trim_start();
            continue;
        }
        chunks.push(piece);
        rest = tail;
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    let last = chunks.len() - 1;
    for (i, chunk) in chunks.into_iter().enumerate() {
        let mut spans = if i == 0 {
            prefix.clone()
        } else {
            vec![Span::raw(indent.clone())]
        };
        spans.push(Span::styled(chunk, label_style));
        if i == last
            && let Some(suffix) = suffix.clone()
        {
            spans.push(suffix);
        }
        lines.push(Line::from(spans));
    }
}

fn clarification_content(
    ov: &ClarificationOverlay,
    theme: &Theme,
    width: usize,
    locale: crate::i18n::Locale,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let t = locale.text();
    let mut lines: Vec<Line> = Vec::new();
    let headline = if ov.legacy() {
        t.clarify_title.to_string()
    } else {
        t.clarify_headline
            .replacen("{}", &ov.answered_count().to_string(), 1)
            .replacen("{}", &ov.len().to_string(), 1)
    };
    lines.push(Line::from(Span::styled(
        headline,
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    )));
    // A strip of one tab is chrome, not information: the legacy single-question
    // shape renders exactly as it always did.
    if !ov.legacy() {
        lines.push(Line::from(""));
        let (tabs, underline) = clarification_tab_rows(ov, theme, width, t);
        lines.push(tabs);
        lines.push(underline);
    }
    lines.push(Line::from(""));

    let questions = ov.questions();
    let Some(active) = questions.get(ov.active()) else {
        lines.push(help_line(theme, t.clarify_hint));
        return (lines, None);
    };

    let mut prompt = vec![Span::styled(
        active.prompt.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    let mut badges: Vec<String> = Vec::new();
    if active.is_multi() {
        badges.push(t.clarify_multi_badge.to_string());
        if active.min_choices > 0 {
            badges.push(
                t.clarify_min_choices
                    .replace("{}", &active.min_choices.to_string()),
            );
        }
        if let Some(max) = active.max_choices {
            badges.push(t.clarify_max_choices.replace("{}", &max.to_string()));
        }
    }
    if !badges.is_empty() {
        prompt.push(Span::styled(
            format!("  · {}", badges.join(" · ")),
            Style::default().fg(theme.text.secondary),
        ));
    }
    lines.push(Line::from(prompt));
    lines.push(Line::from(""));

    let mut cursor_at: Option<(usize, usize)> = None;
    if active.kind == ClarificationQuestionKind::Text {
        let row = lines.len();
        lines.push(Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.accent.primary)),
            Span::raw(active.text.clone()),
        ]));
        cursor_at = Some((row, 2 + UnicodeWidthStr::width(active.text.as_str())));
    } else {
        let recorded = match active.answer.as_ref() {
            Some(crate::overlay::clarification::Answer::Picks(picks)) => picks.first().copied(),
            _ => None,
        };
        let total = active.options.len() + usize::from(active.allow_other);
        for (i, option) in active.options.iter().enumerate() {
            let focused = i == active.cursor;
            let state = active
                .is_multi()
                .then(|| if active.selected[i] { "[x]" } else { "[ ]" });
            let prefix = option_prefix_spans(i, total, focused, "❯", state, theme);
            let label_style = if focused {
                Style::default()
                    .fg(theme.text.primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text.primary)
            };
            // A recorded single pick is marked so reopening a settled question
            // shows what was chosen; the cursor only says where the arrows are.
            let suffix = (recorded == Some(i))
                .then(|| Span::styled(" ✓", Style::default().fg(theme.status.success)));
            push_wrapped_row(&mut lines, prefix, option, label_style, suffix, width);
        }
        if active.allow_other {
            let focused = active.on_other_row();
            let prefix =
                option_prefix_spans(active.options.len(), total, focused, "❯", None, theme);
            let prefix_w: usize = prefix
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let label_style = if focused {
                Style::default()
                    .fg(theme.text.primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text.primary)
            };
            let row = lines.len();
            let text = active.text.clone();
            let mut spans = prefix;
            spans.push(Span::styled(format!("{} ", t.clarify_other), label_style));
            spans.push(Span::raw(text.clone()));
            if focused {
                let col = prefix_w
                    + UnicodeWidthStr::width(t.clarify_other)
                    + 1
                    + UnicodeWidthStr::width(text.as_str());
                cursor_at = Some((row, col));
            }
            lines.push(Line::from(spans));
        }
    }

    if let Some(notice) = ov.notice() {
        let text = match notice {
            crate::overlay::clarification::ClarificationNotice::MinChoices(n) => {
                t.clarify_min_choices_blocked.replace("{}", &n.to_string())
            }
            crate::overlay::clarification::ClarificationNotice::MaxChoices(n) => {
                t.clarify_max_choices_blocked.replace("{}", &n.to_string())
            }
        };
        lines.push(Line::from(Span::styled(
            text,
            Style::default().fg(theme.status.warning),
        )));
    }

    lines.push(Line::from(""));
    let hint = if active.kind == ClarificationQuestionKind::Text || active.on_other_row() {
        t.clarify_nav_hint_text
    } else if active.is_multi() {
        t.clarify_nav_hint_multi
    } else {
        t.clarify_nav_hint
    };
    lines.push(help_line(theme, hint));
    (lines, cursor_at)
}

fn selection_content(
    model: &SelectionModel,
    theme: &Theme,
    locale: crate::i18n::Locale,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let t = locale.text();
    let mut lines: Vec<Line> = Vec::new();
    let mut cursor = None;
    // Without a border there is nowhere else for the title to live, and a list
    // of choices with no question above it is a puzzle.
    lines.push(Line::from(Span::styled(
        model.title.clone(),
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    )));
    if let Some(desc) = &model.description {
        lines.push(Line::from(Span::styled(
            desc.clone(),
            Style::default().fg(theme.text.secondary),
        )));
        lines.push(Line::from(""));
    }
    if model.is_searchable() {
        cursor = Some((lines.len(), 8 + UnicodeWidthStr::width(model.query())));
        lines.push(Line::from(vec![
            Span::styled(t.picker_search, Style::default().fg(theme.text.secondary)),
            Span::raw(model.query().to_string()),
        ]));
        lines.push(Line::from(""));
    }

    let visible = model.visible_rows();
    let total = visible.len();
    for (pos, (_, opt, is_cursor)) in visible.into_iter().enumerate() {
        let focus = if is_cursor { "▸" } else { " " };
        // A searchable list types digits into the query, so it stays
        // unnumbered; every other picker numbers its rows from the count so a
        // two-digit row never shifts the label.
        let number = if model.is_searchable() {
            String::new()
        } else {
            format!("{} ", option_number(pos, total))
        };
        let base = if opt.is_enabled() {
            if is_cursor {
                Style::default()
                    .fg(theme.accent.primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            }
        } else {
            Style::default().fg(theme.text.secondary)
        };
        let mut spans = vec![Span::styled(format!("{focus} {number}{}", opt.label), base)];
        if opt.recommended {
            spans.push(Span::styled(
                t.picker_recommended,
                Style::default().fg(theme.status.success),
            ));
        }
        if opt.current {
            spans.push(Span::styled(
                t.picker_current,
                Style::default().fg(theme.text.secondary),
            ));
        }
        lines.push(Line::from(spans));
        if let Some(desc) = &opt.description {
            lines.push(Line::from(Span::styled(
                format!("     {desc}"),
                Style::default().fg(theme.text.secondary),
            )));
        }
        if let Some(reason) = &opt.disabled_reason {
            lines.push(Line::from(Span::styled(
                format!("     × {reason}"),
                Style::default().fg(theme.text.secondary),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(help_line(theme, t.picker_hint));
    (lines, cursor)
}

fn approval_content(
    ov: &ApprovalOverlay,
    theme: &Theme,
    width: usize,
    locale: crate::i18n::Locale,
) -> Vec<Line<'static>> {
    let req = &ov.request;
    let mut lines: Vec<Line> = Vec::new();
    // The tool's name in the words the transcript already uses (the taxonomy's
    // presentation label), so the prompt reads as part of the conversation
    // rather than as an internal identifier.
    let t = locale.text();
    let action = crate::tool_taxonomy::presentation_label(&req.tool, locale);
    let head = t.approval_head.replace("{}", &action);
    // The command shares the headline and is elided rather than wrapped: this
    // prompt appears many times a session, and a growing block of shell text
    // pushes the conversation off screen every time.
    let detail = req.command.clone().unwrap_or_else(|| req.summary.clone());
    let mut spans = vec![Span::styled(
        head.clone(),
        Style::default()
            .fg(theme.accent.primary)
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
            Style::default().fg(theme.status.warning),
        )));
    }
    let options = ov.options(t);
    let total = options.len();
    for (i, (label, is_cursor)) in options.into_iter().enumerate() {
        let text = format!("{} {label}", option_number(i, total));
        if is_cursor {
            // The focused row is reversed end to end, so the eye lands on the
            // choice rather than hunting for a marker.
            let pad = width.saturating_sub(UnicodeWidthStr::width(text.as_str()) + 2);
            lines.push(Line::from(Span::styled(
                format!("▸ {text}{}", " ".repeat(pad)),
                Style::default()
                    .fg(theme.text.primary)
                    .bg(theme.surface.selection),
            )));
        } else {
            lines.push(Line::from(Span::raw(format!("  {text}"))));
        }
    }
    lines.push(help_line(
        theme,
        if elided {
            t.approval_hint_expand
        } else if ov.expanded() {
            t.approval_hint_collapse
        } else {
            t.approval_hint_plain
        },
    ));
    lines
}

fn help_line(theme: &Theme, text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(theme.text.secondary),
    ))
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::overlay::approval::ApprovalOverlay;
    use leveler_client_protocol::{
        ApprovalId, ClarificationId, UiApprovalRequest, UiClarificationQuestion,
        UiClarificationRequest,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn overlay(summary: &str) -> Overlay {
        Overlay::Approval(Box::new(ApprovalOverlay::new(UiApprovalRequest {
            id: ApprovalId::new("r1"),
            tool: "shell_command".into(),
            summary: summary.into(),
            command: Some(
                "rm -rf src/main.rs && ls src/ && git status --short && echo done".into(),
            ),
            risks: vec!["可能造成破坏性变更".into()],
            call_id: None,
            always_persists: true,
        })))
    }

    fn frame_of(ov: &Overlay, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let theme = Theme::default();
        term.draw(|f| render_overlay(f, f.area(), ov, &theme, crate::i18n::Locale::Zh))
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
            leveler_client_protocol::UiClarificationRequest::single(
                leveler_client_protocol::ClarificationId::new("c1"),
                long.clone(),
                vec![],
            ),
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
            call_id: None,
            always_persists: true,
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
            call_id: None,
            always_persists: true,
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
            call_id: None,
            always_persists: true,
        })));
        let screen = frame_of(&ov, 110, 32).join("\n");
        assert!(screen.contains("记住用户偏好"), "frame:\n{screen}");
    }

    // ── Numbered option rows ────────────────────────────────────────────────

    fn clarification_rows(ov: &ClarificationOverlay) -> Vec<String> {
        let overlay = Overlay::Clarification(Box::new(ov.clone()));
        let (_, lines, _) =
            content_lines(&overlay, &Theme::no_color(), 100, crate::i18n::Locale::Zh);
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn single_choice(options: &[&str]) -> ClarificationOverlay {
        ClarificationOverlay::new(UiClarificationRequest::single(
            ClarificationId::new("c1"),
            "选哪个？",
            options.iter().map(|s| (*s).to_string()).collect(),
        ))
    }

    /// Display column where a label starts in its rendered row.
    fn label_col(rows: &[String], needle: &str) -> usize {
        let row = rows
            .iter()
            .find(|r| r.contains(needle))
            .unwrap_or_else(|| panic!("no row for {needle}: {rows:#?}"));
        let byte = row.find(needle).unwrap();
        UnicodeWidthStr::width(&row[..byte])
    }

    /// The label starts in the same column whether or not the row is focused:
    /// a focus marker that appears/disappears must not shift the text.
    #[test]
    fn single_choice_options_are_numbered_under_a_stable_focus_column() {
        let rows = clarification_rows(&single_choice(&["Go", "TypeScript", "Python", "Rust"]));
        assert!(rows.iter().any(|r| r.contains("❯ 1. Go")), "{rows:#?}");
        assert!(
            rows.iter().any(|r| r.contains("  2. TypeScript")),
            "{rows:#?}"
        );
        assert!(rows.iter().any(|r| r.contains("  4. Rust")), "{rows:#?}");
        assert_eq!(label_col(&rows, "Go"), label_col(&rows, "Python"));
        assert_eq!(label_col(&rows, "Go"), label_col(&rows, "Rust"));
    }

    #[test]
    fn a_single_option_still_gets_a_number() {
        let rows = clarification_rows(&single_choice(&["Only"]));
        assert!(rows.iter().any(|r| r.contains("❯ 1. Only")), "{rows:#?}");
    }

    /// 9 → 10 must not shift the label: the number column is padded from the
    /// option count, not from the row's own digit count.
    #[test]
    fn two_digit_options_keep_every_label_in_one_column() {
        let labels: Vec<String> = (1..=12).map(|i| format!("Opt{i:02}")).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let rows = clarification_rows(&single_choice(&refs));
        assert!(rows.iter().any(|r| r.contains(" 9. Opt09")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("10. Opt10")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("11. Opt11")), "{rows:#?}");
        assert_eq!(label_col(&rows, "Opt01"), label_col(&rows, "Opt09"));
        assert_eq!(label_col(&rows, "Opt09"), label_col(&rows, "Opt10"));
        assert_eq!(label_col(&rows, "Opt10"), label_col(&rows, "Opt12"));
    }

    /// 99 / 100 options: the column widens once, so every label still lines up.
    #[test]
    fn the_number_column_widens_with_the_option_count() {
        assert_eq!(option_number(0, 9), "1.");
        assert_eq!(option_number(8, 9), "9.");
        assert_eq!(option_number(9, 10), "10.");
        assert_eq!(option_number(0, 100), "  1.");
        assert_eq!(option_number(98, 99), "99.");
        assert_eq!(option_number(99, 100), "100.");
        assert_eq!(option_number_width(100), 3);
    }

    /// A multi-choice keeps this project's `[x]` / `[ ]` vocabulary and puts
    /// the stable number before it; focus and selection stay separate marks.
    #[test]
    fn multi_choice_numbers_sit_before_the_selection_box() {
        let mut ov = ClarificationOverlay::new(UiClarificationRequest {
            id: ClarificationId::new("c1"),
            question: "选哪些？".into(),
            options: Vec::new(),
            questions: vec![UiClarificationQuestion {
                header: "范围".into(),
                question: "选哪些？".into(),
                kind: ClarificationQuestionKind::Multi,
                options: vec!["Go".into(), "Python".into()],
                allow_other: false,
                min_choices: 0,
                max_choices: None,
            }],
        });
        ov.on_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()));
        ov.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        let rows = clarification_rows(&ov);
        assert!(rows.iter().any(|r| r.contains("  1. [x] Go")), "{rows:#?}");
        assert!(
            rows.iter().any(|r| r.contains("❯ 2. [ ] Python")),
            "{rows:#?}"
        );
    }

    /// The free-text `其他…` row is part of the same list, so it numbers with
    /// it instead of floating unanchored under the options.
    #[test]
    fn the_other_row_is_numbered_like_an_option() {
        let rows = clarification_rows(&single_choice(&["Go", "Python"]));
        assert!(rows.iter().any(|r| r.contains("  3. 其他…")), "{rows:#?}");
    }
}
