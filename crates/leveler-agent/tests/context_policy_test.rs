//! The context lifecycle as the agent loop actually runs it: which reasoning a
//! provider request carries, and what a fold releases.
//!
//! These drive the real `Executor` with a scripted runtime, so the projection,
//! the pressure figure and the fold are the production ones. No provider is
//! contacted. The estimator's own numbers are the only figures used — no
//! synthetic "provider usage" is injected.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use leveler_agent::coding::policy::ResolvedContextPolicy;
use leveler_agent::{AgentEvent, Executor, NoopSink};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, MissingReasoningReplay, ModelError, ModelEvent,
    ModelEventStream, ModelLimits, ModelProfile, ModelRef, ModelRequest, ModelResponse,
    ModelRuntime, ProjectedReasoning, ReasoningReplayContract, ReasoningReplayScope, Role,
    TokenUsage, ToolCall, ToolChoice,
};
use leveler_tools::ToolContext;
use tokio_util::sync::CancellationToken;

struct Script {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl Script {
    fn new(responses: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ModelRuntime for Script {
    async fn generate(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        Ok(answer("briefing: the earlier rounds read src/lib.rs"))
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let is_summary = request.tool_choice == ToolChoice::None;
        self.requests.lock().unwrap().push(request);
        let response = if is_summary {
            answer("briefing: the earlier rounds read src/lib.rs")
        } else {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted")
        };
        let mut events = vec![Ok(ModelEvent::MessageStarted {
            request_id: response.request_id,
        })];
        for part in &response.message.content {
            match part {
                ContentPart::Reasoning { text } => events.push(Ok(ModelEvent::ReasoningDelta {
                    delta: text.clone(),
                })),
                ContentPart::Text { text } => events.push(Ok(ModelEvent::TextDelta {
                    delta: text.clone(),
                })),
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }))
                }
                other => panic!("unexpected scripted content: {other:?}"),
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id": model.to_string(),
            "provider": model.provider,
            "model_id": model.model,
            "protocol": "openai_chat",
            "capabilities": {
                "streaming": true, "tool_calling": true, "parallel_tool_calls": false,
                "structured_output": false, "reasoning": true, "vision": false
            },
            "limits": {
                "context_window": 32768, "reliable_context": 3000, "max_output_tokens": 1024,
                "max_tool_schema_bytes": 32768, "max_parallel_tool_calls": 1
            },
            "reasoning": {"style": "thinking_flag", "supported_efforts": ["low", "high", "max"], "default_effort": "max"},
            // The route fact: this endpoint validates the reasoning of
            // tool-bearing requests and wants the key present either way.
            "compatibility": {
                "reasoning_replay_scope": "when_tools_present",
                "reasoning_content_key_required": true
            }
        }))
        .unwrap())
    }
}

fn answer(text: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, text),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

/// One turn that thought and then read a file.
fn thought_read(index: usize, thinking: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::from_parts(
            Role::Assistant,
            vec![
                ContentPart::Reasoning {
                    text: thinking.to_string(),
                },
                ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new(format!("read-{index}")),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path": "src/lib.rs"}),
                    },
                },
            ],
            None,
        ),
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        format!("pub fn value() -> i32 {{ 1 }}\n{}", "// note\n".repeat(120)),
    )
    .unwrap();
    dir
}

/// The route fact DeepSeek-shaped endpoints declare: reasoning is replayed on
/// tool-bearing requests and the key must be present either way.
fn replaying_route() -> ReasoningReplayContract {
    ReasoningReplayContract::raw_field(
        ReasoningReplayScope::WhenToolsPresent,
        MissingReasoningReplay::EmptyString,
    )
}

/// The context policy the production factory resolves from this model's
/// declared limits: a 3 000-token quality boundary with a 1 024-token
/// reservation, which is the fold threshold and a retention tail of half of it.
fn resolved_policy() -> ResolvedContextPolicy {
    let limits: ModelLimits = serde_json::from_value(serde_json::json!({
        "context_window": 32_768, "reliable_context": 3_000, "max_output_tokens": 1_024,
        "max_tool_schema_bytes": 32_768, "max_parallel_tool_calls": 1
    }))
    .unwrap();
    ResolvedContextPolicy::resolve(&limits, 1_024, 0)
}

