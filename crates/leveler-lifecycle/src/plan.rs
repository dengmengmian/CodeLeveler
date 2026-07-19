//! Structured plan types shared by agent, engine, and gates.
//!
//! The sole plan event remains `PlanUpdated` (full-list replace). Hosts keep an
//! in-memory [`PlanState`] mirror of the latest steps; resume seeds from the
//! last persisted `PlanUpdated` payload. No parallel plan event type.

use serde::{Deserialize, Serialize};

/// Who created a plan step table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanOrigin {
    /// Model called `update_plan` (or equivalent explicit plan).
    #[default]
    ModelExplicit,
    /// Legacy/resume: host once synthesized a single-step plan for a simple Goal.
    /// Default runtime no longer seeds this for short tasks; keep for event replay.
    HostImplicit,
}

/// One step of the model- or host-maintained plan.
///
/// `status` stays a wire string (`pending` | `in_progress` | `completed`) so
/// older event rows keep replaying without a forced enum migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: String,
    /// Optional stable id for complete_step / ledger; absent on legacy events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Absent on legacy payloads → [`PlanOrigin::ModelExplicit`].
    #[serde(default)]
    pub origin: PlanOrigin,
}

/// Host memory mirror of the latest successful `PlanUpdated.steps`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanState {
    pub steps: Vec<PlanStep>,
}

impl PlanState {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Every step shares one origin; mixed tables are invalid.
    pub fn origin(&self) -> Option<PlanOrigin> {
        let first = self.steps.first()?.origin;
        if self.steps.iter().all(|s| s.origin == first) {
            Some(first)
        } else {
            None
        }
    }

    pub fn is_host_implicit(&self) -> bool {
        matches!(self.origin(), Some(PlanOrigin::HostImplicit))
    }

    pub fn is_model_explicit(&self) -> bool {
        matches!(self.origin(), Some(PlanOrigin::ModelExplicit))
    }

    /// True when a ModelExplicit plan still has pending or in_progress work.
    pub fn has_incomplete_model_todos(&self) -> bool {
        if !self.is_model_explicit() {
            return false;
        }
        self.steps.iter().any(|s| {
            let st = s.status.as_str();
            st == "pending" || st == "in_progress"
        })
    }

