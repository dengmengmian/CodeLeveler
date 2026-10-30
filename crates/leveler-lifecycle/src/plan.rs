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

/// Status of a step the model dropped from its own plan while it was open.
///
/// It is not `completed` (nothing was finished) and it is not open (the model
/// is not working on it). It exists so the record and the projection can tell
/// a replaced step from a delivered one.
pub const ABANDONED: &str = "abandoned";

/// The most replaced steps the live plan keeps. The durable event log keeps
/// every declaration regardless.
const MAX_CARRIED_ABANDONED: usize = 12;

fn is_closed(status: &str) -> bool {
    status == "completed" || status == ABANDONED
}

/// Identity of a step across declarations: its stable id when one exists, else
/// its exact text. Never a similarity score.
fn step_key(step: &PlanStep) -> String {
    match step.id.as_deref() {
        Some(id) if !id.is_empty() => format!("id:{id}"),
        _ => format!("text:{}", step.step.trim()),
    }
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
    /// A step the harness recorded as [`ABANDONED`] is not open: the model
    /// replaced it rather than doing it, and the record says so. Keeping it
    /// here would drive a continuation for work the model has already decided
    /// not to do, which is a loop, not progress.
    pub fn open_steps(&self) -> Vec<&PlanStep> {
        self.steps
            .iter()
            .filter(|step| !is_closed(&step.status))
            .collect()
    }

    /// Steps the model removed from its plan while they were still open.
    ///
    /// They are kept in the list so the projection can show that a step was
    /// replaced rather than finished. Nothing here is a completion claim, and
    /// nothing here is a requirement to resume it.
    pub fn abandoned_steps(&self) -> Vec<&PlanStep> {
        self.steps
            .iter()
            .filter(|step| step.status.as_str() == ABANDONED)
            .collect()
    }

    /// Replace the declared list, keeping the record of steps that were
    /// dropped while still open.
    ///
    /// A re-declaration is the model's right: plans change when the work
    /// changes. What must not silently change is the RECORD — a step that said
    /// `真实 iOS 真机验证` yesterday and is replaced today by `判定是否具备真机验证
    /// 条件` was not completed, and a projection that shows 100% would be
    /// claiming delivery that never happened. The harness therefore keeps such
    /// a step with status [`ABANDONED`] (carried forward across later
    /// re-declarations) and returns the ones this declaration dropped, so the
    /// caller can tell the model which step it replaced instead of finishing.
    ///
    /// Carried steps are bounded so one long task cannot grow the list without
    /// limit; the newest are kept and the slice below is the only policy.
    pub fn absorb_declaration(&mut self, declared: Vec<PlanStep>) -> Vec<PlanStep> {
        let declared_keys: Vec<String> = declared.iter().map(step_key).collect();
        let previous = std::mem::take(&mut self.steps);
        let mut next = declared;
        let mut newly_abandoned = Vec::new();
        let mut carried: Vec<PlanStep> = Vec::new();
        for step in previous {
            if declared_keys.contains(&step_key(&step)) {
                continue;
            }
            if step.status.as_str() == ABANDONED {
                carried.push(step);
            } else if !is_closed(&step.status) {
                let mut abandoned = step;
                abandoned.status = ABANDONED.to_string();
                newly_abandoned.push(abandoned.clone());
                carried.push(abandoned);
            }
        }
        // Oldest abandoned steps fall off first; the event stream still holds
        // every declaration, so this is only the live list's bound.
        if carried.len() > MAX_CARRIED_ABANDONED {
            carried.drain(..carried.len() - MAX_CARRIED_ABANDONED);
        }
        next.extend(carried);
        self.steps = next;
        newly_abandoned
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
    fn a_step_dropped_while_open_is_recorded_as_abandoned_not_done() {
        let mut plan = PlanState::from_model_explicit(vec![
            PlanStep {
                step: "真实 iOS 真机验证".into(),
                status: "in_progress".into(),
                id: None,
                origin: PlanOrigin::ModelExplicit,
            },
            PlanStep {
                step: "补齐文档".into(),
                status: "completed".into(),
                id: None,
                origin: PlanOrigin::ModelExplicit,
            },
        ])
        .unwrap();
        let replaced = PlanState::from_model_explicit(vec![PlanStep {
            step: "判定是否具备真实 iOS 真机验证条件".into(),
            status: "completed".into(),
            id: None,
            origin: PlanOrigin::ModelExplicit,
        }])
        .unwrap();
        let newly = plan.absorb_declaration(replaced.steps);
        assert_eq!(newly.len(), 1);
        assert_eq!(newly[0].status, ABANDONED);
        assert_eq!(newly[0].step, "真实 iOS 真机验证");
        assert!(
            plan.abandoned_steps()
                .iter()
                .any(|step| step.step == "真实 iOS 真机验证"),
            "the replaced step stays visible as abandoned: {:?}",
            plan.steps
        );
        assert!(
            plan.open_steps().is_empty(),
            "an abandoned step is not owed work: {:?}",
            plan.steps
        );
        assert!(
            !plan.is_fully_completed(),
            "a plan with an abandoned step is not 100% delivered"
        );

        // A later declaration that does not mention it again must not erase
        // the record, and must not re-open it either.
        let next = PlanState::from_model_explicit(vec![PlanStep {
            step: "收尾".into(),
            status: "completed".into(),
            id: None,
            origin: PlanOrigin::ModelExplicit,
        }])
        .unwrap();
        assert!(plan.absorb_declaration(next.steps).is_empty());
        assert_eq!(plan.abandoned_steps().len(), 1);
        assert!(plan.open_steps().is_empty());
    }

    #[test]
    fn re_declaring_an_open_step_keeps_it_open_rather_than_abandoning_it() {
        let mut plan = PlanState::from_model_explicit(vec![PlanStep {
            step: "跑测试".into(),
            status: "in_progress".into(),
            id: None,
            origin: PlanOrigin::ModelExplicit,
        }])
        .unwrap();
        let next = PlanState::from_model_explicit(vec![PlanStep {
            step: "跑测试".into(),
            status: "pending".into(),
            id: None,
            origin: PlanOrigin::ModelExplicit,
        }])
        .unwrap();
        assert!(plan.absorb_declaration(next.steps).is_empty());
        assert_eq!(plan.open_steps().len(), 1);
        assert!(plan.abandoned_steps().is_empty());
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
