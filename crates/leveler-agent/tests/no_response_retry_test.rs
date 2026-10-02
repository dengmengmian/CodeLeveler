//! An upstream that received the whole request and never returned response
//! headers used to end the *whole coding task*, while the identical transient
//! condition one phase later (headers received, stream cut before any output)
//! was recovered. This pins down the delivery state that caused the asymmetry.
//!
//! Nothing on the path under test is stubbed: a real socket, the real reqwest
//! read-idle timeout, the real transport error mapping and the real round retry
//! decision all run. The read-idle budget is the test's own 1 s; the 120 s the
//! defect was observed with is not what is under test.

use leveler_agent_core::{AgentEvent, run_model_round};
use leveler_model::{
    ContentPart, DeliveryState, Message, ModelCapabilities, ModelErrorKind, ModelLimits,
    ModelProfile, ModelRef, ModelRequest, ModelRuntime, ProtocolKind, Retryability, Role,
};
use leveler_provider::{
    ModelConfigFile, ProviderConfig, ProviderRegistry, RegistryInputs, Timeouts,
};
use leveler_test_support::{MockResponse, MockServer};
use tokio_util::sync::CancellationToken;

fn provider_config(base_url: String) -> ProviderConfig {
    ProviderConfig {
        id: "mock".into(),
        protocol: ProtocolKind::OpenAiChat,
        base_url,
        api_key_env: String::new(),
        api_key: None,
        headers: Default::default(),
        timeouts: Timeouts {
            connect_seconds: 5,
            request_seconds: 30,
            // The wait that produced the defect, kept small so the regression
            // test costs a second rather than two minutes.
            idle_stream_seconds: 1,
        },
    }
}

fn model_config() -> ModelConfigFile {
    ModelConfigFile {
        profile: ModelProfile {
            id: "m".into(),
            provider: "mock".into(),
            model_id: "mock-model".into(),
            protocol: ProtocolKind::OpenAiChat,
            capabilities: ModelCapabilities {
                streaming: true,
                tool_calling: true,
                parallel_tool_calls: false,
                structured_output: true,
                reasoning: false,
                vision: false,
            },
            limits: ModelLimits {
                context_window: 8192,
                reliable_context: 4096,
                max_output_tokens: 1024,
                max_tool_schema_bytes: 8192,
                max_parallel_tool_calls: 1,
                max_tool_output_bytes: None,
            },
            context_quality: None,
            reasoning: Default::default(),
            compatibility: Default::default(),
            pricing: None,
            thinking: None,
        },
        policy: None,
    }
}

fn registry(server: &MockServer) -> ProviderRegistry {
    ProviderRegistry::build(RegistryInputs {
        providers: vec![(provider_config(server.base_url()), None)],
        models: vec![model_config()],
    })
    .expect("build registry")
}

fn request() -> ModelRequest {
    ModelRequest::new(
        ModelRef::new("mock", "m"),
        vec![Message::text(Role::User, "hi")],
    )
}

fn completed_sse() -> MockResponse {
    MockResponse::sse(&[
        r#"{"choices":[{"delta":{"content":"recovered"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
    ])
}

/// The transport's own classification. The request was written, so this is not
/// `NotSent`; no response byte was ever read, so it is not `Responded` and not
/// `StreamInterrupted` either.
///
/// The *fast* path must leave it alone: it exists for failures that provably did
/// no provider-side work, and a no-response wait costs as much as the timeout
/// that produced it. The bounded logical lifecycle is the owner that may recover
/// it, because nothing observable was received to duplicate.
#[tokio::test]
async fn a_provider_that_never_responds_is_recoverable_but_not_blind_replayed() {
    let server = MockServer::start_one(MockResponse::NeverResponds).await;
    let reg = registry(&server);

    let err = reg
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("a provider that never answers must time out");

    assert_eq!(err.kind, ModelErrorKind::Timeout);
    assert_eq!(err.delivery_state, DeliveryState::SentNoResponse);
    assert_eq!(
        server.request_count(),
        1,
        "the fast path must not re-send a request that was fully written and \
         never answered"
    );
    assert_eq!(
        err.retryability(),
        Retryability::Unknown,
        "the provider may have generated: this is never a blind replay"
    );
    assert!(
        err.is_retryable_by_lifecycle(),
        "the bounded logical lifecycle must be able to recover it"
    );
}

/// The defect end to end: the first upstream connection swallows the request and
/// never returns response headers; the second answers normally. The round must
/// recover inside its own bounded budget instead of failing the task.
#[tokio::test]
async fn a_round_recovers_from_an_upstream_that_never_sends_headers() {
    let server = MockServer::start(vec![MockResponse::NeverResponds, completed_sse()]).await;
    let reg = registry(&server);
    let mut events = Vec::new();

    let round = run_model_round(&reg, request(), &CancellationToken::new(), &mut |e| {
        events.push(e)
    })
    .await
    .expect("the round must recover instead of failing the task");

    assert_eq!(round.retry_count, 1, "exactly one logical retry");
    assert_eq!(
        server.request_count(),
        2,
        "one unanswered attempt, then one retry — and no fast-path replay in \
         between"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ModelRetrying { attempt: 1, .. })),
        "the retry is announced: {events:?}"
    );
    let text: String = round
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, "recovered",
        "the second attempt's answer is the round's"
    );
}
