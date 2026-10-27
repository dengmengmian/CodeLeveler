//! Real-provider acceptance for the reasoning lifecycle.
//!
//! These tests spend real model tokens and are ignored by default. They drive
//! the production `ProviderRegistry` (the same object the agent loop uses) and
//! the production request encoder, so what they record is what the product
//! sends:
//!
//! ```text
//! TAOTOKEN_API_KEY=... BIGMODEL_API_KEY=... cargo test -p leveler-provider \
//!   --test reasoning_wire_real -- --ignored --nocapture
//! ```
//!
//! For each configured route the test records, per turn:
//!
//! * the encoded request's reasoning channel (field present? equal to capture?),
//! * the provider's HTTP-level response and the reasoning it returned,
//! * the token picture (prompt/completion/reasoning tokens, cache hits),
//! * what a Resume (serialize → deserialize) and a Compaction replay.
//!
//! Nothing here prints an API key or the reasoning text: only lengths and
//! short hashes.
//!
//! Route coverage: `taotoken` serves DeepSeek V4-Flash, Kimi K3 and Kimi K2.6;
//! `bigmodel` serves GLM-5.3 on its official coding endpoint. Ark's coding plan
//! answers 403 (expired subscription) and the Kimi coding endpoint answers 403
//! (no subscription), so those two are not asserted here.

use leveler_model::{
    ContentPart, Message, ModelProfile, ModelRef, ModelRequest, ModelRuntime, ProtocolAdapter,
    ProtocolContext, ReasoningConfig, ReasoningRetention, ReasoningStyle, RequestProjection, Role,
    RouteIdentity, ToolCall, ToolChoice, ToolDefinition, ToolResultContent,
};
use leveler_protocol::OpenAiChatAdapter;
use leveler_provider::RegistryInputs;
use leveler_provider::config::{ProviderConfig, Timeouts};
use leveler_provider::registry::ProviderRegistry;
use tokio_util::sync::CancellationToken;

const TAOTOKEN: &str = "https://taotoken.net/api/v1";
const BIGMODEL: &str = "https://open.bigmodel.cn/api/coding/paas/v4";

fn key(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} must be set for the real acceptance test (never printed)")
    })
}

fn provider(id: &str, base_url: &str, key_env: &str) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        protocol: leveler_model::ProtocolKind::OpenAiChat,
        base_url: base_url.into(),
        api_key_env: key_env.into(),
        api_key: Some(key(key_env)),
        headers: Default::default(),
        timeouts: Timeouts {
            connect_seconds: 20,
            request_seconds: 300,
            idle_stream_seconds: 180,
        },
    }
}

/// A route as configuration declares it. The replay contract is NOT declared:
/// it must resolve from the wire family, exactly as `leveler login` output does.
fn profile(
    id: &str,
    provider: &str,
    model_id: &str,
    style: ReasoningStyle,
    reasoning: bool,
) -> ModelProfile {
    ModelProfile {
        id: id.into(),
        provider: provider.into(),
        model_id: model_id.into(),
        protocol: leveler_model::ProtocolKind::OpenAiChat,
        capabilities: leveler_model::ModelCapabilities {
            streaming: true,
            tool_calling: true,
            parallel_tool_calls: false,
            structured_output: true,
            reasoning,
            vision: false,
        },
        limits: leveler_model::ModelLimits {
            context_window: 1_048_576,
            reliable_context: 786_432,
            max_output_tokens: 32_768,
            max_tool_schema_bytes: 32_768,
            max_parallel_tool_calls: 1,
            max_tool_output_bytes: None,
        },
        context_quality: None,
        thinking: None,
        reasoning: ReasoningConfig {
            style,
            supported_efforts: vec![leveler_model::ReasoningEffort::High],
            default_effort: Some(leveler_model::ReasoningEffort::High),
            // GLM's preserved thinking is a model fact; the others have no
            // request field for it.
            preserved_thinking: (model_id.starts_with("glm")).then_some(true),
            forced: model_id.starts_with("glm-5.3"),
        },
        compatibility: Default::default(),
        pricing: None,
    }
}

