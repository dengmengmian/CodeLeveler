//! `/usage` — the durable usage section of the Context & Usage page.
//!
//! The section renders a [`UiUsageReport`] the runtime aggregated from its own
//! model-call ledger. Nothing here adds tokens up: the TUI prints the report's
//! figures, and every "unknown" keeps its own wording so an unmeasured call can
//! never be read as a measured zero.

use leveler_client_protocol::{RuntimeEvent, UiUsageReport, UiUsageScope};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::query_track::QueryTrack;
use crate::state::AppState;
use crate::theme::Theme;

#[derive(Debug, Clone, Default)]
pub struct UsageView {
    pub scope: UiUsageScope,
    /// The latest report the runtime produced for `scope`.
    pub loaded: Option<UiUsageReport>,
    /// Correlation/refresh discipline shared with the other read-only pages.
    pub track: QueryTrack,
}

impl UsageView {
    /// The caller's UTC offset, in minutes. Usage windows are calendar days, and
    /// "today" means the reader's today, not the runtime host's.
    pub fn utc_offset_minutes() -> i32 {
        chrono::Local::now().offset().local_minus_utc() / 60
    }

    /// Whether this page shows the report for its current scope. A report for a
    /// different scope is never rendered under a scope label it does not belong
    /// to (a stale answer that raced a `Tab` press).
    pub fn report_for_current_scope(&self) -> Option<&UiUsageReport> {
        self.loaded.as_ref().filter(|r| r.scope == self.scope)
    }

    /// Switch scope and drop the old report: the numbers on screen must belong
    /// to the scope named on screen.
    pub fn set_scope(&mut self, scope: UiUsageScope) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.loaded = None;
    }

    pub fn cycle_scope(&mut self, forward: bool) {
        let next = if forward {
            self.scope.next()
        } else {
            self.scope.prev()
        };
        self.set_scope(next);
    }
}

/// Issue the `QueryUsage` this view owns.
///
/// Read-only: the runtime answers from rows that already settled, so this is
/// ordinary for a page opened while the agent is running.
pub fn query(state: &mut AppState) -> leveler_client_protocol::ClientCommand {
    let query_id = state.usage.track.issue();
    leveler_client_protocol::ClientCommand::QueryUsage {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
        scope: state.usage.scope,
        utc_offset_minutes: UsageView::utc_offset_minutes(),
    }
}

/// Events that change what a usage report would say.
///
/// `TokenUsage` is the per-round provider measurement, and each turn terminal is
/// the moment a durable row has settled; both make the report stale. Compaction
/// is here for the opposite reason: it must NOT change usage, and refreshing on
/// it is how that is observed.
pub fn should_refresh(event: &RuntimeEvent) -> bool {
    use RuntimeEvent::*;
    matches!(
        event,
        TokenUsage { .. }
            | TurnCompleted
            | TurnCompletedWithWarnings { .. }
            | TurnAnswered
            | TurnFailed { .. }
            | TurnIncomplete { .. }
            | TurnBlocked { .. }
            | TurnCancelled
            | TaskCancelled
            | ContextCompacted { .. }
    )
}

