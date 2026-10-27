//! Reasoning provenance and integrity across the request lifecycle.
//!
//! A provider's `reasoning_content` channel carries no signature. A replay is
//! therefore only meaningful as *the same route's* continued thought: handing
//! one model's private reasoning to another would present foreign content as
//! native context, and the model would spend tokens answering a voice it never
//! produced. These tests pin the boundary, and the three-valued honesty that
//! surrounds it — "same route", "another route", and "nobody recorded it" are
//! different facts.
//!
//! What must never happen:
//!
//! * another model's reasoning is replayed as this model's own;
//! * historical reasoning is folded into the assistant text to "keep the
//!   context" (that would make the model read another model's thinking as its
//!   own visible reply);
//! * an empty turn or an unrecorded one is reported as complete reasoning;
//! * a model switch destroys the user's task context — text, tool calls and
//!   tool results always survive.

use leveler_core::ToolCallId;
use leveler_model::{
    CompatibilityConfig, ContentPart, MissingReasoningReplay, ModelCapabilities, ModelLimits,
    ModelProfile, ProjectedReasoning, ProtocolKind, ReasoningConfig, ReasoningReplayContract,
    ReasoningReplayScope, ReasoningRetention, RequestProjection, Role, RouteIdentity, ToolCall,
    ToolResultContent, deployment_identity,
};

fn route(provider: &str, model: &str) -> RouteIdentity {
    RouteIdentity {
        provider: provider.into(),
        model: model.into(),
        api: Some(ProtocolKind::OpenAiChat),
        deployment: None,
    }
}

fn deepseek_contract() -> ReasoningReplayContract {
    ReasoningReplayContract::raw_field(
        ReasoningReplayScope::WhenToolsPresent,
        MissingReasoningReplay::EmptyString,
    )
}

fn always_empty_contract() -> ReasoningReplayContract {
    ReasoningReplayContract::raw_field(
        ReasoningReplayScope::Always,
        MissingReasoningReplay::EmptyString,
    )
}

fn glm_contract() -> ReasoningReplayContract {
    ReasoningReplayContract::raw_field(
        ReasoningReplayScope::WhenToolsPresent,
        MissingReasoningReplay::Omit,
    )
}

fn user(text: &str) -> leveler_model::Message {
    leveler_model::Message::user_input(text)
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(id),
        name: "clock".into(),
        arguments: serde_json::json!({ "tz": "UTC" }),
    }
}

/// An assistant turn as the runtime assembles it: reasoning and/or text plus the
/// tool calls it made, stamped with the route that produced it.
fn assistant(
    author: Option<RouteIdentity>,
    reasoning: Option<&str>,
    text: &str,
) -> leveler_model::Message {
    let mut content = Vec::new();
    if let Some(reasoning) = reasoning {
        content.push(ContentPart::Reasoning {
            text: reasoning.into(),
        });
    }
    if !text.is_empty() {
        content.push(ContentPart::Text { text: text.into() });
    }
    content.push(ContentPart::ToolCall { call: call("c1") });
    let message = leveler_model::Message::from_parts(Role::Assistant, content, None);
    match author {
        Some(author) => message.with_route(author),
        None => message,
    }
}

fn tool_result() -> leveler_model::Message {
    leveler_model::Message::from_parts(
        Role::Tool,
        vec![ContentPart::ToolResult {
            result: ToolResultContent {
                call_id: ToolCallId::new("c1"),
                content: "12:00 UTC".into(),
                is_error: false,
            },
        }],
        None,
    )
}

fn tools() -> Vec<leveler_model::ToolDefinition> {
    vec![leveler_model::ToolDefinition {
        name: "clock".into(),
        description: "returns the time".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }]
}

