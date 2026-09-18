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
                max_tool_output_bytes: None,
            },
            context_quality: None,
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
    assert_eq!(err.delivery_state, leveler_model::DeliveryState::Responded);
    assert_eq!(err.retryability(), leveler_model::Retryability::Never);
    assert_eq!(err.provider.as_deref(), Some("mock"));
    assert_eq!(err.model(), Some("m"), "the addressed model is stamped");
    assert_eq!(err.status, Some(400));
    assert_eq!(
        err.message, "bad",
        "the provider's own message is the reason, not the raw envelope"
    );
    assert_eq!(server.request_count(), 1, "400 must not be retried");
}

/// A provider's structured error code and correlation id survive the whole
/// transport, so a report can name exactly what the vendor rejected and which
/// request to look up. Extracted once here; nothing downstream parses the body.
#[tokio::test]
async fn provider_error_code_and_request_id_are_preserved() {
    let server = MockServer::start_one(MockResponse::StatusWithHeaders {
        code: 400,
        body: r#"{"error":{"message":"tools.function.parameters is not a valid moonshot flavored json schema","type":"invalid_request_error","code":"invalid_request"}}"#.into(),
        headers: vec![("x-request-id".to_string(), "req_live_123".to_string())],
    })
    .await;
    let reg = registry(&server);

    let err = reg
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("400 must fail");
    assert_eq!(err.status, Some(400));
    assert_eq!(err.provider_code(), Some("invalid_request"));
    assert_eq!(err.request_id(), Some("req_live_123"));
    assert_eq!(
        err.message,
        "tools.function.parameters is not a valid moonshot flavored json schema"
    );
}

/// A provider body is untrusted external text. A secret-shaped token in it must
/// be redacted at the transport boundary, before anything stores or shows it.
#[tokio::test]
async fn a_secret_in_a_provider_error_body_is_redacted() {
    let server = MockServer::start_one(MockResponse::Status {
        code: 401,
        body: r#"{"error":{"message":"invalid key: sk-live-supersecret"}}"#.into(),
    })
    .await;
    let reg = registry(&server);

    let err = reg
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("401 must fail");
    assert!(
        !err.message.contains("sk-live-supersecret"),
        "secret leaked: {}",
        err.message
    );
    assert!(err.message.contains("[redacted]"), "{}", err.message);
}

