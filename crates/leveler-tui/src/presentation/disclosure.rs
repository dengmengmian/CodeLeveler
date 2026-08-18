//! The clickable `▸ / ▾` disclosure row — the shared visual language for any
//! finished execution in the conversation.
//!
//! This renderer only knows the presentation model below. Which executions
//! fold, what the label says, and what counts as a failure are the calling
//! adapter's business (`activity_stream` for Agent tool groups). That split
//! is what lets a future user-shell (`!command`) or capability execution
//! reuse this exact row without touching tool classification.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::render::truncate_display;
use crate::theme::Theme;

/// Everything a disclosure header row shows. Text is already localized by the
/// adapter; this model carries no domain types and no tool names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisclosurePresentation {
    /// Semantic summary ("Ran 1 shell command", "Read 4 files", …).
    pub label: String,
    /// How many of the folded executions failed (0 = clean).
    pub failed: usize,
    /// Extra failure count suffix for multi-execution batches, already
    /// localized ("2 failed"); shown after the label.
    pub failed_suffix: Option<String>,
    /// Open (`▾`) or folded (`▸`).
    pub expanded: bool,
    /// Authoritative runtime-supplied duration. The adapter must only set
    /// this when it IS authoritative (a single execution) — this renderer
    /// will show whatever it is given.
    pub duration_ms: Option<u64>,
    /// First meaningful error line, shown under the folded row so a failure
    /// never requires expanding to notice.
    pub first_error: Option<String>,
}

/// The header row: `▸ label · 1.2s`, `▾ ✗ label · 2 failed`. The whole row is
/// a click target (hit-tested by the conversation build).
pub fn header_line(p: &DisclosurePresentation, theme: &Theme, width: usize) -> Line<'static> {
    let glyph = if p.expanded { "▾" } else { "▸" };
    let failed = p.failed > 0;
    let mut spans = vec![
        Span::styled(
            format!("{glyph} "),
            Style::default().fg(if failed {
                theme.status.warning
            } else {
                theme.text.muted
            }),
        ),
        Span::styled(
            truncate_display(&p.label, width.saturating_sub(16)),
            Style::default().fg(if failed {
                theme.text.primary
            } else {
                theme.text.muted
            }),
        ),
    ];
    if failed {
        spans.insert(
            1,
            Span::styled("✗ ".to_string(), Style::default().fg(theme.status.warning)),
        );
        if let Some(suffix) = &p.failed_suffix {
            spans.push(Span::styled(
                format!(" · {suffix}"),
                Style::default().fg(theme.status.warning),
            ));
        }
    }
    if let Some(ms) = p.duration_ms
        && ms >= 1000
    {
        spans.push(Span::styled(
            format!(" · {:.1}s", ms as f64 / 1000.0),
            Style::default().fg(theme.text.muted),
        ));
    }
    Line::from(spans)
}

/// The folded form: the header row plus, for failures, the first meaningful
/// error line riding along underneath.
pub fn collapsed_lines(
    p: &DisclosurePresentation,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let mut out = vec![header_line(p, theme, width)];
    if let Some(err) = &p.first_error {
        out.push(Line::from(vec![
            Span::styled("  └ ".to_string(), Style::default().fg(theme.text.muted)),
            Span::styled(
                truncate_display(err, width.saturating_sub(4)),
                Style::default().fg(theme.status.warning),
            ),
        ]));
    }
    out
}
