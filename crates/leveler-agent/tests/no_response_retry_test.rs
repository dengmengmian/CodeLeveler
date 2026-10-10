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

#[derive(Default)]
struct AttemptObserver {
    events: Vec<AgentEvent>,
    attempts: Vec<leveler_model::ModelAttempt>,
}

#[async_trait::async_trait]
impl leveler_agent_core::ModelRoundObserver for AttemptObserver {
    type Error = leveler_agent_core::AgentCoreError;

    fn on_event(&mut self, event: AgentEvent) {
        self.events.push(event);
    }

    async fn on_attempt(
        &mut self,
        attempt: leveler_model::ModelAttempt,
    ) -> Result<(), Self::Error> {
        self.attempts.push(attempt);
        Ok(())
    }
}

fn broken_body(frame: Option<&str>) -> MockResponse {
    MockResponse::TruncatedBody {
        body: frame
            .map(|frame| format!("data: {frame}\n\n"))
            .unwrap_or_default(),
        content_type: "text/event-stream".into(),
    }
}

async fn assert_physical_requests(server: &MockServer, expected: usize) -> Vec<serde_json::Value> {
    assert_eq!(
        server.request_count(),
        expected,
        "actual physical HTTP count"
    );
    let bodies = server.request_bodies().await;
    assert_eq!(bodies.len(), expected, "complete parsed request body count");
    bodies
        .iter()
        .map(|body| serde_json::from_str(body).unwrap())
        .collect()
}

#[tokio::test]
async fn body_read_zero_output_recovers_once_and_settles_both_attempts() {
    let server = MockServer::start(vec![broken_body(None), completed_sse()]).await;
    let reg = registry(&server);
    let mut observer = AttemptObserver::default();
    let mut req = request();
    req.deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
    let round = leveler_agent_core::run_model_round_observed(
        &reg,
        req,
        &CancellationToken::new(),
        &mut observer,
    )
    .await
    .expect("no-output HTTP body failure must use the bounded round owner");
    let requests = assert_physical_requests(&server, 2).await;
    assert_eq!(
        requests[0], requests[1],
        "one retry of the same admitted request"
    );
    assert_eq!(round.retry_count, 1);
    assert_eq!(observer.attempts.len(), 2);
    assert_eq!(observer.attempts[0].attempt, 1);
    assert_eq!(observer.attempts[1].attempt, 2);
    let failed = &observer.attempts[0];
    let error = failed.error.as_ref().unwrap();
    assert_eq!(error.kind, ModelErrorKind::Transport);
    assert_eq!(
        error.delivery_state,
        DeliveryState::StreamInterrupted {
            progress: Default::default()
        }
    );
    assert!(
        error.message.contains("body"),
        "raw diagnostic retained: {error:?}"
    );
    assert!(failed.usage.is_none());
    assert!(
        failed.cost_usd_micros.is_none(),
        "unknown usage is not free"
    );
    assert!(failed.estimated_tokens.is_some());
    assert!(observer.attempts[1].error.is_none());
    assert_eq!(observer.attempts[1].usage.unwrap().total(), 7);
    assert_eq!(
        observer
            .events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ModelRetrying { .. }))
            .count(),
        1
    );
    let text: String = observer
        .events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::AssistantDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "recovered", "no duplicate response output");
    assert_eq!(
        round.message.content,
        vec![ContentPart::Text {
            text: "recovered".into()
        }]
    );
}

#[derive(Clone, Copy)]
enum PartialOutput {
    Text,
    Reasoning,
    ToolArguments,
}

impl PartialOutput {
    fn frame(self) -> &'static str {
        match self {
            Self::Text => r#"{"choices":[{"delta":{"content":"partial answer"}}]}"#,
            Self::Reasoning => r#"{"choices":[{"delta":{"reasoning_content":"partial thought"}}]}"#,
            Self::ToolArguments => {
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_partial","function":{"name":"write_file","arguments":"{\"path\":"}}]}}]}"#
            }
        }
    }

    fn progress(self) -> leveler_model::StreamProgress {
        leveler_model::StreamProgress {
            text: matches!(self, Self::Text),
            reasoning: matches!(self, Self::Reasoning),
            tool_args: matches!(self, Self::ToolArguments),
        }
    }
}

