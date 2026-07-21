//! The Anthropic Messages protocol adapter (`/v1/messages`). Claude models speak
//! this natively. Differences from OpenAI Chat that this adapter reconciles:
//! `system` is a top-level field (not a message), `max_tokens` is required,
//! tool calls are `tool_use` blocks with a JSON `input` (not a stringified one),
//! tool results are `tool_result` blocks inside a `user` message, and auth is
//! `x-api-key` + `anthropic-version` rather than a bearer token.
//!
//! Note: extended-thinking *encoding* is intentionally omitted for now (a request
//! without a `thinking` field is valid); thinking *deltas* on the way back are
//! still surfaced as `ReasoningDelta`.

mod stream;
mod wire;

use async_stream::stream;
use futures::StreamExt;

use leveler_core::{RequestId, ToolCallId};
use leveler_model::{
    ContentPart, EncodedRequest, FinishReason, ImageSource, Message, ModelError, ModelErrorKind,
    ModelEvent, ModelEventStream, ModelRequest, ModelResponse, ProtocolAdapter, ProtocolContext,
    ProtocolError, ProtocolKind, RawByteStream, Role, TokenUsage, ToolCall, ToolChoice,
};

use crate::sse::SseDecoder;
use stream::{AnthropicStreamAssembler, map_stop_reason};
use wire::{
    ImageBlockSource, MessagesRequest, MessagesResponse, ReqBlock, ReqMessage, ReqTool, RespBlock,
};

/// Fallback when the caller sets no `max_output_tokens` — Anthropic requires the
/// field. 4096 is within every Claude model's output cap.
const DEFAULT_MAX_TOKENS: u32 = 4096;
/// Pinned stable Messages API version.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Stateless adapter for the Anthropic Messages protocol.
#[derive(Debug, Clone, Default)]
pub struct AnthropicMessagesAdapter;

impl AnthropicMessagesAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl ProtocolAdapter for AnthropicMessagesAdapter {
    fn protocol(&self) -> ProtocolKind {
        ProtocolKind::AnthropicMessages
    }