/// A gateway failure the model never saw is transient: it is retried.
#[tokio::test]
async fn transient_gateway_503_is_retried_then_succeeds() {
    let server = MockServer::start(vec![
        MockResponse::Status {
            code: 503,
            body: r#"{"error":{"message":"down"}}"#.into(),
        },
        MockResponse::sse(&[r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#]),
    ])
    .await;
    let reg = registry(&server);
    let stream = reg
        .stream(request(), CancellationToken::new())
        .await
        .expect("503 is transient");
    let _ = collect(stream).await;
    assert_eq!(server.request_count(), 2, "503 is transient and is retried");
}

/// A 504 may mean the upstream started: not a safe replay, so no auto retry.
#[tokio::test]
async fn gateway_timeout_504_is_not_auto_retried() {
    let server = MockServer::start_one(MockResponse::Status {
        code: 504,
        body: r#"{"error":{"message":"gateway timeout"}}"#.into(),
    })
    .await;
    let reg = registry(&server);
    let err = reg
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("504 must fail");
    assert_eq!(server.request_count(), 1, "504 must not be auto-retried");
    assert_eq!(err.status, Some(504));
    assert_eq!(err.delivery_state, leveler_model::DeliveryState::Responded);
    assert_eq!(err.retryability(), leveler_model::Retryability::Caution);
}

/// A connect failure provably did no provider-side work: `NotSent`, safe to
/// retry. The transport still bounds its own attempts.
#[tokio::test]
async fn a_connect_failure_is_not_sent_and_safe_to_retry() {
    // Nothing listens on port 1: the connection is refused before any byte.
    let reg = ProviderRegistry::build(RegistryInputs {
        providers: vec![(provider_config("http://127.0.0.1:1".into()), None)],
        models: vec![model_config()],
    })
    .expect("build registry");
    let err = reg
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("connect must fail");
    assert_eq!(err.delivery_state, leveler_model::DeliveryState::NotSent);
    assert_eq!(err.retryability(), leveler_model::Retryability::Safe);
    assert_eq!(err.kind, leveler_model::ModelErrorKind::ProviderUnavailable);
    assert_eq!(err.provider.as_deref(), Some("mock"));
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
    // R006 R6-P3: a 429 stays `Safe` to retry (an explicit "come back later") —
    // the exhausted fast budget is signalled separately so the outer logical
    // retry is not silently forbidden to retry at all.
    assert!(
        err.is_safe_to_retry(),
        "a rate limit must survive provider exhaustion as Safe: {err:?}"
    );
    assert!(
        err.provider_retries_exhausted,
        "the exhausted fast budget must be flagged as diagnostic"
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

/// `[DONE]` is the OpenAI SSE protocol's explicit end-of-stream marker. A
/// gateway that streams the whole answer and then sends `[DONE]` WITHOUT ever
/// putting `finish_reason` in a chunk has delivered a complete response — the
/// decoder must accept it.
///
/// Treating `[DONE]` as noise instead makes the decoder keep waiting on a
/// stream the server considers finished: the answer is fully on screen while
/// the status line still reads "waiting for model", until the idle read timeout
/// fires and the whole (large) request is retried as StreamInterrupted.
#[tokio::test]
async fn done_sentinel_completes_a_stream_that_never_sent_finish_reason() {
    let server = MockServer::start_one(MockResponse::sse(&[
        r#"{"choices":[{"delta":{"content":"the whole answer"}}]}"#,
    ]))
    .await;
    let reg = registry(&server);

    let stream = reg
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    assert!(
        !events.iter().any(|e| matches!(
            e,
            ModelEvent::Error { error }
                if error.kind == leveler_model::ModelErrorKind::StreamInterrupted
        )),
        "[DONE] means the server finished; this must not read as an interruption: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelEvent::MessageCompleted { .. })),
        "the stream must terminate with a completion event: {events:?}"
    );
}

/// The counterpart to the case above: `[DONE]` with no content at all is NOT a
/// usable response, so it must keep reporting an interruption (retryable).
#[tokio::test]
async fn done_sentinel_with_no_content_is_still_an_interruption() {
    let server = MockServer::start_one(MockResponse::sse(&[])).await;
    let reg = registry(&server);

    let stream = reg
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            ModelEvent::Error { error }
                if error.kind == leveler_model::ModelErrorKind::StreamInterrupted
        )),
        "an empty [DONE]-only stream has no answer to accept: {events:?}"
    );
}

/// A provider whose idle watchdog fires quickly, so a silent gap is decided in
/// milliseconds instead of the shipped minute. Retries are off: this is about
/// the FIRST attempt surviving, not about recovering from a kill.
fn impatient_config(base_url: String) -> ProviderConfig {
    ProviderConfig {
        timeouts: Timeouts {
            connect_seconds: 5,
            request_seconds: 30,
            idle_stream_seconds: 1,
        },
        retry: RetryConfig {
            max_attempts: 1,
            initial_backoff_ms: 5,
            max_backoff_ms: 20,
        },
        ..provider_config(base_url)
    }
}

fn impatient_registry(server: &MockServer) -> ProviderRegistry {
    ProviderRegistry::build(RegistryInputs {
        providers: vec![(impatient_config(server.base_url()), None)],
        models: vec![model_config()],
    })
    .expect("build registry")
}

/// The provider that thinks for longer than the idle watchdog allows.
fn thinks_for(ms: u64) -> MockResponse {
    MockResponse::SilentThenJson {
        silent_ms: ms,
        body: r#"{"id":"r1","choices":[{"message":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#
            .to_string(),
    }
}

/// F5: a non-streaming model call produces NO bytes while it reasons, so the
/// read-idle watchdog reads thinking as a dead connection and kills it — which
/// is what turned a 20-78s judgment into a 243.5s wall of four killed attempts.
/// A request that says its silence is expected survives it.
#[tokio::test]
async fn a_long_thinking_request_outlives_the_idle_watchdog() {
    let server = MockServer::start_one(thinks_for(1_500)).await;
    let reg = impatient_registry(&server);

    let mut req = request();
    req.transport = leveler_model::TransportPolicy::LongThinkingNonStreaming;
    req.deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
    let resp = reg
        .generate(req, CancellationToken::new())
        .await
        .expect("silence while thinking is not a dead connection");
    assert_eq!(resp.message.text_content(), "answer");
    assert_eq!(server.request_count(), 1, "and it is not retried");
}

/// Control: an ordinary request keeps the watchdog it always had.
#[tokio::test]
async fn an_ordinary_request_still_dies_on_the_idle_watchdog() {
    let server = MockServer::start_one(thinks_for(1_500)).await;
    let reg = impatient_registry(&server);

    let error = reg
        .generate(request(), CancellationToken::new())
        .await
        .expect_err("the default policy is unchanged");
    assert!(
        format!("{error}").to_lowercase().contains("timeout")
            || format!("{error}").to_lowercase().contains("error sending"),
        "expected an idle timeout, got: {error}"
    );
}

/// The relaxed watchdog is not a licence to wait forever: the caller's
/// deadline still ends it, and nothing is returned.
#[tokio::test]
async fn a_long_thinking_request_still_dies_at_its_deadline() {
    let server = MockServer::start_one(thinks_for(3_000)).await;
    let reg = impatient_registry(&server);

    let mut req = request();
    req.transport = leveler_model::TransportPolicy::LongThinkingNonStreaming;
    req.deadline = Some(std::time::Instant::now() + std::time::Duration::from_millis(600));
    let started = std::time::Instant::now();
    let error = reg
        .generate(req, CancellationToken::new())
        .await
        .expect_err("the deadline is the bound");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(2_000),
        "it must end at its own deadline, not at the provider's pace: {:?}",
        started.elapsed()
    );
    assert!(!format!("{error}").is_empty());
}

