//! Active Goal indicator: the read model behind the header's right-hand status.
//!
//! The header answers, in one row, three questions a user asks while a long
//! task runs: *what* is being executed, *how long* it has been executing, and
//! *where it stands*. It is chrome, not a transcript item — the goal is global
//! state and belongs in the workbench header, not above the conversation.
//!
//! This module owns the presentation lifecycle only. It never decides whether a
//! turn is resumable, whether a failure is recoverable, or how long the runtime
//! worked: those facts arrive from the reducer (which projects the runtime) and
//! from the turn input the user actually submitted. The goal's identity is the
//! text the turn was opened with — the same text the runtime records as its
//! task objective — or, when this client did not witness the turn start, the
//! session's own goal.
//!
//! Timing is **active execution time**, matching the status line's busy clock:
//! it accumulates while the runtime reports a live turn and freezes when the
//! user interrupts. A paused goal resumed later continues from where it stopped
//! rather than counting the wall-clock gap between windows.

use std::time::{Duration, Instant};

use ratatui::style::{Color, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::state::AppState;
use crate::status_line::fmt_elapsed;
use crate::theme::Theme;

/// How long a completed goal stays in the header before fading out.
///
/// Completion is not actionable, so it should not hold permanent chrome. A
/// failure is actionable and is never hidden on a timer.
pub const COMPLETED_TTL: Duration = Duration::from_secs(4);

/// Below this many columns the goal title is dropped in favour of the icon and
/// the elapsed clock, which are the two facts the indicator exists to carry.
const MIN_TITLE_COLS: usize = 4;

/// A submitted goal title is a display label, not a document: keep it bounded
/// so a pasted paragraph does not sit in memory for the life of the turn.
const MAX_TITLE_CHARS: usize = 60;

/// Where the Active Goal stands. `Resuming` covers both a user continuing an
/// interrupted task and a runtime reconnecting a transport; the runtime's
/// `reconnecting` flag is folded in at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalPhase {
    Running,
    Paused,
    Resuming,
    Completed,
    Failed,
}

impl GoalPhase {
    /// Single-column status glyph. Color carries the same fact, but the glyph
    /// keeps it legible under `NO_COLOR`.
    pub fn glyph(self) -> char {
        match self {
            GoalPhase::Running => '◆',
            GoalPhase::Paused => '◐',
            GoalPhase::Resuming => '↻',
            GoalPhase::Completed => '✓',
            GoalPhase::Failed => '!',
        }
    }

    /// The semantic ink for the glyph and the elapsed clock. The title stays in
    /// the normal foreground — only the state marks are tinted.
    fn ink(self, theme: &Theme) -> Color {
        match self {
            GoalPhase::Running => theme.status.running,
            GoalPhase::Paused => theme.status.warning,
            GoalPhase::Resuming => theme.status.info,
            GoalPhase::Completed => theme.status.success,
            GoalPhase::Failed => theme.status.error,
        }
    }
}

/// The Active Goal, as the header renders it.
#[derive(Debug, Clone)]
pub struct ActiveGoal {
    title: String,
    /// Full objective shown on the Goal detail page. The header always uses
    /// `title`; keeping the source text here avoids reconstructing it from a
    /// mutable transcript later.
    objective: String,
    phase: GoalPhase,
    /// Active execution time banked from windows that already ended.
    accumulated: Duration,
    /// Start of the live window. `Some` while running/resuming, `None` while
    /// paused or terminal, which is what freezes the clock.
    window_started: Option<Instant>,
    /// When the current phase began — the Completed auto-hide counts from here.
    since: Instant,
}

impl ActiveGoal {
    /// A fresh running goal (a turn this client submitted, or one already live
    /// when the client reconnected).
    pub fn running(title: String, now: Instant) -> Self {
        Self::running_with_objective(title.clone(), title, now)
    }

    pub fn running_with_objective(title: String, objective: String, now: Instant) -> Self {
        Self {
            title,
            objective,
            phase: GoalPhase::Running,
            accumulated: Duration::ZERO,
            window_started: Some(now),
            since: now,
        }
    }

