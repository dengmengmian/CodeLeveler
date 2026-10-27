//! A single large tool result must not end a task that has room for it.
//!
//! The T8 failure shape: a 32K-window model, ~17.6K tokens of fixed prefix,
//! and one `grep` result. The result was capped at the model's STATIC 48 KiB
//! ceiling (~19K tokens), which is larger than the room left to the hard
//! capacity. Nothing was foldable — the transcript had no middle — so the
//! summary had no request, the mechanical fold was a no-op, and the turn died
//! with `ContextManagementFailure`.
//!
//! The contract this test locks:
//!
//! * a tool result is bounded by the room the NEXT request has, not by a
//!   constant that ignores the window;
//! * the full result stays retrievable through the saved reference;
//! * the turn continues and answers instead of aborting.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::coding::policy::{ContextRetentionPolicy, ResolvedContextPolicy};
use leveler_agent::{AgentEvent, Executor, NoopSink, StepLimits, StopReason};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{ArtifactStore, PermissionProfile, RiskLevel, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEvent, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_tools::registry::model_surface;
use leveler_tools::{Capabilities, CapabilityPacks, Tool, ToolContext, ToolError, ToolOutput};

const NEEDLE: &str = "MIDDLE_NEEDLE";

struct Firehose;

#[async_trait]
impl Tool for Firehose {
    fn name(&self) -> &str {
        "firehose"
    }

    fn description(&self) -> &str {
        "test-only result firehose"
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(format!(
            "HEAD-LINE\n{}\n{NEEDLE}\n{}\nTAIL-LINE",
            "x".repeat(100_000),
            "y".repeat(100_000)
        )))
    }
}

struct ScriptRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptRuntime {
    fn new(responses: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn main_requests(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.tool_choice != leveler_model::ToolChoice::None)
            .cloned()
            .collect()
    }

    fn events(response: ModelResponse) -> ModelEventStream {
        let mut events = vec![Ok(ModelEvent::MessageStarted {
            request_id: response.request_id,
        })];
        for part in &response.message.content {
            match part {
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
        Box::pin(futures::stream::iter(events))
    }
}

#[async_trait]
impl ModelRuntime for ScriptRuntime {
    async fn generate(
        &self,
        _: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unreachable!("the executor uses streaming")
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.requests.lock().unwrap().push(request);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        Ok(Self::events(response))
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id": model.to_string(),
            "provider": model.provider,
            "model_id": model.model,
            "protocol": "openai_chat",
            "capabilities": {
                "streaming": true, "tool_calling": true, "parallel_tool_calls": false,
                "structured_output": false, "reasoning": false, "vision": false
            },
            "limits": {
                "context_window": 131072, "reliable_context": 65536, "max_output_tokens": 8192,
                "max_tool_schema_bytes": 32768, "max_parallel_tool_calls": 1
            },
            "reasoning": {"style": "none"}
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

fn firehose_call() -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::from_parts(
            Role::Assistant,
            vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new("fire-1"),
                    name: "firehose".into(),
                    arguments: serde_json::json!({}),
                },
            }],
            None,
        ),
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

fn registry(store: Arc<ArtifactStore>) -> leveler_tools::ToolRegistry {
    let capabilities = Capabilities::in_process(Arc::new(leveler_core::environment().clone()))
        .with_artifact_store(store);
    let mut registry = model_surface(CapabilityPacks::NONE, &capabilities);
    registry.register(Arc::new(Firehose));
    registry
}

fn executor(
    root: &std::path::Path,
    runtime: Arc<ScriptRuntime>,
    registry: leveler_tools::ToolRegistry,
    policy: ResolvedContextPolicy,
) -> Executor {
    Executor::new(
        runtime,
        Arc::new(registry),
        ToolContext::new(Workspace::new(root).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        0,
    )
    .with_delegation(false)
    .with_context_policy(policy)
    .with_step_limits(StepLimits::default())
}

fn soft_policy() -> ResolvedContextPolicy {
    ResolvedContextPolicy {
        context_window: 1_000_000,
        quality_boundary: 500,
        output_reservation: 1_000,
        headroom: 0,
        pressure_threshold: 500,
        soft_percent: 100,
        retention: ContextRetentionPolicy {
            keep_recent_messages: 1_000,
            keep_recent_tokens: 0,
        },
    }
}

fn tool_result_content(request: &ModelRequest, call_id: &str) -> String {
    for message in &request.messages {
        for part in &message.content {
            if let ContentPart::ToolResult { result } = part
                && result.call_id.as_str() == call_id
            {
                return result.content.clone();
            }
        }
    }
    panic!("no result for {call_id}");
}

#[tokio::test]
async fn an_oversized_result_is_bounded_by_remaining_capacity_and_stays_retrievable() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(ArtifactStore::new(dir.path().join("artifacts")));

    // Calibrate the fixed prefix with the SAME registry. The probe answers
    // without calling a tool, so its first request is exactly the system
    // prompt + task + tool schemas the real run starts with.
    let probe_runtime = ScriptRuntime::new(vec![answer("probe")]);
    executor(
        dir.path(),
        probe_runtime.clone(),
        registry(store.clone()),
        soft_policy(),
    )
    .run(
        "find the window boundary",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("probe");
    let baseline = probe_runtime.main_requests()[0]
        .projection
        .as_ref()
        .unwrap()
        .estimated_tokens();

    // Hard capacity sits 4K tokens above the fixed prefix. The static 48 KiB
    // result ceiling (~19K tokens) is far larger than that room; the per-call
    // budget must follow the room instead.
    let capacity = baseline + 4_000;
    let policy = ResolvedContextPolicy {
        context_window: (capacity + 1_000) as u32,
        quality_boundary: 0,
        output_reservation: 1_000,
        headroom: 0,
        pressure_threshold: capacity as u32,
        soft_percent: 100,
        retention: ContextRetentionPolicy {
            // Deliberately no foldable middle: this is the T8 shape.
            keep_recent_messages: 1_000,
            keep_recent_tokens: 0,
        },
    };

    let runtime = ScriptRuntime::new(vec![firehose_call(), answer("done")]);
    let mut events = Vec::new();
    let outcome = executor(dir.path(), runtime.clone(), registry(store.clone()), policy)
        .run(
            "find the window boundary",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("a result that fits the remaining room must not abort the turn");

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    let requests = runtime.main_requests();
    assert_eq!(requests.len(), 2, "the tool round and the answer round");
    let carried = requests[1]
        .projection
        .as_ref()
        .expect("the second request has a projection")
        .estimated_tokens();
    assert!(
        carried <= capacity,
        "the next request must fit the hard capacity: projected {carried}, capacity {capacity}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Compacted { .. })),
        "no fold is needed when the result is bounded at dispatch"
    );

    let result = tool_result_content(&requests[1], "fire-1");
    assert!(
        result.starts_with(leveler_model::tool_output::TOOL_OUTPUT_SAVED_PREFIX),
        "a bounded result must name where the full output is: {}",
        &result[..result.len().min(300)]
    );
    let reference = result.lines().next().unwrap();
    let path = reference
        .strip_prefix(leveler_model::tool_output::TOOL_OUTPUT_SAVED_PREFIX)
        .unwrap()
        .trim()
        .split(" | sha256:")
        .next()
        .expect("the reference names a path");
    let stored = std::fs::read_to_string(path).expect("the full output is on disk");
    assert!(
        stored.contains(NEEDLE),
        "the saved result is the whole output, not the preview"
    );
}
