//! Pure readiness checks for `update_goal(complete)`.
//!
//! No I/O, no shell, no Verifier, no model. Every refusal here is a mechanical
//! fact the runtime can state from its own record: the model's own plan still
//! lists open steps, or a command the USER explicitly named as acceptance has
//! not run green. Whether the work satisfies the user's intent is the model's
//! judgement and the user's acceptance — never decided here.

use serde::{Deserialize, Serialize};

use crate::contract::TaskContract;
use crate::ledger::EvidenceLedger;
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
    #[error("acceptance commands unmet: {commands:?}")]
    AcceptanceCommandsUnmet { commands: Vec<String> },
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

/// Mechanical readiness check against the model's own plan and the user's
/// explicit acceptance commands.
///
/// `explicit_todo_override` must be true (from structured `update_goal` args or
/// a user-approved path) to clear incomplete ModelExplicit todos when
/// [`GateConfig::todo_override_allowed`] is set. A bare second attempt is not enough.
///
/// Acceptance commands are checked whenever the user wrote them: they are the
/// one place the user states, mechanically, what "done" must include.
pub fn check(
    plan: &PlanState,
    ledger: &EvidenceLedger,
    contract: Option<&TaskContract>,
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

    if let Some(contract) = contract {
        let mut unmet = Vec::new();
        for cmd in &contract.acceptance_commands {
            let fp = normalize_acceptance(cmd);
            let ok = ledger.verifications.iter().any(|v| {
                v.exit_code == 0
                    && (v.command_fingerprint == fp || v.command_fingerprint.contains(cmd.trim()))
            });
            if !ok {
                unmet.push(cmd.clone());
            }
        }
        if !unmet.is_empty() {
            return Err(ReadinessFailure::AcceptanceCommandsUnmet { commands: unmet });
        }
    }

    Ok(())
}

fn normalize_acceptance(cmd: &str) -> String {
    let parts: Vec<String> = cmd.split_whitespace().map(|s| s.to_string()).collect();
    if parts.is_empty() {
        return String::new();
    }
    EvidenceLedger::normalize_command_fingerprint(&parts[0], &parts[1..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanOrigin, PlanStep};

    #[test]
    fn todo_override_requires_explicit_flag_not_attempt_count() {
        let plan = PlanState {
            steps: vec![PlanStep {
                step: "a".into(),
                status: "pending".into(),
                id: None,
                origin: PlanOrigin::ModelExplicit,
            }],
        };
        let cfg = GateConfig {
            todo_override_allowed: true,
            ..GateConfig::default()
        };
        let led = EvidenceLedger::default();
        // Bare second (or nth) complete without the flag still refuses.
        assert!(check(&plan, &led, None, &cfg, false).is_err());
        assert!(check(&plan, &led, None, &cfg, false).is_err());
        assert!(check(&plan, &led, None, &cfg, true).is_ok());
    }

    #[test]
    fn todo_override_disallowed_never_passes() {
        let plan = PlanState {
            steps: vec![PlanStep {
                step: "a".into(),
                status: "pending".into(),
                id: None,
                origin: PlanOrigin::ModelExplicit,
            }],
        };
        let cfg = GateConfig {
            todo_override_allowed: false,
            ..GateConfig::default()
        };
        let led = EvidenceLedger::default();
        assert!(check(&plan, &led, None, &cfg, true).is_err());
    }

    /// Case 1 of the authority boundary: "fix bug" with no mutation and no
    /// verification is still the model's call. The runtime has no keyword
    /// classifier and no proof standard to refuse it with.
    #[test]
    fn no_mutation_and_no_verification_is_not_refused() {
        let cfg = GateConfig::default();
        assert!(
            check(
                &PlanState::default(),
                &EvidenceLedger::default(),
                Some(&TaskContract::parse("fix the login bug")),
                &cfg,
                false,
            )
            .is_ok()
        );
    }

    /// A build-relevant mutation with no fresh verification is reported by
    /// the terminal verification status, never refused at the claim.
    #[test]
    fn stale_verification_after_mutation_is_not_refused() {
        let cfg = GateConfig::default();
        let mut led = EvidenceLedger::default();
        led.record_mutation("c1", "apply_patch", vec!["src/lib.rs".into()]);
        led.record_verify("v1", "cargo\u{1f}test", 0);
        led.record_mutation("c2", "replace", vec!["src/lib.rs".into()]);
        assert!(check(&PlanState::default(), &led, None, &cfg, false).is_ok());
    }

    /// Case 4: a command the user explicitly wrote as acceptance is a
    /// mechanical fact the runtime owns, and it is checked unconditionally.
    #[test]
    fn acceptance_commands_must_appear_in_ledger() {
        let cfg = GateConfig::default();
        let mut led = EvidenceLedger::default();
        led.record_mutation("c1", "apply_patch", vec![]);
        led.record_verify("v1", "cargo\u{1f}test", 0);
        let contract = TaskContract {
            acceptance_commands: vec!["cargo clippy".into()],
            ..Default::default()
        };
        assert!(matches!(
            check(&PlanState::default(), &led, Some(&contract), &cfg, false),
            Err(ReadinessFailure::AcceptanceCommandsUnmet { .. })
        ));
        led.record_verify("v2", "cargo\u{1f}clippy", 0);
        assert!(check(&PlanState::default(), &led, Some(&contract), &cfg, false).is_ok());
    }

    /// Case 5: natural-language "make sure this is tested" is not an
    /// acceptance command and produces no mechanical requirement.
    #[test]
    fn prose_about_testing_is_not_an_acceptance_command() {
        let contract = TaskContract::parse("确保这个行为被充分测试。");
        assert!(contract.acceptance_commands.is_empty());
        assert!(
            check(
                &PlanState::default(),
                &EvidenceLedger::default(),
                Some(&contract),
                &GateConfig::default(),
                false,
            )
            .is_ok()
        );
    }
}