fn registry() -> ProviderRegistry {
    let models = vec![
        (
            "deepseek-flash",
            "taotoken",
            "deepseek-flash",
            ReasoningStyle::ThinkingFlag,
        ),
        (
            "glm-5.3",
            "bigmodel",
            "glm-5.3",
            ReasoningStyle::ThinkingFlag,
        ),
        (
            "kimi-k3",
            "taotoken",
            "kimi-k3",
            ReasoningStyle::OpenAiEffort,
        ),
        (
            "kimi-k2.6",
            "taotoken",
            "kimi-k2.6",
            ReasoningStyle::ThinkingFlag,
        ),
    ];
    ProviderRegistry::build(RegistryInputs {
        providers: vec![
            (
                provider("taotoken", TAOTOKEN, "TAOTOKEN_API_KEY"),
                Some(key("TAOTOKEN_API_KEY")),
            ),
            (
                provider("bigmodel", BIGMODEL, "BIGMODEL_API_KEY"),
                Some(key("BIGMODEL_API_KEY")),
            ),
        ],
        models: models
            .into_iter()
            .map(
                |(id, provider_id, wire, style)| leveler_provider::catalog::ModelConfigFile {
                    profile: profile(id, provider_id, wire, style, true),
                    policy: None,
                },
            )
            .collect(),
    })
    .expect("the acceptance registry builds")
}

fn tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "clock".into(),
        description: "Return the time for a timezone.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "tz": { "type": "string" } },
            "required": ["tz"]
        }),
    }]
}

/// The endpoint this acceptance run uses for a provider id.
fn base_url(provider: &str) -> &'static str {
    match provider {
        "bigmodel" => BIGMODEL,
        _ => TAOTOKEN,
    }
}

/// The route's encoded request, built from the same profile facts the registry
/// uses. The projection is the one the runtime would hand over.
fn encoded(
    profile: &ModelProfile,
    messages: &[Message],
    route: &RouteIdentity,
) -> serde_json::Value {
    let contract = leveler_model::ReasoningReplayContract::for_route(profile);
    let mut request = ModelRequest::new(
        ModelRef::new(profile.provider.clone(), profile.id.clone()),
        messages.to_vec(),
    );
    request.tools = tools();
    request.tool_choice = ToolChoice::Auto;
    request.reasoning_effort = profile.reasoning.default_effort;
    request.max_output_tokens = Some(2048);
    request.projection = Some(RequestProjection::project_for_route(
        messages,
        &request.tools,
        contract,
        ReasoningRetention::All,
        Some(route),
    ));
    let context = ProtocolContext {
        base_url: base_url(&profile.provider).into(),
        model_id: profile.model_id.clone(),
        api_key: Some("[redacted]".into()),
        extra_headers: Vec::new(),
        reasoning: profile.reasoning.clone(),
        parallel_tool_calls: false,
        supports_temperature: true,
        thinking_supports_forced_tool_choice: true,
        reasoning_replay: contract,
    };
    OpenAiChatAdapter::new()
        .encode_request(&request, &context, false)
        .expect("the request encodes")
        .body
}

fn reasoning_part(message: &Message) -> Option<&str> {
    message.content.iter().find_map(|part| match part {
        ContentPart::Reasoning { text } => Some(text.as_str()),
        _ => None,
    })
}

fn text_of(message: &Message) -> String {
    message.text_content()
}

fn first_call(message: &Message) -> Option<ToolCall> {
    message.content.iter().find_map(|part| match part {
        ContentPart::ToolCall { call } => Some(call.clone()),
        _ => None,
    })
}

fn sha8(text: &str) -> String {
    // FNV-1a: a stable short digest for evidence lines. Not a security hash.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")[..12].to_string()
}

