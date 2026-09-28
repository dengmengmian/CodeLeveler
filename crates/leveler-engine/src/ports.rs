//! The lifecycle ports: what the engine offers a harness that runs a turn.
//!
//! The engine owns durability, ownership and the event log; a harness owns
//! the model loop. These traits are the seam between them, and they are
//! declared here — on the engine side — so the harness depends on the engine
//! and never the other way round.
//!
//! Every port fails closed. A barrier that cannot make an event durable, a
//! fence that cannot prove ownership, or a checkpoint that cannot commit all
//! abort the run rather than let a side effect happen unrecorded.

use async_trait::async_trait;

use leveler_execution::RiskLevel;
use leveler_model::{FinishReason, Message, TokenUsage};

/// Why a lifecycle port refused. Both variants are terminal for the run: the
/// engine could not make a fact durable, or this runtime no longer owns the
/// task it was writing for.
#[derive(Debug, thiserror::Error)]
pub enum PortError {
    #[error("persistence error: {0}")]
    Persistence(String),
    /// The runtime lost task ownership: a newer OwnerEpoch exists.
    #[error("stale runtime ownership: {0}")]
    StaleOwnership(String),
}

/// The ownership fence: proves the runtime still owns its task before a
/// model-proposed tool may produce an external side effect. Checked AFTER
/// the persistence barriers (ToolCallStarted + approval durable) and BEFORE
/// dispatch. It cannot make side effects exactly-once - it only guarantees a
/// runtime already known stale dispatches nothing new.
#[async_trait::async_trait]
pub trait ExecutionFence: Send + Sync {
    /// Err(reason) = the token is stale; the run must abort.
    async fn ensure_current(&self) -> Result<(), String>;
}

/// Awaitable durability barrier for canonical tool events (side-effect
/// barrier, convergence plan phase 1). `flush` resolves once every canonical
/// event emitted through the observer so far is durable. The loop awaits it
/// after announcing a tool call (before hooks/approval can act) and again
/// after authorization (before dispatch), so a crash can never leave a side
/// effect whose `ToolCallStarted` — or whose approval outcome — was lost.
/// A flush failure aborts the run: the tool is NOT executed, because an
/// unexecuted tool is recoverable but an unrecorded side effect is not.
///
/// Hosts without durable persistence (sub-agents, standalone library use)
/// leave the barrier unset; the loop then proceeds without waiting.
#[async_trait]
pub trait EventBarrier: Send + Sync {
    async fn flush(&self) -> Result<(), PortError>;

    /// Record a canonical tool event from a DELEGATED agent, attributed to it.
    ///
    /// A sub-agent's tool calls used to surface only as transient
    /// `SubAgentActivity`, so a worker child that crashed mid-edit left
    /// nothing the host could reconcile. These are the durable facts instead.
    ///
    /// The implementation MUST enqueue this on the same ordered queue that
    /// [`Self::flush`] drains. Anything else races: the flush marker could
    /// overtake the event it is supposed to be waiting for, and the barrier
    /// would report durability for a call that is not recorded yet.
    ///
    /// Required, deliberately. A default no-op would let a barrier flush
    /// successfully while silently discarding the record — the caller would
    /// then run a delegated side effect believing it was durable, which is
    /// the exact failure this method exists to prevent.
    fn record_child_tool_event(&self, event: ChildToolEvent);
}

/// A delegated agent's tool call, attributed to the child that made it.
#[derive(Debug, Clone, PartialEq)]
pub enum ChildToolEvent {
    Started {
        agent_id: String,
        call_id: String,
        name: String,
        arguments: String,
        /// The tool's declared risk. Supplied by the harness that owns the
        /// registry: the engine records the fact, it does not classify it.
        risk: Option<RiskLevel>,
    },
    Finished {
        agent_id: String,
        call_id: String,
        name: String,
        is_error: bool,
        preview: String,
    },
    /// A write-ownership transition (`claim_write_scope` granted or denied).
    ///
    /// Rides this queue, not the activity channel, because the grant must be
    /// durable BEFORE any event whose authorization depends on it — a child's
    /// writes flush here immediately, so a grant recorded anywhere else can
    /// land after the write it authorized and read as a bypass.
    ///
    /// Deliberately NOT a `Started`/`Finished` pair: `claim_write_scope` is a
    /// virtual tool the drive loop answers inline and never registers, so a
    /// `ToolCallStarted` for it would look like a dangling call to crash
    /// recovery and demand human reconciliation for an operation with no
    /// external side effect — the registry it mutates is in memory and dies
    /// with the process.
    Ownership {
        agent_id: String,
        action: String,
        detail: String,
    },
    /// Messages the child appended to its own transcript.
    ///
    /// On this queue so a child's transcript lands in order with the tool
    /// facts it produced: a round is appended only after its tools ran, and
    /// those tools' `Started` events are already ahead of it here.
    Transcript {
        agent_id: String,
        messages: Vec<leveler_model::Message>,
    },
}

