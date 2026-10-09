//! The one model↔tool loop.

use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use leveler_model::{
    CompactionRecord, ContextAccounting, Message, ModelPricing, ModelRef, ModelRequest,
    ModelRuntime, ReasoningEffort, ReasoningReplayContract, ReasoningRetention, RequestProjection,
    ToolChoice,
};

use crate::error::AgentCoreError;
use crate::event::AgentEvent;
use crate::harness::{AgentHarness, Flow, LoopContext};
use crate::limits::{ModelStepAdmission, ModelStepLimits};
use crate::model_round::{ModelRoundObserver, run_model_round_observed};
use crate::stop::StopReason;
use crate::usage::estimate_tokens;

/// A model, the request shape every model step uses, and the mechanical limits
/// the loop enforces. Everything else about a run comes from the
/// [`AgentHarness`] handed to [`Agent::run`].
pub struct Agent {
    runtime: Arc<dyn ModelRuntime>,
    model: ModelRef,
    max_output_tokens: Option<u32>,
    reasoning_effort: Option<ReasoningEffort>,
    /// Ask the provider to turn reasoning off for this run (the user's `off`).
    thinking_disabled: bool,
    pricing: Option<ModelPricing>,
    limits: ModelStepLimits,
    /// How much historical assistant reasoning this run re-sends to the
    /// provider. Production default is [`ReasoningRetention::All`] (no
    /// projection). The policy is applied when each request is assembled and
    /// never mutates the durable transcript.
    reasoning_retention: ReasoningRetention,
    /// The route's resolved reasoning-replay contract. It is a MODEL/route fact
    /// the host resolved; the kernel only applies it when it projects each
    /// request, and never learns a provider name from it.
    reasoning_replay: ReasoningReplayContract,
    /// The model's declared context window (exact fact), when the host knows it.
    context_window: Option<u32>,
    /// The SOFT fold threshold the host resolved, when it knows one.
    compact_at: Option<u32>,
    /// The EFFECTIVE INPUT CAPACITY (window − completion reservation − safety
    /// headroom) the host resolved. `None` = no hard bound could be formed.
    /// The accounting publishes it so compaction utilization has a
    /// model-agnostic denominator; the kernel never derives it.
    input_capacity: Option<u32>,
    /// The completion reservation the request carries.
    output_reservation: Option<u32>,
    /// Safety headroom beyond the reservation.
    headroom: Option<u32>,
    /// Shared record of the most recent compaction fold, written by the
    /// harness that owns folding and read here when the accounting snapshot
    /// is built. The kernel never folds — it only reports what the fold did.
    compaction: Arc<Mutex<Option<CompactionRecord>>>,
}

