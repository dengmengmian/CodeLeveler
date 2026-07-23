//! The single source of `Executor` construction (plan B2).
//!
//! Every path (direct, chat, orchestrated node) builds its executor here, so
//! there is exactly ONE derivation of the execution configuration — since the
//! model-tier retirement that derivation is `resolve_execution_policy`,
//! fed by model facts and the eval-only ablation seam instead of a bound
//! `ModelPolicy`.

use std::sync::Arc;

use leveler_agent::{
    ContinuationPolicy, Executor, StepLimits, SubAgentExecutionPolicies, SubAgentExecutionPolicy,
    WorkProfile,
};
use leveler_lifecycle::{TaskClass, classify_task};
use leveler_model::{ModelRef, ModelRuntime};
use leveler_tools::{ToolContext, ToolRegistry};

use crate::EngineError;
use crate::policy_resolver::{
    ExecutionOverrides, ExecutionRole, ResolvedExecutionPolicy, resolve_execution_policy,
};

/// What kind of turn the executor will drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnProfile {
    /// Top-level user goal: goal mode on, caller-selected continuation/limits.
    Goal {
        continuation: ContinuationPolicy,
        limits: StepLimits,
    },
    /// Conversational turn: same execution controls, goal mode off.
    Chat {
        continuation: ContinuationPolicy,
        limits: StepLimits,
    },
    /// One orchestrated node: caller-selected continuation/limits/paths, goal mode off.
    Node {
        continuation: ContinuationPolicy,
        limits: StepLimits,
        write_allowlist: Option<Vec<String>>,
    },
}

/// The policy/profile-derived executor shape, factored out as a pure function
/// The hard resource limits for a turn profile. Top-level turns default to no
/// resource ceiling; callers may layer explicit token/cost/duration budgets on
/// them. Node constraints are explicit and independent from turn continuation.
pub fn profile_step_limits(profile: &TurnProfile) -> StepLimits {
    match profile {
        TurnProfile::Goal { limits, .. } | TurnProfile::Chat { limits, .. } => *limits,
        TurnProfile::Node { limits, .. } => *limits,
    }
}

/// P3 task-class grading of the two closeout gates: completion evidence and
/// the answer audit.
///
/// - `Conversational` turns get neither gate — on a command/question turn they
///   only produce redundant verification prompts.
/// - `Implementation` (or unclassifiable) turns keep the resolved policy and
///   the Delivery work-profile audit unchanged.
/// - An **explicit** eval-only override (`ExecutionOverrides::completion_evidence`)
///   still wins over grading: safety rails are lowered only deliberately, and
///   an ablation that pins the rail must not be silently re-graded. Grading
///   applies only when the override leaves the rail unset.
fn grade_gates(
    task_class: Option<TaskClass>,
    overrides: Option<&ExecutionOverrides>,
    work_profile: WorkProfile,
    resolved: &ResolvedExecutionPolicy,
) -> (bool, bool) {
    let conversational = matches!(task_class, Some(TaskClass::Conversational));
    let evidence_override = overrides.and_then(|o| o.completion_evidence);
    let completion_evidence = if conversational && evidence_override.is_none() {
        false
    } else {
        resolved.completion_evidence
    };
    let answer_audit = matches!(work_profile, WorkProfile::Delivery) && !conversational;
    (completion_evidence, answer_audit)
}

/// Builds executors for engine turns. Owns the shared runtime/registry/tool
/// context/model; profile read failures are hard errors (no silently ungated
/// executors). `overrides` is the eval-only ablation seam — production
/// assembly leaves it `None`.
pub struct ExecutorFactory {
    pub runtime: Arc<dyn ModelRuntime>,
    pub registry: Arc<ToolRegistry>,
    pub tool_context: ToolContext,
    pub model: ModelRef,
    pub commit_co_author: bool,
    pub overrides: Option<ExecutionOverrides>,
    /// Product work profile (economy/balanced/delivery).
    pub work_profile: WorkProfile,
    /// Short memory INDEX for system injection (titles only).
    pub memory_index: String,
    /// SEC-1 permission rules (may be empty).
    pub permission_rules: leveler_execution::PermissionRuleSet,
    /// Project permission-rules file; `ApproveAlways` persists new rules here.
    pub permission_rules_path: Option<std::path::PathBuf>,
    /// SEC-8 tool hooks (may be empty).
    pub hook_runner: leveler_execution::HookRunner,
    /// SEC-2 durable grants directory under project state.
    pub grants_state_dir: Option<std::path::PathBuf>,
}

