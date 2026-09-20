//! `/clean` — reclaim CodeLeveler-owned local storage from inside the TUI.
//!
//! The page is a view over the host's cleanup plan. It never scans storage
//! itself: the CLI event loop runs the same `leveler-project::hygiene` engine
//! the `leveler clean` command uses (`spawn_blocking`, off the UI thread) and
//! folds a [`CleanPlanView`] / [`CleanResultView`] back here. That keeps the
//! TUI a pure reducer over plain data, and keeps one cleanup implementation.
//!
//! Only `Safety::Safe` items are ever removed from this page. Confirmation-only
//! items (a deleted project's sessions, browser state) are shown, never acted
//! on.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::i18n::UiText;
use crate::render::{render_list_focused, render_scrolled};
use crate::state::AppState;
use crate::theme::Theme;

/// The kind of a reclaimable item, as the TUI names it. Deliberately plain: the
/// TUI does not depend on the hygiene engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanKind {
    ToolCache,
    ExpiredEphemeral,
    DeadSocket,
    OrphanLock,
    HistoricalAutomation,
    DeletedProjectData,
    BrowserState,
}

/// Whether a cleanup may touch the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanSafety {
    Safe,
    NeedsConfirmation,
}

/// One category line in the plan, already summed by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanCategory {
    pub kind: CleanKind,
    pub bytes: u64,
    pub count: usize,
    pub safety: CleanSafety,
}

/// The frozen plan the host built. Purely presentational.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanPlanView {
    pub categories: Vec<CleanCategory>,
    pub safe_bytes: u64,
    pub needs_confirmation_bytes: u64,
}

impl CleanPlanView {
    pub fn has_safe(&self) -> bool {
        self.safe_bytes > 0
            || self
                .categories
                .iter()
                .any(|c| c.safety == CleanSafety::Safe && c.count > 0)
    }
}

/// One path a cleanup could not remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanFailure {
    pub path: String,
    pub kind: CleanKind,
    pub reason: String,
}

/// What a cleanup did. Real numbers only — never a fabricated percentage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanResultView {
    pub reclaimed_bytes: u64,
    pub removed: usize,
    pub by_kind: Vec<(CleanKind, u64)>,
    pub failures: Vec<CleanFailure>,
}

/// Where the page is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CleanStage {
    /// No scan started yet.
    #[default]
    Idle,
    /// A scan is running off-thread.
    Scanning,
    /// The plan is ready and actions are offered.
    Ready,
    /// A safe cleanup is running off-thread.
    Running,
    /// A cleanup finished (`result` or `error` is set).
    Done,
}

/// The page's whole state. Presentation-only.
#[derive(Debug, Clone, Default)]
pub struct CleanState {
    pub stage: CleanStage,
    pub plan: Option<CleanPlanView>,
    pub result: Option<CleanResultView>,
    pub error: Option<String>,
    /// Selected row in the current action/value list.
    pub selected: usize,
    /// Showing the details sub-page rather than the summary.
    pub details: bool,
}

/// A user choice on the page, mapped by the reducer to an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanAction {
    /// Start the safe cleanup.
    CleanSafe,
    /// Toggle the details sub-page.
    Details,
    /// Leave the page.
    Close,
}

impl CleanState {
    /// Begin a scan: the caller emits the scan effect.
    pub fn begin_scan(&mut self) {
        self.stage = CleanStage::Scanning;
        self.plan = None;
        self.result = None;
        self.error = None;
        self.selected = 0;
        self.details = false;
    }

    /// Fold a finished scan.
    pub fn plan_ready(&mut self, plan: CleanPlanView) {
        self.stage = CleanStage::Ready;
        self.plan = Some(plan);
        self.selected = 0;
    }

    /// The scan or cleanup failed before producing a plan.
    pub fn failed(&mut self, message: impl Into<String>) {
        self.stage = CleanStage::Done;
        self.error = Some(message.into());
        self.selected = 0;
    }

    /// A safe cleanup is running.
    pub fn begin_clean(&mut self) {
        self.stage = CleanStage::Running;
        self.selected = 0;
    }

    /// Fold a finished cleanup.
    pub fn finished(&mut self, result: CleanResultView) {
        self.stage = CleanStage::Done;
        self.result = Some(result);
        self.selected = 0;
    }

    /// Move the selection by `delta`, clamped to the current action list.
    pub fn move_selection(&mut self, delta: isize) {
        let len = self.actions().len();
        if len == 0 {
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, len as isize - 1) as usize;
    }

