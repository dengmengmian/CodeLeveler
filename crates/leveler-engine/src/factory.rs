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
use leveler_model::{ModelRef, ModelRuntime};
use leveler_tools::{ToolContext, ToolRegistry};

use crate::EngineError;
use crate::policy_resolver::{ExecutionOverrides, ExecutionRole, resolve_execution_policy};

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
    pub async fn build(&self, profile: TurnProfile) -> Result<Executor, EngineError> {
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
        let mut executor = Executor::new(
            self.runtime.clone(),
            self.registry.clone(),
            self.tool_context.clone(),
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
        .with_structure(resolved.explicit_plan, resolved.completion_evidence)
        .with_sub_agent_policies(child_policies)
        // Every profile carries only the limits explicitly selected by its caller.
        .with_step_limits(profile_step_limits(&profile));

        executor = executor
            .with_work_profile(self.work_profile)
            .with_memory_index(self.memory_index.clone());

        // K29: answer_audit off by default; Delivery may enable as optional tax.
        let delivery_audit = matches!(self.work_profile, WorkProfile::Delivery);
        executor = match profile {
            TurnProfile::Goal { .. } => executor
                .with_goal_mode(true)
                .with_answer_audit(delivery_audit),
            TurnProfile::Chat { .. } => executor
                .with_goal_mode(false)
                .with_answer_audit(delivery_audit),
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
}
