//! Deterministic execution-policy resolution — the ONE place that decides how
//! hard to drive a model for a given turn.
//!
//! Replaces the retired weak/medium/strong `ModelPolicy` tiers. Inputs are
//! model facts (`ModelProfile`), the executor's seat (`ExecutionRole`), the
//! turn's own limits (`TurnProfile`), and always-on safety rails. Resolution
//! is pure and deterministic: min-composition for concurrency, a precedence
//! chain for reasoning effort, and no runtime auto-tuning in v1.

use leveler_model::{ModelProfile, ReasoningEffort};

use crate::factory::TurnProfile;

/// Local read-only tool batch width for main/explorer seats. This is a *local
/// executor* resource guard over calls the model already emitted — the model's
/// wire-level parallel-tool-call capability does not cap it (see plan doc §3:
/// profile `max_parallel_tool_calls` is a conservative placeholder today, and
/// folding it in would silently drop 4 → 1).
const DEFAULT_PARALLEL_TOOLS: usize = 4;
/// Per-step distinct-modified-files budget (task budget, formerly a policy
/// tier field; value matches the retired `default_policy()` so migration has
/// zero behavior drift).
const DEFAULT_FILES_PER_STEP: usize = 8;

/// Which seat the executor occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionRole {
    /// Top-level turn (Goal/Chat/Node).
    Main,
    /// Delegated agent without a narrower explorer/worker specialization.
    Default,
    /// Read-only investigation sub-agent.
    Explorer,
    /// Writing sub-agent pinned to owned files.
    Worker,
}

/// eval-only injection seam for single-variable ablation. Production assembly
/// never constructs one; every `None` inherits the resolved default. Safety
/// rails (`completion_evidence`, `repeated_read_guard`) can ONLY be switched
/// off through here — that is deliberate: measuring a rail's value is an
/// experiment, not a configuration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutionOverrides {
    pub explicit_plan: Option<bool>,
    pub step_summary_every: Option<u32>,
    pub max_search_calls_per_step: Option<usize>,
    pub max_parallel_tools: Option<usize>,
    pub max_files_per_step: Option<usize>,
    pub completion_evidence: Option<bool>,
    pub repeated_read_guard: Option<bool>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_tool_output_bytes: Option<usize>,
}

/// The fully resolved execution configuration for one executor. For the
/// numeric budget fields `0` means unlimited, matching executor semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedExecutionPolicy {
    pub max_output_tokens: u32,
    pub context_budget: u32,
    pub max_parallel_tools: usize,
    pub max_search_calls_per_step: usize,
    pub max_files_per_step: usize,
    pub explicit_plan: bool,
    pub step_summary_every: u32,
    pub completion_evidence: bool,
    pub repeated_read_guard: bool,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Byte budget for a single tool result (the central output cap).
    pub max_tool_output_bytes: usize,
}

/// min over concurrency caps where `0` means "no opinion / unlimited".
fn min_nonzero(caps: &[usize]) -> usize {
    caps.iter().copied().filter(|&c| c > 0).min().unwrap_or(0)
}

/// The tool-context slice of resolution: per-step modified-files budget and
/// the repeated-read guard. Split out because the tool context is built once
/// per engine (before any turn exists), while the executor is resolved per
/// turn — both must read the SAME defaults or the seam drifts.
pub fn resolve_tool_limits(overrides: Option<&ExecutionOverrides>) -> (usize, bool) {
    let o = overrides.cloned().unwrap_or_default();
    (
        o.max_files_per_step.unwrap_or(DEFAULT_FILES_PER_STEP),
        o.repeated_read_guard.unwrap_or(true),
    )
}