/// The bare executor carries the policies its composition root injects (the
/// coding factory resolves them from the model profile). `fold` selects the
/// resolved context policy above; otherwise folding is disabled.
fn executor(
    root: &std::path::Path,
    runtime: Arc<dyn ModelRuntime>,
    contract: ReasoningReplayContract,
    fold: bool,
) -> Executor {
    Executor::new(
        runtime,
        Arc::new(leveler_tools::default_registry()),
        ToolContext::new(Workspace::new(root).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        0,
    )
    .with_delegation(false)
    .with_reasoning_replay(contract)
    .with_context_policy(if fold {
        resolved_policy()
    } else {
        ResolvedContextPolicy::default()
    })
}

/// Every reasoning string the provider would receive on this request.
fn carried(request: &ModelRequest) -> Vec<String> {
    request
        .projection
        .as_ref()
        .expect("the loop projects every request")
        .messages()
        .iter()
        .filter_map(|message| match &message.reasoning {
            ProjectedReasoning::Captured(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// R2 end to end: the reasoning of the tool exchange reaches the next request,
/// through the projection the encoder reads.
#[tokio::test]
async fn a_tool_turns_reasoning_is_replayed_to_the_provider() {
    let dir = workspace();
    let script = Script::new(vec![
        thought_read(0, "I should read the source first."),
        answer("done"),
    ]);
    executor(dir.path(), script.clone(), replaying_route(), false)
        .run(
            "what does src/lib.rs define?",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("turn");

    let requests = script.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "one tool round then the answer");
    assert!(
        carried(&requests[1]).contains(&"I should read the source first.".to_string()),
        "the tool exchange's reasoning is replayed: {:?}",
        carried(&requests[1])
    );
    let summary = requests[1].projection.as_ref().unwrap().summary();
    assert_eq!(summary.carried_turns, 1);
    assert_eq!(
        summary.captured_turns, 1,
        "the transcript captured exactly one reasoning turn"
    );
    // Nothing folded on this tiny transcript, so the accounting reports a
    // projection rather than a compaction.
    assert_eq!(
        requests[1]
            .projection
            .as_ref()
            .unwrap()
            .summary()
            .captured_turns,
        1
    );
}

/// R1/R4 end to end: once a fold runs, the rounds it elided stop being part of
/// the request — their reasoning included — and the replayed reasoning does not
/// keep growing.
#[tokio::test]
async fn a_fold_releases_the_elided_rounds_reasoning() {
    let dir = workspace();
    let mut responses = Vec::new();
    for index in 0..6 {
        responses.push(thought_read(
            index,
            &format!("round-{index} thinking {}", "t".repeat(600)),
        ));
    }
    responses.push(answer("done"));
    let script = Script::new(responses);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let events = events.clone();
        move |event: AgentEvent| events.lock().unwrap().push(event)
    };
    let mut observer = sink;
    executor(dir.path(), script.clone(), replaying_route(), true)
        .run(
            "keep reading until you understand",
            &mut observer,
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("turn");

    let requests = script.requests.lock().unwrap().clone();
    let rounds: Vec<&ModelRequest> = requests
        .iter()
        .filter(|request| request.tool_choice != ToolChoice::None)
        .collect();
    assert!(rounds.len() >= 4, "the loop ran several rounds");

    let folds: Vec<(usize, usize)> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Compacted { from, to } => Some((*from, *to)),
            _ => None,
        })
        .collect();
    assert!(
        !folds.is_empty(),
        "this fixture crosses the pressure threshold and folds"
    );
    assert!(
        folds.iter().all(|(from, to)| to < from),
        "a reported fold really released messages: {folds:?}"
    );

    let last = carried(rounds.last().expect("a last round"));
    assert!(
        !last.iter().any(|text| text.starts_with("round-0 thinking")),
        "the first round's reasoning was released by the fold: {last:?}"
    );
    assert!(
        last.len() <= 3,
        "replayed reasoning stays bounded by the retention budget instead of \
         growing every round: {} turns carried",
        last.len()
    );
}

/// R3: on a route with no reasoning channel the same transcript produces
/// requests that carry none of it — and the transcript still records it for a
/// route that does.
#[tokio::test]
async fn a_route_without_a_channel_carries_no_reasoning_and_says_so() {
    let dir = workspace();
    let script = Script::new(vec![
        thought_read(0, "private thinking that this route cannot carry."),
        answer("done"),
    ]);
    let runtime: Arc<dyn ModelRuntime> = script.clone();
    executor(dir.path(), runtime, ReasoningReplayContract::NONE, false)
        .run(
            "what does src/lib.rs define?",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("turn");

    let requests = script.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert!(
            carried(request).is_empty(),
            "a route with no channel carries nothing: {:?}",
            carried(request)
        );
        let summary = request.projection.as_ref().unwrap().summary();
        assert_eq!(
            summary.protocol_protected_turns, 0,
            "no turn is reported as protocol-protected when there is no channel"
        );
    }
    let captured = requests
        .last()
        .unwrap()
        .projection
        .as_ref()
        .unwrap()
        .summary()
        .captured_turns;
    assert_eq!(
        captured, 1,
        "the transcript still captured the reasoning for a later route"
    );
}