    /// The action list for the current stage.
    pub fn actions(&self) -> Vec<CleanAction> {
        match self.stage {
            CleanStage::Ready => vec![
                CleanAction::CleanSafe,
                CleanAction::Details,
                CleanAction::Close,
            ],
            CleanStage::Done => vec![CleanAction::Close],
            CleanStage::Scanning | CleanStage::Running | CleanStage::Idle => Vec::new(),
        }
    }

    /// The selected action, if any.
    pub fn selected_action(&self) -> Option<CleanAction> {
        self.actions().get(self.selected).copied()
    }

    /// Whether a background scan or cleanup is in flight.
    pub fn is_busy(&self) -> bool {
        matches!(self.stage, CleanStage::Scanning | CleanStage::Running)
    }
}

fn fmt_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KIB * KIB * KIB {
        format!("{:.1} GB", value / (KIB * KIB * KIB))
    } else if value >= KIB * KIB {
        format!("{:.1} MB", value / (KIB * KIB))
    } else if value >= KIB {
        format!("{:.1} KB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn kind_label(kind: CleanKind, t: &UiText) -> &'static str {
    match kind {
        CleanKind::ToolCache => t.clean.kind_tool_cache,
        CleanKind::ExpiredEphemeral => t.clean.kind_expired_ephemeral,
        CleanKind::DeadSocket => t.clean.kind_dead_socket,
        CleanKind::OrphanLock => t.clean.kind_orphan_lock,
        CleanKind::HistoricalAutomation => t.clean.kind_historical_automation,
        CleanKind::DeletedProjectData => t.clean.kind_deleted_project_data,
        CleanKind::BrowserState => t.clean.kind_browser_state,
    }
}

/// Width-aware label padding (CJK counts as two columns) so numbers line up in
/// both languages.
fn pad_label(label: &str, width: usize) -> String {
    let used: usize = label
        .chars()
        .map(|c| {
            let cp = c as u32;
            let wide = (0x1100..=0x115F).contains(&cp)
                || (0x2E80..=0xA4CF).contains(&cp)
                || (0xAC00..=0xD7A3).contains(&cp)
                || (0xF900..=0xFAFF).contains(&cp)
                || (0xFE30..=0xFE4F).contains(&cp)
                || (0xFF00..=0xFF60).contains(&cp)
                || (0xFFE0..=0xFFE6).contains(&cp);
            if wide { 2 } else { 1 }
        })
        .sum();
    let mut out = String::from(label);
    for _ in used..width {
        out.push(' ');
    }
    out
}

/// Render the `/clean` page.
pub fn render_clean_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let t = state.t();
    let page = crate::secondary::SecondaryPage {
        title: t.clean.title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: t.clean.footer_hint,
    };
    let layout = crate::secondary::layout(area, 0);
    crate::secondary::draw_header(frame, &layout, &page, theme);
    crate::secondary::draw_footer(frame, &layout, &page, theme);
    let body = layout.content;

    let (lines, focus) = body_lines(&state.clean, state.tick, body.width as usize, theme, t);
    match focus {
        Some(focus) => render_list_focused(frame, body, lines, focus, theme),
        None => render_scrolled(frame, body, state, lines),
    }
}

