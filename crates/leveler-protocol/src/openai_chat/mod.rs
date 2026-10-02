//! The OpenAI Chat Completions protocol adapter . DeepSeek and
//! GLM both speak this protocol, so it is reused rather than duplicated.

mod stream;
mod wire;

use async_stream::stream;
use futures::StreamExt;

use leveler_model::{
    ContentPart, FinishReason, ImageSource, Message, ModelError, ModelErrorKind, ModelEvent,
    ModelRequest, ModelResponse, ProtocolAdapter, ProtocolContext, ProtocolError, ProtocolKind,
    RawByteStream, ReasoningStyle, RequestProjection, Role, TokenUsage, ToolCall, ToolChoice,
};

use crate::sse::SseDecoder;
use stream::{ChatStreamAssembler, map_finish_reason};
use wire::{
    ChatFunctionCall, ChatFunctionDef, ChatMessage, ChatRequest, ChatResponse, ChatTool,
    ChatToolCall, StreamOptions, Thinking,
};

use leveler_model::EncodedRequest;

/// Stateless adapter for the OpenAI Chat Completions protocol.
#[derive(Debug, Clone, Default)]
pub struct OpenAiChatAdapter;

impl OpenAiChatAdapter {
    /// The adapter carries no state; every instance is interchangeable.
    pub fn new() -> Self {
        Self
    }
}

impl ProtocolAdapter for OpenAiChatAdapter {
    fn protocol(&self) -> ProtocolKind {
        ProtocolKind::OpenAiChat
    }

    fn encode_request(
        &self,
        request: &ModelRequest,
        context: &ProtocolContext,
        stream: bool,
    ) -> Result<EncodedRequest, ProtocolError> {
        let projection = RequestProjection::for_request(request, context.reasoning_replay);
        let messages = convert_messages(&projection);

        let tools = request
            .tools
            .iter()
            .map(|t| ChatTool {
                kind: "function".to_string(),
                function: ChatFunctionDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect();

        let tool_choice = convert_tool_choice(&request.tool_choice);

        // A reasoning request is spelled differently per provider; the model
        // profile picks the spelling. `None` sends neither field, so a provider
        // that rejects them is unaffected.
        // Effective effort is resolved before encode. The adapter does not
        // fall back to the profile or remap levels.
        let effort = request.reasoning_effort;
        let (thinking, reasoning_effort) = if request.thinking_disabled {
            // An explicit `off` from the canonical Thinking Level. Only the
            // thinking-flag style has a word for it; the others have no way to
            // say "no thinking", which is why the canonical layer does not
            // offer `off` on them in the first place.
            match context.reasoning.style {
                ReasoningStyle::ThinkingFlag => (Some(Thinking { kind: "disabled" }), None),
                ReasoningStyle::OpenAiEffort | ReasoningStyle::None => (None, None),
                ReasoningStyle::AdaptiveThinking | ReasoningStyle::BudgetedThinking { .. } => {
                    return Err(ProtocolError::Encode(
                        "thinking style requires a Messages protocol route".into(),
                    ));
                }
            }
        } else if !context.thinking_supports_forced_tool_choice
            && request.tool_choice.forces_tool_call()
        {
            // The provider rejects a forced tool_choice while thinking, and for
            // these providers thinking is the server-side default — omission
            // does not avoid the rejection. Keep the ToolChoice contract and
            // disable thinking explicitly for exactly this request; effort is a
            // thinking knob, so it is dropped with it.
            (Some(Thinking { kind: "disabled" }), None)
        } else {
            match context.reasoning.style {
                ReasoningStyle::AdaptiveThinking | ReasoningStyle::BudgetedThinking { .. } => {
                    return Err(ProtocolError::Encode(
                        "thinking style requires a Messages protocol route".into(),
                    ));
                }
                ReasoningStyle::None => (None, None),
                ReasoningStyle::OpenAiEffort => (None, effort.map(|e| e.as_wire().to_string())),
                ReasoningStyle::ThinkingFlag => (
                    Some(Thinking { kind: "enabled" }),
                    effort.map(|e| e.as_wire().to_string()),
                ),
            }
        };

        let chat = ChatRequest {
            model: context.model_id.clone(),
            messages,
            tools,
            tool_choice,
            max_tokens: request.max_output_tokens,
            // Omitted entirely for providers that reject a caller-chosen value;
            // coercing it to their default would silently ignore the caller's
            // request for determinism.
            temperature: context
                .supports_temperature
                .then_some(request.temperature)
                .flatten(),
            stop: request.stop.clone(),
            stream,
            stream_options: stream.then_some(StreamOptions {
                include_usage: true,
            }),
            thinking,
            reasoning_effort,
            // Only constrain when the model can't parallelize; otherwise leave it
            // to the provider default (sending nothing).
            parallel_tool_calls: (!context.parallel_tool_calls).then_some(false),
        };

        let body = serde_json::to_value(&chat)
            .map_err(|e| ProtocolError::Encode(format!("serialize chat request: {e}")))?;

        Ok(EncodedRequest {
            path: "/chat/completions".to_string(),
            body,
            headers: Vec::new(),
        })
    }

    fn decode_response(
        &self,
        body: &[u8],
        _context: &ProtocolContext,
    ) -> Result<ModelResponse, ProtocolError> {
        let resp: ChatResponse = serde_json::from_slice(body)
            .map_err(|e| ProtocolError::Decode(format!("parse chat response: {e}")))?;

        let choice = resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ProtocolError::Decode("response had no choices".into()))?;

        let msg = choice.message.unwrap_or(wire::RespMessage {
            content: None,
            reasoning_content: None,
            tool_calls: Vec::new(),
        });

        let mut content = Vec::new();
        if let Some(reasoning) = msg.reasoning_content.filter(|s| !s.is_empty()) {
            content.push(ContentPart::Reasoning { text: reasoning });
        }
        if let Some(text) = msg.content.filter(|s| !s.is_empty()) {
            content.push(ContentPart::Text { text });
        }
        for (i, tc) in msg.tool_calls.into_iter().enumerate() {
            let func = tc.function.unwrap_or(wire::RespFunction {
                name: None,
                arguments: None,
            });
            let name = func.name.unwrap_or_default();
            let arguments = match func.arguments {
                Some(a) if !a.trim().is_empty() => serde_json::from_str(&a).map_err(|e| {
                    ProtocolError::Decode(format!("tool call arguments invalid JSON: {e}"))
                })?,
                _ => serde_json::Value::Object(Default::default()),
            };
            let id = tc
                .id
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("call_{i}"));
            content.push(ContentPart::ToolCall {
                call: ToolCall {
                    id: leveler_core::ToolCallId::new(id),
                    name,
                    arguments,
                },
            });
        }

        let finish_reason = choice
            .finish_reason
            .as_deref()
            .map(map_finish_reason)
            .unwrap_or(FinishReason::Stop);

        let usage = resp
            .usage
            .map(|u| TokenUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                cached_input_tokens: u.cached_input_tokens(),
                cache_creation_input_tokens: 0,
                reasoning_tokens: u.reasoning_tokens(),
            })
            .unwrap_or_default();

