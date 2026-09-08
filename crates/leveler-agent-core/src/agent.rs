//! The one model↔tool loop.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_model::{
    Message, ModelPricing, ModelRef, ModelRequest, ModelRuntime, ReasoningEffort, ToolChoice,
};

use crate::error::AgentCoreError;
use crate::harness::{AgentHarness, Flow, LoopContext};
use crate::limits::{RoundAdmission, RoundLimits};
use crate::model_round::run_model_round;
use crate::stop::StopReason;
use crate::usage::estimate_tokens;

/// A model, the request shape every round uses, and the mechanical limits
/// the loop enforces. Everything else about a run comes from the
/// [`AgentHarness`] handed to [`Agent::run`].
pub struct Agent {
    runtime: Arc<dyn ModelRuntime>,
    model: ModelRef,
    max_output_tokens: Option<u32>,
    reasoning_effort: Option<ReasoningEffort>,
    pricing: Option<ModelPricing>,
    limits: RoundLimits,
}

impl Agent {
    pub fn new(runtime: Arc<dyn ModelRuntime>, model: ModelRef) -> Self {
        Self {
            runtime,
            model,
            max_output_tokens: None,
            reasoning_effort: None,
            pricing: None,
            limits: RoundLimits::default(),
        }
    }

    pub fn with_limits(mut self, limits: RoundLimits) -> Self {
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

    /// Price every round's usage so a cost cap can bind. Required when
    /// [`RoundLimits::max_cost_usd_micros`] is set.
    pub fn with_pricing(mut self, pricing: Option<ModelPricing>) -> Self {
        self.pricing = pricing;
        self
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

    pub fn limits(&self) -> &RoundLimits {
        &self.limits
    }

    /// Run the loop over `messages` until the model stops, the harness stops
    /// it, or a limit fires.
    ///
    /// ```text
    /// loop {
    ///     harness.on_round_start
    ///     admit next round        (limits, cancellation, deadline)
    ///     harness.on_round_admitted
    ///     model round             (stream, retry, fold spend)
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
            // Applied in two phases because the round counter advances between
            // them, and what a stop reports depends on which side of that it
            // falls: a spent budget names the rounds that COMPLETED, while the
            // cancel and deadline paths report the round they were about to
            // start.
            let verdict = ctx.admit();
            match &verdict {
                RoundAdmission::StopBudget(exhaustion)
                    if exhaustion.dimension != crate::limits::BudgetDimension::Duration =>
                {
                    let stop = ctx.stop(
                        StopReason::BudgetExhausted(exhaustion.clone()),
                        ctx.round(),
                        messages,
                    );
                    return harness.on_stop(&mut ctx, stop).await;
                }
                RoundAdmission::StopRoundCeiling { ceiling } => {
                    let stop = ctx.stop(
                        StopReason::RoundCeiling { ceiling: *ceiling },
                        ctx.round(),
                        messages,
                    );
                    return harness.on_stop(&mut ctx, stop).await;
                }
                RoundAdmission::StopWindowLimit => {
                    let limit = self
                        .limits
                        .window_round_limit
                        .expect("a window limit only fires when one is pinned");
                    let stop = ctx.stop(StopReason::WindowLimit { limit }, ctx.round(), messages);
                    return harness.on_stop(&mut ctx, stop).await;
                }
                _ => {}
            }
            ctx.advance_round();
            match verdict {
                RoundAdmission::Cancelled => {
                    let stop = ctx.stop(StopReason::Cancelled, ctx.round(), messages);
                    return harness.on_stop(&mut ctx, stop).await;
                }
                // Only the duration dimension reaches here: the token and cost
                // caps returned in the phase above.
                RoundAdmission::StopBudget(exhaustion) => {
                    let rounds = ctx.round().saturating_sub(1);
                    let stop = ctx.stop(StopReason::BudgetExhausted(exhaustion), rounds, messages);
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
            request.tools = harness.tool_definitions();
            request.tool_choice = ToolChoice::Auto;
            request.max_output_tokens = self.max_output_tokens;
            request.reasoning_effort = self.reasoning_effort;

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
                // round top, where admission reports the duration budget.
                Err(AgentCoreError::Cancelled) if ctx.deadline_expired() => continue,
                Err(error) => match harness
                    .on_model_error(&mut ctx, error, &mut messages)
                    .await?
                {
                    Flow::Continue | Flow::NextRound => continue,
                    Flow::Stop(stop) => return Ok(stop),
                },
            };

            // Fold this round's spend once, here, against the usage the
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
                estimate_tokens(&messages)
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
                        let stop = ctx.stop(StopReason::ModelEnd, ctx.round(), messages);
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
        assert_eq!(stop.rounds, 2);
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
        let agent = agent.with_limits(RoundLimits {
            window_round_limit: Some(2),
            ..RoundLimits::default()
        });
        let stop = agent
            .run(
                vec![Message::text(Role::User, "go")],
                &mut BasicHarness::new(Echo),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(stop.reason, StopReason::WindowLimit { limit: 2 });
        assert_eq!(stop.rounds, 2);
        assert_eq!(model.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_token_cap_binds_on_reported_usage() {
        let (agent, _) = agent(Scripted::new(vec![text("first"), text("second")]));
        let agent = agent.with_limits(RoundLimits {
            max_model_tokens: Some(15),
            ..RoundLimits::default()
        });
        // A harness that always asks for another round, so only the cap ends it.
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
        assert_eq!(stop.rounds, 1);
    }

    #[tokio::test]
    async fn a_cost_cap_without_pricing_is_refused_before_any_model_call() {
        let (agent, model) = agent(Scripted::new(vec![text("x")]));
        let agent = agent.with_limits(RoundLimits {
            max_cost_usd_micros: Some(1),
            ..RoundLimits::default()
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
