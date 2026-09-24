//! The one model↔tool loop.

use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use leveler_model::{
    CompactionRecord, ContextAccounting, Message, ModelPricing, ModelRef, ModelRequest,
    ModelRuntime, ReasoningEffort, ToolChoice,
};

use crate::error::AgentCoreError;
use crate::event::AgentEvent;
use crate::harness::{AgentHarness, Flow, LoopContext};
use crate::limits::{ModelStepAdmission, ModelStepLimits};
use crate::model_round::run_model_round;
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
    pricing: Option<ModelPricing>,
    limits: ModelStepLimits,
    /// The model's declared context window (exact fact), when the host knows it.
    context_window: Option<u32>,
    /// The fold threshold (`reliable_context`), when the host knows it.
    compact_at: Option<u32>,
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
            pricing: None,
            limits: ModelStepLimits::default(),
            context_window: None,
            compact_at: None,
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

    /// Declare the fold threshold (`reliable_context`) the harness folds at.
    pub fn with_compact_at(mut self, compact_at: u32) -> Self {
        self.compact_at = Some(compact_at);
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

            let mut request = ModelRequest::new(self.model.clone(), messages.clone());
            request.messages.extend(harness.request_context(&ctx));
            request.tools = harness.tool_definitions();
            request.tool_choice = ToolChoice::Auto;
            request.max_output_tokens = self.max_output_tokens;
            request.reasoning_effort = self.reasoning_effort;

            // Publish the accounting of the EXACT request about to be sent,
            // before it is sent. This is the runtime's single source of truth
            // for "what is my context made of"; the TUI only renders it.
            let accounting = ContextAccounting::compute(
                self.model.clone(),
                &request.messages,
                &request.tools,
                self.context_window,
                self.compact_at,
                self.compaction.lock().ok().and_then(|g| *g),
            );
            harness.on_event(AgentEvent::ContextUsage(accounting));

            let estimated_request_tokens = estimate_tokens(&request.messages);

            let mut on_event = |event| harness.on_event(event);
            let round = run_model_round(
                self.runtime.as_ref(),
                request,
                ctx.cancellation(),
                &mut on_event,
            )
            .await;
            let mut round = match round {
                Ok(round) => round,
                // The deadline timer cancelled the stream: re-enter at the
                // step top, where admission reports the duration budget.
                Err(AgentCoreError::Cancelled) if ctx.deadline_expired() => continue,
                Err(error) => match harness
                    .on_model_error(&mut ctx, error, &mut messages)
                    .await?
                {
                    Flow::Continue | Flow::NextRound => continue,
                    Flow::Stop(stop) => return Ok(stop),
                },
            };

            // Fold this model step's spend once, here, against the usage the
            // provider reported — cached share included. A zero-usage gateway
            // must not disable the token budget, so the transcript estimate
            // stands in (request + response, mirroring what is billed).
            round.cost_usd_micros = self.pricing.as_ref().map(|p| {
                p.cost_usd_micros_cached(
                    round.usage.input_tokens,
                    round.usage.cached_input_tokens,
                    round.usage.output_tokens,
                )
            });
            round.estimated_tokens = (round.usage.total() == 0).then(|| {
                estimated_request_tokens
                    .saturating_add(estimate_tokens(std::slice::from_ref(&round.message)))
            });
            ctx.record_spend(round.usage, round.cost_usd_micros, round.estimated_tokens);

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
                reasoning_tokens: None,
            },
        }
    }

    fn call(name: &str, args: serde_json::Value) -> ModelResponse {
        ModelResponse {
            request_id: RequestId::generate(),
            message: Message {
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
        struct Observed(BasicHarness<Echo>, u64);
        #[async_trait]
        impl AgentHarness for Observed {
            type Stop = crate::stop::LoopStop;
            type Error = AgentCoreError;
            fn tool_definitions(&self) -> Vec<ToolDefinition> {
                self.0.tool_definitions()
            }
            fn request_context(&self, ctx: &LoopContext) -> Vec<Message> {
                vec![Message::text(
                    Role::System,
                    format!(
                        "observation step={} {}",
                        ctx.model_steps(),
                        "state ".repeat(100)
                    ),
                )]
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
        let mut harness = Observed(BasicHarness::new(Echo), 0);
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
            let observations: Vec<_> = request
                .messages
                .iter()
                .filter(|m| m.text_content().starts_with("observation step="))
                .collect();
            assert_eq!(observations.len(), 1);
            assert!(
                observations[0]
                    .text_content()
                    .starts_with(&format!("observation step={}", i + 1))
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
            estimate_tokens(&requests[0].messages) + estimate_tokens(&[first_message]),
            "missing provider usage must still bill the entire request projection"
        );
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