fn evidence(label: &str, response: &leveler_model::ModelResponse, body: &serde_json::Value) {
    let reasoning = reasoning_part(&response.message).unwrap_or("");
    let replayed: Vec<String> = body["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter_map(|message| message.get("reasoning_content"))
                .map(|value| match value.as_str() {
                    Some(text) => format!("key({} chars, {})", text.chars().count(), sha8(text)),
                    None => "non-string".to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    eprintln!(
        "[{label}] status=200 finish={:?} text={:?} reasoning_len={} reasoning_sha={} \
         prompt_tokens={} completion_tokens={} reasoning_tokens={:?} cached={} \
         wire_reasoning={:?} wire_thinking={:?}",
        response.finish_reason,
        text_of(&response.message),
        reasoning.chars().count(),
        sha8(reasoning),
        response.usage.input_tokens,
        response.usage.output_tokens,
        response.usage.reasoning_tokens,
        response.usage.cached_input_tokens,
        replayed,
        body.get("thinking"),
    );
}

#[tokio::test]
#[ignore = "spends real model tokens; see the module docs for the command"]
async fn real_routes_capture_and_replay_their_own_reasoning() {
    let registry = registry();
    let mut profiles: Vec<ModelProfile> = Vec::new();
    for id in ["deepseek-flash", "glm-5.3", "kimi-k3", "kimi-k2.6"] {
        let provider_id = match id {
            "glm-5.3" => "bigmodel",
            _ => "taotoken",
        };
        profiles.push(
            registry
                .profile(&ModelRef::new(provider_id, id))
                .await
                .expect("profile"),
        );
    }

    for profile in profiles {
        let model = ModelRef::new(profile.provider.clone(), profile.id.clone());
        let route = registry.route_identity(&model).expect("a real route");
        assert_eq!(
            leveler_model::ReasoningReplayContract::for_route(&profile).arm_name(),
            match profile.model_id.as_str() {
                "deepseek-flash" | "kimi-k3" => "when_tools+empty",
                _ => "when_tools",
            },
            "{}: the resolved contract comes from the wire family",
            profile.model_id
        );

        // Turn 1: a tool-bearing request. The model should reason, then call.
        let messages = vec![Message::user_input(
            "What time is it in UTC? Call the clock tool, then tell me the answer.",
        )];
        let body = encoded(&profile, &messages, &route);
        let mut request = ModelRequest::new(model.clone(), messages.clone());
        request.tools = tools();
        request.tool_choice = ToolChoice::Auto;
        request.reasoning_effort = profile.reasoning.default_effort;
        request.max_output_tokens = Some(2048);
        request.projection = Some(RequestProjection::project_for_route(
            &messages,
            &request.tools,
            leveler_model::ReasoningReplayContract::for_route(&profile),
            ReasoningRetention::All,
            Some(&route),
        ));
        let first = registry
            .generate(request, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("{}: turn 1 failed: {error}", profile.model_id));
        evidence(&format!("{} turn1", profile.model_id), &first, &body);

        let call = first_call(&first.message).unwrap_or_else(|| {
            panic!(
                "{}: turn 1 returned no tool call; text={:?}",
                profile.model_id,
                text_of(&first.message)
            )
        });
        // The assistant turn as the runtime assembles it: real response message
        // plus the route stamp the kernel adds.
        let mut assistant = first.message.clone();
        assistant.route = Some(route.clone());
        let captured = reasoning_part(&assistant).map(str::to_string);

        let mut history = messages.clone();
        history.push(assistant.clone());
        history.push(Message::from_parts(
            Role::Tool,
            vec![ContentPart::ToolResult {
                result: ToolResultContent {
                    call_id: call.id.clone(),
                    content: "12:00 UTC".into(),
                    is_error: false,
                },
            }],
            None,
        ));

        // Turn 2: the same route continues. Its own reasoning must travel back
        // verbatim on the wire, and the provider must accept it.
        let body2 = encoded(&profile, &history, &route);
        let assistant_on_wire = body2["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "assistant")
            .expect("the replayed assistant turn");
        let contract = leveler_model::ReasoningReplayContract::for_route(&profile);
        match &captured {
            Some(text) => assert_eq!(
                assistant_on_wire["reasoning_content"], *text,
                "{}: captured reasoning is replayed verbatim",
                profile.model_id
            ),
            // A provider can complete a turn without producing reasoning. That
            // is the EMPTY state, not a missing record: the route's own
            // protocol decides whether the channel is still spelled (empty
            // string) or omitted.
            None if contract.requires_empty_key(true) => assert_eq!(
                assistant_on_wire["reasoning_content"], "",
                "{}: a measured empty turn carries the legal empty key",
                profile.model_id
            ),
            None => assert!(
                assistant_on_wire.get("reasoning_content").is_none(),
                "{}: no captured reasoning, no invented content",
                profile.model_id
            ),
        }
        if profile.model_id.starts_with("glm") {
            assert_eq!(
                body2["thinking"]["clear_thinking"], false,
                "GLM sends preserved thinking as the route declares it"
            );
        }

        let mut second_request = ModelRequest::new(model.clone(), history.clone());
        second_request.tools = tools();
        second_request.reasoning_effort = profile.reasoning.default_effort;
        second_request.max_output_tokens = Some(2048);
        second_request.projection = Some(RequestProjection::project_for_route(
            &history,
            &second_request.tools,
            leveler_model::ReasoningReplayContract::for_route(&profile),
            ReasoningRetention::All,
            Some(&route),
        ));
        let second = registry
            .generate(second_request, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("{}: turn 2 failed: {error}", profile.model_id));
        evidence(&format!("{} turn2", profile.model_id), &second, &body2);
        assert!(
            !text_of(&second.message).trim().is_empty(),
            "{}: the tool loop completes with an answer",
            profile.model_id
        );

        // Resume: the transcript survives serialization and still replays the
        // same reasoning, from the same recorded route.
        let payload = serde_json::to_string(&history).expect("the transcript serializes");
        let resumed: Vec<Message> = serde_json::from_str(&payload).expect("the transcript loads");
        assert_eq!(
            resumed[1].route.as_ref(),
            Some(&route),
            "{}: the route survives resume",
            profile.model_id
        );
        let body3 = encoded(&profile, &resumed, &route);
        if let Some(text) = &captured {
            assert_eq!(
                body3["messages"][1]["reasoning_content"], *text,
                "{}: resume replays the same reasoning",
                profile.model_id
            );
        }

        // Compaction: the retained turn keeps its reasoning, the elided rounds
        // release theirs with the turn.
        let mut longer = history.clone();
        for round in 0..3 {
            let mut extra = assistant.clone();
            extra.content.push(ContentPart::Text {
                text: format!("round {round}"),
            });
            longer.push(extra);
            longer.push(Message::from_parts(
                Role::Tool,
                vec![ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: call.id.clone(),
                        content: format!("result {round}"),
                        is_error: false,
                    },
                }],
                None,
            ));
        }
        let folded = leveler_context::compact_messages(
            &longer,
            2,
            0,
            Some("the user asked for the UTC time"),
            Some("tell me the UTC time"),
        );
        let body4 = encoded(&profile, &folded, &route);
        let replayed: Vec<&str> = body4["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["reasoning_content"].as_str())
            .collect();
        eprintln!(
            "[{} compacted] messages={} replayed_reasoning_len={:?}",
            profile.model_id,
            body4["messages"].as_array().unwrap().len(),
            replayed
                .iter()
                .map(|text| text.chars().count())
                .collect::<Vec<_>>()
        );
        let mut folded_request = ModelRequest::new(model.clone(), folded.clone());
        folded_request.tools = tools();
        folded_request.reasoning_effort = profile.reasoning.default_effort;
        folded_request.max_output_tokens = Some(2048);
        folded_request.projection = Some(RequestProjection::project_for_route(
            &folded,
            &folded_request.tools,
            leveler_model::ReasoningReplayContract::for_route(&profile),
            ReasoningRetention::All,
            Some(&route),
        ));
        let after_fold = registry
            .generate(folded_request, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("{}: post-fold turn failed: {error}", profile.model_id));
        evidence(
            &format!("{} post-fold", profile.model_id),
            &after_fold,
            &body4,
        );
    }
}

/// A real model switch: A runs a tool round, B continues the SAME transcript.
///
/// The assertion that matters is on the wire B receives: A's reasoning is gone,
/// everything the task depends on is there, and B answers normally.
#[tokio::test]
#[ignore = "spends real model tokens; see the module docs for the command"]
async fn a_real_model_switch_keeps_the_task_and_drops_private_reasoning() {
    let registry = registry();
    // Kimi K3 always thinks, so this test can prove that A's reasoning exists
    // and that B never receives it. DeepSeek's provider-side default can finish
    // a trivial turn without reasoning (measured), which is a different fact,
    // covered by the empty-state assertions in the other test.
    let a = registry
        .profile(&ModelRef::new("taotoken", "kimi-k3"))
        .await
        .expect("route A");
    let b = registry
        .profile(&ModelRef::new("bigmodel", "glm-5.3"))
        .await
        .expect("route B");
    let (route_a, route_b) = (
        registry
            .route_identity(&ModelRef::new("taotoken", "kimi-k3"))
            .unwrap(),
        registry
            .route_identity(&ModelRef::new("bigmodel", "glm-5.3"))
            .unwrap(),
    );
    assert!(!route_a.is_same_route(&route_b));

    // A live model can occasionally answer a trivial tool prompt without
    // emitting reasoning. Retry with a prompt that asks for deliberation so the
    // handover has real private thinking to (not) carry; the attempt count is
    // reported, never hidden.
    let mut attempt = 0;
    let (ask, first, a_reasoning) = loop {
        attempt += 1;
        let ask = vec![Message::user_input(format!(
            "Think about which tool answers this, then call it: what time is it in UTC? \
             (attempt {attempt})"
        ))];
        let mut request =
            ModelRequest::new(ModelRef::new(a.provider.clone(), a.id.clone()), ask.clone());
        request.tools = tools();
        request.reasoning_effort = a.reasoning.default_effort;
        request.max_output_tokens = Some(2048);
        request.projection = Some(RequestProjection::project_for_route(
            &ask,
            &request.tools,
            leveler_model::ReasoningReplayContract::for_route(&a),
            ReasoningRetention::All,
            Some(&route_a),
        ));
        let first = registry
            .generate(request, CancellationToken::new())
            .await
            .expect("route A turn 1");
        let Some(reasoning) = reasoning_part(&first.message).map(str::to_string) else {
            eprintln!(
                "[cross-model] route A attempt {attempt}: no reasoning captured \
                 (reasoning_tokens={:?}); retrying",
                first.usage.reasoning_tokens
            );
            assert!(attempt < 3, "route A never returned reasoning");
            continue;
        };
        break (ask, first, reasoning);
    };
    let call = first_call(&first.message).expect("A calls the tool");
    let mut assistant = first.message.clone();
    assistant.route = Some(route_a.clone());

    let mut history = ask.clone();
    history.push(assistant);
    history.push(Message::from_parts(
        Role::Tool,
        vec![ContentPart::ToolResult {
            result: ToolResultContent {
                call_id: call.id.clone(),
                content: "12:00 UTC".into(),
                is_error: false,
            },
        }],
        None,
    ));

    // B continues: its encoded request must not contain A's reasoning.
    let body = encoded(&b, &history, &route_b);
    let assistant_on_wire = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("the handed-over assistant turn");
    assert!(
        assistant_on_wire
            .get("reasoning_content")
            .and_then(|value| value.as_str())
            .is_none_or(|text| !text.contains(&a_reasoning) && !a_reasoning.contains(text)),
        "route B must not receive route A's private reasoning: {assistant_on_wire}"
    );
    assert!(
        assistant_on_wire["tool_calls"].is_array(),
        "the tool call A made is part of the handover"
    );
    assert_eq!(
        body["thinking"]["clear_thinking"], false,
        "B still gets its own preserved-thinking request"
    );

    let mut request = ModelRequest::new(
        ModelRef::new(b.provider.clone(), b.id.clone()),
        history.clone(),
    );
    request.tools = tools();
    request.reasoning_effort = b.reasoning.default_effort;
    request.max_output_tokens = Some(2048);
    request.projection = Some(RequestProjection::project_for_route(
        &history,
        &request.tools,
        leveler_model::ReasoningReplayContract::for_route(&b),
        ReasoningRetention::All,
        Some(&route_b),
    ));
    let continuation = registry
        .generate(request, CancellationToken::new())
        .await
        .expect("route B continues the task");
    evidence("cross-model handover (B)", &continuation, &body);
    assert!(
        !text_of(&continuation.message).trim().is_empty(),
        "B completes the task instead of refusing because of provenance"
    );

    // The projection B ran under reports the handover honestly.
    let projection = RequestProjection::project_for_route(
        &history,
        &tools(),
        leveler_model::ReasoningReplayContract::for_route(&b),
        ReasoningRetention::All,
        Some(&route_b),
    );
    eprintln!(
        "[cross-model] foreign_turns={} unknown={} empty={} carried={}",
        projection.summary().foreign_turns,
        projection.summary().unknown_provenance_turns,
        projection.summary().empty_turns,
        projection.summary().carried_turns
    );
    assert_eq!(projection.summary().foreign_turns, 1);
    assert_eq!(projection.summary().carried_turns, 0);
}

/// Streaming capture on a real route: the assembled message carries the
/// provider's reasoning, and the same route replays it on the next request.
#[tokio::test]
#[ignore = "spends real model tokens; see the module docs for the command"]
async fn a_real_streamed_round_captures_reasoning_for_the_next_turn() {
    use futures::StreamExt;

    let registry = registry();
    let profile =
        futures::executor::block_on(registry.profile(&ModelRef::new("taotoken", "kimi-k3")))
            .expect("kimi-k3 route");
    let model = ModelRef::new(profile.provider.clone(), profile.id.clone());
    let route = registry.route_identity(&model).unwrap();
    let ask = vec![Message::user_input(
        "What time is it in UTC? Call the clock tool, then tell me the answer.",
    )];
    let mut request = ModelRequest::new(model.clone(), ask.clone());
    request.tools = tools();
    request.reasoning_effort = profile.reasoning.default_effort;
    request.max_output_tokens = Some(2048);
    request.projection = Some(RequestProjection::project_for_route(
        &ask,
        &request.tools,
        leveler_model::ReasoningReplayContract::for_route(&profile),
        ReasoningRetention::All,
        Some(&route),
    ));
    let mut stream = registry
        .stream(request, CancellationToken::new())
        .await
        .expect("the stream opens");
    let mut reasoning = String::new();
    let mut text = String::new();
    let mut call = None;
    while let Some(event) = stream.next().await {
        match event.expect("a stream event") {
            leveler_model::ModelEvent::ReasoningDelta { delta } => reasoning.push_str(&delta),
            leveler_model::ModelEvent::TextDelta { delta } => text.push_str(&delta),
            leveler_model::ModelEvent::ToolCallCompleted { call: completed } => {
                call = Some(completed)
            }
            // A stream is covered by its terminal event below.
            _ => {}
        }
    }
    eprintln!(
        "[kimi-k3 stream] reasoning_len={} reasoning_sha={} text_len={} tool_call={:?}",
        reasoning.chars().count(),
        sha8(&reasoning),
        text.chars().count(),
        call.as_ref().map(|call| call.name.clone())
    );
    assert!(
        !reasoning.trim().is_empty(),
        "a thinking-mode route streams reasoning deltas"
    );
    let call = call.expect("the streamed round called the tool");

    let mut assistant = Message::from_parts(
        Role::Assistant,
        {
            let mut content = vec![ContentPart::Reasoning {
                text: reasoning.clone(),
            }];
            if !text.is_empty() {
                content.push(ContentPart::Text { text: text.clone() });
            }
            content.push(ContentPart::ToolCall { call: call.clone() });
            content
        },
        None,
    );
    assistant.route = Some(route.clone());
    let mut history = ask;
    history.push(assistant);
    history.push(Message::from_parts(
        Role::Tool,
        vec![ContentPart::ToolResult {
            result: ToolResultContent {
                call_id: call.id.clone(),
                content: "12:00 UTC".into(),
                is_error: false,
            },
        }],
        None,
    ));
    let body = encoded(&profile, &history, &route);
    assert_eq!(
        body["messages"][1]["reasoning_content"], reasoning,
        "the streamed reasoning is what the next request replays"
    );
    assert_eq!(
        body["messages"][1]["reasoning_content"]
            .as_str()
            .map(|text| sha8(text)),
        Some(sha8(&reasoning))
    );
}

/// A real route with no historical-reasoning channel receives no such field —
/// even though its history contains reasoning produced elsewhere.
///
/// This is the "do not send what nobody asked for" half of the contract, proven
/// against a live OpenAI-compatible route rather than a fixture.
#[tokio::test]
#[ignore = "spends real model tokens; see the module docs for the command"]
async fn a_real_route_without_a_replay_channel_gets_no_reasoning_field() {
    let plain = profile(
        "gpt-5.5",
        "taotoken",
        "gpt-5.5",
        ReasoningStyle::None,
        false,
    );
    assert_eq!(
        leveler_model::ReasoningReplayContract::for_route(&plain),
        leveler_model::ReasoningReplayContract::NONE,
        "a model family with no reasoning channel resolves to no replay"
    );
    let route = RouteIdentity {
        provider: "taotoken".into(),
        model: "gpt-5.5".into(),
        api: Some(leveler_model::ProtocolKind::OpenAiChat),
        deployment: Some(TAOTOKEN.into()),
    };

    let mut assistant = Message::from_parts(
        Role::Assistant,
        vec![
            ContentPart::Reasoning {
                text: "reasoning from an earlier route".into(),
            },
            ContentPart::Text {
                text: "12:00 UTC".into(),
            },
        ],
        None,
    );
    assistant.route = Some(RouteIdentity {
        provider: "bigmodel".into(),
        model: "glm-5.3".into(),
        api: Some(leveler_model::ProtocolKind::OpenAiChat),
        deployment: Some(BIGMODEL.into()),
    });
    let history = vec![
        Message::user_input("What time is it in UTC?"),
        assistant,
        Message::from_parts(
            Role::Assistant,
            vec![ContentPart::Text {
                text: "the answer is noon".into(),
            }],
            None,
        )
        .with_route(route.clone()),
    ];

    let body = encoded(&plain, &history, &route);
    assert!(
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|message| message.get("reasoning_content").is_none()),
        "no reasoning channel means no field, whatever the history holds: {body}"
    );
    assert!(body.get("thinking").is_none(), "{body}");

    let mut request = ModelRequest::new(ModelRef::new("taotoken", "gpt-5.5"), history.clone());
    request.tools = tools();
    request.max_output_tokens = Some(2048);
    request.projection = Some(RequestProjection::project_for_route(
        &history,
        &request.tools,
        leveler_model::ReasoningReplayContract::NONE,
        ReasoningRetention::All,
        Some(&route),
    ));
    // The route is not in the registry's model list; build its request directly
    // through the same adapter + transport the registry uses instead.
    let context = ProtocolContext {
        base_url: TAOTOKEN.into(),
        model_id: "gpt-5.5".into(),
        api_key: Some(key("TAOTOKEN_API_KEY")),
        extra_headers: Vec::new(),
        reasoning: plain.reasoning.clone(),
        parallel_tool_calls: true,
        supports_temperature: true,
        thinking_supports_forced_tool_choice: true,
        reasoning_replay: leveler_model::ReasoningReplayContract::NONE,
    };
    let encoded_request = OpenAiChatAdapter::new()
        .encode_request(&request, &context, false)
        .expect("encodes");
    let status = send_plain(&context, &encoded_request).await;
    eprintln!("[gpt-5.5 no-replay-channel] status={status}");
    assert_eq!(
        status, 200,
        "the provider accepts the field-less transcript"
    );
}

/// Minimal live POST for a route the acceptance registry does not carry.
async fn send_plain(context: &ProtocolContext, encoded: &leveler_model::EncodedRequest) -> u16 {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap();
    let response = client
        .post(format!(
            "{}{}",
            context.base_url.trim_end_matches('/'),
            encoded.path
        ))
        .bearer_auth(context.api_key.clone().unwrap_or_default())
        .json(&encoded.body)
        .send()
        .await
        .expect("the request is sent");
    let status = response.status().as_u16();
    if status != 200 {
        eprintln!(
            "[gpt-5.5] body={}",
            response.text().await.unwrap_or_default()
        );
    }
    status
}
