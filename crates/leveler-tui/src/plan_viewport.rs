//! Plan viewport: which plan steps fit the rows the layout can actually spare.
//!
//! Pure presentation. The sticky plan dock used to hardcode "header + at most
//! five steps" and then `take()` whatever the row count allowed, so a 9-step
//! plan silently lost items 6..9 — including the one that was running. This
//! module answers the real question instead: given N rows, which steps are
//! shown, and how many are hidden on each side.

use leveler_client_protocol::{PlanStepStatus, UiPlan, UiPlanStep};

/// One row of the rendered plan body (the header is the caller's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanViewportRow<'a> {
    Step(&'a UiPlanStep),
    /// `n` steps scrolled off the top.
    HiddenBefore(usize),
    /// `n` steps scrolled off the bottom.
    HiddenAfter(usize),
    /// Both sides hidden with only one row to say so (very short terminals).
    HiddenBoth {
        before: usize,
        after: usize,
    },
}

/// The step the viewport must keep visible: the running one, else the first
/// failure, else the next pending. Never invents a "current" step.
pub(crate) fn plan_focus_index(plan: &UiPlan) -> usize {
    let find = |want: PlanStepStatus| plan.steps.iter().position(|s| s.status == want);
    find(PlanStepStatus::Running)
        .or_else(|| find(PlanStepStatus::Failed))
        .or_else(|| find(PlanStepStatus::Pending))
        .unwrap_or(0)
}

/// Rows the plan body wants when nothing is cut: one per step.
pub(crate) fn plan_desired_body_rows(plan: &UiPlan) -> usize {
    plan.steps.len()
}

/// The body rows for `budget` available rows.
///
/// Invariants: the result never exceeds `budget`; every step is present when
/// they all fit; the focus step is always present; the hidden counts are the
/// exact number of steps outside the window.
pub(crate) fn plan_viewport_rows(plan: &UiPlan, budget: usize) -> Vec<PlanViewportRow<'_>> {
    let n = plan.steps.len();
    if budget == 0 || n == 0 {
        return Vec::new();
    }
    if n <= budget {
        return plan.steps.iter().map(PlanViewportRow::Step).collect();
    }
    let focus = plan_focus_index(plan);
    // Largest step window that still fits once its overflow indicators are
    // counted. `budget < n` here, so this always shrinks to a real answer.
    for cap in (1..=budget).rev() {
        let (start, end) = window(n, focus, cap);
        let before = start;
        let after = n - end;
        let rows = (end - start) + usize::from(before > 0) + usize::from(after > 0);
        if rows <= budget {
            let mut out = Vec::with_capacity(rows);
            if before > 0 {
                out.push(PlanViewportRow::HiddenBefore(before));
            }
            out.extend(plan.steps[start..end].iter().map(PlanViewportRow::Step));
            if after > 0 {
                out.push(PlanViewportRow::HiddenAfter(after));
            }
            return out;
        }
    }
    // Too short for a step plus both indicators: the focus step wins the row,
    // and the overflow degrades to one compact line if there is a row left.
    let (start, end) = window(n, focus, 1);
    let mut out = vec![PlanViewportRow::Step(&plan.steps[start])];
    if budget >= 2 {
        out.push(PlanViewportRow::HiddenBoth {
            before: start,
            after: n - end,
        });
    }
    out
}

