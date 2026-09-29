//! Deterministic execution-policy resolution — the ONE place that decides how
//! hard to drive a model for a given turn.
//!
//! Replaces the retired weak/medium/strong `ModelPolicy` tiers. Inputs are
//! model facts (`ModelProfile`), the executor's seat (`ExecutionRole`), the
//! turn's own limits (`TurnProfile`), and always-on safety rails. Resolution
//! is pure and deterministic: min-composition for concurrency, a precedence
//! chain for reasoning effort, and no runtime auto-tuning in v1.

use leveler_model::{ModelProfile, ReasoningEffort, ReasoningRetention};

use crate::coding::factory::TurnProfile;

/// Local read-only tool batch width for main/explorer seats. This is a *local
/// executor* resource guard over calls the model already emitted — the model's
/// wire-level parallel-tool-call capability does not cap it (see plan doc §3:
/// profile `max_parallel_tool_calls` is a conservative placeholder today, and
/// folding it in would silently drop 4 → 1).
const DEFAULT_PARALLEL_TOOLS: usize = 4;
/// Per-step distinct-modified-files budget. `0` is unlimited, and that is the
/// default: a patch touching many files is a wide refactor, not evidence that
/// the model needs supervising. Only an explicit caller budget bounds it.
const DEFAULT_FILES_PER_STEP: usize = 0;

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

/// When the post-edit action-throughput guidance is visible to the model.
///
/// The experiment holds pre-edit behavior fixed by keeping the guidance out of
/// the transcript until the runtime has mechanically confirmed the drive's
/// first effective mutation. `Always` reproduces the previous experiment's
/// arm (guidance in the system prompt from round 1) and exists for debugging
/// only — it is never a formal A/B arm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PostEditThroughputMode {
    /// Production: the guidance is never present.
    #[default]
    Off,
    /// Inject the guidance once the first effective mutation of this drive is
    /// committed (the post-edit-only experiment arm).
    PostEdit,
    /// The guidance is in the system prompt from the first round.
    Always,
}

/// eval-only injection seam for single-variable ablation. Production assembly
/// never constructs one; every `None` inherits the resolved default. The
/// executor's progress-guard rail (`repeated_read_guard`, kept under that name
/// because existing experiment configs use it) can ONLY be switched off
/// through here — that is deliberate: measuring a rail's value is an
/// experiment, not a configuration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutionOverrides {
    pub max_parallel_tools: Option<usize>,
    pub max_files_per_step: Option<usize>,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Reasoning effort for the top-level seat only; delegated seats keep
    /// `reasoning_effort` / the model default. MA4-C parent-budget ablation.
    pub main_reasoning_effort: Option<ReasoningEffort>,
    pub max_tool_output_bytes: Option<usize>,
    /// Measurement knob: persist the model context after every round
    /// (`ContextSnapshot`), not only when it diverges from the transcript.
    /// Context-cost attribution reads those rows; production never sets it.
    pub context_trace: Option<bool>,
    /// Experiment knob: add the generic independent-observation batching
    /// guidance to the system prompt. `None` (production) leaves the prompt
    /// byte-identical to today; the eval seam may turn it on to measure whether
    /// the model uses the parallel read-only batch it already has.
    pub investigation_batching: Option<bool>,
    /// Experiment knob: when the generic post-edit action-throughput guidance
    /// becomes visible. `None` (production) leaves the prompt byte-identical;
    /// the eval seam turns on exactly this one variable.
    pub post_edit_action_throughput: Option<PostEditThroughputMode>,
    /// Experiment knob: how much historical assistant reasoning is re-sent to
    /// the provider. `None` (production) means [`ReasoningRetention::All`],
    /// byte-identical to the pre-experiment behaviour. The eval seam turns on
    /// exactly this one variable; the durable transcript is never changed.
    pub reasoning_retention: Option<ReasoningRetention>,
    /// Harness context policy: extra pressure headroom beyond the completion
    /// reservation. `None` means none declared — there is no measured extra
    /// margin to claim, so nothing is invented.
    pub context_headroom_tokens: Option<u32>,
}

