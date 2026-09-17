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
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::composer::Composer;
use crate::state::AppState;
use crate::status_line::{StatusPhase, status_lines, status_phase};

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
}

impl BtwTurn {
    fn streaming(question: String) -> Self {
        Self {
            question,
            answer: String::new(),
            state: BtwTurnState::Streaming,
        }
    }
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

    /// Close the newest streaming turn. `detail` is appended for a failure so
    /// the reason stays on the turn instead of only flashing as a toast.
    pub fn finish(&mut self, state: BtwTurnState, detail: Option<&str>) {
        if let Some(turn) = self.turns.last_mut()
            && turn.state == BtwTurnState::Streaming
        {
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
    let composer_rows = crate::render::composer_visible_rows(state, area.width as usize) as u16;
    // header(1) + gap(1) + body(Min) + composer + hint(1)
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(composer_rows),
        Constraint::Length(1),
    ])
    .split(area);

    // Header: how to get back, plus the main run's canonical status. Reusing
    // `status_lines` / the last turn-end marker means this is a projection of
    // the main state, never a second copy that can drift.
    let mut header: Vec<Span<'static>> = vec![Span::styled(
        format!("{}  ", state.t().btw_back_main),
        Style::default()
            .fg(state.theme.accent.secondary)
            .add_modifier(Modifier::BOLD),
    )];
    header.extend(main_status_spans(state, area.width as usize));
    frame.render_widget(Paragraph::new(Line::from(header)), chunks[0]);

    let body_area = chunks[2];
    let lines = body_lines(state, body_area.width as usize);
    let view = window(&lines, body_area.height as usize, state.btw.scroll);
    frame.render_widget(Paragraph::new(view), body_area);

    // Composer: the same box the main surface uses.
    state.input_rect = Some((chunks[3].x, chunks[3].y, chunks[3].width, chunks[3].height));
    let (box_lines, (cx, cy)) = crate::render::composer_box_lines(state, chunks[3].width as usize);
    let shown: Vec<Line> = box_lines
        .into_iter()
        .take(chunks[3].height as usize)
        .collect();
    frame.render_widget(Paragraph::new(shown), chunks[3]);
    let x = chunks[3].x + cx;
    let y = chunks[3].y + cy;
    if x < chunks[3].x + chunks[3].width && y < chunks[3].y + chunks[3].height {
        frame.set_cursor_position(ratatui::layout::Position::new(x, y));
    }

    let hint = if state.btw.generating {
        state.t().btw_footer_hint_stop
    } else {
        state.t().btw_footer_hint
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                " {}",
                crate::render::truncate_display(hint, area.width as usize)
            ),
            Style::default().fg(state.theme.text.muted),
        ))),
        chunks[4],
    );
}

/// The main run's live status, as spans. Busy / awaiting-user come straight
/// from the shared status strip; an idle surface shows the last turn's own
/// terminal marker, so "Completed" / "Failed" are the runtime's words.
fn main_status_spans(state: &AppState, width: usize) -> Vec<Span<'static>> {
    if (state.is_busy() || status_phase(state) == StatusPhase::AwaitingUser)
        && let Some(line) = status_lines(state, width).into_iter().next()
    {
        return line.spans;
    }
    if state.status == leveler_client_protocol::RuntimeStatus::Error {
        return vec![Span::styled(
            format!("✗ {}", state.t().final_failed),
            Style::default().fg(state.theme.status.error),
        )];
    }
    if let Some(block) = state.transcript.last_turn_end() {
        let item = crate::transcript::TranscriptItem::TurnEnd(block.clone());
        let lines = crate::render::item_render(&item, &state.theme, width, false, state.t());
        if let Some(line) = lines.into_iter().next() {
            return line.spans;
        }
    }
    vec![Span::styled(
        state.t().btw_main_idle,
        Style::default().fg(state.theme.text.muted),
    )]
}

/// Conversation body lines, oldest first.
fn body_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let theme = &state.theme;
    let t = state.t();
    let label = |s: &'static str| {
        Line::from(Span::styled(
            s,
            Style::default()
                .fg(theme.text.muted)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    if state.btw.is_empty() {
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            t.btw_empty.to_string(),
            Style::default().fg(theme.text.secondary),
        )));
        return out;
    }
    let inner = width.saturating_sub(2).max(8);
    for (index, turn) in state.btw.turns.iter().enumerate() {
        if index > 0 {
            out.push(Line::from(""));
        }
        out.push(label(t.btw_you));
        for line in crate::render::text::wrap(&turn.question, inner) {
            out.push(Line::from(Span::styled(
                line,
                Style::default().fg(theme.text.primary),
            )));
        }
        out.push(Line::from(""));
        out.push(label(t.btw_assistant));
        match turn.state {
            BtwTurnState::Streaming if turn.answer.is_empty() => {
                out.push(Line::from(Span::styled(
                    t.btw_answering.to_string(),
                    Style::default().fg(theme.text.secondary),
                )));
            }
            BtwTurnState::Streaming => {
                out.extend(crate::markdown::MdDoc::parse(&turn.answer).to_lines(inner, theme));
            }
            BtwTurnState::Failed => {
                out.push(Line::from(Span::styled(
                    t.btw_failed.to_string(),
                    Style::default().fg(theme.status.error),
                )));
                if !turn.answer.is_empty() {
                    for line in crate::render::text::wrap(&turn.answer, inner) {
                        out.push(Line::from(Span::styled(
                            line,
                            Style::default().fg(theme.status.error),
                        )));
                    }
                }
            }
            BtwTurnState::Cancelled => {
                if !turn.answer.is_empty() {
                    out.extend(crate::markdown::MdDoc::parse(&turn.answer).to_lines(inner, theme));
                }
                out.push(Line::from(Span::styled(
                    t.btw_cancelled.to_string(),
                    Style::default().fg(theme.text.muted),
                )));
            }
            BtwTurnState::Done => {
                out.extend(crate::markdown::MdDoc::parse(&turn.answer).to_lines(inner, theme));
            }
        }
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
