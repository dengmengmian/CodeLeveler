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
    /// Independent read-only reviewer of work the main agent already did.
    ///
    /// R007b N7: `REQUIRED_REVIEWER` existed only as a supervisor label — the
    /// harness had never heard of it, so R008 and R009 were both designated
    /// and both ignored it. A reviewer has to be a seat the product knows
    /// about before any of it can be measured.
    Reviewer,
}

/// Whether the harness launches an independent reviewer at closure.
///
/// Explicit only: the user, the eval, or the caller says so. The runtime
/// never infers from file names or diff size that a change "needs" a second
/// model — that is a judgement about the work, not a mechanical fact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IndependentReviewPolicy {
    #[default]
    Off,
    /// Launch a read-only reviewer child over every product mutation.
    Required,
}

/// eval-only injection seam for single-variable ablation. Production assembly
/// never constructs one; every `None` inherits the resolved default. The
/// loop-guard rail (`repeated_read_guard`) can ONLY be switched off through
/// here — that is deliberate: measuring a rail's value is an experiment, not
/// a configuration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutionOverrides {
    pub explicit_plan: Option<bool>,
    pub max_search_calls_per_step: Option<usize>,
    pub max_parallel_tools: Option<usize>,
    pub max_files_per_step: Option<usize>,
    pub repeated_read_guard: Option<bool>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_tool_output_bytes: Option<usize>,
    /// C5-S3 ablation knob: switch the candidate adaptive-context behavior on.
    /// Production default stays Disabled until the eval verdict flips it.
    pub adaptive_context: Option<bool>,
    /// Candidate knob: keep a streamed round's reasoning on its assistant
    /// message, so a pass-back provider is given the chain instead of an
    /// empty string. Unmeasured in both directions; default off.
    pub keep_reasoning: Option<bool>,
    /// Candidate knob: trim oversized old tool results before folding.
    /// Production default stays off until an A/B demonstrates the value.
    pub prune_tool_results: Option<bool>,
    /// Measurement knob: persist the model context after every round
    /// (`ContextSnapshot`), not only when it diverges from the transcript.
    /// Context-cost attribution reads those rows; production never sets it.
    pub context_trace: Option<bool>,
}

/// How this runtime USES a model's context capability (C5-S1). The capability
/// itself — window size, output ceiling — lives in `ModelLimits` and describes
/// facts; this describes policy. S1 is a structural migration: every value
/// here reproduces the pre-S1 behavior exactly, and the single-variant enums
/// name today's behavior so later stages can add alternatives behind the same
/// seam instead of new call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPolicy {
    /// Fold threshold at task start (estimated tokens). Governs when held
    /// context is compacted — never how much may be read. `0` disables.
    pub initial_budget: u32,
    /// Ceiling this task may expand the fold threshold to. S1: always equal
    /// to `initial_budget`; adaptive expansion is C5-S3.
    pub max_budget: u32,
    pub compaction: CompactionPolicy,
    pub retention: RetentionPolicy,
    /// C5-S3: whether the fold threshold may climb during the task.
    pub expansion: ExpansionPolicy,
}

/// How folding happens. One variant today: anchored spans folded into a
/// handoff briefing, merged incrementally on refold (`compaction.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionPolicy {
    AnchoredBriefing,
}

/// What survives a fold. One variant today: the briefing carries decisions,
/// failed attempts, paths and constraints; file content is summarized rather
/// than pointed at. Fingerprinted read pointers are C5-S4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionPolicy {
    BriefingOnly,
}

/// Whether the fold threshold may climb during a task (C5-S3).
///
/// `Disabled` reproduces the S2 static behavior exactly. `Adaptive` lets the
/// executor raise the threshold one tier at a time in response to
/// authoritative runtime evidence (re-read pressure after a fold, a repair
/// turn) — expansion preserves the request prefix, folding rewrites it, so an
/// eligible expansion is always preferred over a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpansionPolicy {
    Disabled,
    Adaptive,
}

