//! The `/update` surface: presentation for a self-update in flight.
//!
//! The TUI owns only what the user sees. The update itself — resolving,
//! downloading, verifying, installing — is `leveler-update`'s job; this module
//! folds its progress steps into a small view and draws them.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use leveler_update::{UpdateOutcome, UpdateStep, Version};

use crate::theme::Theme;

/// Which step the update is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePhase {
    Checking,
    Downloading,
    Verifying,
    Installing,
    /// The binary on disk is new; the process is about to be replaced.
    Installed,
    UpToDate,
    Failed,
}

/// What the `/update` panel shows.
#[derive(Debug, Clone)]
pub struct UpdateView {
    pub current: Version,
    pub latest: Option<Version>,
    pub phase: UpdatePhase,
    pub received: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
}

impl UpdateView {
    pub fn new() -> Self {
        Self {
            current: leveler_update::current_version(),
            latest: None,
            phase: UpdatePhase::Checking,
            received: 0,
            total: None,
            error: None,
        }
    }

    /// Fold one update step. Non-terminal steps leave the phase where it is.
    pub fn apply(&mut self, step: UpdateStep) {
        match step {
            UpdateStep::Checking => self.phase = UpdatePhase::Checking,
            UpdateStep::Available { current, latest } => {
                self.current = current;
                self.latest = Some(latest);
                self.phase = UpdatePhase::Downloading;
            }
            UpdateStep::Downloading {
                received, total, ..
            } => {
                self.phase = UpdatePhase::Downloading;
                self.received = received;
                self.total = total;
            }
            UpdateStep::Verifying => self.phase = UpdatePhase::Verifying,
            UpdateStep::Installing => self.phase = UpdatePhase::Installing,
            UpdateStep::Installed { to, .. } => {
                self.latest = Some(to);
                self.phase = UpdatePhase::Installed;
            }
            UpdateStep::UpToDate { current } => {
                self.current = current;
                self.phase = UpdatePhase::UpToDate;
            }
        }
    }

    /// The update ended without installing. `message` is the honest reason.
    pub fn fail(&mut self, message: String) {
        self.phase = UpdatePhase::Failed;
        self.error = Some(message);
    }

    /// Fold the terminal outcome (already installed → restart; else stay).
    pub fn finish(&mut self, outcome: Result<UpdateOutcome, String>) {
        match outcome {
            Ok(UpdateOutcome::UpToDate { current }) => {
                self.current = current;
                self.phase = UpdatePhase::UpToDate;
            }
            Ok(UpdateOutcome::Installed { to, .. }) => {
                self.latest = Some(to);
                self.phase = UpdatePhase::Installed;
            }
            Err(message) => self.fail(message),
        }
    }

    /// Whether the panel has reached a state the user can dismiss.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.phase,
            UpdatePhase::Installed | UpdatePhase::UpToDate | UpdatePhase::Failed
        )
    }

    /// Whether the process must be replaced to run the new binary.
    pub fn wants_restart(&self) -> bool {
        self.phase == UpdatePhase::Installed
    }
}

impl Default for UpdateView {
    fn default() -> Self {
        Self::new()
    }
}

/// Draw the update panel, centred over the workbench.
pub fn render_panel(
    frame: &mut Frame,
    area: Rect,
    view: &UpdateView,
    theme: &Theme,
    t: &crate::i18n::UiText,
) {
    if area.width < 12 || area.height < 6 {
        // No room for a panel: a one-line notice beats a nested mess.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                phase_label(view, t).to_string(),
                Style::default().fg(theme.text.secondary),
            ))),
            Rect { height: 1, ..area },
        );
        return;
    }

    let width = area.width.clamp(12, 64);
    let height = 9u16.min(area.height);
    let panel = centered(area, width, height);
    frame.render_widget(Clear, panel);

    let title = format!(" {} ", t.update_panel_title);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border.normal))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.text.primary)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(6);
    lines.push(kv(t.update_current, format!("v{}", view.current), theme));
    lines.push(kv(
        t.update_latest,
        view.latest
            .as_ref()
            .map(|v| format!("v{v}"))
            .unwrap_or_else(|| "—".to_string()),
        theme,
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        phase_label(view, t),
        phase_style(view, theme),
    )));
    if view.phase == UpdatePhase::Downloading
        && let Some(bar) = progress_bar(view)
    {
        lines.push(Line::from(Span::styled(
            format!("  {bar}"),
            Style::default().fg(theme.accent.primary),
        )));
    }
    if let Some(error) = &view.error {
        // Clamp so a long reason cannot push the panel off-screen.
        let shown = crate::render::truncate_display(error, inner.width as usize);
        lines.push(Line::from(Span::styled(
            format!("  {shown}"),
            Style::default().fg(theme.status.error),
        )));
    } else {
        lines.push(Line::from(""));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn kv(label: &str, value: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {label:<9}"),
            Style::default().fg(theme.text.muted),
        ),
        Span::styled(value, Style::default().fg(theme.text.primary)),
    ])
}