async fn assert_partial_body_is_not_replayed(partial: PartialOutput) {
    let server = MockServer::start(vec![broken_body(Some(partial.frame())), completed_sse()]).await;
    let reg = registry(&server);
    let mut observer = AttemptObserver::default();
    let result = leveler_agent_core::run_model_round_observed(
        &reg,
        request(),
        &CancellationToken::new(),
        &mut observer,
    )
    .await;
    assert_physical_requests(&server, 1).await;
    let error = match result {
        Err(leveler_agent_core::AgentCoreError::Model(error)) => error,
        other => panic!("partial output must fail without fabricating completion: {other:?}"),
    };
    assert_eq!(error.kind, ModelErrorKind::Transport);
    assert_eq!(
        error.delivery_state,
        DeliveryState::StreamInterrupted {
            progress: partial.progress()
        }
    );
    assert!(!error.is_retryable_by_lifecycle());
    assert!(error.message.contains("body"));
    assert_eq!(observer.attempts.len(), 1);
    assert_eq!(observer.attempts[0].error.as_ref(), Some(&error));
    assert_eq!(
        observer.attempts[0].partial_text.as_deref(),
        matches!(partial, PartialOutput::Text).then_some("partial answer")
    );
    assert!(observer.attempts[0].usage.is_none());
    assert!(observer.attempts[0].cost_usd_micros.is_none());
    assert!(!observer.events.iter().any(|e| matches!(
        e,
        AgentEvent::ModelRetrying { .. } | AgentEvent::ToolCallStarted { .. }
    )));
    let text: String = observer
        .events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::AssistantDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    let reasoning: String = observer
        .events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ReasoningDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text,
        if matches!(partial, PartialOutput::Text) {
            "partial answer"
        } else {
            ""
        }
    );
    assert_eq!(
        reasoning,
        if matches!(partial, PartialOutput::Reasoning) {
            "partial thought"
        } else {
            ""
        }
    );
    assert_eq!(
        observer
            .events
            .iter()
            .filter(|e| matches!(e, AgentEvent::AssistantDelta(_)))
            .count(),
        usize::from(matches!(partial, PartialOutput::Text)),
        "the single fixture frame is delivered exactly once"
    );
    assert_eq!(
        observer
            .events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ReasoningDelta(_)))
            .count(),
        usize::from(matches!(partial, PartialOutput::Reasoning))
    );
}

#[tokio::test]
async fn body_read_partial_text_is_not_replayed() {
    assert_partial_body_is_not_replayed(PartialOutput::Text).await;
}
#[tokio::test]
async fn body_read_partial_reasoning_is_not_replayed() {
    assert_partial_body_is_not_replayed(PartialOutput::Reasoning).await;
}
#[tokio::test]
async fn body_read_partial_tool_arguments_are_not_replayed() {
    assert_partial_body_is_not_replayed(PartialOutput::ToolArguments).await;
}

async fn executor_with_responses(
    responses: Vec<MockResponse>,
) -> (
    MockServer,
    Result<leveler_agent::AgentOutcome, leveler_agent::AgentError>,
    Vec<leveler_agent::AgentEvent>,
) {
    let server = MockServer::start(responses).await;
    let dir = tempfile::tempdir().unwrap();
    let workspace = leveler_execution::Workspace::new(dir.path()).unwrap();
    let context =
        leveler_tools::ToolContext::new(workspace, leveler_execution::PermissionProfile::Assisted);
    let executor = leveler_agent::Executor::new(
        std::sync::Arc::new(registry(&server)),
        std::sync::Arc::new(leveler_tools::ToolRegistry::new()),
        context,
        ModelRef::new("mock", "m"),
        4,
    );
    let mut events = Vec::new();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        executor.run(
            "Answer the question without tools.",
            &mut |e| events.push(e),
            &mut leveler_agent::NoopSink,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("bounded real HTTP executor test");
    (server, result, events)
}

#[tokio::test]
async fn body_read_partial_outputs_do_not_inject_tool_json_repair() {
    for partial in [
        PartialOutput::Text,
        PartialOutput::Reasoning,
        PartialOutput::ToolArguments,
    ] {
        let (server, result, events) =
            executor_with_responses(vec![broken_body(Some(partial.frame())), completed_sse()])
                .await;
        assert_physical_requests(&server, 1).await;
        let error = match result {
            Err(leveler_agent::AgentError::Model(error)) => error,
            other => panic!("no repaired/fake success for body failure: {other:?}"),
        };
        assert_eq!(error.kind, ModelErrorKind::Transport);
        assert_eq!(
            error.delivery_state,
            DeliveryState::StreamInterrupted {
                progress: partial.progress()
            }
        );
        assert!(!events.iter().any(|e| matches!(e, leveler_agent::AgentEvent::RuntimeInjection { kind, .. } if kind == "protocol_repair")));
        assert!(!events.iter().any(|e| matches!(
            e,
            leveler_agent::AgentEvent::Finished(_) | leveler_agent::AgentEvent::ToolCall { .. }
        )));
    }
}

#[tokio::test]
async fn true_invalid_tool_json_retains_executor_protocol_repair() {
    let invalid = MockResponse::sse(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_bad","function":{"name":"write_file","arguments":"{invalid"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    let (server, result, events) = executor_with_responses(vec![invalid, completed_sse()]).await;
    let bodies = assert_physical_requests(&server, 2).await;
    let outcome = result.expect("genuine malformed tool JSON is repairable");
    assert_eq!(outcome.stop_reason, leveler_agent::StopReason::Answered);
    assert_eq!(outcome.model_steps, 2);
    assert!(outcome.budget_exhaustion.is_none());
    assert_eq!(outcome.final_text, "recovered");
    assert!(bodies[1]["messages"].as_array().unwrap().iter().any(|m| {
        m["content"]
            .as_str()
            .is_some_and(|t| t.contains("工具调用参数不是合法 JSON"))
    }));
    assert!(events.iter().any(|e| matches!(e, leveler_agent::AgentEvent::RuntimeInjection { kind, .. } if kind == "protocol_repair")));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, leveler_agent::AgentEvent::ToolCall { .. }))
    );
}
