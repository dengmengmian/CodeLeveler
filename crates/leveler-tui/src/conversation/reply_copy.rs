//! Inline actions for a finished, classified assistant reply.

use std::time::{Duration, Instant};

use leveler_client_protocol::{MessageId, SessionId};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::action::Effect;
use crate::clipboard::{CopyReceipt, Delivery};
use crate::i18n::UiText;
use crate::state::{AppState, WorkbenchFocus};
use crate::theme::Theme;
use crate::transcript::TranscriptItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyFormat {
    Markdown,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTarget {
    pub message_id: MessageId,
    pub format: ReplyFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feedback {
    Copied,
    Sent,
    Failed,
}

impl Feedback {
    fn label(self, width: usize, t: &UiText) -> &'static str {
        match (self, width >= 8) {
            (Self::Copied, true) => t.reply_copied,
            (Self::Sent, true) => t.reply_sent,
            (Self::Failed, true) => t.reply_failed,
            (Self::Copied, false) => "[✓]",
            (Self::Sent, false) => "[→]",
            (Self::Failed, false) => "[×]",
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ReplyCopyView {
    hits: Vec<(Rect, ReplyTarget)>,
    pointer: Option<(u16, u16)>,
    focus: Option<ReplyTarget>,
    pending: bool,
    feedback: Option<(ReplyTarget, Feedback, Instant)>,
}

impl ReplyCopyView {
    pub fn reset_hits(&mut self) {
        self.hits.clear();
        self.focus = None;
        self.pointer = None;
    }

    pub fn observe_pointer(&mut self, x: u16, y: u16) {
        self.pointer = Some((x, y));
    }

    pub fn hit(&self, x: u16, y: u16) -> Option<ReplyTarget> {
        self.hits
            .iter()
            .find(|(rect, _)| rect.contains((x, y).into()))
            .map(|(_, target)| target.clone())
    }

    pub fn clear_focus(&mut self) {
        self.focus = None;
    }

    pub fn advance_focus(&mut self) -> bool {
        let next = self
            .focus
            .as_ref()
            .and_then(|focus| self.hits.iter().position(|(_, target)| target == focus))
            .map_or(0, |index| index + 1);
        self.focus = self.hits.get(next).map(|(_, target)| target.clone());
        self.focus.is_some()
    }

    pub fn focused(&self) -> Option<ReplyTarget> {
        self.focus.clone()
    }

    pub fn clear_feedback(&mut self) {
        self.feedback = None;
    }

    pub fn expire_feedback(&mut self, now: Instant) -> bool {
        if self
            .feedback
            .as_ref()
            .is_some_and(|(_, _, deadline)| now >= *deadline)
        {
            self.clear_feedback();
            return true;
        }
        false
    }
}

pub(crate) fn request(state: &mut AppState, target: ReplyTarget) -> Vec<Effect> {
    if state.conv.reply_copy.pending {
        return Vec::new();
    }
    let Some(block) = state.transcript.items().iter().find_map(|item| match item {
        TranscriptItem::Assistant(block)
            if block.id == target.message_id && block.is_copyable_reply() =>
        {
            Some(block)
        }
        _ => None,
    }) else {
        // The history may have been replaced since the action was painted.
        return Vec::new();
    };
    let text = match target.format {
        ReplyFormat::Markdown => block.text.clone(),
        ReplyFormat::Text => crate::markdown::reply_plain_text(&block.text),
    };
    state.conv.reply_copy.pending = true;
    vec![Effect::CopyReply {
        session_id: state.session_id.clone(),
        target,
        text,
    }]
}

pub(crate) fn finish(
    state: &mut AppState,
    session_id: SessionId,
    target: ReplyTarget,
    result: Result<CopyReceipt, String>,
) {
    let view = &mut state.conv.reply_copy;
    view.pending = false;
    if session_id == state.session_id {
        let feedback = match result {
            Ok(receipt) if receipt.delivery == Delivery::VerifiedNative => Feedback::Copied,
            Ok(_) => Feedback::Sent,
            Err(error) => {
                tracing::warn!(%error, "reply clipboard write failed");
                Feedback::Failed
            }
        };
        view.feedback = Some((target, feedback, Instant::now() + Duration::from_secs(2)));
    }
}

type ActionLabel = (ReplyFormat, &'static str);

struct ActionRow {
    prefix: &'static str,
    first: Option<ActionLabel>,
    second: Option<ActionLabel>,
}

impl ActionRow {
    // Share exactly the same columns between the projection and paint-time hits.
    fn labels(&self) -> impl Iterator<Item = (usize, ActionLabel)> {
        let mut col = 0;
        if !self.prefix.is_empty() {
            col = self.prefix.width() + 2;
        }
        [self.first, self.second]
            .into_iter()
            .flatten()
            .map(move |label| {
                let start = col;
                col += label.1.width() + 2;
                (start, label)
            })
    }
}

fn layout(width: usize, t: &UiText) -> Vec<ActionRow> {
    let prefix = t.reply_actions_prefix;
    let row = |prefix, first, second| ActionRow {
        prefix,
        first,
        second,
    };
    for (md, text) in [
        (t.reply_copy_markdown, t.reply_copy_text),
        (t.reply_copy_markdown_short, t.reply_copy_text_short),
        (t.reply_copy_markdown_min, t.reply_copy_text_min),
    ] {
        let md_width = md.width();
        let text_width = text.width();
        let md = Some((ReplyFormat::Markdown, md));
        let text = Some((ReplyFormat::Text, text));
        if width >= prefix.width() + md_width + text_width + 4 {
            return vec![row(prefix, md, text)];
        }
        if width >= md_width.max(text_width).max(prefix.width()) {
            return if width >= prefix.width() + md_width + 2 {
                vec![row(prefix, md, None), row("", text, None)]
            } else if width >= md_width + text_width + 2 {
                vec![row(prefix, None, None), row("", md, text)]
            } else {
                vec![
                    row(prefix, None, None),
                    row("", md, None),
                    row("", text, None),
                ]
            };
        }
    }
    Vec::new()
}

pub(crate) fn rows(width: usize, theme: &Theme, t: &UiText) -> impl Iterator<Item = Line<'static>> {
    let style = Style::default().fg(theme.text.secondary);
    layout(width, t).into_iter().map(move |row| {
        let mut spans = Vec::with_capacity(5);
        if !row.prefix.is_empty() {
            spans.push(Span::styled(row.prefix, style));
        }
        for (_, (_, label)) in row.labels() {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(label, style));
        }
        Line::from(spans)
    })
}

/// Paint only visible labels and publish the geometry that was actually drawn.
pub(crate) fn paint(
    state: &mut AppState,
    item_spans: &[(usize, usize)],
    content: Rect,
    scroll: usize,
    lines: &mut [Line<'static>],
) {
    let t = state.t();
    let view = &mut state.conv.reply_copy;
    view.hits.clear();
    let layout = layout(content.width as usize, t);
    if layout.is_empty() {
        view.clear_focus();
        return;
    }
    let keyboard = state.workbench_focus == WorkbenchFocus::Conversation;
    for (index, &(start, end)) in item_spans.iter().enumerate() {
        if end <= scroll || start >= scroll + lines.len() {
            continue;
        }
        let Some(TranscriptItem::Assistant(block)) = state.transcript.items().get(index) else {
            continue;
        };
        if !block.is_copyable_reply() || end.saturating_sub(start) <= layout.len() {
            continue;
        }
        let first = end - layout.len();
        for (offset, row) in layout.iter().enumerate() {
            let Some(line_index) = (first + offset).checked_sub(scroll) else {
                continue;
            };
            let Some(line) = lines.get_mut(line_index) else {
                continue;
            };
            let first_label = if row.prefix.is_empty() { 0 } else { 2 };
            for ((col, (format, label)), span) in row
                .labels()
                .zip(line.spans.iter_mut().skip(first_label).step_by(2))
            {
                let target = ReplyTarget {
                    message_id: block.id.clone(),
                    format,
                };
                let rect = Rect::new(
                    content.x + col as u16,
                    content.y + line_index as u16,
                    label.width() as u16,
                    1,
                );
                if !state.conv.selection.is_active()
                    && (view.pointer.is_some_and(|p| rect.contains(p.into()))
                        || (keyboard && view.focus.as_ref() == Some(&target)))
                {
                    span.style = span
                        .style
                        .fg(state.theme.text.primary)
                        .add_modifier(Modifier::UNDERLINED);
                }
                if let Some((_, feedback, _)) =
                    view.feedback.as_ref().filter(|(id, _, _)| id == &target)
                {
                    let feedback = feedback.label(label.width(), t);
                    let padding = " ".repeat(label.width().saturating_sub(feedback.width()));
                    span.content = format!("{feedback}{padding}").into();
                }
                view.hits.push((rect, target));
            }
        }
    }
    if !keyboard
        || !view
            .hits
            .iter()
            .any(|(_, target)| view.focus.as_ref() == Some(target))
    {
        view.clear_focus();
    }
}