impl Agent {
    pub fn new(runtime: Arc<dyn ModelRuntime>, model: ModelRef) -> Self {
        Self {
            runtime,
            model,
            max_output_tokens: None,
            reasoning_effort: None,
            thinking_disabled: false,
            pricing: None,
            limits: ModelStepLimits::default(),
            reasoning_retention: ReasoningRetention::All,
            reasoning_replay: ReasoningReplayContract::NONE,
            context_window: None,
            compact_at: None,
            input_capacity: None,
            output_reservation: None,
            headroom: None,
            compaction: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_limits(mut self, limits: ModelStepLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Cap the model's output tokens per request. `None` keeps the
    /// provider's default.
    pub fn with_max_output_tokens(mut self, max_output_tokens: Option<u32>) -> Self {
        self.max_output_tokens = max_output_tokens;
        self
    }

    pub fn with_reasoning_effort(mut self, reasoning_effort: Option<ReasoningEffort>) -> Self {
        self.reasoning_effort = reasoning_effort;
        self
    }

    /// Ask the provider to turn reasoning off for this run.
    pub fn with_thinking_disabled(mut self, thinking_disabled: bool) -> Self {
        self.thinking_disabled = thinking_disabled;
        self
    }

    /// Set the historical-reasoning retention policy for this run. `All` is the
    /// production default and performs no projection.
    pub fn with_reasoning_retention(mut self, retention: ReasoningRetention) -> Self {
        self.reasoning_retention = retention;
        self
    }

    /// Set the route's resolved reasoning-replay contract (which turns carry
    /// captured reasoning, and what a turn that captured none carries).
    /// Resolved by the host from the model profile; the kernel applies it but
    /// never interprets it.
    pub fn with_reasoning_replay(mut self, contract: ReasoningReplayContract) -> Self {
        self.reasoning_replay = contract;
        self
    }

    /// Price every model step's usage so a cost cap can bind. Required when
    /// [`ModelStepLimits::max_cost_usd_micros`] is set.
    pub fn with_pricing(mut self, pricing: Option<ModelPricing>) -> Self {
        self.pricing = pricing;
        self
    }

    /// Declare the model's context window so the accounting can report a
    /// real `free` figure and pressure level. `None` (or omitted) keeps the
    /// window unknown rather than inventing one.
    pub fn with_context_window(mut self, context_window: u32) -> Self {
        self.context_window = Some(context_window);
        self
    }

    /// Declare the SOFT fold threshold the harness folds at.
    pub fn with_compact_at(mut self, compact_at: u32) -> Self {
        self.compact_at = Some(compact_at);
        self
    }

    /// Declare the resolved input budget this request is measured against:
    /// the effective input capacity, the completion reservation and the safety
    /// headroom. All three come from the harness's `ResolvedContextPolicy`; the
    /// kernel does not compute a threshold and does not re-derive a percent.
    pub fn with_input_budget(
        mut self,
        input_capacity: Option<u32>,
        output_reservation: u32,
        headroom: u32,
    ) -> Self {
        self.input_capacity = input_capacity;
        self.output_reservation = (output_reservation > 0).then_some(output_reservation);
        self.headroom = (headroom > 0).then_some(headroom);
        self
    }

    /// Share the compaction record handle with the harness that owns folding.
    /// The harness writes `Some(record)` when it folds; the kernel reads it
    /// when it builds the next accounting snapshot.
    pub fn with_compaction_record(
        mut self,
        compaction: Arc<Mutex<Option<CompactionRecord>>>,
    ) -> Self {
        self.compaction = compaction;
        self
    }

    pub fn compaction_record(&self) -> Arc<Mutex<Option<CompactionRecord>>> {
        self.compaction.clone()
    }

    pub fn model(&self) -> &ModelRef {
        &self.model
    }

    pub fn runtime(&self) -> &Arc<dyn ModelRuntime> {
        &self.runtime
    }

    pub fn pricing(&self) -> Option<&ModelPricing> {
        self.pricing.as_ref()
    }

    pub fn limits(&self) -> &ModelStepLimits {
        &self.limits
    }

    /// Run the loop over `messages` until the model stops, the harness stops
    /// it, or a limit fires.
    ///
    /// One iteration is one **model step**: the kernel admits it, calls the
    /// model once (provider retries are internal to that call), hands the
    /// response to the harness, and runs whatever tool batch it produced.
    ///
    /// ```text
    /// loop {
    ///     harness.on_round_start
    ///     admit next model step   (limits, cancellation, deadline)
    ///     harness.on_round_admitted
    ///     model call              (stream, retry, fold spend)
    ///     harness.on_response
    ///     no tool calls → harness.on_quiet → StopReason::ModelEnd
    ///     tool calls    → harness.execute_calls
    /// }
    /// ```
    pub async fn run<H: AgentHarness>(
        &self,
        mut messages: Vec<Message>,
        harness: &mut H,
        cancellation: CancellationToken,
    ) -> Result<H::Stop, H::Error> {
        if self.limits.max_cost_usd_micros.is_some() && self.pricing.is_none() {
            return Err(AgentCoreError::InvalidLimits(
                "a cost limit requires pricing on the agent".to_string(),
            )
            .into());
        }
        let mut ctx = LoopContext::new(self.limits, cancellation);

        loop {
            match harness.on_round_start(&mut ctx, &mut messages).await? {
                Flow::Continue => {}
                Flow::NextRound => continue,
                Flow::Stop(stop) => return Ok(stop),
            }

            // The one place that decides whether another model call happens.
            // Applied in two phases because the model-step counter advances between
            // them, and what a stop reports depends on which side of that it
            // falls: a spent budget names the model steps that COMPLETED, while the
            // cancel and deadline paths report the step they were about to
            // start.
            let verdict = ctx.admit();
            match &verdict {
                ModelStepAdmission::StopBudget(exhaustion)
                    if exhaustion.dimension != crate::limits::BudgetDimension::Duration =>
                {
                    let stop = ctx.stop(
                        StopReason::BudgetExhausted(exhaustion.clone()),
                        ctx.model_steps(),
                        messages,
                    );
                    return harness.on_stop(&mut ctx, stop).await;
                }
                ModelStepAdmission::StopModelStepCeiling { ceiling } => {
                    let stop = ctx.stop(
                        StopReason::ModelStepCeiling { ceiling: *ceiling },
                        ctx.model_steps(),
                        messages,
                    );
                    return harness.on_stop(&mut ctx, stop).await;
                }
                ModelStepAdmission::StopModelStepWindowLimit => {
                    let limit = self
                        .limits
                        .model_step_window_limit
                        .expect("a window limit only fires when one is pinned");
                    let stop = ctx.stop(
                        StopReason::ModelStepWindowLimit { limit },
                        ctx.model_steps(),
                        messages,
                    );
                    return harness.on_stop(&mut ctx, stop).await;
                }
                _ => {}
            }
            ctx.advance_model_step();
            match verdict {
                ModelStepAdmission::Cancelled => {
                    let stop = ctx.stop(StopReason::Cancelled, ctx.model_steps(), messages);
                    return harness.on_stop(&mut ctx, stop).await;
                }
                // Only the duration dimension reaches here: the token and cost
                // caps returned in the phase above.
                ModelStepAdmission::StopBudget(exhaustion) => {
                    let model_steps = ctx.model_steps().saturating_sub(1);
                    let stop = ctx.stop(
                        StopReason::BudgetExhausted(exhaustion),
                        model_steps,
                        messages,
                    );
                    return harness.on_stop(&mut ctx, stop).await;
                }
                _ => {}
            }

            match harness.on_round_admitted(&mut ctx, &mut messages).await? {
                Flow::Continue => {}
                Flow::NextRound => continue,
                Flow::Stop(stop) => return Ok(stop),
            }

            // The provider request is a PROJECTION of the durable transcript,
            // computed once here by the projection owner: the retention arm
            // says how much historical reasoning is requested, the route
            // contract says what this provider's protocol does with it. The
            // projection travels on the request, so the wire encoder and the
            // accounting below read one decision instead of deriving two.
            // `messages` itself is never touched, so persistence, telemetry
            // and re-analysis still see the full reasoning.
            let mut request = ModelRequest::new(self.model.clone(), messages.clone());
            request.control_context = harness.request_context(&ctx);
            request.tools = harness.tool_definitions();
            request.projection = Some(RequestProjection::project_with_control_context(
                &request.messages,
                &request.tools,
                self.reasoning_replay,
                self.reasoning_retention,
                &request.control_context,
            ));
            request.tool_choice = ToolChoice::Auto;
            request.max_output_tokens = self.max_output_tokens;
            request.reasoning_effort = self.reasoning_effort;
            request.thinking_disabled = self.thinking_disabled;

            // Publish the accounting of the EXACT request about to be sent —
            // its projection, not a second reading of the transcript. This is
            // the runtime's single source of truth for "what is my context
            // made of"; the TUI only renders it.
            let accounting = {
                let projection = request
                    .projection
                    .as_ref()
                    .expect("the projection was just set");
                ContextAccounting::compute(
                    self.model.clone(),
                    projection,
                    self.context_window,
                    self.compact_at,
                    self.compaction.lock().ok().and_then(|g| *g),
                )
                .with_input_budget(self.input_capacity, self.output_reservation, self.headroom)
            };
            harness.on_event(AgentEvent::ContextUsage(accounting));

            let estimated_request_tokens = request
                .projection
                .as_ref()
                .map(|p| p.estimated_tokens())
                .unwrap_or_else(|| estimate_tokens(&request.messages));

            let cancellation = ctx.cancellation().clone();
            let round = run_model_round_observed(
                self.runtime.as_ref(),
                request,
                &cancellation,
                &mut KernelObserver {
                    harness,
                    ctx: &mut ctx,
                    pricing: self.pricing.as_ref(),
                    limits: &self.limits,
                },
            )
            .await;
            let mut round = match round {
                Ok(round) => round,
                Err(KernelRoundError::Host(error)) => return Err(error),
                Err(KernelRoundError::Core(AgentCoreError::Cancelled))
                    if ctx.deadline_expired() =>
                {
                    continue;
                }
                Err(KernelRoundError::Core(error)) => match harness
                    .on_model_error(&mut ctx, error, &mut messages)
                    .await?
                {
                    Flow::Continue | Flow::NextRound => continue,
                    Flow::Stop(stop) => return Ok(stop),
                },
            };
            // Answer-local statistics remain distinct from the per-attempt
            // task spend already folded by the observer.
            round.cost_usd_micros = self
                .pricing
                .as_ref()
                .and_then(|p| p.cost_for_usage(&round.usage));
            round.estimated_tokens = (round.usage.total() == 0).then(|| {
                estimated_request_tokens
                    .saturating_add(estimate_tokens(std::slice::from_ref(&round.message)))
            });

            match harness.on_response(&mut ctx, &round, &mut messages).await? {
                Flow::Continue => {}
                Flow::NextRound => continue,
                Flow::Stop(stop) => return Ok(stop),
            }

            let assistant = round.message;
            let calls = assistant
                .content
                .iter()
                .filter_map(|part| match part {
                    leveler_model::ContentPart::ToolCall { call } => Some(call.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            ctx.note_text(&assistant.text_content());
            messages.push(assistant.clone());

            if calls.is_empty() {
                match harness.on_quiet(&mut ctx, assistant, &mut messages).await? {
                    Flow::Continue => {
                        let stop = ctx.stop(StopReason::ModelEnd, ctx.model_steps(), messages);
                        return harness.on_stop(&mut ctx, stop).await;
                    }
                    Flow::NextRound => continue,
                    Flow::Stop(stop) => return Ok(stop),
                }
            }

            match harness
                .execute_calls(&mut ctx, assistant, calls, &mut messages)
                .await?
            {
                Flow::Continue | Flow::NextRound => {}
                Flow::Stop(stop) => return Ok(stop),
            }
        }
    }
}

enum KernelRoundError<E> {
    Core(AgentCoreError),
    Host(E),
}
impl<E> From<AgentCoreError> for KernelRoundError<E> {
    fn from(error: AgentCoreError) -> Self {
        Self::Core(error)
    }
}
struct KernelObserver<'a, H> {
    harness: &'a mut H,
    ctx: &'a mut LoopContext,
    pricing: Option<&'a ModelPricing>,
    limits: &'a ModelStepLimits,
}
#[async_trait::async_trait]
impl<H: AgentHarness> ModelRoundObserver for KernelObserver<'_, H> {
    type Error = KernelRoundError<H::Error>;
    async fn before_attempt(&mut self) -> Result<(), Self::Error> {
        self.harness
            .before_model_attempt(self.ctx)
            .await
            .map_err(KernelRoundError::Host)
    }
    fn on_event(&mut self, event: AgentEvent) {
        self.harness.on_event(event);
    }
    async fn on_attempt(
        &mut self,
        mut attempt: leveler_model::ModelAttempt,
    ) -> Result<(), Self::Error> {
        if attempt.cost_usd_micros.is_none() {
            attempt.cost_usd_micros = attempt.usage.and_then(|usage| {
                self.pricing
                    .and_then(|pricing| pricing.cost_for_usage(&usage))
            });
        }
        self.ctx.record_spend(
            attempt.usage.unwrap_or_default(),
            attempt.cost_usd_micros,
            attempt.estimated_tokens,
        );
        self.harness
            .on_model_attempt(self.ctx, &attempt)
            .await
            .map_err(KernelRoundError::Host)?;
        if self.limits.max_cost_usd_micros.is_some() && attempt.cost_usd_micros.is_none() {
            return Err(AgentCoreError::InvalidLimits("provider usage is unavailable; the configured cost budget cannot admit further model calls".into()).into());
        }
        if attempt.error.is_some()
            && (self
                .limits
                .max_model_tokens
                .is_some_and(|cap| self.ctx.model_tokens_spent() >= cap)
                || self
                    .limits
                    .max_cost_usd_micros
                    .is_some_and(|cap| self.ctx.cost_spent_micros() >= cap))
        {
            return Err(AgentCoreError::InvalidLimits(
                "model budget exhausted by a failed attempt; retry refused".into(),
            )
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::BasicHarness;
    use crate::limits::BudgetDimension;
    use crate::tool_runtime::{ToolOutcome, ToolRuntime, ToolRuntimeError};
    use async_trait::async_trait;
    use leveler_core::{RequestId, ToolCallId};
    use leveler_model::{
        ContentPart, FinishReason, ModelError, ModelEventStream, ModelProfile, ModelResponse, Role,
        TokenUsage, ToolCall, ToolDefinition, stream_from_response,
    };
    use std::sync::Mutex;

    /// A model that replays scripted responses in order.
    struct Scripted {
        responses: Mutex<Vec<ModelResponse>>,
        requests: Mutex<Vec<ModelRequest>>,
    }

    impl Scripted {
        fn new(responses: Vec<ModelResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ModelRuntime for Scripted {
        async fn stream(
            &self,
            request: ModelRequest,
            cancellation: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            Ok(stream_from_response(
                self.generate(request, cancellation).await?,
            ))
        }

        async fn generate(
            &self,
            request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<ModelResponse, ModelError> {
            self.requests.lock().unwrap().push(request);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err(ModelError::new(
                    leveler_model::ModelErrorKind::Other,
                    "script exhausted",
                ));
            }
            Ok(responses.remove(0))
        }

        async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
            Err(ModelError::new(
                leveler_model::ModelErrorKind::Other,
                "unused",
            ))
        }
    }

    fn text(t: &str) -> ModelResponse {
        ModelResponse {
            request_id: RequestId::generate(),
            message: Message::text(Role::Assistant, t),
            finish_reason: FinishReason::Stop,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reasoning_tokens: None,
            },
        }
    }

    fn call(name: &str, args: serde_json::Value) -> ModelResponse {
        ModelResponse {
            request_id: RequestId::generate(),
            message: Message {
                origin: None,
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new("c1"),
                        name: name.to_string(),
                        arguments: args,
                    },
                }],
            },
            finish_reason: FinishReason::ToolCalls,
            usage: TokenUsage::default(),
        }
    }

    /// One assistant message carrying two tool calls.
    fn two_calls() -> ModelResponse {
        ModelResponse {
            request_id: RequestId::generate(),
            message: Message {
                origin: None,
                role: Role::Assistant,
                content: vec![
                    ContentPart::ToolCall {
                        call: ToolCall {
                            id: ToolCallId::new("c1"),
                            name: "echo".to_string(),
                            arguments: serde_json::json!({"text": "one"}),
                        },
                    },
                    ContentPart::ToolCall {
                        call: ToolCall {
                            id: ToolCallId::new("c2"),
                            name: "echo".to_string(),
                            arguments: serde_json::json!({"text": "two"}),
                        },
                    },
                ],
            },
            finish_reason: FinishReason::ToolCalls,
            usage: TokenUsage::default(),
        }
    }

    struct Echo;

    #[async_trait]
    impl ToolRuntime for Echo {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "echo".into(),
                description: "echo".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }]
        }

        async fn execute(
            &self,
            call: ToolCall,
            _cancellation: CancellationToken,
        ) -> Result<ToolOutcome, ToolRuntimeError> {
            Ok(ToolOutcome::ok(
                call.arguments["text"].as_str().unwrap_or("").to_string(),
            ))
        }
    }

    fn agent(model: Scripted) -> (Agent, Arc<Scripted>) {
        let model = Arc::new(model);
        (Agent::new(model.clone(), ModelRef::new("mock", "m")), model)
    }

    #[tokio::test]
    async fn request_observations_are_fresh_and_count_against_unreported_usage() {
        struct Observed(BasicHarness<Echo>, u64, Vec<u64>);
        #[async_trait]
        impl AgentHarness for Observed {
            type Stop = crate::stop::LoopStop;
            type Error = AgentCoreError;
            fn on_event(&mut self, event: AgentEvent) {
                if let AgentEvent::ContextUsage(accounting) = event {
                    self.2.push(accounting.used_tokens);
                }
            }
            fn tool_definitions(&self) -> Vec<ToolDefinition> {
                self.0.tool_definitions()
            }
            fn request_context(&self, ctx: &LoopContext) -> leveler_model::ControlContext {
                leveler_model::ControlContext {
                    blocks: vec![leveler_model::PromptSegment::control(
                        "execution_state",
                        leveler_model::PromptSource::ExecutionState,
                        leveler_model::PromptAuthority::RuntimeFact,
                        leveler_model::SegmentLifecycle::RequestEphemeral,
                        false,
                        format!(
                            "observation step={} {}",
                            ctx.model_steps(),
                            "state ".repeat(100)
                        ),
                    )],
                }
            }
            async fn execute_calls(
                &mut self,
                ctx: &mut LoopContext,
                assistant: Message,
                calls: Vec<ToolCall>,
                messages: &mut Vec<Message>,
            ) -> Result<Flow<Self::Stop>, Self::Error> {
                self.0.execute_calls(ctx, assistant, calls, messages).await
            }
            async fn on_stop(
                &mut self,
                ctx: &mut LoopContext,
                stop: Self::Stop,
            ) -> Result<Self::Stop, Self::Error> {
                self.1 = ctx.usage().estimated_model_tokens;
                self.0.on_stop(ctx, stop).await
            }
        }
        let first = call("echo", serde_json::json!({"text":"a"}));
        let first_message = first.message.clone();
        let (agent, model) = agent(Scripted::new(vec![first, text("done")]));
        let mut harness = Observed(BasicHarness::new(Echo), 0, Vec::new());
        let stop = agent
            .run(
                vec![Message::text(Role::User, "go")],
                &mut harness,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for (i, request) in requests.iter().enumerate() {
            assert!(
                !request
                    .messages
                    .iter()
                    .any(|m| m.text_content().starts_with("observation step=")),
                "host control context must never become a conversation message"
            );
            let observations = &request.control_context.blocks;
            assert_eq!(observations.len(), 1);
            assert_eq!(observations[0].name, "execution_state");
            assert!(!observations[0].stable);
            assert!(
                observations[0]
                    .text
                    .starts_with(&format!("observation step={}", i + 1))
            );
            let without_control = RequestProjection::project(
                &request.messages,
                &request.tools,
                ReasoningReplayContract::NONE,
                ReasoningRetention::All,
            );
            assert!(
                request.projection.as_ref().unwrap().estimated_tokens()
                    > without_control.estimated_tokens(),
                "context accounting must include host control blocks"
            );
            let accounting_without_control = ContextAccounting::compute(
                request.model.clone(),
                &without_control,
                None,
                None,
                None,
            );
            assert!(
                harness.2[i] > accounting_without_control.used_tokens,
                "published context usage must include host control blocks"
            );
        }
        assert!(
            !stop
                .messages
                .iter()
                .any(|m| m.text_content().starts_with("observation step="))
        );
        assert_eq!(
            harness.1,
            requests[0]
                .projection
                .as_ref()
                .expect("the kernel projects every request")
                .estimated_tokens()
                + estimate_tokens(&[first_message]),
            "missing provider usage must still bill the entire request projection, \
             tool schemas included"
        );
    }

    /// The retention policy is a REQUEST projection, and on a route that
    /// replays captured reasoning the provider requires every retained turn's
    /// reasoning, so the arm is reported as protocol-protected rather than
    /// applied. The transcript handed back to the caller keeps every block.
    #[tokio::test]
    async fn reasoning_retention_projects_the_request_without_touching_the_transcript() {
        fn reasoning_turn(i: usize) -> Message {
            Message {
                origin: None,
                role: Role::Assistant,
                content: vec![
                    ContentPart::Reasoning {
                        text: format!("r{i}"),
                    },
                    ContentPart::Text {
                        text: format!("t{i}"),
                    },
                ],
            }
        }
        fn tool_turn(i: usize) -> Message {
            Message {
                origin: None,
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    result: leveler_model::ToolResultContent {
                        call_id: ToolCallId::new(format!("c{i}")),
                        content: format!("o{i}"),
                        is_error: false,
                    },
                }],
            }
        }
        fn transcript() -> Vec<Message> {
            let mut messages = vec![Message::text(Role::User, "go")];
            for i in 1..=5 {
                messages.push(reasoning_turn(i));
                messages.push(tool_turn(i));
            }
            messages
        }
        /// Reasoning the provider actually receives, read from the request's
        /// projection — the one owner of that decision.
        fn projected_reasoning(request: &ModelRequest) -> Vec<String> {
            request
                .projection
                .as_ref()
                .expect("the kernel projects every request")
                .messages()
                .iter()
                .filter_map(|m| match &m.reasoning {
                    leveler_model::ProjectedReasoning::Captured(text) => Some(text.clone()),
                    _ => None,
                })
                .collect()
        }

        /// Reasoning the request still carries as its semantic source: the
        /// kernel never rewrites the conversation it was given.
        fn source_reasoning(request: &ModelRequest) -> usize {
            request
                .messages
                .iter()
                .map(|m| {
                    m.content
                        .iter()
                        .filter(|p| matches!(p, ContentPart::Reasoning { .. }))
                        .count()
                })
                .sum()
        }
        fn assistant_texts(request: &ModelRequest) -> Vec<String> {
            request
                .messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .map(|m| m.text_content())
                .collect()
        }

        // A route that replays captured reasoning requires all of it, so the
        // arm's window is reported as protocol-protected, never applied.
        for (policy, expected, protected) in [
            (ReasoningRetention::All, 5, 0),
            (ReasoningRetention::LastTurns(3), 5, 2),
            (ReasoningRetention::None, 5, 5),
        ] {
            let (agent, model) = agent(Scripted::new(vec![text("done")]));
            let agent = agent
                .with_reasoning_replay(leveler_model::ReasoningReplayContract::raw_field(
                    leveler_model::ReasoningReplayScope::Always,
                    leveler_model::MissingReasoningReplay::Omit,
                ))
                .with_reasoning_retention(policy);
            let mut harness = BasicHarness::new(Echo);
            let stop = agent
                .run(transcript(), &mut harness, CancellationToken::new())
                .await
                .unwrap();
            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), 1, "{policy:?}");
            assert_eq!(
                projected_reasoning(&requests[0]).len(),
                expected,
                "{policy:?}: every retained turn's reasoning is replayed"
            );
            assert_eq!(
                requests[0]
                    .projection
                    .as_ref()
                    .expect("the kernel projects every request")
                    .summary()
                    .protocol_protected_turns,
                protected,
                "{policy:?}: the arm's difference is reported, not absorbed"
            );
            assert_eq!(
                source_reasoning(&requests[0]),
                5,
                "{policy:?}: the request's semantic source is never rewritten"
            );
            assert_eq!(
                assistant_texts(&requests[0]),
                vec!["t1", "t2", "t3", "t4", "t5"],
                "{policy:?} must keep every assistant text"
            );
            // The transcript handed back to the caller is untouched: the
            // policy only ever projects the provider request.
            let visible_reasoning: usize = stop
                .messages
                .iter()
                .map(|m| {
                    m.content
                        .iter()
                        .filter(|p| matches!(p, ContentPart::Reasoning { .. }))
                        .count()
                })
                .sum();
            assert_eq!(
                visible_reasoning, 5,
                "{policy:?} must not mutate the transcript"
            );
        }
    }

    /// §A: one logical model request plus the tool batch it produced is exactly
    /// ONE model step, however many calls that batch carried. Two steps here =
    /// the batched request, then the request that ends the run.
    #[tokio::test]
    async fn one_model_step_covers_a_whole_tool_batch() {
        let (agent, _) = agent(Scripted::new(vec![two_calls(), text("done")]));
        let mut harness = BasicHarness::new(Echo);
        let stop = agent
            .run(
                vec![Message::text(Role::User, "go")],
                &mut harness,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            stop.model_steps, 2,
            "a two-call batch is one step, and the closing request is the second"
        );
    }

    #[tokio::test]
    async fn a_tool_result_is_fed_back_in_call_order_and_the_run_ends_where_the_model_stops() {
        let (agent, model) = agent(Scripted::new(vec![
            call("echo", serde_json::json!({"text": "pong"})),
            text("done"),
        ]));
        let mut harness = BasicHarness::new(Echo);
        let stop = agent
            .run(
                vec![Message::text(Role::User, "ping")],
                &mut harness,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(stop.reason, StopReason::ModelEnd);
        assert_eq!(stop.model_steps, 2);
        assert_eq!(stop.last_text, "done");
        // user, assistant(call), tool(result), assistant(done)
        assert_eq!(stop.messages.len(), 4);
        assert_eq!(stop.messages[2].role, Role::Tool);
        assert!(matches!(
            &stop.messages[2].content[0],
            ContentPart::ToolResult { result } if result.content == "pong" && !result.is_error
        ));
        // The second request carried the tool result and advertised the tool.
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].messages.len(), 3);
        assert_eq!(requests[1].tools.len(), 1);
    }

    #[tokio::test]
    async fn the_window_limit_ends_the_run_without_another_model_call() {
        let (agent, model) = agent(Scripted::new(vec![
            call("echo", serde_json::json!({"text": "a"})),
            call("echo", serde_json::json!({"text": "b"})),
            text("never asked"),
        ]));
        let agent = agent.with_limits(ModelStepLimits {
            model_step_window_limit: Some(2),
            ..ModelStepLimits::default()
        });
        let stop = agent
            .run(
                vec![Message::text(Role::User, "go")],
                &mut BasicHarness::new(Echo),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(stop.reason, StopReason::ModelStepWindowLimit { limit: 2 });
        assert_eq!(stop.model_steps, 2);
        assert_eq!(model.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_token_cap_binds_on_reported_usage() {
        let (agent, _) = agent(Scripted::new(vec![text("first"), text("second")]));
        let agent = agent.with_limits(ModelStepLimits {
            max_model_tokens: Some(15),
            ..ModelStepLimits::default()
        });
        // A harness that always asks for another model step, so only the cap ends it.
        struct Again;
        #[async_trait]
        impl AgentHarness for Again {
            type Stop = crate::stop::LoopStop;
            type Error = AgentCoreError;
            fn tool_definitions(&self) -> Vec<ToolDefinition> {
                Vec::new()
            }
            async fn on_quiet(
                &mut self,
                _ctx: &mut LoopContext,
                _assistant: Message,
                messages: &mut Vec<Message>,
            ) -> Result<Flow<Self::Stop>, Self::Error> {
                messages.push(Message::text(Role::User, "more"));
                Ok(Flow::NextRound)
            }
            async fn execute_calls(
                &mut self,
                _ctx: &mut LoopContext,
                _assistant: Message,
                _calls: Vec<ToolCall>,
                _messages: &mut Vec<Message>,
            ) -> Result<Flow<Self::Stop>, Self::Error> {
                unreachable!()
            }
            async fn on_stop(
                &mut self,
                _ctx: &mut LoopContext,
                stop: crate::stop::LoopStop,
            ) -> Result<Self::Stop, Self::Error> {
                Ok(stop)
            }
        }
        let stop = agent
            .run(
                vec![Message::text(Role::User, "go")],
                &mut Again,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        match stop.reason {
            StopReason::BudgetExhausted(e) => {
                assert_eq!(e.dimension, BudgetDimension::ModelTokens);
                assert_eq!(e.spent, 15);
            }
            other => panic!("expected a token budget stop, got {other:?}"),
        }
        assert_eq!(stop.model_steps, 1);
    }

    #[tokio::test]
    async fn missing_usage_is_not_a_free_call_under_a_cost_cap() {
        let mut response = text("answer");
        response.usage = TokenUsage::default();
        let (agent, model) = agent(Scripted::new(vec![response]));
        let agent = agent
            .with_pricing(Some(ModelPricing {
                input_usd_per_mtok: 1.0,
                output_usd_per_mtok: 2.0,
                cached_input_usd_per_mtok: None,
            }))
            .with_limits(ModelStepLimits {
                max_cost_usd_micros: Some(1000),
                ..Default::default()
            });
        let error = agent
            .run(
                vec![Message::text(Role::User, "go")],
                &mut BasicHarness::new(Echo),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AgentCoreError::InvalidLimits(_)));
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn a_cost_cap_without_pricing_is_refused_before_any_model_call() {
        let (agent, model) = agent(Scripted::new(vec![text("x")]));
        let agent = agent.with_limits(ModelStepLimits {
            max_cost_usd_micros: Some(1),
            ..ModelStepLimits::default()
        });
        let err = agent
            .run(
                vec![Message::text(Role::User, "go")],
                &mut BasicHarness::new(Echo),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AgentCoreError::InvalidLimits(_)), "{err}");
        assert!(model.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_external_cancel_ends_the_run_as_cancelled() {
        let (agent, _) = agent(Scripted::new(vec![text("x")]));
        let token = CancellationToken::new();
        token.cancel();
        let stop = agent
            .run(
                vec![Message::text(Role::User, "go")],
                &mut BasicHarness::new(Echo),
                token,
            )
            .await
            .unwrap();
        assert_eq!(stop.reason, StopReason::Cancelled);
    }
}