    pub fn phase(&self) -> GoalPhase {
        self.phase
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn objective(&self) -> &str {
        &self.objective
    }

    /// Bank the live window (idempotent) so the clock stops advancing.
    /// `now` is a parameter, not `Instant::now()`, so the lifecycle is testable
    /// against a fixed clock.
    fn bank(&mut self, now: Instant) {
        if let Some(start) = self.window_started.take() {
            self.accumulated += now.saturating_duration_since(start);
        }
    }

    /// Active execution time at `now`: banked windows plus the live one.
    pub fn elapsed_at(&self, now: Instant) -> Duration {
        match self.window_started {
            Some(start) => self.accumulated + now.saturating_duration_since(start),
            None => self.accumulated,
        }
    }

    /// Begin a continuation of this same goal: keep identity and elapsed, and
    /// show `↻` until the first real progress arrives.
    pub fn resume(&mut self, now: Instant) {
        self.window_started = Some(now);
        self.phase = GoalPhase::Resuming;
        self.since = now;
    }

    /// Replace this goal with a new one. Elapsed restarts at zero.
    pub fn replace(&mut self, title: String, now: Instant) {
        self.replace_with_objective(title.clone(), title, now);
    }

    pub fn replace_with_objective(&mut self, title: String, objective: String, now: Instant) {
        self.title = title;
        self.objective = objective;
        self.phase = GoalPhase::Running;
        self.accumulated = Duration::ZERO;
        self.window_started = Some(now);
        self.since = now;
    }

    /// First real progress (a model round, a tool call): a resuming goal is
    /// running again. A goal that is already running is untouched.
    pub fn note_progress(&mut self) {
        if self.phase == GoalPhase::Resuming {
            self.phase = GoalPhase::Running;
        }
    }

    pub fn pause(&mut self, now: Instant) {
        self.bank(now);
        self.phase = GoalPhase::Paused;
        self.since = now;
    }

    pub fn complete(&mut self, now: Instant) {
        self.bank(now);
        self.phase = GoalPhase::Completed;
        self.since = now;
    }

    pub fn fail(&mut self, now: Instant) {
        self.bank(now);
        self.phase = GoalPhase::Failed;
        self.since = now;
    }

    /// A completed goal fades out on its own; every other phase persists until
    /// the next goal or an explicit clear. Failures stay because they are the
    /// one state the user may need to act on.
    pub fn is_expired(&self, now: Instant) -> bool {
        self.phase == GoalPhase::Completed
            && now.saturating_duration_since(self.since) >= COMPLETED_TTL
    }

    pub fn is_visible(&self, now: Instant) -> bool {
        !self.is_expired(now)
    }
}

/// The bounded display label for a submitted turn input: its first non-empty
/// line. The header truncates further as the terminal narrows, so this only
/// bounds memory and keeps a multi-line paste from leaking newlines into the
/// one-row indicator.
pub fn short_title(content: &str) -> Option<String> {
    let line = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let title: String = line.chars().take(MAX_TITLE_CHARS).collect();
    (!title.is_empty()).then_some(title)
}

/// A user turn's goal identity, staged before the turn flips to Busy.
///
/// `continuation` is the runtime's own continuation semantics as the client
/// parsed it (`继续` / `go on`): it says this input re-enters the goal already
/// shown rather than opening a new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedGoal {
    pub title: Option<String>,
    pub objective: Option<String>,
    pub continuation: bool,
}

/// Start (or continue) the Active Goal when a turn becomes Busy.
///
/// A continuation of a goal that is still shown keeps its identity and banked
/// clock; anything else — a new task, or a goal that already reached a terminal
/// state — replaces it. Falls back to the session's authoritative goal when the
/// client did not witness the input.
pub fn begin(state: &mut AppState, staged: Option<StagedGoal>, now: Instant) {
    let fallback = state.goal.clone();
    let staged = staged.unwrap_or(StagedGoal {
        title: None,
        objective: None,
        continuation: false,
    });
    let Some(goal) = state.active_goal.as_mut() else {
        let title = staged
            .title
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| fallback.clone());
        let objective = staged
            .objective
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(fallback);
        let mut goal = ActiveGoal::running_with_objective(title, objective, now);
        if staged.continuation {
            goal.phase = GoalPhase::Resuming;
        }
        state.active_goal = Some(goal);
        return;
    };
    let same_goal = staged.continuation
        && matches!(
            goal.phase(),
            GoalPhase::Running | GoalPhase::Paused | GoalPhase::Resuming
        );
    if same_goal {
        goal.resume(now);
    } else {
        let title = staged
            .title
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| fallback.clone());
        let objective = staged
            .objective
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(fallback);
        goal.replace_with_objective(title, objective, now);
    }
}

/// The right-hand header spans for the current Active Goal, or an empty vector
/// when there is nothing to show. `max` is the columns available to the
/// indicator.
pub fn header(state: &AppState, max: usize) -> Vec<Span<'static>> {
    let Some(goal) = state.active_goal.as_ref() else {
        return Vec::new();
    };
    let now = Instant::now();
    if !goal.is_visible(now) {
        return Vec::new();
    }
    // A transport retry owns the `↻` mark over the goal's stored phase: the
    // runtime is reconnecting, which is exactly what the user is watching.
    let phase = if state.reconnecting.is_some() {
        GoalPhase::Resuming
    } else {
        goal.phase()
    };
    let elapsed = fmt_elapsed(goal.elapsed_at(now).as_secs());
    spans(phase, goal.title(), &elapsed, &state.theme, max)
}