/// The harness's context policy for one executor seat: how much of a model's
/// declared capacity to spend, and what to reserve.
///
/// Ownership: [`leveler_model::ModelLimits`] declares FACTS (window, completion
/// capability, quality boundary). This type decides how the harness USES them —
/// the completion reservation, the safety headroom, the pressure threshold, the
/// retention budget. None of those belong on a model profile, because they are
/// not facts about the model; two harnesses may spend the same model's window
/// differently. The split is also why a "headroom" constant is not invented:
/// `headroom: 0` means exactly "no measured extra margin beyond the completion".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedContextPolicy {
    /// The model's declared window (exact fact); `0` = unknown.
    pub context_window: u32,
    /// The quality boundary (where recall is expected to degrade); `0` = not
    /// declared, in which case only capacity bounds the threshold.
    pub quality_boundary: u32,
    /// The completion this harness reserves per request — the effective
    /// request cap, not the model's maximum capability. A request that asks for
    /// less must not be measured against a reservation it will never use.
    pub output_reservation: u32,
    /// Extra safety margin on top of the reservation; `0` = none declared.
    pub headroom: u32,
    /// The pressure threshold: `min(quality_boundary, capacity)` where
    /// `capacity = window - output_reservation - headroom`. `0` = folding
    /// disabled (no window declared).
    pub pressure_threshold: u32,
    /// How much recent history stays verbatim across a fold.
    pub retention: ContextRetentionPolicy,
}

/// The recent-history budget a fold keeps verbatim.
///
/// Deliberately its own concept rather than "half the threshold": the two
/// answer different questions ("when do I fold?" vs "what do I keep?") and
/// must be tunable apart. The values are UNCHANGED from the pre-policy
/// behaviour, which is why they carry no claim of being measured — see the
/// report's `REQUIRES EXPERIMENT` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextRetentionPolicy {
    /// Tail bound in messages (the count half of the pair).
    pub keep_recent_messages: usize,
    /// Tail bound in estimated tokens (the budget half).
    pub keep_recent_tokens: u64,
}

impl ContextRetentionPolicy {
    /// The production retention: the working set is bounded by BOTH a message
    /// count and a token budget, because a single huge tool output inside the
    /// newest messages defeats a count-only bound.
    fn from_threshold(pressure_threshold: u32) -> Self {
        Self {
            keep_recent_messages: leveler_context::COMPACT_KEEP_RECENT,
            keep_recent_tokens: u64::from(pressure_threshold) / RETENTION_TAIL_DIVISOR,
        }
    }
}

/// Fraction of the pressure threshold retained verbatim. Pre-existing value,
/// kept for behavioural compatibility; changing it is an experiment, not a
/// refactor.
const RETENTION_TAIL_DIVISOR: u64 = 2;

impl Default for ResolvedContextPolicy {
    fn default() -> Self {
        Self {
            context_window: 0,
            quality_boundary: 0,
            output_reservation: 0,
            headroom: 0,
            pressure_threshold: 0,
            retention: ContextRetentionPolicy::from_threshold(0),
        }
    }
}

impl ResolvedContextPolicy {
    /// Derive the harness policy from a model's declared facts.
    ///
    /// `output_reservation` is the EFFECTIVE per-request completion cap — what
    /// this executor will actually ask for, not the model's theoretical
    /// maximum — so a seat that caps its output lower gets a correspondingly
    /// larger prompt budget.
    pub fn resolve(
        limits: &leveler_model::ModelLimits,
        output_reservation: u32,
        headroom: u32,
    ) -> Self {
        let window = limits.context_window;
        let reservation = if output_reservation > 0 {
            output_reservation
        } else {
            limits.max_output_tokens
        };
        if window == 0 {
            // No declared window: there is nothing to bound, and `0` keeps its
            // established meaning of "folding disabled" rather than inventing
            // a threshold.
            return Self {
                context_window: 0,
                quality_boundary: limits.reliable_context,
                output_reservation: reservation,
                headroom,
                pressure_threshold: 0,
                retention: ContextRetentionPolicy::from_threshold(0),
            };
        }
        let capacity = window.saturating_sub(reservation).saturating_sub(headroom);
        // The quality boundary: where recall is expected to degrade. With no
        // declaration the usable capacity is the only bound — and when the
        // reservation has consumed the whole window there is no capacity to
        // name, so the window itself is the bound.
        let quality = if limits.reliable_context == 0 {
            if capacity > 0 { capacity } else { window }
        } else {
            limits.reliable_context
        };
        // A reservation plus headroom that exhausts the window leaves no input
        // capacity. `hard_capacity()` reports `None` for that: a hard bound of
        // zero would mark every request as over capacity and force a fold that
        // can never fit, aborting the task. Folding therefore runs on the
        // quality boundary alone — the behaviour that existed before a capacity
        // was derived — which is the honest reading of a route whose declared
        // completion cap exceeds its window (e.g. a reduced-context route that
        // keeps the model's full output declaration).
        let pressure_threshold = if capacity > 0 {
            quality.min(capacity).max(1)
        } else {
            quality.max(1)
        };
        Self {
            context_window: window,
            quality_boundary: limits.reliable_context,
            output_reservation: reservation,
            headroom,
            pressure_threshold,
            retention: ContextRetentionPolicy::from_threshold(pressure_threshold),
        }
    }

