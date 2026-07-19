//! End-to-end integration tests driving the real HTTP transport + OpenAI Chat
//! decoder against a scripted mock provider (spec §48, §53.15-16).

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use leveler_model::{
    ModelCapabilities, ModelEvent, ModelLimits, ModelProfile, ModelRef, ModelRequest, ModelRuntime,
    ProtocolKind, Role,
};
use leveler_provider::{
    ModelConfigFile, ProviderConfig, ProviderRegistry, RegistryInputs, RetryConfig, Timeouts,
};
use leveler_test_support::{MockResponse, MockServer};

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
            idle_stream_seconds: 10,
        },
        retry: RetryConfig {
            max_attempts: 3,
            initial_backoff_ms: 5,
            max_backoff_ms: 20,
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
            },
            reasoning: Default::default(),
            compatibility: Default::default(),
            instructions: None,
            pricing: None,
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
        vec![leveler_model::Message::text(Role::User, "hi")],
    )
}

async fn collect(stream: leveler_model::ModelEventStream) -> Vec<ModelEvent> {
    stream.filter_map(|e| async move { e.ok() }).collect().await
}

#[tokio::test]
async fn streaming_happy_path() {
    let server = MockServer::start_one(MockResponse::sse(&[
        r#"{"choices":[{"delta":{"content":"Hello"}}]}"#,
        r#"{"choices":[{"delta":{"content":", world"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
    ]))
    .await;
    let reg = registry(&server);

    let stream = reg
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    assert!(matches!(
        events.first(),
        Some(ModelEvent::MessageStarted { .. })
    ));
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello, world");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelEvent::MessageCompleted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelEvent::UsageUpdated { .. }))
    );
    assert!(!events.iter().any(|e| matches!(e, ModelEvent::Error { .. })));
}

#[tokio::test]
async fn streaming_tool_call_reassembly() {
    let server = MockServer::start_one(MockResponse::sse(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"grep","arguments":"{\"pat"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"tern\":\"x\"}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]))
    .await;
    let reg = registry(&server);

    let stream = reg
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    let call = events
        .iter()
        .find_map(|e| match e {
            ModelEvent::ToolCallCompleted { call } => Some(call),
            _ => None,
        })
        .expect("tool call completes");
    assert_eq!(call.name, "grep");
    assert_eq!(call.id.as_str(), "call_1");
    assert_eq!(call.arguments["pattern"], "x");
}

#[tokio::test]
async fn stream_interrupted_is_reported() {
    // No [DONE], no finish event — the connection just closes.
    let server = MockServer::start_one(MockResponse::sse_interrupted(&[
        r#"{"choices":[{"delta":{"content":"partial"}}]}"#,
    ]))
    .await;
    let reg = registry(&server);

    let stream = reg
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    assert!(events.iter().any(|e| matches!(
        e,
        ModelEvent::Error {
            error
        } if error.kind == leveler_model::ModelErrorKind::StreamInterrupted
    )));
}

#[tokio::test]
async fn retries_on_429_then_succeeds() {
    let server = MockServer::start(vec![
        MockResponse::too_many_requests(),
        MockResponse::sse(&[r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#]),
    ])
    .await;
    let reg = registry(&server);

    let stream = reg
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "ok");
    assert_eq!(
        server.request_count(),
        2,
        "should have retried once after 429"
    );
}

#[tokio::test]
async fn non_retryable_400_fails_fast() {
    let server = MockServer::start_one(MockResponse::Status {
        code: 400,
        body: r#"{"error":{"message":"bad"}}"#.into(),
    })
    .await;
    let reg = registry(&server);

    let result = reg.stream(request(), CancellationToken::new()).await;
    let err = result.err().expect("should fail");
    assert_eq!(err.kind, leveler_model::ModelErrorKind::InvalidRequest);
    assert_eq!(server.request_count(), 1, "400 must not be retried");
}

#[tokio::test]
async fn exhausted_provider_retries_are_terminal_for_outer_layers() {
    let server = MockServer::start(vec![
        MockResponse::too_many_requests(),
        MockResponse::too_many_requests(),
        MockResponse::too_many_requests(),
    ])
    .await;
    let reg = registry(&server);

    let err = reg
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("the exhausted provider must fail");

    assert_eq!(server.request_count(), 3);
    assert!(
        !err.retryable,
        "the agent must not multiply an already exhausted provider retry loop"
    );
}

#[tokio::test]
async fn fragmented_stream_is_tolerated() {
    // A complete SSE body delivered one byte per network chunk.
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"frag\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let server = MockServer::start_one(MockResponse::fragmented(body, 1)).await;
    let reg = registry(&server);

    let stream = reg
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "frag");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelEvent::MessageCompleted { .. }))
    );
}

#[tokio::test]
async fn non_streaming_generate() {
    let server = MockServer::start_one(MockResponse::json_ok(
        r#"{"id":"r1","choices":[{"message":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
    ))
    .await;
    let reg = registry(&server);

    let resp = reg
        .generate(request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.message.text_content(), "answer");
    assert_eq!(resp.usage.total(), 4);
}

#[tokio::test]
async fn cancellation_stops_before_request() {
    let server = MockServer::start_one(MockResponse::sse(&[
        r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"stop"}]}"#,
    ]))
    .await;
    let reg = registry(&server);

    let token = CancellationToken::new();
    token.cancel();
    let result = reg.stream(request(), token).await;
    assert_eq!(
        result.err().map(|e| e.kind),
        Some(leveler_model::ModelErrorKind::Cancelled)
    );
}

#[tokio::test]
async fn rate_limit_retry_honors_retry_after_header() {
    // The provider says "wait 1s". The configured backoff is 5ms — if the
    // header is ignored, the retry lands almost immediately.
    let server = MockServer::start(vec![
        MockResponse::too_many_requests_retry_after(1),
        MockResponse::sse(&[r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#]),
    ])
    .await;
    let reg = registry(&server);

    let started = std::time::Instant::now();
    let stream = reg
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;
    let elapsed = started.elapsed();

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "ok", "the retry after the advertised delay succeeds");
    assert_eq!(server.request_count(), 2);
    assert!(
        elapsed >= std::time::Duration::from_millis(900),
        "the retry must wait out Retry-After (~1s), not the 5ms backoff; \
         elapsed: {elapsed:?}"
    );
}