/// A sink that persists the transcript as the loop advances, enabling resume.
/// Called with the messages appended in each step (seed, then per round).
#[async_trait]
pub trait TranscriptSink: Send {
    async fn append(&mut self, messages: &[Message]) -> Result<(), PortError>;

    /// Persist only observed text from a failed invocation. Durable transcript
    /// implementations may attach host metadata for history projection; the
    /// default still uses the same append sink and propagates storage errors.
    async fn append_partial_response(&mut self, text: &str) -> Result<(), PortError> {
        self.append(&[Message::interrupted_response(text)]).await
    }

    async fn record_model_request(
        &mut self,
        _record: &ModelRequestRecord,
    ) -> Result<(), PortError> {
        Ok(())
    }
}

/// Diagnostic facts for one provider attempt, including failed attempts.
/// Persisting the normalized finish reason distinguishes truncation from completion.
#[derive(Debug, Clone)]
pub struct ModelRequestRecord {
    /// Goal identity, or root turn identity for a conversation without a goal.
    /// Recovery reconciles only this budget's attempts.
    pub budget_scope: Option<String>,
    /// Admission estimate when usage is absent; never provider-reported usage.
    pub estimated_tokens: Option<u64>,
    /// The provider's request id, when it reported one. A diagnostic, not a
    /// key: the engine generates the row's identity, because a repeated
    /// provider id used to abort the turn on a UNIQUE violation.
    pub provider_request_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub usage: TokenUsage,
    pub finish_reason: Option<FinishReason>,
    /// Stable failure class for attempts that did not finish normally.
    pub error_kind: Option<String>,
    pub latency_ms: u64,
    /// Wall-clock duration of this attempt. `None` on a path that never
    /// measured it — an absent measurement, never a zero.
    pub attempt_ms: Option<u64>,
    /// Request start to response headers.
    pub connect_ms: Option<u64>,
    /// Request start to the first generated content event. Synthetic start
    /// markers and usage-only events are not tokens.
    pub ttft_ms: Option<u64>,
    /// Longest stretch inside the stream with no event at all.
    pub max_event_gap_ms: Option<u64>,
    /// Retry contribution: 1 for a retried attempt, 0 for an initial attempt.
    /// Legacy rows aggregate retries for a logical call; summing this field
    /// retains the total number of retries across both formats.
    pub retry_count: u32,
    /// The runtime's own estimate of the prompt it sent, priced over the
    /// provider-visible request projection. Beside — never instead of —
    /// `usage.input_tokens`: the provider's number is what was billed.
    /// `None` on a path that never projected a request (an auxiliary call).
    pub projected_input_tokens: Option<u64>,
    /// The runtime's own estimate of how much of that projected prompt was the
    /// historical-reasoning channel — the replayed thinking the route carries.
    /// `Some(0)` is "projected, and this route carries no reasoning"; `None`
    /// is "no projection was measured". Beside
    /// [`Self::usage`]`.reasoning_tokens`, which is a SUBSET of the
    /// provider's *output*; this is a slice of the *input*.
    pub projected_reasoning_tokens: Option<u64>,
    /// Which lane this call belongs to. A fold's summarization is a provider
    /// call like any other; recording it under its own lane is what lets a
    /// session's cost be attributed to the work versus the overhead.
    pub kind: ModelCallKind,
    /// The sub-agent that made this call, or `None` for the parent's own. A
    /// child runs as an owned `'static` future and cannot borrow the parent's
    /// sink, so its records travel back over the progress channel carrying
    /// this; without it a reviewer's spend has nowhere to land.
    pub agent_id: Option<String>,
    /// Estimated cost in micro-USD, priced where the model has pricing
    /// configured. `None` means unpriced, never free.
    pub cost_usd_micros: Option<u64>,
    /// The reasoning effort this call asked the provider for (`high`, …), as
    /// sent on the wire. `None`: the request named none, so the provider's
    /// default applied. Recorded so an agent's declared effort can be checked
    /// against what actually ran, not against configuration.
    pub reasoning_effort: Option<String>,
}

