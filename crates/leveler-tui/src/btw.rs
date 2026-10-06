//! The `/btw` side thread: a conversation surface of its own.
//!
//! This is deliberately NOT a card inside the main transcript. The main run
//! stays the authoritative task execution; this surface observes it and keeps
//! its own conversation. The model conversation and cancellation live in the
//! runtime — this module is presentation, built from the runtime's `Btw*`
//! events.
//!
//! Ownership:
//!
//! ```text
//! Main run            → authoritative task execution (untouched by /btw)
//! SurfaceFocus::Main  → the main workbench
//! SurfaceFocus::Btw   → this side thread
//! Main live state     → read-only projection shared by both surfaces
//! ```

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::composer::Composer;
use crate::state::AppState;

/// Which conversation surface owns the viewport and the composer.
///
/// One field decides both, so the two can never disagree about who is focused
/// or who receives a keystroke. Orthogonal to [`crate::screen::Screen`]: the
/// screen says which top-level view is up, the surface says which conversation
/// that view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SurfaceFocus {
    #[default]
    Main,
    Btw,
}

/// Terminal state of one side-thread turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtwTurnState {
    Streaming,
    Done,
    /// Stopped by the user before it finished (Ctrl+C). Not a failure.
    Cancelled,
    Failed,
}

/// One question and its answer on the side thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtwTurn {
    pub question: String,
    pub answer: String,
    pub state: BtwTurnState,
    /// Read-only tool calls made while answering, in call order.
    pub tools: Vec<BtwToolActivity>,
}

impl BtwTurn {
    fn streaming(question: String) -> Self {
        Self {
            question,
            answer: String::new(),
            state: BtwTurnState::Streaming,
            tools: Vec::new(),
        }
    }
}

/// Where one side-question tool call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtwToolState {
    Running,
    Finished,
    Error,
    /// The side turn reached a terminal state before this call reported its
    /// own outcome (stop, failure, or a lost event). Never shown as running.
    Interrupted,
}

/// One read-only tool call made while answering a side question.
///
/// Activity, not a record: this says what the observer reached for and how
/// long it took. It never leaves the side surface and is never persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtwToolActivity {
    pub call_id: String,
    pub tool: String,
    pub state: BtwToolState,
    /// Measured duration. Meaningful only once the call reported its outcome.
    pub elapsed_ms: u64,
}

/// The side thread's presentation state: its turns, its own composer draft,
/// and whether an answer is streaming.
///
/// The runtime owns the model conversation and the cancel handle. This is what
/// the surface draws.
#[derive(Debug, Default)]
pub struct BtwThread {
    pub turns: Vec<BtwTurn>,
    /// The side thread's own input buffer. Swapped into `AppState.composer`
    /// while [`SurfaceFocus::Btw`] owns the keyboard, so the one composer
    /// implementation serves both surfaces without leaking a draft across.
    pub draft: Composer,
    /// True while an answer is streaming. Set by `BtwStarted`, cleared by any
    /// terminal `Btw*` event — never by the view being hidden, so navigation
    /// cannot strand or drop an answer.
    pub generating: bool,
    /// Lines scrolled up from the live bottom (0 = newest).
    pub scroll: usize,
}

impl BtwThread {
    /// Whether this side thread has anything to show.
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// The user asked a question: open a streaming turn. Mirrors the runtime's
    /// `BtwStarted`, so this is called from the event, not from submit.
    pub fn begin(&mut self, question: String) {
        self.turns.push(BtwTurn::streaming(question));
        self.generating = true;
        self.scroll = 0;
    }

    pub fn append(&mut self, delta: &str) {
        if let Some(turn) = self.turns.last_mut()
            && turn.state == BtwTurnState::Streaming
        {
            turn.answer.push_str(delta);
        }
    }

    /// The side question started a read-only tool call. Recorded on the
    /// streaming turn only; a call that arrives with no streaming turn has no
    /// answer to belong to and is dropped rather than attached to an old one.
    pub fn tool_started(&mut self, call_id: String, tool: String) {
        if let Some(turn) = self.streaming_turn() {
            turn.tools.push(BtwToolActivity {
                call_id,
                tool,
                state: BtwToolState::Running,
                elapsed_ms: 0,
            });
        }
    }

    /// That call's outcome. Matched by `call_id` against a call this view saw
    /// start; an unmatched finish is ignored instead of inventing activity.
    pub fn tool_finished(&mut self, call_id: &str, is_error: bool, elapsed_ms: u64) {
        if let Some(turn) = self.streaming_turn()
            && let Some(activity) = turn.tools.iter_mut().find(|activity| {
                activity.call_id == call_id && activity.state == BtwToolState::Running
            })
        {
            activity.state = if is_error {
                BtwToolState::Error
            } else {
                BtwToolState::Finished
            };
            activity.elapsed_ms = elapsed_ms;
        }
    }

    fn streaming_turn(&mut self) -> Option<&mut BtwTurn> {
        self.turns
            .last_mut()
            .filter(|turn| turn.state == BtwTurnState::Streaming)
    }

