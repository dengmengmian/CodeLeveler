//! Provider independence for the context decision.
//!
//! CodeLeveler decides what a request carries; a protocol adapter decides how
//! to spell it. These tests encode ONE decision through both adapters and
//! check that neither one re-decides: nothing is added that the decision
//! dropped, nothing is dropped that the decision kept, and the wire difference
//! between the two routes is a channel the route does or does not have.
//!
//! The route facts themselves (which scope a protocol resolves to) are unit
//! tested next to [`leveler_model::ReasoningReplayContract::resolve`]; what is
//! tested here is that an adapter cannot branch on anything else.

use leveler_core::ToolCallId;
use leveler_model::{
    ContentPart, Message, MissingReasoningReplay, ModelRef, ModelRequest, ProtocolAdapter,
    ProtocolContext, ReasoningConfig, ReasoningReplayContract, ReasoningReplayScope,
    ReasoningRetention, RequestProjection, Role, ToolCall, ToolDefinition, ToolResultContent,
};
use leveler_protocol::{AnthropicMessagesAdapter, OpenAiChatAdapter};

const THINKING: &str = "read the file, then answer";

fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "read_file".to_string(),
        description: "read a file".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
    }
}

/// A user turn, a thinking tool turn with its result, and a final answer.
fn unsigned_history() -> Vec<Message> {
    vec![
        Message::user_input("what does it define?"),
        Message::from_parts(
            Role::Assistant,
            vec![
                ContentPart::Reasoning {
                    text: THINKING.to_string(),
                },
                ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new("call-1"),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path": "src/lib.rs"}),
                    },
                },
            ],
            None,
        ),
        Message::from_parts(
            Role::Tool,
            vec![ContentPart::ToolResult {
                result: ToolResultContent {
                    call_id: ToolCallId::new("call-1"),
                    content: "pub fn value() -> i32 { 1 }".into(),
                    is_error: false,
                },
            }],
            None,
        ),
        Message::text(Role::Assistant, "it defines value()"),
    ]
}

/// The same shape with provider-authenticated thinking.
fn signed_history() -> Vec<Message> {
    let mut messages = unsigned_history();
    messages[1] = Message::from_parts(
        Role::Assistant,
        vec![
            ContentPart::SignedReasoning {
                text: THINKING.to_string(),
                signature: "sig-abc".to_string(),
            },
            ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new("call-1"),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "src/lib.rs"}),
                },
            },
        ],
        None,
    );
    messages
}

fn context(model_id: &str, reasoning_replay: ReasoningReplayContract) -> ProtocolContext {
    ProtocolContext {
        base_url: "https://example.invalid".into(),
        model_id: model_id.into(),
        api_key: Some("secret".into()),
        extra_headers: vec![],
        reasoning: ReasoningConfig::default(),
        parallel_tool_calls: true,
        supports_temperature: true,
        thinking_supports_forced_tool_choice: true,
        reasoning_replay,
    }
}

fn when_tools() -> ReasoningReplayContract {
    ReasoningReplayContract::raw_field(
        ReasoningReplayScope::WhenToolsPresent,
        MissingReasoningReplay::EmptyString,
    )
}

fn request(messages: Vec<Message>, projection: RequestProjection) -> ModelRequest {
    let mut request = ModelRequest::new(ModelRef::new("route", "m"), messages);
    request.tools = vec![tool()];
    request.tool_choice = leveler_model::ToolChoice::Auto;
    request.max_output_tokens = Some(1024);
    request.projection = Some(projection);
    request
}

/// Reasoning strings the OpenAI wire carries on assistant turns.
fn openai_reasoning(body: &serde_json::Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "assistant")
        .filter_map(|message| message["reasoning_content"].as_str())
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .collect()
}

/// Reasoning strings the Messages wire carries as thinking blocks.
fn anthropic_thinking(body: &serde_json::Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .filter(|block| block["type"] == "thinking")
        .filter_map(|block| block["thinking"].as_str().map(str::to_string))
        .collect()
}

