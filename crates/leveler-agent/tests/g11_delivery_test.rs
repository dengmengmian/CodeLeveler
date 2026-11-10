//! G10/G11: which model text is the final answer.
//!
//! The protocol layer under test is one fact the model states about its own
//! text: `update_goal(deliver_answer = true)` means the visible text of that
//! same turn is the answer the user is owed. Without that declaration the
//! drive must not promote prose it merely found lying in the conversation —
//! the bounded post-settlement delivery request is the compatibility path.
//!
//! Every test drives the real executor loop, the real injected goal tools and
//! the real settlement path with a scripted provider. Only the provider's
//! responses are scripted; nothing here inspects the model's words.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, Executor, NoopSink, StepLimits, StopReason};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_tools::{ToolContext, ToolRegistry};

const TRANSITION: &str =
    "The goal checker expects the requirements phrased exactly as stated. I'll resubmit.";
const ANSWER: &str = "Implemented --doc-count and verified it: go test ./... exited 0 and the CLI counted 3 documents.";

fn default_registry() -> ToolRegistry {
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    registry
}

struct MockRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
    stream_errors: Mutex<VecDeque<Option<ModelError>>>,
}

impl MockRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
            stream_errors: Mutex::new(VecDeque::new()),
        }
    }

    fn with_stream_error(responses: Vec<ModelResponse>, error: ModelError) -> Self {
        let mut errors: Vec<Option<ModelError>> = responses.iter().map(|_| None).collect();
        if let Some(last) = errors.last_mut() {
            *last = Some(error);
        }
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
            stream_errors: Mutex::new(VecDeque::from(errors)),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn mock_profile(model: &ModelRef) -> ModelProfile {
    serde_json::from_value(serde_json::json!({
        "id": model.to_string(),
        "provider": model.provider,
        "model_id": model.model,
        "protocol": "openai_chat",
        "capabilities": {
            "streaming": true, "tool_calling": true, "parallel_tool_calls": false,
            "structured_output": false, "reasoning": false, "vision": false
        },
        "limits": {
            "context_window": 131072, "reliable_context": 65536,
            "max_output_tokens": 8192, "max_tool_schema_bytes": 32768,
            "max_parallel_tool_calls": 1
        },
        "reasoning": { "style": "none" }
    }))
    .expect("valid mock model profile")
}

#[async_trait]
impl ModelRuntime for MockRuntime {
    async fn generate(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        self.requests.lock().unwrap().push(request);
        let response = self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })?;
        let mut events: Vec<Result<ModelEvent, ModelError>> = Vec::new();
        events.push(Ok(ModelEvent::MessageStarted {
            request_id: response.request_id.clone(),
        }));
        for part in &response.message.content {
            match part {
                ContentPart::Text { text } => events.push(Ok(ModelEvent::TextDelta {
                    delta: text.clone(),
                })),
                ContentPart::Reasoning { text } => events.push(Ok(ModelEvent::ReasoningDelta {
                    delta: text.clone(),
                })),
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }))
                }
                _ => {}
            }
        }
        if let Some(Some(error)) = self.stream_errors.lock().unwrap().pop_front() {
            events.push(Ok(ModelEvent::Error { error }));
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(mock_profile(model))
    }

    fn route_identity(&self, model: &ModelRef) -> Option<leveler_model::RouteIdentity> {
        Some(leveler_model::RouteIdentity {
            provider: model.provider.clone(),
            model: model.model.clone(),
            api: Some(leveler_model::ProtocolKind::OpenAiChat),
            deployment: None,
        })
    }
}

fn assistant_text(text: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, text),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

fn assistant_tool_call(id: &str, name: &str, args: serde_json::Value) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: name.to_string(),
                    arguments: args,
                },
            }],
            route: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

/// The shape every G10/G11 trace shares: visible prose published together with
/// a bookkeeping call in one provider response.
fn assistant_says_and_calls(
    id: &str,
    text: &str,
    name: &str,
    args: serde_json::Value,
) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![
                ContentPart::Text {
                    text: text.to_string(),
                },
                ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new(id),
                        name: name.to_string(),
                        arguments: args,
                    },
                },
            ],
            route: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

/// A response that publishes several tool calls and no prose.
fn assistant_calls(calls: &[(&str, &str, serde_json::Value)]) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: calls
                .iter()
                .map(|(id, name, arguments)| ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new(*id),
                        name: (*name).to_string(),
                        arguments: arguments.clone(),
                    },
                })
                .collect(),
            route: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