    fn encode_request(
        &self,
        request: &ModelRequest,
        context: &ProtocolContext,
        stream: bool,
    ) -> Result<EncodedRequest, ProtocolError> {
        let (system, messages) = convert_messages(&request.messages);

        let tools = request
            .tools
            .iter()
            .map(|t| ReqTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect();

        let req = MessagesRequest {
            model: context.model_id.clone(),
            max_tokens: request.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            system,
            messages,
            tools,
            tool_choice: convert_tool_choice(&request.tool_choice),
            // Omitted entirely for providers that reject a caller-chosen value.
            temperature: context
                .supports_temperature
                .then_some(request.temperature)
                .flatten(),
            stop_sequences: request.stop.clone(),
            stream,
        };

        let body = serde_json::to_value(&req)
            .map_err(|e| ProtocolError::Encode(format!("serialize messages request: {e}")))?;

        // Anthropic auth is a header pair, not a bearer token; the transport
        // suppresses `Authorization` when it sees an explicit `x-api-key`.
        let mut headers = vec![(
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string(),
        )];
        if let Some(key) = &context.api_key {
            headers.push(("x-api-key".to_string(), key.clone()));
        }

        Ok(EncodedRequest {
            path: "/v1/messages".to_string(),
            body,
            headers,
        })
    }

    fn decode_response(
        &self,
        body: &[u8],
        _context: &ProtocolContext,
    ) -> Result<ModelResponse, ProtocolError> {
        let resp: MessagesResponse = serde_json::from_slice(body)
            .map_err(|e| ProtocolError::Decode(format!("parse messages response: {e}")))?;

        let mut content = Vec::new();
        for block in resp.content {
            match block {
                RespBlock::Thinking { thinking } if !thinking.is_empty() => {
                    content.push(ContentPart::Reasoning { text: thinking });
                }
                RespBlock::Text { text } if !text.is_empty() => {
                    content.push(ContentPart::Text { text });
                }
                RespBlock::ToolUse { id, name, input } => {
                    let id = if id.is_empty() {
                        format!("call_{}", content.len())
                    } else {
                        id
                    };
                    content.push(ContentPart::ToolCall {
                        call: ToolCall {
                            id: ToolCallId::new(id),
                            name,
                            arguments: input,
                        },
                    });
                }
                _ => {}
            }
        }

        let finish_reason = resp
            .stop_reason
            .as_deref()
            .map(map_stop_reason)
            .unwrap_or(FinishReason::Stop);

        let usage = resp
            .usage
            .map(|u| TokenUsage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cached_input_tokens: u.cache_read_input_tokens,
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
    ) -> Result<ModelEventStream, ProtocolError> {
        let out = stream! {
            let mut decoder = SseDecoder::new();
            let mut assembler = AnthropicStreamAssembler::new();

            while let Some(item) = raw.next().await {
                match item {
                    Ok(bytes) => {
                        for sse in decoder.feed(&bytes) {
                            let data = sse.data.trim();
                            if data.is_empty() {
                                continue;
                            }
                            let event = sse.event.as_deref().unwrap_or("");
                            for ev in assembler.on_event(event, data) {
                                yield Ok(ev);
                            }
                        }
                    }
                    Err(err) => {
                        yield Ok(ModelEvent::Error { error: err });
                        return;
                    }
                }
            }

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

/// Split unified messages into Anthropic's `system` field plus `user`/`assistant`
/// messages. `Role::System` text is hoisted to `system`; `Role::Tool` results
/// become `tool_result` blocks inside a `user` message.
fn convert_messages(messages: &[Message]) -> (Option<String>, Vec<ReqMessage>) {
    let mut system_parts = Vec::new();
    let mut out = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                let text = collect_text(&msg.content);
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            Role::Tool => {
                let mut blocks = Vec::new();
                for part in &msg.content {
                    if let ContentPart::ToolResult { result } = part {
                        blocks.push(ReqBlock::ToolResult {
                            tool_use_id: result.call_id.to_string(),
                            content: result.content.clone(),
                            is_error: result.is_error,
                        });
                    }
                }
                if !blocks.is_empty() {
                    out.push(ReqMessage {
                        role: "user".to_string(),
                        content: blocks,
                    });
                }
            }
            Role::User | Role::Assistant => {
                let mut blocks = Vec::new();
                for part in &msg.content {
                    match part {
                        ContentPart::Text { text } if !text.is_empty() => {
                            blocks.push(ReqBlock::Text { text: text.clone() });
                        }
                        ContentPart::ToolCall { call } => blocks.push(ReqBlock::ToolUse {
                            id: call.id.to_string(),
                            name: call.name.clone(),
                            input: call.arguments.clone(),
                        }),
                        ContentPart::Image { source } => blocks.push(ReqBlock::Image {
                            source: image_source(source),
                        }),
                        // Reasoning parts are not replayed upstream.
                        _ => {}
                    }
                }
                // Anthropic rejects an empty content array — skip empty turns.
                if !blocks.is_empty() {
                    out.push(ReqMessage {
                        role: role_str(msg.role).to_string(),
                        content: blocks,
                    });
                }
            }
        }
    }

    let system = (!system_parts.is_empty()).then(|| system_parts.join("\n\n"));
    (system, out)
}

fn collect_text(content: &[ContentPart]) -> String {
    let mut text = String::new();
    for part in content {
        if let ContentPart::Text { text: t } = part {
            text.push_str(t);
        }
    }
    text
}

fn image_source(source: &ImageSource) -> ImageBlockSource {
    match source {
        ImageSource::Url { url } => ImageBlockSource::Url { url: url.clone() },
        ImageSource::Base64 { media_type, data } => ImageBlockSource::Base64 {
            media_type: media_type.clone(),
            data: data.clone(),
        },
    }
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::Assistant => "assistant",
        // System/Tool are handled before this; User is the only other case.
        _ => "user",
    }
}

fn convert_tool_choice(choice: &ToolChoice) -> Option<serde_json::Value> {
    match choice {
        ToolChoice::Auto => None,
        ToolChoice::None => Some(serde_json::json!({ "type": "none" })),
        ToolChoice::Required => Some(serde_json::json!({ "type": "any" })),
        ToolChoice::Tool(name) => Some(serde_json::json!({ "type": "tool", "name": name })),
    }
}

fn request_id_from(id: &Option<String>) -> RequestId {
    match id {
        Some(s) if !s.is_empty() => RequestId::new(s.clone()),
        _ => RequestId::generate(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_model::{ModelRef, ReasoningConfig, ToolDefinition};

    fn ctx() -> ProtocolContext {
        ProtocolContext {
            base_url: "https://api.anthropic.com".into(),
            model_id: "claude-sonnet-5".into(),
            api_key: Some("secret".into()),
            extra_headers: vec![],
            reasoning: ReasoningConfig::default(),
            parallel_tool_calls: true,
            supports_temperature: true,
        }
    }

    fn user_req(text: &str) -> ModelRequest {
        ModelRequest::new(
            ModelRef::new("anthropic", "claude-sonnet-5"),
            vec![Message::text(Role::User, text)],
        )
    }

    #[test]
    fn encodes_path_headers_and_required_max_tokens() {
        let enc = AnthropicMessagesAdapter::new()
            .encode_request(&user_req("hi"), &ctx(), true)
            .unwrap();
        assert_eq!(enc.path, "/v1/messages");
        assert_eq!(enc.body["model"], "claude-sonnet-5");
        assert_eq!(enc.body["stream"], true);
        // max_tokens is required and must always be present.
        assert_eq!(enc.body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(enc.body["messages"][0]["role"], "user");
        assert_eq!(enc.body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(enc.body["messages"][0]["content"][0]["text"], "hi");
        // Auth header pair, no bearer.
        assert!(
            enc.headers
                .iter()
                .any(|(k, v)| k == "x-api-key" && v == "secret")
        );
        assert!(
            enc.headers
                .iter()
                .any(|(k, v)| k == "anthropic-version" && v == ANTHROPIC_VERSION)
        );
    }

    #[test]
    fn system_message_is_hoisted_to_top_level_field() {
        let req = ModelRequest::new(
            ModelRef::new("anthropic", "claude-sonnet-5"),
            vec![
                Message::text(Role::System, "be terse"),
                Message::text(Role::User, "hi"),
            ],
        );
        let enc = AnthropicMessagesAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap();
        assert_eq!(enc.body["system"], "be terse");
        // The system turn must NOT appear as a message.
        assert_eq!(enc.body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(enc.body["messages"][0]["role"], "user");
    }

    #[test]
    fn tool_result_becomes_a_user_tool_result_block() {
        let req = ModelRequest::new(
            ModelRef::new("anthropic", "claude-sonnet-5"),
            vec![Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    result: leveler_model::ToolResultContent {
                        call_id: ToolCallId::new("tu_1"),
                        content: "42".into(),
                        is_error: false,
                    },
                }],
            }],
        );
        let enc = AnthropicMessagesAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap();
        assert_eq!(enc.body["messages"][0]["role"], "user");
        assert_eq!(enc.body["messages"][0]["content"][0]["type"], "tool_result");
        assert_eq!(enc.body["messages"][0]["content"][0]["tool_use_id"], "tu_1");
        assert_eq!(enc.body["messages"][0]["content"][0]["content"], "42");
    }

    #[test]
    fn tool_call_encodes_input_as_json_object_not_string() {
        let req = ModelRequest::new(
            ModelRef::new("anthropic", "claude-sonnet-5"),
            vec![Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new("tu_1"),
                        name: "grep".into(),
                        arguments: serde_json::json!({"pattern": "x"}),
                    },
                }],
            }],
        );
        let enc = AnthropicMessagesAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap();
        let block = &enc.body["messages"][0]["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["name"], "grep");
        // input is a JSON object, not a stringified blob.
        assert_eq!(block["input"]["pattern"], "x");
    }

    #[test]
    fn encodes_tools_and_tool_choice() {
        let mut req = user_req("hi");
        req.tools = vec![ToolDefinition {
            name: "grep".into(),
            description: "search".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        req.tool_choice = ToolChoice::Required;
        let enc = AnthropicMessagesAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap();
        assert_eq!(enc.body["tools"][0]["name"], "grep");
        assert_eq!(enc.body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(enc.body["tool_choice"]["type"], "any");
    }

    #[test]
    fn temperature_dropped_when_provider_rejects_it() {
        let mut req = user_req("hi");
        req.temperature = Some(0.0);
        let context = ProtocolContext {
            supports_temperature: false,
            ..ctx()
        };
        let enc = AnthropicMessagesAdapter::new()
            .encode_request(&req, &context, false)
            .unwrap();
        assert!(enc.body.get("temperature").is_none());
    }

    #[test]
    fn decodes_text_and_tool_use_response() {
        let body = serde_json::to_vec(&serde_json::json!({
            "id": "msg_1",
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "tool_use", "id": "tu_9", "name": "grep", "input": {"q": "x"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5, "output_tokens": 2, "cache_read_input_tokens": 3}
        }))
        .unwrap();
        let resp = AnthropicMessagesAdapter::new()
            .decode_response(&body, &ctx())
            .unwrap();
        assert_eq!(resp.request_id.as_str(), "msg_1");
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
        assert_eq!(resp.usage.input_tokens, 5);
        assert_eq!(resp.usage.cached_input_tokens, 3);
        assert!(
            resp.message
                .content
                .iter()
                .any(|p| matches!(p, ContentPart::Text { text } if text == "hello"))
        );
        let call = resp
            .message
            .content
            .iter()
            .find_map(|p| match p {
                ContentPart::ToolCall { call } => Some(call),
                _ => None,
            })
            .expect("tool call decoded");
        assert_eq!(call.name, "grep");
        assert_eq!(call.id.as_str(), "tu_9");
        assert_eq!(call.arguments["q"], "x");
    }
}