    /// Whether folding is enabled at all.
    pub fn folding_enabled(&self) -> bool {
        self.pressure_threshold > 0
    }

    /// The largest projected input this harness will legally SEND: the declared
    /// window with the completion reservation and safety headroom removed.
    ///
    /// This is the SECOND bound, and it answers a different question than
    /// [`Self::pressure_threshold`]. The threshold is where recall is expected
    /// to degrade — folding there is a quality choice that may be abandoned.
    /// The capacity is where a request can no longer be sent at all — folding
    /// there is required to make the next request legal. When the model
    /// declares a quality boundary below its usable window the two differ, and
    /// the gap between them is exactly the room a failed fold may continue in.
    ///
    /// `None` when no window is declared, or when the reservation and headroom
    /// leave no input room: with no hard limit there is no request compaction
    /// is obliged to make legal, so nothing here may claim one. A zero would be
    /// such a claim and would abort every task, so it is never returned.
    pub fn hard_capacity(&self) -> Option<u64> {
        let capacity = self
            .context_window
            .saturating_sub(self.output_reservation)
            .saturating_sub(self.headroom);
        (self.context_window > 0 && capacity > 0).then_some(u64::from(capacity))
    }
}

/// The interactive-chat fold threshold. Chat holds a conservative window;
/// a task folds at the model's own declared reliable context. Resolved
/// through this one seam so the two cannot drift apart unnoticed (C2.1
/// recorded them diverging: 24k vs the task budget).
pub const CHAT_CONTEXT_BUDGET: u32 = crate::PRE_REQUEST_COMPACT_THRESHOLD as u32;

/// The fully resolved execution configuration for one executor. For the
/// numeric budget fields `0` means unlimited, matching executor semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedExecutionPolicy {
    pub max_output_tokens: u32,
    /// How the harness spends this model's declared capacity: window, quality
    /// boundary, completion reservation, headroom, fold threshold and the
    /// retention budget, resolved once here.
    pub context_policy: ResolvedContextPolicy,
    /// This route's resolved reasoning-replay contract, passed to the kernel so
    /// the projection it applies is the same one the adapter encodes.
    pub reasoning_replay: leveler_model::ReasoningReplayContract,
    pub max_parallel_tools: usize,
    pub max_files_per_step: usize,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Byte budget for a single tool result (the central output cap).
    pub max_tool_output_bytes: usize,
    /// Persist the model context every round (eval measurement seam).
    pub context_trace: bool,
    /// The independent-observation batching soft policy is in the prompt.
    pub investigation_batching: bool,
    /// When the independent-action (post-edit throughput) soft policy is visible.
    pub post_edit_action_throughput: PostEditThroughputMode,
    /// How much historical assistant reasoning the provider request carries.
    /// Production resolves to [`ReasoningRetention::All`].
    pub reasoning_retention: ReasoningRetention,
}

/// min over concurrency caps where `0` means "no opinion / unlimited".
fn min_nonzero(caps: &[usize]) -> usize {
    caps.iter().copied().filter(|&c| c > 0).min().unwrap_or(0)
}

