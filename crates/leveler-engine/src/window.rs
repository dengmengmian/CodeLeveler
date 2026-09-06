//! Durable control state for one goal invocation's work windows.
//!
//! The supervisor decides whether another window opens from a handful of
//! facts: how many extensions it has already granted, how many windows in a
//! row produced nothing, and what the current budget segment looked like when
//! it started. Every one of them lived in a local variable, so a process that
//! died mid-goal and came back handed the resumed run a clean slate — full
//! extension quota, no-progress counter at zero, segment baseline forgotten.
//! The guards were not merely inaccurate after a crash; they were absent, and
//! nothing in the recovered state said so.
//!
//! This is that state, written down. It owns ONLY what decides the next
//! window. Goal content, the evidence ledger, the transcript, the workspace,
//! usage rows and child results each have an owner already, and a second copy
//! here would be a second authority.
//!
//! Scope is one goal invocation. A crash inside an invocation must not reset
//! these counters — that is what makes them durable. A fresh invocation of the
//! goal is a fresh mandate from the user and starts clean; what carries across
//! invocations is the goal record's `windows_run` and the progress ledger's
//! spend, both of which have their own homes.

use serde::{Deserialize, Serialize};

/// The facts that decide whether another window opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WindowState {
    /// Windows opened so far in this invocation, the first drive included.
    /// Zero only before the first drive has been accounted for.
    #[serde(default)]
    pub window_index: u32,
    /// Resource-budget extensions (tokens / cost / duration / commands /
    /// files) already granted. Bounded by `MAX_EXTENSIONS`; a restart that
    /// forgot this handed the run a fresh quota of them.
    #[serde(default)]
    pub budget_extensions: u32,
    /// Round-budget extensions already granted against the task total.
    /// Bounded by the budget's own `max_extensions`.
    #[serde(default)]
    pub round_extensions: u32,
    /// Consecutive windows that produced no effective work. The convergence
    /// guard: at `MAX_NO_PROGRESS_WINDOWS` the supervisor stops opening more.
    #[serde(default)]
    pub windows_without_progress: u32,
    /// Segment baseline — source-file mutations recorded when the current
    /// budget segment began. A round extension is earned by moving PAST this,
    /// so a forgotten baseline reads every prior change as new work.
    #[serde(default)]
    pub segment_source_changes: usize,
    /// Segment baseline — refused close attempts when the segment began.
    #[serde(default)]
    pub segment_close_attempts: usize,
    /// Progress baseline — distinct modified files at the last window boundary.
    #[serde(default)]
    pub progress_files_mark: usize,
    /// Progress baseline — total mutation operations at the last boundary.
    #[serde(default)]
    pub progress_ops_mark: u64,
    /// Progress baseline — whether a green verification covered the latest
    /// mutations at the last boundary.
    #[serde(default)]
    pub progress_fresh_verify_mark: bool,
}

impl WindowState {
    /// Note that a window opened.
    pub fn open_window(&mut self) {
        self.window_index = self.window_index.saturating_add(1);
    }

    /// Fold one window's progress verdict into the convergence guard.
    pub fn note_window_progress(&mut self, made_progress: bool) {
        self.windows_without_progress = if made_progress {
            0
        } else {
            self.windows_without_progress.saturating_add(1)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one property recovery depends on: a state read back from its own
    /// serialized form is the state that was written, guards and all.
    #[test]
    fn a_serialized_state_round_trips_every_guard() {
        let state = WindowState {
            window_index: 3,
            budget_extensions: 2,
            round_extensions: 1,
            windows_without_progress: 2,
            segment_source_changes: 7,
            segment_close_attempts: 4,
            progress_files_mark: 5,
            progress_ops_mark: 11,
            progress_fresh_verify_mark: true,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<WindowState>(&json).unwrap(), state);
    }

    /// An older log has no window state at all; reading one must produce the
    /// clean slate, not fail the resume that needed it.
    #[test]
    fn an_absent_field_reads_as_a_fresh_slate() {
        let state: WindowState = serde_json::from_str("{}").unwrap();
        assert_eq!(state, WindowState::default());
    }

    #[test]
    fn progress_resets_the_convergence_guard_and_its_absence_advances_it() {
        let mut state = WindowState::default();
        state.note_window_progress(false);
        state.note_window_progress(false);
        assert_eq!(state.windows_without_progress, 2);
        state.note_window_progress(true);
        assert_eq!(state.windows_without_progress, 0);
    }
}
