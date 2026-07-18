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
        for next_step in &next.steps {
            if next_step.status != "completed" {
                continue;
            }
            if let Some(prev) = previous.steps.iter().find(|p| p.step == next_step.step)
                && prev.status == "pending"
            {
                return Err(format!(
                    "plan step \"{}\" cannot jump from pending to completed; \
                     mark it in_progress first, then complete it",
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

    #[test]
    fn rejects_pending_to_completed_skip() {
        let prev = PlanState::from_model_explicit(vec![
            PlanStep {
                step: "a".into(),
                status: "pending".into(),
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
        let next = PlanState::from_model_explicit(vec![
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
        assert!(PlanState::validate_no_skip_complete(&prev, &next).is_err());
    }
}
