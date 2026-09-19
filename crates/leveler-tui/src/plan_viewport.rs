//! Plan summary window: which steps the sticky plan dock shows by default.
//!
//! The dock is a summary surface, not the plan's full history — the archived
//! plan in the transcript owns completeness. A finished step is no longer worth
//! a permanent row, so the default window drops every settled step and keeps
//! only unfinished work: at most [`MAX_VISIBLE_ACTIVE_STEPS`], always including
//! the step the plan declares in progress, with one quiet row naming whatever it
//! left off. This module never mutates the plan; it only chooses what to paint.

use leveler_client_protocol::{PlanStepStatus, UiPlan, UiPlanStep};

/// How many unfinished steps the default summary keeps on screen.
///
/// This is a cap on *unfinished* steps, not a fixed row count: a plan with two
/// open steps shows two rows and is not padded with finished history.
pub(crate) const MAX_VISIBLE_ACTIVE_STEPS: usize = 5;

/// The default plan summary: settled steps drop out, at most
/// [`MAX_VISIBLE_ACTIVE_STEPS`] unfinished steps remain, and the step the plan
/// declares in progress stays visible even when unusual ordering would push it
/// outside the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanSummaryWindow<'a> {
    /// Unfinished steps to render, in plan order.
    pub visible: Vec<&'a UiPlanStep>,
    /// Unfinished steps left off the window: the exact count behind
    /// "… N more to do". Never the settled history, never a step of its own.
    pub hidden_active: usize,
    /// Whether the hidden-count row fits in the body budget.
    pub show_hidden_line: bool,
}

impl PlanSummaryWindow<'_> {
    /// Body rows this window paints: the visible steps plus the optional
    /// hidden-count row.
    pub(crate) fn desired_rows(&self) -> usize {
        self.visible.len() + usize::from(self.show_hidden_line)
    }
}

/// A settled step is one the plan will not return to.
///
/// The header's [`crate::workbench::plan_done_total`] counts on this same
/// predicate, so the summary and the global count cannot disagree about what
/// finished means.
pub(crate) fn is_plan_step_settled(status: PlanStepStatus) -> bool {
    matches!(status, PlanStepStatus::Done | PlanStepStatus::Skipped)
}

/// The step the window must keep visible: the running one, else the first
/// failure, else the next pending. Never invents a "current" step.
pub(crate) fn plan_focus_index(plan: &UiPlan) -> usize {
    let find = |want: PlanStepStatus| plan.steps.iter().position(|s| s.status == want);
    find(PlanStepStatus::Running)
        .or_else(|| find(PlanStepStatus::Failed))
        .or_else(|| find(PlanStepStatus::Pending))
        .unwrap_or(0)
}