impl ModelRequestRecord {
    /// Fill in the estimated cost from the model's pricing, if any.
    ///
    /// Cost is priced once, here, against the usage the provider actually
    /// reported — including how much of the prompt it served from cache. A row
    /// that carries its own cost can be summed later without re-deriving it
    /// from a price table that may since have changed.
    ///
    /// Reasoning tokens are deliberately absent from the pricing inputs: the
    /// provider's `output_tokens` already contains them, so charging them again
    /// would bill the same thinking twice.
    pub fn priced(mut self, pricing: Option<&leveler_model::ModelPricing>) -> Self {
        if self.cost_usd_micros.is_none() {
            self.cost_usd_micros = pricing
                .filter(|_| self.usage.total() > 0)
                .and_then(|p| p.cost_for_usage(&self.usage));
        }
        self
    }
}

/// Which lane a model call belongs to. The drive loop's rounds are the work;
/// everything else is overhead the runtime chose to spend, and cost
/// attribution has to be able to tell them apart. Mapped onto the storage
/// enum at the engine boundary — this crate does not depend on storage.
///
/// This is the ONE taxonomy of automatic model calls. A new automatic call
/// site must name a kind here rather than appearing as an anonymous request,
/// and each kind owns the reasoning policy a caller uses when it names none —
/// so a one-sentence auxiliary task can never silently inherit a coding
/// model's Max effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCallKind {
    /// A main-loop round: the model deciding what to do next.
    Round,
    /// The summarization behind a compaction fold.
    Compaction,
    /// Semantic memory-candidate extraction from the user's own turns.
    MemoryExtraction,
    /// The idle "what would the user type next" prompt suggestion.
    PromptSuggestion,
    /// The idle recap sentence shown when the user returns.
    AwaySummary,
    /// The bounded wording pass for a manually requested recap.
    SemanticRecap,
    /// An explicit side question (`/btw`) on the session's side thread.
    SideQuestion,
    /// A provider health probe. Never a session's cost.
    ProviderProbe,
    /// A bounded harness-initiated call that is not a main-loop round, and not
    /// one of the named product affordances above.
    Advisory,
}

impl ModelCallKind {
    /// The stable token. Persisted, so it is not a display name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Round => "round",
            Self::Compaction => "compaction",
            Self::MemoryExtraction => "memory_extraction",
            Self::PromptSuggestion => "prompt_suggestion",
            Self::AwaySummary => "away_summary",
            Self::SemanticRecap => "semantic_recap",
            Self::SideQuestion => "side_question",
            Self::ProviderProbe => "provider_probe",
            Self::Advisory => "advisory",
        }
    }

    /// The reasoning effort a caller uses when the task names none.
    ///
    /// Mechanical auxiliary tasks (extraction, suggestions, recaps, probes)
    /// are small and bounded: they ask for the lowest useful effort and never
    /// for the model's configured coding effort. Compaction is the one
    /// exception — a fold's briefing is quality-critical for the rest of the
    /// session, so it asks for the middle of the scale, not the floor.
    ///
    /// `None` for [`Self::Round`] and [`Self::SideQuestion`]: those are the
    /// user's own work, so their effort is the resolved coding policy (or the
    /// user's explicit choice), not an auxiliary default.
    pub fn default_reasoning_effort(self) -> Option<leveler_model::ReasoningEffort> {
        use leveler_model::ReasoningEffort;
        match self {
            Self::Round | Self::SideQuestion | Self::Advisory => None,
            Self::Compaction => Some(ReasoningEffort::Medium),
            Self::MemoryExtraction
            | Self::PromptSuggestion
            | Self::AwaySummary
            | Self::SemanticRecap
            | Self::ProviderProbe => Some(ReasoningEffort::Low),
        }
    }
}

/// A child this runtime durably STARTED that never produced a terminal fact.
///
/// Every field is a mechanical fact the engine read off the event log. The
/// engine passes them along; it does not interpret them.
#[derive(Debug, Clone)]
pub struct LostChild {
    pub id: String,
    pub nickname: String,
    /// The role label recorded on `SubAgentStarted`, carried verbatim.
    pub role: String,
}

/// An interrupted child the engine recorded as resumed for this turn: the
/// harness launches its new activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumedChild {
    pub id: String,
    pub nickname: String,
    /// The role label recorded on `SubAgentStarted`, carried verbatim.
    pub role: String,
    /// Which resume this is (1 = the first activation after the original).
    pub attempt: u32,
}