fn goal_complete(id: &str, deliver: Option<bool>) -> ModelResponse {
    let mut args = serde_json::json!({
        "status": "complete",
        "summary": "Finished the requested work.",
    });
    if let Some(deliver) = deliver {
        args["deliver_answer"] = serde_json::Value::Bool(deliver);
    }
    assistant_tool_call(id, "update_goal", args)
}

struct Run {
    outcome: leveler_agent::AgentOutcome,
    events: Vec<AgentEvent>,
}

impl Run {
    fn finished_texts(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Finished(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn final_text(&self) -> String {
        self.outcome.final_text.clone()
    }
}

async fn run_goal(script: Vec<ModelResponse>) -> Run {
    run_goal_with(script, |executor| executor).await
}

async fn run_goal_for(objective: &str, script: Vec<ModelResponse>) -> Run {
    run_goal_for_with(objective, script, |executor| executor).await
}

async fn run_goal_with(
    script: Vec<ModelResponse>,
    configure: impl FnOnce(Executor) -> Executor,
) -> Run {
    run_goal_for_with("implement the counting feature", script, configure).await
}

async fn run_goal_for_with(
    objective: &str,
    script: Vec<ModelResponse>,
    configure: impl FnOnce(Executor) -> Executor,
) -> Run {
    // A unique directory per run. PID + nanos collides when two tests start
    // in the same clock tick; one test's cleanup then deletes the other's
    // workspace and the command cwd resolves outside it.
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::new(script));
    let executor = configure(
        Executor::new(
            runtime,
            Arc::new(default_registry()),
            ToolContext::new(
                Workspace::new(dir.path()).unwrap(),
                PermissionProfile::Assisted,
            ),
            ModelRef::new("mock", "m"),
            20,
        )
        .with_goal_mode(true),
    );
    let mut events = Vec::new();
    let outcome = executor
        .run(
            objective,
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("the scripted goal run must complete");
    Run { outcome, events }
}

/// The goal is settled without a delivery declaration, so the transition text
/// that happened to share the round is prose, not the answer. The drive must
/// take its one post-settlement request and publish the answer that follows.
#[tokio::test]
async fn same_round_transition_text_is_not_delivered_as_the_answer() {
    let run = run_goal(vec![
        assistant_says_and_calls(
            "g1",
            TRANSITION,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Goal complete.",
            }),
        ),
        assistant_text(ANSWER),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(
        run.final_text(),
        ANSWER,
        "the answer after the settlement, not the transition: {:?}",
        run.finished_texts()
    );
    assert_eq!(
        run.finished_texts().len(),
        1,
        "one terminal delivery, got {:?}",
        run.finished_texts()
    );
}

/// The original G10 shape: the transition text and an `update_plan` sit in an
/// earlier round; the settling round is textless. The earlier text must not be
/// promoted merely because bookkeeping happened in between.
#[tokio::test]
async fn cross_round_transition_text_is_not_delivered_as_the_answer() {
    let run = run_goal(vec![
        assistant_says_and_calls(
            "p1",
            TRANSITION,
            "update_plan",
            serde_json::json!({"plan": [{"step": "implement", "status": "completed"}]}),
        ),
        goal_complete("g1", None),
        assistant_text(ANSWER),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(
        run.final_text(),
        ANSWER,
        "the settled delivery, not the earlier transition: {:?}",
        run.finished_texts()
    );
    assert_eq!(run.finished_texts(), vec![ANSWER.to_string()]);
}

/// The fast path that the declaration exists for: an answer and the successful
/// settlement in one response. It must finish with zero extra model requests.
#[tokio::test]
async fn a_declared_same_round_answer_finishes_without_another_request() {
    let run = run_goal(vec![assistant_says_and_calls(
        "g1",
        ANSWER,
        "update_goal",
        serde_json::json!({
            "status": "complete",
            "summary": "Goal complete.",
            "deliver_answer": true,
        }),
    )])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(run.final_text(), ANSWER);
    assert_eq!(run.outcome.model_steps, 1, "no extra delivery request");
    assert_eq!(run.finished_texts(), vec![ANSWER.to_string()]);
}

/// Case B: the substantive answer is in an earlier bookkeeping round and the
/// settling round only declares "that text is the answer". The drive delivers
/// it as written — no regenerated answer, no extra request.
#[tokio::test]
async fn a_declared_earlier_answer_is_delivered_without_another_request() {
    let run = run_goal(vec![
        assistant_says_and_calls(
            "p1",
            ANSWER,
            "update_plan",
            serde_json::json!({"plan": [{"step": "implement", "status": "completed"}]}),
        ),
        goal_complete("g1", Some(true)),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(
        run.final_text(),
        ANSWER,
        "the earlier declared answer is delivered verbatim"
    );
    assert_eq!(run.outcome.model_steps, 2, "no delivery round was added");
}

/// The compatibility path for a model that does not declare: one bounded
/// request shows it the delivery notice, and the answer it then writes is the
/// only thing published.
#[tokio::test]
async fn an_undeclared_settlement_gets_one_bounded_delivery_request() {
    let run = run_goal(vec![
        assistant_says_and_calls(
            "g1",
            TRANSITION,
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "Goal complete."}),
        ),
        assistant_text(ANSWER),
    ])
    .await;

    assert_eq!(run.outcome.model_steps, 2);
    assert_eq!(run.final_text(), ANSWER);
}

/// A declaration with no visible text is not an answer. The drive asks once
/// for the delivery; the text that follows is published.
#[tokio::test]
async fn a_declared_answer_without_visible_text_gets_one_delivery_request() {
    let run = run_goal(vec![
        goal_complete("g1", Some(true)),
        assistant_text(ANSWER),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(run.outcome.model_steps, 2);
    assert_eq!(run.final_text(), ANSWER);
    assert_eq!(
        run.finished_texts(),
        vec![ANSWER.to_string()],
        "only the follow-up answer is delivered"
    );
}

/// The model declares delivery but never writes text, even on the bounded
/// follow-up. The turn ends honestly: no empty or stale text becomes the
/// answer, and the terminal records that the person was not answered.
#[tokio::test]
async fn a_declared_answer_that_stays_textless_ends_with_an_honest_missing_delivery() {
    let run = run_goal(vec![
        goal_complete("g1", Some(true)),
        goal_complete("g2", Some(true)),
        goal_complete("g3", Some(true)),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(
        run.outcome.model_steps, 3,
        "the settlement plus two bounded wrap-up rounds"
    );
    assert!(
        run.outcome
            .completion_warnings
            .iter()
            .any(|warning| warning.contains("no final answer")),
        "the terminal must record the missing delivery: {:?}",
        run.outcome.completion_warnings
    );
    assert_eq!(
        run.final_text(),
        "Finished the requested work.",
        "the goal summary is the only honest fallback"
    );
}

/// The delivery request must actually tell the model that the person has not
/// been answered yet. Otherwise the compatibility path is a request without a
/// fact.
#[tokio::test]
async fn the_delivery_request_states_that_the_answer_is_still_owed() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_says_and_calls(
            "g1",
            TRANSITION,
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "Goal complete."}),
        ),
        assistant_text(ANSWER),
    ]));
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(default_registry()),
        ToolContext::new(
            Workspace::new(dir.path()).unwrap(),
            PermissionProfile::Assisted,
        ),
        ModelRef::new("mock", "m"),
        20,
    )
    .with_goal_mode(true);
    executor
        .run(
            "implement the counting feature",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("run completes");
    let requests = runtime.requests();
    assert_eq!(requests.len(), 2);
    let seen = requests[1]
        .messages
        .iter()
        .map(|message| message.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        seen.contains("accepted and is recorded for this goal")
            && seen.contains("this round is the delivery"),
        "the model must be told the answer is still owed; saw:\n{seen}"
    );
}

/// A model that keeps calling `update_goal` after an undeclared settlement gets
/// the bounded wrap-up, then the turn ends with the summary and an explicit
/// warning. It must never loop, and it must never publish the transition.
#[tokio::test]
async fn repeated_bookkeeping_ends_with_an_honest_missing_delivery() {
    let run = run_goal(vec![
        assistant_says_and_calls(
            "g1",
            TRANSITION,
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "Goal complete."}),
        ),
        goal_complete("g2", None),
        goal_complete("g3", None),
        goal_complete("g4", None),
        goal_complete("g5", None),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_ne!(
        run.final_text(),
        TRANSITION,
        "unconfirmed prose must never be published as the answer"
    );
    assert!(
        run.outcome
            .completion_warnings
            .iter()
            .any(|warning| warning.contains("no final answer")),
        "the terminal must record that no answer was delivered: {:?}",
        run.outcome.completion_warnings
    );
    assert!(
        run.outcome.model_steps <= 4,
        "the wrap-up is bounded, got {} steps",
        run.outcome.model_steps
    );
}

/// A tool call after the settlement is wrap-up: its result has not been seen by
/// the model, so the turn cannot end on it, and the delivery duty survives.
#[tokio::test]
async fn post_settlement_work_keeps_the_delivery_duty() {
    let run = run_goal(vec![
        assistant_says_and_calls(
            "g1",
            TRANSITION,
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "Goal complete."}),
        ),
        assistant_tool_call("r1", "list_files", serde_json::json!({"path": "."})),
        assistant_text(ANSWER),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(
        run.final_text(),
        ANSWER,
        "the answer after wrap-up, not the transition"
    );
    assert_eq!(run.finished_texts(), vec![ANSWER.to_string()]);
}

/// A refused completion is not a settlement: the text that claimed success was
/// never accepted, and it must not be delivered later as if it had been.
#[tokio::test]
async fn a_refused_verification_claim_is_not_delivered() {
    let run = run_goal(vec![
        assistant_says_and_calls(
            "g1",
            "All tests pass.",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Goal complete.",
                "verification": {"status": "passed", "detail": "go test ./..."},
                "deliver_answer": true,
            }),
        ),
        assistant_says_and_calls(
            "g2",
            "Blocked: the module cache is not writable, so go test cannot run.",
            "update_goal",
            serde_json::json!({
                "status": "blocked",
                "summary": "Module cache is read-only.",
            }),
        ),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Blocked);
    assert_eq!(
        run.final_text(),
        "Blocked: the module cache is not writable, so go test cannot run."
    );
    assert!(
        !run.finished_texts()
            .iter()
            .any(|text| text == "All tests pass."),
        "a refused claim must not become the delivery: {:?}",
        run.finished_texts()
    );
}

fn deliveries_of(run: &Run) -> Vec<&str> {
    run.events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::DeliveryCommitted { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn command_calls(run: &Run) -> usize {
    run.events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AgentEvent::ToolCall { name, .. } if name == "run_command"
            )
        })
        .count()
}