pub fn has_visible_goal(state: &AppState) -> bool {
    state
        .active_goal
        .as_ref()
        .is_some_and(|goal| goal.is_visible(Instant::now()))
}

/// Build the indicator, degrading with the available width.
///
/// Priority when space runs out is glyph + elapsed + detail affordance first,
/// title last: the full objective is always reachable even when its summary is
/// truncated away.
pub fn spans(
    phase: GoalPhase,
    title: &str,
    elapsed: &str,
    theme: &Theme,
    max: usize,
) -> Vec<Span<'static>> {
    let ink = Style::default().fg(phase.ink(theme));
    let glyph = phase.glyph();
    let elapsed_w = UnicodeWidthStr::width(elapsed);
    // `glyph`, a separating space, the elapsed clock, and " ↗".
    let minimum = 1 + 1 + elapsed_w + 2;
    if minimum > max {
        // Not even the clock fits; the identity header keeps the row.
        return Vec::new();
    }
    let clock = || {
        vec![
            Span::styled(format!("{glyph} "), ink),
            Span::styled(elapsed.to_string(), ink),
            Span::styled(" ↗", Style::default().fg(theme.accent.primary)),
        ]
    };
    let title = title.trim();
    if title.is_empty() {
        return clock();
    }
    // `glyph`, space, title, " · ", elapsed, " ↗".
    let fixed = 1 + 1 + 3 + elapsed_w + 2;
    let title_room = max.saturating_sub(fixed);
    if title_room < MIN_TITLE_COLS {
        return clock();
    }
    let shown = crate::render::truncate_display(title, title_room);
    vec![
        Span::styled(format!("{glyph} "), ink),
        Span::styled(shown, Style::default().fg(theme.text.primary)),
        Span::styled(" · ", Style::default().fg(theme.text.muted)),
        Span::styled(elapsed.to_string(), ink),
        Span::styled(" ↗", Style::default().fg(theme.accent.primary)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn plain(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn width(spans: &[Span<'static>]) -> usize {
        spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum()
    }

    /// A fixed base so every offset in a test shares one clock.
    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn every_phase_has_its_own_glyph() {
        use GoalPhase::*;
        let glyphs = [
            Running.glyph(),
            Paused.glyph(),
            Resuming.glyph(),
            Completed.glyph(),
            Failed.glyph(),
        ];
        assert_eq!(glyphs, ['◆', '◐', '↻', '✓', '!']);
        let mut unique = glyphs.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), glyphs.len());
    }

    #[test]
    fn elapsed_banks_only_while_the_clock_runs() {
        let base = t0();
        let secs = |n: u64| base + Duration::from_secs(n);
        let mut goal = ActiveGoal::running("fix the flaky test".into(), secs(0));
        assert_eq!(goal.elapsed_at(secs(42)), Duration::from_secs(42));

        // Pause at 42s, wait 10 minutes: the wait is not execution time.
        goal.pause(secs(42));
        assert_eq!(goal.elapsed_at(secs(642)), Duration::from_secs(42));

        // Resume: the clock continues, it does not restart.
        goal.resume(secs(642));
        assert_eq!(goal.elapsed_at(secs(650)), Duration::from_secs(50));
        assert_eq!(goal.phase(), GoalPhase::Resuming);
        goal.note_progress();
        assert_eq!(goal.phase(), GoalPhase::Running);
        assert_eq!(goal.elapsed_at(secs(650)), Duration::from_secs(50));
    }

    #[test]
    fn completion_freezes_the_clock_and_fails_persistently() {
        let base = t0();
        let secs = |n: u64| base + Duration::from_secs(n);
        let mut goal = ActiveGoal::running("ship it".into(), secs(0));
        goal.complete(secs(90));
        assert_eq!(goal.elapsed_at(secs(900)), Duration::from_secs(90));
        assert!(goal.is_visible(secs(90)));
        // Visible just before the TTL boundary, gone at it.
        assert!(goal.is_visible(secs(90) + COMPLETED_TTL - Duration::from_millis(1)));
        assert!(!goal.is_visible(secs(90) + COMPLETED_TTL));
        assert!(goal.is_expired(secs(90) + COMPLETED_TTL));

        let mut failed = ActiveGoal::running("ship it".into(), secs(0));
        failed.fail(secs(30));
        assert!(failed.is_visible(secs(30) + Duration::from_secs(3600)));
        assert!(!failed.is_expired(secs(30) + Duration::from_secs(3600)));
    }

    #[test]
    fn replace_starts_a_new_goal_with_a_new_clock() {
        let base = t0();
        let secs = |n: u64| base + Duration::from_secs(n);
        let mut goal = ActiveGoal::running("goal a".into(), secs(0));
        goal.complete(secs(600));
        goal.replace("goal b".into(), secs(601));
        assert_eq!(goal.phase(), GoalPhase::Running);
        assert_eq!(goal.title(), "goal b");
        assert_eq!(goal.elapsed_at(secs(601)), Duration::ZERO);
    }

    #[test]
    fn short_title_uses_the_first_non_empty_line() {
        assert_eq!(
            short_title("\n\n  修复断线后任务续接  \nmore").as_deref(),
            Some("修复断线后任务续接")
        );
        assert_eq!(short_title("   \n\t\n").as_deref(), None);
        assert_eq!(short_title("").as_deref(), None);
        let long = "x".repeat(500);
        assert_eq!(short_title(&long).unwrap().chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn the_full_indicator_carries_glyph_title_separator_and_clock() {
        let theme = Theme::dark();
        let spans = spans(
            GoalPhase::Running,
            "修复断线后任务续接",
            "6m 42s",
            &theme,
            120,
        );
        assert_eq!(plain(&spans), "◆ 修复断线后任务续接 · 6m 42s ↗");
        // Only the glyph and clock are tinted; the title keeps normal ink.
        assert_eq!(spans[1].style.fg, Some(theme.text.primary));
        assert_eq!(spans[0].style.fg, Some(theme.status.running));
        assert_eq!(spans[3].style.fg, Some(theme.status.running));
    }

    #[test]
    fn each_phase_tints_its_marks_with_its_own_token() {
        let theme = Theme::dark();
        for (phase, color) in [
            (GoalPhase::Running, theme.status.running),
            (GoalPhase::Paused, theme.status.warning),
            (GoalPhase::Resuming, theme.status.info),
            (GoalPhase::Completed, theme.status.success),
            (GoalPhase::Failed, theme.status.error),
        ] {
            let spans = spans(phase, "t", "1s", &theme, 80);
            assert_eq!(spans[0].style.fg, Some(color), "{phase:?}");
            assert_eq!(spans[3].style.fg, Some(color), "{phase:?}");
        }
    }

    #[test]
    fn a_long_title_truncates_before_the_clock_is_touched() {
        let theme = Theme::dark();
        let spans = spans(
            GoalPhase::Running,
            "非常长的中文目标名称需要被截断",
            "6m 42s",
            &theme,
            20,
        );
        let text = plain(&spans);
        assert!(text.contains('…'), "title should be truncated: {text}");
        assert!(text.ends_with("6m 42s ↗"), "clock must survive: {text}");
        assert!(
            width(&spans) <= 20,
            "must not exceed its budget: {}",
            width(&spans)
        );
    }

    #[test]
    fn a_wide_title_is_measured_by_columns_not_chars() {
        let theme = Theme::dark();
        // Each CJK glyph is two columns; the budget must count them as two.
        let spans = spans(
            GoalPhase::Running,
            "修复断线后任务续接修复断线",
            "10s",
            &theme,
            24,
        );
        assert!(width(&spans) <= 24, "measured by width: {}", width(&spans));
    }

    #[test]
    fn a_very_narrow_row_keeps_only_the_glyph_and_clock() {
        let theme = Theme::dark();
        let spans = spans(GoalPhase::Paused, "some goal", "9m 47s", &theme, 10);
        assert_eq!(plain(&spans), "◐ 9m 47s ↗");
        assert_eq!(width(&spans), 10);
    }

    #[test]
    fn too_narrow_for_the_clock_yields_no_indicator() {
        let theme = Theme::dark();
        assert!(spans(GoalPhase::Running, "t", "1m 00s", &theme, 4).is_empty());
        // Empty title still degrades to the clock when the clock fits.
        assert_eq!(
            plain(&spans(GoalPhase::Running, "", "6s", &theme, 8)),
            "◆ 6s ↗"
        );
    }

    #[test]
    fn unicode_titles_do_not_break_the_single_row_contract() {
        let theme = Theme::dark();
        for title in [
            "🚀 deploy to prod",
            "修复：断线/ESC 后任务无法续接",
            "fix\u{2028}line break",
        ] {
            let spans = spans(GoalPhase::Running, title, "2m 05s", &theme, 30);
            let text = plain(&spans);
            assert!(!text.contains('\n'), "one row only: {text:?}");
            assert!(
                width(&spans) <= 30,
                "fits budget: {text:?} = {}",
                width(&spans)
            );
        }
    }
}