        Ok(ModelResponse {
            request_id: request_id_from(&resp.id),
            message: Message {
                origin: None,
                role: Role::Assistant,
                content,
            },
            finish_reason,
            usage,
        })
    }

    fn decode_stream(
        &self,
        mut raw: RawByteStream,
        _context: &ProtocolContext,
    ) -> Result<leveler_model::ModelEventStream, ProtocolError> {
        let out = stream! {
                   let mut decoder = SseDecoder::new();
                   let mut assembler = ChatStreamAssembler::new();

                   while let Some(item) = raw.next().await {
                       match item {
                           Ok(bytes) => {
                               let events = match decoder.try_feed(&bytes) {
                                   Ok(events) => events,
                                   Err(error) => {
                                       yield Ok(ModelEvent::Error {
                                           error: ModelError::new(
                                               ModelErrorKind::Decode,
                                               format!("malformed SSE stream: {error}"),
                                           ),
                                       });
                                       return;
                                   }
                               };
                               for sse in events {
                                   let data = sse.data.trim();
                                   if data.is_empty() {
                                       continue;
                                   }
                                   if data == "[DONE]" {
                                       for ev in assembler.on_done() {
                                           yield Ok(ev);
                                       }
                                       if !assembler.is_completed() {
                                           yield Ok(ModelEvent::Error {
                                               error: ModelError::new(
                                                   ModelErrorKind::StreamInterrupted,
                                                   "stream ended before completion",
                                               ),
                                           });
                                       }
                                       return;
                                   }
                                   match serde_json::from_str::<wire::ChatChunk>(data) {
                                       Ok(chunk) => {
                                           for ev in assembler.on_chunk(chunk) {
                                               yield Ok(ev);
                                           }
                                       }
                                       Err(e) => {
                                           yield Ok(ModelEvent::Error {
                                               error: ModelError::new(
                                                   ModelErrorKind::Decode,
                                                   format!("malformed stream chunk: {e}"),
                                               ),
                                           });
                                       }
                                   }
                               }
                           }
                           Err(err) => {
        // Transport-level failure mid-stream.
                               yield Ok(ModelEvent::Error { error: err });
                               return;
                           }
                       }
                   }

        // If the transport ended without a terminal event, surface an
        // interrupted-stream error so recovery can retry .
                   if !assembler.is_completed() {
                       yield Ok(ModelEvent::Error {
                           error: ModelError::new(
                               ModelErrorKind::StreamInterrupted,
                               "stream ended before completion",
                           ),
                       });
                   }
               };

        Ok(Box::pin(out))
    }
}

/// Convert a provider-visible projection to OpenAI chat messages.
///
/// This function carries NO replay policy: whether a turn's reasoning reaches
/// the wire was decided by [`leveler_model::RequestProjection`], and what this
/// encoder does is spell the decision.
///
/// The control channel has two positions here, and which block goes where was
/// decided by the projection:
///
/// ```text
/// [system: stable control][transcript...][system: single-request control]
/// ```
///
/// Both are `system` messages, so authority is unchanged. The single-request
/// blocks trail the transcript because they are rebuilt every round: ahead of
/// the history they would break the provider's prefix cache on the first
/// differing byte, which is the whole transcript.
fn convert_messages(projection: &RequestProjection) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    let control = projection.control_prefix_text();
    if !control.is_empty() {
        out.push(system_message(control));
    }
    for msg in projection.messages() {
        // Tool-result messages map to one `role: tool` message per result.
        if msg.role == Role::Tool {
            for part in &msg.content {
                if let ContentPart::ToolResult { result } = part {
                    out.push(ChatMessage {
                        role: "tool".to_string(),
                        content: Some(wire::ChatContent::Text(result.content.clone())),
                        reasoning_content: None,
                        tool_calls: Vec::new(),
                        tool_call_id: Some(result.call_id.to_string()),
                    });
                }
            }
            continue;
        }

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut images: Vec<wire::ChatImageUrl> = Vec::new();
        for part in &msg.content {
            match part {
                ContentPart::Text { text: t } => text.push_str(t),
                ContentPart::ToolCall { call } => tool_calls.push(ChatToolCall {
                    id: call.id.to_string(),
                    kind: "function".to_string(),
                    function: ChatFunctionCall {
                        name: call.name.clone(),
                        arguments: call.arguments.to_string(),
                    },
                }),
                ContentPart::Image { source } => images.push(wire::ChatImageUrl {
                    url: image_url(source),
                }),
                _ => {}
            }
        }

        let reasoning_content = msg.reasoning.as_wire().map(str::to_string);

        // Use the multimodal array form only when images are present.
        let content = if images.is_empty() {
            (!text.is_empty()).then_some(wire::ChatContent::Text(text))
        } else {
            let mut parts = Vec::new();
            if !text.is_empty() {
                parts.push(wire::ChatContentPart::Text { text });
            }
            for image_url in images {
                parts.push(wire::ChatContentPart::ImageUrl { image_url });
            }
            Some(wire::ChatContent::Parts(parts))
        };

        out.push(ChatMessage {
            role: role_str(msg.role).to_string(),
            content,
            reasoning_content,
            tool_calls,
            tool_call_id: None,
        });
    }

    // Attached after the transcript, never before it: see the doc comment.
    let trailing = projection.control_trailing_text();
    if !trailing.is_empty() {
        out.push(system_message(trailing));
    }
    out
}

fn system_message(text: String) -> ChatMessage {
    ChatMessage {
        role: "system".to_string(),
        content: Some(wire::ChatContent::Text(text)),
        reasoning_content: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
    }
}