/// Render the Usage section of the Context & Usage page.
///
/// Numbers and grouped detail only — the diamond map belongs to the Context
/// section, where it answers "how full is the window". Every label comes from
/// `t.usage`, every figure from the runtime's report, and an absent figure keeps
/// the "unknown" wording instead of collapsing to a fabricated zero.
///
/// Layout rule, for every row at every width: a label and its figure share a
/// line only when both fit; otherwise the figure moves to its own line under the
/// label. Nothing is silently truncated and no two values share a column.
pub fn section_lines(state: &AppState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let t = state.t();
    let u = &t.usage;
    let heading = Style::default().fg(theme.text.secondary);
    let label = Style::default().fg(theme.text.secondary);
    let value = Style::default().fg(theme.text.primary);
    let dim = Style::default().fg(theme.text.muted);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let scope = scope_label(u, state.usage.scope);
    // The heading names the section AND the range: one scope per line, so the
    // figures under it can never be read as belonging to another window.
    let title = format!("{} · {scope}", u.section);
    let hint = format!("   {}", u.scope_hint);
    let title_width = unicode_width::UnicodeWidthStr::width(title.as_str())
        + unicode_width::UnicodeWidthStr::width(hint.as_str());
    let mut heading_line = vec![Span::styled(title, heading)];
    if title_width <= width {
        heading_line.push(Span::styled(hint, dim));
    }
    lines.push(Line::from(heading_line));

    let Some(report) = state.usage.report_for_current_scope() else {
        // No answer for THIS scope yet. That is not "empty" and not `0`: either
        // the request is in flight, or it failed and nothing answered — and the
        // two must not read the same.
        let message = if state.usage.track.in_flight() {
            u.waiting
        } else {
            u.unavailable
        };
        lines.push(Line::from(Span::styled(
            message,
            Style::default().fg(theme.text.secondary),
        )));
        return lines;
    };

    if report.is_empty() {
        lines.push(Line::from(Span::styled(
            u.empty,
            Style::default().fg(theme.text.secondary),
        )));
        return lines;
    }

    // Calls happened but the provider measured nothing: every token figure is
    // unknown, and saying "0" would be a measurement nobody made.
    let measured = !report.has_no_measured_usage();
    let token = |n: u64| {
        if measured {
            fmt_count(n)
        } else {
            u.unknown.to_string()
        }
    };

    push_row(
        &mut lines,
        width,
        label,
        value,
        u.total_tokens,
        &token(report.total_tokens()),
    );
    push_row(
        &mut lines,
        width,
        label,
        value,
        u.requests,
        &report.requests.to_string(),
    );
    // Cost: a known total, a floor, or nothing. Never `$0.00` for "unpriced",
    // and never presented as a subscription quota.
    let cost = match report.cost_usd() {
        Some(usd) if report.cost_is_partial() => format!("~${usd:.4} · {}", u.cost_partial),
        Some(usd) => format!("~${usd:.4} · {}", u.cost_metered),
        None => u.cost_unknown.to_string(),
    };
    push_row(&mut lines, width, label, value, u.cost, &cost);

    lines.push(Line::from(""));
    // Token detail. Cached input is a slice of `input` and reasoning a slice of
    // `output`, which each label says so nobody adds them up again.
    let cached = report
        .cached_input_tokens
        .map(fmt_count)
        .unwrap_or_else(|| u.unknown.to_string());
    let reasoning = report
        .reasoning_tokens
        .map(fmt_count)
        .unwrap_or_else(|| u.unknown.to_string());
    push_detail(
        &mut lines,
        width,
        label,
        value,
        u.input,
        &token(report.input_tokens),
        u.cached,
        &cached,
    );
    push_detail(
        &mut lines,
        width,
        label,
        value,
        u.output,
        &token(report.output_tokens),
        u.reasoning,
        &reasoning,
    );
    if !measured {
        lines.push(Line::from(Span::styled(
            u.unmeasured,
            Style::default().fg(theme.status.warning),
        )));
    }

    if !report.models.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(u.models.to_string(), heading)));
        for model in &report.models {
            let name = format!("{} / {}", model.provider, model.model);
            let mut figures = u
                .model_row
                .replacen("{}", &model.requests.to_string(), 1)
                .replacen("{}", &fmt_count(model.total_tokens()), 1);
            if let Some(micros) = model.cost_usd_micros {
                figures.push_str(&format!(" · ~${:.4}", micros as f64 / 1_000_000.0));
            }
            if model.unmeasured_requests > 0 {
                figures.push_str(&format!(" · {}", u.unmeasured));
            }
            push_row(&mut lines, width, value, label, &name, &figures);
        }
    }
    lines
}

/// One metric row: label on the left, figure right-aligned on the same line when
/// both fit, otherwise the figure on its own line beneath the label.
fn push_row(
    lines: &mut Vec<Line<'static>>,
    width: usize,
    label_style: Style,
    value_style: Style,
    label: &str,
    value: &str,
) {
    const INDENT: usize = 2;
    let label_w = unicode_width::UnicodeWidthStr::width(label);
    let value_w = unicode_width::UnicodeWidthStr::width(value);
    if INDENT + label_w + 2 + value_w <= width {
        let pad = width.saturating_sub(INDENT + label_w + value_w).max(1);
        lines.push(Line::from(vec![
            Span::styled(format!("  {label}"), label_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(value.to_string(), value_style),
        ]));
    } else {
        lines.push(Line::from(Span::styled(format!("  {label}"), label_style)));
        // A value that does not fit on its own line breaks at its own
        // separators ("a · b") rather than being clipped: the reader loses
        // layout, never the figure or what it means.
        if INDENT + 2 + value_w <= width {
            lines.push(Line::from(Span::styled(
                format!("    {value}"),
                value_style,
            )));
            return;
        }
        for part in value.split(" · ") {
            // Still too wide for the pane on its own: wrap it at word
            // boundaries instead of letting the painter cut it mid-word.
            let part_width = unicode_width::UnicodeWidthStr::width(part);
            if INDENT + 2 + part_width <= width {
                lines.push(Line::from(Span::styled(format!("    {part}"), value_style)));
                continue;
            }
            for row in crate::render::wrap(part, width.saturating_sub(INDENT + 2).max(1)) {
                lines.push(Line::from(Span::styled(format!("    {row}"), value_style)));
            }
        }
    }
}

/// A metric with a slice beside it: `label  value · sliceLabel sliceValue`.
///
/// When the whole row cannot fit, the slice gets its own row rather than being
/// cut: a cached-token figure that reads `缓存` is worse than a second line.
#[allow(clippy::too_many_arguments)]
fn push_detail(
    lines: &mut Vec<Line<'static>>,
    width: usize,
    label_style: Style,
    value_style: Style,
    label: &str,
    value: &str,
    slice_label: &str,
    slice_value: &str,
) {
    const INDENT: usize = 2;
    let combined = format!("{value} · {slice_label} {slice_value}");
    let fits = INDENT
        + unicode_width::UnicodeWidthStr::width(label)
        + 2
        + unicode_width::UnicodeWidthStr::width(combined.as_str())
        <= width;
    if fits {
        push_row(lines, width, label_style, value_style, label, &combined);
    } else {
        push_row(lines, width, label_style, value_style, label, value);
        push_row(
            lines,
            width,
            label_style,
            value_style,
            slice_label,
            slice_value,
        );
    }
}

fn scope_label(u: &crate::i18n::UsageText, scope: UiUsageScope) -> &'static str {
    match scope {
        UiUsageScope::Turn => u.scope_turn,
        UiUsageScope::Session => u.scope_session,
        UiUsageScope::Today => u.scope_today,
        UiUsageScope::Last7Days => u.scope_7d,
        UiUsageScope::Last30Days => u.scope_30d,
        UiUsageScope::All => u.scope_all,
    }
}

