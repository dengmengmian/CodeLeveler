//! The Anthropic Messages protocol adapter (`/v1/messages`). Claude models speak
//! this natively. Differences from OpenAI Chat that this adapter reconciles:
//! `system` is a top-level field (not a message), `max_tokens` is required,
//! tool calls are `tool_use` blocks with a JSON `input` (not a stringified one),
//! tool results are `tool_result` blocks inside a `user` message, and auth is
//! `x-api-key` + `anthropic-version` rather than a bearer token.
//!
//! Thinking modes are explicit profile capabilities. Signed and redacted
//! blocks survive both decode paths and are replayed without modification.

mod stream;
mod wire;

use async_stream::stream;
use futures::StreamExt;

use leveler_core::{RequestId, ToolCallId};
use leveler_model::{
    ContentPart, EncodedRequest, FinishReason, ImageSource, Message, ModelError, ModelErrorKind,
    ModelEvent, ModelEventStream, ModelRequest, ModelResponse, ProtocolAdapter, ProtocolContext,
    ProtocolError, ProtocolKind, RawByteStream, Role, ToolCall, ToolChoice,
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
    /// The adapter carries no state; every instance is interchangeable.
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
        // Authenticated thinking blocks are replayed verbatim by the shared
        // projection; plain reasoning has no signature and is omitted.
        let projection =
            leveler_model::RequestProjection::for_request(request, context.reasoning_replay);
        let (system, messages) = convert_messages(&projection);

        let tools = request
            .tools
            .iter()
            .map(|t| ReqTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect();

        let thinking = match context.reasoning.style {
            leveler_model::ReasoningStyle::None => None,
            leveler_model::ReasoningStyle::AdaptiveThinking => {
                Some(serde_json::json!({"type":"adaptive"}))
            }
            leveler_model::ReasoningStyle::BudgetedThinking { budget_tokens } => {
                if budget_tokens < 1024
                    || budget_tokens >= request.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS)
                {
                    return Err(ProtocolError::Encode(
                        "thinking budget must be at least 1024 and below max_output_tokens".into(),
                    ));
                }
                Some(serde_json::json!({"type":"enabled", "budget_tokens":budget_tokens}))
            }
            _ => {
                return Err(ProtocolError::Encode(
                    "thinking style is not supported by the Messages protocol".into(),
                ));
            }
        };
        if thinking.is_some() && request.tool_choice.forces_tool_call() {
            return Err(ProtocolError::Encode(
                "forced tool choice is incompatible with thinking".into(),
            ));
        }
        if thinking.is_some()
            && request.reasoning_effort == Some(leveler_model::ReasoningEffort::Minimal)
        {
            return Err(ProtocolError::Encode(
                "Messages thinking does not support minimal effort".into(),
            ));
        }
        let output_config = thinking
            .as_ref()
            .and(request.reasoning_effort)
            .map(|effort| serde_json::json!({"effort":effort.as_wire()}));
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
            thinking,
            output_config,
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
                RespBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    if signature.is_empty() {
                        return Err(ProtocolError::Decode(
                            "thinking block is missing its replay signature".into(),
                        ));
                    }
                    content.push(ContentPart::SignedReasoning {
                        text: thinking,
                        signature,
                    });
                }
                RespBlock::RedactedThinking { data } => {
                    content.push(ContentPart::RedactedReasoning { data })
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

        let usage = resp.usage.map(|u| u.canonical()).unwrap_or_default();

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
    ) -> Result<ModelEventStream, ProtocolError> {
        let out = stream! {
            let mut decoder = SseDecoder::new();
            let mut assembler = AnthropicStreamAssembler::new();

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
/// messages. Current control is encoded on the system channel. A leftover
/// `Role::System` row stays on that channel too (it must not become a user
/// turn) but that wire choice does not assign it contract authority — the
/// projection's segment authority is unchanged. Tool results become
/// `tool_result` blocks inside a user message.
fn convert_messages(
    projection: &leveler_model::RequestProjection,
) -> (Option<String>, Vec<ReqMessage>) {
    let mut system_parts = Vec::new();
    let control = projection.control_text();
    if !control.is_empty() {
        system_parts.push(control);
    }
    let mut out = Vec::new();

    for msg in projection.messages() {
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
                        ContentPart::SignedReasoning { text, signature } => {
                            blocks.push(ReqBlock::Thinking {
                                thinking: text.clone(),
                                signature: signature.clone(),
                            })
                        }
                        ContentPart::RedactedReasoning { data } => {
                            blocks.push(ReqBlock::RedactedThinking { data: data.clone() })
                        }
                        ContentPart::ToolCall { call } => blocks.push(ReqBlock::ToolUse {
                            id: call.id.to_string(),
                            name: call.name.clone(),
                            input: call.arguments.clone(),
                        }),
                        ContentPart::Image { source } => blocks.push(ReqBlock::Image {
                            source: image_source(source),
                        }),
                        // Reasoning is a projection decision, never content
                        // here: the contract resolved `Never` for this route, so
                        // it cannot appear at all. The arm stays defensive.
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
    use leveler_model::{ModelRef, ReasoningConfig, ReasoningEffort, ToolDefinition};

    fn ctx() -> ProtocolContext {
        ProtocolContext {
            base_url: "https://api.anthropic.com".into(),
            model_id: "claude-sonnet-5".into(),
            api_key: Some("secret".into()),
            extra_headers: vec![],
            reasoning: ReasoningConfig::default(),
            parallel_tool_calls: true,
            supports_temperature: true,
            thinking_supports_forced_tool_choice: true,
            reasoning_replay: leveler_model::ReasoningReplayContract::NONE,
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
    fn undeclared_thinking_capability_does_not_emit_control_fields() {
        // No thinking control is invented for an undeclared capability.
        let mut req = user_req("hi");
        req.reasoning_effort = Some(ReasoningEffort::High);
        let enc = AnthropicMessagesAdapter::new()
            .encode_request(&req, &ctx(), true)
            .unwrap();
        assert!(enc.body.get("thinking").is_none(), "{}", enc.body);
        assert!(enc.body.get("output_config").is_none(), "{}", enc.body);
        assert!(enc.body.get("reasoning_effort").is_none(), "{}", enc.body);
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
    fn legacy_system_messages_never_become_user_turns() {
        let req = ModelRequest::new(
            ModelRef::new("anthropic", "claude-sonnet-5"),
            vec![
                Message::text(Role::System, "base instructions"),
                Message::text(Role::System, "skill injection"),
                Message::text(Role::User, "read src/a.rs"),
                Message::text(Role::Assistant, "reading"),
                Message::text(Role::System, "Project rules:\n--- from src/AGENTS.md ---"),
            ],
        );
        let enc = AnthropicMessagesAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap();
        assert_eq!(
            enc.body["system"],
            "base instructions\n\nskill injection\n\nProject rules:\n--- from src/AGENTS.md ---",
            "legacy control text keeps system authority"
        );
        let messages = enc.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "{:#?}", messages);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
    }

    #[test]
    fn control_context_has_system_authority_in_both_encoders() {
        let mut req = ModelRequest::new(
            ModelRef::new("test", "model"),
            vec![Message::text(Role::User, "read src/a.rs")],
        );
        req.control_context
            .push(leveler_model::PromptSegment::stable(
                "base",
                "base instructions",
            ));
        req.control_context
            .push(leveler_model::PromptSegment::variable(
                "rules",
                "Project rules: no unwrap",
            ));
        let anthropic = AnthropicMessagesAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap()
            .body;
        let openai = crate::OpenAiChatAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap()
            .body;
        assert_eq!(
            anthropic["system"],
            "base instructions\n\nProject rules: no unwrap"
        );
        assert_eq!(openai["messages"][0]["role"], "system");
        assert_eq!(openai["messages"][0]["content"], anthropic["system"]);
        assert_eq!(anthropic["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            anthropic["messages"][0]["content"][0]["text"],
            "read src/a.rs"
        );
        assert_eq!(openai["messages"][1]["content"], "read src/a.rs");
        assert_eq!(req.messages.len(), 1);
    }

    /// Providers change the wire shape of control. They do not reorder
    /// segments or rewrite authority.
    #[test]
    fn encoders_preserve_segment_order_and_authority() {
        use leveler_model::{PromptAuthority, PromptSource, SegmentLifecycle};
        let mut req = ModelRequest::new(
            ModelRef::new("test", "model"),
            vec![Message::text(Role::User, "do the task")],
        );
        let classes = [
            (
                "base",
                PromptSource::BasePrompt,
                PromptAuthority::CoreContract,
                "core contract",
            ),
            (
                "project_rules",
                PromptSource::ProjectRules {
                    paths: vec!["AGENTS.md".into()],
                },
                PromptAuthority::ProjectInstruction,
                "Project rules:\nuse the project style",
            ),
            (
                "selected_skills",
                PromptSource::Skill {
                    names: vec!["demo".into()],
                },
                PromptAuthority::UserSelectedProcedure,
                "follow the demo procedure",
            ),
            (
                "execution_state",
                PromptSource::ExecutionState,
                PromptAuthority::RuntimeFact,
                "cwd: /repo",
            ),
            (
                "memory_recall",
                PromptSource::MemoryRecall {
                    ids: vec!["mem-1".into()],
                },
                PromptAuthority::AdvisoryContext,
                "recalled note",
            ),
        ];
        for (name, source, authority, text) in classes {
            req.control_context
                .push(leveler_model::PromptSegment::control(
                    name,
                    source,
                    authority,
                    SegmentLifecycle::Turn,
                    false,
                    text,
                ));
        }
        let before = req.control_context.provenance();
        let anthropic = AnthropicMessagesAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap()
            .body;
        let openai = crate::OpenAiChatAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap()
            .body;
        let projection = leveler_model::RequestProjection::for_request(
            &req,
            leveler_model::ReasoningReplayContract::NONE,
        );
        assert_eq!(projection.control_context().provenance(), before);
        let wire = projection.control_text();
        assert_eq!(anthropic["system"], wire);
        assert_eq!(openai["messages"][0]["role"], "system");
        assert_eq!(openai["messages"][0]["content"], wire);
        for text in [
            "core contract",
            "Project rules:\nuse the project style",
            "follow the demo procedure",
            "cwd: /repo",
            "recalled note",
        ] {
            let at = wire.find(text).expect(text);
            assert!(wire.find("core contract").unwrap() <= at);
        }
        let encoded = anthropic.to_string() + &openai.to_string();
        assert!(!encoded.contains("\"authority\""));
        assert!(!encoded.contains("core_contract"));
        assert_eq!(
            before.iter().map(|item| item.authority).collect::<Vec<_>>(),
            vec![
                PromptAuthority::CoreContract,
                PromptAuthority::ProjectInstruction,
                PromptAuthority::UserSelectedProcedure,
                PromptAuthority::RuntimeFact,
                PromptAuthority::AdvisoryContext,
            ]
        );
    }

    /// OpenAI and Anthropic may spell the user transport role. They do not
    /// change whether that row is user intent.
    #[test]
    fn encoders_do_not_change_harness_user_authority() {
        use leveler_model::{
            PromptAuthority, PromptSource, ProtocolRepairKind, RuntimeNoticeKind, TranscriptOrigin,
        };
        let messages = vec![
            Message::user_input("the real request"),
            Message::user(
                "child finished",
                TranscriptOrigin::RuntimeNotice {
                    notice: RuntimeNoticeKind::ChildSettlement,
                },
            ),
            Message::user(
                "repair the call",
                TranscriptOrigin::ProtocolRepair {
                    repair: ProtocolRepairKind::InvalidToolJson,
                },
            ),
            Message::user(
                "resolve the goal",
                TranscriptOrigin::ProtocolRepair {
                    repair: ProtocolRepairKind::GoalUnresolved,
                },
            ),
            Message::user(
                "【旁问 / btw】这是主任务之外的旁问。回答需要时可以使用只读工具。\n\n不要修改工作区，不要推进或改变主任务。\n\nwhat changed?",
                TranscriptOrigin::RuntimeNotice {
                    notice: RuntimeNoticeKind::SideQuestion,
                },
            ),
            Message::text(Role::User, "legacy unknown"),
        ];
        let req = ModelRequest::new(ModelRef::new("test", "model"), messages);
        let projection = leveler_model::RequestProjection::for_request(
            &req,
            leveler_model::ReasoningReplayContract::NONE,
        );
        let before = projection.transcript_authority();
        let anthropic = AnthropicMessagesAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap()
            .body;
        let openai = crate::OpenAiChatAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap()
            .body;
        let after = leveler_model::RequestProjection::for_request(
            &req,
            leveler_model::ReasoningReplayContract::NONE,
        )
        .transcript_authority();
        assert_eq!(before, after);
        assert_eq!(
            before.iter().map(|item| item.authority).collect::<Vec<_>>(),
            vec![
                PromptAuthority::UserIntent,
                PromptAuthority::AdvisoryContext,
                PromptAuthority::CoreContract,
                PromptAuthority::CoreContract,
                PromptAuthority::CoreContract,
                PromptAuthority::Unclassified,
            ]
        );
        assert!(before.iter().any(|item| {
            item.source
                == PromptSource::RuntimeNotice {
                    notice: RuntimeNoticeKind::ChildSettlement,
                }
                && item.authority != PromptAuthority::UserIntent
        }));
        let side = before
            .iter()
            .find(|item| {
                item.source
                    == PromptSource::RuntimeNotice {
                        notice: RuntimeNoticeKind::SideQuestion,
                    }
            })
            .expect("side question");
        assert_ne!(side.authority, PromptAuthority::RuntimeFact);
        assert_eq!(side.authority, PromptAuthority::CoreContract);
        assert!(!side.authority_mismatch);
        assert_ne!(side.authority, PromptAuthority::UserIntent);
        let wire = anthropic.to_string() + &openai.to_string();
        assert!(!wire.contains("\"origin\""));
        assert!(!wire.contains("user_input"));
        assert!(!wire.contains("protocol_repair"));
        assert!(!wire.contains("user_intent"));
        assert!(!wire.contains("side_question"));
        assert!(wire.contains("the real request"));
        assert!(wire.contains("child finished"));
        assert!(wire.contains("可以使用只读工具"));
        assert!(!wire.contains("不要调用任何工具"));
        assert!(wire.contains("不要修改工作区"));
        assert!(wire.contains("不要推进或改变主任务"));
    }

    /// Extracting legacy system text leaves the tool exchange intact.
    #[test]
    fn a_tail_rule_after_a_tool_result_keeps_system_authority() {
        let call = ToolCall {
            id: ToolCallId::new("c1"),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        };
        let req = ModelRequest::new(
            ModelRef::new("anthropic", "claude-sonnet-5"),
            vec![
                Message::text(Role::System, "base"),
                Message::text(Role::User, "go"),
                Message {
                    origin: None,
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolCall { call }],
                },
                Message {
                    origin: None,
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        result: leveler_model::ToolResultContent {
                            call_id: ToolCallId::new("c1"),
                            content: "fn a() {}".into(),
                            is_error: false,
                        },
                    }],
                },
                Message::text(Role::System, "Project rules:\nno unwrap"),
            ],
        );
        let enc = AnthropicMessagesAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap();
        let messages = enc.body["messages"].as_array().unwrap();
        assert_eq!(enc.body["system"], "base\n\nProject rules:\nno unwrap");
        assert_eq!(messages.len(), 3, "{:#?}", messages);
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(
            messages[2]["content"].as_array().unwrap().len(),
            1,
            "the rule must not join the tool_result block list"
        );
    }

    /// A multi-call round: every `tool_use` of one assistant turn is answered
    /// by the next user turn's `tool_result` blocks, same ids, same order.
    #[test]
    fn a_multi_call_round_pairs_tool_use_with_tool_result_blocks() {
        let calls = ["tu_a", "tu_b"];
        let req = ModelRequest::new(
            ModelRef::new("anthropic", "claude-sonnet-5"),
            vec![
                Message::text(Role::User, "go"),
                Message {
                    origin: None,
                    role: Role::Assistant,
                    content: calls
                        .iter()
                        .map(|id| ContentPart::ToolCall {
                            call: ToolCall {
                                id: ToolCallId::new(*id),
                                name: "read_file".into(),
                                arguments: serde_json::json!({}),
                            },
                        })
                        .collect(),
                },
                Message {
                    origin: None,
                    role: Role::Tool,
                    content: calls
                        .iter()
                        .map(|id| ContentPart::ToolResult {
                            result: leveler_model::ToolResultContent {
                                call_id: ToolCallId::new(*id),
                                content: "ok".into(),
                                is_error: false,
                            },
                        })
                        .collect(),
                },
            ],
        );
        let enc = AnthropicMessagesAdapter::new()
            .encode_request(&req, &ctx(), false)
            .unwrap();
        let messages = enc.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3, "{messages:#?}");
        let ids = |m: &serde_json::Value, key: &str| -> Vec<String> {
            m["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b[key].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(ids(&messages[1], "id"), calls);
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(ids(&messages[2], "tool_use_id"), calls);
    }

    #[test]
    fn tool_result_becomes_a_user_tool_result_block() {
        let req = ModelRequest::new(
            ModelRef::new("anthropic", "claude-sonnet-5"),
            vec![Message {
                origin: None,
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
                origin: None,
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
        assert_eq!(resp.usage.input_tokens, 8);
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

#[cfg(test)]
mod thinking_contract_tests {
    use super::*;
    fn context() -> ProtocolContext {
        ProtocolContext {
            base_url: "http://unused".into(),
            model_id: "configured-model".into(),
            api_key: None,
            extra_headers: vec![],
            reasoning: Default::default(),
            parallel_tool_calls: true,
            supports_temperature: false,
            thinking_supports_forced_tool_choice: false,
            reasoning_replay: leveler_model::ReasoningReplayContract::resolve(
                ProtocolKind::AnthropicMessages,
                &Default::default(),
            ),
        }
    }
    #[test]
    fn signed_and_redacted_thinking_roundtrips_verbatim_and_in_order() {
        let content = serde_json::json!([
            {"type":"thinking","thinking":"","signature":"opaque-signature"},
            {"type":"text","text":"first"},
            {"type":"redacted_thinking","data":"opaque-redaction"},
            {"type":"tool_use","id":"c","name":"read","input":{}}
        ]);
        let adapter = AnthropicMessagesAdapter::new();
        let response = adapter
            .decode_response(
                &serde_json::to_vec(
                    &serde_json::json!({"content":content,"stop_reason":"tool_use"}),
                )
                .unwrap(),
                &context(),
            )
            .unwrap();
        let request = ModelRequest::new(
            leveler_model::ModelRef::new("p", "m"),
            vec![response.message],
        );
        let wire = adapter.encode_request(&request, &context(), false).unwrap();
        assert_eq!(
            wire.body["messages"][0]["content"], content,
            "signed blocks, including empty thinking, are protocol history"
        );
    }
    #[test]
    fn budgeted_thinking_validates_budget_and_forced_choice() {
        let mut context = context();
        context.reasoning.style = leveler_model::ReasoningStyle::BudgetedThinking {
            budget_tokens: 1024,
        };
        let mut request = ModelRequest::new(
            leveler_model::ModelRef::new("p", "m"),
            vec![Message::text(Role::User, "hi")],
        );
        request.max_output_tokens = Some(2048);
        let adapter = AnthropicMessagesAdapter::new();
        let wire = adapter.encode_request(&request, &context, false).unwrap();
        assert_eq!(
            wire.body["thinking"],
            serde_json::json!({"type":"enabled","budget_tokens":1024})
        );
        request.max_output_tokens = Some(1024);
        assert!(adapter.encode_request(&request, &context, false).is_err());
        request.max_output_tokens = Some(2048);
        request.tool_choice = ToolChoice::Required;
        assert!(adapter.encode_request(&request, &context, false).is_err());
    }
    #[test]
    fn adaptive_thinking_uses_declared_capability_and_effort() {
        let reasoning = serde_json::from_value(
            serde_json::json!({"style":"adaptive_thinking","supported_efforts":["high"]}),
        );
        assert!(
            reasoning.is_ok(),
            "adaptive thinking must be a declared protocol capability"
        );
        let mut context = context();
        context.reasoning = reasoning.unwrap();
        let mut request = ModelRequest::new(
            leveler_model::ModelRef::new("p", "m"),
            vec![Message::text(Role::User, "hi")],
        );
        request.reasoning_effort = Some(leveler_model::ReasoningEffort::High);
        let wire = AnthropicMessagesAdapter::new()
            .encode_request(&request, &context, false)
            .unwrap();
        assert_eq!(wire.body["thinking"]["type"], "adaptive");
        assert_eq!(wire.body["output_config"]["effort"], "high");
    }
}
