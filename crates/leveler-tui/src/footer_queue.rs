//! Prompt Queue panel — waiting user tasks (not Plan, not Activity).
//!
//! Workbench placement: between Conversation and Plan. Hidden when empty.
//! Collapsed by default after idle; auto-expands when the queue changes.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render::truncate_display;
use crate::state::{AppState, InputQueues};

/// Max body rows when expanded (title is separate).
const QUEUE_BODY_MAX: usize = 5;

/// One visible queue row with a status glyph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueueRow {
    pub glyph: &'static str,
    pub text: String,
    /// Index into the waiting list for reorder/delete (`rejected` then `queued`).
    /// `None` for `pending` (already handed to runtime — not reordered).
    pub waiting_index: Option<usize>,
}

/// Flatten queues into display order: pending (sending) → rejected → queued.
pub(crate) fn queue_rows(q: &InputQueues) -> Vec<QueueRow> {
    let mut rows = Vec::with_capacity(q.visible_len());
    for text in &q.pending {
        rows.push(QueueRow {
            glyph: "⟳",
            text: text.replace('\n', " "),
            waiting_index: None,
        });
    }
    let mut wi = 0usize;
    for text in &q.rejected {
        rows.push(QueueRow {
            glyph: "↻",
            text: text.replace('\n', " "),
            waiting_index: Some(wi),
        });
        wi += 1;
    }
    for text in &q.queued {
        rows.push(QueueRow {
            glyph: "○",
            text: text.replace('\n', " "),
            waiting_index: Some(wi),
        });
        wi += 1;
    }
    rows
}

/// Height reserved in the workbench layout (0 when empty).
pub(crate) fn queue_panel_height(state: &AppState) -> u16 {
    if state.input_queues.is_empty() {
        return 0;
    }
    if state.queue_collapsed {
        return 1;
    }
    let n = state.input_queues.visible_len();
    let body = n.min(QUEUE_BODY_MAX);
    let more = if n > QUEUE_BODY_MAX { 1 } else { 0 };
    // Action hint sits under the list whenever there is something to act on.
    let hint = if state.input_queues.waiting_len() > 0 {
        1
    } else {
        0
    };
    // title + body rows (+ overflow marker) (+ action hint)
    (1 + body + more + hint) as u16
}

/// Full panel lines for the workbench Queue region.
pub(crate) fn queue_panel_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    if state.input_queues.is_empty() {
        return Vec::new();
    }
    let theme = &state.theme;
    let t = state.t();
    let n = state.input_queues.visible_len();
    let disclosure = if state.queue_collapsed { "▶" } else { "▼" };
    let mut title = format!("{disclosure} Queue ({n})");
    if !state.input_queues.rejected.is_empty() {
        title.push_str(&t.queue_retry_n.replacen(
            "{}",
            &state.input_queues.rejected.len().to_string(),
            1,
        ));
    }
    let mut lines = vec![Line::from(Span::styled(
        truncate_display(&title, width),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ))];

    if state.queue_collapsed {
        return lines;
    }

    let rows = queue_rows(&state.input_queues);
    let max_body = QUEUE_BODY_MAX;
    let scroll = state.queue_scroll.min(rows.len().saturating_sub(max_body));
    let end = (scroll + max_body).min(rows.len());
    let sel = state.queue_selected;

    for (i, row) in rows[scroll..end].iter().enumerate() {
        let abs = scroll + i;
        let selected = sel == Some(abs);
        let color = match row.glyph {
            "⟳" => theme.accent,
            "↻" => theme.warning,
            _ => {
                if selected {
                    theme.text
                } else {
                    theme.muted
                }
            }
        };
        let mark = if selected { "→" } else { " " };
        let body = format!("{mark}{} {}", row.glyph, row.text);
        lines.push(Line::from(Span::styled(
            truncate_display(&body, width),
            Style::default().fg(color).add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        )));
    }
    if rows.len() > max_body {
        let more = rows.len() - end;
        if more > 0 {
            lines.push(Line::from(Span::styled(
                truncate_display(&format!("  … +{more}"), width),
                Style::default().fg(theme.border),
            )));
        }
    }
    // Discoverable actions for the selected/next waiting item.
    if state.input_queues.waiting_len() > 0 {
        lines.push(Line::from(Span::styled(
            truncate_display(t.queue_actions_hint, width),
            Style::default().fg(theme.border),
        )));
    }
    lines
}

/// Legacy one-line summary (inline footer / tests). Prefers the multi-line panel
/// for workbench; this remains for the old conversation_footer path.
pub(crate) fn queued_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    // Workbench owns the panel; keep a compact fallback for any legacy paint.
    let panel = queue_panel_lines(state, width);
    if panel.is_empty() {
        return Vec::new();
    }
    // Compact: first line only for very tight footers when collapsed.
    if state.queue_collapsed || panel.len() == 1 {
        return panel;
    }
    panel
}

impl InputQueues {
    /// Remove waiting item by flat index (rejected then queued).
    pub fn remove_waiting_at(&mut self, index: usize) -> Option<String> {
        let rej_n = self.rejected.len();
        if index < rej_n {
            Some(self.rejected.remove(index))
        } else {
            let i = index - rej_n;
            if i < self.queued.len() {
                Some(self.queued.remove(i))
            } else {
                None
            }
        }
    }

    /// Promote the waiting item at flat `index` to the front of the queue so it
    /// drains next. Returns its new flat waiting index, or `None` if invalid.
    pub fn promote_waiting_to_front(&mut self, index: usize) -> Option<usize> {
        let text = self.remove_waiting_at(index)?;
        self.queued.insert(0, text);
        Some(self.rejected.len())
    }