/// A full command plus a trailing result note is that execution. The note
/// must not force another run of the same check.
#[tokio::test]
async fn a_trailing_result_note_accepts_without_another_command() {
    let answer = "Git was available. The version check passed.";
    let run = run_goal(vec![
        assistant_tool_call(
            "probe",
            "run_command",
            serde_json::json!({"program": "git", "args": ["--version"]}),
        ),
        assistant_says_and_calls(
            "g1",
            answer,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Git version verified.",
                "verification": {"status": "passed", "detail": "git --version → available"},
                "deliver_answer": true,
            }),
        ),
        assistant_text("this later progress must not be requested"),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert!(
        !run.events.iter().any(|event| matches!(
            event,
            AgentEvent::GoalIntercepted { kind, .. } if kind == "verification_refused"
        )),
        "a result note is not a different command: {:?}",
        run.events
    );
    assert_eq!(command_calls(&run), 1);
    assert_eq!(deliveries_of(&run), vec![answer]);
    assert_eq!(run.final_text(), answer);
    assert_eq!(
        run.outcome.model_steps, 2,
        "acceptance does not add a round"
    );
}

/// The answer written with the refused citation stays the delivery when a
/// later round only corrects the citation.
#[tokio::test]
async fn a_citation_repair_keeps_the_answer_from_the_refused_attempt() {
    let answer = "Git was available. The version check passed. Backup kept. Service is up.";
    let repair = "我去掉附加说明后重新提交";
    let run = run_goal(vec![
        assistant_tool_call(
            "probe",
            "run_command",
            serde_json::json!({"program": "git", "args": ["--version"]}),
        ),
        assistant_says_and_calls(
            "g1",
            answer,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Git version verified.",
                "verification": {"status": "passed", "detail": "git status"},
                "deliver_answer": true,
            }),
        ),
        assistant_says_and_calls(
            "g2",
            repair,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Git version verified.",
                "verification": {"status": "passed", "detail": "call_id:probe"},
                "deliver_answer": true,
            }),
        ),
        assistant_text("this later progress must not replace the answer"),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(
        run.events
            .iter()
            .filter(|event| matches!(
                event,
                AgentEvent::GoalIntercepted { kind, .. } if kind == "verification_refused"
            ))
            .count(),
        1
    );
    assert_eq!(
        command_calls(&run),
        1,
        "the cited execution is not repeated"
    );
    assert_eq!(deliveries_of(&run), vec![answer]);
    assert_eq!(run.finished_texts(), vec![answer.to_string()]);
    assert_ne!(run.final_text(), repair);
    assert_eq!(
        run.outcome.model_steps, 3,
        "the repair settles; it does not ask again"
    );
}

