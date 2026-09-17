//! The OpenAI Chat Completions protocol adapter . DeepSeek and
//! GLM both speak this protocol, so it is reused rather than duplicated.

mod stream;
mod wire;

use async_stream::stream;
use futures::StreamExt;

use leveler_model::{
    ContentPart, FinishReason, ImageSource, Message, ModelError, ModelErrorKind, ModelEvent,
    ModelRequest, ModelResponse, ProtocolAdapter, ProtocolContext, ProtocolError, ProtocolKind,
    RawByteStream, ReasoningStyle, Role, TokenUsage, ToolCall, ToolChoice,
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
        let messages = convert_messages(&request.messages, context.passback_reasoning_content);

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
        let (thinking, reasoning_effort) = if !context.thinking_supports_forced_tool_choice
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
            })
            .unwrap_or_default();

        Ok(ModelResponse {
            request_id: request_id_from(&resp.id),
            message: Message {
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

/// Convert unified messages to OpenAI chat messages.
fn convert_messages(messages: &[Message], passback_reasoning: bool) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    for msg in messages {
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
        let mut reasoning = String::new();
        let mut tool_calls = Vec::new();
        let mut images: Vec<wire::ChatImageUrl> = Vec::new();
        for part in &msg.content {
            match part {
                ContentPart::Text { text: t } => text.push_str(t),
                ContentPart::Reasoning { text: t } => reasoning.push_str(t),
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

        // Providers with the pass-back contract (DeepSeek thinking mode)
        // validate that assistant tool-call messages carry a
        // `reasoning_content` key: the captured reasoning, or the empty
        // string when the round produced none (e.g. it ran with thinking
        // explicitly disabled). Everyone else keeps the legacy wire — the
        // key is never sent.
        let reasoning_content =
            (passback_reasoning && msg.role == Role::Assistant && !tool_calls.is_empty())
                .then_some(reasoning);

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
    out
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
        ModelRef, ReasoningConfig, ReasoningEffort, ReasoningStyle, ToolDefinition,
    };

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
            passback_reasoning_content: false,
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
        let req = ModelRequest::new(
            ModelRef::new("deepseek", "deepseek-chat"),
            vec![
                Message::text(Role::User, "hi"),
                Message {
                    role: Role::Assistant,
                    content: assistant_parts,
                },
                Message {
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
        OpenAiChatAdapter::new()
            .encode_request(&req, context, true)
            .unwrap()
            .body
    }

    #[test]
    fn passback_echoes_captured_reasoning_on_assistant_tool_call_messages() {
        let context = ProtocolContext {
            passback_reasoning_content: true,
            ..ctx()
        };
        let body = encode_tool_loop(&context, Some("check the clock"));
        assert_eq!(body["messages"][1]["reasoning_content"], "check the clock");
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
            passback_reasoning_content: true,
            ..ctx()
        };
        let body = encode_tool_loop(&context, None);
        assert_eq!(body["messages"][1]["reasoning_content"], "");
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
    fn passback_does_not_touch_plain_assistant_text_messages() {
        let context = ProtocolContext {
            passback_reasoning_content: true,
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
            "only tool-call messages carry the echo: {body}"
        );
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
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                result: leveler_model::ToolResultContent {
                    call_id: leveler_core::ToolCallId::new("c1"),
                    content: "42".into(),
                    is_error: false,
                },
            }],
        }];
        let converted = convert_messages(&msgs, false);
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
        let converted = convert_messages(&msgs, false);
        let json = serde_json::to_value(&converted[0]).unwrap();
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][1]["type"], "image_url");
        assert_eq!(
            json["content"][1]["image_url"]["url"],
            "data:image/png;base64,abc"
        );
    }
}