    /// Move waiting item at flat index by `delta` (-1 up / +1 down).
    /// Returns the new flat index, or `None` if index was invalid.
    pub fn move_waiting(&mut self, index: usize, delta: i32) -> Option<usize> {
        let rej_n = self.rejected.len();
        let mut waiting: Vec<String> = std::mem::take(&mut self.rejected);
        waiting.append(&mut self.queued);
        if index >= waiting.len() {
            let mut it = waiting.into_iter();
            self.rejected = it.by_ref().take(rej_n).collect();
            self.queued = it.collect();
            return None;
        }
        let target = index as i32 + delta;
        if target < 0 || target as usize >= waiting.len() {
            let mut it = waiting.into_iter();
            self.rejected = it.by_ref().take(rej_n).collect();
            self.queued = it.collect();
            return Some(index);
        }
        waiting.swap(index, target as usize);
        let mut it = waiting.into_iter();
        self.rejected = it.by_ref().take(rej_n).collect();
        self.queued = it.collect();
        Some(target as usize)
    }
}

/// Clamp selection/scroll after queue mutations.
pub(crate) fn normalize_queue_focus(state: &mut AppState) {
    let n = state.input_queues.visible_len();
    if n == 0 {
        state.queue_selected = None;
        state.queue_scroll = 0;
        return;
    }
    if let Some(sel) = state.queue_selected {
        // pending rows are first and not selectable for reorder; clamp to waiting.
        let pending_n = state.input_queues.pending.len();
        let waiting_n = state.input_queues.waiting_len();
        if waiting_n == 0 {
            state.queue_selected = None;
        } else {
            let min_sel = pending_n;
            let max_sel = pending_n + waiting_n - 1;
            state.queue_selected = Some(sel.clamp(min_sel, max_sel));
        }
    }
    let body = n.min(QUEUE_BODY_MAX);
    let max_scroll = n.saturating_sub(body);
    if state.queue_scroll > max_scroll {
        state.queue_scroll = max_scroll;
    }
}

/// Auto-expand queue panel when content changes (design: expand once on change).
pub(crate) fn on_queue_changed(state: &mut AppState) {
    if !state.input_queues.is_empty() {
        state.queue_collapsed = false;
    }
    normalize_queue_focus(state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Boot;
    use crate::theme::Theme;
    use leveler_client_protocol::SessionId;

    fn line_str(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    fn test_state() -> AppState {
        AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
            },
        )
    }

    #[test]
    fn empty_queue_renders_nothing() {
        let state = test_state();
        assert!(queue_panel_lines(&state, 80).is_empty());
        assert_eq!(queue_panel_height(&state), 0);
    }

    #[test]
    fn expanded_panel_lists_items_with_glyphs() {
        let mut state = test_state();
        state.queue_collapsed = false;
        state.input_queues.rejected = vec!["retry-me".into()];
        state.input_queues.pending = vec!["sending".into()];
        state.input_queues.queued = vec!["queued-a".into(), "queued-b".into()];

        let joined = queue_panel_lines(&state, 80)
            .iter()
            .map(line_str)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(joined.contains("Queue (4)"), "{joined}");
        assert!(
            joined.contains("⟳") && joined.contains("sending"),
            "{joined}"
        );
        assert!(
            joined.contains("↻") && joined.contains("retry-me"),
            "{joined}"
        );
        assert!(
            joined.contains("○") && joined.contains("queued-a"),
            "{joined}"
        );
        assert!(joined.contains("queued-b"), "{joined}");
    }

    #[test]
    fn collapsed_panel_is_title_only() {
        let mut state = test_state();
        state.input_queues.queued = vec!["a".into(), "b".into()];
        state.queue_collapsed = true;
        let lines = queue_panel_lines(&state, 80);
        assert_eq!(lines.len(), 1);
        assert!(line_str(&lines[0]).contains("Queue (2)"));
        assert_eq!(queue_panel_height(&state), 1);
    }

    #[test]
    fn move_waiting_reorders_queued() {
        let mut q = InputQueues {
            queued: vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string(),
            ],
            ..Default::default()
        };
        assert_eq!(q.move_waiting(0, 1), Some(1));
        assert_eq!(
            q.queued,
            vec![
                "second".to_string(),
                "first".to_string(),
                "third".to_string()
            ]
        );
    }

    #[test]
    fn promote_moves_item_to_front() {
        let mut q = InputQueues {
            queued: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            ..Default::default()
        };
        assert_eq!(q.promote_waiting_to_front(2), Some(0));
        assert_eq!(
            q.queued,
            vec!["c".to_string(), "a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn expanded_panel_shows_action_hint() {
        let mut state = test_state();
        state.queue_collapsed = false;
        state.input_queues.queued = vec!["item".into()];
        let joined = queue_panel_lines(&state, 80)
            .iter()
            .map(line_str)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("马上开始") && joined.contains("取消"),
            "action hint missing: {joined}"
        );
    }

    #[test]
    fn next_item_order_pending_then_rejected_then_queued() {
        let q = InputQueues {
            pending: vec!["p".to_string()],
            rejected: vec!["r".to_string()],
            queued: vec!["q".to_string()],
        };
        let rows = queue_rows(&q);
        assert_eq!(rows[0].glyph, "⟳");
        assert_eq!(rows[1].glyph, "↻");
        assert_eq!(rows[2].glyph, "○");
    }
}