/// A second bad citation does not replace the answer captured on the first.
#[tokio::test]
async fn repeated_citation_refusals_keep_the_first_answer() {
    let answer = "Git was available. The version check passed. Backup kept. Service is up.";
    let run = run_goal(vec![
        assistant_tool_call(
            "probe",
            "run_command",
            serde_json::json!({"program": "git", "args": ["--version"]}),
        ),
        assistant_says_and_calls(
            "g1",
            answer,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Git version verified.",
                "verification": {"status": "passed", "detail": "git status"},
                "deliver_answer": true,
            }),
        ),
        assistant_says_and_calls(
            "g2",
            "still the wrong command",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Git version verified.",
                "verification": {"status": "passed", "detail": "git status --short"},
                "deliver_answer": true,
            }),
        ),
        assistant_says_and_calls(
            "g3",
            "我去掉附加说明后重新提交",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Git version verified.",
                "verification": {"status": "passed", "detail": "call_id:probe"},
                "deliver_answer": true,
            }),
        ),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(command_calls(&run), 1);
    assert_eq!(deliveries_of(&run), vec![answer]);
    assert_eq!(run.outcome.model_steps, 4);
}

/// A textless blocked resolution must not reach back for an earlier refused
/// claim. The report the model owed is its own text; the summary is the
/// honest fallback.
#[tokio::test]
async fn a_textless_block_does_not_reuse_an_earlier_refused_claim() {
    let run = run_goal(vec![
        assistant_says_and_calls(
            "g1",
            "All tests pass.",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Goal complete.",
                "verification": {"status": "passed", "detail": "go test ./..."},
                "deliver_answer": true,
            }),
        ),
        assistant_tool_call(
            "g2",
            "update_goal",
            serde_json::json!({
                "status": "blocked",
                "summary": "Module cache is read-only.",
            }),
        ),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Blocked);
    assert!(
        !run.finished_texts()
            .iter()
            .any(|text| text == "All tests pass."),
        "the refused claim was never accepted, so it is not the delivery: {:?}",
        run.finished_texts()
    );
    assert!(run.final_text().contains("Module cache is read-only."));
}

