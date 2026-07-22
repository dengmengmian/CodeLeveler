//! Pure readiness checks for process gates (todo + delivery ledger).
//!
//! No I/O, no shell, no Verifier.

use serde::{Deserialize, Serialize};

use crate::contract::TaskContract;
use crate::impact::ChangeImpact;
use crate::ledger::EvidenceLedger;
use crate::plan::PlanState;

/// Why `update_goal(complete)` was refused by the process gate.
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
    #[error("delivery gate: implementation task has no recorded workspace mutation")]
    UnprovenNoMutation,
    #[error(
        "delivery gate: mutations exist but no successful verification after last mutation (last_mutation_seq={last_mutation_seq})"
    )]
    MissingVerification { last_mutation_seq: u64 },
    #[error("delivery gate: step `{step_id}` lacks a fresh evidence_ref")]
    MissingEvidenceRef { step_id: String },
    #[error("delivery gate: acceptance commands unmet: {commands:?}")]
    AcceptanceCommandsUnmet { commands: Vec<String> },
}

/// Process evidence snapshot for simple callers (maps into ledger rules).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessEvidence {
    pub mutation_count: u32,
    pub verification_passed_after_mutation: bool,
    pub task_looks_like_implementation: bool,
}

/// Gate knobs for update_goal(complete).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateConfig {
    pub goal_todo_gate: bool,
    /// When true, an **explicit** `override_incomplete_todos` flag (or user
    /// approval) may clear incomplete ModelExplicit todos. Attempt count alone
    /// never bypasses the gate.
    pub todo_override_allowed: bool,
    pub delivery_gate: bool,
    pub reject_unproven_no_mutation: bool,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            goal_todo_gate: true,
            todo_override_allowed: true,
            delivery_gate: false,
            reject_unproven_no_mutation: false,
        }
    }
}

impl GateConfig {
    pub fn for_work_profile(work: crate::WorkProfile) -> Self {
        let delivery = matches!(work, crate::WorkProfile::Delivery);
        Self {
            goal_todo_gate: true,
            todo_override_allowed: true,
            delivery_gate: delivery,
            reject_unproven_no_mutation: delivery,
        }
    }
}

/// Full readiness check against plan + ledger + optional contract.
///
/// `explicit_todo_override` must be true (from structured `update_goal` args or
/// a user-approved path) to clear incomplete ModelExplicit todos when
/// [`GateConfig::todo_override_allowed`] is set. A bare second attempt is not enough.
pub fn check(
    plan: &PlanState,
    ledger: &EvidenceLedger,
    contract: Option<&TaskContract>,
    cfg: &GateConfig,
    explicit_todo_override: bool,
    task_looks_like_implementation: bool,
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

    if cfg.delivery_gate {
        let last_mut = ledger.last_mutation_seq();
        if cfg.reject_unproven_no_mutation && last_mut == 0 && task_looks_like_implementation {
            return Err(ReadinessFailure::UnprovenNoMutation);
        }
        if last_mut > 0 && !ledger.has_fresh_successful_verify() {
            // Inert changes (docs, scripts, deleted non-build files) cannot
            // affect the build, so they never require a fresh verify — the
            // same `is_build_relevant` heuristic the verifier uses to scope
            // its gates, via the shared ChangeImpact view.
            let impact = ChangeImpact::from_ledger(ledger);
            if impact.build_relevant {
                return Err(ReadinessFailure::MissingVerification {
                    last_mutation_seq: last_mut,
                });
            }
        }
        // Explicit plan steps that finished need complete_step receipts with fresh evidence.
        if plan.is_model_explicit() && !plan.steps.is_empty() {
            for step in &plan.steps {
                if step.status != "completed" {
                    continue;
                }
                let id = step.id.clone().unwrap_or_else(|| step.step.clone());
                let receipt = ledger
                    .step_receipts
                    .iter()
                    .find(|r| r.step_id == id || r.step_text == step.step);
                match receipt {
                    None if plan.steps.len() > 1 => {
                        // Multi-step explicit plans require complete_step for completed rows.
                        return Err(ReadinessFailure::MissingEvidenceRef { step_id: id });
                    }
                    Some(r) if !ledger.evidence_ref_is_fresh(&r.evidence_ref) => {
                        return Err(ReadinessFailure::MissingEvidenceRef { step_id: id });
                    }
                    _ => {}
                }
            }
        }
        if let Some(contract) = contract {
            let mut unmet = Vec::new();
            for cmd in &contract.acceptance_commands {
                let fp = normalize_acceptance(cmd);
                let ok = ledger.verifications.iter().any(|v| {
                    v.exit_code == 0
                        && (v.command_fingerprint == fp
                            || v.command_fingerprint.contains(cmd.trim()))
                });
                if !ok {
                    unmet.push(cmd.clone());
                }
            }
            if !unmet.is_empty() {
                return Err(ReadinessFailure::AcceptanceCommandsUnmet { commands: unmet });
            }
        }
    }

    Ok(())
}

