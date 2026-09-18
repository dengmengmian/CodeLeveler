//! 待发送 — what the user wrote while a turn runs and has not sent yet.
//!
//! Bottom control state, never conversation truth. An item leaves this list in
//! exactly two ways: the runtime admits it (and it becomes a user message in
//! the conversation), or the user deletes it while it is still unsent. The
//! list is a FIFO continuation queue: once the runtime is ready for the next
//! turn its head is submitted automatically, oldest first, one at a time
//! (`reducer::submit::drain_pending_input`). Sending reuses the ordinary
//! turn-input delivery (`pending_submissions`, one `CommandId`, at-least-once
//! retries); nothing here is a second delivery path.

use leveler_client_protocol::{CommandId, SessionId};

/// One staged input and where its delivery stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInput {
    pub text: String,
    pub state: PendingInputState,
    /// The session this was written for. The 待发送 area is shared bottom
    /// control state, so an item staged for one session must never be sent —
    /// manually or by the automatic drain — into whichever session is on
    /// screen now.
    pub session_id: SessionId,
}

/// Where a staged input stands.
///
/// The list is a continuation queue, not a draft box: an item written while a
/// turn ran is `Queued`, and the runtime's next ready moment turns it into the
/// next user turn (see `reducer::submit::drain_pending_input`). Only a delivery
/// the client actually attempted can leave this item's outcome unknown; merely
/// waiting in the queue is a known state and must never render as unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingInputState {
    /// Written, not sent. The only state the automatic drain may take.
    Queued,
    /// Sent under this id; the runtime has not answered yet.
    Submitting(CommandId),
    /// Sent, and the first attempt got no answer. Retries carry the same id;
    /// it may already be in the runtime, so it can be neither resent nor
    /// deleted until the runtime answers. This is the only "状态未知" state:
    /// a delivery was attempted and its outcome cannot be determined.
    DeliveryUnknown(CommandId),
    /// The runtime refused it. Still unsent: the user may send or delete it.
    Failed(String),
}

impl PendingInput {
    /// Stage `text` for `session_id`: queued and unsent.
    pub fn queued(text: impl Into<String>, session_id: SessionId) -> Self {
        Self {
            text: text.into(),
            state: PendingInputState::Queued,
            session_id,
        }
    }

    /// The id this item is out under, while it is out.
    pub fn command_id(&self) -> Option<&CommandId> {
        match &self.state {
            PendingInputState::Submitting(id) | PendingInputState::DeliveryUnknown(id) => Some(id),
            _ => None,
        }
    }

    /// Unsent: the only states in which it may be sent or deleted.
    pub fn is_unsent(&self) -> bool {
        matches!(
            self.state,
            PendingInputState::Queued | PendingInputState::Failed(_)
        )
    }

    /// Queued for the next turn: written, never sent, still deleteable. The
    /// automatic drain takes exactly this state, so a delivery already in
    /// flight (`Submitting` / `DeliveryUnknown`) or refused (`Failed`) cannot
    /// be queued up a second time.
    pub fn is_queued(&self) -> bool {
        matches!(self.state, PendingInputState::Queued)
    }
}

/// A painted row of the 待发送 area: screen row, the item it shows (`None`
/// for the "还有 N 条" row), and the column spans of its actions when shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingInputHit {
    pub row: u16,
    pub index: Option<usize>,
    pub send: Option<(u16, u16)>,
    pub delete: Option<(u16, u16)>,
}

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::state::{AppState, WorkbenchFocus};

/// Widest the item's text runs. A queue item is its own surface, so its text
/// stops at a readable measure instead of stretching toward the terminal's
/// right edge — which is what pushed the actions away from what they act on.
const ITEM_TEXT_MAX_WIDTH: usize = 72;

/// Blank cells between the text and its actions. Fixed, so the gap does not
/// grow with the terminal the way a terminal-right-edge alignment does.
const ITEM_ACTION_GAP: usize = 3;

/// How many item rows the area may show at terminal height `height`: enough
/// to act on a few, never enough to crowd out the conversation or composer.
fn visible_cap(height: u16) -> usize {
    match height {
        0..=19 => 1,
        20..=29 => 2,
        30..=39 => 3,
        _ => 5,
    }
}

/// Rows the 待发送 area takes: nothing when empty; a header, the visible
/// items, and one "还有 N 条" row when some are hidden.
pub(crate) fn panel_height(state: &AppState, terminal_height: u16) -> u16 {
    let n = state.pending_inputs.len();
    if n == 0 {
        return 0;
    }
    let shown = n.min(visible_cap(terminal_height));
    (1 + shown + usize::from(shown < n)) as u16
}

/// The first item shown: the window follows the selection, which starts on
/// the most recently written item.
fn window_start(state: &AppState, shown: usize) -> usize {
    let n = state.pending_inputs.len();
    let anchor = state
        .pending_hover
        .unwrap_or(state.pending_selected)
        .min(n.saturating_sub(1));
    anchor
        .saturating_sub(shown.saturating_sub(1))
        .min(n.saturating_sub(shown))
}