/// Two policies, one registry: the patient request must not lend its patience
/// to anyone else.
#[tokio::test]
async fn the_two_policies_do_not_share_a_watchdog() {
    let server = MockServer::start_one(thinks_for(1_500)).await;
    let reg = impatient_registry(&server);

    let mut patient = request();
    patient.transport = leveler_model::TransportPolicy::LongThinkingNonStreaming;
    patient.deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
    let (patient_result, ordinary_result) = tokio::join!(
        reg.generate(patient, CancellationToken::new()),
        reg.generate(request(), CancellationToken::new()),
    );
    assert!(patient_result.is_ok(), "{patient_result:?}");
    assert!(ordinary_result.is_err(), "{ordinary_result:?}");
}

fn tool_call_message(ids: &[&str]) -> leveler_model::Message {
    leveler_model::Message {
        role: Role::Assistant,
        content: ids
            .iter()
            .map(|id| leveler_model::ContentPart::ToolCall {
                call: leveler_model::ToolCall {
                    id: leveler_core::ToolCallId::new(*id),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "a.rs"}),
                },
            })
            .collect(),
    }
}

fn tool_result_message(ids: &[&str]) -> leveler_model::Message {
    leveler_model::Message {
        role: Role::Tool,
        content: ids
            .iter()
            .map(|id| leveler_model::ContentPart::ToolResult {
                result: leveler_model::ToolResultContent {
                    call_id: leveler_core::ToolCallId::new(*id),
                    content: format!("result {id}"),
                    is_error: false,
                },
            })
            .collect(),
    }
}