/// `not_done` requirements refuse completion; the earlier claim of success is
/// again not a settlement and must not be delivered by the honest block.
#[tokio::test]
async fn an_incomplete_goal_cannot_publish_its_success_claim() {
    let run = run_goal_for(
        "implement the counting feature in `src/count.go`",
        vec![
            assistant_says_and_calls(
                "g1",
                "Everything is done.",
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "Goal complete.",
                    "requirements": [
                        {"requirement": "src/count.go", "status": "not_done", "evidence": "not implemented"}
                    ],
                }),
            ),
            assistant_says_and_calls(
                "g2",
                "Blocked: src/count.go cannot be added because the source tree is frozen.",
                "update_goal",
                serde_json::json!({
                    "status": "blocked",
                    "summary": "The source tree is frozen.",
                }),
            ),
        ],
    )
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Blocked);
    assert_eq!(
        run.final_text(),
        "Blocked: src/count.go cannot be added because the source tree is frozen."
    );
}

/// A provider failure on the delivery request is a real failure, never a
/// successful delivery of the earlier transition text.
#[tokio::test]
async fn a_provider_failure_on_the_delivery_round_is_not_a_completed_delivery() {
    let runtime = Arc::new(MockRuntime::with_stream_error(
        vec![
            assistant_says_and_calls(
                "g1",
                TRANSITION,
                "update_goal",
                serde_json::json!({"status": "complete", "summary": "Goal complete."}),
            ),
            assistant_text(ANSWER),
        ],
        ModelError::new(leveler_model::ModelErrorKind::Transport, "connection reset"),
    ));
    let dir = tempfile::tempdir().unwrap();
    let mut events = Vec::new();
    let result = Executor::new(
        runtime,
        Arc::new(default_registry()),
        ToolContext::new(
            Workspace::new(dir.path()).unwrap(),
            PermissionProfile::Assisted,
        ),
        ModelRef::new("mock", "m"),
        20,
    )
    .with_goal_mode(true)
    .run(
        "implement the counting feature",
        &mut |event| events.push(event),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await;

    let finished: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Finished(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    match result {
        Ok(outcome) => {
            assert_ne!(
                outcome.stop_reason,
                StopReason::Completed,
                "a failed delivery round cannot be a completed delivery"
            );
            assert!(!finished.contains(&TRANSITION.to_string()));
        }
        Err(error) => {
            assert!(!finished.contains(&TRANSITION.to_string()), "{error:?}");
        }
    }
}

/// If the hard budget stops the run before the delivery request can be sent,
/// the missing delivery must stay visible (an explicit stop reason), not be
/// papered over with the transition text.
#[tokio::test]
async fn an_exhausted_request_budget_leaves_a_visible_missing_delivery() {
    let run = run_goal_with(
        vec![
            assistant_says_and_calls(
                "g1",
                TRANSITION,
                "update_goal",
                serde_json::json!({"status": "complete", "summary": "Goal complete."}),
            ),
            assistant_text(ANSWER),
        ],
        |executor| {
            executor.with_step_limits(StepLimits {
                max_model_steps: Some(1),
                ..Default::default()
            })
        },
    )
    .await;

    // The goal itself settled, so the terminal still reports the settlement;
    // the missing answer is an explicit warning, and the transition text is
    // not dressed up as one.
    assert_eq!(run.final_text(), "Goal complete.");
    assert_ne!(run.final_text(), TRANSITION);
    assert!(!run.finished_texts().contains(&TRANSITION.to_string()));
    assert!(
        run.outcome
            .completion_warnings
            .iter()
            .any(|warning| warning.contains("no final answer")),
        "the missing delivery stays observable: {:?}",
        run.outcome.completion_warnings
    );
}

/// The model may correct its own conclusion. A refusal is not a settlement:
/// the answer offered with it is only a candidate, and an explicit revision
/// (`revise_answer`) replaces it. The delivered text is the revision, not the
/// first answer that happened to be written.
#[tokio::test]
async fn an_explicit_revision_replaces_the_refused_answer() {
    let first = "Git was available. The repository is clean. Service is up.";
    let revised = "Git was available. The repository has two modified files. Service is up.";
    let run = run_goal(vec![
        assistant_tool_call(
            "probe",
            "run_command",
            serde_json::json!({"program": "git", "args": ["--version"]}),
        ),
        assistant_says_and_calls(
            "g1",
            first,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Git version verified.",
                "verification": {"status": "passed", "detail": "git status"},
                "deliver_answer": true,
            }),
        ),
        assistant_says_and_calls(
            "g2",
            revised,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Git version verified.",
                "verification": {"status": "passed", "detail": "call_id:probe"},
                "deliver_answer": true,
                "revise_answer": true,
            }),
        ),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(deliveries_of(&run), vec![revised]);
    assert_eq!(run.final_text(), revised);
    assert_ne!(run.final_text(), first);
    assert_eq!(
        run.outcome.model_steps, 3,
        "the revision settles in the same request"
    );
}

