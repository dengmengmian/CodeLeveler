//! Canonical intent is inspected through production wire encoders, never a mock encoder.
use leveler_model::{
    Message, ModelRef, ModelRequest, ProtocolAdapter, ProtocolContext, ReasoningConfig,
    ReasoningEffort as E, ReasoningReplayContract, ReasoningStyle as S, Role, ThinkingCapabilities,
    ThinkingLevel as L, ThinkingProjection,
};
use leveler_protocol::{AnthropicMessagesAdapter, OpenAiChatAdapter};

#[test]
fn canonical_capability_wire_matrix() {
    let cases = [
        (false, S::None, vec![]),
        (true, S::None, vec![]),
        (true, S::ThinkingFlag, vec![E::High]),
        (true, S::ThinkingFlag, vec![E::Low, E::High, E::Max]),
        (true, S::OpenAiEffort, vec![E::Low, E::High]),
        (true, S::OpenAiEffort, vec![E::Low, E::Medium, E::High]),
        (
            true,
            S::OpenAiEffort,
            vec![E::Low, E::Medium, E::High, E::XHigh],
        ),
        (
            true,
            S::AdaptiveThinking,
            vec![E::Low, E::Medium, E::High, E::Max],
        ),
        (
            true,
            S::BudgetedThinking {
                budget_tokens: 1024,
            },
            vec![],
        ),
    ];
    for (reasons, style, supported) in cases {
        let config = ReasoningConfig {
            style,
            default_effort: supported.last().copied(),
            supported_efforts: supported.clone(),
        };
        let caps = ThinkingCapabilities::of(reasons, &config);
        let context = ProtocolContext {
            base_url: "https://example.invalid".into(),
            model_id: "matrix".into(),
            api_key: None,
            extra_headers: vec![],
            reasoning: config,
            parallel_tool_calls: true,
            supports_temperature: true,
            thinking_supports_forced_tool_choice: true,
            reasoning_replay: ReasoningReplayContract::NONE,
        };
        let messages_style = matches!(style, S::AdaptiveThinking | S::BudgetedThinking { .. });
        let adapter: Box<dyn ProtocolAdapter> = if messages_style {
            Box::new(AnthropicMessagesAdapter::new())
        } else {
            Box::new(OpenAiChatAdapter::new())
        };
        for level in L::ALL {
            let expected = match level {
                L::Auto => None,
                L::Max => supported.last().copied(),
                L::Minimal => Some(E::Minimal).filter(|e| supported.contains(e)),
                L::Low => Some(E::Low).filter(|e| supported.contains(e)),
                L::Medium => Some(E::Medium).filter(|e| supported.contains(e)),
                L::High => Some(E::High).filter(|e| supported.contains(e)),
                L::Off => None,
            };
            let projection = caps.request(level);
            let mut request = ModelRequest::new(
                ModelRef::new("test", "matrix"),
                vec![Message::text(Role::User, "hello")],
            );
            request.max_output_tokens = Some(4096);
            request.reasoning_effort = match projection {
                ThinkingProjection::Effort(e) => Some(e),
                _ => None,
            };
            request.thinking_disabled = projection == ThinkingProjection::Disabled;
            assert_eq!(request.reasoning_effort, expected, "{style:?}/{level}");
            let body = adapter
                .encode_request(&request, &context, false)
                .unwrap()
                .body;
            let wire_effort = if messages_style {
                body.get("output_config").and_then(|v| v.get("effort"))
            } else {
                body.get("reasoning_effort")
            };
            assert_eq!(
                wire_effort.and_then(|v| v.as_str()),
                expected.map(E::as_wire),
                "{style:?}/{level}: {body}"
            );
            if matches!(style, S::ThinkingFlag) {
                assert_eq!(
                    body["thinking"]["type"],
                    if level == L::Off {
                        "disabled"
                    } else {
                        "enabled"
                    }
                );
            } else if matches!(style, S::None | S::OpenAiEffort) {
                assert!(body.get("thinking").is_none(), "{style:?}/{level}: {body}");
            }
            if level == L::Auto {
                assert!(
                    wire_effort.is_none(),
                    "auto must not read default_effort: {body}"
                );
            }
        }
        let mut effects = Vec::new();
        for level in caps.levels() {
            let effect = caps.request(*level);
            assert!(
                !effects.contains(&effect),
                "duplicate visible effect: {style:?}/{level}"
            );
            effects.push(effect);
        }
    }
}
