//! Presentation-only fold state for a transcript entry.
//!
//! How much of an entry is painted is a *view* decision. It carries no
//! lifecycle, no persistence and no domain meaning: the runtime never learns
//! it, a replayed session never restores it, and a fold never changes what an
//! entry says — only how much of it is on screen.
//!
//! One entry therefore owns exactly one [`DisplayMode`]. The alternative —
//! a per-kind `expanded: bool` on each block — made a Thought, a tool group and
//! a run answer the same question three different ways.

use ratatui::text::Line;

/// How much of a foldable transcript entry is painted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DisplayMode {
    /// Header (and outcome) only. The default for a finished entry: a settled
    /// body is evidence, not something the reader has to wade through.
    Collapsed,
    /// Header plus a bounded preview of the body's TAIL. The tail is what an
    /// entry that is still being written is writing; a finished entry has
    /// nothing at the end to justify the space, so it never defaults here.
    Truncated,
    /// Header plus the whole body. The explicit user choice.
    #[default]
    Expanded,
}

impl DisplayMode {
    pub const fn is_collapsed(self) -> bool {
        matches!(self, Self::Collapsed)
    }

    pub const fn is_expanded(self) -> bool {
        matches!(self, Self::Expanded)
    }

    /// How much body is painted. Comparing ranks is how a fold tells whether it
    /// grew (reading intent) or shrank.
    pub const fn rank(self) -> u8 {
        match self {
            Self::Collapsed => 0,
            Self::Truncated => 1,
            Self::Expanded => 2,
        }
    }
}

/// Body lines a `Truncated` preview keeps. Enough to see what is happening,
/// never enough to swallow the viewport.
pub const TRUNCATED_LINES: usize = 6;

/// The mode one manual toggle lands on.
///
/// A running entry never lands on `Collapsed`: the user asked to see less, not
/// to lose sight of work in progress. `Collapsed` and `Truncated` both open to
/// `Expanded`, so one toggle always reveals the body; toggling a running entry
/// a second time returns it to its live preview rather than a blank header.
pub const fn toggled(current: DisplayMode, running: bool) -> DisplayMode {
    if running {
        match current {
            DisplayMode::Collapsed | DisplayMode::Truncated => DisplayMode::Expanded,
            DisplayMode::Expanded => DisplayMode::Truncated,
        }
    } else {
        match current {
            DisplayMode::Collapsed => DisplayMode::Expanded,
            DisplayMode::Truncated | DisplayMode::Expanded => DisplayMode::Collapsed,
        }
    }
}

/// The mode a manual collapse lands on.
pub const fn collapse_target(running: bool) -> DisplayMode {
    if running {
        DisplayMode::Truncated
    } else {
        DisplayMode::Collapsed
    }
}

/// Truncate a body to [`TRUNCATED_LINES`] by keeping its last lines, and mark
/// the elision with a leading `…` row so a preview never reads as the whole
/// body. A body that already fits is left untouched.
pub fn truncate_to_tail(lines: &mut Vec<Line<'static>>, marker: Line<'static>) {
    let limit = TRUNCATED_LINES.saturating_sub(1).max(1);
    if lines.len() <= TRUNCATED_LINES {
        return;
    }
    lines.drain(..lines.len() - limit);
    lines.insert(0, marker);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_toggle_opens_and_closes() {
        assert_eq!(
            toggled(DisplayMode::Collapsed, false),
            DisplayMode::Expanded
        );
        assert_eq!(
            toggled(DisplayMode::Expanded, false),
            DisplayMode::Collapsed
        );
        assert_eq!(
            toggled(DisplayMode::Truncated, false),
            DisplayMode::Collapsed
        );
    }

    /// A running entry cannot be folded away to a bare header: the live preview
    /// is the floor, and one toggle from it reveals the whole body.
    #[test]
    fn a_running_toggle_never_reaches_a_bare_header() {
        assert_eq!(toggled(DisplayMode::Truncated, true), DisplayMode::Expanded);
        assert_eq!(toggled(DisplayMode::Expanded, true), DisplayMode::Truncated);
        assert_eq!(toggled(DisplayMode::Collapsed, true), DisplayMode::Expanded);
        assert_eq!(collapse_target(true), DisplayMode::Truncated);
        assert_eq!(collapse_target(false), DisplayMode::Collapsed);
    }

    #[test]
    fn a_short_body_keeps_every_line() {
        let mut lines: Vec<Line<'static>> = (0..TRUNCATED_LINES).map(|_| Line::from("x")).collect();
        truncate_to_tail(&mut lines, Line::from("…"));
        assert_eq!(lines.len(), TRUNCATED_LINES);
        assert_ne!(lines[0].spans[0].content, "…");
    }

    #[test]
    fn a_long_body_keeps_its_tail_under_a_marker() {
        let mut lines: Vec<Line<'static>> = (0..40).map(|_| Line::from("x")).collect();
        truncate_to_tail(&mut lines, Line::from("…"));
        assert_eq!(lines.len(), TRUNCATED_LINES);
        assert_eq!(lines[0].spans[0].content, "…");
    }
}