/// What a harness has to say about one lost child.
#[derive(Debug, Clone, Default)]
pub struct LostChildNote {
    /// A clause appended to the engine's own sentence, after "; ". The engine
    /// states the lifecycle fact; this states what it meant for the task.
    pub detail: Option<String>,
    /// What the child contributed before it was lost. The engine cannot
    /// compute this: it would have to read the harness's role vocabulary and
    /// its evidence record.
    pub contribution: Option<leveler_lifecycle::ChildResultProjection>,
    /// The harness's four-way reading of what the lost child left behind.
    pub outcome: Option<leveler_lifecycle::ChildStatus>,
}

/// Speaks for the children a runtime lost.
///
/// The engine owns the whole mechanism: detecting the loss, ordering the
/// terminal before the turn's own, attributing it to the turn the child
/// STARTED in, stamping `ok: false`, and writing it durably. None of that is
/// delegated, and a harness cannot turn a lost child into a successful one.
///
/// What the child CONTRIBUTED is the one thing the engine cannot know, so a
/// harness that has an answer supplies it here. One with none — the engine's
/// own tests, another product built on this kernel — supplies no voice at all
/// and still gets a truthful terminal; it just says less.
///
/// Infallible on purpose. A harness that cannot read its own evidence returns
/// no note for that child: a ghost left running forever is worse than a terse
/// terminal, so nothing a harness does here may block the settlement.
#[async_trait]
pub trait LostChildVoice: Send + Sync {
    /// A note per child the harness can speak for, keyed by child id. Children
    /// left unnamed get the engine's mechanical terminal.
    async fn speak_for(&self, lost: &[LostChild]) -> Vec<(String, LostChildNote)>;