/// Exact token counts with thousands separators: `999`, `6,100`, `45,632,710`.
/// The usage page is where accounting is read; a compact magnitude would hide
/// digits the ledger actually holds.
fn fmt_count(n: u64) -> String {
    crate::status_line::fmt_tokens_u64(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::UiModelUsage;

    fn report(scope: UiUsageScope) -> UiUsageReport {
        UiUsageReport {
            scope,
            from: None,
            to: None,
            generated_at: "2026-01-01T00:00:00+00:00".into(),
            requests: 1,
            unmeasured_requests: 0,
            input_tokens: 10,
            cached_input_tokens: None,
            output_tokens: 2,
            reasoning_tokens: None,
            cost_usd_micros: None,
            priced_requests: 0,
            models: vec![UiModelUsage {
                provider: "p".into(),
                model: "m".into(),
                requests: 1,
                unmeasured_requests: 0,
                input_tokens: 10,
                cached_input_tokens: None,
                output_tokens: 2,
                reasoning_tokens: None,
                cost_usd_micros: None,
                priced_requests: 0,
            }],
        }
    }

    #[test]
    fn token_figures_keep_every_digit_the_ledger_has() {
        // The usage page is where accounting is read. A compact magnitude
        // would turn a measured 45,632,710 into 45.6M and hide the digits the
        // receipt claims to report.
        assert_eq!(fmt_count(45_632_710), "45,632,710");
        assert_eq!(fmt_count(6_100), "6,100");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(0), "0");
    }

    #[test]
    fn a_report_from_another_scope_is_not_shown_under_this_label() {
        let mut view = UsageView {
            loaded: Some(report(UiUsageScope::Session)),
            ..UsageView::default()
        };
        assert!(view.report_for_current_scope().is_some());
        view.set_scope(UiUsageScope::Today);
        assert!(
            view.report_for_current_scope().is_none(),
            "the session total must not be labeled as today"
        );
        view.loaded = Some(report(UiUsageScope::Today));
        assert!(view.report_for_current_scope().is_some());
    }

    #[test]
    fn cycling_scopes_drops_the_old_report() {
        let mut view = UsageView {
            loaded: Some(report(UiUsageScope::Session)),
            ..UsageView::default()
        };
        view.cycle_scope(true);
        assert_eq!(view.scope, UiUsageScope::Today);
        assert!(view.loaded.is_none());
        view.cycle_scope(false);
        assert_eq!(view.scope, UiUsageScope::Session);
    }

    #[test]
    fn refresh_triggers_are_the_facts_that_move_usage() {
        assert!(should_refresh(&RuntimeEvent::TurnCompleted));
        assert!(should_refresh(&RuntimeEvent::TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
            cached_input_tokens: 0,
            reasoning_tokens: Some(0),
        }));
        assert!(!should_refresh(&RuntimeEvent::ContextLoaded {
            query_id: None,
            accounting: None,
        }));
        assert!(!should_refresh(&RuntimeEvent::ToolCallOutput {
            id: leveler_client_protocol::ToolCallId::new("c1"),
            stream: "stdout".into(),
            chunk: "x".into(),
        }));
    }
}