    /// True when every step is `completed` (empty plan is not complete).
    ///
    /// Used by the executor closeout guard: once the model has marked the whole
    /// plan done, pure observe-only tool thrashing (git status, re-list) is
    /// refused so the turn can finish instead of burning tokens.
    pub fn is_fully_completed(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|s| s.status.as_str() == "completed")
    }

    /// Build a host single-step plan (tests / resume repair). Not seeded by default
    /// for short Goals — those run without a plan shell until `update_plan`.
    pub fn host_implicit_single_step(task: &str) -> Self {
        let text = task.trim();
        let step = if text.is_empty() {
            "Complete the goal".to_string()
        } else {
            // Cap length so a huge task dump does not blow the plan panel.
            let mut s = text.chars().take(200).collect::<String>();
            if text.chars().count() > 200 {
                s.push('…');
            }
            s
        };
        Self {
            steps: vec![PlanStep {
                step,
                status: "in_progress".to_string(),
                id: Some("host-implicit-0".to_string()),
                origin: PlanOrigin::HostImplicit,
            }],
        }
    }

    /// Mark every step completed (used when HostImplicit completes with the goal).
    pub fn mark_all_completed(&mut self) {
        for step in &mut self.steps {
            step.status = "completed".to_string();
        }
    }

    /// Force every step to ModelExplicit (successful update_plan path).
    pub fn from_model_explicit(mut steps: Vec<PlanStep>) -> Result<Self, String> {
        if steps.is_empty() {
            return Err("plan must have at least one step".to_string());
        }
        for step in &mut steps {
            step.origin = PlanOrigin::ModelExplicit;
        }
        Ok(Self { steps })
    }

    /// Reject pending→completed jumps against the previous table (same step text).
    pub fn validate_no_skip_complete(previous: &PlanState, next: &PlanState) -> Result<(), String> {
        if previous.is_empty() {
            return Ok(());
        }
        // A step may go straight pending → completed (finishing the current
        // step in one shot is normal, not a skip). The only real skip is
        // completing a step while an EARLIER step in the list is still
        // unfinished — that jumps ahead of outstanding work.
        for (i, next_step) in next.steps.iter().enumerate() {
            if next_step.status != "completed" {
                continue;
            }
            let was_pending = previous
                .steps
                .iter()
                .find(|p| p.step == next_step.step)
                .is_some_and(|p| p.status == "pending");
            if !was_pending {
                continue;
            }
            let earlier_all_completed = next.steps[..i].iter().all(|s| s.status == "completed");
            if !earlier_all_completed {
                return Err(format!(
                    "plan step \"{}\" cannot be completed while an earlier step is \
                     still unfinished; complete the steps in order",
                    next_step.step
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_plan_step_json_defaults_origin_to_model_explicit() {
        let step: PlanStep =
            serde_json::from_str(r#"{"step":"edit","status":"in_progress"}"#).unwrap();
        assert_eq!(step.origin, PlanOrigin::ModelExplicit);
        assert!(step.id.is_none());
    }

    #[test]
    fn plan_origin_round_trips() {
        let step = PlanStep {
            step: "a".into(),
            status: "pending".into(),
            id: Some("1".into()),
            origin: PlanOrigin::HostImplicit,
        };
        let v = serde_json::to_value(&step).unwrap();
        let back: PlanStep = serde_json::from_value(v).unwrap();
        assert_eq!(back, step);
    }

    #[test]
    fn mixed_origin_table_is_detected() {
        let state = PlanState {
            steps: vec![
                PlanStep {
                    step: "a".into(),
                    status: "pending".into(),
                    id: None,
                    origin: PlanOrigin::ModelExplicit,
                },
                PlanStep {
                    step: "b".into(),
                    status: "pending".into(),
                    id: None,
                    origin: PlanOrigin::HostImplicit,
                },
            ],
        };
        assert!(state.origin().is_none());
    }

    #[test]
    fn incomplete_todos_only_for_model_explicit() {
        let explicit = PlanState::from_model_explicit(vec![PlanStep {
            step: "a".into(),
            status: "pending".into(),
            id: None,
            origin: PlanOrigin::ModelExplicit,
        }])
        .unwrap();
        assert!(explicit.has_incomplete_model_todos());
        assert!(!explicit.is_fully_completed());

        let implicit = PlanState::host_implicit_single_step("fix it");
        assert!(!implicit.has_incomplete_model_todos());
        assert!(!implicit.is_fully_completed());
    }

    #[test]
    fn fully_completed_requires_every_step_done() {
        let mut plan = PlanState::from_model_explicit(vec![
            PlanStep {
                step: "a".into(),
                status: "completed".into(),
                id: None,
                origin: PlanOrigin::ModelExplicit,
            },
            PlanStep {
                step: "b".into(),
                status: "in_progress".into(),
                id: None,
                origin: PlanOrigin::ModelExplicit,
            },
        ])
        .unwrap();
        assert!(!plan.is_fully_completed());
        plan.steps[1].status = "completed".into();
        assert!(plan.is_fully_completed());
        assert!(!plan.has_incomplete_model_todos());
    }

    fn step(name: &str, status: &str) -> PlanStep {
        PlanStep {
            step: name.into(),
            status: status.into(),
            id: None,
            origin: PlanOrigin::ModelExplicit,
        }
    }

    #[test]
    fn allows_completing_the_current_step_directly() {
        // Finishing the first outstanding step in one shot (pending → completed
        // without a separate in_progress hop) is normal, not a skip — the model
        // shouldn't be forced through a two-step ritual it finds unnatural.
        let prev = PlanState::from_model_explicit(vec![step("a", "pending"), step("b", "pending")])
            .unwrap();
        let next =
            PlanState::from_model_explicit(vec![step("a", "completed"), step("b", "pending")])
                .unwrap();
        assert!(PlanState::validate_no_skip_complete(&prev, &next).is_ok());
    }

    #[test]
    fn rejects_completing_a_step_before_an_earlier_unfinished_one() {
        // Completing `b` while `a` (earlier in the list) is still unfinished is
        // a real skip — that stays rejected.
        let prev = PlanState::from_model_explicit(vec![step("a", "pending"), step("b", "pending")])
            .unwrap();
        let next =
            PlanState::from_model_explicit(vec![step("a", "pending"), step("b", "completed")])
                .unwrap();
        assert!(PlanState::validate_no_skip_complete(&prev, &next).is_err());
    }
}
