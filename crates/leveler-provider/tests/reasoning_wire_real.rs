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
//! Route coverage: `taotoken` serves DeepSeek V4-Flash and Kimi K3; `bigmodel`
//! serves GLM-5.3 on its official coding endpoint. The official Kimi Code
//! endpoint (`api.kimi.com/coding/v1`) answers 401 for the credential available
//! here, so the Kimi K2.7/K2.8 protocol is asserted at the request-serialization
//! level and recorded as NOT VERIFIED on the live endpoint — an older Kimi
//! generation on a gateway never stands in for it.

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
/// The official Kimi Code endpoint (docs: platform.kimi.ai, kimi.com/code).
const KIMI_CODING: &str = "https://api.kimi.com/coding/v1";

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
        // The official Kimi Code ids. Every supported Kimi (K2.7 and later)
        // always thinks and always preserves, and takes no `thinking` field:
        // K3/K3.1 are addressed by `reasoning_effort`, K2.8 Preview the same,
        // and K2.7 Code has no effort knob at all.
        ("k3", "kimi-coding", "k3", ReasoningStyle::OpenAiEffort),
        (
            "k3.1-pro",
            "kimi-coding",
            "k3.1-pro",
            ReasoningStyle::OpenAiEffort,
        ),
        (
            "kimi-for-coding",
            "kimi-coding",
            "kimi-for-coding",
            ReasoningStyle::OpenAiEffort,
        ),
        (
            "kimi-for-coding-highspeed",
            "kimi-coding",
            "kimi-for-coding-highspeed",
            ReasoningStyle::None,
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
            (
                provider("kimi-coding", KIMI_CODING, "KIMI_API_KEY"),
                Some(key("KIMI_API_KEY")),
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
        "kimi-coding" => KIMI_CODING,
        _ => TAOTOKEN,
    }
}

/// The environment variable that holds a provider's key. Never printed.
fn key_env_for(provider: &str) -> &'static str {
    match provider {
        "bigmodel" => "BIGMODEL_API_KEY",
        "kimi-coding" => "KIMI_API_KEY",
        _ => "TAOTOKEN_API_KEY",
    }
}

/// The live context for one route: same facts the registry holds, plus the key
/// the transport needs. Used by the focused single-request checks below.
fn live_context(
    profile: &ModelProfile,
    contract: leveler_model::ReasoningReplayContract,
) -> ProtocolContext {
    ProtocolContext {
        base_url: base_url(&profile.provider).into(),
        model_id: profile.model_id.clone(),
        api_key: Some(key(key_env_for(&profile.provider))),
        extra_headers: Vec::new(),
        reasoning: profile.reasoning.clone(),
        parallel_tool_calls: false,
        supports_temperature: true,
        thinking_supports_forced_tool_choice: true,
        reasoning_replay: contract,
    }
}

/// Send one encoded request and return its status plus the decoded body.
async fn send_json(
    context: &ProtocolContext,
    encoded: &leveler_model::EncodedRequest,
) -> (u16, serde_json::Value) {
    send_body(context, &encoded.body).await
}

/// Send a request body the production encoder produced.
async fn send_body(
    context: &ProtocolContext,
    body: &serde_json::Value,
) -> (u16, serde_json::Value) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap();
    let response = client
        .post(format!(
            "{}/chat/completions",
            context.base_url.trim_end_matches('/')
        ))
        .bearer_auth(context.api_key.clone().unwrap_or_default())
        .json(body)
        .send()
        .await
        .expect("the request is sent");
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if status != 200 {
        eprintln!("[status {status}] body={text}");
    }
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