/// A `cap`-wide window over `0..n` that contains `focus`, kept in range.
fn window(n: usize, focus: usize, cap: usize) -> (usize, usize) {
    debug_assert!(cap <= n && cap > 0);
    let start = focus.saturating_sub(cap / 2).min(n - cap);
    (start, start + cap)
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

    /// `done` steps, then one running, then pending up to `total`.
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

    fn step_indices(rows: &[PlanViewportRow<'_>]) -> Vec<usize> {
        rows.iter()
            .filter_map(|r| match r {
                PlanViewportRow::Step(s) => Some(s.index),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_step_shows_when_the_budget_is_large_enough() {
        let p = running_at(9, 5);
        let rows = plan_viewport_rows(&p, 9);
        assert_eq!(step_indices(&rows), (0..9).collect::<Vec<_>>());
        assert!(
            !rows.iter().any(|r| !matches!(r, PlanViewportRow::Step(_))),
            "no overflow indicator when nothing is hidden"
        );
    }

    #[test]
    fn the_running_step_stays_visible_when_the_budget_is_short() {
        let p = running_at(9, 5);
        let rows = plan_viewport_rows(&p, 6);
        assert!(rows.len() <= 6);
        assert!(step_indices(&rows).contains(&5), "{rows:?}");
        let hidden: usize = rows
            .iter()
            .map(|r| match r {
                PlanViewportRow::HiddenBefore(n) | PlanViewportRow::HiddenAfter(n) => *n,
                _ => 0,
            })
            .sum();
        assert_eq!(hidden, 9 - step_indices(&rows).len());
    }

    #[test]
    fn a_deep_plan_windows_around_the_running_step_with_exact_counts() {
        let p = running_at(30, 17);
        let rows = plan_viewport_rows(&p, 6);
        assert!(rows.len() <= 6);
        let shown = step_indices(&rows);
        assert!(shown.contains(&17), "{rows:?}");
        let before = match rows.first() {
            Some(PlanViewportRow::HiddenBefore(n)) => *n,
            other => panic!("expected a top indicator, got {other:?}"),
        };
        let after = match rows.last() {
            Some(PlanViewportRow::HiddenAfter(n)) => *n,
            other => panic!("expected a bottom indicator, got {other:?}"),
        };
        assert_eq!(before, *shown.first().unwrap());
        assert_eq!(after, 30 - 1 - *shown.last().unwrap());
        assert_eq!(before + shown.len() + after, 30);
    }

    #[test]
    fn no_top_overflow_is_invented_near_the_start() {
        let p = running_at(9, 1);
        let rows = plan_viewport_rows(&p, 5);
        assert!(
            !matches!(rows.first(), Some(PlanViewportRow::HiddenBefore(_))),
            "{rows:?}"
        );
        assert_eq!(step_indices(&rows).first(), Some(&0));
    }

    #[test]
    fn the_tail_wins_when_the_current_step_is_last() {
        let p = running_at(30, 28);
        let rows = plan_viewport_rows(&p, 6);
        let shown = step_indices(&rows);
        assert!(shown.contains(&28), "{rows:?}");
        assert_eq!(shown.last(), Some(&29), "the end of the plan is visible");
        assert!(
            !matches!(rows.last(), Some(PlanViewportRow::HiddenAfter(_))),
            "nothing is hidden below the last step: {rows:?}"
        );
    }

    #[test]
    fn a_single_body_row_keeps_the_current_step() {
        let p = running_at(30, 17);
        let rows = plan_viewport_rows(&p, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(step_indices(&rows), vec![17]);
    }

    #[test]
    fn two_body_rows_degrade_overflow_to_one_compact_line() {
        let p = running_at(30, 17);
        let rows = plan_viewport_rows(&p, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(step_indices(&rows), vec![17]);
        assert_eq!(
            rows[1],
            PlanViewportRow::HiddenBoth {
                before: 17,
                after: 12
            }
        );
    }

    #[test]
    fn a_zero_budget_renders_nothing_instead_of_panicking() {
        let p = running_at(30, 17);
        assert!(plan_viewport_rows(&p, 0).is_empty());
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

    #[test]
    fn every_budget_and_focus_respects_the_row_cap_and_keeps_focus() {
        for total in 1..=30usize {
            for current in 0..total {
                let p = running_at(total, current);
                for budget in 0..=(total + 2) {
                    let rows = plan_viewport_rows(&p, budget);
                    assert!(rows.len() <= budget.max(0), "{total}/{current}/{budget}");
                    if budget > 0 {
                        assert!(
                            step_indices(&rows).contains(&current),
                            "focus lost at {total}/{current}/{budget}: {rows:?}"
                        );
                    }
                }
            }
        }
    }
}