/// Convenience wrapper from ProcessEvidence (legacy simple path).
pub fn check_goal_complete(
    plan: &PlanState,
    config: GateConfig,
    evidence: Option<&ProcessEvidence>,
) -> Result<(), ReadinessFailure> {
    let mut ledger = EvidenceLedger {
        plan: plan.clone(),
        ..Default::default()
    };
    let mut impl_like = false;
    if let Some(ev) = evidence {
        impl_like = ev.task_looks_like_implementation;
        for i in 0..ev.mutation_count {
            ledger.record_mutation(format!("m{i}"), "apply_patch", vec![]);
        }
        if ev.verification_passed_after_mutation && ev.mutation_count > 0 {
            ledger.record_verify("v-legacy", "cargo\u{1f}test", 0);
        }
    }
    check(plan, &ledger, None, &config, false, impl_like)
}

fn normalize_acceptance(cmd: &str) -> String {
    let parts: Vec<String> = cmd.split_whitespace().map(|s| s.to_string()).collect();
    if parts.is_empty() {
        return String::new();
    }
    EvidenceLedger::normalize_command_fingerprint(&parts[0], &parts[1..])
}

/// How a request is classified for gate assembly (P3): whether it is
/// implementation work that merits the completion-evidence gate and answer
/// audit, or a conversational/command-style turn where those gates only add
/// redundant verification prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskClass {
    Conversational,
    Implementation,
}

/// Appendix C — task classification heuristic (v1). Read-only/question words
/// and command-style git-operation verbs (回退/清理/删除/还原 — the kind of
/// request that runs one command and is done) classify as
/// [`TaskClass::Conversational`]; modification verbs as
/// [`TaskClass::Implementation`]. Anything unrecognized defaults to
/// `Conversational`: a misclassification only skips an optional verification
/// prompt, never relaxes a gate on real implementation work that announces
/// itself with a modification verb.
pub fn classify_task(task: &str) -> TaskClass {
    let lower = task.to_lowercase();
    let conversational = [
        "解释", "explain", "what is", "在哪", "how does", "why is", "回退", "清理", "删除", "还原",
        "撤销", "revert",
    ];
    if conversational.iter().any(|w| lower.contains(w)) {
        return TaskClass::Conversational;
    }
    let implementation = [
        "fix",
        "修复",
        "实现",
        "implement",
        "修",
        "add",
        "添加",
        "改",
        "修改",
        "write",
        "编辑",
        "bug",
        "重构",
        "refactor",
        "优化",
    ];
    if implementation.iter().any(|w| lower.contains(w)) {
        TaskClass::Implementation
    } else {
        TaskClass::Conversational
    }
}