/// A later revision offered with a still-bad citation is the new candidate.
/// When the following round repairs the citation without a declaration, the
/// last explicit revision is delivered — never the first answer.
#[tokio::test]
async fn a_revision_chain_delivers_the_last_explicit_revision() {
    let first = "The migration removed 12 rows.";
    let second = "The migration removed 10 rows; two were re-inserted.";
    let third = "The migration removed 10 rows; two remained by design.";
    let run = run_goal(vec![
        assistant_tool_call(
            "probe",
            "run_command",
            serde_json::json!({"program": "git", "args": ["--version"]}),
        ),
        assistant_says_and_calls(
            "g1",
            first,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Migration checked.",
                "verification": {"status": "passed", "detail": "git status"},
                "deliver_answer": true,
            }),
        ),
        assistant_says_and_calls(
            "g2",
            second,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Migration checked.",
                "verification": {"status": "passed", "detail": "git status --short"},
                "deliver_answer": true,
                "revise_answer": true,
            }),
        ),
        assistant_says_and_calls(
            "g3",
            third,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Migration checked.",
                "verification": {"status": "passed", "detail": "call_id:probe"},
                "deliver_answer": true,
                "revise_answer": true,
            }),
        ),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(deliveries_of(&run), vec![third]);
    assert_eq!(run.final_text(), third);
    assert_eq!(run.outcome.model_steps, 4);
}

