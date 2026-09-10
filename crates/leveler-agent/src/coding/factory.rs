//! The single source of `Executor` construction (plan B2).
//!
//! Every path (direct goal, chat, node) builds its executor here, so there is
//! exactly ONE derivation of the execution configuration — since the model-tier
//! retirement that derivation is `resolve_execution_policy`, fed by model facts
//! and the eval-only ablation seam instead of a bound `ModelPolicy`.

use std::sync::Arc;

use crate::{
    ContinuationPolicy, Executor, StepLimits, SubAgentExecutionPolicies, SubAgentExecutionPolicy,
};
use leveler_model::{ModelRef, ModelRuntime};
use leveler_tools::{ToolContext, ToolRegistry};

use crate::coding::policy::{ExecutionOverrides, ExecutionRole, resolve_execution_policy};
use leveler_engine::EngineError;

/// What kind of turn the executor will drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnProfile {
    /// Top-level user goal: goal mode on, caller-selected continuation/limits.
    Goal {
        continuation: ContinuationPolicy,
        limits: StepLimits,
        /// This turn continues the goal already active in the session — the
        /// runtime issued it after a refused close — rather than opening a
        /// new one. Nothing else distinguishes the two: both run as
        /// `TurnKind::User` with `TurnInput::Content`, and the prior
        /// `closing` flag reads the same either way. Without an explicit
        /// identity the seeder took "a close was attempted" for "the epoch
        /// is finished" and dropped every mutation and verification.
        continues_active_goal: bool,
    },
    /// Conversational turn: same execution controls, goal mode off.
    Chat {
        continuation: ContinuationPolicy,
        limits: StepLimits,
    },
    /// Scoped worker turn: caller-selected continuation/limits/paths, goal mode off.
    Node {
        continuation: ContinuationPolicy,
        limits: StepLimits,
        write_allowlist: Option<Vec<String>>,
    },
}

/// Host-handled `update_goal(complete|blocked)` is required only for Goal turns.
pub fn profile_enables_goal_mode(profile: &TurnProfile) -> bool {
    matches!(profile, TurnProfile::Goal { .. })
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
    /// Short memory INDEX for system injection (titles only).
    pub memory_index: String,
    /// Durable project memory root. The RUNTIME reads it for per-turn recall
    /// injection, for parking an unapproved `remember`, and — at terminal
    /// settlement — nothing else; the memory TOOLS get their own handle at
    /// construction. `None` = memory unconfigured.
    pub memory_root: Option<std::path::PathBuf>,
    /// The process-lived background task registry, so terminal settlement can
    /// reap this session's detached processes. Held here rather than fished
    /// out of the tool context: reaping is the engine's job, not a tool's.
    pub background_tasks: Arc<leveler_execution::BackgroundTaskRegistry>,
    /// SEC-1 permission rules (may be empty).
    pub permission_rules: leveler_execution::PermissionRuleSet,
    /// Project permission-rules file; `ApproveAlways` persists new rules here.
    pub permission_rules_path: Option<std::path::PathBuf>,
    /// SEC-8 tool hooks (may be empty).
    pub hook_runner: leveler_execution::HookRunner,
    /// Mid-turn user input for the main turn, when the host supplies any.
    /// Sub-agents deliberately do not inherit it (see `Executor::child_for_role_on`).
    pub steering: Option<Arc<dyn crate::SteeringSource>>,
    /// When false, top-level executors do not advertise `spawn_agent`.
    pub allow_delegation: bool,
    /// Whether the harness launches an independent reviewer at closure.
    /// Default `Off`; only explicit configuration turns it on.
    pub independent_review: crate::coding::policy::IndependentReviewPolicy,
}

impl ExecutorFactory {
    /// `task` is the turn's raw request text when the caller has it (fresh
    /// Goal/Content turns; `None` for resumes). It is not classified: the
    /// runtime no longer grades a request as implementation-shaped or not.
    pub async fn build(
        &self,
        profile: TurnProfile,
        _task: Option<&str>,
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
        let child_policy = |role| {
            let policy =
                resolve_execution_policy(&model_profile, role, &profile, self.overrides.as_ref());
            SubAgentExecutionPolicy {
                max_parallel_tools: policy.max_parallel_tools,
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
        tool_context.policy.tool_output_budget = resolved.max_tool_output_bytes;
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
        .with_context_trace(resolved.context_trace)
        // A model profile may ship its own system prompt; None keeps the default.
        .with_base_instructions(model_profile.instructions.clone())
        .with_permission_rules(self.permission_rules.clone())
        .with_permission_rules_path(self.permission_rules_path.clone())
        .with_hook_runner(self.hook_runner.clone())
        .with_steering_opt(self.steering.clone())
        .with_commit_co_author(self.commit_co_author)
        .with_execution_controls(resolved.max_parallel_tools)
        .with_sub_agent_policies(child_policies)
        .with_delegation(self.allow_delegation)
        // Every profile carries only the limits explicitly selected by its caller.
        .with_step_limits(profile_step_limits(&profile));

        executor = executor
            .with_memory_index(self.memory_index.clone())
            .with_memory_root(self.memory_root.clone());

        executor = if profile_enables_goal_mode(&profile) {
            executor.with_goal_mode(true)
        } else {
            match profile {
                TurnProfile::Node {
                    write_allowlist, ..
                } => executor.with_write_allowlist(write_allowlist),
                TurnProfile::Goal { .. } | TurnProfile::Chat { .. } => {
                    executor.with_goal_mode(false)
                }
            }
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
                continues_active_goal: false,
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
    fn only_goal_profile_enables_goal_mode() {
        let goal = TurnProfile::Goal {
            continuation: ContinuationPolicy::UntilTerminal,
            limits: StepLimits::default(),
            continues_active_goal: false,
        };
        let chat = TurnProfile::Chat {
            continuation: ContinuationPolicy::UntilTerminal,
            limits: StepLimits::default(),
        };
        let node = TurnProfile::Node {
            continuation: ContinuationPolicy::UntilTerminal,
            limits: StepLimits::default(),
            write_allowlist: None,
        };
        assert!(profile_enables_goal_mode(&goal));
        assert!(!profile_enables_goal_mode(&chat));
        assert!(!profile_enables_goal_mode(&node));
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