/// Appendix C — implementation-class request heuristic (v1).
pub fn task_looks_like_implementation(task: &str) -> bool {
    classify_task(task) == TaskClass::Implementation
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkProfile;
    use crate::plan::{PlanOrigin, PlanStep};

    #[test]
    fn blocks_incomplete_model_explicit() {
        let plan = PlanState {
            steps: vec![PlanStep {
                step: "a".into(),
                status: "pending".into(),
                id: None,
                origin: PlanOrigin::ModelExplicit,
            }],
        };
        assert!(check_goal_complete(&plan, GateConfig::default(), None).is_err());
    }

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
        assert!(check(&plan, &led, None, &cfg, false, false).is_err());
        assert!(check(&plan, &led, None, &cfg, false, false).is_err());
        assert!(check(&plan, &led, None, &cfg, true, false).is_ok());
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
        assert!(check(&plan, &led, None, &cfg, true, false).is_err());
    }

    #[test]
    fn delivery_blocks_implementation_without_mutation() {
        let cfg = GateConfig::for_work_profile(WorkProfile::Delivery);
        let ev = ProcessEvidence {
            mutation_count: 0,
            verification_passed_after_mutation: false,
            task_looks_like_implementation: true,
        };
        assert!(matches!(
            check_goal_complete(&PlanState::default(), cfg, Some(&ev)),
            Err(ReadinessFailure::UnprovenNoMutation)
        ));
    }

    #[test]
    fn delivery_blocks_stale_verify_after_mutation() {
        let cfg = GateConfig::for_work_profile(WorkProfile::Delivery);
        let mut led = EvidenceLedger::default();
        led.record_mutation("c1", "apply_patch", vec![]);
        led.record_verify("v1", "cargo\u{1f}test", 0);
        led.record_mutation("c2", "replace", vec![]);
        assert!(matches!(
            check(&PlanState::default(), &led, None, &cfg, false, true),
            Err(ReadinessFailure::MissingVerification { .. })
        ));
    }

    #[test]
    fn delivery_blocks_stale_verify_after_build_relevant_mutation() {
        // Regression: a build-relevant mutation without a fresh verify is
        // still refused, even when inert files were touched alongside it.
        let cfg = GateConfig::for_work_profile(WorkProfile::Delivery);
        let mut led = EvidenceLedger::default();
        led.record_mutation("c1", "apply_patch", vec!["src/lib.rs".into()]);
        led.record_verify("v1", "cargo\u{1f}test", 0);
        led.record_mutation(
            "c2",
            "replace",
            vec!["README.md".into(), "crates/x/src/main.rs".into()],
        );
        assert!(matches!(
            check(&PlanState::default(), &led, None, &cfg, false, true),
            Err(ReadinessFailure::MissingVerification { .. })
        ));
    }

    #[test]
    fn delivery_ignores_stale_verify_for_inert_changes() {
        // Docs-only mutations cannot affect the build: no fresh verify is
        // required, so the gate must not bounce update_goal(complete) back
        // with MissingVerification.
        let cfg = GateConfig::for_work_profile(WorkProfile::Delivery);
        let mut led = EvidenceLedger::default();
        led.record_mutation("c1", "apply_patch", vec!["README.md".into()]);
        led.record_verify("v1", "cargo\u{1f}test", 0);
        led.record_mutation(
            "c2",
            "replace",
            vec!["docs/guide.md".into(), "scripts/audit.sh".into()],
        );
        assert!(
            check(&PlanState::default(), &led, None, &cfg, false, true).is_ok(),
            "inert changes must not trigger MissingVerification"
        );
    }

    #[test]
    fn acceptance_commands_must_appear_in_ledger() {
        let cfg = GateConfig::for_work_profile(WorkProfile::Delivery);
        let mut led = EvidenceLedger::default();
        led.record_mutation("c1", "apply_patch", vec![]);
        led.record_verify("v1", "cargo\u{1f}test", 0);
        let contract = TaskContract {
            acceptance_commands: vec!["cargo clippy".into()],
            ..Default::default()
        };
        assert!(matches!(
            check(
                &PlanState::default(),
                &led,
                Some(&contract),
                &cfg,
                false,
                true
            ),
            Err(ReadinessFailure::AcceptanceCommandsUnmet { .. })
        ));
    }

    #[test]
    fn implementation_heuristic() {
        assert!(task_looks_like_implementation("fix the login bug"));
        assert!(!task_looks_like_implementation("explain how auth works"));
    }

    #[test]
    fn classify_task_grades_command_and_question_turns_conversational() {
        // The incident trigger: a git-operation command must not assemble the
        // evidence gate / answer audit.
        assert_eq!(
            classify_task("没有提交的都回退掉"),
            TaskClass::Conversational
        );
        assert_eq!(
            classify_task("这个函数是干什么的"),
            TaskClass::Conversational
        );
        for task in [
            "把工作区清理一下",
            "删除未跟踪的文件",
            "还原刚才的改动",
            "revert the last commit",
        ] {
            assert_eq!(
                classify_task(task),
                TaskClass::Conversational,
                "{task} must be conversational"
            );
        }
    }

    #[test]
    fn classify_task_grades_modification_requests_implementation() {
        assert_eq!(
            classify_task("修复 login 的 panic"),
            TaskClass::Implementation
        );
        for task in [
            "实现用户登录接口",
            "重构 session 模块",
            "优化这段代码的查询性能",
            "refactor the session module",
        ] {
            assert_eq!(
                classify_task(task),
                TaskClass::Implementation,
                "{task} must be implementation"
            );
        }
        // Command verbs win over modification words: the request is a git
        // operation, not code modification.
        assert_eq!(
            classify_task("把刚才的修改回退掉"),
            TaskClass::Conversational
        );
    }
}