/// The wire's assistant turn, if the body carries one.
fn assistant_on_wire(body: &serde_json::Value) -> Option<&serde_json::Value> {
    body["messages"]
        .as_array()?
        .iter()
        .find(|message| message["role"] == "assistant")
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
    for id in ["deepseek-flash", "glm-5.3", "kimi-k3"] {
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
                // The tool-keyed family: replay is required with `tools` and
                // ignored by the endpoint without them.
                "deepseek-flash" => "when_tools+empty",
                // K3 preserves unconditionally; GLM's bundled profile declares
                // preserved thinking, so every request owes the chain back.
                "kimi-k3" => "always+empty",
                _ => "always",
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

/// Phase A/B on real endpoints: the preserved-thinking spelling each family
/// actually accepts, and the exit request a deployment may legally send.
///
/// * GLM-5.3 with preserved thinking OFF and replay off must send
///   `clear_thinking: true` (a real mode change) and no historical reasoning.
/// * A Kimi K2.7+ route that declares preserved thinking is refused: the mode
///   is unconditional there and has no request field, so the resolved contract
///   still replays without inventing one.
/// * Kimi K3 / K2.7-code take no `thinking` field at all, and still replay.
#[tokio::test]
#[ignore = "spends real model tokens; see the module docs for the command"]
async fn real_routes_accept_the_preserved_mode_their_family_spells() {
    let registry = registry();

    // --- GLM: the legal exit request -------------------------------------
    let mut glm = registry
        .profile(&ModelRef::new("bigmodel", "glm-5.3"))
        .await
        .expect("glm-5.3");
    // The deployment decides to stop preserving (and therefore stop replaying).
    glm.reasoning.preserved_thinking = Some(false);
    glm.compatibility.reasoning_replay_scope = Some(leveler_model::ReasoningReplayScope::Never);
    leveler_model::ReasoningReplayContract::check_route(&glm)
        .expect("a cleared mode is a legal exit");
    let contract = leveler_model::ReasoningReplayContract::for_route(&glm);
    assert_eq!(contract.arm_name(), "never");

    let handled = Message::from_parts(
        Role::Assistant,
        vec![
            ContentPart::Reasoning {
                text: "earlier analysis that must not travel".into(),
            },
            ContentPart::Text {
                text: "12:00 UTC".into(),
            },
        ],
        None,
    )
    .with_route(RouteIdentity {
        provider: "bigmodel".into(),
        model: "glm-5.3".into(),
        api: Some(leveler_model::ProtocolKind::OpenAiChat),
        deployment: Some(leveler_model::deployment_identity(BIGMODEL)),
    });
    let history = vec![
        Message::user_input("What time is it in UTC? Answer in one short sentence."),
        handled,
        Message::text(Role::User, "And in one word: noon or midnight?"),
    ];
    let context = live_context(&glm, contract);
    let mut request = ModelRequest::new(ModelRef::new("bigmodel", "glm-5.3"), history.clone());
    request.reasoning_effort = glm.reasoning.default_effort;
    request.max_output_tokens = Some(2048);
    request.projection = Some(RequestProjection::project_for_route(
        &history,
        &request.tools,
        contract,
        ReasoningRetention::All,
        Some(
            &registry
                .route_identity(&ModelRef::new("bigmodel", "glm-5.3"))
                .unwrap(),
        ),
    ));
    let encoded_request = OpenAiChatAdapter::new()
        .encode_request(&request, &context, false)
        .expect("the exit request encodes");
    assert_eq!(
        encoded_request.body["thinking"]["clear_thinking"], true,
        "the exit request really clears prior thinking: {}",
        encoded_request.body
    );
    assert!(
        encoded_request.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m.get("reasoning_content").is_none()),
        "an opted-out route sends no historical reasoning: {}",
        encoded_request.body
    );
    let (status, body) = send_json(&context, &encoded_request).await;
    eprintln!(
        "[glm-5.3 exit] status={status} thinking={:?} text={:?}",
        encoded_request.body["thinking"], body["choices"][0]["message"]["content"]
    );
    assert_eq!(status, 200, "the cleared-mode request is accepted");

    // --- Kimi K2.7+/K2.8: no `thinking` field, and the declaration of a
    // preserved mode is refused because the mode has no request field ------
    for (provider_id, id) in [
        ("kimi-coding", "kimi-for-coding"),
        ("kimi-coding", "kimi-for-coding-highspeed"),
    ] {
        let mut kimi = registry
            .profile(&ModelRef::new(provider_id, id))
            .await
            .expect("the Kimi route");
        kimi.reasoning.preserved_thinking = Some(true);
        let conflict = leveler_model::ReasoningReplayContract::check_route(&kimi)
            .expect_err("Kimi K2.7+ preservation is unconditional, not a request field");
        assert!(
            conflict.to_string().contains("preserved_thinking"),
            "{conflict}"
        );
        // The resolved contract still replays legally, so an unchecked caller
        // cannot build a request that withholds the complete assistant message.
        let contract = leveler_model::ReasoningReplayContract::for_route(&kimi);
        assert_eq!(contract.arm_name(), "always+empty", "{id}");
        let context = live_context(&kimi, contract);
        let ask = vec![Message::user_input(
            "Call the clock tool for UTC, then answer in one sentence.",
        )];
        let route = registry
            .route_identity(&ModelRef::new(provider_id, id))
            .unwrap();
        let mut request = ModelRequest::new(ModelRef::new(provider_id, id), ask.clone());
        request.tools = tools();
        request.reasoning_effort = kimi.reasoning.default_effort;
        request.max_output_tokens = Some(2048);
        request.projection = Some(RequestProjection::project_for_route(
            &ask,
            &request.tools,
            contract,
            ReasoningRetention::All,
            Some(&route),
        ));
        let encoded_request = OpenAiChatAdapter::new()
            .encode_request(&request, &context, false)
            .expect("encodes");
        assert!(
            encoded_request.body.get("thinking").is_none(),
            "{id}: this family documents no `thinking` parameter: {}",
            encoded_request.body
        );
        assert!(
            encoded_request.body.get("reasoning_effort").is_some()
                == (kimi.reasoning.style == ReasoningStyle::OpenAiEffort),
            "{id}: effort is sent exactly where the endpoint supports it: {}",
            encoded_request.body
        );
        let (status, body) = send_json(&context, &encoded_request).await;
        if !entitled(label_kimi(id), status, &body) {
            continue;
        }
        let reasoning = body["choices"][0]["message"]["reasoning_content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        eprintln!(
            "[kimi {id}] status={status} reasoning_len={} reasoning_sha={} tool_calls={}",
            reasoning.chars().count(),
            sha8(&reasoning),
            body["choices"][0]["message"]["tool_calls"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0)
        );
        assert_eq!(status, 200);
    }

    // --- Kimi K3 (official): no `thinking` field at all, still replays ----
    let k3 = registry
        .profile(&ModelRef::new("kimi-coding", "k3"))
        .await
        .expect("k3");
    let contract = leveler_model::ReasoningReplayContract::for_route(&k3);
    assert_eq!(contract.arm_name(), "always+empty");
    let route = registry
        .route_identity(&ModelRef::new("kimi-coding", "k3"))
        .unwrap();
    let context = live_context(&k3, contract);
    let ask = vec![Message::user_input(
        "Think about which tool answers this, then call it: what time is it in UTC?",
    )];
    let mut request = ModelRequest::new(ModelRef::new("kimi-coding", "k3"), ask.clone());
    request.tools = tools();
    request.reasoning_effort = k3.reasoning.default_effort;
    request.max_output_tokens = Some(2048);
    request.projection = Some(RequestProjection::project_for_route(
        &ask,
        &request.tools,
        contract,
        ReasoningRetention::All,
        Some(&route),
    ));
    let encoded_request = OpenAiChatAdapter::new()
        .encode_request(&request, &context, false)
        .expect("encodes");
    assert!(
        encoded_request.body.get("thinking").is_none(),
        "K3 takes no thinking field: {}",
        encoded_request.body
    );
    let (status, body) = send_json(&context, &encoded_request).await;
    if !entitled(label_kimi("kimi-k3"), status, &body) {
        return;
    }
    let reasoning = body["choices"][0]["message"]["reasoning_content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let call = body["choices"][0]["message"]["tool_calls"][0]["id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    eprintln!(
        "[kimi-k3 official turn1] status={status} reasoning_len={} reasoning_sha={} call={:?}",
        reasoning.chars().count(),
        sha8(&reasoning),
        !call.is_empty()
    );
    assert_eq!(status, 200);
    assert!(!call.is_empty(), "K3 called the tool: {body}");

    // Turn 2 with the official K3: the complete assistant message comes back.
    let call_id = leveler_core::ToolCallId::new(call);
    let assistant = Message::from_parts(
        Role::Assistant,
        {
            let mut content = Vec::new();
            if !reasoning.is_empty() {
                content.push(ContentPart::Reasoning {
                    text: reasoning.clone(),
                });
            }
            content.push(ContentPart::ToolCall {
                call: ToolCall {
                    id: call_id.clone(),
                    name: "clock".into(),
                    arguments: serde_json::json!({"timezone": "UTC"}),
                },
            });
            content
        },
        None,
    )
    .with_route(route.clone());
    let mut history = ask.clone();
    history.push(assistant);
    history.push(Message::from_parts(
        Role::Tool,
        vec![ContentPart::ToolResult {
            result: ToolResultContent {
                call_id,
                content: "12:00 UTC".into(),
                is_error: false,
            },
        }],
        None,
    ));
    let mut request = ModelRequest::new(ModelRef::new("kimi-coding", "k3"), history.clone());
    request.tools = tools();
    request.reasoning_effort = k3.reasoning.default_effort;
    request.max_output_tokens = Some(2048);
    request.projection = Some(RequestProjection::project_for_route(
        &history,
        &request.tools,
        contract,
        ReasoningRetention::All,
        Some(&route),
    ));
    let encoded_request = OpenAiChatAdapter::new()
        .encode_request(&request, &context, false)
        .expect("encodes");
    if let Some(replayed) = assistant_on_wire(&encoded_request.body)
        .and_then(|message| message.get("reasoning_content"))
        .and_then(|value| value.as_str())
    {
        assert_eq!(
            replayed, reasoning,
            "the official K3 route replays its own reasoning verbatim"
        );
    }
    let (status, body) = send_json(&context, &encoded_request).await;
    eprintln!(
        "[kimi-k3 official turn2] status={status} text_len={} reasoning_len={}",
        body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .chars()
            .count(),
        body["choices"][0]["message"]["reasoning_content"]
            .as_str()
            .unwrap_or("")
            .chars()
            .count()
    );
    assert_eq!(
        status, 200,
        "K3 accepts the replayed full assistant message"
    );

    // --- Kimi K2.7-code: always thinking, no field, tool loop ------------
    let k27 = registry
        .profile(&ModelRef::new("kimi-coding", "kimi-for-coding-highspeed"))
        .await
        .expect("k2.7-code");
    let contract = leveler_model::ReasoningReplayContract::for_route(&k27);
    assert_eq!(contract.arm_name(), "always+empty");
    let route = registry
        .route_identity(&ModelRef::new("kimi-coding", "kimi-for-coding-highspeed"))
        .unwrap();
    let context = live_context(&k27, contract);
    let ask = vec![Message::user_input(
        "Call the clock tool for UTC, then answer in one sentence.",
    )];
    let mut request = ModelRequest::new(
        ModelRef::new("kimi-coding", "kimi-for-coding-highspeed"),
        ask.clone(),
    );
    request.tools = tools();
    request.max_output_tokens = Some(2048);
    request.projection = Some(RequestProjection::project_for_route(
        &ask,
        &request.tools,
        contract,
        ReasoningRetention::All,
        Some(&route),
    ));
    let encoded_request = OpenAiChatAdapter::new()
        .encode_request(&request, &context, false)
        .expect("encodes");
    assert!(
        encoded_request.body.get("thinking").is_none(),
        "K2.7-code takes no thinking field: {}",
        encoded_request.body
    );
    let (status, body) = send_json(&context, &encoded_request).await;
    if !entitled(label_kimi("kimi-for-coding-highspeed"), status, &body) {
        return;
    }
    eprintln!(
        "[kimi-k2.7-code turn1] status={status} reasoning_len={} tool_calls={}",
        body["choices"][0]["message"]["reasoning_content"]
            .as_str()
            .unwrap_or("")
            .chars()
            .count(),
        body["choices"][0]["message"]["tool_calls"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    );
    assert_eq!(status, 200);
}

/// Phase C on real endpoints: history whose provenance is unproven, and history
/// from a deployment the alias no longer addresses, must not be replayed — and
/// the resulting request must still be one the provider accepts.
#[tokio::test]
#[ignore = "spends real model tokens; see the module docs for the command"]
async fn real_routes_accept_unproven_and_foreign_history_without_its_reasoning() {
    let registry = registry();
    let profile = registry
        .profile(&ModelRef::new("taotoken", "deepseek-flash"))
        .await
        .expect("deepseek-flash");
    let contract = leveler_model::ReasoningReplayContract::for_route(&profile);
    assert_eq!(contract.arm_name(), "when_tools+empty");
    let route = registry
        .route_identity(&ModelRef::new("taotoken", "deepseek-flash"))
        .unwrap();
    let context = live_context(&profile, contract);

    // (a) Unproven: a legacy row that captured reasoning but recorded no route.
    let legacy = Message::from_parts(
        Role::Assistant,
        vec![
            ContentPart::Reasoning {
                text: "legacy private thinking".into(),
            },
            ContentPart::Text {
                text: "12:00 UTC".into(),
            },
        ],
        None,
    );
    assert_eq!(legacy.route, None);
    // (b) Foreign: the same alias now addresses a different deployment.
    let mut moved = route.clone();
    moved.deployment = Some("https://old.example.com/v1".into());
    let foreign = Message::from_parts(
        Role::Assistant,
        vec![
            ContentPart::Reasoning {
                text: "old endpoint private thinking".into(),
            },
            ContentPart::Text {
                text: "also noon".into(),
            },
        ],
        None,
    )
    .with_route(moved);

    for (label, handled) in [("unproven", legacy), ("foreign", foreign)] {
        let history = vec![
            Message::user_input("What time is it in UTC? One short sentence."),
            handled,
            Message::text(Role::User, "Thanks — one word: noon or midnight?"),
        ];
        let mut request =
            ModelRequest::new(ModelRef::new("taotoken", "deepseek-flash"), history.clone());
        request.reasoning_effort = profile.reasoning.default_effort;
        // Thinking shares this budget; a small cap truncates the answer after
        // the reasoning and would report a failure of the test, not the wire.
        request.max_output_tokens = Some(2048);
        // A tool-bearing request is the shape DeepSeek validates: the key's
        // presence is required there, whatever the turn captured.
        request.tools = tools();
        let projection = RequestProjection::project_for_route(
            &history,
            &request.tools,
            contract,
            ReasoningRetention::All,
            Some(&route),
        );
        let summary = projection.summary().clone();
        request.projection = Some(projection);
        let encoded_request = OpenAiChatAdapter::new()
            .encode_request(&request, &context, false)
            .expect("encodes");
        let wire = assistant_on_wire(&encoded_request.body).expect("the assistant turn");
        eprintln!(
            "[deepseek {label}] unproven_turns={} unproven_reasoning_turns={} foreign_turns={} \
             wire_reasoning={:?}",
            summary.unknown_provenance_turns,
            summary.unproven_reasoning_turns,
            summary.foreign_turns,
            wire.get("reasoning_content")
        );
        assert_eq!(
            wire.get("reasoning_content")
                .and_then(|value| value.as_str()),
            Some(""),
            "{label}: the required key stays legal and empty, never the text"
        );
        assert!(
            wire.get("reasoning_content")
                .and_then(|value| value.as_str())
                .is_none_or(|text| !text.contains("private thinking")),
            "{label}: no private reasoning reaches the provider"
        );
        // The rest of the turn is intact, and the provider accepts the request.
        assert!(wire["tool_calls"].is_null() || wire["tool_calls"].is_array());
        let expected_answer = if label == "unproven" {
            "12:00 UTC"
        } else {
            "also noon"
        };
        assert_eq!(
            wire["content"], expected_answer,
            "{label}: the assistant's visible answer survives"
        );
        let (status, body) = send_json(&context, &encoded_request).await;
        let text = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        eprintln!(
            "[deepseek {label}] status={status} answer_len={}",
            text.chars().count()
        );
        assert_eq!(
            status, 200,
            "{label}: the provider accepts the degraded replay"
        );
        // A thinking model may answer here or decide to call the tool it was
        // given; both are the task continuing, which is what the degraded
        // replay must not prevent.
        let called = body["choices"][0]["message"]["tool_calls"]
            .as_array()
            .is_some_and(|calls| !calls.is_empty());
        assert!(
            !text.trim().is_empty() || called,
            "{label}: the task continues (finish={}); no reasoning text is logged",
            body["choices"][0]["finish_reason"]
        );
        eprintln!(
            "[deepseek {label}] finish={} cached={} prompt_tokens={}",
            body["choices"][0]["finish_reason"],
            body["usage"]["prompt_tokens_details"]["cached_tokens"],
            body["usage"]["prompt_tokens"]
        );
    }
}

/// Encode one live request through the production adapter, with the same
/// contract and projection the runtime would hand over.
fn encode_live(
    profile: &ModelProfile,
    history: &[Message],
    route: &RouteIdentity,
    tools: Vec<ToolDefinition>,
    thinking_disabled: bool,
) -> serde_json::Value {
    let contract = leveler_model::ReasoningReplayContract::for_route(profile);
    let context = live_context(profile, contract);
    let mut request = ModelRequest::new(
        ModelRef::new(profile.provider.clone(), profile.id.clone()),
        history.to_vec(),
    );
    request.tools = tools.clone();
    request.reasoning_effort = profile.reasoning.default_effort;
    request.max_output_tokens = Some(2048);
    request.thinking_disabled = thinking_disabled;
    request.projection = Some(RequestProjection::project_for_route(
        history,
        &tools,
        contract,
        ReasoningRetention::All,
        Some(route),
    ));
    OpenAiChatAdapter::new()
        .encode_request(&request, &context, false)
        .expect("the request encodes")
        .body
}

/// Experiment 1/2/3: a preserved mode replays a TOOL-FREE turn.
///
/// Each section first asks a question with no tools at all, then sends the
/// assistant turn the endpoint returned plus one follow-up — still with no
/// tools — and checks the encoded body: the historical `reasoning_content`
/// comes back byte for byte, GLM still says `clear_thinking: false`, and no
/// Kimi route invents a `thinking` field. The follow-up is a real call, so the
/// endpoint has the chance to reject the shape.
#[tokio::test]
#[ignore = "spends real model tokens; see the module docs for the command"]
async fn real_preserved_modes_replay_a_tool_free_turn() {
    let registry = registry();
    for (provider_id, id) in [
        ("bigmodel", "glm-5.3"),
        // The same K3 model through a gateway: real evidence for the wire
        // shape, not a substitute for the official endpoint's acceptance.
        ("taotoken", "kimi-k3"),
        ("kimi-coding", "kimi-for-coding"),
        ("kimi-coding", "k3"),
    ] {
        let profile = registry
            .profile(&ModelRef::new(provider_id, id))
            .await
            .expect("the acceptance route");
        let contract = leveler_model::ReasoningReplayContract::for_route(&profile);
        assert_eq!(
            contract.arm_name(),
            if profile.reasoning.preserved_thinking == Some(true) && id.starts_with("glm") {
                "always"
            } else {
                "always+empty"
            },
            "{id}: the preserved mode is not a tool-loop detail"
        );
        let route = registry
            .route_identity(&ModelRef::new(provider_id, id))
            .expect("a real route");
        let context = live_context(&profile, contract);
        // The family fact, not the endpoint: every Kimi K2.7+/K3 route takes no
        // `thinking` field, whichever gateway serves it.
        let kimi = id.starts_with("kimi") || id.starts_with("k3");

        // Turn 1: no tools.
        let ask = Message::user_input(
            "In two sentences, explain why the sky is blue. Think it through, then answer.",
        );
        let first = encode_live(
            &profile,
            std::slice::from_ref(&ask),
            &route,
            Vec::new(),
            false,
        );
        assert!(
            first.get("tools").is_none(),
            "{id}: the request exposes no tools: {first}"
        );
        if kimi {
            assert!(
                first.get("thinking").is_none(),
                "{id}: Kimi K2.7+ takes no `thinking` field: {first}"
            );
        } else {
            assert_eq!(
                first["thinking"]["clear_thinking"], false,
                "{id}: preserved thinking is requested in the family's own word: {first}"
            );
        }
        let (status, body) = send_body(&context, &first).await;
        if kimi && !entitled(label_kimi(id), status, &body) {
            continue;
        }
        assert_eq!(status, 200, "{id}: the first tool-free request is accepted");
        let reasoning = body["choices"][0]["message"]["reasoning_content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let answer = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(
            !reasoning.is_empty(),
            "{id}: this model always thinks: {body}"
        );
        assert!(!answer.trim().is_empty(), "{id}: it answered: {body}");
        let answer_len = answer.chars().count();

        // Turn 2: the complete assistant message plus a follow-up, still no
        // tools, so only the preserved-mode contract can put the chain back.
        let assistant = Message::from_parts(
            Role::Assistant,
            vec![
                ContentPart::Reasoning {
                    text: reasoning.clone(),
                },
                ContentPart::Text { text: answer },
            ],
            None,
        )
        .with_route(route.clone());
        let history = vec![
            ask,
            assistant,
            Message::text(Role::User, "Now say it in one short sentence."),
        ];
        let second = encode_live(&profile, &history, &route, Vec::new(), false);
        let wire_assistant = assistant_on_wire(&second).expect("the assistant turn");
        assert_eq!(
            wire_assistant["reasoning_content"].as_str(),
            Some(reasoning.as_str()),
            "{id}: the preserved chain is replayed verbatim"
        );
        assert!(second.get("tools").is_none(), "{id}: {second}");
        if kimi {
            assert!(second.get("thinking").is_none(), "{id}: {second}");
        } else {
            assert_eq!(
                second["thinking"]["clear_thinking"], false,
                "{id}: {second}"
            );
        }
        let (status, body) = send_body(&context, &second).await;
        if kimi && !entitled(label_kimi(id), status, &body) {
            continue;
        }
        let continued = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        eprintln!(
            "[{id} tool-free replay] status={status} reasoning_len={} reasoning_sha={} \
             answer_len={} continued_len={}",
            reasoning.chars().count(),
            sha8(&reasoning),
            answer_len,
            continued.chars().count()
        );
        assert_eq!(
            status, 200,
            "{id}: the endpoint accepts the replayed tool-free history"
        );
        assert!(
            !continued.trim().is_empty(),
            "{id}: the conversation continues: {body}"
        );
    }
}

/// Experiment 3, scoped to the Kimi route: an ordinary multi-turn
/// conversation with no tools at all must still return the complete chain.
///
/// It is separate from the combined preserved-mode test because the official
/// GLM credential in this lab is an environment input: when it is expired that
/// test aborts before it reaches the Kimi routes, and a gateway PASS there
/// would otherwise be reported without a Kimi measurement.
#[tokio::test]
#[ignore = "spends real model tokens; see the module docs for the command"]
async fn real_kimi_replays_the_whole_chain_without_tools() {
    let registry = registry();
    for (provider_id, id) in [
        ("taotoken", "kimi-k3"),
        ("kimi-coding", "k3"),
        ("kimi-coding", "k3.1-pro"),
    ] {
        let profile = registry
            .profile(&ModelRef::new(provider_id, id))
            .await
            .expect("the Kimi route");
        let contract = leveler_model::ReasoningReplayContract::for_route(&profile);
        assert_eq!(contract.arm_name(), "always+empty", "{id}");
        let route = registry
            .route_identity(&ModelRef::new(provider_id, id))
            .expect("a real route");
        let context = live_context(&profile, contract);
        let label = format!("{provider_id}/{id}");

        let ask = Message::user_input(
            "Think it through: a train leaves at 09:40 and takes 2h35m. What time does it arrive? \
             Answer with the time and one sentence of reasoning.",
        );
        let first = encode_live(
            &profile,
            std::slice::from_ref(&ask),
            &route,
            Vec::new(),
            false,
        );
        assert!(first.get("tools").is_none(), "{id}: {first}");
        assert!(first.get("thinking").is_none(), "{id}: {first}");
        let (status, body) = send_body(&context, &first).await;
        if !entitled(label.clone(), status, &body) {
            continue;
        }
        assert_eq!(status, 200, "{label}: turn 1");
        let reasoning = body["choices"][0]["message"]["reasoning_content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let answer = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(
            !reasoning.is_empty(),
            "{label}: this model always thinks: {body}"
        );

        let assistant = Message::from_parts(
            Role::Assistant,
            vec![
                ContentPart::Reasoning {
                    text: reasoning.clone(),
                },
                ContentPart::Text {
                    text: answer.clone(),
                },
            ],
            None,
        )
        .with_route(route.clone());
        let history = vec![
            ask,
            assistant,
            Message::text(
                Role::User,
                "Now name the same arrival time in 24-hour form only.",
            ),
        ];
        let second = encode_live(&profile, &history, &route, Vec::new(), false);
        let wire_assistant = assistant_on_wire(&second).expect("the assistant turn");
        assert_eq!(
            wire_assistant["reasoning_content"].as_str(),
            Some(reasoning.as_str()),
            "{label}: the chain is replayed verbatim without tools"
        );
        let (status, body) = send_body(&context, &second).await;
        if !entitled(label.clone(), status, &body) {
            continue;
        }
        assert_eq!(status, 200, "{label}: the tool-free follow-up is accepted");
        let continued = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("");
        eprintln!(
            "[{label} tool-free replay] status={status} reasoning_len={} reasoning_sha={} \
             answer_len={} continued_len={}",
            reasoning.chars().count(),
            sha8(&reasoning),
            answer.chars().count(),
            continued.chars().count()
        );
        assert!(
            !continued.trim().is_empty(),
            "{label}: the conversation continues: {body}"
        );
    }
}

/// Experiment 4: Thinking On → Off → On with the production encoder.
///
/// DeepSeek documents replay in terms of the request shape, not the current
/// mode. The off request is therefore legal in both shapes: a tool-bearing one
/// keeps the historical `reasoning_content`, a tool-free one omits it, and the
/// explicit `thinking: {"type": "disabled"}` never travels with an effort.
/// Turning thinking back on replays the same bytes.
#[tokio::test]
#[ignore = "spends real model tokens; see the module docs for the command"]
async fn real_thinking_off_and_on_stay_legal_and_keep_the_replay_rule() {
    let registry = registry();
    let profile = registry
        .profile(&ModelRef::new("taotoken", "deepseek-flash"))
        .await
        .expect("deepseek-flash");
    let contract = leveler_model::ReasoningReplayContract::for_route(&profile);
    assert_eq!(contract.arm_name(), "when_tools+empty");
    let route = registry
        .route_identity(&ModelRef::new("taotoken", "deepseek-flash"))
        .expect("a real route");
    let context = live_context(&profile, contract);

    // Turn 1, thinking on, no tools: a reasoning-heavy question that leaves a
    // captured chain to replay in the later shapes.
    let ask = Message::user_input(
        "Think step by step: what is 17 * 23, and which UTC offset does Beijing use in October? \
         Answer in two short sentences.",
    );
    let first = encode_live(
        &profile,
        std::slice::from_ref(&ask),
        &route,
        Vec::new(),
        false,
    );
    assert_eq!(first["thinking"]["type"], "enabled");
    let (status, body) = send_body(&context, &first).await;
    assert_eq!(status, 200, "the first turn is accepted");
    let reasoning = body["choices"][0]["message"]["reasoning_content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let answer = body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(!reasoning.is_empty(), "the model thought: {body}");
    assert!(!answer.trim().is_empty(), "the model answered: {body}");
    let answer_text = answer.clone();

    // The next turn exposes tools and asks for a call, so the request is the
    // shape DeepSeek validates and the history is one it must replay.
    let assistant = Message::from_parts(
        Role::Assistant,
        vec![
            ContentPart::Reasoning {
                text: reasoning.clone(),
            },
            ContentPart::Text { text: answer },
        ],
        None,
    )
    .with_route(route.clone());
    let history = vec![
        ask,
        assistant,
        Message::text(
            Role::User,
            "Now call the clock tool for UTC and state the time in one sentence.",
        ),
    ];

    // On: the documented tool-bearing replay.
    let on = encode_live(&profile, &history, &route, tools(), false);
    assert_eq!(on["thinking"]["type"], "enabled");
    assert_eq!(on["messages"][1]["reasoning_content"], reasoning.as_str());
    let (status, _) = send_body(&context, &on).await;
    eprintln!(
        "[deepseek on+tools] status={status} reasoning_sha={}",
        sha8(&reasoning)
    );
    assert_eq!(status, 200);

    // Off, tools present: legal, and the history it is given is not rewritten.
    let off = encode_live(&profile, &history, &route, tools(), true);
    assert_eq!(off["thinking"]["type"], "disabled");
    assert!(off.get("reasoning_effort").is_none(), "{off}");
    assert_eq!(off["messages"][1]["reasoning_content"], reasoning.as_str());
    let (status, _) = send_body(&context, &off).await;
    eprintln!("[deepseek off+tools] status={status}");
    assert_eq!(status, 200, "a tool-bearing off request is accepted");

    // Off, no tools: the requirement does not cover this shape, so the channel
    // is absent and nothing else is lost.
    let off_no_tools = encode_live(&profile, &history, &route, Vec::new(), true);
    assert_eq!(off_no_tools["thinking"]["type"], "disabled");
    assert!(
        off_no_tools["messages"][1]
            .get("reasoning_content")
            .is_none(),
        "{off_no_tools}"
    );
    assert_eq!(
        off_no_tools["messages"][1]["content"], answer_text,
        "the visible answer is a task fact, not reasoning: it stays"
    );
    let (status, _) = send_body(&context, &off_no_tools).await;
    eprintln!("[deepseek off no tools] status={status}");
    assert_eq!(status, 200, "a tool-free off request is accepted");

    // On again: the same chain comes back, so the provider sees its own
    // reasoning rather than a record that quietly lost it.
    let back_on = encode_live(&profile, &history, &route, tools(), false);
    assert_eq!(back_on["thinking"]["type"], "enabled");
    assert_eq!(
        back_on["messages"][1]["reasoning_content"],
        reasoning.as_str()
    );
    let (status, body) = send_body(&context, &back_on).await;
    eprintln!(
        "[deepseek back on] status={status} answer_len={}",
        body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .chars()
            .count()
    );
    assert_eq!(status, 200);
}

fn label_kimi(model: &str) -> String {
    format!("kimi-coding/{model}")
}

/// Whether the official Kimi Code endpoint served the request at all.
///
/// A subscription that does not cover Kimi Code answers 403
/// `permission_denied_error`. That is an environment block, not a protocol
/// result: it is reported as NOT VERIFIED and never counted as a pass — a
/// gateway PASS must not stand in for an official-endpoint PASS.
fn entitled(label: String, status: u16, body: &serde_json::Value) -> bool {
    if status == 200 {
        return true;
    }
    // Two environment gaps, neither of which is a protocol result:
    //  * 403 `permission_denied_error`: the subscription does not cover this
    //    model;
    //  * 401 `invalid_authentication_error`: the credential is not valid for
    //    that endpoint (a key for a different host, or one that expired).
    let denied = (status == 403 && body["error"]["type"] == "permission_denied_error")
        || (status == 401 && body["error"]["type"] == "invalid_authentication_error");
    assert!(denied, "{label}: unexpected status {status}: {body}");
    eprintln!(
        "[{label}] NOT VERIFIED: the official endpoint answered HTTP {status} \
         ({}); nothing about its wire behaviour is claimed",
        body["error"]["type"].as_str().unwrap_or("unknown")
    );
    false
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
