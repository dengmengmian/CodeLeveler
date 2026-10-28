//! Active Goal indicator: the read model behind the header's right-hand status.
//!
//! The header identifies goal mode, its execution phase and active elapsed
//! time. Ordinary chat does not occupy this surface. The full objective stays
//! accessible through the goal detail page.
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
    /// Full objective shown on the Goal detail page. Keeping the source text
    /// here avoids reconstructing it from a mutable transcript later.
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

    /// Adopt the durable goal's identity without restarting the display
    /// clock. A reconnecting client already has the turn's elapsed time; it
    /// is only missing the objective the runtime actually committed to.
    pub fn adopt_objective(&mut self, title: String, objective: String) {
        if !title.trim().is_empty() {
            self.title = title;
        }
        if !objective.trim().is_empty() {
            self.objective = objective;
        }
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
    // Follow-up text in goal mode never renames or reopens a settled objective.
    // Only an explicit new goal (continuation=false) replaces that identity.
    if staged.continuation
        && state.collaboration.eq_ignore_ascii_case("goal")
        && matches!(goal.phase(), GoalPhase::Completed | GoalPhase::Failed)
    {
        return;
    }
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

/// The compact task-execution badge. An ordinary chat turn never occupies this
/// surface; an explicitly delegated task does, even though the session stays a
/// Chat session. The full objective remains available through the detail page.
pub fn header(state: &AppState, max: usize) -> Vec<Span<'static>> {
    if !state.collaboration.eq_ignore_ascii_case("goal") && !state.execution_task_active {
        return Vec::new();
    }
    let now = Instant::now();
    let goal = state
        .active_goal
        .as_ref()
        .filter(|goal| goal.is_visible(now));
    let phase = goal.map(|goal| {
        if state.reconnecting.is_some() {
            GoalPhase::Resuming
        } else {
            goal.phase()
        }
    });
    let pending = state.pending_submissions.iter().any(|pending| {
        pending.command.session_id() == Some(&state.session_id)
            && matches!(
                pending.command,
                leveler_client_protocol::ClientCommand::RunGoal { .. }
                    | leveler_client_protocol::ClientCommand::SubmitMessage { .. }
                    | leveler_client_protocol::ClientCommand::ResumeTask { .. }
            )
    });
    let label = match phase {
        Some(GoalPhase::Running | GoalPhase::Resuming) if pending => {
            state.t().pending_input_sending
        }
        Some(GoalPhase::Running) if state.is_busy() => state.t().goal_header_active,
        Some(GoalPhase::Resuming) => state.t().goal_phase_resuming,
        Some(GoalPhase::Paused) => state.t().goal_phase_paused,
        Some(GoalPhase::Completed) => state.t().goal_phase_completed,
        Some(GoalPhase::Failed) => state.t().goal_phase_failed,
        _ => state.t().goal_header_enabled,
    };
    // In a Chat session the task was declared by the model, not by a `/goal`
    // command; naming the actual mode keeps the badge honest.
    let base = if state.collaboration.eq_ignore_ascii_case("goal") {
        format!("◎ /goal {label}")
    } else {
        format!("◎ {} {label}", state.t().goal_mode)
    };
    let timed =
        goal.map(|goal| format!("{base} ({})", fmt_elapsed(goal.elapsed_at(now).as_secs())));
    // Keep the explicit mode and state before the clock as the pane narrows.
    let shown = timed
        .filter(|text| UnicodeWidthStr::width(text.as_str()) <= max)
        .unwrap_or(base);
    if UnicodeWidthStr::width(shown.as_str()) > max {
        return Vec::new();
    }
    vec![Span::styled(
        shown,
        Style::default()
            .fg(phase.map_or(state.theme.accent.primary, |phase| phase.ink(&state.theme))),
    )]
}

pub fn has_visible_goal(state: &AppState) -> bool {
    (state.collaboration.eq_ignore_ascii_case("goal") || state.execution_task_active)
        && state
            .active_goal
            .as_ref()
            .is_some_and(|goal| goal.is_visible(Instant::now()))
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