/// The tool-context slice of resolution: the per-step modified-files budget.
/// Split out because the tool context is built once per engine (before any
/// turn exists), while the executor is resolved per turn — both must read the
/// SAME defaults or the seam drifts.
pub fn resolve_tool_limits(overrides: Option<&ExecutionOverrides>) -> usize {
    overrides
        .and_then(|o| o.max_files_per_step)
        .unwrap_or(DEFAULT_FILES_PER_STEP)
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

    // The context policy is resolved from the model's declared facts and the
    // harness's reservation/headroom choices. `resolved_output_cap` is what
    // this seat will actually ask the provider for, so a seat that caps its
    // output lower gets the capacity back as prompt budget.
    let resolved_output_cap = profile.limits.max_output_tokens;
    let context_policy = ResolvedContextPolicy::resolve(
        &profile.limits,
        resolved_output_cap,
        o.context_headroom_tokens.unwrap_or(0),
    );

    ResolvedExecutionPolicy {
        max_output_tokens: profile.limits.max_output_tokens,
        context_policy,
        reasoning_replay: leveler_model::ReasoningReplayContract::resolve(
            profile.protocol,
            &profile.compatibility,
        ),
        max_parallel_tools,
        max_files_per_step: o.max_files_per_step.unwrap_or(DEFAULT_FILES_PER_STEP),
        // Safety rail: only the eval seam may lower it.
        reasoning_effort: leveler_model::resolve_reasoning_effort(
            match role {
                ExecutionRole::Main => o.main_reasoning_effort.or(o.reasoning_effort),
                _ => o.reasoning_effort,
            },
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
        context_trace: o.context_trace.unwrap_or(false),
        investigation_batching: o.investigation_batching.unwrap_or(false),
        post_edit_action_throughput: o
            .post_edit_action_throughput
            .unwrap_or(PostEditThroughputMode::Off),
        // Production default is All: the request projection is a no-op.
        reasoning_retention: o.reasoning_retention.unwrap_or(ReasoningRetention::All),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding::factory::TurnProfile;
    use crate::{ContinuationPolicy, StepLimits};
    use leveler_model::{ModelProfile, ReasoningEffort, ReasoningRetention};

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

    /// The fold threshold is derived from the model's declared facts, not
    /// copied from `reliable_context`: a quality bound declared above
    /// `context_window - max_output_tokens` must not authorize a request that
    /// leaves no room for its own completion.
    #[test]
    fn fold_budget_reserves_the_declared_completion() {
        let mut p = profile();
        p.limits.context_window = 1_048_576;
        p.limits.reliable_context = 786_432;
        p.limits.max_output_tokens = 393_216;
        let resolved = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None);
        assert_eq!(
            resolved.context_policy.pressure_threshold, 655_360,
            "the completion reservation must cap the quality bound"
        );
        // A quality bound below capacity is left alone.
        p.limits.reliable_context = 400_000;
        let resolved = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None);
        assert_eq!(resolved.context_policy.pressure_threshold, 400_000);
    }

    /// The prompt budget reserves exactly the completion envelope the executor
    /// will put on the request (`ResolvedExecutionPolicy::max_output_tokens`,
    /// which the drive passes to `Agent::with_max_output_tokens`). Reserving a
    /// different number — the model's theoretical maximum while requesting less
    /// — would fold early for room the request never asks for.
    #[test]
    fn the_reservation_is_the_request_envelope_the_executor_asks_for() {
        let mut p = profile();
        p.limits.context_window = 1_048_576;
        p.limits.reliable_context = 786_432;
        p.limits.max_output_tokens = 393_216;
        let resolved = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None);
        assert_eq!(resolved.max_output_tokens, 393_216);
        assert_eq!(
            resolved.context_policy.output_reservation, resolved.max_output_tokens,
            "the reservation must be the cap this request actually carries"
        );
        assert_eq!(
            resolved.context_policy.pressure_threshold, 655_360,
            "capacity (window − reservation) caps the quality bound"
        );
    }

    /// The pressure threshold, the reservation and the retention budget are
    /// separate policy fields, and each is derived from the model's facts plus
    /// the harness's choices.
    #[test]
    fn context_policy_separates_capacity_quality_reservation_and_retention() {
        use super::{ContextRetentionPolicy, ResolvedContextPolicy};
        let limits = |window, quality, output| leveler_model::ModelLimits {
            context_window: window,
            reliable_context: quality,
            max_output_tokens: output,
            max_tool_schema_bytes: 32_768,
            max_parallel_tool_calls: 1,
            max_tool_output_bytes: None,
        };

        // Capacity binds: the quality bound sits above the usable window.
        let policy = ResolvedContextPolicy::resolve(&limits(1_048_576, 786_432, 393_216), 0, 0);
        assert_eq!(policy.quality_boundary, 786_432);
        assert_eq!(policy.output_reservation, 393_216);
        assert_eq!(policy.headroom, 0);
        assert_eq!(policy.pressure_threshold, 655_360);
        assert_eq!(policy.retention.keep_recent_tokens, 327_680);
        assert_eq!(
            policy.retention.keep_recent_messages,
            leveler_context::COMPACT_KEEP_RECENT
        );

        // A request that asks for LESS completion reserves less, so the prompt
        // budget grows: the reservation is the effective one, not the model's
        // maximum capability.
        let smaller =
            ResolvedContextPolicy::resolve(&limits(1_048_576, 786_432, 393_216), 32_768, 0);
        assert_eq!(smaller.output_reservation, 32_768);
        assert_eq!(smaller.pressure_threshold, 786_432, "quality binds now");

        // Quality binds when it sits below capacity…
        let quality = ResolvedContextPolicy::resolve(&limits(128_000, 64_000, 8_192), 0, 0);
        assert_eq!(quality.pressure_threshold, 64_000);
        // …and headroom only ever lowers it.
        let headroom = ResolvedContextPolicy::resolve(&limits(128_000, 64_000, 8_192), 0, 65_536);
        assert_eq!(headroom.headroom, 65_536);
        assert_eq!(headroom.pressure_threshold, 54_272);

        // An undeclared window keeps folding disabled (`0`), and an undeclared
        // quality bound is not invented.
        let unknown = ResolvedContextPolicy::resolve(&limits(0, 0, 0), 0, 0);
        assert!(!unknown.folding_enabled());
        assert_eq!(unknown.quality_boundary, 0);
        assert_eq!(
            ResolvedContextPolicy::resolve(&limits(128_000, 0, 8_192), 0, 0).pressure_threshold,
            119_808,
            "no quality declaration → capacity is the whole bound"
        );

        // A reservation plus headroom that exhausts the window is a
        // misdeclaration; the harness cannot claim a hard bound it can never
        // satisfy, so it folds on the quality boundary instead of forcing a
        // fold that cannot fit (which would abort the task).
        let exhausted = ResolvedContextPolicy::resolve(&limits(8_192, 4_096, 8_191), 0, 8_192);
        assert_eq!(exhausted.pressure_threshold, 4_096);
        assert_eq!(exhausted.hard_capacity(), None);
        assert!(exhausted.folding_enabled());

        // A reduced-context route that keeps the model's full completion
        // declaration reserves more output than it has window. The harness
        // must not derive `hard_capacity() == Some(0)` from that: the zero
        // marks every request over capacity and aborts the task with a fold
        // that can never fit. It folds on the quality boundary instead.
        let over_window = ResolvedContextPolicy::resolve(&limits(131_072, 24_000, 393_216), 0, 0);
        assert_eq!(over_window.output_reservation, 393_216);
        assert_eq!(over_window.quality_boundary, 24_000);
        assert_eq!(over_window.pressure_threshold, 24_000);
        assert_eq!(over_window.hard_capacity(), None);
        assert!(over_window.folding_enabled());

        // The retention budget is its own field, not a fraction re-derived at
        // the call site: a policy with a different threshold keeps the same
        // shape of relationship but a different number.
        let other = ResolvedContextPolicy::resolve(&limits(64_000, 32_000, 4_000), 0, 0);
        assert_eq!(other.pressure_threshold, 32_000);
        assert_eq!(other.retention.keep_recent_tokens, 16_000);
        assert_eq!(
            other.retention,
            ContextRetentionPolicy {
                keep_recent_messages: leveler_context::COMPACT_KEEP_RECENT,
                keep_recent_tokens: 16_000,
            }
        );
    }

    /// The route's replay contract travels with the resolved policy, so the
    /// kernel applies the same contract the adapter encodes.
    #[test]
    fn resolution_carries_the_route_replay_contract() {
        let p = profile();
        let resolved = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None);
        assert_eq!(
            resolved.reasoning_replay,
            leveler_model::ReasoningReplayContract::NONE
        );
        let mut passback = profile();
        passback.compatibility = serde_json::from_value(serde_json::json!({
            "reasoning_replay_scope": "when_tools_present",
            "reasoning_content_key_required": true
        }))
        .unwrap();
        let resolved = resolve_execution_policy(&passback, ExecutionRole::Main, &goal_turn(), None);
        assert_eq!(resolved.reasoning_replay.arm_name(), "when_tools+empty");
    }

    /// Migration contract: for a main seat with no overrides, resolution must
    /// equal what the retired `default_policy()` produced through the old
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

    /// The batching soft policy is an experiment: production runs never carry
    /// it, and the eval seam can turn it on without touching anything else.
    #[test]
    fn investigation_batching_is_off_unless_the_eval_seam_asks_for_it() {
        let p = profile();
        assert!(
            !resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None)
                .investigation_batching,
            "production never adds the batching guidance"
        );
        let o = ExecutionOverrides {
            investigation_batching: Some(true),
            ..ExecutionOverrides::default()
        };
        let resolved = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&o));
        assert!(resolved.investigation_batching);
        assert!(!resolved.context_trace, "the knob flips one variable only");
        assert_eq!(
            resolved.max_parallel_tools, 4,
            "the batch width is untouched"
        );
    }

    /// The post-edit throughput knob is an experiment: production runs never
    /// carry its guidance, and the eval seam can turn it on without touching
    /// any other resolver input.
    #[test]
    fn post_edit_action_throughput_is_off_unless_the_eval_seam_asks_for_it() {
        let p = profile();
        assert_eq!(
            resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None)
                .post_edit_action_throughput,
            PostEditThroughputMode::Off,
            "production never adds the post-edit batching guidance"
        );
        let o = ExecutionOverrides {
            post_edit_action_throughput: Some(PostEditThroughputMode::PostEdit),
            ..ExecutionOverrides::default()
        };
        let resolved = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&o));
        assert_eq!(
            resolved.post_edit_action_throughput,
            PostEditThroughputMode::PostEdit
        );
        assert!(
            !resolved.investigation_batching,
            "the post-edit knob does not also flip the pre-edit one"
        );
        assert!(!resolved.context_trace, "the knob flips one variable only");
    }

    /// The retention policy is an experiment: production resolves to `All`
    /// (no projection), and the eval seam can select a window without
    /// touching any other resolver input.
    #[test]
    fn reasoning_retention_defaults_to_all_and_only_the_seam_changes_it() {
        let p = profile();
        assert_eq!(
            resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), None)
                .reasoning_retention,
            ReasoningRetention::All,
            "production carries every historical reasoning block"
        );
        let o = ExecutionOverrides {
            reasoning_retention: Some(ReasoningRetention::LastTurns(3)),
            ..ExecutionOverrides::default()
        };
        let resolved = resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&o));
        assert_eq!(
            resolved.reasoning_retention,
            ReasoningRetention::LastTurns(3)
        );
        assert_eq!(resolved.reasoning_effort, None, "effort is untouched");
        assert_eq!(resolved.max_parallel_tools, 4, "batch width is untouched");
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
    fn tool_limits_resolve_to_the_task_file_budget() {
        // De-engineering Wave 2 made the per-step file budget unlimited by
        // default: a patch touching nine files is a wide refactor, not a
        // mistake, and only an explicit caller budget bounds it.
        assert_eq!(resolve_tool_limits(None), DEFAULT_FILES_PER_STEP);
        assert_eq!(DEFAULT_FILES_PER_STEP, 0, "0 means unlimited");
        let o = ExecutionOverrides {
            max_files_per_step: Some(2),
            ..ExecutionOverrides::default()
        };
        assert_eq!(resolve_tool_limits(Some(&o)), 2);
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
    fn main_reasoning_effort_lowers_only_the_top_level_seat() {
        let mut p = profile();
        p.capabilities.reasoning = true;
        p.reasoning.default_effort = Some(ReasoningEffort::Max);
        p.reasoning.supported_efforts = vec![
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ];
        let parent_only = ExecutionOverrides {
            main_reasoning_effort: Some(ReasoningEffort::High),
            ..ExecutionOverrides::default()
        };
        let main =
            resolve_execution_policy(&p, ExecutionRole::Main, &goal_turn(), Some(&parent_only));
        assert_eq!(main.reasoning_effort, Some(ReasoningEffort::High));
        for role in [
            ExecutionRole::Default,
            ExecutionRole::Explorer,
            ExecutionRole::Worker,
            ExecutionRole::Reviewer,
        ] {
            let child = resolve_execution_policy(&p, role, &goal_turn(), Some(&parent_only));
            assert_eq!(
                child.reasoning_effort,
                Some(ReasoningEffort::Max),
                "{role:?} keeps the model default"
            );
        }
    }
}
