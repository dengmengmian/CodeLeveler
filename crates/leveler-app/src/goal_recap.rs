//! Projection: a persisted GoalCheckpoint → the wire `UiGoalRecap`.
//!
//! One projection for every delivery path (live `GoalRecapCreated` events and
//! snapshot reconstruction), so the TUI always presents the same persisted
//! facts. Truth rules survive the mapping: `CheckpointFindings::Unknown`
//! stays `None` (never zero), `Unmeasured` verification stays the string
//! `"unmeasured"` (never a pass), and `display_summary` falls back to the
//! deterministic structured rendering — a durable checkpoint always presents.

use leveler_client_protocol::UiGoalRecap;
use leveler_lifecycle::{CheckpointFindings, CheckpointVerification, GoalCheckpoint};
use leveler_storage::GoalCheckpointRecord;

/// Project a persisted checkpoint row.
pub(crate) fn project_goal_recap(record: &GoalCheckpointRecord) -> UiGoalRecap {
    project_goal_recap_parts(
        record.id.as_str(),
        record.goal_id.as_str(),
        record.reason.as_str(),
        &record.created_at.to_rfc3339(),
        &record.payload,
    )
}

/// Project from the parts an `EngineEvent::GoalCheckpointCreated` carries.
pub(crate) fn project_goal_recap_parts(
    checkpoint_id: &str,
    goal_id: &str,
    reason: &str,
    created_at: &str,
    payload: &GoalCheckpoint,
) -> UiGoalRecap {
    let (verification, verification_detail) = match &payload.verification {
        CheckpointVerification::Passed { evidence } => ("passed", Some(evidence.clone())),
        CheckpointVerification::Failed { detail } => ("failed", Some(detail.clone())),
        CheckpointVerification::Unmeasured => ("unmeasured", None),
    };
    let (findings_total, findings_open, findings_blocking) = match &payload.findings {
        CheckpointFindings::Known {
            total,
            open,
            open_blocking,
            ..
        } => (Some(*total), Some(*open), Some(*open_blocking)),
        // UNKNOWN is None on the wire — a client must never render it as 0.
        CheckpointFindings::Unknown => (None, None, None),
    };
    UiGoalRecap {
        checkpoint_id: checkpoint_id.to_string(),
        goal_id: goal_id.to_string(),
        reason: reason.to_string(),
        created_at: created_at.to_string(),
        transcript_ordinal: payload.transcript_ordinal,
        display_summary: payload
            .display_summary
            .clone()
            .unwrap_or_else(|| payload.fallback_display_summary()),
        phase: payload.phase.clone(),
        next_action: payload
            .next_action
            .clone()
            .or_else(|| payload.plan.as_ref().and_then(|p| p.next_step.clone())),
        plan_completed: payload.plan.as_ref().map(|p| p.completed),
        plan_total: payload.plan.as_ref().map(|p| p.total),
        completed_milestones: payload.completed_milestones.clone(),
        verification: verification.to_string(),
        verification_detail,
        findings_total,
        findings_open,
        findings_blocking,
        known_limitations: payload.known_limitations.clone(),
        unresolved_work: payload.unresolved_work.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_lifecycle::CheckpointPlan;

    /// The wire projection must keep UNKNOWN distinct from zero and
    /// UNMEASURED distinct from passed — the same rule as everywhere else.
    #[test]
    fn unknown_stays_unknown_on_the_wire() {
        let payload = GoalCheckpoint {
            objective: "x".into(),
            ..Default::default()
        };
        let recap =
            project_goal_recap_parts("c1", "g1", "manual", "2026-08-27T00:00:00Z", &payload);
        assert_eq!(recap.findings_total, None, "unknown findings are not zero");
        assert_eq!(recap.verification, "unmeasured");
        assert_eq!(recap.verification_detail, None);
        assert!(
            !recap.display_summary.is_empty(),
            "a structured checkpoint always presents"
        );
    }

    #[test]
    fn next_action_falls_back_to_the_plan_step() {
        let payload = GoalCheckpoint {
            objective: "x".into(),
            plan: Some(CheckpointPlan {
                total: 3,
                completed: 1,
                completed_steps: vec![],
                next_step: Some("implement persistence".into()),
            }),
            ..Default::default()
        };
        let recap =
            project_goal_recap_parts("c1", "g1", "manual", "2026-08-27T00:00:00Z", &payload);
        assert_eq!(recap.next_action.as_deref(), Some("implement persistence"));
        assert_eq!(recap.plan_completed, Some(1));
        assert_eq!(recap.plan_total, Some(3));
    }
}
