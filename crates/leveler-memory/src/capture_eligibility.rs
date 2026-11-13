//! Which settled turns automatic capture may still queue.
//!
//! Capture cursors are positions in the settled user/chat turn list. A cursor
//! by itself cannot tell a crash (the turn was eligible and the job never
//! finished) from a turn the user excluded by pausing or turning memory off.
//! Callers persist that decision on the session row, then ask this planner
//! for the next enqueue and skip steps. Skipped turns advance the cursor
//! without a job, so a hole does not get retried and does not stall later
//! turns.

use crate::capture_store::{CaptureError, CaptureRange};

/// Whether one settled user/chat turn may be queued for automatic capture.
///
/// `Unknown` is a turn this process still owes a decision for. The planner
/// stops before it so a missing write is retried instead of being skipped
/// forever or captured by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureAdmission {
    /// Memory was on when the turn settled, and no capture job has covered it yet.
    Eligible,
    /// The user paused memory, or memory was globally off, when the turn settled.
    /// Historical turns with no eligibility row are also not eligible.
    NotEligible,
    /// This process has not recorded a decision yet. Do not skip or capture it.
    Unknown,
}

/// One monotonic step that moves the requested cursor forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePlanStep {
    /// Queue a capture job for this inclusive index range.
    Enqueue(CaptureRange),
    /// Record that this inclusive index range must not be captured, and move
    /// the requested cursor past it without a model call.
    Skip(CaptureRange),
}

/// Plan the capture work still owed after `requested`.
///
/// `admissions` is the settled user/chat list in order. Index 1 is the first
/// entry. Turns at or below `requested` are already acknowledged. An
/// [`CaptureAdmission::Unknown`] entry stops the plan: nothing after it is
/// queued or skipped until the caller records a decision.
pub fn plan_capture_admissions(
    requested: u32,
    admissions: &[CaptureAdmission],
) -> Result<Vec<CapturePlanStep>, CaptureError> {
    if admissions.len() > 999_999 {
        return Err(CaptureError::Invalid(
            "session exceeds the durable capture turn limit".to_owned(),
        ));
    }
    let len = u32::try_from(admissions.len()).map_err(|_| {
        CaptureError::Invalid("session exceeds the durable capture turn limit".to_owned())
    })?;
    let mut steps = Vec::new();
    let mut index = requested.saturating_add(1);
    while index <= len {
        let Some(admission) = admission_at(admissions, index)? else {
            break;
        };
        match admission {
            CaptureAdmission::Unknown => break,
            CaptureAdmission::NotEligible => {
                let from = index;
                let mut through = index;
                index = index.saturating_add(1);
                while index <= len {
                    match admission_at(admissions, index)? {
                        Some(CaptureAdmission::NotEligible) => {
                            through = index;
                            index = index.saturating_add(1);
                        }
                        _ => break,
                    }
                }
                steps.push(CapturePlanStep::Skip(CaptureRange::try_new(from, through)?));
            }
            CaptureAdmission::Eligible => {
                let from = index;
                let mut through = index;
                index = index.saturating_add(1);
                while index <= len {
                    match admission_at(admissions, index)? {
                        Some(CaptureAdmission::Eligible) => {
                            through = index;
                            index = index.saturating_add(1);
                        }
                        _ => break,
                    }
                }
                steps.push(CapturePlanStep::Enqueue(CaptureRange::try_new(
                    from, through,
                )?));
            }
        }
    }
    Ok(steps)
}

fn admission_at(
    admissions: &[CaptureAdmission],
    index: u32,
) -> Result<Option<CaptureAdmission>, CaptureError> {
    let position = usize::try_from(index.saturating_sub(1)).map_err(|_| {
        CaptureError::Invalid("capture turn index does not fit this process".to_owned())
    })?;
    Ok(admissions.get(position).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step_kinds(steps: &[CapturePlanStep]) -> Vec<(&'static str, u32, u32)> {
        steps
            .iter()
            .map(|step| match step {
                CapturePlanStep::Enqueue(range) => {
                    ("enqueue", range.start_turn(), range.through_turn())
                }
                CapturePlanStep::Skip(range) => ("skip", range.start_turn(), range.through_turn()),
            })
            .collect()
    }

    #[test]
    fn paused_and_disabled_turns_are_skipped_and_eligible_turns_stay_queued() {
        use CaptureAdmission::{Eligible, NotEligible};
        // Turn 1 on, 2–3 paused, 4–5 on (5 not yet captured), 6 globally off, 7 on.
        let admissions = [
            Eligible,
            NotEligible,
            NotEligible,
            Eligible,
            Eligible,
            NotEligible,
            Eligible,
        ];
        let steps = plan_capture_admissions(0, &admissions).unwrap();
        assert_eq!(
            step_kinds(&steps),
            vec![
                ("enqueue", 1, 1),
                ("skip", 2, 3),
                ("enqueue", 4, 5),
                ("skip", 6, 6),
                ("enqueue", 7, 7),
            ]
        );
    }

    #[test]
    fn a_crash_before_enqueue_still_queues_the_eligible_turn_and_not_the_gap() {
        use CaptureAdmission::{Eligible, NotEligible};
        // Turns 1 and 4 are already requested. Turn 5 was eligible when the
        // process exited before its job was queued. Turn 6 is not eligible.
        let admissions = [
            Eligible,
            NotEligible,
            NotEligible,
            Eligible,
            Eligible,
            NotEligible,
            Eligible,
        ];
        let steps = plan_capture_admissions(4, &admissions).unwrap();
        assert_eq!(
            step_kinds(&steps),
            vec![("enqueue", 5, 5), ("skip", 6, 6), ("enqueue", 7, 7)]
        );
    }

    #[test]
    fn an_unrecorded_turn_stops_the_plan_instead_of_being_captured_or_skipped() {
        use CaptureAdmission::{Eligible, Unknown};
        let admissions = [Eligible, Unknown, Eligible];
        let steps = plan_capture_admissions(0, &admissions).unwrap();
        assert_eq!(step_kinds(&steps), vec![("enqueue", 1, 1)]);
    }

    #[test]
    fn legacy_turns_with_no_eligibility_are_skipped_without_a_job() {
        use CaptureAdmission::NotEligible;
        let admissions = [NotEligible, NotEligible, NotEligible];
        let steps = plan_capture_admissions(0, &admissions).unwrap();
        assert_eq!(step_kinds(&steps), vec![("skip", 1, 3)]);
    }

    #[test]
    fn an_already_requested_prefix_is_not_planned_again() {
        use CaptureAdmission::{Eligible, NotEligible};
        let admissions = [Eligible, NotEligible, Eligible];
        assert!(plan_capture_admissions(3, &admissions).unwrap().is_empty());
    }
}