/// The deterministic budget ladder for a model: strictly increasing tiers,
/// clamped to the model's declared quality bound. Production never climbs
/// past `reliable_context` — the window headroom above it (max_output_tokens,
/// tool schema, framing) has no verified safe-input ceiling yet, so crossing
/// the quality boundary is reachable only through the eval override seam.
/// A small-window model simply gets a one-tier ladder.
pub fn expansion_tiers(profile: &ModelProfile) -> Vec<u32> {
    let ceiling = profile.limits.reliable_context;
    let mut tiers: Vec<u32> = [256 * 1024, 512 * 1024, ceiling]
        .into_iter()
        .filter(|&t| t > 0)
        .map(|t| t.min(ceiling))
        .collect();
    tiers.sort_unstable();
    tiers.dedup();
    tiers
}

impl ContextPolicy {
    /// The task-execution policy for a model: fold at the profile's declared
    /// reliable context, exactly as before S1. `reliable_context` is a model
    /// QUALITY declaration (recall degrades past it), not a hard cap — the
    /// policy chooses to fold there; it does not refuse to exceed it.
    pub fn for_profile(profile: &ModelProfile) -> Self {
        Self {
            initial_budget: profile.limits.reliable_context,
            max_budget: profile.limits.reliable_context,
            compaction: CompactionPolicy::AnchoredBriefing,
            retention: RetentionPolicy::BriefingOnly,
            expansion: ExpansionPolicy::Disabled,
        }
    }

    /// The interactive-chat policy: the conservative pre-request threshold the
    /// chat path has always used. Same value as
    /// `leveler_agent::PRE_REQUEST_COMPACT_THRESHOLD`, now resolved through
    /// the same seam as the task policy so the two units cannot drift apart
    /// unnoticed (C2.1 recorded them diverging: 24k vs the task budget).
    pub fn chat_default() -> Self {
        Self {
            initial_budget: leveler_agent::PRE_REQUEST_COMPACT_THRESHOLD as u32,
            max_budget: leveler_agent::PRE_REQUEST_COMPACT_THRESHOLD as u32,
            compaction: CompactionPolicy::AnchoredBriefing,
            retention: RetentionPolicy::BriefingOnly,
            expansion: ExpansionPolicy::Disabled,
        }
    }
}

