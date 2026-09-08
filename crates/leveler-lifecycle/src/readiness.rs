//! Pure readiness checks for `update_goal(complete)`.
//!
//! No I/O, no shell, no Verifier, no model. The one refusal here is a
//! mechanical fact the runtime can state from its own record: the model's own
//! plan still lists open steps. Whether the work satisfies the user's intent
//! is the model's judgement and the user's acceptance — never decided here.
//!
//! The user's mechanical acceptance is the project's own `verify:` commands
//! (`.leveler/config.yaml`), run by the Verifier and reported as
//! `VerificationStatus`. This gate used to ALSO enforce acceptance commands
//! guessed out of the goal prose by a `TaskContract` parser; a sentence the
//! user wrote is not a command they asked for.

use serde::{Deserialize, Serialize};

use crate::plan::PlanState;

/// Why `update_goal(complete)` was refused by the mechanical readiness gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadinessFailure {
    #[error(
        "plan still has incomplete todos ({pending} pending, {in_progress} in progress; override_allowed={override_allowed})"
    )]
    IncompleteTodos {
        pending: usize,
        in_progress: usize,
        override_allowed: bool,
    },
}

/// Gate knobs for update_goal(complete).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateConfig {
    pub goal_todo_gate: bool,
    /// When true, an **explicit** `override_incomplete_todos` flag (or user
    /// approval) may clear incomplete ModelExplicit todos. Attempt count alone
    /// never bypasses the gate.
    pub todo_override_allowed: bool,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            goal_todo_gate: true,
            todo_override_allowed: true,
        }
    }
}

/// Mechanical readiness check against the model's own plan.
///
/// `explicit_todo_override` must be true (from structured `update_goal` args or
/// a user-approved path) to clear incomplete ModelExplicit todos when
/// [`GateConfig::todo_override_allowed`] is set. A bare second attempt is not enough.
pub fn check(
    plan: &PlanState,
    cfg: &GateConfig,
    explicit_todo_override: bool,
) -> Result<(), ReadinessFailure> {
    if cfg.goal_todo_gate && plan.has_incomplete_model_todos() {
        let allow_override = cfg.todo_override_allowed && explicit_todo_override;
        if !allow_override {
            let pending = plan.steps.iter().filter(|s| s.status == "pending").count();
            let in_progress = plan
                .steps
                .iter()
                .filter(|s| s.status == "in_progress")
                .count();
            return Err(ReadinessFailure::IncompleteTodos {
                pending,
                in_progress,
                override_allowed: cfg.todo_override_allowed,
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanOrigin, PlanStep};

    fn one_pending() -> PlanState {
        PlanState {
            steps: vec![PlanStep {
                step: "a".into(),
                status: "pending".into(),
                id: None,
                origin: PlanOrigin::ModelExplicit,
            }],
        }
    }

    #[test]
    fn todo_override_requires_explicit_flag_not_attempt_count() {
        let cfg = GateConfig {
            todo_override_allowed: true,
            ..GateConfig::default()
        };
        // Bare second (or nth) complete without the flag still refuses.
        assert!(check(&one_pending(), &cfg, false).is_err());
        assert!(check(&one_pending(), &cfg, false).is_err());
        assert!(check(&one_pending(), &cfg, true).is_ok());
    }

    #[test]
    fn todo_override_disallowed_never_passes() {
        let cfg = GateConfig {
            todo_override_allowed: false,
            ..GateConfig::default()
        };
        assert!(check(&one_pending(), &cfg, true).is_err());
    }

    /// The authority boundary: "fix the login bug" with no mutation and no
    /// verification is still the model's call. The runtime has no keyword
    /// classifier, no proof standard, and — since the free-text contract was
    /// deleted — no acceptance command guessed out of the prose to refuse it
    /// with. What the user wrote as `verify:` is enforced by the Verifier and
    /// reported beside the outcome, not here.
    #[test]
    fn an_empty_plan_is_never_refused() {
        assert!(check(&PlanState::default(), &GateConfig::default(), false).is_ok());
    }

    /// The gate can be switched off entirely; then even open todos pass.
    #[test]
    fn a_disabled_todo_gate_refuses_nothing() {
        let cfg = GateConfig {
            goal_todo_gate: false,
            ..GateConfig::default()
        };
        assert!(check(&one_pending(), &cfg, false).is_ok());
    }
}