/// Render an image source as an OpenAI `image_url` value (URL or data URI).
fn image_url(source: &ImageSource) -> String {
    match source {
        ImageSource::Url { url } => url.clone(),
        ImageSource::Base64 { media_type, data } => {
            format!("data:{media_type};base64,{data}")
        }
    }
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn convert_tool_choice(choice: &ToolChoice) -> Option<serde_json::Value> {
    match choice {
        ToolChoice::Auto => None,
        ToolChoice::None => Some(serde_json::Value::String("none".into())),
        ToolChoice::Required => Some(serde_json::Value::String("required".into())),
        ToolChoice::Tool(name) => Some(serde_json::json!({
            "type": "function",
            "function": { "name": name }
        })),
    }
}

fn request_id_from(id: &Option<String>) -> leveler_core::RequestId {
    match id {
        Some(s) if !s.is_empty() => leveler_core::RequestId::new(s.clone()),
        _ => leveler_core::RequestId::generate(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_model::{
        ModelRef, ReasoningConfig, ReasoningEffort, ReasoningStyle, ToolCall, ToolDefinition,
        ToolResultContent,
    };

    /// The DeepSeek-shaped route contract: captured reasoning is carried on
    /// tool-bearing requests, and a turn that captured none still carries the
    /// empty key.
    fn reasoning_carrying_contract() -> leveler_model::ReasoningReplayContract {
        leveler_model::ReasoningReplayContract::raw_field(
            leveler_model::ReasoningReplayScope::WhenToolsPresent,
            leveler_model::MissingReasoningReplay::EmptyString,
        )
    }

    /// The same scope without the empty-key requirement.
    fn reasoning_carrying_without_empty_key() -> leveler_model::ReasoningReplayContract {
        leveler_model::ReasoningReplayContract::raw_field(
            leveler_model::ReasoningReplayScope::WhenToolsPresent,
            leveler_model::MissingReasoningReplay::Omit,
        )
    }

    fn ctx() -> ProtocolContext {
        ProtocolContext {
            base_url: "https://api.deepseek.com".into(),
            model_id: "deepseek-chat".into(),
            api_key: Some("secret".into()),
            extra_headers: vec![],
            reasoning: ReasoningConfig::default(),
            parallel_tool_calls: true,
            supports_temperature: true,
            thinking_supports_forced_tool_choice: true,
            reasoning_replay: leveler_model::ReasoningReplayContract::NONE,
        }
    }

    fn ctx_reasoning(style: ReasoningStyle, effort: Option<ReasoningEffort>) -> ProtocolContext {
        ProtocolContext {
            reasoning: ReasoningConfig {
                style,
                supported_efforts: effort.into_iter().collect(),
                default_effort: effort,
            },
            ..ctx()
        }
    }

    fn encode_with(context: &ProtocolContext) -> serde_json::Value {
        let mut req = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-chat"),
            vec![Message::text(Role::User, "hi")],
        );
        req.reasoning_effort = context.reasoning.default_effort;
        OpenAiChatAdapter::new()
            .encode_request(&req, context, true)
            .unwrap()
            .body
    }

    fn encode_with_temperature(context: &ProtocolContext, temperature: f32) -> serde_json::Value {
        let mut req = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-chat"),
            vec![Message::text(Role::User, "hi")],
        );
        req.temperature = Some(temperature);
        OpenAiChatAdapter::new()
            .encode_request(&req, context, true)
            .unwrap()
            .body
    }

    #[test]
    fn temperature_is_sent_when_the_provider_accepts_it() {
        let body = encode_with_temperature(&ctx(), 0.0);
        assert_eq!(body["temperature"], 0.0);
    }

    #[test]
    fn temperature_is_dropped_when_the_provider_rejects_it() {
        // Kimi For Coding rejects any temperature but 1 outright ("invalid
        // temperature: only 1 is allowed for this model"), so callers that ask
        // for a deterministic 0.0 must not have it forwarded.
        let context = ProtocolContext {
            supports_temperature: false,
            ..ctx()
        };
        let body = encode_with_temperature(&context, 0.0);
        assert!(
            body.get("temperature").is_none(),
            "temperature must be omitted, not coerced: {body}"
        );
    }

    #[test]
    fn reasoning_style_none_sends_nothing() {
        let body = encode_with(&ctx());
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn parallel_capable_model_omits_the_flag() {
        // ctx() has parallel_tool_calls: true → leave it to the provider default.
        let body = encode_with(&ctx());
        assert!(body.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn non_parallel_model_disables_parallel_tool_calls() {
        let context = ProtocolContext {
            parallel_tool_calls: false,
            ..ctx()
        };
        let body = encode_with(&context);
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn thinking_flag_style_enables_thinking_and_sends_effort() {
        let body = encode_with(&ctx_reasoning(
            ReasoningStyle::ThinkingFlag,
            Some(ReasoningEffort::High),
        ));
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn thinking_flag_without_effort_omits_effort() {
        let body = encode_with(&ctx_reasoning(ReasoningStyle::ThinkingFlag, None));
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    /// Encode with tools attached and a specific tool choice — the shape the
    /// executor sends on plan-repair rounds (forced `update_plan`).
    fn encode_with_choice(context: &ProtocolContext, choice: ToolChoice) -> serde_json::Value {
        let mut req = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-chat"),
            vec![Message::text(Role::User, "hi")],
        );
        req.reasoning_effort = context.reasoning.default_effort;
        req.tools = vec![ToolDefinition {
            name: "update_plan".into(),
            description: "plan".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        req.tool_choice = choice;
        OpenAiChatAdapter::new()
            .encode_request(&req, context, true)
            .unwrap()
            .body
    }

    /// The wire contract: a tool's published `required` list reaches the
    /// provider verbatim. If a conversion step ever drops or rewrites
    /// `required`, a schema fix in the tool crate silently stops reaching the
    /// model — exactly the run_command failure mode.
    #[test]
    fn tool_schema_required_reaches_the_outbound_request_verbatim() {
        let mut req = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-chat"),
            vec![Message::text(Role::User, "hi")],
        );
        req.tools = vec![ToolDefinition {
            name: "run_command".into(),
            description: "run".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "program": {"type": ["string", "null"]},
                    "args": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["program"]
            }),
        }];
        let body = OpenAiChatAdapter::new()
            .encode_request(&req, &ctx(), true)
            .unwrap()
            .body;
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["required"],
            serde_json::json!(["program"]),
            "outbound parameters must carry required verbatim"
        );
    }

    #[test]
    fn thinking_model_with_auto_choice_keeps_thinking_and_omits_tool_choice() {
        let context = ctx_reasoning(ReasoningStyle::ThinkingFlag, Some(ReasoningEffort::Max));
        let body = encode_with_choice(&context, ToolChoice::Auto);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "max");
        assert!(body.get("tool_choice").is_none());
        assert_eq!(body["tools"][0]["function"]["name"], "update_plan");
    }

    #[test]
    fn non_reasoning_model_sends_required_choice_without_thinking() {
        let body = encode_with_choice(&ctx(), ToolChoice::Required);
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["tool_choice"], "required");
    }

    #[test]
    fn incompatible_provider_disables_thinking_on_required_choice() {
        // DeepSeek rejects forced tool_choice in thinking mode, and thinking is
        // its server-side default — so the adapter must send an explicit
        // disable, never downgrade the forced choice to auto.
        let context = ProtocolContext {
            thinking_supports_forced_tool_choice: false,
            ..ctx_reasoning(ReasoningStyle::ThinkingFlag, Some(ReasoningEffort::Max))
        };
        let body = encode_with_choice(&context, ToolChoice::Required);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert_eq!(body["tool_choice"], "required");
        assert!(
            body.get("reasoning_effort").is_none(),
            "effort is a thinking knob; it goes with it: {body}"
        );
        assert_eq!(body["tools"][0]["function"]["name"], "update_plan");
    }

    #[test]
    fn forced_choice_keeps_temperature_when_disabling_deepseek_thinking() {
        let context = ProtocolContext {
            thinking_supports_forced_tool_choice: false,
            supports_temperature: true,
            ..ctx_reasoning(ReasoningStyle::ThinkingFlag, Some(ReasoningEffort::Max))
        };
        let mut req = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-flash"),
            vec![Message::text(Role::User, "hi")],
        );
        req.temperature = Some(0.2);
        req.tools = vec![ToolDefinition {
            name: "update_plan".into(),
            description: "plan".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        req.tool_choice = ToolChoice::Required;

        let body = OpenAiChatAdapter::new()
            .encode_request(&req, &context, true)
            .unwrap()
            .body;
        assert_eq!(body["thinking"]["type"], "disabled");
        assert_eq!(body["tool_choice"], "required");
        let temperature = body["temperature"].as_f64().unwrap();
        assert!((temperature - 0.2).abs() < 1e-6, "{body}");
    }

    #[test]
    fn incompatible_provider_disables_thinking_on_named_tool_choice() {
        // Style None still gets the explicit disable: absence of the thinking
        // field means "provider default", which for these providers is ON.
        let context = ProtocolContext {
            thinking_supports_forced_tool_choice: false,
            ..ctx()
        };
        let body = encode_with_choice(&context, ToolChoice::Tool("update_plan".into()));
        assert_eq!(body["thinking"]["type"], "disabled");
        assert_eq!(body["tool_choice"]["type"], "function");
        assert_eq!(body["tool_choice"]["function"]["name"], "update_plan");
    }

    /// Encode a tool-loop continuation: user → assistant(reasoning? + tool
    /// call) → tool result. The shape DeepSeek's thinking mode validates.
    fn encode_tool_loop(context: &ProtocolContext, reasoning: Option<&str>) -> serde_json::Value {
        let mut assistant_parts = Vec::new();
        if let Some(text) = reasoning {
            assistant_parts.push(ContentPart::Reasoning { text: text.into() });
        }
        assistant_parts.push(ContentPart::ToolCall {
            call: ToolCall {
                id: leveler_core::ToolCallId::new("call_1"),
                name: "get_time".into(),
                arguments: serde_json::json!({}),
            },
        });
        let mut req = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-chat"),
            vec![
                Message::text(Role::User, "hi"),
                Message {
                    origin: None,
                    role: Role::Assistant,
                    content: assistant_parts,
                },
                Message {
                    origin: None,
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        result: leveler_model::ToolResultContent {
                            call_id: leveler_core::ToolCallId::new("call_1"),
                            content: "12:00".into(),
                            is_error: false,
                        },
                    }],
                },
            ],
        );
        req.tools = vec![ToolDefinition {
            name: "get_time".into(),
            description: "read the clock".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        OpenAiChatAdapter::new()
            .encode_request(&req, context, true)
            .unwrap()
            .body
    }

    #[test]
    fn passback_echoes_captured_reasoning_on_assistant_tool_call_messages() {
        let context = ProtocolContext {
            reasoning_replay: reasoning_carrying_contract(),
            ..ctx()
        };
        let body = encode_tool_loop(&context, Some("check the clock"));
        assert_eq!(body["messages"][1]["reasoning_content"], "check the clock");
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["name"],
            "get_time"
        );
    }

    /// Request-body contract for the reasoning-retention projection: a route
    /// that replays captured reasoning replays the full reasoning of every
    /// retained assistant turn, so the wire keeps every turn's reasoning under
    /// every arm — the arm cannot strip one while the turn is present. Nothing
    /// else about the assistant turn changes, and a turn that captured no
    /// reasoning still carries the key the provider validates.
    #[test]
    fn retention_projection_cannot_strip_a_retained_turns_reasoning() {
        use leveler_model::ReasoningRetention;

        fn tool_turn(i: usize) -> Vec<Message> {
            vec![
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
                        ContentPart::ToolCall {
                            call: ToolCall {
                                id: leveler_core::ToolCallId::new(format!("c{i}")),
                                name: "get_time".into(),
                                arguments: serde_json::json!({}),
                            },
                        },
                    ],
                },
                Message {
                    origin: None,
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        result: leveler_model::ToolResultContent {
                            call_id: leveler_core::ToolCallId::new(format!("c{i}")),
                            content: format!("o{i}"),
                            is_error: false,
                        },
                    }],
                },
            ]
        }

        let transcript: Vec<Message> = std::iter::once(Message::text(Role::User, "go"))
            .chain((1..=5).flat_map(tool_turn))
            .collect();
        let context = ProtocolContext {
            reasoning_replay: reasoning_carrying_contract(),
            ..ctx()
        };

        for policy in [
            ReasoningRetention::All,
            ReasoningRetention::LastTurns(3),
            ReasoningRetention::None,
        ] {
            let mut request = ModelRequest::new(
                ModelRef::new("deepseek", "deepseek-flash"),
                transcript.clone(),
            );
            request.tools = vec![ToolDefinition {
                name: "get_time".into(),
                description: "read the clock".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }];
            request.projection = Some(leveler_model::RequestProjection::project(
                &request.messages,
                &request.tools,
                context.reasoning_replay,
                policy,
            ));
            let body = OpenAiChatAdapter::new()
                .encode_request(&request, &context, true)
                .unwrap()
                .body;
            let messages = body["messages"].as_array().unwrap();
            let assistant: Vec<&serde_json::Value> = messages
                .iter()
                .filter(|m| m["role"] == "assistant")
                .collect();
            let carried: Vec<&str> = assistant
                .iter()
                .filter_map(|m| m["reasoning_content"].as_str())
                .collect();
            assert_eq!(
                carried,
                vec!["r1", "r2", "r3", "r4", "r5"],
                "{policy:?} must keep every retained turn's reasoning"
            );
            for message in &assistant {
                assert!(
                    message.get("reasoning_content").is_some(),
                    "{policy:?}: the required key is present on every replayed assistant turn"
                );
            }
            let assistant_texts: Vec<&str> = assistant
                .iter()
                .map(|m| m["content"].as_str().unwrap_or(""))
                .collect();
            assert_eq!(
                assistant_texts,
                vec!["t1", "t2", "t3", "t4", "t5"],
                "{policy:?}"
            );
            let tool_calls: usize = messages
                .iter()
                .map(|m| m["tool_calls"].as_array().map(Vec::len).unwrap_or(0))
                .sum();
            assert_eq!(tool_calls, 5, "{policy:?} must keep every tool call");
        }
    }

    #[test]
    fn passback_echoes_kernel_assembled_reasoning_not_an_empty_string() {
        // The kernel assembles a streamed round as reasoning → text → tool
        // calls. This is that exact shape: the captured reasoning must reach
        // the wire as `reasoning_content` when the provider requires passback
        // and the request exposes tools, never as the empty string the
        // provider would otherwise receive.
        let context = ProtocolContext {
            reasoning_replay: reasoning_carrying_contract(),
            ..ctx()
        };
        let mut request = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-chat"),
            vec![
                Message::text(Role::User, "hi"),
                Message {
                    origin: None,
                    role: Role::Assistant,
                    content: vec![
                        ContentPart::Reasoning {
                            text: "分析...继续分析...".into(),
                        },
                        ContentPart::Text {
                            text: "checking".into(),
                        },
                        ContentPart::ToolCall {
                            call: ToolCall {
                                id: leveler_core::ToolCallId::new("call_1"),
                                name: "get_time".into(),
                                arguments: serde_json::json!({}),
                            },
                        },
                    ],
                },
                Message {
                    origin: None,
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        result: leveler_model::ToolResultContent {
                            call_id: leveler_core::ToolCallId::new("call_1"),
                            content: "12:00".into(),
                            is_error: false,
                        },
                    }],
                },
            ],
        );
        request.tools = vec![ToolDefinition {
            name: "get_time".into(),
            description: "read the clock".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];

        let body = OpenAiChatAdapter::new()
            .encode_request(&request, &context, true)
            .unwrap()
            .body;
        assert_eq!(
            body["messages"][1]["reasoning_content"], "分析...继续分析...",
            "captured reasoning must be echoed, not dropped: {body}"
        );
        assert_eq!(body["messages"][1]["content"], "checking");
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["name"],
            "get_time"
        );
    }

    #[test]
    fn passback_sends_empty_reasoning_when_the_round_produced_none() {
        // A round that ran with thinking disabled has no reasoning to echo —
        // the provider still requires the key, and the empty string satisfies
        // it (measured against DeepSeek 2026-08-07).
        let context = ProtocolContext {
            reasoning_replay: reasoning_carrying_contract(),
            ..ctx()
        };
        let body = encode_tool_loop(&context, None);
        assert_eq!(body["messages"][1]["reasoning_content"], "");
    }

    /// The wire and the accounting read ONE projection.
    ///
    /// This is the owner seam, not a comparison of two implementations: the
    /// same [`leveler_model::RequestProjection`] is handed to the encoder and to
    /// [`leveler_model::ContextAccounting`], and both must agree with it about
    /// every turn — including the turns where the route carries nothing.
    #[test]
    fn wire_and_accounting_read_one_projection() {
        use leveler_model::{
            ContextAccounting, ModelRef, ReasoningReplayContract, ReasoningReplayScope,
            RequestProjection,
        };

        fn history(with_reasoning: bool, call: bool) -> Vec<Message> {
            let mut assistant = Vec::new();
            if with_reasoning {
                assistant.push(ContentPart::Reasoning {
                    text: "deliberation".into(),
                });
            }
            assistant.push(ContentPart::Text {
                text: "checking".into(),
            });
            if call {
                assistant.push(ContentPart::ToolCall {
                    call: ToolCall {
                        id: leveler_core::ToolCallId::new("c1"),
                        name: "get_time".into(),
                        arguments: serde_json::json!({}),
                    },
                });
            }
            vec![
                Message::text(Role::User, "hi"),
                Message {
                    origin: None,
                    role: Role::Assistant,
                    content: assistant,
                },
                Message {
                    origin: None,
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        result: leveler_model::ToolResultContent {
                            call_id: leveler_core::ToolCallId::new("c1"),
                            content: "12:00".into(),
                            is_error: false,
                        },
                    }],
                },
            ]
        }

        let contracts = [
            ("never", ReasoningReplayContract::NONE),
            ("when_tools", reasoning_carrying_without_empty_key()),
            ("when_tools+empty", reasoning_carrying_contract()),
            (
                "always",
                ReasoningReplayContract::raw_field(
                    ReasoningReplayScope::Always,
                    leveler_model::MissingReasoningReplay::Omit,
                ),
            ),
        ];
        let tool = ToolDefinition {
            name: "get_time".into(),
            description: "read the clock".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };

        for (name, contract) in contracts {
            for has_tools in [true, false] {
                for with_reasoning in [true, false] {
                    let mut request = ModelRequest::new(
                        ModelRef::new("deepseek", "deepseek-chat"),
                        history(with_reasoning, true),
                    );
                    if has_tools {
                        request.tools = vec![tool.clone()];
                    }
                    let projection = RequestProjection::project(
                        &request.messages,
                        &request.tools,
                        contract,
                        leveler_model::ReasoningRetention::All,
                    );
                    let accounting = ContextAccounting::compute(
                        request.model.clone(),
                        &projection,
                        Some(128_000),
                        Some(64_000),
                        None,
                    );
                    request.projection = Some(projection.clone());
                    let context = ProtocolContext {
                        reasoning_replay: contract,
                        ..ctx()
                    };
                    let body = OpenAiChatAdapter::new()
                        .encode_request(&request, &context, true)
                        .unwrap()
                        .body;

                    for (index, projected) in projection.messages().iter().enumerate() {
                        let wire = body["messages"][index].get("reasoning_content");
                        assert_eq!(
                            wire.and_then(|v| v.as_str()),
                            projected.reasoning.as_wire(),
                            "{name} tools={has_tools} reasoning={with_reasoning} message {index}"
                        );
                    }

                    // The accounting prices exactly the channel the wire
                    // carried, and the whole snapshot is the projection's own
                    // estimate (within per-slice integer division).
                    let mut carried = leveler_model::TokenEstimate::new();
                    for message in projection.messages() {
                        if let Some(text) = message.reasoning.as_wire() {
                            carried.add_text(text);
                        }
                    }
                    let reasoning_tokens = carried.tokens();
                    let category = accounting
                        .categories
                        .iter()
                        .find(|c| c.name == "messages")
                        .and_then(|m| m.children.iter().find(|c| c.name == "reasoning"))
                        .map(|c| c.tokens)
                        .unwrap_or(0);
                    assert_eq!(
                        category, reasoning_tokens,
                        "{name} tools={has_tools} reasoning={with_reasoning}: \
                         the meter must price the channel the wire carried"
                    );
                    assert!(
                        accounting
                            .used_tokens
                            .abs_diff(projection.estimated_tokens())
                            <= accounting.categories.len() as u64,
                        "{name} tools={has_tools} reasoning={with_reasoning}"
                    );
                }
            }
        }
    }

    /// The complete serialization matrix for `reasoning_content`, as an
    /// invariant rather than a list of expected bodies:
    ///
    /// * the field is present iff the route replays reasoning, the request
    ///   exposes tools, the turn is an assistant turn, and the turn either
    ///   captured reasoning or the route requires the key's presence;
    /// * turning replay on/off changes ONLY that field — `role`, `content`,
    ///   `tool_calls` and the tool-result message are byte-identical either
    ///   way, so a reasoning decision can never silently reshape a turn.
    ///
    /// Shapes: reasoning+text+call, reasoning+call, reasoning+text,
    /// reasoning only, text only, call only, empty.
    #[test]
    fn reasoning_content_matrix_preserves_every_other_field() {
        fn shape(reasoning: bool, text: bool, call: bool) -> Message {
            let mut content = Vec::new();
            if reasoning {
                content.push(ContentPart::Reasoning {
                    text: "because".into(),
                });
            }
            if text {
                content.push(ContentPart::Text {
                    text: "checking".into(),
                });
            }
            if call {
                content.push(ContentPart::ToolCall {
                    call: ToolCall {
                        id: leveler_core::ToolCallId::new("call_1"),
                        name: "get_time".into(),
                        arguments: serde_json::json!({}),
                    },
                });
            }
            Message {
                origin: None,
                role: Role::Assistant,
                content,
            }
        }

        let shapes = [
            ("reasoning+text+call", shape(true, true, true), true),
            ("reasoning+call", shape(true, false, true), true),
            ("reasoning+text", shape(true, true, false), true),
            ("reasoning only", shape(true, false, false), true),
            ("text only", shape(false, true, false), false),
            ("call only", shape(false, false, true), false),
            ("empty", shape(false, false, false), false),
        ];

        for (name, assistant, captured) in shapes {
            let messages = vec![
                Message::text(Role::User, "hi"),
                assistant,
                Message {
                    origin: None,
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        result: leveler_model::ToolResultContent {
                            call_id: leveler_core::ToolCallId::new("call_1"),
                            content: "12:00".into(),
                            is_error: false,
                        },
                    }],
                },
            ];
            let tool = ToolDefinition {
                name: "get_time".into(),
                description: "read the clock".into(),
                input_schema: serde_json::json!({"type": "object"}),
            };

            for tools in [true, false] {
                for (replay, key_required) in [(false, false), (true, false), (true, true)] {
                    let context = ProtocolContext {
                        reasoning_replay: if key_required {
                            reasoning_carrying_contract()
                        } else if replay {
                            reasoning_carrying_without_empty_key()
                        } else {
                            leveler_model::ReasoningReplayContract::NONE
                        },
                        ..ctx()
                    };
                    let mut request = ModelRequest::new(
                        ModelRef::new("deepseek", "deepseek-chat"),
                        messages.clone(),
                    );
                    if tools {
                        request.tools = vec![tool.clone()];
                    }
                    let body = OpenAiChatAdapter::new()
                        .encode_request(&request, &context, true)
                        .unwrap()
                        .body;
                    let got = &body["messages"][1];
                    let expect_key = replay && tools && (captured || key_required);
                    assert_eq!(
                        got.get("reasoning_content").is_some(),
                        expect_key,
                        "{name} tools={tools} replay={replay} key_required={key_required}: {got}"
                    );
                    if expect_key && captured {
                        assert_eq!(got["reasoning_content"], "because", "{name}");
                    }
                    if expect_key && !captured {
                        assert_eq!(got["reasoning_content"], "", "{name}");
                    }
                    // The rest of the turn is a function of the shape alone.
                    assert_eq!(got["role"], "assistant", "{name}");
                    let has_call = got["tool_calls"].as_array().is_some_and(|c| !c.is_empty());
                    assert_eq!(has_call, shape_has_call(&messages[1]), "{name}");
                    assert_eq!(
                        got["content"].as_str() == Some("checking"),
                        shape_has_text(&messages[1]),
                        "{name}"
                    );
                    // The tool-result turn is never touched by a reasoning decision.
                    assert_eq!(body["messages"][2]["role"], "tool", "{name}");
                    assert_eq!(body["messages"][2]["content"], "12:00", "{name}");
                    assert!(
                        body["messages"][2].get("reasoning_content").is_none(),
                        "{name}"
                    );
                }
            }
        }
    }

    fn shape_has_call(message: &Message) -> bool {
        message
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ToolCall { .. }))
    }

    fn shape_has_text(message: &Message) -> bool {
        message
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::Text { .. }))
    }

    /// The shipped DeepSeek route's wire, pinned byte for byte.
    ///
    /// The context-architecture refactor moved the replay decision out of this
    /// encoder and into the projection; the bytes a DeepSeek request uploads
    /// must not have moved with it, or every cached prefix in every existing
    /// session is re-billed at the uncached rate.
    #[test]
    fn shipped_deepseek_wire_is_unchanged_by_the_projection_refactor() {
        let context = ProtocolContext {
            reasoning_replay: reasoning_carrying_contract(),
            ..ctx()
        };
        // Reasoning present: carried verbatim, alongside the tool call.
        let with = encode_tool_loop(&context, Some("check the clock"));
        assert_eq!(
            with["messages"][1],
            serde_json::json!({
                "role": "assistant",
                "reasoning_content": "check the clock",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_time", "arguments": "{}"}
                }]
            }),
            "the assistant turn's exact bytes"
        );
        // No reasoning captured: the key is present and empty.
        let without = encode_tool_loop(&context, None);
        assert_eq!(
            without["messages"][1],
            serde_json::json!({
                "role": "assistant",
                "reasoning_content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_time", "arguments": "{}"}
                }]
            })
        );
        // The tool result turn is untouched by any of this.
        assert_eq!(
            without["messages"][2],
            serde_json::json!({"role": "tool", "content": "12:00", "tool_call_id": "call_1"})
        );
    }

    /// The scope is a contract value, not a hard-coded `has_tools` check: a
    /// route that declares `Always` carries reasoning even with no tools, and a
    /// route that declares `Never` carries none even with tools.
    #[test]
    fn replay_scope_is_honoured_in_both_directions() {
        use leveler_model::{MissingReasoningReplay, ReasoningReplayScope};
        let with_tools_already = |scope| ProtocolContext {
            reasoning_replay: leveler_model::ReasoningReplayContract::raw_field(
                scope,
                MissingReasoningReplay::Omit,
            ),
            ..ctx()
        };
        let mut request = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-chat"),
            vec![
                Message::text(Role::User, "hi"),
                Message {
                    origin: None,
                    role: Role::Assistant,
                    content: vec![ContentPart::Reasoning {
                        text: "thought".into(),
                    }],
                },
                Message::text(Role::User, "again"),
            ],
        );

        // Always: no tools in the request, reasoning still carried.
        let body = OpenAiChatAdapter::new()
            .encode_request(
                &request,
                &with_tools_already(ReasoningReplayScope::Always),
                true,
            )
            .unwrap()
            .body;
        assert_eq!(body["messages"][1]["reasoning_content"], "thought");

        // WhenToolsPresent: the same request with no tools carries nothing.
        let body = OpenAiChatAdapter::new()
            .encode_request(
                &request,
                &with_tools_already(ReasoningReplayScope::WhenToolsPresent),
                true,
            )
            .unwrap()
            .body;
        assert!(body["messages"][1].get("reasoning_content").is_none());

        // Never: even with tools exposed, nothing is carried.
        request.tools = vec![ToolDefinition {
            name: "grep".into(),
            description: "search".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let body = OpenAiChatAdapter::new()
            .encode_request(
                &request,
                &with_tools_already(ReasoningReplayScope::Never),
                true,
            )
            .unwrap()
            .body;
        assert!(body["messages"][1].get("reasoning_content").is_none());
    }

    /// A route that replays reasoning but does NOT validate the key's presence
    /// must leave a turn without captured reasoning alone: fabricating an
    /// empty field would add content the model never produced.
    #[test]
    fn replay_without_key_requirement_omits_the_field_for_a_reasoning_free_turn() {
        let context = ProtocolContext {
            reasoning_replay: reasoning_carrying_without_empty_key(),
            ..ctx()
        };
        let body = encode_tool_loop(&context, None);
        assert!(
            body["messages"][1].get("reasoning_content").is_none(),
            "{body}"
        );
    }

    #[test]
    fn passback_off_keeps_the_legacy_wire_without_the_key() {
        let body = encode_tool_loop(&ctx(), Some("check the clock"));
        assert!(
            body["messages"][1].get("reasoning_content").is_none(),
            "default wire must be unchanged: {body}"
        );
    }

    #[test]
    fn passback_does_not_touch_plain_assistant_text_messages_without_tools() {
        let context = ProtocolContext {
            reasoning_replay: reasoning_carrying_contract(),
            ..ctx()
        };
        let req = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-chat"),
            vec![
                Message::text(Role::User, "hi"),
                Message::text(Role::Assistant, "hello"),
                Message::text(Role::User, "again"),
            ],
        );
        let body = OpenAiChatAdapter::new()
            .encode_request(&req, &context, true)
            .unwrap()
            .body;
        assert!(
            body["messages"][1].get("reasoning_content").is_none(),
            "requests without tools must keep the legacy wire: {body}"
        );
    }

    #[test]
    fn passback_echoes_reasoning_from_plain_assistant_rounds_when_tools_are_available() {
        let context = ProtocolContext {
            reasoning_replay: reasoning_carrying_contract(),
            ..ctx()
        };
        let mut request = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-flash"),
            vec![
                Message::text(Role::User, "first"),
                Message {
                    origin: None,
                    role: Role::Assistant,
                    content: vec![
                        ContentPart::Reasoning {
                            text: "consider the next step".into(),
                        },
                        ContentPart::Text {
                            text: "done".into(),
                        },
                    ],
                },
                Message::text(Role::User, "continue with tools"),
            ],
        );
        request.tools = vec![ToolDefinition {
            name: "grep".into(),
            description: "search".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];

        let body = OpenAiChatAdapter::new()
            .encode_request(&request, &context, true)
            .unwrap()
            .body;
        assert_eq!(
            body["messages"][1]["reasoning_content"],
            "consider the next step"
        );
    }

    #[test]
    fn passback_omits_plain_round_reasoning_when_current_request_has_no_tools() {
        let context = ProtocolContext {
            reasoning_replay: reasoning_carrying_contract(),
            ..ctx()
        };
        let request = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-flash"),
            vec![
                Message::text(Role::User, "first"),
                Message {
                    origin: None,
                    role: Role::Assistant,
                    content: vec![
                        ContentPart::Reasoning {
                            text: "private chain".into(),
                        },
                        ContentPart::Text {
                            text: "done".into(),
                        },
                    ],
                },
                Message::text(Role::User, "continue without tools"),
            ],
        );

        let body = OpenAiChatAdapter::new()
            .encode_request(&request, &context, true)
            .unwrap()
            .body;
        assert!(body["messages"][1].get("reasoning_content").is_none());
    }

    #[test]
    fn compatible_provider_wire_is_unchanged_on_forced_choice() {
        // Legacy/compatible profiles (flag defaults to true) keep the exact
        // pre-existing wire: thinking + effort + the forced choice together.
        let context = ctx_reasoning(ReasoningStyle::ThinkingFlag, Some(ReasoningEffort::High));
        let body = encode_with_choice(&context, ToolChoice::Required);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["tool_choice"], "required");
    }

    #[test]
    fn openai_effort_style_sends_effort_without_thinking() {
        let body = encode_with(&ctx_reasoning(
            ReasoningStyle::OpenAiEffort,
            Some(ReasoningEffort::Max),
        ));
        assert!(body.get("thinking").is_none());
        assert_eq!(body["reasoning_effort"], "max");
    }

    #[test]
    fn request_reasoning_effort_overrides_the_profile_recommendation() {
        let context = ctx_reasoning(ReasoningStyle::OpenAiEffort, Some(ReasoningEffort::Low));
        let mut request = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-chat"),
            vec![Message::text(Role::User, "hi")],
        );
        request.reasoning_effort = Some(ReasoningEffort::High);
        let body = OpenAiChatAdapter::new()
            .encode_request(&request, &context, true)
            .unwrap()
            .body;
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn an_explicit_off_disables_thinking_and_drops_the_effort_with_it() {
        // The canonical `off` level reaches the wire as the route's own
        // disable, with no effort beside it: a disabled thinking mode has no
        // strength, and sending one anyway is a request the provider rejects.
        let context = ctx_reasoning(ReasoningStyle::ThinkingFlag, Some(ReasoningEffort::Max));
        let mut request = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-v4-pro"),
            vec![Message::text(Role::User, "hi")],
        );
        request.reasoning_effort = Some(ReasoningEffort::Max);
        request.thinking_disabled = true;
        let body = OpenAiChatAdapter::new()
            .encode_request(&request, &context, true)
            .unwrap()
            .body;
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(
            body.get("reasoning_effort").is_none(),
            "effort goes with the thinking knob: {body}"
        );
    }

    #[test]
    fn an_effort_style_has_no_way_to_say_off_and_sends_nothing() {
        // Which is why the canonical layer does not offer `off` on this style.
        let context = ctx_reasoning(ReasoningStyle::OpenAiEffort, Some(ReasoningEffort::High));
        let mut request = ModelRequest::new(
            ModelRef::new("openai", "gpt-5"),
            vec![Message::text(Role::User, "hi")],
        );
        request.reasoning_effort = Some(ReasoningEffort::High);
        request.thinking_disabled = true;
        let body = OpenAiChatAdapter::new()
            .encode_request(&request, &context, true)
            .unwrap()
            .body;
        assert!(body.get("thinking").is_none(), "{body}");
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }

    #[test]
    fn adapter_encodes_effective_effort_without_remapping() {
        // Policy already chose `high`. The adapter must not second-guess it
        // even if the profile default is `max`.
        let context = ctx_reasoning(ReasoningStyle::ThinkingFlag, Some(ReasoningEffort::Max));
        let mut request = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-v4-pro"),
            vec![Message::text(Role::User, "hi")],
        );
        request.reasoning_effort = Some(ReasoningEffort::High);
        let body = OpenAiChatAdapter::new()
            .encode_request(&request, &context, true)
            .unwrap()
            .body;
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn style_none_never_sends_effort_even_if_request_has_one() {
        let mut request = ModelRequest::new(
            ModelRef::new("kimi", "kimi-for-coding"),
            vec![Message::text(Role::User, "hi")],
        );
        request.reasoning_effort = Some(ReasoningEffort::Max);
        let body = OpenAiChatAdapter::new()
            .encode_request(&request, &ctx(), true)
            .unwrap()
            .body;
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn encodes_basic_request() {
        let adapter = OpenAiChatAdapter::new();
        let req = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-chat"),
            vec![Message::text(Role::User, "hi")],
        );
        let enc = adapter.encode_request(&req, &ctx(), true).unwrap();
        assert_eq!(enc.path, "/chat/completions");
        assert_eq!(enc.body["model"], "deepseek-chat");
        assert_eq!(enc.body["stream"], true);
        assert_eq!(enc.body["messages"][0]["role"], "user");
        assert_eq!(enc.body["messages"][0]["content"], "hi");
        assert_eq!(enc.body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn encodes_tools() {
        let adapter = OpenAiChatAdapter::new();
        let mut req = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-chat"),
            vec![Message::text(Role::User, "hi")],
        );
        req.tools = vec![ToolDefinition {
            name: "grep".into(),
            description: "search".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let enc = adapter.encode_request(&req, &ctx(), false).unwrap();
        assert_eq!(enc.body["tools"][0]["type"], "function");
        assert_eq!(enc.body["tools"][0]["function"]["name"], "grep");
        assert!(enc.body.get("stream_options").is_none());
    }

    #[test]
    fn decodes_non_streaming_response() {
        let adapter = OpenAiChatAdapter::new();
        let body = serde_json::to_vec(&serde_json::json!({
            "id": "resp_1",
            "choices": [{
                "message": {"content": "hello world"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        }))
        .unwrap();
        let resp = adapter.decode_response(&body, &ctx()).unwrap();
        assert_eq!(resp.message.text_content(), "hello world");
        assert_eq!(resp.finish_reason, FinishReason::Stop);
        assert_eq!(resp.usage.total(), 7);
        assert_eq!(resp.request_id.as_str(), "resp_1");
    }

    /// A reasoning-capable gateway reports the breakdown under
    /// `completion_tokens_details`. `completion_tokens` stays the total, so the
    /// parsed usage must carry 1000 output of which 700 were reasoning — not
    /// 300, and not 1700.
    #[test]
    fn decodes_provider_reasoning_tokens() {
        let adapter = OpenAiChatAdapter::new();
        let body = serde_json::to_vec(&serde_json::json!({
            "id": "resp_2",
            "choices": [{
                "message": {"content": "done"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 1000,
                "completion_tokens_details": {"reasoning_tokens": 700}
            }
        }))
        .unwrap();
        let resp = adapter.decode_response(&body, &ctx()).unwrap();
        assert_eq!(resp.usage.output_tokens, 1000);
        assert_eq!(resp.usage.reasoning_tokens, Some(700));
        assert_eq!(resp.usage.visible_output_tokens(), Some(300));
        assert_eq!(resp.usage.total(), 1100);
    }

    #[test]
    fn a_provider_without_a_reasoning_breakdown_stays_unknown() {
        let adapter = OpenAiChatAdapter::new();
        let body = serde_json::to_vec(&serde_json::json!({
            "choices": [{"message": {"content": "x"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 40}
        }))
        .unwrap();
        let resp = adapter.decode_response(&body, &ctx()).unwrap();
        assert_eq!(resp.usage.reasoning_tokens, None);
        assert_eq!(resp.usage.visible_output_tokens(), None);
    }

    /// A gateway may send the details object for other reasons without a
    /// reasoning count; that is still an absent measurement.
    #[test]
    fn completion_details_without_reasoning_is_unknown() {
        let adapter = OpenAiChatAdapter::new();
        let body = serde_json::to_vec(&serde_json::json!({
            "choices": [{"message": {"content": "x"}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 4,
                "completion_tokens_details": {"accepted_prediction_tokens": 0}
            }
        }))
        .unwrap();
        let resp = adapter.decode_response(&body, &ctx()).unwrap();
        assert_eq!(resp.usage.reasoning_tokens, None);
    }

    #[test]
    fn a_reported_zero_reasoning_count_is_kept() {
        let adapter = OpenAiChatAdapter::new();
        let body = serde_json::to_vec(&serde_json::json!({
            "choices": [{"message": {"content": "x"}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 4,
                "completion_tokens_details": {"reasoning_tokens": 0}
            }
        }))
        .unwrap();
        let resp = adapter.decode_response(&body, &ctx()).unwrap();
        assert_eq!(resp.usage.reasoning_tokens, Some(0));
        assert_eq!(resp.usage.visible_output_tokens(), Some(4));
    }

    #[test]
    fn decodes_response_with_null_tool_calls() {
        // GLM (and some gateways) send `"tool_calls": null` — must not fail.
        let adapter = OpenAiChatAdapter::new();
        let body = serde_json::to_vec(&serde_json::json!({
            "choices": [{
                "message": {"content": "OK", "tool_calls": null},
                "finish_reason": "stop"
            }]
        }))
        .unwrap();
        let resp = adapter.decode_response(&body, &ctx()).unwrap();
        assert_eq!(resp.message.text_content(), "OK");
    }

    #[test]
    fn decodes_tool_call_in_response() {
        let adapter = OpenAiChatAdapter::new();
        let body = serde_json::to_vec(&serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "c1",
                        "function": {"name": "grep", "arguments": "{\"q\":\"x\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }))
        .unwrap();
        let resp = adapter.decode_response(&body, &ctx()).unwrap();
        let has_tool = resp
            .message
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ToolCall { .. }));
        assert!(has_tool);
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn round_trips_tool_result_message() {
        let msgs = vec![Message {
            origin: None,
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                result: leveler_model::ToolResultContent {
                    call_id: leveler_core::ToolCallId::new("c1"),
                    content: "42".into(),
                    is_error: false,
                },
            }],
        }];
        let converted = convert_messages(&leveler_model::RequestProjection::project(
            &msgs,
            &[],
            leveler_model::ReasoningReplayContract::NONE,
            leveler_model::ReasoningRetention::All,
        ));
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "tool");
        assert_eq!(converted[0].tool_call_id.as_deref(), Some("c1"));
        assert!(matches!(
            &converted[0].content,
            Some(wire::ChatContent::Text(t)) if t == "42"
        ));
    }

    #[test]
    fn image_content_serializes_as_image_url_part() {
        use leveler_model::ImageSource;
        let msgs = vec![Message {
            origin: None,
            role: Role::User,
            content: vec![
                ContentPart::Text {
                    text: "look".into(),
                },
                ContentPart::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "abc".into(),
                    },
                },
            ],
        }];
        let converted = convert_messages(&leveler_model::RequestProjection::project(
            &msgs,
            &[],
            leveler_model::ReasoningReplayContract::NONE,
            leveler_model::ReasoningRetention::All,
        ));
        let json = serde_json::to_value(&converted[0]).unwrap();
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][1]["type"], "image_url");
        assert_eq!(
            json["content"][1]["image_url"]["url"],
            "data:image/png;base64,abc"
        );
    }

    /// Byte offset of the first difference between two wire strings.
    fn first_divergence(a: &str, b: &str) -> usize {
        a.bytes()
            .zip(b.bytes())
            .position(|(x, y)| x != y)
            .unwrap_or_else(|| a.len().min(b.len()))
    }

    /// A request whose control channel carries both stable session blocks and
    /// one per-request observation, the shape the coding harness builds.
    fn request_with_execution_state(
        transcript: Vec<Message>,
        execution_state: &str,
    ) -> ModelRequest {
        use leveler_model::{
            ControlContext, PromptAuthority, PromptSegment, PromptSource, SegmentLifecycle,
        };
        let mut control = ControlContext::default();
        control.push(PromptSegment::control(
            "core_contract",
            PromptSource::BasePrompt,
            PromptAuthority::CoreContract,
            SegmentLifecycle::SessionPrefix,
            true,
            "Core contract body.",
        ));
        control.push(PromptSegment::control(
            "project_rules",
            PromptSource::ProjectRules {
                paths: vec!["AGENTS.md".into()],
            },
            PromptAuthority::ProjectInstruction,
            SegmentLifecycle::SessionPrefix,
            true,
            "Root rule.",
        ));
        control.push(PromptSegment::control(
            "execution_state",
            PromptSource::ExecutionState,
            PromptAuthority::RuntimeFact,
            SegmentLifecycle::RequestEphemeral,
            false,
            execution_state,
        ));
        let mut req = ModelRequest::new(ModelRef::new("deepseek", "deepseek-flash"), transcript);
        req.control_context = control;
        req.tools = vec![ToolDefinition {
            name: "read_file".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type":"object"}),
        }];
        req
    }

    fn assistant_tool_call() -> Message {
        Message::from_parts(
            Role::Assistant,
            vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: leveler_core::ToolCallId::new("c1"),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "src/lib.rs"}),
                },
            }],
            None,
        )
    }

    fn tool_result() -> Message {
        Message::from_parts(
            Role::Tool,
            vec![ContentPart::ToolResult {
                result: ToolResultContent {
                    call_id: leveler_core::ToolCallId::new("c1"),
                    content: "pub fn old() {}".into(),
                    is_error: false,
                },
            }],
            None,
        )
    }

    /// A per-request control block changes on every round. It must not sit
    /// before the transcript, or the provider's prefix cache can never match
    /// past it and the whole history is re-processed every round.
    #[test]
    fn volatile_request_control_does_not_break_the_transcript_prefix() {
        let round1 = request_with_execution_state(
            vec![Message::text(Role::User, "goal")],
            "Execution state:\n{\"step\":1}",
        );
        let round2 = request_with_execution_state(
            vec![
                Message::text(Role::User, "goal"),
                assistant_tool_call(),
                tool_result(),
            ],
            "Execution state:\n{\"step\":2}",
        );

        let body1 = OpenAiChatAdapter::new()
            .encode_request(&round1, &ctx(), true)
            .unwrap()
            .body;
        let body2 = OpenAiChatAdapter::new()
            .encode_request(&round2, &ctx(), true)
            .unwrap()
            .body;
        let m1 = body1["messages"].as_array().unwrap();
        let m2 = body2["messages"].as_array().unwrap();

        // The leading system message is the stable prefix.
        assert_eq!(m1[0]["role"], "system");
        assert_eq!(
            m1[0], m2[0],
            "the leading system prefix changed between rounds"
        );
        assert!(
            !m1[0]["content"].as_str().unwrap().contains("step"),
            "a single-request observation must not be in the leading prefix: {}",
            m1[0]["content"]
        );

        // Everything both rounds share is byte-identical, and the wire must not
        // diverge inside it.
        let shared = m1.len().min(m2.len());
        let shared = (0..shared).take_while(|i| m1[*i] == m2[*i]).count();
        assert!(
            shared >= 2,
            "leading prefix and the shared transcript must match; shared={shared}"
        );
        let shared_json = serde_json::to_string(&m1[..shared]).unwrap();
        let shared_end = shared_json.len() - 2; // drop the enclosing brackets
        let wire1 = serde_json::to_string(m1).unwrap();
        let wire2 = serde_json::to_string(m2).unwrap();
        assert!(
            first_divergence(&wire1, &wire2) > shared_end,
            "the wire diverged inside the shared transcript (offset {} of {} bytes); \
             a per-request block is ahead of it again",
            first_divergence(&wire1, &wire2),
            shared_end
        );

        // The observation is attached after the transcript, still on the
        // system channel so its authority is unchanged.
        let tail1 = m1.last().unwrap();
        let tail2 = m2.last().unwrap();
        assert_eq!(tail1["role"], "system");
        assert_eq!(tail2["role"], "system");
        assert!(tail1["content"].as_str().unwrap().contains("\"step\":1"));
        assert!(tail2["content"].as_str().unwrap().contains("\"step\":2"));

        // The stable prefix really is stable, not merely equal by accident.
        assert!(
            m1[0]["content"]
                .as_str()
                .unwrap()
                .contains("Core contract body.")
        );
        assert!(m1[0]["content"].as_str().unwrap().contains("Root rule."));
    }

    /// Encoding one decision twice must be byte-identical: no map iteration or
    /// discovery order may leak into the wire.
    #[test]
    fn encoding_the_same_request_is_byte_deterministic() {
        let build = || {
            request_with_execution_state(
                vec![
                    Message::text(Role::User, "goal"),
                    assistant_tool_call(),
                    tool_result(),
                ],
                "Execution state:\n{\"step\":3}",
            )
        };
        let a = OpenAiChatAdapter::new()
            .encode_request(&build(), &ctx(), true)
            .unwrap()
            .body;
        let b = OpenAiChatAdapter::new()
            .encode_request(&build(), &ctx(), true)
            .unwrap()
            .body;
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }
}