fn carried(projection: &RequestProjection) -> Vec<String> {
    projection
        .messages()
        .iter()
        .filter_map(|message| match &message.reasoning {
            ProjectedReasoning::Captured(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn project(
    messages: &[leveler_model::Message],
    contract: ReasoningReplayContract,
    current: &RouteIdentity,
) -> RequestProjection {
    RequestProjection::project_for_route(
        messages,
        &tools(),
        contract,
        ReasoningRetention::All,
        Some(current),
    )
}

#[test]
fn the_same_route_replays_its_own_reasoning_verbatim() {
    let current = route("deepseek", "deepseek-flash");
    let messages = vec![
        user("what time is it? use the tool"),
        assistant(Some(current.clone()), Some("I should call clock."), ""),
        tool_result(),
    ];
    let projection = project(&messages, deepseek_contract(), &current);
    assert_eq!(carried(&projection), vec!["I should call clock."]);
    assert_eq!(projection.summary().foreign_turns, 0);
    assert_eq!(projection.summary().unknown_provenance_turns, 0);
    assert_eq!(projection.summary().empty_turns, 0);
}

#[test]
fn another_models_reasoning_is_never_replayed_as_this_models_own() {
    // Same provider, different model: the strongest case for "this is not my
    // thought". The turn keeps its text, its tool call and the tool result.
    let current = route("deepseek", "deepseek-v4-pro");
    let messages = vec![
        user("what time is it? use the tool"),
        assistant(
            Some(route("deepseek", "deepseek-flash")),
            Some("flash's private reasoning"),
            "calling the clock",
        ),
        tool_result(),
    ];
    let projection = project(&messages, deepseek_contract(), &current);
    assert_eq!(carried(&projection), Vec::<String>::new());
    assert_eq!(projection.summary().foreign_turns, 1);
    // The handover keeps every task fact.
    assert_eq!(projection.messages()[1].role, Role::Assistant);
    assert_eq!(
        projection.messages()[1]
            .content
            .iter()
            .filter(|part| matches!(part, ContentPart::Text { .. }))
            .count(),
        1,
        "the assistant text survives a foreign-reasoning drop"
    );
    assert_eq!(
        projection.messages()[1]
            .content
            .iter()
            .filter(|part| matches!(part, ContentPart::ToolCall { .. }))
            .count(),
        1,
        "the tool call survives a foreign-reasoning drop"
    );
    assert!(matches!(
        projection.messages()[2].content[0],
        ContentPart::ToolResult { .. }
    ));
}

#[test]
fn the_same_model_through_another_gateway_is_another_route() {
    // `deepseek-flash` on two gateways is two routes: the wire id agrees, the
    // deployment does not.
    let current = route("taotoken", "deepseek-flash");
    let messages = vec![
        user("go"),
        assistant(
            Some(route("ark", "deepseek-flash")),
            Some("ark's thought"),
            "",
        ),
        tool_result(),
    ];
    let projection = project(&messages, deepseek_contract(), &current);
    assert_eq!(carried(&projection), Vec::<String>::new());
    assert_eq!(projection.summary().foreign_turns, 1);

    // A recorded deployment breaks the tie even when the provider alias is the
    // same: repointing an endpoint is a new deployment.
    let mut here = route("deepseek", "deepseek-flash");
    here.deployment = Some("https://taotoken.net/api/v1".into());
    let mut there = route("deepseek", "deepseek-flash");
    there.deployment = Some("https://api.deepseek.com".into());
    let messages = vec![
        user("go"),
        assistant(Some(there), Some("the other endpoint's thought"), ""),
        tool_result(),
    ];
    let projection = project(&messages, deepseek_contract(), &here);
    assert_eq!(carried(&projection), Vec::<String>::new());
    assert_eq!(projection.summary().foreign_turns, 1);
}

#[test]
fn switching_back_to_the_original_route_makes_its_own_history_replayable() {
    // A and B alternated. On A, A's turns are this model's own thought and B's
    // are not — the decision is per turn, not per session.
    let a = route("deepseek", "deepseek-flash");
    let b = route("bigmodel", "glm-5.3");
    let messages = vec![
        user("start"),
        assistant(Some(a.clone()), Some("A first"), ""),
        tool_result(),
        assistant(Some(b.clone()), Some("B in the middle"), ""),
        tool_result(),
        assistant(Some(a.clone()), Some("A again"), ""),
        tool_result(),
    ];
    let on_a = project(&messages, deepseek_contract(), &a);
    assert_eq!(carried(&on_a), vec!["A first", "A again"]);
    assert_eq!(on_a.summary().foreign_turns, 1);

    let on_b = project(&messages, glm_contract(), &b);
    assert_eq!(carried(&on_b), vec!["B in the middle"]);
    assert_eq!(on_b.summary().foreign_turns, 2);
}

#[test]
fn an_unrecorded_author_is_unknown_and_its_reasoning_is_not_replayed() {
    // Historical payload (or a runtime that cannot name its route). Unknown is
    // not SameRoute: the captured text is real, but nothing proves it belongs to
    // the route this request addresses, so it never enters the request as this
    // model's own thought. The turn itself — text, tool call, tool result —
    // survives; only the private channel is withheld, and the projection says
    // so instead of reporting a measured empty or a complete replay.
    let current = route("deepseek", "deepseek-flash");
    let messages = vec![
        user("go"),
        assistant(None, Some("legacy thought"), "observed"),
        tool_result(),
    ];
    let projection = project(&messages, deepseek_contract(), &current);
    assert_eq!(carried(&projection), Vec::<String>::new());
    assert_eq!(projection.summary().unknown_provenance_turns, 1);
    assert_eq!(projection.summary().unproven_reasoning_turns, 1);
    assert_eq!(
        projection.summary().empty_turns,
        0,
        "an unrecorded author is not evidence that the model produced nothing"
    );
    assert_eq!(projection.summary().foreign_turns, 0);
    assert_eq!(
        projection.summary().carried_turns,
        0,
        "an unproven channel is never counted as carried"
    );
    // DeepSeek validates the key's presence on a replayed assistant turn, so the
    // legal wire form is the empty value — a compatibility spelling, never a
    // claim of completeness.
    assert_eq!(
        projection.messages()[1].reasoning,
        ProjectedReasoning::RequiredEmpty
    );
    assert_eq!(projection.messages()[1].content.len(), 2);
    assert!(matches!(
        projection.messages()[1].content.last(),
        Some(ContentPart::ToolCall { .. })
    ));
    assert!(matches!(
        projection.messages()[2].content.last(),
        Some(ContentPart::ToolResult { .. })
    ));
}

#[test]
fn a_one_sided_deployment_is_unproven_in_both_directions() {
    // The historical turn names its endpoint, this request does not (or the
    // other way round). Provider, model and protocol agree, so the turn is not
    // foreign — but one endpoint is unrecorded, which is not proof of the same
    // deployment. Unproven reasoning is not replayed; the context is kept.
    let mut recorded = route("gateway", "deepseek-flash");
    recorded.deployment = Some(deployment_identity("https://gw.example.com/v1"));
    let unnamed = route("gateway", "deepseek-flash");

    for (author, current) in [
        (recorded.clone(), unnamed.clone()),
        (unnamed.clone(), recorded.clone()),
    ] {
        let messages = vec![
            user("go"),
            assistant(Some(author), Some("private thought"), ""),
            tool_result(),
        ];
        let projection = project(&messages, deepseek_contract(), &current);
        assert_eq!(carried(&projection), Vec::<String>::new());
        assert_eq!(projection.summary().unknown_provenance_turns, 1);
        assert_eq!(projection.summary().foreign_turns, 0);
        assert_eq!(projection.summary().unproven_reasoning_turns, 1);
    }
}

#[test]
fn retargeting_a_provider_alias_to_another_endpoint_is_another_route() {
    // One `[providers.*]` alias addresses exactly one deployment. Editing its
    // `base_url` moves the alias to a different endpoint, and the history the
    // old endpoint produced is foreign there even though provider, model and
    // protocol all still match by name.
    let mut author = route("gateway", "deepseek-flash");
    author.deployment = Some(deployment_identity("https://old.example.com/v1"));
    let mut current = route("gateway", "deepseek-flash");
    current.deployment = Some(deployment_identity("https://new.example.com/v1"));

    let messages = vec![
        user("go"),
        assistant(Some(author), Some("old endpoint thought"), ""),
        tool_result(),
    ];
    let projection = project(&messages, deepseek_contract(), &current);
    assert_eq!(carried(&projection), Vec::<String>::new());
    assert_eq!(projection.summary().foreign_turns, 1);
    assert_eq!(projection.summary().unknown_provenance_turns, 0);
}

#[test]
fn the_same_deployment_spelled_differently_still_replays() {
    // The registry records the normalized identity, so a trailing slash, host
    // case or an explicit default port cannot turn one endpoint into two and
    // silently drop the model's own chain.
    let mut author = route("gateway", "deepseek-flash");
    author.deployment = Some(deployment_identity("https://API.Gateway.example/v1/"));
    let mut current = route("gateway", "deepseek-flash");
    current.deployment = Some(deployment_identity("https://api.gateway.example:443/v1"));

    let messages = vec![
        user("go"),
        assistant(Some(author), Some("its own thought"), ""),
        tool_result(),
    ];
    let projection = project(&messages, deepseek_contract(), &current);
    assert_eq!(carried(&projection), vec!["its own thought"]);
    assert_eq!(projection.summary().unknown_provenance_turns, 0);
}

#[test]
fn an_empty_turn_on_this_route_is_a_measured_empty() {
    let current = route("deepseek", "deepseek-flash");
    let messages = vec![
        user("go"),
        assistant(Some(current.clone()), None, "no thinking, just the call"),
        tool_result(),
    ];
    let projection = project(&messages, deepseek_contract(), &current);
    assert_eq!(projection.summary().empty_turns, 1);
    assert_eq!(projection.summary().unknown_provenance_turns, 0);
    // DeepSeek requires the key's presence, so the legal wire form is the empty
    // string — a compatibility spelling, never a claim of completeness.
    assert_eq!(
        projection.messages()[1].reasoning,
        ProjectedReasoning::RequiredEmpty
    );
    assert_eq!(
        projection.messages()[1].reasoning.as_wire(),
        Some(""),
        "the empty channel is a wire form, not captured content"
    );
    assert_eq!(projection.summary().carried_turns, 0);
}

#[test]
fn a_foreign_turn_still_carries_the_required_key_without_its_content() {
    // The provider validates the key's presence on every replayed assistant
    // turn. Dropping the foreign content is a provenance decision; the protocol
    // shape is still honoured with the legal empty value.
    let messages = vec![
        user("go"),
        assistant(
            Some(route("bigmodel", "glm-5.3")),
            Some("foreign thought"),
            "",
        ),
        tool_result(),
    ];
    let projection = RequestProjection::project_for_route(
        &messages,
        &tools(),
        deepseek_contract(),
        ReasoningRetention::All,
        Some(&route("deepseek", "deepseek-flash")),
    );
    assert_eq!(carried(&projection), Vec::<String>::new());
    assert_eq!(
        projection.messages()[1].reasoning,
        ProjectedReasoning::RequiredEmpty
    );
}

#[test]
fn a_route_without_an_empty_key_omits_the_channel_entirely() {
    // GLM's protocol does not require the key on a turn that produced none, so
    // a foreign turn carries no field at all.
    let current = route("bigmodel", "glm-5.3");
    let messages = vec![
        user("go"),
        assistant(
            Some(route("deepseek", "deepseek-flash")),
            Some("foreign thought"),
            "",
        ),
        tool_result(),
    ];
    let projection = project(&messages, glm_contract(), &current);
    assert_eq!(
        projection.messages()[1].reasoning,
        ProjectedReasoning::Omitted
    );
}

#[test]
fn a_tool_free_request_replays_nothing_but_keeps_the_history() {
    let current = route("deepseek", "deepseek-flash");
    let messages = vec![
        user("explain"),
        assistant(Some(current.clone()), Some("some thought"), "the answer"),
    ];
    let projection = RequestProjection::project_for_route(
        &messages,
        &[],
        deepseek_contract(),
        ReasoningRetention::All,
        Some(&current),
    );
    assert_eq!(carried(&projection), Vec::<String>::new());
    assert_eq!(
        projection.messages()[1].reasoning,
        ProjectedReasoning::Omitted
    );
    assert_eq!(
        projection.messages()[1].text_content_placeholder(),
        "the answer"
    );
}

/// The concrete text of a projected assistant turn, read without inventing a
/// second content model.
trait TextOf {
    fn text_content_placeholder(&self) -> String;
}

impl TextOf for leveler_model::ProjectedMessage {
    fn text_content_placeholder(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[test]
fn an_unrecorded_payload_from_before_provenance_still_deserializes() {
    // A session row written before this change has no `route` key. It must load
    // as "unknown provenance" — not fail, and not be promoted to a completed
    // reasoning turn.
    let legacy = serde_json::json!({
        "role": "assistant",
        "content": [
            { "type": "reasoning", "text": "old thought" },
            { "type": "text", "text": "old answer" }
        ]
    });
    let message: leveler_model::Message = serde_json::from_value(legacy).unwrap();
    assert_eq!(message.route, None);
    assert_eq!(message.text_content(), "old answer");
}

#[test]
fn a_stamped_message_round_trips_through_the_transcript_payload() {
    let current = route("bigmodel", "glm-5.3");
    let message = assistant(Some(current.clone()), Some("thought"), "answer");
    let json = serde_json::to_string(&message).unwrap();
    assert!(json.contains("\"provider\":\"bigmodel\""), "{json}");
    let back: leveler_model::Message = serde_json::from_str(&json).unwrap();
    assert_eq!(back.route.as_ref(), Some(&current));
    assert_eq!(back, message);
}

#[test]
fn the_family_table_separates_required_from_opt_out() {
    // K3's protocol requires the complete assistant message back on every
    // request, so a weaker declaration is refused.
    let k3 = profile("k3");
    assert_eq!(
        leveler_model::ReasoningReplayContract::for_route(&k3),
        always_empty_contract(),
        "K3 requires every historical turn's reasoning and the empty key"
    );
    let mut k3_off = k3.clone();
    k3_off.compatibility.reasoning_replay_scope = Some(ReasoningReplayScope::Never);
    assert!(
        leveler_model::ReasoningReplayContract::check_route(&k3_off).is_err(),
        "K3's requirement is hard"
    );

    let glm = profile("glm-5.3");
    let mut glm_off = glm.clone();
    glm_off.compatibility.reasoning_replay_scope = Some(ReasoningReplayScope::Never);
    leveler_model::ReasoningReplayContract::check_route(&glm_off)
        .expect("a GLM deployment may disable preserved thinking");

    // K2.7 Code always thinks and always preserves, so its rule is the same
    // hard one: every historical assistant message comes back.
    let k27 = profile("kimi-k2.7-code");
    assert_eq!(
        leveler_model::ReasoningReplayContract::for_route(&k27),
        always_empty_contract()
    );
    let mut k27_off = k27.clone();
    k27_off.compatibility.reasoning_replay_scope = Some(ReasoningReplayScope::Never);
    assert!(
        leveler_model::ReasoningReplayContract::check_route(&k27_off).is_err(),
        "the unconditional preservation requirement is not an option"
    );

    // A Kimi id below K2.7 is no longer a supported family, so it does not
    // inherit the requirement by name similarity.
    let legacy = profile("kimi-k2-0905-preview");
    assert_eq!(
        leveler_model::ReasoningReplayContract::for_route(&legacy),
        ReasoningReplayContract::NONE
    );
}

fn profile(model_id: &str) -> ModelProfile {
    ModelProfile {
        id: format!("alias-{model_id}"),
        provider: "gateway".into(),
        model_id: model_id.into(),
        protocol: ProtocolKind::OpenAiChat,
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            parallel_tool_calls: true,
            structured_output: true,
            reasoning: true,
            vision: false,
        },
        limits: ModelLimits {
            context_window: 131_072,
            reliable_context: 65_536,
            max_output_tokens: 8_192,
            max_tool_schema_bytes: 32_768,
            max_parallel_tool_calls: 4,
            max_tool_output_bytes: None,
        },
        context_quality: None,
        thinking: None,
        reasoning: ReasoningConfig::default(),
        compatibility: CompatibilityConfig::default(),
        pricing: None,
    }
}
