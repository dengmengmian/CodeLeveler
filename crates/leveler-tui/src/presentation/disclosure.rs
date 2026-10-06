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
use crate::theme::{Ink, Theme};

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
    /// Executions that did not fail on their own but lack a permission
    /// ("2 need network permission"), already localized. Marked ⚠, not ✗.
    pub needs_permission_suffix: Option<String>,
    /// Extra suffix a FINISHED batch wears when every execution in it
    /// succeeded ("全部成功"), already localized. Muted, unlike the
    /// warning-coloured failure/permission suffixes: it confirms, it does not
    /// warn. `None` over a lone execution, where a success is the row itself.
    pub ok_suffix: Option<String>,
    /// Open (`▾`) or folded (`▸`).
    pub expanded: bool,
    /// The execution this row heads is still in flight. A running row is not a
    /// summary: it wears `⋮`, says what is happening now, and carries no
    /// outcome — no failure count, no success claim, no final duration. It
    /// exists so the group's first row is structurally the same open and
    /// closed, so closing a group updates text instead of inserting a row.
    pub running: bool,
    /// This row opens an independent detail surface instead of expanding
    /// inline. A drill-down renders `↗` (the shared drill-down marker) and
    /// ignores `expanded`, because there is no inline form to fold. A tool
    /// group keeps `▸/▾`.
    pub drill_down: bool,
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
    // `↗` is the one marker that means "this opens its own detail surface".
    // An inline disclosure keeps `▸/▾`, so the two affordances never wear the
    // same glyph. `⋮` is the live form of the same row: the stage is running,
    // not folded.
    let glyph = if p.running {
        "\u{22ee}"
    } else if p.drill_down {
        "\u{2197}"
    } else if p.expanded {
        "\u{25be}"
    } else {
        "\u{25b8}"
    };
    let failed = p.failed > 0;
    let attention = failed || p.needs_permission_suffix.is_some();
    let mut spans = vec![
        Span::styled(
            format!("{glyph} "),
            Style::default().fg(if p.running {
                theme.accent.primary
            } else if attention {
                theme.status.warning
            } else {
                theme.text.muted
            }),
        ),
        Span::styled(
            truncate_display(&p.label, width.saturating_sub(16)),
            Style::default().fg(if p.running {
                theme.ink(Ink::Active)
            } else if attention {
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
    } else if attention {
        spans.insert(
            1,
            Span::styled("⚠ ".to_string(), Style::default().fg(theme.status.warning)),
        );
    }
    if let Some(suffix) = &p.needs_permission_suffix {
        spans.push(Span::styled(
            format!(" · {suffix}"),
            Style::default().fg(theme.status.warning),
        ));
    }
    if let Some(suffix) = &p.ok_suffix {
        spans.push(Span::styled(
            format!(" · {suffix}"),
            Style::default().fg(theme.text.muted),
        ));
    }
    // Any MEASURED duration is worth stating: the aggregate is a fact about
    // the whole stage. Sub-100ms is not measured, only clocked — it would
    // print `0.0s` — so it stays off. A single call never reaches this
    // header, so this cannot add clock noise to one row.
    if let Some(ms) = p.duration_ms
        && ms >= 100
    {
        spans.push(Span::styled(
            format!(" · {}", format_duration(ms)),
            Style::default().fg(theme.text.muted),
        ));
    }
    Line::from(spans)
}

/// A duration as the rest of the activity stream writes it: `1.2s` under a
/// minute, `2m03s` past it. Seconds only — the row never claims milliseconds.
fn format_duration(ms: u64) -> String {
    if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
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