    /// Which of these interrupted children the harness continues in a new
    /// activation, by id. Whether a child CAN be continued is the harness's
    /// knowledge (its spec, its role, its evidence); the engine records the
    /// resume and settles every child not named as lost. A harness with no
    /// child semantics continues none.
    async fn continues(&self, _interrupted: &[LostChild]) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod call_kind_policy_tests {
    use leveler_model::ReasoningEffort;

    use super::ModelCallKind;

    /// Every auxiliary lane names its own effort. The point is that a small
    /// task is never left to inherit the coding model's `default_effort=max`:
    /// a caller that names nothing gets this value instead.
    #[test]
    fn auxiliary_kinds_never_implicitly_ask_for_max() {
        for kind in [
            ModelCallKind::MemoryExtraction,
            ModelCallKind::PromptSuggestion,
            ModelCallKind::AwaySummary,
            ModelCallKind::SemanticRecap,
            ModelCallKind::ProviderProbe,
            ModelCallKind::Compaction,
        ] {
            let effort = kind
                .default_reasoning_effort()
                .unwrap_or_else(|| panic!("{kind:?} must declare an effort"));
            assert_ne!(effort, ReasoningEffort::Max, "{kind:?} inherited Max");
        }
    }

    #[test]
    fn mechanical_auxiliary_tasks_ask_for_low() {
        for kind in [
            ModelCallKind::MemoryExtraction,
            ModelCallKind::PromptSuggestion,
            ModelCallKind::AwaySummary,
            ModelCallKind::SemanticRecap,
            ModelCallKind::ProviderProbe,
        ] {
            assert_eq!(
                kind.default_reasoning_effort(),
                Some(ReasoningEffort::Low),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn compaction_asks_for_the_middle_not_the_floor() {
        // A fold's briefing carries the rest of the session; it is the one
        // auxiliary call that deliberately does not ask for the cheapest tier.
        assert_eq!(
            ModelCallKind::Compaction.default_reasoning_effort(),
            Some(ReasoningEffort::Medium)
        );
    }

    #[test]
    fn the_users_own_work_keeps_the_resolved_policy() {
        // Round and side question are the user's work: returning `None` means
        // "use the resolved coding policy", not "use the model default".
        assert_eq!(ModelCallKind::Round.default_reasoning_effort(), None);
        assert_eq!(ModelCallKind::SideQuestion.default_reasoning_effort(), None);
    }

    #[test]
    fn stored_tokens_round_trip_through_the_kind_taxonomy() {
        for kind in [
            ModelCallKind::Round,
            ModelCallKind::Compaction,
            ModelCallKind::MemoryExtraction,
            ModelCallKind::PromptSuggestion,
            ModelCallKind::AwaySummary,
            ModelCallKind::SemanticRecap,
            ModelCallKind::SideQuestion,
            ModelCallKind::ProviderProbe,
            ModelCallKind::Advisory,
        ] {
            assert_eq!(
                leveler_storage::ModelCallKind::from_stored(kind.as_str()),
                match kind {
                    ModelCallKind::Round => leveler_storage::ModelCallKind::Round,
                    ModelCallKind::Compaction => leveler_storage::ModelCallKind::Compaction,
                    ModelCallKind::MemoryExtraction => {
                        leveler_storage::ModelCallKind::MemoryExtraction
                    }
                    ModelCallKind::PromptSuggestion => {
                        leveler_storage::ModelCallKind::PromptSuggestion
                    }
                    ModelCallKind::AwaySummary => leveler_storage::ModelCallKind::AwaySummary,
                    ModelCallKind::SemanticRecap => leveler_storage::ModelCallKind::SemanticRecap,
                    ModelCallKind::SideQuestion => leveler_storage::ModelCallKind::SideQuestion,
                    ModelCallKind::ProviderProbe => leveler_storage::ModelCallKind::ProviderProbe,
                    ModelCallKind::Advisory => leveler_storage::ModelCallKind::Advisory,
                },
                "{kind:?}"
            );
        }
    }
}

#[cfg(test)]
mod priced_tests {
    use leveler_model::{FinishReason, ModelPricing, TokenUsage};

    use super::{ModelCallKind, ModelRequestRecord};

    fn record(usage: TokenUsage) -> ModelRequestRecord {
        ModelRequestRecord {
            budget_scope: None,
            estimated_tokens: None,
            provider_request_id: None,
            provider: "deepseek".to_string(),
            model: "deepseek-flash".to_string(),
            usage,
            finish_reason: Some(FinishReason::Stop),
            error_kind: None,
            latency_ms: 1,
            attempt_ms: None,
            connect_ms: None,
            ttft_ms: None,
            max_event_gap_ms: None,
            retry_count: 0,
            kind: ModelCallKind::Round,
            agent_id: None,
            projected_input_tokens: None,
            projected_reasoning_tokens: None,
            cost_usd_micros: None,
            reasoning_effort: None,
        }
    }

    /// 1000 in (900 cached) + 1000 out, at the configured DeepSeek-Flash rates.
    fn pricing() -> ModelPricing {
        ModelPricing {
            input_usd_per_mtok: 0.1389,
            output_usd_per_mtok: 0.2778,
            cached_input_usd_per_mtok: Some(0.0139),
        }
    }

    #[test]
    fn absent_usage_is_not_priced_as_free() {
        assert_eq!(
            record(TokenUsage::default())
                .priced(Some(&pricing()))
                .cost_usd_micros,
            None
        );
    }

    #[test]
    fn a_proven_unsent_attempt_keeps_its_known_zero_cost() {
        let mut unsent = record(TokenUsage::default());
        unsent.cost_usd_micros = Some(0);
        assert_eq!(unsent.priced(Some(&pricing())).cost_usd_micros, Some(0));
    }

    /// The reasoning breakdown is a SUBSET of the completion count, so a row
    /// that carries one must cost exactly what the same row costs without it.
    /// Pricing `output + reasoning` would bill the same thinking twice.
    #[test]
    fn a_reasoning_breakdown_does_not_change_the_price() {
        let without = record(TokenUsage {
            input_tokens: 1_000,
            output_tokens: 1_000,
            cached_input_tokens: 900,
            cache_creation_input_tokens: 0,
            reasoning_tokens: None,
        })
        .priced(Some(&pricing()));
        let with = record(TokenUsage {
            input_tokens: 1_000,
            output_tokens: 1_000,
            cached_input_tokens: 900,
            cache_creation_input_tokens: 0,
            reasoning_tokens: Some(700),
        })
        .priced(Some(&pricing()));

        assert_eq!(with.cost_usd_micros, without.cost_usd_micros);
        // 100 uncached in × 0.1389 + 900 cached × 0.0139 + 1000 out × 0.2778
        // = 13.89 + 12.51 + 277.80 micro-USD.
        assert_eq!(with.cost_usd_micros, Some(304));
        // And the stored output total is still the provider's own.
        assert_eq!(with.usage.output_tokens, 1_000);
    }

    /// An unpriced model stays unpriced: a reasoning count is not a price.
    #[test]
    fn reasoning_does_not_invent_a_cost_for_an_unpriced_model() {
        let row = record(TokenUsage {
            input_tokens: 10,
            output_tokens: 10,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: Some(7),
        })
        .priced(None);
        assert_eq!(row.cost_usd_micros, None);
    }
}