/// Resolve the execution configuration for one executor seat. Pure function;
/// `overrides` is the eval-only ablation seam.
pub fn resolve_execution_policy(
    profile: &ModelProfile,
    role: ExecutionRole,
    turn: &TurnProfile,
    overrides: Option<&ExecutionOverrides>,
) -> ResolvedExecutionPolicy {
    // The turn's own StepLimits are enforced by the executor. Structured-plan
    // support stays enabled for every seat; the executor applies its task-based
    // complexity check so simple one-step work is not forced through a plan.
    let _ = turn;
    let o = overrides.cloned().unwrap_or_default();

    let role_parallel = match role {
        ExecutionRole::Main | ExecutionRole::Default | ExecutionRole::Explorer => {
            DEFAULT_PARALLEL_TOOLS
        }
        // Write path stays serial: parallel writes conflict and amplify errors.
        ExecutionRole::Worker => 1,
    };
    let max_parallel_tools = min_nonzero(&[role_parallel, o.max_parallel_tools.unwrap_or(0)]);

    ResolvedExecutionPolicy {
        max_output_tokens: profile.limits.max_output_tokens,
        context_budget: profile.limits.reliable_context,
        max_parallel_tools,
        max_search_calls_per_step: o.max_search_calls_per_step.unwrap_or(0),
        max_files_per_step: o.max_files_per_step.unwrap_or(DEFAULT_FILES_PER_STEP),
        // Planning is task-driven, not model-tier-driven. Enabling the gate here
        // lets the executor enforce it only when the actual request is complex.
        explicit_plan: o.explicit_plan.unwrap_or(true),
        step_summary_every: o.step_summary_every.unwrap_or(0),
        // Safety rails: only the eval seam may lower them.
        completion_evidence: o.completion_evidence.unwrap_or(true),
        repeated_read_guard: o.repeated_read_guard.unwrap_or(true),
        reasoning_effort: o.reasoning_effort.or(profile.reasoning.effort),
        // Explicit configuration only (no auto-tuning in v1): eval seam, then
        // the model profile, then the global default cap.
        max_tool_output_bytes: o
            .max_tool_output_bytes
            .or(profile.limits.max_tool_output_bytes)
            .unwrap_or(leveler_tools::registry::MAX_TOOL_OUTPUT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::TurnProfile;
    use leveler_agent::{ContinuationPolicy, StepLimits};
    use leveler_model::{ModelProfile, ReasoningEffort};

    fn profile() -> ModelProfile {
        serde_json::from_value(serde_json::json!({
            "id": "deepseek-v4-flash",
            "provider": "deepseek",
            "model_id": "deepseek-v4-flash",
            "protocol": "openai_chat",
            "capabilities": {
                "streaming": true, "tool_calling": true,
                "parallel_tool_calls": false, "structured_output": false,
                "reasoning": false, "vision": false
            },
            "limits": {
                "context_window": 131072, "reliable_context": 65536,
                "max_output_tokens": 8192, "max_tool_schema_bytes": 32768,
                "max_parallel_tool_calls": 1
            },
            "reasoning": { "style": "none" }
        }))
        .expect("valid test profile")
    }

    fn goal_turn() -> TurnProfile {
        TurnProfile::Goal {
            continuation: ContinuationPolicy::UntilTerminal,
            limits: StepLimits::default(),
        }
    }

    /// Migration contract: for a main seat with no overrides, resolution must
    /// equal what the retired `default_policy()` produced through the old
    /// `leveling_from_policy` — pinned here as literals so deleting the old
    /// path cannot silently shift behavior.
    #[test]
    fn main_seat_resolution_equals_the_retired_default_policy_leveling() {
        let p = profile();
        let new = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None);

        assert_eq!(new.max_output_tokens, 8192);
        assert_eq!(new.context_budget, 65536);
        assert_eq!(new.max_parallel_tools, 4);
        assert_eq!(new.max_search_calls_per_step, 0, "0 = unlimited");
        assert!(
            new.explicit_plan,
            "complex tasks must have a structured-plan gate"
        );
        assert_eq!(new.step_summary_every, 0);
        assert!(new.completion_evidence);
        assert_eq!(new.max_files_per_step, 8, "task budget, was policy field");
        assert!(new.repeated_read_guard, "safety rail is always on");
        assert_eq!(
            new.max_tool_output_bytes,
            48 * 1024,
            "no profile/override opinion → today's central cap, zero drift"
        );
    }

    #[test]
    fn tool_output_budget_prefers_override_then_profile_then_default() {
        let mut p = profile();
        p.limits.max_tool_output_bytes = Some(16 * 1024);
        let r = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None);
        assert_eq!(r.max_tool_output_bytes, 16 * 1024, "profile value wins");

        let o = ExecutionOverrides {
            max_tool_output_bytes: Some(8 * 1024),
            ..ExecutionOverrides::default()
        };
        let r = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&o));
        assert_eq!(r.max_tool_output_bytes, 8 * 1024, "eval seam wins over profile");
    }

    #[test]
    fn tool_limits_resolve_to_task_budget_and_always_on_guard() {
        assert_eq!(resolve_tool_limits(None), (8, true));
        let o = ExecutionOverrides {
            max_files_per_step: Some(2),
            repeated_read_guard: Some(false),
            ..ExecutionOverrides::default()
        };
        assert_eq!(resolve_tool_limits(Some(&o)), (2, false));
    }

    #[test]
    fn safety_rails_are_on_without_overrides_and_only_eval_can_lower_them() {
        let p = profile();
        let plain = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None);
        assert!(plain.completion_evidence);
        assert!(plain.repeated_read_guard);

        let ablated = ExecutionOverrides {
            completion_evidence: Some(false),
            repeated_read_guard: Some(false),
            ..ExecutionOverrides::default()
        };
        let r = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&ablated));
        assert!(!r.completion_evidence);
        assert!(!r.repeated_read_guard);
    }

    #[test]
    fn worker_seat_serializes_writes_and_explorer_keeps_wide_read_parallelism() {
        let p = profile();
        let worker = resolve_execution_policy(&p, ExecutionRole::Worker, &goal_turn(), None);
        assert_eq!(worker.max_parallel_tools, 1, "write path stays serial");

        let explorer = resolve_execution_policy(&p, ExecutionRole::Explorer, &goal_turn(), None);
        assert_eq!(
            explorer.max_parallel_tools, 4,
            "read-only investigation keeps the wide local batch"
        );
    }

    #[test]
    fn min_composition_ignores_zero_and_override_wins_when_tighter() {
        assert_eq!(min_nonzero(&[0, 4, 0]), 4);
        assert_eq!(min_nonzero(&[3, 4]), 3);
        assert_eq!(min_nonzero(&[0, 0]), 0, "all-unlimited stays unlimited");

        let p = profile();
        let tighter = ExecutionOverrides {
            max_parallel_tools: Some(2),
            ..ExecutionOverrides::default()
        };
        let r = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&tighter));
        assert_eq!(r.max_parallel_tools, 2);
    }

    #[test]
    fn reasoning_effort_prefers_override_then_profile_recommendation() {
        let mut p = profile();
        p.reasoning.effort = Some(ReasoningEffort::Low);
        let r = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None);
        assert_eq!(r.reasoning_effort, Some(ReasoningEffort::Low));

        let task = ExecutionOverrides {
            reasoning_effort: Some(ReasoningEffort::High),
            ..ExecutionOverrides::default()
        };
        let r = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&task));
        assert_eq!(r.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn explicit_plan_gate_defaults_on_but_can_be_overridden_per_task() {
        let p = profile();
        let plain = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None);
        assert!(
            plain.explicit_plan,
            "the executor decides task complexity; every model gets the gate"
        );
        assert_eq!(plain.step_summary_every, 0);

        let complex_task = ExecutionOverrides {
            explicit_plan: Some(false),
            step_summary_every: Some(6),
            ..ExecutionOverrides::default()
        };
        let r =
            resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&complex_task));
        assert!(!r.explicit_plan);
        assert_eq!(r.step_summary_every, 6);
    }
}