/// The default summary window for a body with `body_budget` rows available.
///
/// Invariants: settled steps never appear in `visible`; `visible` holds at most
/// [`MAX_VISIBLE_ACTIVE_STEPS`] unfinished steps in plan order; the focus step
/// is always present when `body_budget > 0`; `hidden_active` is exactly the
/// number of unfinished steps not shown; and [`PlanSummaryWindow::desired_rows`]
/// never exceeds `body_budget`.
pub(crate) fn plan_summary_window(plan: &UiPlan, body_budget: usize) -> PlanSummaryWindow<'_> {
    // (plan position, step) so the focus step can be matched without trusting
    // `UiPlanStep::index` to equal its slice position.
    let active: Vec<(usize, &UiPlanStep)> = plan
        .steps
        .iter()
        .enumerate()
        .filter(|(_, s)| !is_plan_step_settled(s.status))
        .collect();
    if active.is_empty() || body_budget == 0 {
        return PlanSummaryWindow {
            visible: Vec::new(),
            hidden_active: active.len(),
            show_hidden_line: false,
        };
    }
    let focus_plan_pos = plan_focus_index(plan);
    let focus = active
        .iter()
        .position(|(i, _)| *i == focus_plan_pos)
        .unwrap_or(0);
    // Default cap: five unfinished steps, plus one row for the count when
    // anything is left over. A short dock shrinks the window further rather
    // than overflowing.
    let mut take = active.len().min(MAX_VISIBLE_ACTIVE_STEPS);
    while take > 0 && take + usize::from(active.len() > take) > body_budget {
        take -= 1;
    }
    if take == 0 {
        // One row left: the focus step wins it and the count row yields.
        take = 1;
    }
    // First `take` unfinished steps, shifted only enough to keep the focus step
    // on screen. Plan order is never rearranged.
    let mut start = 0;
    let mut end = take;
    if focus >= end {
        end = focus + 1;
        start = end - take;
    }
    let visible: Vec<&UiPlanStep> = active[start..end].iter().map(|(_, s)| *s).collect();
    let hidden_active = active.len() - visible.len();
    let show_hidden_line = hidden_active > 0 && visible.len() < body_budget;
    PlanSummaryWindow {
        visible,
        hidden_active,
        show_hidden_line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(statuses: &[PlanStepStatus]) -> UiPlan {
        UiPlan {
            steps: statuses
                .iter()
                .enumerate()
                .map(|(i, s)| UiPlanStep {
                    index: i,
                    description: format!("step {}", i + 1),
                    status: *s,
                })
                .collect(),
        }
    }

    /// `done` finished steps, then one running, then pending up to `total`.
    fn running_at(total: usize, current: usize) -> UiPlan {
        let statuses: Vec<PlanStepStatus> = (0..total)
            .map(|i| match i.cmp(&current) {
                std::cmp::Ordering::Less => PlanStepStatus::Done,
                std::cmp::Ordering::Equal => PlanStepStatus::Running,
                std::cmp::Ordering::Greater => PlanStepStatus::Pending,
            })
            .collect();
        plan(&statuses)
    }

    fn shown_indices(window: &PlanSummaryWindow<'_>) -> Vec<usize> {
        window.visible.iter().map(|s| s.index).collect()
    }

    #[test]
    fn four_active_steps_show_four_and_hide_none() {
        let p = running_at(10, 6); // 6 done, 1 running, 3 pending
        let w = plan_summary_window(&p, 10);
        assert_eq!(shown_indices(&w), vec![6, 7, 8, 9]);
        assert_eq!(w.hidden_active, 0);
        assert!(!w.show_hidden_line);
        assert_eq!(w.desired_rows(), 4);
    }

    #[test]
    fn seventeen_active_steps_show_five_and_count_twelve() {
        let p = running_at(20, 3); // 3 done, 1 running, 16 pending
        let w = plan_summary_window(&p, 10);
        assert_eq!(w.visible.len(), 5);
        assert_eq!(w.hidden_active, 12);
        assert!(w.show_hidden_line);
        assert_eq!(w.desired_rows(), 6);
    }

    #[test]
    fn exactly_five_open_steps_show_five_without_a_count_row() {
        let p = running_at(10, 5); // 5 done, running + 4 pending = 5 open
        let w = plan_summary_window(&p, 10);
        assert_eq!(w.visible.len(), 5);
        assert_eq!(w.hidden_active, 0);
        assert!(!w.show_hidden_line);
    }

    #[test]
    fn three_open_steps_show_three_without_padding() {
        let p = running_at(10, 7); // 7 done, running + 2 pending = 3 open
        let w = plan_summary_window(&p, 10);
        assert_eq!(w.visible.len(), 3);
        assert_eq!(w.hidden_active, 0);
        assert!(!w.show_hidden_line);
    }

    #[test]
    fn a_fully_settled_plan_hides_every_step() {
        let p = plan(&[PlanStepStatus::Done; 10]);
        let w = plan_summary_window(&p, 10);
        assert!(w.visible.is_empty());
        assert_eq!(w.hidden_active, 0);
        assert!(!w.show_hidden_line);
        assert_eq!(w.desired_rows(), 0);
    }

    #[test]
    fn settled_steps_never_reach_the_default_window() {
        let p = plan(&[
            PlanStepStatus::Done,
            PlanStepStatus::Skipped,
            PlanStepStatus::Running,
            PlanStepStatus::Done,
            PlanStepStatus::Pending,
        ]);
        let w = plan_summary_window(&p, 10);
        assert_eq!(shown_indices(&w), vec![2, 4]);
    }

    #[test]
    fn skipped_counts_as_settled_like_done() {
        let p = plan(&[PlanStepStatus::Skipped, PlanStepStatus::Pending]);
        let w = plan_summary_window(&p, 10);
        assert_eq!(shown_indices(&w), vec![1]);
    }

    #[test]
    fn a_failure_is_unfinished_and_kept_ahead_of_trailing_pending() {
        let p = plan(&[
            PlanStepStatus::Done,
            PlanStepStatus::Failed,
            PlanStepStatus::Pending,
        ]);
        let w = plan_summary_window(&p, 10);
        assert_eq!(shown_indices(&w), vec![1, 2]);
    }

    #[test]
    fn the_focus_step_survives_a_one_row_body() {
        let p = running_at(30, 17);
        let w = plan_summary_window(&p, 1);
        assert_eq!(shown_indices(&w), vec![17]);
        assert_eq!(w.hidden_active, 12); // 13 unfinished minus the one shown
        assert!(!w.show_hidden_line);
        assert_eq!(w.desired_rows(), 1);
    }

    #[test]
    fn a_short_body_shrinks_the_window_and_keeps_the_count_exact() {
        let p = running_at(30, 17); // active = 18..30 -> 13 steps
        let w = plan_summary_window(&p, 2);
        assert_eq!(w.visible.len(), 1);
        assert!(w.show_hidden_line);
        assert_eq!(w.hidden_active, 12);
        assert!(
            shown_indices(&w).contains(&17),
            "the running step stays on screen: {:?}",
            shown_indices(&w)
        );
    }

    #[test]
    fn the_focus_step_is_visible_when_ordering_would_push_it_past_the_cap() {
        // Six unfinished steps before the running one: a naive "first five"
        // window would drop the step that is actually executing.
        let mut statuses = vec![PlanStepStatus::Pending; 6];
        statuses.push(PlanStepStatus::Running);
        statuses.push(PlanStepStatus::Pending);
        let p = plan(&statuses);
        let w = plan_summary_window(&p, 10);
        assert_eq!(w.visible.len(), MAX_VISIBLE_ACTIVE_STEPS);
        assert!(
            shown_indices(&w).contains(&6),
            "running step missing: {:?}",
            shown_indices(&w)
        );
        assert_eq!(w.hidden_active, 3);
    }

    #[test]
    fn a_zero_budget_renders_nothing_and_hides_the_count_row() {
        let p = running_at(30, 17);
        let w = plan_summary_window(&p, 0);
        assert!(w.visible.is_empty());
        assert_eq!(w.hidden_active, 13);
        assert!(!w.show_hidden_line);
        assert_eq!(w.desired_rows(), 0);
    }

    #[test]
    fn an_empty_plan_is_empty() {
        let p = UiPlan { steps: vec![] };
        let w = plan_summary_window(&p, 10);
        assert!(w.visible.is_empty());
        assert_eq!(w.hidden_active, 0);
        assert!(!w.show_hidden_line);
    }

    #[test]
    fn the_count_row_only_ever_names_hidden_unfinished_steps() {
        for (total, current) in [(10usize, 6usize), (20, 3), (10, 5), (10, 7), (30, 17)] {
            let p = running_at(total, current);
            let w = plan_summary_window(&p, 10);
            let active = total - current; // done = current, rest unfinished
            assert_eq!(
                w.visible.len() + w.hidden_active,
                active,
                "{total}/{current}: shown + hidden must equal unfinished"
            );
            assert_eq!(w.show_hidden_line, w.hidden_active > 0);
        }
    }

    #[test]
    fn every_budget_keeps_focus_and_never_overflows() {
        for total in 1..=30usize {
            for current in 0..total {
                let p = running_at(total, current);
                for budget in 0..=(total + 2) {
                    let w = plan_summary_window(&p, budget);
                    assert!(
                        w.desired_rows() <= budget.max(0),
                        "{total}/{current}/{budget}: {:?}",
                        w.desired_rows()
                    );
                    if budget > 0 {
                        assert!(
                            w.visible.iter().any(|s| s.index == current),
                            "focus lost at {total}/{current}/{budget}: {:?}",
                            shown_indices(&w)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn focus_falls_back_to_the_first_failure_then_the_next_pending() {
        let p = plan(&[
            PlanStepStatus::Done,
            PlanStepStatus::Failed,
            PlanStepStatus::Pending,
        ]);
        assert_eq!(plan_focus_index(&p), 1);
        let p = plan(&[
            PlanStepStatus::Done,
            PlanStepStatus::Done,
            PlanStepStatus::Pending,
        ]);
        assert_eq!(plan_focus_index(&p), 2);
    }
}