    /// Close the newest streaming turn. `detail` is appended for a failure so
    /// the reason stays on the turn instead of only flashing as a toast.
    ///
    /// Any tool call still running at this point is closed too: a terminal
    /// side state must never leave a call rendered as in flight.
    pub fn finish(&mut self, state: BtwTurnState, detail: Option<&str>) {
        if let Some(turn) = self.turns.last_mut()
            && turn.state == BtwTurnState::Streaming
        {
            for activity in &mut turn.tools {
                if activity.state == BtwToolState::Running {
                    activity.state = BtwToolState::Interrupted;
                }
            }
            if let Some(detail) = detail {
                if !turn.answer.is_empty() {
                    turn.answer.push('\n');
                }
                turn.answer.push_str(detail);
            }
            turn.state = state;
        }
        self.generating = false;
    }
}

/// Draw the side thread over `area`. `state.composer` is the side thread's
/// draft while this surface is focused.
pub fn render(frame: &mut Frame, area: Rect, state: &mut AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let inner_width = area
        .width
        .saturating_sub(crate::secondary::PADDING_X.saturating_mul(2));
    let composer_rows = crate::render::composer_visible_rows(state, inner_width as usize) as u16;
    let page = crate::secondary::SecondaryPage {
        title: state.t().btw_surface_title,
        // The main run's canonical status, a projection of the same shared
        // strip the workbench paints — never a second copy that can drift.
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: if state.btw.generating {
            state.t().btw_footer_hint_stop
        } else {
            state.t().btw_footer_hint
        },
    };
    let layout = crate::secondary::layout(area, composer_rows);
    crate::secondary::draw_header(frame, &layout, &page, &state.theme);

    let lines = body_lines(state, layout.content.width as usize);
    let view = window(&lines, layout.content.height as usize, state.btw.scroll);
    frame.render_widget(Paragraph::new(view), layout.content);

    // Composer: the same box the main surface uses, inside the page padding.
    let composer = layout.composer;
    if composer.height > 0 {
        state.input_rect = Some((composer.x, composer.y, composer.width, composer.height));
        let (box_lines, (cx, cy)) =
            crate::render::composer_box_lines(state, composer.width as usize);
        let shown: Vec<Line> = box_lines
            .into_iter()
            .take(composer.height as usize)
            .collect();
        frame.render_widget(Paragraph::new(shown), composer);
        let x = composer.x + cx;
        let y = composer.y + cy;
        if x < composer.x + composer.width && y < composer.y + composer.height {
            frame.set_cursor_position(ratatui::layout::Position::new(x, y));
        }
    }

    crate::secondary::draw_footer(frame, &layout, &page, &state.theme);
}

/// Role marker for a user turn.
const USER_MARKER: char = '›';
/// Role marker for an assistant turn.
const ASSISTANT_MARKER: char = '●';
/// The marker glyph plus the one-cell gap after it. Continuation text aligns
/// under this many columns.
const MARKER_PREFIX: usize = 2;

fn marker_style(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// Conversation body lines, oldest first.
///
/// Roles are carried by a light marker — `›` for the user, `●` for the
/// assistant — plus spacing, never by a "You"/"Assistant" title. Wrapped
/// continuation lines align under the message text so one message reads as one
/// block, and wrapping is by terminal cell width (CJK-safe).
fn body_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let theme = &state.theme;
    let t = state.t();
    let mut out: Vec<Line<'static>> = Vec::new();
    if state.btw.is_empty() {
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            t.btw_empty.to_string(),
            Style::default().fg(theme.text.secondary),
        )));
        return out;
    }
    let body = Style::default().fg(theme.text.primary);
    for (index, turn) in state.btw.turns.iter().enumerate() {
        if index > 0 {
            out.push(Line::from(""));
        }
        out.extend(marked_wrapped(
            USER_MARKER,
            theme.accent.primary,
            &turn.question,
            width,
            body,
        ));
        out.push(Line::from(""));
        out.extend(tool_lines(turn, theme, t, width));
        match turn.state {
            BtwTurnState::Streaming if turn.answer.is_empty() => {
                // While a tool call is the only thing happening, the activity
                // IS the progress; the bare "answering" note would hide it.
                if turn.tools.is_empty() {
                    out.extend(marked_wrapped(
                        ASSISTANT_MARKER,
                        theme.accent.secondary,
                        t.btw_answering,
                        width,
                        Style::default().fg(theme.text.secondary),
                    ));
                }
            }
            BtwTurnState::Streaming | BtwTurnState::Done => {
                out.extend(marked_markdown(
                    ASSISTANT_MARKER,
                    theme.accent.secondary,
                    &turn.answer,
                    width,
                    theme,
                ));
            }
            BtwTurnState::Failed => {
                out.extend(marked_wrapped(
                    ASSISTANT_MARKER,
                    theme.accent.secondary,
                    t.btw_failed,
                    width,
                    Style::default().fg(theme.status.error),
                ));
                if !turn.answer.is_empty() {
                    out.extend(marked_markdown(
                        ASSISTANT_MARKER,
                        theme.accent.secondary,
                        &turn.answer,
                        width,
                        theme,
                    ));
                }
            }
            BtwTurnState::Cancelled => {
                if !turn.answer.is_empty() {
                    out.extend(marked_markdown(
                        ASSISTANT_MARKER,
                        theme.accent.secondary,
                        &turn.answer,
                        width,
                        theme,
                    ));
                }
                out.push(marked_note(
                    ASSISTANT_MARKER,
                    theme.accent.secondary,
                    t.btw_cancelled,
                    theme,
                ));
            }
        }
    }
    out
}