fn body_lines(
    clean: &CleanState,
    tick: u64,
    _width: usize,
    theme: &Theme,
    t: &UiText,
) -> (Vec<Line<'static>>, Option<usize>) {
    let dim = Style::default().fg(theme.text.muted);
    let secondary = Style::default().fg(theme.text.secondary);
    let accent = Style::default()
        .fg(theme.accent.primary)
        .add_modifier(Modifier::BOLD);
    let ok = Style::default().fg(theme.status.success);
    let warn = Style::default().fg(theme.status.warning);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut selected_row: Option<usize> = None;

    match clean.stage {
        CleanStage::Idle | CleanStage::Scanning => {
            // A single spinner tick; no fabricated byte progress.
            let spin = ["●", "◐", "◓", "◑", "◒"][(tick as usize) % 5];
            lines.push(Line::from(Span::styled(
                format!("{spin} {}", t.clean.analyzing),
                accent,
            )));
            lines.push(Line::from(Span::styled(t.clean.analyzing_note, dim)));
        }
        CleanStage::Running => {
            let spin = ["●", "◐", "◓", "◑", "◒"][(tick as usize) % 5];
            lines.push(Line::from(Span::styled(
                format!("{spin} {}", t.clean.cleaning),
                accent,
            )));
        }
        CleanStage::Done => {
            if let Some(error) = &clean.error {
                lines.push(Line::from(Span::styled(
                    format!("⚠ {}: {error}", t.clean.failed),
                    warn,
                )));
            } else if let Some(result) = &clean.result {
                lines.push(Line::from(Span::styled(
                    format!(
                        "✓ {} {}",
                        t.clean.reclaimed,
                        fmt_bytes(result.reclaimed_bytes)
                    ),
                    ok,
                )));
                lines.push(Line::default());
                for (kind, bytes) in &result.by_kind {
                    if *bytes == 0 {
                        continue;
                    }
                    lines.push(Line::from(vec![
                        Span::styled("  ", secondary),
                        Span::styled(pad_label(kind_label(*kind, t), 22), secondary),
                        Span::styled(fmt_bytes(*bytes), secondary),
                    ]));
                }
                if !result.failures.is_empty() {
                    lines.push(Line::default());
                    lines.push(Line::from(Span::styled(
                        format!("⚠ {}", t.clean.partial_failure),
                        warn,
                    )));
                    for failure in result.failures.iter().take(5) {
                        lines.push(Line::from(Span::styled(
                            format!("  {} — {}", failure.path, failure.reason),
                            dim,
                        )));
                    }
                }
            }
        }
        CleanStage::Ready => {
            if clean.details {
                render_details(&mut lines, clean, theme, t);
            } else {
                render_summary(&mut lines, clean, theme, t);
            }
        }
    }

    // Actions.
    let actions = clean.actions();
    if !actions.is_empty() {
        lines.push(Line::default());
        for (index, action) in actions.iter().enumerate() {
            let selected = index == clean.selected;
            let marker = if selected { "›" } else { " " };
            let label = match action {
                CleanAction::CleanSafe => t.clean.action_clean_safe,
                CleanAction::Details => {
                    if clean.details {
                        t.clean.action_back
                    } else {
                        t.clean.action_details
                    }
                }
                CleanAction::Close => t.clean.action_close,
            };
            let style = if selected { accent } else { secondary };
            if selected {
                selected_row = Some(lines.len());
            }
            lines.push(Line::from(Span::styled(format!("{marker} {label}"), style)));
        }
    }

    (lines, selected_row)
}

fn render_summary(lines: &mut Vec<Line<'static>>, clean: &CleanState, theme: &Theme, t: &UiText) {
    let Some(plan) = &clean.plan else {
        return;
    };
    lines.push(Line::from(Span::styled(
        t.clean.reclaimable,
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());
    for category in &plan.categories {
        if category.bytes == 0 && category.count == 0 {
            continue;
        }
        let mut spans = vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                pad_label(kind_label(category.kind, t), 24),
                Style::default().fg(theme.text.secondary),
            ),
            Span::styled(
                format!("{:>10}", fmt_bytes(category.bytes)),
                Style::default().fg(theme.text.secondary),
            ),
        ];
        if category.safety == CleanSafety::NeedsConfirmation {
            spans.push(Span::styled(
                format!("   {}", t.clean.needs_confirmation),
                Style::default().fg(theme.status.warning),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            pad_label(t.clean.safe_total, 24),
            Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>10}", fmt_bytes(plan.safe_bytes)),
            Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
}

fn render_details(lines: &mut Vec<Line<'static>>, clean: &CleanState, theme: &Theme, t: &UiText) {
    let Some(plan) = &clean.plan else {
        return;
    };
    let dim = Style::default().fg(theme.text.muted);
    let ok = Style::default().fg(theme.status.success);
    let warn = Style::default().fg(theme.status.warning);
    lines.push(Line::from(Span::styled(
        t.clean.details_safe,
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());
    for category in plan
        .categories
        .iter()
        .filter(|c| c.safety == CleanSafety::Safe)
    {
        lines.push(Line::from(Span::styled(
            format!(
                "  ✓ {}  {}",
                pad_label(kind_label(category.kind, t), 24),
                fmt_bytes(category.bytes)
            ),
            ok,
        )));
    }
    let confirm: Vec<_> = plan
        .categories
        .iter()
        .filter(|c| c.safety == CleanSafety::NeedsConfirmation)
        .collect();
    if !confirm.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            t.clean.details_needs,
            Style::default()
                .fg(theme.status.warning)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());
        for category in confirm {
            lines.push(Line::from(Span::styled(
                format!(
                    "  ⚠ {}  {}",
                    pad_label(kind_label(category.kind, t), 24),
                    fmt_bytes(category.bytes)
                ),
                warn,
            )));
            let note = match category.kind {
                CleanKind::DeletedProjectData => Some(t.clean.note_deleted_project),
                CleanKind::BrowserState => Some(t.clean.note_browser_state),
                _ => None,
            };
            if let Some(note) = note {
                lines.push(Line::from(Span::styled(format!("     {note}"), dim)));
            }
        }
    }
}