/// Prose next to a refused completion that did not declare itself the answer
/// is progress, not a candidate. The runtime asks for the answer instead of
/// promoting the progress note.
#[tokio::test]
async fn an_undeclared_refusal_does_not_make_its_prose_the_answer() {
    let progress = "Let me check the recorded output before I claim completion.";
    let answer = "Git was available; the version check exited 0.";
    let run = run_goal(vec![
        assistant_tool_call(
            "probe",
            "run_command",
            serde_json::json!({"program": "git", "args": ["--version"]}),
        ),
        assistant_says_and_calls(
            "g1",
            progress,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Git version checked.",
                "verification": {"status": "passed", "detail": "git status"},
            }),
        ),
        assistant_says_and_calls(
            "g2",
            "citation corrected",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Git version checked.",
                "verification": {"status": "passed", "detail": "call_id:probe"},
            }),
        ),
        assistant_text(answer),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(deliveries_of(&run), vec![answer]);
    assert_eq!(run.final_text(), answer);
    assert_ne!(run.final_text(), progress);
    assert_eq!(
        run.outcome.model_steps, 4,
        "one delivery request was needed"
    );
}

/// Work and progress between the refused answer and the accepted settlement
/// do not touch the offered answer. With no declaration on the accepting
/// round, the answer already offered is the delivery.
#[tokio::test]
async fn progress_between_the_refused_answer_and_settlement_keeps_the_answer() {
    let answer = "Git was available; the version check exited 0.";
    let run = run_goal(vec![
        assistant_tool_call(
            "probe",
            "run_command",
            serde_json::json!({"program": "git", "args": ["--version"]}),
        ),
        assistant_says_and_calls(
            "g1",
            answer,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Git version checked.",
                "verification": {"status": "passed", "detail": "git status"},
                "deliver_answer": true,
            }),
        ),
        assistant_says_and_calls(
            "p1",
            "Checking the workspace once more.",
            "list_files",
            serde_json::json!({"path": "."}),
        ),
        assistant_says_and_calls(
            "g2",
            "Resubmitting the corrected citation.",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Git version checked.",
                "verification": {"status": "passed", "detail": "call_id:probe"},
            }),
        ),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(deliveries_of(&run), vec![answer]);
    assert_eq!(run.final_text(), answer);
}

/// An answer the model wrote on a quiet round, before it attempted to settle,
/// is still the text a later refused settlement explicitly declared. The
/// repair round must not force the model to write the answer again.
#[tokio::test]
async fn an_answer_written_before_a_refused_settlement_is_reused_after_the_repair() {
    let run = run_goal(vec![
        assistant_text(ANSWER),
        assistant_calls(&[(
            "g1",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Goal complete.",
                "deliver_answer": true,
                "verification": {"status": "passed", "detail": "go test ./... → exit 0"},
            }),
        )]),
        assistant_calls(&[(
            "g2",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Goal complete.",
                "deliver_answer": true,
                "verification": {"status": "not_applicable", "detail": "no executable check applies"},
            }),
        )]),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(
        run.events
            .iter()
            .filter(|event| matches!(
                event,
                AgentEvent::GoalIntercepted { kind, .. } if kind == "verification_refused"
            ))
            .count(),
        1
    );
    assert_eq!(deliveries_of(&run), vec![ANSWER]);
    assert_eq!(run.final_text(), ANSWER);
    assert_eq!(
        run.outcome.model_steps, 3,
        "the accepted repair reused the answer instead of asking for it again"
    );
}