/// One CodeLeveler decision, two wires. The route that validates reasoning on
/// tool-bearing requests carries it; the route with no channel for unsigned
/// thinking carries none of it and fabricates no block.
#[test]
fn one_decision_is_what_both_adapters_encode() {
    let projection = RequestProjection::project(
        &unsigned_history(),
        &[tool()],
        when_tools(),
        ReasoningRetention::All,
    );
    assert_eq!(projection.summary().carried_turns, 1);
    assert_eq!(projection.summary().captured_turns, 1);

    let request = request(unsigned_history(), projection.clone());
    let openai = OpenAiChatAdapter::new()
        .encode_request(&request, &context("m", when_tools()), false)
        .unwrap()
        .body;
    let anthropic = AnthropicMessagesAdapter::new()
        .encode_request(
            &request,
            &context("m", ReasoningReplayContract::NONE),
            false,
        )
        .unwrap()
        .body;

    assert_eq!(
        openai_reasoning(&openai),
        vec![THINKING.to_string()],
        "the route's channel carries exactly the decision: {openai}"
    );
    assert!(
        anthropic_thinking(&anthropic).is_empty(),
        "a route without a channel fabricates no thinking block: {anthropic}"
    );
    assert!(
        !anthropic.to_string().contains("sig-abc"),
        "no signature is invented for a route that never authenticated one"
    );
}

/// An authenticated thinking block is carried by the route that authenticates
/// it — from the same decision shape, with the block kept in the surface.
#[test]
fn an_authenticated_block_is_carried_by_the_route_that_authenticates_it() {
    let anthropic_route = ReasoningReplayContract::signed_block();
    let projection = RequestProjection::project(
        &signed_history(),
        &[tool()],
        anthropic_route,
        ReasoningRetention::All,
    );
    let request = request(signed_history(), projection);
    let anthropic = AnthropicMessagesAdapter::new()
        .encode_request(&request, &context("m", anthropic_route), false)
        .unwrap()
        .body;
    assert_eq!(
        anthropic_thinking(&anthropic),
        vec![THINKING.to_string()],
        "the authenticated block survives with its signature: {anthropic}"
    );
    let block = anthropic["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .find(|block| block["type"] == "thinking")
        .expect("a thinking block");
    assert_eq!(block["signature"], "sig-abc");

    // The same decision through the chat protocol: it has no field for an
    // authenticated block, so it writes none rather than downgrading it to
    // plain reasoning it never produced.
    let openai = OpenAiChatAdapter::new()
        .encode_request(&request, &context("m", anthropic_route), false)
        .unwrap()
        .body;
    assert!(
        openai_reasoning(&openai).is_empty(),
        "an authenticated block is not re-spelled as plain reasoning: {openai}"
    );
}

/// The adapter encodes the projection it was handed; it never re-decides
/// retention. A request whose decision carried reasoning carries it even when
/// the encoding context would have resolved to no channel, and a request whose
/// decision dropped reasoning stays dropped even when the context would have
/// carried it.
#[test]
fn the_adapter_encodes_the_decision_and_never_re_decides_it() {
    // Decision: carry.
    let carried = RequestProjection::project(
        &unsigned_history(),
        &[tool()],
        when_tools(),
        ReasoningRetention::All,
    );
    let carried_request = request(unsigned_history(), carried);
    let body = OpenAiChatAdapter::new()
        .encode_request(
            &carried_request,
            &context("m", ReasoningReplayContract::NONE),
            false,
        )
        .unwrap()
        .body;
    assert_eq!(
        openai_reasoning(&body),
        vec![THINKING.to_string()],
        "the encoder must not strip what the decision kept: {body}"
    );

    // Decision: drop (this route has no channel).
    let dropped = RequestProjection::project(
        &unsigned_history(),
        &[tool()],
        ReasoningReplayContract::NONE,
        ReasoningRetention::All,
    );
    let dropped_request = request(unsigned_history(), dropped);
    let body = OpenAiChatAdapter::new()
        .encode_request(&dropped_request, &context("m", when_tools()), false)
        .unwrap()
        .body;
    assert!(
        openai_reasoning(&body).is_empty(),
        "the encoder must not add what the decision dropped: {body}"
    );
}

/// The carried set depends on the route's resolved contract and the requested
/// arm — never on the model id or provider name in the request.
#[test]
fn the_decision_does_not_change_with_the_route_name() {
    let projection = RequestProjection::project(
        &unsigned_history(),
        &[tool()],
        when_tools(),
        ReasoningRetention::All,
    );
    let first = request(unsigned_history(), projection.clone());
    let second = {
        let mut second = request(unsigned_history(), projection);
        second.model = ModelRef::new("another-provider", "another-model");
        second
    };
    let encode = |request: &ModelRequest| {
        OpenAiChatAdapter::new()
            .encode_request(request, &context("also-ignored", when_tools()), false)
            .unwrap()
            .body
    };
    assert_eq!(
        openai_reasoning(&encode(&first)),
        openai_reasoning(&encode(&second)),
        "a model or provider name cannot change what the decision carries"
    );
}