fn phase_label(view: &UpdateView, t: &crate::i18n::UiText) -> &'static str {
    match view.phase {
        UpdatePhase::Checking => t.update_checking,
        UpdatePhase::Downloading => t.update_downloading,
        UpdatePhase::Verifying => t.update_verifying,
        UpdatePhase::Installing => t.update_installing,
        UpdatePhase::Installed => t.update_installed_restarting,
        UpdatePhase::UpToDate => t.update_up_to_date,
        UpdatePhase::Failed => t.update_failed,
    }
}

fn phase_style(view: &UpdateView, theme: &Theme) -> Style {
    match view.phase {
        UpdatePhase::Installed | UpdatePhase::UpToDate => Style::default().fg(theme.status.success),
        UpdatePhase::Failed => Style::default().fg(theme.status.error),
        _ => Style::default().fg(theme.text.secondary),
    }
}

/// A real bar only when the server reported a content length; otherwise the
/// step text carries the meaning and no percentage is invented.
fn progress_bar(view: &UpdateView) -> Option<String> {
    let total = view.total.filter(|t| *t > 0)?;
    let width = 24usize;
    let done = ((view.received.min(total) as u128 * width as u128) / total as u128) as usize;
    let pct = view.received.min(total) * 100 / total;
    Some(format!(
        "{}{} {pct:>3}%",
        "█".repeat(done),
        "░".repeat(width - done)
    ))
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_update::parse_version;

    fn v(s: &str) -> Version {
        parse_version(s).unwrap()
    }

    #[test]
    fn steps_advance_the_phase_and_keep_the_versions() {
        let mut view = UpdateView::new();
        view.apply(UpdateStep::Available {
            current: v("1.0.0"),
            latest: v("1.0.1"),
        });
        assert_eq!(view.phase, UpdatePhase::Downloading);
        assert_eq!(view.latest, Some(v("1.0.1")));

        view.apply(UpdateStep::Downloading {
            asset: "x.tar.gz".into(),
            received: 50,
            total: Some(100),
        });
        assert_eq!(view.phase, UpdatePhase::Downloading);
        assert_eq!(view.received, 50);

        view.apply(UpdateStep::Verifying);
        assert_eq!(view.phase, UpdatePhase::Verifying);
        view.apply(UpdateStep::Installing);
        assert_eq!(view.phase, UpdatePhase::Installing);
        view.apply(UpdateStep::Installed {
            from: v("1.0.0"),
            to: v("1.0.1"),
        });
        assert!(view.is_terminal());
        assert!(view.wants_restart());
    }

    #[test]
    fn up_to_date_is_terminal_but_does_not_restart() {
        let mut view = UpdateView::new();
        view.apply(UpdateStep::UpToDate {
            current: v("1.0.1"),
        });
        assert!(view.is_terminal());
        assert!(!view.wants_restart());
    }

    #[test]
    fn a_failure_is_terminal_and_carries_the_reason() {
        let mut view = UpdateView::new();
        view.fail("checksum verification failed".into());
        assert!(view.is_terminal());
        assert_eq!(view.error.as_deref(), Some("checksum verification failed"));
        assert!(!view.wants_restart());
    }

    #[test]
    fn a_bar_needs_a_known_total() {
        let mut view = UpdateView::new();
        view.apply(UpdateStep::Downloading {
            asset: "x".into(),
            received: 5,
            total: None,
        });
        assert!(progress_bar(&view).is_none());
        view.total = Some(10);
        assert!(progress_bar(&view).unwrap().contains("50%"));
    }
}