fn request_with(messages: Vec<leveler_model::Message>) -> ModelRequest {
    ModelRequest::new(ModelRef::new("mock", "m"), messages)
}

fn assert_refused_before_send(error: &leveler_model::ModelError, needle: &str) {
    assert_eq!(
        error.kind,
        leveler_model::ModelErrorKind::ConversationProtocol,
        "{error}"
    );
    assert_eq!(error.delivery_state, leveler_model::DeliveryState::NotSent);
    assert_eq!(error.retryability(), leveler_model::Retryability::Never);
    assert!(error.message.contains(needle), "{}", error.message);
}

/// The DeepSeek 400 "Messages with role 'tool' must be a response to a
/// preceding message with 'tool_calls'" is a CodeLeveler defect when it
/// happens. The registry is the one provider boundary; an orphan result is
/// refused there, typed as an internal protocol error, and never sent.
#[tokio::test]
async fn an_orphan_tool_result_is_refused_before_it_is_sent() {
    let server = MockServer::start_one(MockResponse::sse(&[
        r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"stop"}]}"#,
    ]))
    .await;
    let reg = registry(&server);
    let orphan = || {
        request_with(vec![
            leveler_model::Message::text(Role::User, "hi"),
            leveler_model::Message::text(Role::Assistant, "sure"),
            tool_result_message(&["call_orphan"]),
        ])
    };

    let Err(streamed) = reg.stream(orphan(), CancellationToken::new()).await else {
        panic!("stream must refuse an orphan tool result");
    };
    assert_refused_before_send(&streamed, "call_orphan");
    assert_eq!(streamed.provider.as_deref(), Some("mock"));

    let generated = reg
        .generate(orphan(), CancellationToken::new())
        .await
        .expect_err("generate must refuse an orphan tool result");
    assert_refused_before_send(&generated, "call_orphan");

    assert_eq!(server.request_count(), 0, "nothing reached the provider");
}

#[tokio::test]
async fn a_mismatched_tool_call_id_is_refused_before_it_is_sent() {
    let server = MockServer::start_one(MockResponse::sse(&[
        r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"stop"}]}"#,
    ]))
    .await;
    let reg = registry(&server);
    let request = request_with(vec![
        leveler_model::Message::text(Role::User, "hi"),
        tool_call_message(&["call_a"]),
        tool_result_message(&["call_b"]),
    ]);
    let Err(error) = reg.stream(request, CancellationToken::new()).await else {
        panic!("a result for a call nobody made must be refused");
    };
    assert_refused_before_send(&error, "call_b");
    assert_eq!(server.request_count(), 0);
}

/// A valid multi-call round reaches the Chat Completions wire as one
/// `assistant.tool_calls` followed by one `role: tool` per call, in order.
#[tokio::test]
async fn a_multi_call_round_reaches_the_wire_paired() {
    let server = MockServer::start_one(MockResponse::sse(&[
        r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
    ]))
    .await;
    let reg = registry(&server);
    let request = request_with(vec![
        leveler_model::Message::text(Role::User, "hi"),
        tool_call_message(&["call_a", "call_b"]),
        tool_result_message(&["call_a", "call_b"]),
        leveler_model::Message::text(Role::Assistant, "done"),
        leveler_model::Message::text(Role::User, "again"),
    ]);
    let stream = reg.stream(request, CancellationToken::new()).await.unwrap();
    let _ = collect(stream).await;

    let bodies = server.request_bodies().await;
    let body: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "tool", "tool", "assistant", "user"]
    );
    let call_ids: Vec<&str> = messages[1]["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(call_ids, ["call_a", "call_b"]);
    assert_eq!(messages[2]["tool_call_id"], "call_a");
    assert_eq!(messages[3]["tool_call_id"], "call_b");
}
