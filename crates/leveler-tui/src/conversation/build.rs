//! Conversation build: transcript (+ live reasoning) → wrapped display lines
//! plus the disclosure hit rows, memoized under one cache key.
//!
//! The hit rows are built in the same pass and cached in the same entry as
//! the lines — a hit can never describe stale lines. This invariant is what
//! makes mouse disclosure clicks trustworthy; do not split the caches.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render::{item_render, items_need_gap, sub_agent_tree_lines};
use crate::state::AppState;
use crate::transcript::TranscriptItem;

use super::ConvKey;

/// How many lines the conversation content needs at `width` (for scroll math).
pub fn conversation_line_count(state: &AppState, width: usize) -> usize {
    state.conversation_lines(width).len()
}

fn wrap_simple(s: &str, width: usize) -> Vec<String> {
    crate::render::wrap(s, width)
}

impl AppState {
    /// Cache-aware conversation lines: re-wraps the whole transcript only when a
    /// render input changed; otherwise returns the previously built lines (an
    /// `Rc` clone, O(1)). The empty/splash case is not cached — the splash reads
    /// repo/branch, which the transcript `version` does not track.
    pub(crate) fn conversation_lines(&self, width: usize) -> std::rc::Rc<Vec<Line<'static>>> {
        self.conversation_lines_and_hits(width).0
    }

    /// Cache-aware lines plus the disclosure hit rows built alongside them.
    /// One cache entry carries both so a hit can never describe stale lines.
    pub(crate) fn conversation_lines_and_hits(
        &self,
        width: usize,
    ) -> (
        std::rc::Rc<Vec<Line<'static>>>,
        std::rc::Rc<Vec<(usize, usize)>>,
    ) {
        if crate::splash::conversation_is_empty(self) {
            let (lines, hits) = build_conversation_lines_with_hits(self, width);
            return (std::rc::Rc::new(lines), std::rc::Rc::new(hits));
        }
        let key = ConvKey {
            version: self.transcript.version(),
            width,
            theme_id: self.theme.id,
            monochrome: self.theme.monochrome,
            locale: self.locale,
            tools_expanded: self.tools_expanded,
            reasoning_expanded: self.reasoning_expanded,
            reasoning: self.reasoning.clone(),
        };
        if let Some((k, lines, hits)) = self.conv.cache.borrow().as_ref()
            && *k == key
        {
            return (lines.clone(), hits.clone());
        }
        let (lines, hits) = build_conversation_lines_with_hits(self, width);
        let (lines, hits) = (std::rc::Rc::new(lines), std::rc::Rc::new(hits));
        *self.conv.cache.borrow_mut() = Some((key, lines.clone(), hits.clone()));
        (lines, hits)
    }

    /// The transcript item behind the disclosure row at `abs_line`, if any.
    pub(crate) fn disclosure_item_at(&self, width: usize, abs_line: usize) -> Option<usize> {
        let (_, hits) = self.conversation_lines_and_hits(width);
        hits.iter()
            .find(|(line, _)| *line == abs_line)
            .map(|(_, item)| *item)
    }
}

#[cfg(test)]
pub fn build_conversation_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    build_conversation_lines_with_hits(state, width).0
}