impl ExecutorFactory {
    /// `task` is the turn's raw request text when the caller has it (fresh
    /// Goal/Content turns; `None` for resumes). It feeds P3 task-class
    /// grading: conversational turns skip the completion-evidence gate and the
    /// answer audit.
    pub async fn build(
        &self,
        profile: TurnProfile,
        task: Option<&str>,
    ) -> Result<Executor, EngineError> {
        let model_profile = self
            .runtime
            .profile(&self.model)
            .await
            .map_err(|e| EngineError::Config(format!("cannot read model profile: {e}")))?;
        let resolved = resolve_execution_policy(
            &model_profile,
            ExecutionRole::Main,
            &profile,
            self.overrides.as_ref(),
        );
        let task_class = task.map(classify_task);
        let (completion_evidence, answer_audit) = grade_gates(
            task_class,
            self.overrides.as_ref(),
            self.work_profile,
            &resolved,
        );
        // P3 留痕: the classification and the gates it produced, for offline
        // accuracy analysis of the keyword classifier.
        tracing::info!(
            task_class = ?task_class,
            completion_evidence,
            answer_audit,
            "task classification graded gate assembly"
        );
        let child_policy = |role| {
            let policy =
                resolve_execution_policy(&model_profile, role, &profile, self.overrides.as_ref());
            SubAgentExecutionPolicy {
                step_summary_every: policy.step_summary_every,
                max_search_calls_per_step: policy.max_search_calls_per_step,
                max_parallel_tools: policy.max_parallel_tools,
                require_explicit_plan: policy.explicit_plan,
                require_completion_evidence: policy.completion_evidence,
                reasoning_effort: policy.reasoning_effort,
            }
        };
        let child_policies = SubAgentExecutionPolicies {
            default: child_policy(ExecutionRole::Default),
            explorer: child_policy(ExecutionRole::Explorer),
            worker: child_policy(ExecutionRole::Worker),
        };

        let continuation = match &profile {
            TurnProfile::Goal { continuation, .. } | TurnProfile::Chat { continuation, .. } => {
                *continuation
            }
            TurnProfile::Node { continuation, .. } => *continuation,
        };
        // Per-model tool-result budget rides the turn's tool context.
        let mut tool_context = self.tool_context.clone();
        tool_context.tool_output_budget = resolved.max_tool_output_bytes;
        let mut executor = Executor::new(
            self.runtime.clone(),
            self.registry.clone(),
            tool_context,
            self.model.clone(),
            0,
        )
        .with_continuation_policy(continuation)
        .with_max_output_tokens(resolved.max_output_tokens)
        .with_pricing(model_profile.pricing)
        .with_context_budget(resolved.context_budget)
        .with_reasoning_effort(resolved.reasoning_effort)
        // A model profile may ship its own system prompt; None keeps the default.
        .with_base_instructions(model_profile.instructions.clone())
        .with_permission_rules(self.permission_rules.clone())
        .with_permission_rules_path(self.permission_rules_path.clone())
        .with_hook_runner(self.hook_runner.clone())
        .with_grants_state_dir_opt(self.grants_state_dir.clone())
        .with_commit_co_author(self.commit_co_author)
        .with_execution_controls(
            resolved.step_summary_every,
            resolved.max_search_calls_per_step,
            resolved.max_parallel_tools,
        )
        .with_structure(resolved.explicit_plan, completion_evidence)
        .with_sub_agent_policies(child_policies)
        // Every profile carries only the limits explicitly selected by its caller.
        .with_step_limits(profile_step_limits(&profile));

        executor = executor
            .with_work_profile(self.work_profile)
            .with_memory_index(self.memory_index.clone());

        // K29: answer_audit off by default; Delivery may enable as optional
        // tax — but not for conversational turns (graded in `grade_gates`).
        executor = match profile {
            TurnProfile::Goal { .. } => executor
                .with_goal_mode(true)
                .with_answer_audit(answer_audit),
            TurnProfile::Chat { .. } => executor
                .with_goal_mode(false)
                .with_answer_audit(answer_audit),
            TurnProfile::Node {
                write_allowlist, ..
            } => executor
                .with_goal_mode(false)
                .with_write_allowlist(write_allowlist),
        };
        Ok(executor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_profiles_have_no_default_resource_ceiling() {
        for profile in [
            TurnProfile::Goal {
                continuation: ContinuationPolicy::UntilTerminal,
                limits: StepLimits::default(),
            },
            TurnProfile::Chat {
                continuation: ContinuationPolicy::UntilTerminal,
                limits: StepLimits::default(),
            },
        ] {
            let limits = profile_step_limits(&profile);
            assert_eq!(
                limits.max_duration, None,
                "{profile:?} must run until terminal"
            );
            assert_eq!(
                limits.max_commands, None,
                "interactive runs do not cap commands"
            );
            assert_eq!(
                limits.max_modified_files, None,
                "interactive runs do not cap modified files"
            );
        }
    }

    #[test]
    fn node_profile_keeps_explicit_safety_constraints() {
        let node = TurnProfile::Node {
            continuation: ContinuationPolicy::UntilTerminal,
            limits: StepLimits {
                max_commands: Some(10),
                max_modified_files: Some(8),
                max_duration: Some(std::time::Duration::from_secs(900)),
                ..StepLimits::default()
            },
            write_allowlist: None,
        };
        let limits = profile_step_limits(&node);
        assert_eq!(limits.max_commands, Some(10));
        assert_eq!(limits.max_modified_files, Some(8));
        assert_eq!(
            limits.max_duration,
            Some(std::time::Duration::from_secs(900))
        );
    }

    fn resolved_policy(completion_evidence: bool) -> ResolvedExecutionPolicy {
        ResolvedExecutionPolicy {
            max_output_tokens: 8192,
            context_budget: 65536,
            max_parallel_tools: 4,
            max_search_calls_per_step: 0,
            max_files_per_step: 8,
            explicit_plan: true,
            step_summary_every: 0,
            completion_evidence,
            repeated_read_guard: true,
            reasoning_effort: None,
            max_tool_output_bytes: 48 * 1024,
        }
    }

    #[test]
    fn conversational_turn_disables_evidence_gate_and_answer_audit() {
        // The incident shape: a command-style task under the Delivery profile
        // must not assemble either closeout gate.
        let (evidence, audit) = grade_gates(
            Some(TaskClass::Conversational),
            None,
            WorkProfile::Delivery,
            &resolved_policy(true),
        );
        assert!(!evidence);
        assert!(!audit);
    }

    #[test]
    fn implementation_turn_keeps_resolved_gates() {
        let (evidence, audit) = grade_gates(
            Some(TaskClass::Implementation),
            None,
            WorkProfile::Delivery,
            &resolved_policy(true),
        );
        assert!(evidence);
        assert!(audit);

        // Unknown task text (resume) keeps the pre-P3 behavior too.
        let (evidence, audit) =
            grade_gates(None, None, WorkProfile::Delivery, &resolved_policy(true));
        assert!(evidence);
        assert!(audit);

        // Non-Delivery profiles never had the audit.
        let (_, audit) = grade_gates(
            Some(TaskClass::Implementation),
            None,
            WorkProfile::Balanced,
            &resolved_policy(true),
        );
        assert!(!audit);
    }

    #[test]
    fn explicit_override_wins_over_conversational_grading() {
        // Eval seam pins the rail on: grading must not switch it back off.
        let overrides = ExecutionOverrides {
            completion_evidence: Some(true),
            ..ExecutionOverrides::default()
        };
        let (evidence, audit) = grade_gates(
            Some(TaskClass::Conversational),
            Some(&overrides),
            WorkProfile::Delivery,
            &resolved_policy(true),
        );
        assert!(evidence, "explicit override beats task-class grading");
        assert!(!audit, "answer audit has no override seam");
    }
}