/// The fully resolved execution configuration for one executor. For the
/// numeric budget fields `0` means unlimited, matching executor semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedExecutionPolicy {
    pub max_output_tokens: u32,
    /// Compatibility mirror of `context.initial_budget` (C5-S1 migration):
    /// existing consumers keep reading this field; it is always populated
    /// from `context`. New code should read `context` directly.
    pub context_budget: u32,
    /// How this executor uses the model's context capability.
    pub context: ContextPolicy,
    pub max_parallel_tools: usize,
    pub max_search_calls_per_step: usize,
    pub max_files_per_step: usize,
    pub explicit_plan: bool,
    pub repeated_read_guard: bool,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Byte budget for a single tool result (the central output cap).
    pub max_tool_output_bytes: usize,
    /// Keep a round's reasoning in history (candidate; default off).
    pub keep_reasoning: bool,
    /// Trim oversized old tool results before folding (candidate; default off).
    pub prune_tool_results: bool,
    /// Persist the model context every round (eval measurement seam).
    pub context_trace: bool,
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
        // A reviewer reads the same way an explorer does — it just reads work
        // that already exists rather than code it is about to change.
        ExecutionRole::Main
        | ExecutionRole::Default
        | ExecutionRole::Explorer
        | ExecutionRole::Reviewer => DEFAULT_PARALLEL_TOOLS,
        // Write path stays serial: parallel writes conflict and amplify errors.
        ExecutionRole::Worker => 1,
    };
    let max_parallel_tools = min_nonzero(&[role_parallel, o.max_parallel_tools.unwrap_or(0)]);

    let mut context = ContextPolicy::for_profile(profile);
    if o.adaptive_context.unwrap_or(false) {
        context.expansion = ExpansionPolicy::Adaptive;
        // Candidate shape under ablation: start at the smallest tier and let
        // evidence climb the ladder. Disabled keeps initial == reliable.
        context.initial_budget = expansion_tiers(profile)[0];
    }
    ResolvedExecutionPolicy {
        max_output_tokens: profile.limits.max_output_tokens,
        context_budget: context.initial_budget,
        context,
        max_parallel_tools,
        max_search_calls_per_step: o.max_search_calls_per_step.unwrap_or(0),
        max_files_per_step: o.max_files_per_step.unwrap_or(DEFAULT_FILES_PER_STEP),
        // Planning is task-driven, not model-tier-driven. This only shapes the
        // prompt's planning guidance and the one soft plan reminder; nothing
        // is refused for a missing plan.
        explicit_plan: o.explicit_plan.unwrap_or(true),
        // Safety rail: only the eval seam may lower it.
        repeated_read_guard: o.repeated_read_guard.unwrap_or(true),
        reasoning_effort: leveler_model::resolve_reasoning_effort(
            o.reasoning_effort,
            &profile.reasoning,
        )
        .effective,
        // Explicit configuration only (no auto-tuning in v1): eval seam, then
        // the model profile, then the global default cap.
        max_tool_output_bytes: o
            .max_tool_output_bytes
            .or(profile.limits.max_tool_output_bytes)
            .unwrap_or(leveler_tools::registry::MAX_TOOL_OUTPUT)
            .clamp(
                leveler_tools::registry::MIN_TOOL_OUTPUT,
                leveler_tools::registry::MAX_TOOL_OUTPUT,
            ),
        keep_reasoning: o.keep_reasoning.unwrap_or(false),
        prune_tool_results: o.prune_tool_results.unwrap_or(false),
        context_trace: o.context_trace.unwrap_or(false),
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
            continues_active_goal: false,
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
            "complex tasks get structured-plan guidance"
        );
        assert_eq!(new.max_files_per_step, 8, "task budget, was policy field");
        assert!(new.repeated_read_guard, "safety rail is always on");
        assert_eq!(
            new.max_tool_output_bytes,
            48 * 1024,
            "no profile/override opinion → today's central cap, zero drift"
        );
    }

    #[test]
    fn context_policy_migration_is_behavior_identical() {
        // C5-S1 is a structural migration: the new ContextPolicy must resolve
        // to exactly the values the old flat field carried, and the compat
        // mirror must never drift from it.
        let p = profile();
        let r = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None);
        assert_eq!(r.context.initial_budget, p.limits.reliable_context);
        assert_eq!(
            r.context_budget, r.context.initial_budget,
            "the compat mirror must equal the policy value"
        );
        assert_eq!(
            r.context.max_budget, r.context.initial_budget,
            "S1 has no expansion: max == initial until C5-S3"
        );
        assert_eq!(r.context.compaction, CompactionPolicy::AnchoredBriefing);
        assert_eq!(r.context.retention, RetentionPolicy::BriefingOnly);
    }

    #[test]
    fn chat_policy_pins_the_historical_pre_request_threshold() {
        // The chat path folded at 24k before S1; routing it through the same
        // seam must not move the number. If someone changes the threshold,
        // this failure forces them to say so.
        let chat = ContextPolicy::chat_default();
        assert_eq!(
            u64::from(chat.initial_budget),
            leveler_agent::PRE_REQUEST_COMPACT_THRESHOLD
        );
        assert_eq!(chat.initial_budget, 24_000);
        assert_eq!(chat.max_budget, chat.initial_budget);
    }

    #[test]
    fn model_limits_carry_no_runtime_policy() {
        // Capability purity: a profile deserialized without any policy-ish
        // key still yields a full ContextPolicy from the resolver — policy is
        // derived, never stored on the model. And context_quality is absent
        // unless measured.
        let p = profile();
        assert!(p.context_quality.is_none(), "unmeasured must stay None");
        let r = resolve_execution_policy(&p, ExecutionRole::Worker, &goal_turn(), None);
        assert_eq!(r.context.initial_budget, p.limits.reliable_context);
    }

    #[test]
    fn expansion_tiers_clamp_and_dedup_for_small_models() {
        // A model smaller than the ladder's steps degenerates to one tier;
        // no tier ever exceeds the quality bound.
        let mut small = profile();
        small.limits.reliable_context = 128 * 1024;
        assert_eq!(expansion_tiers(&small), vec![128 * 1024]);
        let mut mid = profile();
        mid.limits.reliable_context = 512 * 1024;
        assert_eq!(expansion_tiers(&mid), vec![256 * 1024, 512 * 1024]);
        let mut large = profile();
        large.limits.reliable_context = 786_432;
        assert_eq!(
            expansion_tiers(&large),
            vec![256 * 1024, 512 * 1024, 786_432]
        );
    }

    #[test]
    fn adaptive_override_starts_small_and_default_stays_static() {
        let p = profile();
        let plain = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None);
        assert_eq!(plain.context.expansion, ExpansionPolicy::Disabled);
        assert_eq!(plain.context.initial_budget, p.limits.reliable_context);

        let overrides = ExecutionOverrides {
            adaptive_context: Some(true),
            ..Default::default()
        };
        let adaptive =
            resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&overrides));
        assert_eq!(adaptive.context.expansion, ExpansionPolicy::Adaptive);
        assert_eq!(
            adaptive.context.initial_budget,
            expansion_tiers(&p)[0],
            "the candidate starts at the smallest tier"
        );
        // The compatibility mirror follows INITIAL, never a live budget.
        assert_eq!(adaptive.context_budget, adaptive.context.initial_budget);
    }

    #[test]
    fn keeping_reasoning_is_off_unless_the_eval_seam_asks_for_it() {
        let p = profile();
        assert!(
            !resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None).keep_reasoning,
            "an unmeasured candidate never ships on by default"
        );
        let o = ExecutionOverrides {
            keep_reasoning: Some(true),
            ..ExecutionOverrides::default()
        };
        assert!(
            resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&o))
                .keep_reasoning
        );
    }

    #[test]
    fn tool_result_pruning_is_off_unless_the_eval_seam_asks_for_it() {
        let p = profile();
        assert!(
            !resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None)
                .prune_tool_results,
            "an unmeasured candidate never ships on by default"
        );
        let o = ExecutionOverrides {
            prune_tool_results: Some(true),
            ..ExecutionOverrides::default()
        };
        assert!(
            resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&o))
                .prune_tool_results
        );
    }

    #[test]
    fn context_trace_is_off_unless_the_eval_seam_asks_for_it() {
        let p = profile();
        assert!(
            !resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None).context_trace,
            "production never persists the context every round"
        );
        let o = ExecutionOverrides {
            context_trace: Some(true),
            ..ExecutionOverrides::default()
        };
        assert!(
            resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&o)).context_trace
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
        assert_eq!(
            r.max_tool_output_bytes,
            8 * 1024,
            "eval seam wins over profile"
        );
    }

    #[test]
    fn tool_output_budget_is_clamped_to_safe_global_bounds() {
        let p = profile();
        let huge = ExecutionOverrides {
            max_tool_output_bytes: Some(leveler_tools::registry::MAX_TOOL_OUTPUT * 10),
            ..ExecutionOverrides::default()
        };
        assert_eq!(
            resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&huge))
                .max_tool_output_bytes,
            leveler_tools::registry::MAX_TOOL_OUTPUT
        );

        let zero = ExecutionOverrides {
            max_tool_output_bytes: Some(0),
            ..ExecutionOverrides::default()
        };
        assert_eq!(
            resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&zero))
                .max_tool_output_bytes,
            leveler_tools::registry::MIN_TOOL_OUTPUT
        );
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
        assert!(plain.repeated_read_guard);

        let ablated = ExecutionOverrides {
            repeated_read_guard: Some(false),
            ..ExecutionOverrides::default()
        };
        let r = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&ablated));
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
        p.reasoning.default_effort = Some(ReasoningEffort::Low);
        p.reasoning.supported_efforts = vec![ReasoningEffort::Low, ReasoningEffort::High];
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
    fn reasoning_effort_upgrades_unsupported_override() {
        let mut p = profile();
        p.capabilities.reasoning = true;
        p.reasoning.default_effort = Some(ReasoningEffort::Max);
        p.reasoning.supported_efforts = vec![ReasoningEffort::High, ReasoningEffort::Max];
        let task = ExecutionOverrides {
            reasoning_effort: Some(ReasoningEffort::Medium),
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

        // Overrides still reach the resolved policy — checked on a knob that
        // is actually consulted at runtime.
        let complex_task = ExecutionOverrides {
            explicit_plan: Some(false),
            max_search_calls_per_step: Some(6),
            ..ExecutionOverrides::default()
        };
        let r =
            resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&complex_task));
        assert!(!r.explicit_plan);
        assert_eq!(r.max_search_calls_per_step, 6);
    }
}
