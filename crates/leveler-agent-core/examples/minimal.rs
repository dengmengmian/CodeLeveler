//! The smallest embedding of the agent kernel: a deterministic in-process
//! model and two trivial tools, no CodeLeveler harness, no repository, no
//! storage, no configuration.
//!
//! ```text
//! cargo run -p leveler-agent-core --example minimal
//! ```
//!
//! The model is scripted so the run is reproducible; the point is to prove
//! that `leveler-agent-core` drives a complete model↔tool loop on its own.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent_core::{
    Agent, AgentEvent, BasicHarness, RoundLimits, ToolOutcome, ToolRuntime, ToolRuntimeError,
};
use leveler_core::{RequestId, ToolCallId};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelErrorKind, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
    ToolDefinition, stream_from_response,
};

/// A model that answers from a script: two tool calls, then a final answer.
struct ScriptedModel {
    turn: Mutex<u32>,
}

impl ScriptedModel {
    fn response(turn: u32) -> ModelResponse {
        let usage = TokenUsage {
            input_tokens: 120 + u64::from(turn) * 40,
            output_tokens: 20,
            cached_input_tokens: 0,
        };
        let (content, finish_reason) = match turn {
            0 => (
                vec![ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new("call-1"),
                        name: "calculator".into(),
                        arguments: serde_json::json!({"expression": "6 * 7"}),
                    },
                }],
                FinishReason::ToolCalls,
            ),
            1 => (
                vec![ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new("call-2"),
                        name: "echo".into(),
                        arguments: serde_json::json!({"text": "the answer is 42"}),
                    },
                }],
                FinishReason::ToolCalls,
            ),
            _ => (
                vec![ContentPart::Text {
                    text: "6 × 7 = 42, and the echo tool confirmed it.".into(),
                }],
                FinishReason::Stop,
            ),
        };
        ModelResponse {
            request_id: RequestId::generate(),
            message: Message {
                role: Role::Assistant,
                content,
            },
            finish_reason,
            usage,
        }
    }
}

#[async_trait]
impl ModelRuntime for ScriptedModel {
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
        let mut turn = self.turn.lock().expect("script lock");
        println!(
            "[model] request {} with {} message(s), {} tool(s) advertised",
            *turn + 1,
            request.messages.len(),
            request.tools.len()
        );
        let response = Self::response(*turn);
        *turn += 1;
        Ok(response)
    }

    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Err(ModelError::new(
            ModelErrorKind::Other,
            "the scripted model has no profile",
        ))
    }
}

/// Two deterministic tools behind the kernel's tool boundary.
struct DemoTools;

impl DemoTools {
    fn calculate(expression: &str) -> Result<i64, String> {
        let mut parts = expression.split_whitespace();
        let (Some(a), Some(op), Some(b), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(format!("expected `<a> <op> <b>`, got `{expression}`"));
        };
        let a: i64 = a.parse().map_err(|_| format!("not a number: {a}"))?;
        let b: i64 = b.parse().map_err(|_| format!("not a number: {b}"))?;
        match op {
            "+" => Ok(a + b),
            "-" => Ok(a - b),
            "*" => Ok(a * b),
            "/" if b != 0 => Ok(a / b),
            "/" => Err("division by zero".into()),
            other => Err(format!("unknown operator: {other}")),
        }
    }
}

#[async_trait]
impl ToolRuntime for DemoTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "calculator".into(),
                description: "Evaluate `<a> <op> <b>` over integers.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"expression": {"type": "string"}},
                    "required": ["expression"]
                }),
            },
            ToolDefinition {
                name: "echo".into(),
                description: "Return the given text unchanged.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }),
            },
        ]
    }

    async fn execute(
        &self,
        call: ToolCall,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let outcome = match call.name.as_str() {
            "calculator" => {
                let expression = call.arguments["expression"].as_str().unwrap_or("");
                match Self::calculate(expression) {
                    Ok(value) => ToolOutcome::ok(value.to_string()),
                    Err(error) => ToolOutcome::error(error),
                }
            }
            "echo" => ToolOutcome::ok(call.arguments["text"].as_str().unwrap_or("").to_string()),
            other => ToolOutcome::error(format!("unknown tool: {other}")),
        };
        Ok(outcome)
    }
}

#[tokio::main]
async fn main() {
    let agent = Agent::new(
        Arc::new(ScriptedModel {
            turn: Mutex::new(0),
        }),
        ModelRef::new("scripted", "demo"),
    )
    .with_limits(RoundLimits {
        window_round_limit: Some(8),
        ..RoundLimits::default()
    });

    let mut harness = BasicHarness::new(DemoTools).with_events(|event| match event {
        AgentEvent::ToolCallStarted {
            name, arguments, ..
        } => println!("[tool] {name}({arguments})"),
        AgentEvent::ToolCallFinished {
            name,
            is_error,
            preview,
            ..
        } => println!(
            "[tool] {name} → {preview}{}",
            if is_error { " (error)" } else { "" }
        ),
        AgentEvent::AssistantDelta(delta) => println!("[assistant] {delta}"),
        AgentEvent::Usage(usage) => println!(
            "[usage] in={} out={}",
            usage.input_tokens, usage.output_tokens
        ),
        AgentEvent::StreamAttemptStarted | AgentEvent::ReasoningDelta(_) => {}
    });

    let stop = agent
        .run(
            vec![Message::text(
                Role::User,
                "What is 6 times 7? Confirm with echo.",
            )],
            &mut harness,
            CancellationToken::new(),
        )
        .await
        .expect("the scripted run cannot fail");

    println!();
    println!("stop reason: {:?}", stop.reason);
    println!("rounds: {}", stop.rounds);
    println!("transcript: {} message(s)", stop.messages.len());
    println!("final answer: {}", stop.last_text);
}
