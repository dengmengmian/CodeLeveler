//! Style-only highlight of the query inside the transcript projection.
//!
//! The highlight is painted onto the frame's window AFTER the projection is
//! built: it never changes a line's text, its wrapping, or the scroll anchor,
//! so opening a result cannot move the conversation around. Only the located
//! transcript entry (or the located tool call inside it) is touched; every
//! other fold and every other row is left exactly as the reader left it.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::conversation::view::SourceSpan;
use crate::find::matching;
use crate::state::AppState;

/// Paint the remembered find highlight over the visible window.
pub fn paint(
    state: &AppState,
    item_spans: &[(usize, usize)],
    sources: &[SourceSpan],
    scroll: usize,
    lines: &mut [Line<'static>],
) {
    let Some(highlight) = &state.find_highlight else {
        return;
    };
    if highlight.query.trim().is_empty() {
        return;
    }
    let ranges: Vec<(usize, usize)> = if let Some(call) = &highlight.call {
        sources
            .iter()
            .filter(|source| source.item == highlight.item && source.call.as_ref() == Some(call))
            .map(|source| (source.start, source.end))
            .collect()
    } else {
        item_spans
            .get(highlight.item)
            .copied()
            .map(|(start, end)| vec![(start, end)])
            .unwrap_or_default()
    };
    if ranges.is_empty() {
        return;
    }
    let style = Style::default()
        .bg(state.theme.surface.selection)
        .fg(state.theme.accent.primary)
        .add_modifier(Modifier::BOLD);
    for (offset, line) in lines.iter_mut().enumerate() {
        let row = scroll + offset;
        if !ranges
            .iter()
            .any(|(start, end)| *start <= row && row < *end)
        {
            continue;
        }
        *line = highlight_line(
            std::mem::replace(line, Line::from("")),
            &highlight.query,
            style,
        );
    }
}

/// Split one painted line at every occurrence of `query` and apply `style` to
/// the matched segments only. Span styles the line already carried are kept on
/// the unmatched text.
fn highlight_line(line: Line<'static>, query: &str, style: Style) -> Line<'static> {
    let plain: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let matches = matching::find_all(&plain, query, 64);
    if matches.is_empty() {
        return line;
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut at = 0usize;
    for found in matches {
        if at < found.start {
            push_slice(&line, at, found.start, None, &mut out);
        }
        push_slice(&line, found.start, found.end, Some(style), &mut out);
        at = found.end;
    }
    if at < plain.len() {
        push_slice(&line, at, plain.len(), None, &mut out);
    }
    Line::from(out)
}

/// Append the intersection of `[from, to)` with every span, preserving the
/// span's own style and optionally patching the highlight on top.
fn push_slice(
    line: &Line<'static>,
    from: usize,
    to: usize,
    patch: Option<Style>,
    out: &mut Vec<Span<'static>>,
) {
    let mut offset = 0usize;
    for span in &line.spans {
        let text = span.content.as_ref();
        let span_start = offset;
        let span_end = offset + text.len();
        offset = span_end;
        let start = from.max(span_start);
        let end = to.min(span_end);
        if start >= end {
            continue;
        }
        let style = match patch {
            Some(patch) => span.style.patch(patch),
            None => span.style,
        };
        out.push(Span::styled(
            text[start - span_start..end - span_start].to_string(),
            style,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styles(line: &Line<'static>) -> Vec<(String, Style)> {
        line.spans
            .iter()
            .map(|span| (span.content.to_string(), span.style))
            .collect()
    }

    #[test]
    fn only_the_matched_text_gets_the_highlight() {
        let line = Line::from(vec![
            Span::raw("PASS "),
            Span::styled(
                "refund_test",
                Style::default().add_modifier(Modifier::ITALIC),
            ),
            Span::raw(" done"),
        ]);
        let style = Style::default().add_modifier(Modifier::BOLD);
        let out = highlight_line(line, "refund", style);
        let spans = styles(&out);
        let text: String = spans.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(text, "PASS refund_test done");
        let matched: Vec<&(String, Style)> = spans
            .iter()
            .filter(|(_, style)| style.add_modifier.contains(Modifier::BOLD))
            .collect();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].0, "refund");
        // The unmatched remainder of the styled span keeps its own italic
        // attribute; the highlight did not repaint the whole line.
        assert!(
            spans
                .iter()
                .any(|(text, style)| text == "_test"
                    && style.add_modifier.contains(Modifier::ITALIC))
        );
    }

    #[test]
    fn case_insensitive_and_multiple_occurrences_are_split() {
        let line = Line::from("Refund then refund again");
        let out = highlight_line(
            line,
            "REFUND",
            Style::default().add_modifier(Modifier::BOLD),
        );
        let bold: Vec<String> = out
            .spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .map(|span| span.content.to_string())
            .collect();
        assert_eq!(bold, ["Refund", "refund"]);
    }

    #[test]
    fn a_line_without_a_match_is_returned_unchanged() {
        let line = Line::from("nothing here");
        let out = highlight_line(line, "refund", Style::default());
        assert_eq!(out.spans.len(), 1);
        assert_eq!(out.spans[0].content, "nothing here");
    }

    #[test]
    fn a_cjk_match_is_split_on_char_boundaries() {
        let line = Line::from("状态：退款审批失败");
        let out = highlight_line(
            line,
            "退款审批",
            Style::default().add_modifier(Modifier::BOLD),
        );
        let text: String = out.spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "状态：退款审批失败");
        let bold: String = out
            .spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(bold, "退款审批");
    }
}