/// Paint the area and record its rows for the mouse.
pub(crate) fn render(frame: &mut Frame, area: Rect, state: &mut AppState, terminal_height: u16) {
    state.pending_hits.clear();
    if area.height == 0 || state.pending_inputs.is_empty() {
        return;
    }
    let theme = &state.theme;
    let t = state.t();
    let width = area.width as usize;
    let muted = Style::default().fg(theme.text.muted);
    let n = state.pending_inputs.len();
    let shown = n.min(visible_cap(terminal_height));
    let start = window_start(state, shown);
    let focused = state.workbench_focus == WorkbenchFocus::Pending;

    let mut lines = vec![Line::from(Span::styled(
        t.pending_inputs_title.replace("{}", &n.to_string()),
        muted,
    ))];
    let mut hits = Vec::new();
    for index in start..start + shown {
        let item = &state.pending_inputs[index];
        let row = area.y + lines.len() as u16;
        let active =
            state.pending_hover == Some(index) || (focused && state.pending_selected == index);
        // Actions appear only on the item being pointed at or selected, and
        // only while it is still unsent.
        let (right, actions) = if active && item.is_unsent() {
            (
                format!("{} · {}", t.pending_input_send, t.pending_input_delete),
                true,
            )
        } else {
            let label = match &item.state {
                // The section header already says 待发送; a queued item has no
                // outcome to report yet, so it gets no status column at all.
                PendingInputState::Queued => "",
                PendingInputState::Submitting(_) => t.pending_input_sending,
                PendingInputState::DeliveryUnknown(_) => t.pending_input_unknown,
                PendingInputState::Failed(_) => t.pending_input_failed,
            };
            (label.to_string(), false)
        };
        let right_w = UnicodeWidthStr::width(right.as_str());
        let marker_style = if focused && state.pending_selected == index {
            Style::default().fg(theme.accent.primary)
        } else {
            muted
        };
        let first_line = item.text.lines().next().unwrap_or("");
        let text_w = width
            .saturating_sub(2 + ITEM_ACTION_GAP + right_w)
            .clamp(1, ITEM_TEXT_MAX_WIDTH);
        let text = crate::render::truncate_display(first_line, text_w);
        let used = 2 + UnicodeWidthStr::width(text.as_str());
        let mut spans = vec![
            Span::styled("› ", marker_style),
            Span::styled(text, Style::default().fg(theme.text.primary)),
            Span::raw(" ".repeat(ITEM_ACTION_GAP)),
        ];
        let mut hit = PendingInputHit {
            row,
            index: Some(index),
            send: None,
            delete: None,
        };
        if actions {
            let x = area.x + (used + ITEM_ACTION_GAP) as u16;
            let send_w = UnicodeWidthStr::width(t.pending_input_send) as u16;
            let sep_w = UnicodeWidthStr::width(" · ") as u16;
            let delete_w = UnicodeWidthStr::width(t.pending_input_delete) as u16;
            hit.send = Some((x, x + send_w));
            hit.delete = Some((x + send_w + sep_w, x + send_w + sep_w + delete_w));
            spans.push(Span::styled(
                t.pending_input_send.to_string(),
                Style::default().fg(theme.accent.primary),
            ));
            spans.push(Span::styled(" · ", muted));
            spans.push(Span::styled(t.pending_input_delete.to_string(), muted));
        } else {
            let color = match item.state {
                PendingInputState::DeliveryUnknown(_) | PendingInputState::Failed(_) => {
                    theme.status.warning
                }
                _ => theme.text.muted,
            };
            spans.push(Span::styled(right, Style::default().fg(color)));
        }
        lines.push(Line::from(spans));
        hits.push(hit);
    }
    if shown < n {
        hits.push(PendingInputHit {
            row: area.y + lines.len() as u16,
            index: None,
            send: None,
            delete: None,
        });
        lines.push(Line::from(Span::styled(
            format!(
                "  {}",
                t.pending_inputs_more
                    .replace("{}", &(n - shown).to_string())
            ),
            muted,
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
    state.pending_hits = hits;
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::SessionId;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Render one selected item at `width` and return where its actions land.
    fn actions_at(text: &str, width: u16) -> PendingInputHit {
        let mut state = AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 200_000,
                locale: crate::i18n::Locale::En,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        );
        state
            .pending_inputs
            .push(PendingInput::queued(text, SessionId::new("s1")));
        state.workbench_focus = WorkbenchFocus::Pending;
        state.pending_selected = 0;
        let area = Rect::new(0, 0, width, 4);
        let mut terminal = Terminal::new(TestBackend::new(width, 4)).unwrap();
        terminal
            .draw(|frame| render(frame, area, &mut state, 4))
            .unwrap();
        state.pending_hits[0]
    }

    #[test]
    fn actions_do_not_follow_the_terminal_right_edge() {
        let narrow = actions_at("hi", 80);
        let wide = actions_at("hi", 320);
        assert_eq!(
            narrow.send, wide.send,
            "the action column must not move when the terminal grows"
        );
        let (x, _) = wide.send.expect("the selected item shows its actions");
        assert!(
            x < 40,
            "short text keeps the actions near the text, got column {x}"
        );
    }

    #[test]
    fn long_text_caps_before_the_actions_and_hides_none_of_them() {
        let long = "x".repeat(400);
        let hit = actions_at(&long, 320);
        let (x, end) = hit.send.expect("the selected item shows its actions");
        assert_eq!(
            usize::from(x),
            2 + ITEM_TEXT_MAX_WIDTH + ITEM_ACTION_GAP,
            "text stops at the readable measure, then the fixed gap"
        );
        assert!(end < 320, "the actions stay inside the terminal");
        assert!(
            hit.delete.is_some_and(|(_, de)| de <= x + 40),
            "delete stays part of the same bounded cluster"
        );
    }
}