/// Build the conversation plus its disclosure hit rows: for every finished,
/// non-edit tool group the FIRST emitted line is the clickable `▸/▾` header.
pub fn build_conversation_lines_with_hits(
    state: &AppState,
    width: usize,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let theme = &state.theme;
    let t = state.t();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<(usize, usize)> = Vec::new();

    // Empty session: brand splash (logo + tagline) instead of a blank void.
    if crate::splash::conversation_is_empty(state) {
        return (crate::splash::splash_lines(state, width, theme, t), hits);
    }

    let items = state.transcript.items();
    let mut idx = 0;
    while idx < items.len() {
        let item = &items[idx];
        // /btw is a floating overlay, not scroll content.
        if matches!(item, TranscriptItem::Btw(_)) {
            idx += 1;
            continue;
        }
        // Remember where this item starts: a group whose tools are all Silent
        // (ls / probe runs) renders nothing, and a separator emitted before it
        // would leave a blank gap with no content — the reader sees a hole
        // between their prompt and the answer. The gap is undone below if the
        // item turned out to be invisible.
        let gap_at = (idx > 0 && items_need_gap(&items[idx - 1], item)).then(|| {
            out.push(Line::from(""));
            out.len() - 1
        });
        let before_item = out.len();
        // Message types are distinguished by shape, not role headings:
        // `▌` user prompt, `●` agent prose, status glyphs for tool activity.
        match item {
            TranscriptItem::User(text) => {
                // A solid heading bar + bold body marks the user's turn clearly
                // apart from the assistant's `●` bullet and normal-weight prose.
                let bar = Style::default()
                    .fg(theme.text.primary)
                    .add_modifier(Modifier::BOLD);
                let body = Style::default()
                    .fg(theme.text.primary)
                    .add_modifier(Modifier::BOLD);
                for line in wrap_simple(text, width.saturating_sub(2).max(1)) {
                    out.push(Line::from(vec![
                        Span::styled("▌ ", bar),
                        Span::styled(line, body),
                    ]));
                }
            }
            TranscriptItem::Assistant(_) => {
                out.extend(item_render(item, theme, width, state.tools_expanded, t));
            }
            TranscriptItem::ToolGroup(group) => {
                // Product activity stream — not a raw tool trace:
                // Silent (ls/list_files/probes) stay out; Normal exploration
                // aggregates; Important edits/runs stay one bold line each.
                // A finished, non-edit group's first line is its disclosure
                // row — record it as a click target for this exact item.
                if crate::activity_stream::group_has_disclosure(group) {
                    hits.push((out.len(), idx));
                }
                out.extend(crate::activity_stream::render_group(
                    group,
                    theme,
                    width,
                    state.locale,
                    t,
                    state.elapsed_secs,
                ));
            }
            TranscriptItem::SubAgent(first) => {
                // A run of consecutive sub-agent blocks renders as one tree
                // (aggregate header + ├─/└─ children). Any other item breaks
                // the run — batches split by tool calls stay separate.
                let mut blocks = vec![first];
                while let Some(TranscriptItem::SubAgent(next)) = items.get(idx + 1) {
                    blocks.push(next);
                    idx += 1;
                }
                out.extend(sub_agent_tree_lines(
                    &blocks,
                    theme,
                    width,
                    t,
                    state.elapsed_secs,
                ));
            }
            TranscriptItem::UserShell(shell) => {
                // Every user shell row is a click target: click opens its
                // Shell Details (running or finished) — the reducer
                // dispatches by item type, geometry stays untouched.
                hits.push((out.len(), idx));
                out.extend(crate::render::user_shell_lines(
                    shell,
                    theme,
                    width,
                    t,
                    state.elapsed_secs,
                ));
            }
            _ => {
                out.extend(item_render(item, theme, width, state.tools_expanded, t));
            }
        }
        // The item produced nothing (an all-Silent tool group): take the
        // separator back so no hole is left behind.
        if out.len() == before_item
            && let Some(at) = gap_at
        {
            out.remove(at);
        }
        idx += 1;
    }

    // Live reasoning as activity while in flight.
    if !state.reasoning.is_empty() {
        out.push(Line::from(""));
        let disclosure = if state.reasoning_expanded {
            "▾"
        } else {
            "▸"
        };
        let reasoning_lines: Vec<&str> = state
            .reasoning
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let n = reasoning_lines.len();
        out.push(Line::from(vec![
            Span::styled(
                format!("{disclosure} {} · {n} lines", t.thinking),
                Style::default().fg(theme.text.secondary),
            ),
            Span::styled(
                if state.reasoning_expanded {
                    String::new()
                } else {
                    "  Ctrl+O".to_string()
                },
                Style::default().fg(theme.border.normal),
            ),
        ]));
        if state.reasoning_expanded {
            // Cap body so CoT cannot flood the viewport; show honest remainder.
            const MAX_EXPANDED_REASONING: usize = 24;
            for line in reasoning_lines.iter().take(MAX_EXPANDED_REASONING) {
                out.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(theme.text.secondary),
                )));
            }
            if n > MAX_EXPANDED_REASONING {
                out.push(Line::from(Span::styled(
                    format!("  … (+{} lines)", n - MAX_EXPANDED_REASONING),
                    Style::default().fg(theme.border.normal),
                )));
            }
        }
    }

    (out, hits)
}