/// A refused update_goal is a refusal of the SETTLEMENT, not of the answer it
/// declared. The declaration survives even a later work round, so the accepted
/// settlement delivers it instead of asking the model to write it again.
#[tokio::test]
async fn a_refused_status_declaration_keeps_the_answer_it_declared() {
    let refused_round = ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![
                ContentPart::Text {
                    text: ANSWER.to_string(),
                },
                ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new("w1"),
                        name: "list_files".to_string(),
                        arguments: serde_json::json!({"path": "."}),
                    },
                },
                ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new("g1"),
                        name: "update_goal".to_string(),
                        arguments: serde_json::json!({
                            "status": "done",
                            "summary": "Goal complete.",
                            "deliver_answer": true,
                        }),
                    },
                },
            ],
            route: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    };
    let run = run_goal(vec![
        assistant_text(ANSWER),
        refused_round,
        goal_complete("g2", Some(true)),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(
        run.events
            .iter()
            .filter(|event| matches!(
                event,
                AgentEvent::GoalIntercepted { kind, .. } if kind == "invalid_status"
            ))
            .count(),
        1
    );
    assert_eq!(deliveries_of(&run), vec![ANSWER]);
    assert_eq!(run.final_text(), ANSWER);
    assert_eq!(
        run.outcome.model_steps, 3,
        "the accepted settlement reused the declared answer"
    );
}

/// A refusal that follows real work in the same round must not reach back for
/// the prose written before that work. The declaration promises "the answer I
/// wrote", and after a work round with no visible text there is no such answer
/// — the model's next stated answer is the one the goal settles on.
#[tokio::test]
async fn a_refusal_after_work_does_not_resurrect_the_pre_work_answer() {
    let stale = "The repository contains 3 tracked files.";
    let revised = "The repository contains 2 tracked files.";
    let run = run_goal(vec![
        assistant_says_and_calls(
            "p1",
            stale,
            "update_plan",
            serde_json::json!({"plan": [{"step": "count the tracked files", "status": "completed"}]}),
        ),
        assistant_calls(&[
            ("w1", "list_files", serde_json::json!({"path": "."})),
            (
                "g1",
                "update_goal",
                serde_json::json!({
                    "status": "complete",
                    "summary": "Counted the tracked files.",
                    "deliver_answer": true,
                    "verification": {"status": "passed", "detail": "git status"},
                }),
            ),
        ]),
        assistant_says_and_calls(
            "g2",
            revised,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Counted the tracked files.",
                "deliver_answer": true,
            }),
        ),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(deliveries_of(&run), vec![revised]);
    assert_eq!(run.final_text(), revised);
}

/// A revision is still only a candidate until the goal settles. When the
/// revised completion is refused too and the model blocks, neither the
/// revision nor the superseded answer may be presented as delivered.
#[tokio::test]
async fn a_refused_revision_is_not_delivered_when_the_goal_blocks() {
    let first = "The migration removed 12 rows.";
    let revised = "The migration removed 10 rows.";
    let blocked = "Blocked: the migration ledger cannot be read without credentials.";
    let run = run_goal(vec![
        assistant_tool_call(
            "probe",
            "run_command",
            serde_json::json!({"program": "git", "args": ["--version"]}),
        ),
        assistant_says_and_calls(
            "g1",
            first,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Migration checked.",
                "verification": {"status": "passed", "detail": "git status"},
                "deliver_answer": true,
            }),
        ),
        assistant_says_and_calls(
            "g2",
            revised,
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "Migration checked.",
                "verification": {"status": "passed", "detail": "git status --short"},
                "deliver_answer": true,
                "revise_answer": true,
            }),
        ),
        assistant_says_and_calls(
            "g3",
            blocked,
            "update_goal",
            serde_json::json!({
                "status": "blocked",
                "summary": "Migration ledger unavailable.",
            }),
        ),
    ])
    .await;

    assert_eq!(run.outcome.stop_reason, StopReason::Blocked);
    assert_eq!(run.final_text(), blocked);
    assert_eq!(deliveries_of(&run), vec![blocked]);
    assert!(!run.finished_texts().iter().any(|text| text == first));
    assert!(!run.finished_texts().iter().any(|text| text == revised));
}

/// Two accepted settlements in one response are still one commit. Settlement
/// is idempotent: a duplicate call cannot publish a second delivery.
#[tokio::test]
async fn a_duplicate_settlement_in_one_response_commits_once() {
    let answer = "Git was available; the version check exited 0.";
    let settle = || ToolCall {
        id: ToolCallId::new("g"),
        name: "update_goal".to_string(),
        arguments: serde_json::json!({
            "status": "complete",
            "summary": "Git version checked.",
            "deliver_answer": true,
        }),
    };
    let mut first = settle();
    let mut second = settle();
    first.id = ToolCallId::new("g1");
    second.id = ToolCallId::new("g2");
    let response = ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![
                ContentPart::Text {
                    text: answer.to_string(),
                },
                ContentPart::ToolCall { call: first },
                ContentPart::ToolCall { call: second },
            ],
            route: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    };
    let run = run_goal(vec![response]).await;

    assert_eq!(run.outcome.stop_reason, StopReason::Completed);
    assert_eq!(deliveries_of(&run), vec![answer]);
    assert_eq!(run.outcome.model_steps, 1);
}
