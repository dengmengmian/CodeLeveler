//! Structured plan types shared by agent, engine, and hosts.
//!
//! A plan is the agent's declared progress: what it intends to do, what it has
//! declared done, and what it declares it is working on. The runtime accepts a
//! structurally valid declaration, persists it, restores it on resume and
//! projects it to clients; it does not observe whether the work is really at
//! the declared step, and the plan is never a completion authority.
//!
//! Exactly one runtime reading exists, and it is not a completion verdict: a
//! turn that goes quiet while its own latest declaration still lists open steps
//! is told that fact once (see [`PlanState::open_steps`] and the closeout in
//! `leveler-agent`). The model decides whether those steps are still owed.
//!
//! The sole plan event remains `PlanUpdated` (full-list replace). Hosts keep an
//! in-memory [`PlanState`] mirror of the latest steps; resume seeds from the
//! last persisted `PlanUpdated` payload — the last declared plan, not a
//! reconstruction of what happened. No parallel plan event type.

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

/// Legacy status synthesized for a step omitted from a later declaration.
///
/// Retained for replay of old events. New declarations replace the complete
/// list as sent; previous declarations remain in the durable event log.
pub const ABANDONED: &str = "abandoned";

fn is_closed(status: &str) -> bool {
    status == "completed" || status == ABANDONED
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

    /// True when every step is `completed` (empty plan is not complete).
    ///
    /// Used by the executor closeout guard: once the model has marked the whole
    /// plan done, pure observe-only tool thrashing (git status, re-list) is
    /// refused so the turn can finish instead of burning tokens.
    pub fn is_fully_completed(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|s| s.status.as_str() == "completed")
    }

    /// The steps the model published and never declared `completed`, in order.
    ///
    /// This is the model's own record, not a verdict about the work: a step
    /// listed here may be unstarted, in progress, or obsolete. Its one reader
    /// is the closeout (`leveler-agent`): a Chat turn that goes quiet with this
    /// list non-empty is answered with the fact instead of being treated as
    /// finished work. Nothing marks these steps done, and a full list is not
    /// completion evidence either.
    ///
    /// Legacy [`ABANDONED`] steps remain closed on replay. New declarations
    /// never synthesize a status from an omitted or reworded step.
    pub fn open_steps(&self) -> Vec<&PlanStep> {
        self.steps
            .iter()
            .filter(|step| !is_closed(&step.status))
            .collect()
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
    fn a_plan_with_open_steps_is_not_fully_completed() {
        let explicit = PlanState::from_model_explicit(vec![PlanStep {
            step: "a".into(),
            status: "pending".into(),
            id: None,
            origin: PlanOrigin::ModelExplicit,
        }])
        .unwrap();
        assert!(!explicit.is_fully_completed());

        let implicit = PlanState::host_implicit_single_step("fix it");
        assert!(!implicit.is_fully_completed());
    }

    #[test]
    fn legacy_abandoned_steps_remain_closed_on_replay() {
        let plan: PlanState = serde_json::from_value(serde_json::json!({
            "steps": [
                {"step": "implementation", "status": "completed"},
                {"step": "old declaration", "status": "abandoned"}
            ]
        }))
        .unwrap();
        assert!(plan.open_steps().is_empty());
        assert!(!plan.is_fully_completed());
        assert_eq!(plan.steps[1].status, ABANDONED);
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
        assert_eq!(plan.open_steps().len(), 1);
        assert_eq!(plan.open_steps()[0].step, "b");
        plan.steps[1].status = "completed".into();
        assert!(plan.is_fully_completed());
        assert!(plan.open_steps().is_empty());
    }
}
