//! Route-level reasoning-replay resolution.
//!
//! A route declares its protocol facts in configuration, but an OpenAiChat
//! model family can impose a hard protocol requirement the user never wrote
//! down. These tests pin the resolution precedence:
//!
//! 1. the provider/model protocol capability,
//! 2. the user's explicit declaration,
//! 3. the conservative compatibility default (`never`).
//!
//! The precedence must not depend on which entry point wrote the config:
//! `leveler login`, a hand-written config, the lab, or a dogfood config all
//! resolve through this one function.

use leveler_model::{
    CompatibilityConfig, MissingReasoningReplay, ModelCapabilities, ModelLimits, ModelProfile,
    ProtocolKind, ReasoningConfig, ReasoningReplayContract, ReasoningReplayScope,
};

fn profile(model_id: &str, protocol: ProtocolKind, reasoning: bool) -> ModelProfile {
    ModelProfile {
        id: format!("alias-for-{model_id}"),
        provider: "gateway".into(),
        model_id: model_id.into(),
        protocol,
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            parallel_tool_calls: true,
            structured_output: true,
            reasoning,
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

fn with_scope(mut p: ModelProfile, scope: ReasoningReplayScope) -> ModelProfile {
    p.compatibility.reasoning_replay_scope = Some(scope);
    p
}

#[test]
fn an_undeclared_deepseek_route_resolves_the_protocol_requirement() {
    // The dogfood/lab config declares only `reasoning = true` and a thinking
    // style; these two compatibility fields are absent. The wire protocol
    // still requires replay on every tool-bearing request.
    for wire_id in [
        "deepseek-flash",
        "deepseek-v4-flash",
        "deepseek-v4-pro",
        "DeepSeek-V3.2",
        "deepseek/deepseek-chat",
    ] {
        let contract =
            ReasoningReplayContract::for_route(&profile(wire_id, ProtocolKind::OpenAiChat, true));
        assert_eq!(
            contract,
            ReasoningReplayContract::raw_field(
                ReasoningReplayScope::WhenToolsPresent,
                MissingReasoningReplay::EmptyString,
            ),
            "{wire_id}"
        );
    }
}

#[test]
fn a_replay_accepting_family_also_declares_its_requirement() {
    // GLM documents that tool loops must carry the historical reasoning back
    // and never validated an empty key, so a turn that captured none omits the
    // field.
    for wire_id in ["glm-5.3", "glm-4.6", "glm_for_coding"] {
        let contract =
            ReasoningReplayContract::for_route(&profile(wire_id, ProtocolKind::OpenAiChat, true));
        assert_eq!(
            contract,
            ReasoningReplayContract::raw_field(
                ReasoningReplayScope::WhenToolsPresent,
                MissingReasoningReplay::Omit,
            ),
            "{wire_id}"
        );
    }
    // Kimi K2.7 and later always think and always preserve thinking, so every
    // historical assistant message's reasoning comes back — on a tool loop and
    // on an ordinary multi-turn conversation alike. The key's presence is the
    // conservative spelling: a turn that captured none carries the legal empty
    // value instead of omitting a key the K2.x line validates.
    for wire_id in [
        "kimi-k2.7-code",
        "kimi-k2.7-code-highspeed",
        "kimi-k2.8-preview",
        "kimi-for-coding",
        "kimi-for-coding-highspeed",
        "k3",
        "k3-256k",
        "k3.1-pro",
        "k3.1-pro-highspeed",
        "kimi-k3.1-pro",
    ] {
        let contract =
            ReasoningReplayContract::for_route(&profile(wire_id, ProtocolKind::OpenAiChat, true));
        assert_eq!(
            contract,
            ReasoningReplayContract::raw_field(
                ReasoningReplayScope::Always,
                MissingReasoningReplay::EmptyString,
            ),
            "{wire_id}"
        );
    }
}

#[test]
fn a_preserved_thinking_route_replays_a_tool_free_turn() {
    // Preserved thinking is the mode, not a tool-loop detail: the endpoint is
    // asked to keep every earlier turn's reasoning, so a request that carries
    // no tools still owes it back. GLM documents this as the condition of
    // `clear_thinking: false` without a tools caveat.
    let mut preserved = profile("glm-5.3", ProtocolKind::OpenAiChat, true);
    preserved.reasoning.style = leveler_model::ReasoningStyle::ThinkingFlag;
    preserved.reasoning.preserved_thinking = Some(true);
    let contract = ReasoningReplayContract::for_route(&preserved);
    assert_eq!(
        contract,
        ReasoningReplayContract::raw_field(
            ReasoningReplayScope::Always,
            MissingReasoningReplay::Omit,
        )
    );
    assert!(contract.replays_captured(false), "a tool-free turn replays");
    assert!(contract.requires_exact_retained_reasoning(false));

    // A weaker declaration contradicts the mode: the endpoint is told to keep
    // a chain the client would not return on every request.
    let mut weaker = preserved.clone();
    weaker.compatibility.reasoning_replay_scope = Some(ReasoningReplayScope::WhenToolsPresent);
    let conflict = ReasoningReplayContract::check_route(&weaker)
        .expect_err("preserved thinking with a tool-only scope is illegal");
    assert!(
        conflict.to_string().contains("preserved_thinking"),
        "{conflict}"
    );
}

#[test]
fn the_resolved_scope_decides_what_a_tool_free_request_carries() {
    // The scope is the contract's own answer; the projection must not decide
    // it again. A preserved-thinking route owes its chain back on a request
    // that exposes no tools, and the same history on a route that only replays
    // inside a tool loop must not carry it.
    use leveler_model::{
        ContentPart, Message, ReasoningRetention, ReasoningStyle, RequestProjection, Role,
        RouteIdentity,
    };

    let route = RouteIdentity {
        provider: "gateway".into(),
        model: "glm-5.3".into(),
        api: Some(ProtocolKind::OpenAiChat),
        deployment: None,
    };
    let handled = Message::from_parts(
        Role::Assistant,
        vec![
            ContentPart::Reasoning {
                text: "kept chain".into(),
            },
            ContentPart::Text {
                text: "answer".into(),
            },
        ],
        None,
    )
    .with_route(route.clone());
    let history = vec![
        Message::user_input("go"),
        handled,
        Message::text(Role::User, "continue"),
    ];

    let mut preserved = profile("glm-5.3", ProtocolKind::OpenAiChat, true);
    preserved.reasoning.style = ReasoningStyle::ThinkingFlag;
    preserved.reasoning.preserved_thinking = Some(true);
    let projection = RequestProjection::project_for_route(
        &history,
        &[],
        ReasoningReplayContract::for_route(&preserved),
        ReasoningRetention::All,
        Some(&route),
    );
    assert_eq!(
        projection.messages()[1].reasoning.as_wire(),
        Some("kept chain"),
        "preserved thinking owes the chain back without tools"
    );

    let mut tool_loop_only = profile("glm-5.3", ProtocolKind::OpenAiChat, true);
    tool_loop_only.reasoning.style = ReasoningStyle::ThinkingFlag;
    let projection = RequestProjection::project_for_route(
        &history,
        &[],
        ReasoningReplayContract::for_route(&tool_loop_only),
        ReasoningRetention::All,
        Some(&route),
    );
    assert_eq!(
        projection.messages()[1].reasoning.as_wire(),
        None,
        "a tool-only requirement leaves a tool-free request unchanged"
    );
}

#[test]
fn an_unlisted_below_k27_kimi_id_keeps_the_conservative_default() {
    // The product supports Kimi K2.7 and later only. An id that is not a
    // documented K2.7+/K3 model must not inherit their rule by string
    // similarity: it keeps the conservative `never` default, and a config that
    // wants replay declares its own scope.
    for wire_id in ["kimi-k2.6", "kimi-k2.5", "kimi-k2-0905-preview"] {
        let contract =
            ReasoningReplayContract::for_route(&profile(wire_id, ProtocolKind::OpenAiChat, true));
        assert_eq!(contract, ReasoningReplayContract::NONE, "{wire_id}");
    }
    let mut legacy = profile("kimi-k2.6", ProtocolKind::OpenAiChat, true);
    legacy.reasoning.preserved_thinking = Some(true);
    assert!(
        ReasoningReplayContract::check_route(&legacy).is_err(),
        "the unsupported id has no verified preserved-thinking field"
    );
}

#[test]
fn a_route_with_no_historical_reasoning_channel_stays_silent() {
    for wire_id in ["gpt-4o", "claude-sonnet-4-5", "some-local-model"] {
        let contract =
            ReasoningReplayContract::for_route(&profile(wire_id, ProtocolKind::OpenAiChat, true));
        assert_eq!(contract, ReasoningReplayContract::NONE, "{wire_id}");
    }
}

#[test]
fn an_under_declared_route_still_resolves_the_protocol_requirement() {
    // `reasoning = false` is a model-capability declaration, not a protocol
    // fact. DeepSeek's thinking is a server-side default, so a config that
    // forgets to declare it must not produce a tool request the provider
    // rejects. The family requirement wins.
    let contract = ReasoningReplayContract::for_route(&profile(
        "deepseek-flash",
        ProtocolKind::OpenAiChat,
        false,
    ));
    assert_eq!(
        contract,
        ReasoningReplayContract::raw_field(
            ReasoningReplayScope::WhenToolsPresent,
            MissingReasoningReplay::EmptyString,
        )
    );
}

#[test]
fn an_explicit_weaker_declaration_is_a_diagnosed_conflict() {
    let never = with_scope(
        profile("deepseek-v4-flash", ProtocolKind::OpenAiChat, true),
        ReasoningReplayScope::Never,
    );
    let conflict = ReasoningReplayContract::check_route(&never).unwrap_err();
    let message = conflict.to_string();
    assert!(message.contains("deepseek-v4-flash"), "{message}");
    assert!(message.contains("HTTP 400"), "{message}");
    assert!(message.contains("reasoning_replay_scope"), "{message}");

    let mut no_empty_key = profile("deepseek-v4-flash", ProtocolKind::OpenAiChat, true);
    no_empty_key.compatibility.reasoning_content_key_required = Some(false);
    let conflict = ReasoningReplayContract::check_route(&no_empty_key).unwrap_err();
    assert!(
        conflict
            .to_string()
            .contains("reasoning_content_key_required"),
        "{conflict}"
    );
}

#[test]
fn a_documented_default_may_be_opted_out_but_a_hard_requirement_may_not() {
    // GLM documents replay as the correct tool-loop default, but a standard
    // endpoint can run with preserved thinking disabled. An explicit `never`
    // is a real deployment fact, not an illegal request.
    let glm = with_scope(
        profile("glm-5.3", ProtocolKind::OpenAiChat, true),
        ReasoningReplayScope::Never,
    );
    ReasoningReplayContract::check_route(&glm).unwrap();
    assert_eq!(
        ReasoningReplayContract::for_route(&glm),
        ReasoningReplayContract::NONE
    );

    // DeepSeek rejects the request itself, so the same declaration is refused.
    let deepseek = with_scope(
        profile("deepseek-flash", ProtocolKind::OpenAiChat, true),
        ReasoningReplayScope::Never,
    );
    assert!(ReasoningReplayContract::check_route(&deepseek).is_err());
}

#[test]
fn an_explicit_stronger_declaration_is_honoured() {
    let mut always = with_scope(
        profile("deepseek-flash", ProtocolKind::OpenAiChat, true),
        ReasoningReplayScope::Always,
    );
    always.compatibility.reasoning_content_key_required = Some(true);
    ReasoningReplayContract::check_route(&always).unwrap();
    assert_eq!(
        ReasoningReplayContract::for_route(&always),
        ReasoningReplayContract::raw_field(
            ReasoningReplayScope::Always,
            MissingReasoningReplay::EmptyString,
        )
    );
}

#[test]
fn the_legacy_alias_keeps_its_scope_and_gains_the_family_requirement() {
    // `passback_reasoning_content = true` was the old spelling of the scope;
    // it never expressed the empty-key requirement. A deepseek route that
    // still uses it must resolve to the complete requirement instead of a
    // contract that can still 400.
    let mut legacy = profile("deepseek-flash", ProtocolKind::OpenAiChat, true);
    legacy.compatibility = serde_json::from_value(serde_json::json!({
        "passback_reasoning_content": true
    }))
    .unwrap();
    assert_eq!(
        legacy.compatibility.reasoning_replay_scope,
        Some(ReasoningReplayScope::WhenToolsPresent)
    );
    assert_eq!(legacy.compatibility.reasoning_content_key_required, None);
    assert_eq!(
        ReasoningReplayContract::for_route(&legacy),
        ReasoningReplayContract::raw_field(
            ReasoningReplayScope::WhenToolsPresent,
            MissingReasoningReplay::EmptyString,
        )
    );
}

#[test]
fn a_preserved_thinking_route_may_not_stop_replaying() {
    // GLM's preserved thinking is a mode the route asks for on the wire
    // (`clear_thinking: false`) and its documented condition is that the
    // complete, unmodified reasoning comes back. Declaring `never` would make
    // the request ask the endpoint to keep a chain the client never returns —
    // the one combination that is not a deployment difference but a
    // contradiction.
    let mut preserved = profile("glm-5.3", ProtocolKind::OpenAiChat, true);
    preserved.reasoning.style = leveler_model::ReasoningStyle::ThinkingFlag;
    preserved.reasoning.preserved_thinking = Some(true);
    ReasoningReplayContract::check_route(&preserved).unwrap();
    assert_eq!(
        ReasoningReplayContract::for_route(&preserved),
        ReasoningReplayContract::raw_field(
            ReasoningReplayScope::Always,
            MissingReasoningReplay::Omit,
        )
    );

    let mut stop = preserved.clone();
    stop.compatibility.reasoning_replay_scope = Some(ReasoningReplayScope::Never);
    let conflict = ReasoningReplayContract::check_route(&stop)
        .expect_err("preserved thinking without replay is illegal");
    assert!(
        conflict.to_string().contains("preserved_thinking"),
        "{conflict}"
    );
    // Even an unresolved caller cannot build the illegal request: the resolved
    // contract still replays.
    assert_eq!(
        ReasoningReplayContract::for_route(&stop).scope,
        ReasoningReplayScope::Always
    );
}

#[test]
fn a_deployment_that_turned_preserved_thinking_off_may_stop_replaying() {
    // The legal exit is a real mode change, not a silent drop: `clear_thinking:
    // true` tells the endpoint to discard prior thinking, and only then does
    // not returning it describe the same mode.
    let mut cleared = profile("glm-5.3", ProtocolKind::OpenAiChat, true);
    cleared.reasoning.style = leveler_model::ReasoningStyle::ThinkingFlag;
    cleared.reasoning.preserved_thinking = Some(false);
    cleared.compatibility.reasoning_replay_scope = Some(ReasoningReplayScope::Never);
    ReasoningReplayContract::check_route(&cleared).expect("a cleared mode may stop replaying");
    assert_eq!(
        ReasoningReplayContract::for_route(&cleared).scope,
        ReasoningReplayScope::Never
    );
}

#[test]
fn the_preserved_mode_is_a_family_fact_not_a_universal_one() {
    // A preserved-thinking declaration only means something where the wire has
    // a field for it, and today only GLM spells the mode (`clear_thinking`).
    // DeepSeek has no such field; Kimi K2.7 and later and K3/K3.1 preserve
    // thinking unconditionally and take no `thinking` parameter at all, so a
    // declaration there would either send another endpoint's field or do
    // nothing — both are silent lies about the request.
    for wire in [
        "deepseek-flash",
        "k3",
        "k3.1-pro",
        "kimi-k2.7-code",
        "kimi-k2.8-preview",
        "kimi-for-coding",
        "kimi-for-coding-highspeed",
    ] {
        let mut p = profile(wire, ProtocolKind::OpenAiChat, true);
        p.reasoning.preserved_thinking = Some(true);
        assert!(
            ReasoningReplayContract::check_route(&p).is_err(),
            "{wire} has no preserved-thinking field to honour"
        );
    }

    // GLM is the one family that spells the mode, in both directions.
    let mut glm = profile("glm-5.3", ProtocolKind::OpenAiChat, true);
    glm.reasoning.style = leveler_model::ReasoningStyle::ThinkingFlag;
    glm.reasoning.preserved_thinking = Some(true);
    ReasoningReplayContract::check_route(&glm).expect("GLM honours clear_thinking");
    let mut glm_stop = glm.clone();
    glm_stop.compatibility.reasoning_replay_scope = Some(ReasoningReplayScope::Never);
    assert!(ReasoningReplayContract::check_route(&glm_stop).is_err());
}

#[test]
fn the_legacy_alias_still_parses_but_cannot_weaken_a_hard_requirement() {
    // `passback_reasoning_content = false` was the old spelling of `never`. It
    // keeps loading (an old config must not stop working), and it keeps its
    // meaning wherever the route may exit — but on a route whose protocol
    // requires replay it is the same illegal pair as the new spelling.
    let mut old = profile("deepseek-flash", ProtocolKind::OpenAiChat, true);
    old.compatibility = serde_json::from_value(serde_json::json!({
        "passback_reasoning_content": false
    }))
    .unwrap();
    assert_eq!(
        old.compatibility.reasoning_replay_scope,
        Some(ReasoningReplayScope::Never)
    );
    assert!(ReasoningReplayContract::check_route(&old).is_err());
    // The resolved contract still replays, so an unchecked caller cannot send
    // the request the provider would reject.
    assert!(ReasoningReplayContract::for_route(&old).replays_captured(true));

    // On a GLM route that asked the endpoint to preserve thinking, the same
    // legacy spelling is the contradiction `preserved_thinking` refuses.
    let mut glm = profile("glm-5.3", ProtocolKind::OpenAiChat, true);
    glm.reasoning.style = leveler_model::ReasoningStyle::ThinkingFlag;
    glm.reasoning.preserved_thinking = Some(true);
    glm.compatibility = old.compatibility.clone();
    assert!(ReasoningReplayContract::check_route(&glm).is_err());
}

#[test]
fn a_kimi_route_that_spells_a_thinking_object_is_refused() {
    // The K2.7+/K3 families define no `thinking` request object: K2.7 Code is
    // always thinking and takes no parameter for it, K3/K3.1 address reasoning
    // with a top-level `reasoning_effort`. A `thinking_flag` declaration would
    // put an illegal field on the wire (or be silently dropped), so it is a
    // diagnosed contradiction instead.
    for wire in [
        "k2.7-code",
        "kimi-k2.7-code",
        "kimi-for-coding",
        "kimi-for-coding-highspeed",
        "k3",
        "k3.1-pro",
    ] {
        let mut p = profile(wire, ProtocolKind::OpenAiChat, true);
        p.reasoning.style = leveler_model::ReasoningStyle::ThinkingFlag;
        p.reasoning.supported_efforts = vec![leveler_model::ReasoningEffort::High];
        let conflict = ReasoningReplayContract::check_route(&p)
            .expect_err("a family without a thinking field cannot declare one");
        assert!(
            conflict.to_string().contains("thinking"),
            "{wire}: {conflict}"
        );

        // The documented styles stay legal: effort on K3/K3.1 and K2.8, and no
        // style at all on K2.7 Code.
        p.reasoning.style = leveler_model::ReasoningStyle::OpenAiEffort;
        ReasoningReplayContract::check_route(&p).expect("effort-addressed routes are legal");
        p.reasoning.style = leveler_model::ReasoningStyle::None;
        p.reasoning.supported_efforts.clear();
        p.reasoning.default_effort = None;
        ReasoningReplayContract::check_route(&p).expect("K2.7 Code takes no effort knob");
    }
}

#[test]
fn a_protocol_without_a_reasoning_content_field_is_not_widened_by_a_model_family() {
    // The Messages protocol has its own authenticated-block channel; the
    // families here describe the OpenAI-Chat `reasoning_content` field only.
    let anthropic = ReasoningReplayContract::for_route(&profile(
        "deepseek-flash",
        ProtocolKind::AnthropicMessages,
        true,
    ));
    assert_eq!(anthropic, ReasoningReplayContract::signed_block());

    for protocol in [
        ProtocolKind::OpenAiResponses,
        ProtocolKind::GeminiGenerateContent,
    ] {
        let contract =
            ReasoningReplayContract::for_route(&profile("deepseek-flash", protocol, true));
        assert_eq!(contract, ReasoningReplayContract::NONE, "{protocol:?}");
    }
}