/// At most this many tool activity lines per side answer. The answer is the
/// point; activity is supporting chrome.
const MAX_TOOL_LINES: usize = 5;

/// The read-only activity behind one side answer, oldest hidden first.
///
/// Only the newest [`MAX_TOOL_LINES`] calls are shown; the rest collapse into
/// one count line above them, so live activity stays visible instead of an
/// ever-growing list pushing the answer down.
fn tool_lines(
    turn: &BtwTurn,
    theme: &crate::theme::Theme,
    t: &crate::i18n::UiText,
    width: usize,
) -> Vec<Line<'static>> {
    if turn.tools.is_empty() {
        return Vec::new();
    }
    let hidden = turn.tools.len().saturating_sub(MAX_TOOL_LINES);
    let mut out: Vec<Line<'static>> = Vec::new();
    if hidden > 0 {
        out.push(Line::from(vec![
            Span::raw(" ".repeat(MARKER_PREFIX)),
            Span::styled(
                t.btw_tool_hidden.replace("{n}", &hidden.to_string()),
                Style::default().fg(theme.text.muted),
            ),
        ]));
    }
    for activity in turn.tools.iter().skip(hidden) {
        let (glyph, color) = match activity.state {
            BtwToolState::Running => ('◌', theme.accent.primary),
            BtwToolState::Finished => ('✓', theme.status.success),
            BtwToolState::Error => ('✗', theme.status.error),
            BtwToolState::Interrupted => ('?', theme.status.warning),
        };
        let detail = match activity.state {
            BtwToolState::Running => t.tool_status_running.to_string(),
            BtwToolState::Interrupted => t.btw_tool_interrupted.to_string(),
            BtwToolState::Finished | BtwToolState::Error => {
                format!("{}ms", activity.elapsed_ms)
            }
        };
        out.extend(marked_wrapped(
            glyph,
            color,
            &format!("{} · {detail}", activity.tool),
            width,
            Style::default().fg(theme.text.muted),
        ));
    }
    out
}

/// A marker plus plain text, wrapped to the pane and aligned under the text.
fn marked_wrapped(
    marker: char,
    marker_color: Color,
    text: &str,
    width: usize,
    body: Style,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(MARKER_PREFIX).max(1);
    let wrapped = crate::render::text::wrap(text, inner);
    if wrapped.is_empty() {
        return vec![Line::from(vec![
            Span::styled(format!("{marker} "), marker_style(marker_color)),
            Span::styled(String::new(), body),
        ])];
    }
    let mut out: Vec<Line<'static>> = Vec::with_capacity(wrapped.len());
    for (i, segment) in wrapped.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(2);
        if i == 0 {
            spans.push(Span::styled(
                format!("{marker} "),
                marker_style(marker_color),
            ));
        } else {
            spans.push(Span::raw(" ".repeat(MARKER_PREFIX)));
        }
        spans.push(Span::styled(segment, body));
        out.push(Line::from(spans));
    }
    out
}

/// A marker plus a markdown answer, indented under the marker.
fn marked_markdown(
    marker: char,
    marker_color: Color,
    text: &str,
    width: usize,
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(MARKER_PREFIX).max(1);
    let blocks = crate::markdown::MdDoc::parse(text).to_lines(inner, theme);
    indent_under_marker(marker, marker_color, blocks)
}

/// A single marked line (status word), no wrapping.
fn marked_note(
    marker: char,
    marker_color: Color,
    text: &str,
    theme: &crate::theme::Theme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{marker} "), marker_style(marker_color)),
        Span::styled(text.to_string(), Style::default().fg(theme.text.muted)),
    ])
}

/// Indent pre-built lines under the marker: the first line carries the glyph,
/// every later line is padded so the block stays aligned.
fn indent_under_marker(
    marker: char,
    marker_color: Color,
    blocks: Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::with_capacity(blocks.len());
    for (i, line) in blocks.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 1);
        if i == 0 {
            spans.push(Span::styled(
                format!("{marker} "),
                marker_style(marker_color),
            ));
        } else {
            spans.push(Span::raw(" ".repeat(MARKER_PREFIX)));
        }
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    out
}

/// Show the bottom `height` lines, lifted by `scroll` from the newest edge.
fn window(lines: &[Line<'static>], height: usize, scroll: usize) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    let total = lines.len();
    let max_scroll = total.saturating_sub(height);
    let scroll = scroll.min(max_scroll);
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(height);
    lines[start..end].to_vec()
}
