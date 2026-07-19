//! Drives the executor loop with a scripted mock model runtime, proving the
//! model↔tool round-trip actually reads files, applies a patch, and finishes.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentError, AgentEvent, Executor, NoopSink, StopReason};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{
    ApprovalRequest, AutoReviewer, PermissionProfile, ReviewVerdict, Workspace,
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_tools::{
    RiskLevel, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry, default_registry,
};

/// A model runtime that replays scripted responses in order.
struct MockRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
    /// Optional per-round stream-level error, injected before `MessageCompleted`
    /// to mirror the real assembler surfacing a tool-call JSON decode failure.
    /// `Some(None)` means "no error this round".
    stream_errors: Mutex<VecDeque<Option<ModelError>>>,
}

struct DenyReviewer;

struct UnterminatedRuntime;

#[async_trait]
impl AutoReviewer for DenyReviewer {
    async fn review(&self, _request: &ApprovalRequest) -> ReviewVerdict {
        ReviewVerdict::Deny("blocked by auto-review".to_string())
    }
}

#[async_trait]
impl ModelRuntime for UnterminatedRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unreachable!("the executor uses streaming")
    }

    async fn stream(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(ModelEvent::MessageStarted {
                request_id: RequestId::new("unterminated"),
            }),
            Ok(ModelEvent::TextDelta {
                delta: "partial".to_string(),
            }),
        ])))
    }

    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

impl MockRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
            stream_errors: Mutex::new(VecDeque::new()),
        }
    }

    /// Like `new`, but injects a stream-level error on the given rounds. Each
    /// entry aligns with one response, in order.
    fn with_stream_errors(responses: Vec<ModelResponse>, errors: Vec<Option<ModelError>>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
            stream_errors: Mutex::new(VecDeque::from(errors)),
        }
    }

    fn recorded_requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
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
                ContentPart::Text { text } => {
                    events.push(Ok(ModelEvent::TextDelta {
                        delta: text.clone(),
                    }));
                }
                ContentPart::Reasoning { text } => {
                    events.push(Ok(ModelEvent::ReasoningDelta {
                        delta: text.clone(),
                    }));
                }
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }));
                }
                _ => {}
            }
        }
        // A scripted decode failure for this round arrives after content and
        // before completion, exactly as the real assembler surfaces it.
        if let Some(Some(error)) = self.stream_errors.lock().unwrap().pop_front() {
            events.push(Ok(ModelEvent::Error { error }));
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn scoped_agents_rules_are_injected_after_reading_matching_path() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-scoped-rules-{}",
        std::process::id() as u64 * 19 + 6
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("AGENTS.md"), "Root rule.").unwrap();
    std::fs::write(dir.join("src/AGENTS.md"), "Nested src rule.").unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn old() {}\n").unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::with_environment(
        workspace,
        PermissionProfile::Assisted,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )),
    );
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call("c1", "read_file", serde_json::json!({"path": "src/lib.rs"})),
        assistant_text("done"),
    ]));

    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );

    executor
        .run(
            "read lib",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let requests = runtime.recorded_requests();
    assert_eq!(requests.len(), 2);
    let first_system = requests[0].messages[0].text_content();
    let second_system = requests[1].messages[0].text_content();

    // Root rules live in the system prompt, which must never change mid-loop:
    // it is the first thing the provider's prefix cache matches on.
    assert!(first_system.contains("Root rule."));
    assert_eq!(first_system, second_system);
    assert!(!second_system.contains("Nested src rule."));

    // Every message round 1 sent must survive byte-identical as a prefix of
    // round 2, or the prefix cache misses from the first divergent token.
    for (i, sent) in requests[0].messages.iter().enumerate() {
        assert_eq!(
            sent.text_content(),
            requests[1].messages[i].text_content(),
            "message {i} changed between rounds — this breaks the prefix cache"
        );
    }

    // The nested rule arrives as a fresh message appended at the tail.
    let tail = requests[1].messages.last().unwrap().text_content();
    assert!(tail.contains("--- from src/AGENTS.md ---"), "tail: {tail}");
    assert!(tail.contains("Nested src rule."), "tail: {tail}");

    std::fs::remove_dir_all(&dir).ok();
}

/// Reading a second file under the same directory must not re-inject that
/// directory's `AGENTS.md`: a duplicate would grow the transcript every round.
#[tokio::test]
async fn scoped_rules_are_injected_at_most_once() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-scoped-rules-once-{}",
        std::process::id() as u64 * 23 + 11
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/AGENTS.md"), "Nested src rule.").unwrap();
    std::fs::write(dir.join("src/a.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(dir.join("src/b.rs"), "pub fn b() {}\n").unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call("c1", "read_file", serde_json::json!({"path": "src/a.rs"})),
        assistant_tool_call("c2", "read_file", serde_json::json!({"path": "src/b.rs"})),
        assistant_text("done"),
    ]));

    Executor::new(
        runtime.clone(),
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "read both",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let requests = runtime.recorded_requests();
    let last = requests.last().unwrap();
    let injections = last
        .messages
        .iter()
        .filter(|m| m.text_content().contains("--- from src/AGENTS.md ---"))
        .count();
    assert_eq!(injections, 1, "scoped rule injected {injections} times");

    std::fs::remove_dir_all(&dir).ok();
}

/// A mock runtime that also records the messages of every request it receives,
/// so a test can assert what the executor actually sent the model.
struct RecordingRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl ModelRuntime for RecordingRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unimplemented!()
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        self.seen.lock().unwrap().push(request.messages.clone());
        let response = self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })?;
        let mut events: Vec<Result<ModelEvent, ModelError>> =
            vec![Ok(ModelEvent::MessageStarted {
                request_id: response.request_id.clone(),
            })];
        for part in &response.message.content {
            match part {
                ContentPart::Text { text } => events.push(Ok(ModelEvent::TextDelta {
                    delta: text.clone(),
                })),
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }))
                }
                _ => {}
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

/// A successful update_plan tool call must surface the structured plan as an
/// [`AgentEvent::PlanUpdated`] — otherwise the plan lives only in the tool
/// result metadata and the UI (Plan screen / inline checklist) never sees it
/// on the direct path.
#[tokio::test]
async fn update_plan_tool_call_emits_a_plan_updated_event() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-plan-event-{}",
        std::process::id() as u64 * 31 + 7
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "update_plan",
            serde_json::json!({
                "plan": [
                    {"step": "add a failing test", "status": "in_progress"},
                    {"step": "implement the fix", "status": "pending"},
                ]
            }),
        ),
        assistant_text("done"),
    ]));

    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );

    let mut events: Vec<AgentEvent> = Vec::new();
    executor
        .run(
            "do the thing",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let plan = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::PlanUpdated { steps } => Some(steps.clone()),
            _ => None,
        })
        .expect("a successful update_plan call must emit PlanUpdated");
    assert_eq!(plan.len(), 2);
    assert_eq!(plan[0].step, "add a failing test");
    assert_eq!(plan[0].status, "in_progress");
    assert_eq!(plan[1].status, "pending");

    std::fs::remove_dir_all(&dir).ok();
}

fn assistant_tool_call(id: &str, name: &str, args: serde_json::Value) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: name.to_string(),
                    arguments: args,
                },
            }],
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
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

fn assistant_text_finished(text: &str, finish_reason: FinishReason) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, text),
        finish_reason,
        usage: TokenUsage::default(),
    }
}

fn assistant_reasoning_then_text(reasoning: &str, text: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::Reasoning {
                    text: reasoning.to_string(),
                },
                ContentPart::Text {
                    text: text.to_string(),
                },
            ],
        },
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

#[tokio::test]
async fn executor_forwards_reasoning_delta() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-reasoning-{}",
        std::process::id() as u64 * 29 + 3
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(MockRuntime::new(vec![assistant_reasoning_then_text(
        "thinking", "done",
    )]));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );

    let mut events = Vec::new();
    executor
        .run(
            "answer",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(events.iter().any(|event| {
        matches!(event, AgentEvent::ReasoningDelta(delta) if delta == "thinking")
    }));
    assert!(
        events
            .iter()
            .any(|event| { matches!(event, AgentEvent::AssistantDelta(delta) if delta == "done") })
    );
}

#[tokio::test]
async fn auto_reviewer_can_deny_before_user_approval() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-auto-review-{}",
        std::process::id() as u64 * 31 + 4
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "run_command",
            serde_json::json!({"program": "rm", "args": ["-rf", "scratch"]}),
        ),
        assistant_text("stopped"),
    ]));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_auto_reviewer(Arc::new(DenyReviewer));

    let mut events = Vec::new();
    let outcome = executor
        .run(
            "push the branch",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        outcome.progress.cumulative_commands, 0,
        "a command denied before execution must not consume command budget"
    );

    assert!(events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::ToolResult {
                is_error: true,
                preview,
                ..
            } if preview.contains("blocked by auto-review")
        )
    }));
}

#[tokio::test]
async fn executor_reads_patches_and_finishes() {
    // A real workspace with one file to inspect.
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-{}",
        std::process::id() as u64 * 7 + 1
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn old() {}\n").unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Script: read the file, apply a patch adding a function, then finish.
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call("c1", "read_file", serde_json::json!({"path": "src/lib.rs"})),
        assistant_tool_call(
            "c2",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
            }),
        ),
        assistant_text("Added the `added` function to src/lib.rs."),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );

    let mut events = Vec::new();
    let outcome = executor
        .run(
            "Add an `added` function to lib.rs",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(outcome.final_text.contains("added"));
    assert_eq!(outcome.rounds, 3);
    assert!(outcome.modified_files.contains(&"src/lib.rs".to_string()));

    let content = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    assert_eq!(content, "pub fn old() {}\npub fn added() {}\n");

    // The loop should have surfaced both tool calls.
    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            leveler_agent::AgentEvent::ToolCall { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls, vec!["read_file", "apply_patch"]);

    std::fs::remove_dir_all(&dir).ok();
}

/// A weaker model routinely emits a `run_command` tool call whose arguments are
/// not valid JSON (an unescaped backslash from a regex/path). Today that decode
/// error kills the whole turn. Instead, the loop should feed the error back to
/// the model and let it retry, so a single malformed tool call is recoverable.
#[tokio::test]
async fn goal_mode_quiet_does_not_finish_until_update_goal() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-goalmode-{}",
        std::process::id() as u64 * 41 + 5
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Round 1: the model goes quiet (no tool call) — must NOT finish in goal mode.
    // Round 2: it resolves explicitly via update_goal(complete).
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_text("I believe the task is done."),
        assistant_tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "All requirements verified."}),
        ),
    ]));

    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_goal_mode(true);

    let outcome = executor
        .run(
            "do the task",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("goal-mode run should not error");

    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert!(
        outcome.final_text.contains("All requirements verified"),
        "final text is the update_goal summary: {}",
        outcome.final_text
    );
    // Two model requests: the quiet round was re-prompted, then it resolved.
    assert_eq!(
        runtime.recorded_requests().len(),
        2,
        "quiet round was re-prompted"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn goal_mode_update_goal_emits_completed_tool_event() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-goalevent-{}",
        std::process::id() as u64 * 47 + 9
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(MockRuntime::new(vec![assistant_tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "Done."}),
    )]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_goal_mode(true);

    let mut events = Vec::new();
    let outcome = executor
        .run(
            "do the task",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("goal-mode run should not error");

    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCall { id, name, .. }
                if id == "g1" && name == "update_goal"
        )),
        "update_goal should still be visible as a tool call event"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResult {
                id,
                name,
                is_error,
                ..
            } if id == "g1" && name == "update_goal" && !is_error
        )),
        "update_goal needs a matching successful result so UIs do not mark it failed"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn goal_mode_blocked_reports_blocked() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-goalblocked-{}",
        std::process::id() as u64 * 43 + 7
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(MockRuntime::new(vec![assistant_tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "blocked", "summary": "Missing an upstream credential."}),
    )]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_goal_mode(true);

    let outcome = executor
        .run(
            "do the task",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Blocked);
    assert!(outcome.final_text.contains("credential"));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn invalid_tool_call_json_is_fed_back_and_retried() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-decode-retry-{}",
        std::process::id() as u64 * 31 + 3
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Round 1: the model "emits" a run_command whose JSON args fail to decode.
    // Round 2: after the feedback, it finishes cleanly.
    let decode_error = ModelError::new(
        leveler_model::ModelErrorKind::Decode,
        "tool call `run_command` (index 0) had invalid JSON arguments: \
         invalid escape at line 1 column 113",
    );
    let runtime = Arc::new(MockRuntime::with_stream_errors(
        vec![
            assistant_text("Let me search the code."),
            assistant_text("Done."),
        ],
        vec![Some(decode_error), None],
    ));

    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );

    let mut events = Vec::new();
    let outcome = executor
        .run(
            "search the code",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("a single malformed tool call must not fail the whole turn");

    assert_eq!(outcome.stop_reason, StopReason::Answered);

    // The loop re-prompted the model after the decode error.
    let requests = runtime.recorded_requests();
    assert_eq!(
        requests.len(),
        2,
        "should have retried once after the error"
    );

    // The retry request carried the decode error back to the model.
    let fed_back =
        requests[1].messages.iter().any(|m| {
            m.content.iter().any(|p| matches!(
            p,
            ContentPart::Text { text } if text.contains("invalid JSON") || text.contains("JSON")
        ))
        });
    assert!(
        fed_back,
        "the retry must feed the decode error back to the model"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn project_agents_md_is_injected_before_the_first_user_message() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-agentsmd-{}",
        std::process::id() as u64 * 29 + 8
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("AGENTS.md"),
        "Always run cargo fmt before finishing.",
    )
    .unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = Arc::new(RecordingRuntime {
        responses: Mutex::new(VecDeque::from(vec![assistant_text("ok")])),
        seen: seen.clone(),
    });

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    );
    executor
        .run(
            "hello",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let requests = seen.lock().unwrap();
    let first = &requests[0];
    let system_idx = first.iter().position(|m| m.role == Role::System).unwrap();
    let task_idx = first
        .iter()
        .position(|m| m.role == Role::User && m.text_content() == "hello")
        .expect("the user task message");
    let system_text = first[system_idx].text_content();

    assert!(
        system_idx < task_idx,
        "system prompt must precede the user task"
    );
    assert!(
        system_text.contains("Project rules:"),
        "project rules block missing: {system_text}"
    );
    assert!(
        system_text.contains("Always run cargo fmt before finishing."),
        "AGENTS.md content missing: {system_text}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn dangerous_command_denied_is_fed_back_not_executed() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-deny-{}",
        std::process::id() as u64 * 13 + 2
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // The model tries `rm -rf` (dangerous); after the denial it gives up.
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "run_command",
            serde_json::json!({"program": "rm", "args": ["-rf", "scratch"]}),
        ),
        assistant_text("Understood, I will not delete it."),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_approver(Arc::new(leveler_execution::AutoDeny));

    let mut events = Vec::new();
    let outcome = executor
        .run(
            "push the branch",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    // The run_command result must be an error mentioning the denial.
    let denied = events.iter().any(|e| {
        matches!(e, leveler_agent::AgentEvent::ToolResult { name, is_error, preview, .. }
            if name == "run_command" && *is_error && preview.contains("denied"))
    });
    assert!(denied, "expected a denied run_command result: {events:?}");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn consecutive_search_budget_denies_the_excess_and_feeds_it_back() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-search-{}",
        std::process::id() as u64 * 13 + 3
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn old() {}\n").unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // The model greps three times in a row; with a budget of 2 the third is
    // denied without ever running the tool.
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call("c1", "grep", serde_json::json!({"pattern": "old"})),
        assistant_tool_call("c2", "grep", serde_json::json!({"pattern": "fn"})),
        assistant_tool_call("c3", "grep", serde_json::json!({"pattern": "pub"})),
        assistant_text("done"),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_execution_controls(0, 2, 0);

    let mut events = Vec::new();
    executor
        .run(
            "search around",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let denied = events.iter().any(|e| {
        matches!(e, leveler_agent::AgentEvent::ToolResult { name, is_error, preview, .. }
            if name == "grep" && *is_error && preview.contains("budget"))
    });
    assert!(
        denied,
        "third grep should be denied by the search budget: {events:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn unsupported_task_alias_tells_the_model_to_use_spawn_agent() {
    let dir = std::env::temp_dir().join(format!("leveler-agent-task-alias-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "task-1",
            "task",
            serde_json::json!({
                "description": "Explore providers",
                "prompt": "Inspect provider architecture"
            }),
        ),
        assistant_text("I will use spawn_agent instead."),
    ]));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );

    let mut events = Vec::new();
    executor
        .run(
            "inspect architecture",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        events.iter().any(|event| {
            matches!(event,
            AgentEvent::ToolResult { name, is_error: true, preview, .. }
                if name == "task" && preview.contains("spawn_agent"))
        }),
        "unknown task feedback must name the supported delegation tool: {events:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn completion_evidence_gate_allows_plain_text_without_workspace_change() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-plain-{}",
        std::process::id() as u64 * 13 + 4
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(MockRuntime::new(vec![assistant_text("你好！")]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_structure(false, true);

    let outcome = executor
        .run("你好", &mut |_| {}, &mut NoopSink, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(outcome.rounds, 1, "plain chat must not be forced to verify");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn completion_evidence_gate_refuses_unverified_finish_after_edit() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-evidence-{}",
        std::process::id() as u64 * 13 + 5
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn old() {}\n").unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
            }),
        ),
        assistant_text("All done."),
        assistant_text("Confirmed, tests pass."),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_structure(false, true);

    let outcome = executor
        .run(
            "Add an `added` function to lib.rs",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(
        outcome.rounds, 3,
        "first unverified completion after an edit must be refused"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A runtime that reports a huge token count every round (to force compaction)
/// and records whether any request it received carried the compaction
/// breadcrumb — proof the `drive` loop actually folded the transcript.
struct CompactingRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    saw_breadcrumb: Arc<std::sync::atomic::AtomicBool>,
}

struct UsageRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Arc<std::sync::atomic::AtomicUsize>,
    usage: TokenUsage,
}

#[async_trait]
impl ModelRuntime for UsageRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unimplemented!()
    }

    async fn stream(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let response = self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })?;
        let mut events = vec![Ok(ModelEvent::MessageStarted {
            request_id: response.request_id.clone(),
        })];
        for part in &response.message.content {
            match part {
                ContentPart::Text { text } => {
                    events.push(Ok(ModelEvent::TextDelta {
                        delta: text.clone(),
                    }));
                }
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }));
                }
                _ => {}
            }
        }
        events.push(Ok(ModelEvent::UsageUpdated { usage: self.usage }));
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn configured_token_budget_stops_before_another_model_request() {
    let dir =
        std::env::temp_dir().join(format!("leveler-agent-token-budget-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runtime = Arc::new(UsageRuntime {
        responses: Mutex::new(VecDeque::from(vec![
            assistant_tool_call("c1", "list_files", serde_json::json!({"path": "."})),
            assistant_text("must not be requested"),
        ])),
        requests: requests.clone(),
        usage: TokenUsage {
            input_tokens: 60,
            output_tokens: 40,
            cached_input_tokens: 0,
        },
    });
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        ToolContext::new(workspace, PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        0,
    )
    .with_step_limits(leveler_agent::StepLimits {
        max_model_tokens: Some(100),
        ..Default::default()
    });

    let outcome = executor
        .run(
            "inspect within budget",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::BudgetExhausted);
    assert_eq!(outcome.rounds, 1);
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(outcome.final_text.contains("token"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn configured_cost_budget_uses_profile_pricing_and_stops_before_next_request() {
    let dir =
        std::env::temp_dir().join(format!("leveler-agent-cost-budget-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runtime = Arc::new(UsageRuntime {
        responses: Mutex::new(VecDeque::from(vec![
            assistant_tool_call("c1", "list_files", serde_json::json!({"path": "."})),
            assistant_text("must not be requested"),
        ])),
        requests: requests.clone(),
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 10,
            cached_input_tokens: 0,
        },
    });
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        ToolContext::new(workspace, PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        0,
    )
    .with_pricing(Some(leveler_model::ModelPricing {
        input_usd_per_mtok: 1.0,
        output_usd_per_mtok: 2.0,
    }))
    .with_step_limits(leveler_agent::StepLimits {
        max_cost_usd_micros: Some(30),
        ..Default::default()
    });

    let outcome = executor
        .run(
            "inspect within cost budget",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::BudgetExhausted);
    assert_eq!(outcome.rounds, 1);
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(outcome.final_text.contains("cost"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn configured_cost_budget_requires_model_pricing_before_first_request() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-cost-budget-no-pricing-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runtime = Arc::new(UsageRuntime {
        responses: Mutex::new(VecDeque::from(vec![assistant_text("unused")])),
        requests: requests.clone(),
        usage: TokenUsage::default(),
    });
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        ToolContext::new(workspace, PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        0,
    )
    .with_step_limits(leveler_agent::StepLimits {
        max_cost_usd_micros: Some(1),
        ..Default::default()
    });

    let error = executor
        .run(
            "inspect within cost budget",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, AgentError::InvalidBudget(_)));
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[async_trait]
impl ModelRuntime for CompactingRuntime {
    /// Compaction summarizes the elided middle through `generate`. Deliberately
    /// avoids the word "compacted" so the breadcrumb assertion still proves the
    /// fold happened, not just that a summary came back.
    async fn generate(
        &self,
        _r: ModelRequest,
        _c: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        Ok(assistant_text("earlier rounds read src/lib.rs repeatedly"))
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _c: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        if request
            .messages
            .iter()
            .any(|m| m.text_content().contains("compacted"))
        {
            self.saw_breadcrumb
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::new(leveler_model::ModelErrorKind::Other, "drained"))?;
        let mut events: Vec<Result<ModelEvent, ModelError>> =
            vec![Ok(ModelEvent::MessageStarted {
                request_id: response.request_id.clone(),
            })];
        for part in &response.message.content {
            match part {
                ContentPart::Text { text } => events.push(Ok(ModelEvent::TextDelta {
                    delta: text.clone(),
                })),
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }))
                }
                _ => {}
            }
        }
        // Report a context size far above the budget so compaction triggers.
        events.push(Ok(ModelEvent::UsageUpdated {
            usage: TokenUsage {
                input_tokens: 500_000,
                output_tokens: 100,
                cached_input_tokens: 0,
            },
        }));
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, _m: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn long_run_auto_compacts_when_over_budget() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-compact-{}",
        std::process::id() as u64 * 17 + 5
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    for i in 0..16 {
        std::fs::write(
            dir.join(format!("src/f{i}.rs")),
            format!("pub fn f{i}() {{}}\n"),
        )
        .unwrap();
    }
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // 16 distinct read rounds (enough transcript to have a compactible middle,
    // without tripping the identical-call loop guard) + finish.
    let mut responses: Vec<ModelResponse> = (0..16)
        .map(|i| {
            assistant_tool_call(
                &format!("c{i}"),
                "read_file",
                serde_json::json!({"path": format!("src/f{i}.rs")}),
            )
        })
        .collect();
    responses.push(assistant_text("done reading"));

    let saw = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runtime = Arc::new(CompactingRuntime {
        responses: Mutex::new(VecDeque::from(responses)),
        saw_breadcrumb: saw.clone(),
    });

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        20,
    )
    .with_context_budget(1000); // 500k reported >> 1000 → compact every round

    let mut context_snapshots = 0usize;
    let outcome = executor
        .run(
            "read the file many times",
            &mut |event| {
                if matches!(event, AgentEvent::ContextSnapshot { .. }) {
                    context_snapshots += 1;
                }
            },
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(
        saw.load(std::sync::atomic::Ordering::SeqCst),
        "a later request should carry the compaction breadcrumb"
    );
    assert!(
        context_snapshots > 0,
        "the exact compacted model context must be emitted for durable recovery"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A runtime whose stream errors with a retryable StreamInterrupted the first
/// N times, then succeeds — proving the executor retries the round instead of
/// aborting the whole task on a transient network blip.
struct FlakyStreamRuntime {
    fails_left: Mutex<u32>,
    response: Mutex<Option<ModelResponse>>,
}

#[async_trait]
impl ModelRuntime for FlakyStreamRuntime {
    async fn generate(
        &self,
        _r: ModelRequest,
        _c: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unimplemented!()
    }
    async fn stream(
        &self,
        _request: ModelRequest,
        _c: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::{ModelErrorKind, ModelEvent};
        {
            let mut left = self.fails_left.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                // Deliberately diverges from the eventual successful answer.
                // A failed attempt's visible prefix must never leak into the
                // UI because there is no append-only event that can erase it.
                let events = vec![
                    Ok(ModelEvent::MessageStarted {
                        request_id: RequestId::new(format!("failed-{left}")),
                    }),
                    Ok(ModelEvent::TextDelta {
                        delta: format!("stale attempt {left}"),
                    }),
                    Err(ModelError::new(
                        ModelErrorKind::StreamInterrupted,
                        "connection dropped",
                    )),
                ];
                return Ok(Box::pin(futures::stream::iter(events)));
            }
        }
        let response = self.response.lock().unwrap().clone().unwrap();
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::MessageStarted {
                request_id: response.request_id.clone(),
            }),
            Ok(ModelEvent::TextDelta {
                delta: "all done".into(),
            }),
            Ok(ModelEvent::MessageCompleted {
                finish_reason: response.finish_reason,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
    async fn profile(&self, _m: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn retryable_mid_stream_error_is_retried_not_fatal() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-flaky-{}",
        std::process::id() as u64 * 19 + 6
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(FlakyStreamRuntime {
        fails_left: Mutex::new(2), // fail twice, succeed on the 3rd attempt
        response: Mutex::new(Some(assistant_text("all done"))),
    });
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    );

    let mut events = Vec::new();
    let outcome = executor
        .run(
            "say done",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("retryable stream error must not abort the run");
    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(outcome.final_text.contains("all done"));
    let first_delta = events
        .iter()
        .position(|event| matches!(event, AgentEvent::AssistantDelta(_)))
        .unwrap();
    let second_reset = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event, AgentEvent::StreamAttemptStarted))
        .nth(1)
        .map(|(index, _)| index)
        .unwrap();
    assert!(
        first_delta < second_reset,
        "failed-attempt deltas must stream live before retry begins"
    );
    let mut visible = String::new();
    for event in &events {
        match event {
            AgentEvent::StreamAttemptStarted => visible.clear(),
            AgentEvent::AssistantDelta(delta) => visible.push_str(delta),
            _ => {}
        }
    }
    assert_eq!(
        visible, "all done",
        "only the successful divergent attempt may become visible"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn repeated_identical_call_is_blocked_by_loop_guard() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-loopguard-{}",
        std::process::id() as u64 * 23 + 7
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // The model calls the same list_files 4 times (identical output), then stops.
    let mut responses: Vec<ModelResponse> = (0..4)
        .map(|i| {
            assistant_tool_call(
                &format!("c{i}"),
                "list_files",
                serde_json::json!({"path": "."}),
            )
        })
        .collect();
    responses.push(assistant_text("done"));
    let runtime = Arc::new(MockRuntime::new(responses));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "list repeatedly",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        outcome.stop_reason,
        StopReason::Incomplete,
        "identical-call thrash hard-stop must not be Answered"
    );
    let blocked = events.iter().any(|e| {
        matches!(e,
        leveler_agent::AgentEvent::ToolResult { is_error: true, preview, .. }
            if preview.contains("no progress"))
    });
    assert!(
        blocked,
        "loop guard should block the repeated identical call: {events:?}"
    );

    // A guard refuses the call before it runs, but the call still happened —
    // it must be surfaced. A ToolResult whose id was never announced by a
    // ToolCall leaves the UI with no arguments to render, so the row comes out
    // blank ("✗ 运行" with no command attached).
    let announced: std::collections::HashSet<&str> = events
        .iter()
        .filter_map(|e| match e {
            leveler_agent::AgentEvent::ToolCall { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let orphans: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            leveler_agent::AgentEvent::ToolResult { id, .. }
                if !announced.contains(id.as_str()) =>
            {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(
        orphans.is_empty(),
        "every tool result must have a matching tool call; orphaned results: {orphans:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// After every plan step is completed, closeout thrash (re-running git status
/// via different wrappers) must be refused and the turn must hard-stop rather
/// than burn rounds re-auditing. Plan complete means the task is done, so the
/// stop is Answered (verify decides pass/fail), NOT Incomplete — Incomplete
/// would misreport a finished task as a failure.
#[tokio::test]
async fn completed_plan_refuses_observe_thrash_and_stops() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-closeout-{}",
        std::process::id() as u64 * 41 + 3
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "p1",
            "update_plan",
            serde_json::json!({
                "plan": [
                    {"step": "update docs", "status": "completed"},
                    {"step": "mark roadmap", "status": "completed"}
                ]
            }),
        ),
        // Round 1 thrash after plan green.
        assistant_tool_call(
            "g1",
            "shell_command",
            serde_json::json!({"cmd": "git status --porcelain"}),
        ),
        // Round 2 thrash with a different wrapper — still observe:git_status.
        assistant_tool_call(
            "g2",
            "run_command",
            serde_json::json!({"program": "git", "args": ["status", "-sb"]}),
        ),
        // Must not be reached: hard-stop after two deny rounds.
        assistant_text("should not need another model round"),
    ]));

    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "update remaining work docs",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        outcome.stop_reason,
        StopReason::Answered,
        "plan complete = done; closeout thrash must not be misreported as Incomplete"
    );
    assert!(
        outcome
            .stop_detail
            .as_deref()
            .is_some_and(|d| d.contains("closeout")),
        "expected a closeout short-circuit detail: {:?}",
        outcome.stop_detail
    );
    let closeout_denials = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ToolResult { is_error: true, preview, .. }
                    if preview.contains("Plan steps are complete")
            )
        })
        .count();
    assert!(
        closeout_denials >= 2,
        "expected ≥2 plan-complete observe denials: {events:?}"
    );
    // Third scripted text response must remain unused.
    assert_eq!(
        runtime.recorded_requests().len(),
        3,
        "plan + 2 thrash rounds only (no extra model call after hard-stop)"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn complex_task_must_register_a_structured_plan_before_tools_run() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-plan-gate-{}",
        std::process::id() as u64 * 31 + 11
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let patch = "*** Begin Patch\n*** Update File: a.txt\n-old\n+new\n*** End Patch";
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call("c1", "apply_patch", serde_json::json!({"patch": patch})),
        assistant_tool_call(
            "c2",
            "update_plan",
            serde_json::json!({
                "plan": [
                    {"step": "inspect", "status": "completed"},
                    {"step": "edit", "status": "in_progress"},
                    {"step": "verify", "status": "pending"}
                ]
            }),
        ),
        assistant_tool_call("c3", "apply_patch", serde_json::json!({"patch": patch})),
        assistant_text("done"),
    ]));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    )
    .with_structure(true, false);
    let mut events = Vec::new();

    executor
        .run(
            "1. inspect the current implementation\n2. change the behavior\n3. run verification",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let first_edit_was_blocked = events.iter().any(|event| {
        matches!(event,
            AgentEvent::ToolResult { id, is_error: true, preview, .. }
                if id == "c1" && preview.contains("update_plan"))
    });
    assert!(first_edit_was_blocked, "events: {events:?}");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "new\n");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn nested_agents_rules_are_loaded_before_the_first_scoped_edit() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-scoped-edit-gate-{}",
        std::process::id() as u64 * 31 + 12
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/AGENTS.md"),
        "Run the focused test before editing.",
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n-old\n+new\n*** End Patch";
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call("c1", "apply_patch", serde_json::json!({"patch": patch})),
        assistant_text("stopped after reading rules"),
    ]));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        4,
    );
    let mut events = Vec::new();

    executor
        .run(
            "rename old",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
        "old\n"
    );
    assert!(
        events.iter().any(|event| {
            matches!(event,
            AgentEvent::ToolResult { id, is_error: true, preview, .. }
                if id == "c1" && preview.contains("AGENTS.md"))
        }),
        "events: {events:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Replays scripted responses while recording every request's transcript, so
/// tests can assert exactly what the model saw.
struct RequestRecordingRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl RequestRecordingRuntime {
    fn new(responses: Vec<ModelResponse>) -> (Self, Arc<Mutex<Vec<Vec<Message>>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                requests: requests.clone(),
            },
            requests,
        )
    }
}

#[async_trait]
impl ModelRuntime for RequestRecordingRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unimplemented!()
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        self.requests.lock().unwrap().push(request.messages.clone());
        let response = self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })?;
        let mut events: Vec<Result<ModelEvent, ModelError>> = Vec::new();
        events.push(Ok(ModelEvent::MessageStarted {
            request_id: response.request_id.clone(),
        }));
        for part in &response.message.content {
            match part {
                ContentPart::Text { text } => {
                    events.push(Ok(ModelEvent::TextDelta {
                        delta: text.clone(),
                    }));
                }
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }));
                }
                _ => {}
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, _m: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

const AUDIT_MARKER: &str = "Treat completion as unproven";

#[tokio::test]
async fn completion_audit_restates_task_and_does_not_duplicate_messages() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-audit-{}",
        std::process::id() as u64 * 29 + 8
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let (runtime, requests) = RequestRecordingRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
            }),
        ),
        assistant_text("All done."),
        assistant_text("Confirmed, tests pass."),
    ]);

    let executor = Executor::new(
        Arc::new(runtime),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_structure(false, true);

    let outcome = executor
        .run(
            "Add an `added` function to lib.rs",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(outcome.rounds, 3, "one audit, then the fuse accepts");

    let requests = requests.lock().unwrap();
    let last = requests.last().expect("at least one request");
    // The audit nudge restates the original task so the model re-verifies
    // against the real objective, not a shrunken memory of it.
    let audit = last
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.text_content())
        .find(|t| t.contains(AUDIT_MARKER))
        .expect("audit nudge must be injected after an unverified finish");
    assert!(
        audit.contains("Add an `added` function"),
        "audit must restate the objective: {audit}"
    );
    assert!(
        audit.contains("Do not redefine the task"),
        "audit must forbid shrinking the task: {audit}"
    );
    assert!(
        audit.contains("not your memory"),
        "audit must demand evidence from the current state: {audit}"
    );
    // The refused completion must appear exactly once in the transcript the
    // model sees next (no duplicated assistant message).
    let dones = last
        .iter()
        .filter(|m| m.role == Role::Assistant && m.text_content() == "All done.")
        .count();
    assert_eq!(dones, 1, "the refused completion must not be duplicated");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn completion_audit_requires_verification_after_the_last_edit() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-stale-{}",
        std::process::id() as u64 * 31 + 9
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // A successful command BEFORE the edit is stale evidence: the edit
    // invalidates it, so the unverified finish must still be audited.
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "run_command",
            serde_json::json!({"program": "echo", "args": ["ok"]}),
        ),
        assistant_tool_call(
            "c2",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
            }),
        ),
        assistant_text("Done."),
        assistant_text("Done again."),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_structure(false, true);

    let outcome = executor
        .run(
            "Add an `added` function to lib.rs",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(
        outcome.rounds, 4,
        "verification that predates the last edit must not satisfy the gate"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn completion_audit_accepts_fresh_verification() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-fresh-{}",
        std::process::id() as u64 * 37 + 10
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let (runtime, requests) = RequestRecordingRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
            }),
        ),
        assistant_text("Done."),
        assistant_tool_call(
            "c2",
            "run_command",
            // A verification-class program (checked by basename); --version
            // keeps the test hermetic.
            serde_json::json!({"program": "cargo", "args": ["--version"]}),
        ),
        assistant_text("All verified."),
    ]);

    let executor = Executor::new(
        Arc::new(runtime),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_structure(false, true);

    let outcome = executor
        .run(
            "Add an `added` function to lib.rs",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(outcome.rounds, 4);
    assert_eq!(outcome.final_text, "All verified.");

    // Exactly one audit: verification after the edit satisfies the gate.
    let requests = requests.lock().unwrap();
    let last = requests.last().unwrap();
    let audits = last
        .iter()
        .filter(|m| m.role == Role::User && m.text_content().contains(AUDIT_MARKER))
        .count();
    assert_eq!(audits, 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn completion_audit_reengages_only_after_progress() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-reaudit-{}",
        std::process::id() as u64 * 41 + 11
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Finish → audit #1 → the model works (read_file) but still doesn't verify
    // → finish → audit #2 (progress happened) → finish → fuse accepts.
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
            }),
        ),
        assistant_text("Done."),
        assistant_tool_call("c2", "read_file", serde_json::json!({"path": "src/lib.rs"})),
        assistant_text("Done."),
        assistant_text("Final answer."),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_structure(false, true);

    let outcome = executor
        .run(
            "Add an `added` function to lib.rs",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(
        outcome.rounds, 5,
        "a second audit fires only because the model made progress in between"
    );
    assert_eq!(outcome.final_text, "Final answer.");

    std::fs::remove_dir_all(&dir).ok();
}

/// One assistant message that requests several tool calls at once.
fn assistant_tool_calls(calls: Vec<(&str, &str, serde_json::Value)>) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: calls
                .into_iter()
                .map(|(id, name, args)| ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new(id),
                        name: name.to_string(),
                        arguments: args,
                    },
                })
                .collect(),
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

/// A read-only tool that blocks on a shared 2-party barrier before returning.
/// Two of these can only both complete if the executor runs them concurrently;
/// under serial execution the first would wait forever for the second to start.
struct BarrierTool {
    barrier: Arc<tokio::sync::Barrier>,
}

#[async_trait]
impl Tool for BarrierTool {
    fn name(&self) -> &'static str {
        "barrier"
    }
    fn description(&self) -> &'static str {
        "waits on a shared barrier (test-only)"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": true })
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    fn supports_parallel(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.barrier.wait().await;
        Ok(ToolOutput::ok("released"))
    }
}

#[tokio::test]
async fn parallel_safe_tools_in_one_round_run_concurrently() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-parallel-{}",
        std::process::id() as u64 * 31 + 9
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(BarrierTool {
        barrier: Arc::new(tokio::sync::Barrier::new(2)),
    }));
    let registry = Arc::new(registry);

    // One round asks for two barrier calls; they only both finish if run at the
    // same time. A serial loop would deadlock, so guard the whole run with a
    // timeout — a timeout here is the failing (red) signal.
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_calls(vec![
            ("c1", "barrier", serde_json::json!({})),
            ("c2", "barrier", serde_json::json!({})),
        ]),
        assistant_text("both released"),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    );

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        executor.run("go", &mut |_| {}, &mut NoopSink, CancellationToken::new()),
    )
    .await
    .expect("two parallel-safe tools must run concurrently, not deadlock serially")
    .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn multiple_read_tools_are_all_run_and_results_kept_in_call_order() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-order-{}",
        std::process::id() as u64 * 31 + 10
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Three read-only tools in one round, then finish.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = Arc::new(RecordingRuntime {
        responses: Mutex::new(VecDeque::from(vec![
            assistant_tool_calls(vec![
                ("c1", "read_file", serde_json::json!({"path": "src/lib.rs"})),
                ("c2", "list_files", serde_json::json!({"path": "."})),
                ("c3", "grep", serde_json::json!({"pattern": "fn"})),
            ]),
            assistant_text("done"),
        ])),
        seen: seen.clone(),
    });

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    );
    executor
        .run(
            "look around",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // The second request carries the tool results of the first round. Their
    // call_ids must appear in the original call order.
    let requests = seen.lock().unwrap();
    let tool_msg = requests[1]
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("a tool-result message");
    let ids: Vec<String> = tool_msg
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolResult { result } => Some(result.call_id.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        vec!["c1".to_string(), "c2".to_string(), "c3".to_string()],
        "all three read-only tools ran and their results stayed in call order"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn goal_mode_quiet_exhaustion_returns_stalled() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-goalstall-{}",
        std::process::id() as u64 * 53 + 11
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // The model goes quiet every round and never calls update_goal: three
    // nudges, then a fourth quiet round. That is a stall, not a completion.
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_text("I think it's done."),
        assistant_text("Still done."),
        assistant_text("Done, really."),
        assistant_text("Done."),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_goal_mode(true);

    let outcome = executor
        .run(
            "do the task",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        outcome.stop_reason,
        StopReason::Stalled,
        "quiet-nudge exhaustion must not be reported as a successful completion"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn update_goal_missing_status_is_rejected_and_retried() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-goalnostatus-{}",
        std::process::id() as u64 * 59 + 13
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Round 1: update_goal without a status — must be rejected, not treated as
    // complete. Round 2: an explicit resolution.
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"summary": "did things"}),
        ),
        assistant_tool_call(
            "g2",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "did things, explicitly"}),
        ),
    ]));

    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_goal_mode(true);

    let mut events = Vec::new();
    let outcome = executor
        .run(
            "do the task",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(
        runtime.recorded_requests().len(),
        2,
        "the malformed resolution must be fed back for a retry"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResult { id, is_error, .. } if id == "g1" && *is_error
        )),
        "the status-less update_goal should surface as an errored tool result"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn echo_command_is_not_completion_evidence() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-echoevidence-{}",
        std::process::id() as u64 * 61 + 17
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn old() {}\n").unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Edit, then "verify" with a plain echo, then declare done. The echo is
    // not verification evidence, so the first completion must still be
    // refused by the evidence gate (one nudge round).
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
            }),
        ),
        assistant_tool_call(
            "c2",
            "run_command",
            serde_json::json!({"program": "echo", "args": ["hi"]}),
        ),
        assistant_text("All done."),
        assistant_text("Confirmed."),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_structure(false, true);

    let outcome = executor
        .run(
            "Add an `added` function to lib.rs",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(
        outcome.rounds, 4,
        "an echo run must not satisfy the completion-evidence gate"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn cargo_command_is_completion_evidence() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-cargoevidence-{}",
        std::process::id() as u64 * 67 + 19
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn old() {}\n").unwrap();

    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::FullAccess);
    let registry = Arc::new(default_registry());

    // This test covers evidence classification, not OS sandbox integration.
    // Use Cargo's absolute build-time path in FullAccess mode; unit-test
    // ToolContext intentionally has no inherited PATH. Confinement has
    // dedicated platform canaries in leveler-execution.
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
            }),
        ),
        assistant_tool_call(
            "c2",
            "run_command",
            serde_json::json!({"program": env!("CARGO"), "args": ["--version"]}),
        ),
        assistant_text("All done."),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_structure(false, true);

    let outcome = executor
        .run(
            "Add an `added` function to lib.rs",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(
        outcome.rounds, 3,
        "a verification-class command satisfies the evidence gate"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn command_budget_is_enforced_before_the_call() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-cmdbudget-{}",
        std::process::id() as u64 * 71 + 23
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Two command rounds against a 1-command budget: the second must be
    // refused without running and the run must stop as budget-exhausted.
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "run_command",
            serde_json::json!({"program": "echo", "args": ["one"]}),
        ),
        assistant_tool_call(
            "c2",
            "run_command",
            serde_json::json!({"program": "echo", "args": ["two"]}),
        ),
        assistant_text("should never be requested"),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_step_limits(leveler_agent::StepLimits {
        max_commands: Some(1),
        ..Default::default()
    });

    let mut events = Vec::new();
    let outcome = executor
        .run(
            "run things",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::BudgetExhausted);
    assert_eq!(outcome.rounds, 2);
    assert!(
        outcome.final_text.contains("command"),
        "the reason must name the exhausted budget: {}",
        outcome.final_text
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResult { id, is_error, .. } if id == "c2" && *is_error
        )),
        "the over-budget command must surface as a refused tool result"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn modified_files_budget_blocks_further_edits() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-filebudget-{}",
        std::process::id() as u64 * 73 + 29
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(dir.join("src/b.rs"), "pub fn b() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/a.rs\n pub fn a() {}\n+pub fn a2() {}\n*** End Patch"
            }),
        ),
        assistant_tool_call(
            "c2",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/b.rs\n pub fn b() {}\n+pub fn b2() {}\n*** End Patch"
            }),
        ),
        assistant_text("should never be requested"),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_step_limits(leveler_agent::StepLimits {
        max_modified_files: Some(1),
        ..Default::default()
    });

    let outcome = executor
        .run(
            "edit files",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::BudgetExhausted);
    assert!(
        outcome.final_text.contains("file"),
        "the reason must name the exhausted budget: {}",
        outcome.final_text
    );
    let b = std::fs::read_to_string(dir.join("src/b.rs")).unwrap();
    assert_eq!(b, "pub fn b() {}\n", "the over-budget edit must not land");

    std::fs::remove_dir_all(&dir).ok();
}

/// One apply_patch that touches 2 new files must not land when max_modified_files=1
/// (residual was 1; multi-file patch would oversell the task budget).
#[tokio::test]
async fn single_patch_cannot_exceed_task_file_budget() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-multifile-budget-{}",
        std::process::id() as u64 * 73 + 41
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(dir.join("src/b.rs"), "pub fn b() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/a.rs\n pub fn a() {}\n+pub fn a2() {}\n*** End Patch\n*** Update File: src/b.rs\n pub fn b() {}\n+pub fn b2() {}\n*** End Patch"
            }),
        ),
        assistant_text("should not need a second round"),
    ]));

    let mut events = Vec::new();
    let outcome = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_step_limits(leveler_agent::StepLimits {
        max_modified_files: Some(1),
        ..Default::default()
    })
    .run(
        "edit two files at once",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let refused = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolResult {
                is_error: true,
                preview,
                ..
            } if preview.contains("file") || preview.contains("budget")
        )
    });
    assert!(
        refused || outcome.stop_reason == StopReason::BudgetExhausted,
        "multi-file patch must hit task file budget; outcome={outcome:?} events={events:?}"
    );
    let a = std::fs::read_to_string(dir.join("src/a.rs")).unwrap();
    let b = std::fs::read_to_string(dir.join("src/b.rs")).unwrap();
    assert_eq!(
        a, "pub fn a() {}\n",
        "over-budget multi-file patch must not write a"
    );
    assert_eq!(
        b, "pub fn b() {}\n",
        "over-budget multi-file patch must not write b"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn duration_budget_stops_the_run_between_rounds() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-timebudget-{}",
        std::process::id() as u64 * 79 + 31
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "run_command",
            serde_json::json!({"program": "sleep", "args": ["0.05"]}),
        ),
        assistant_text("should never be requested"),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_step_limits(leveler_agent::StepLimits {
        max_duration: Some(std::time::Duration::from_millis(10)),
        ..Default::default()
    });

    let outcome = executor
        .run(
            "run things",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::BudgetExhausted);
    assert_eq!(outcome.rounds, 1, "round 2 must never be requested");
    assert!(
        outcome.final_text.contains("duration") || outcome.final_text.contains("time"),
        "the reason must name the exhausted budget: {}",
        outcome.final_text
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn replace_outside_write_allowlist_is_rejected_before_running() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-replallow-{}",
        std::process::id() as u64 * 83 + 37
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("src/ok.rs"), "pub fn ok() {}\n").unwrap();
    std::fs::write(dir.join("docs/out.md"), "keep me\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "replace",
            serde_json::json!({"path": "docs/out.md", "old": "keep me", "new": "changed"}),
        ),
        assistant_text("giving up"),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_write_allowlist(Some(vec!["src".to_string()]));

    let mut events = Vec::new();
    executor
        .run(
            "edit docs",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.join("docs/out.md")).unwrap(),
        "keep me\n",
        "the out-of-allowlist replace must not land"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResult { id, is_error, preview, .. }
                if id == "c1" && *is_error && preview.contains("src")
        )),
        "the rejection must name the allowed paths: {events:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn apply_patch_outside_write_allowlist_is_rejected_before_running() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-patchallow-{}",
        std::process::id() as u64 * 89 + 41
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs/out.md"), "keep me\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "apply_patch",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: docs/out.md\n keep me\n+added\n*** End Patch"
            }),
        ),
        assistant_text("giving up"),
    ]));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_write_allowlist(Some(vec!["src".to_string()]));

    let outcome = executor
        .run(
            "edit docs",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.join("docs/out.md")).unwrap(),
        "keep me\n",
        "the out-of-allowlist patch must not land: {}",
        outcome.final_text
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn length_finished_text_is_continued_before_the_turn_finishes() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-length-continuation-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_text_finished("first half", FinishReason::Length),
        assistant_text("second half"),
    ]));
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(default_registry()),
        ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        4,
    );

    let outcome = executor
        .run(
            "give a complete answer",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(runtime.recorded_requests().len(), 2);
    assert!(outcome.final_text.contains("first half"));
    assert!(outcome.final_text.contains("second half"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn length_finished_tool_call_is_never_executed() {
    let dir = std::env::temp_dir().join(format!("leveler-length-tool-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("readme.md"), "secret\n").unwrap();
    // Truncated tool calls get bounded re-issue nudges (2); a model that keeps
    // overrunning past the cap still fails as Truncated, and none of the
    // partial calls ever executes.
    let truncated = || {
        let mut call =
            assistant_tool_call("c1", "read_file", serde_json::json!({"path": "readme.md"}));
        call.finish_reason = FinishReason::Length;
        call
    };
    let runtime = Arc::new(MockRuntime::new(vec![
        truncated(),
        truncated(),
        truncated(),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        4,
    );
    let mut events = Vec::new();

    let error = executor
        .run(
            "read the file",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        leveler_agent::AgentError::Model(ref error)
            if error.kind == leveler_model::ModelErrorKind::Truncated
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCall { .. })),
        "a length-truncated call must never reach the tool executor: {events:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn content_filtered_text_is_not_reported_as_completed() {
    let dir = std::env::temp_dir().join(format!("leveler-content-filter-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let runtime = Arc::new(MockRuntime::new(vec![assistant_text_finished(
        "partial",
        FinishReason::ContentFilter,
    )]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        4,
    );

    let result = executor
        .run(
            "answer",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await;

    assert!(
        result.is_err(),
        "filtered output must not complete: {result:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn stream_eof_without_terminal_event_is_interrupted_not_completed() {
    let dir = std::env::temp_dir().join(format!("leveler-stream-eof-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let executor = Executor::new(
        Arc::new(UnterminatedRuntime),
        Arc::new(default_registry()),
        ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        2,
    );

    let error = executor
        .run(
            "answer",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        leveler_agent::AgentError::Model(ref error)
            if error.kind == leveler_model::ModelErrorKind::StreamInterrupted
    ));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn natural_stop_is_an_answer_end_not_proof_that_the_task_completed() {
    let dir = std::env::temp_dir().join(format!("leveler-answer-end-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let runtime = Arc::new(MockRuntime::new(vec![assistant_text(
        "Here is the answer.",
    )]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        2,
    );

    let outcome = executor
        .run(
            "explain this",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn tool_heavy_answer_is_repaired_when_completeness_audit_finds_a_missing_branch() {
    let dir = std::env::temp_dir().join(format!("leveler-answer-audit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("flow.txt"), "worker cleanup\n").unwrap();
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call("c1", "read_file", serde_json::json!({"path": "flow.txt"})),
        assistant_text("flow stops at ErrNeedsHuman"),
        assistant_text(r#"{"complete":false,"missing":["worker cleanup"]}"#),
        assistant_text("; then worker cleanup completes the task"),
        assistant_text(r#"{"complete":true,"missing":[]}"#),
    ]));
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(default_registry()),
        ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        6,
    )
    .with_answer_audit(true);

    let outcome = executor
        .run(
            "explain the complete flow",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(runtime.recorded_requests().len(), 5);
    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(outcome.final_text.contains("ErrNeedsHuman"));
    assert!(outcome.final_text.contains("worker cleanup completes"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn unavailable_completeness_audit_does_not_downgrade_a_finished_answer() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-answer-audit-unavailable-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("flow.txt"), "worker cleanup\n").unwrap();
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call("c1", "read_file", serde_json::json!({"path": "flow.txt"})),
        assistant_text("The flow and its terminal state are fully explained."),
        assistant_text("audit service returned non-JSON"),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        4,
    )
    .with_answer_audit(true);

    let outcome = executor
        .run(
            "explain the complete flow",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(outcome.final_text.contains("fully explained"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn heuristic_audit_cannot_mark_a_repaired_read_only_answer_incomplete() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-answer-audit-advisory-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("flow.txt"), "worker cleanup\n").unwrap();
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call("c1", "read_file", serde_json::json!({"path": "flow.txt"})),
        assistant_text("flow stops at ErrNeedsHuman"),
        assistant_text(r#"{"complete":false,"missing":["worker cleanup"]}"#),
        assistant_text("; then worker cleanup completes the task"),
        assistant_text(r#"{"complete":false,"missing":["more context"]}"#),
        assistant_text("; additional context is now covered"),
        assistant_text(r#"{"complete":false,"missing":["subjective preference"]}"#),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        6,
    )
    .with_answer_audit(true);

    let outcome = executor
        .run(
            "explain the complete flow",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(outcome.final_text.contains("worker cleanup completes"));
    std::fs::remove_dir_all(&dir).ok();
}

/// The summary a scripted model returns when asked to compact the transcript.
const COMPACT_SUMMARY_MARKER: &str =
    "HANDOFF-SUMMARY: inspected src/lib.rs; the next step is to edit `old`";

/// A runtime that forces compaction every round (huge reported context), serves
/// the summarization request through `generate`, and records whether a later
/// streamed request actually carried the model-written summary.
struct SummarizingCompactRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    summary_calls: Arc<std::sync::atomic::AtomicUsize>,
    saw_summary: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl ModelRuntime for SummarizingCompactRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _c: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.summary_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(assistant_text(COMPACT_SUMMARY_MARKER))
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _c: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        if request
            .messages
            .iter()
            .any(|m| m.text_content().contains(COMPACT_SUMMARY_MARKER))
        {
            self.saw_summary
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::new(leveler_model::ModelErrorKind::Other, "drained"))?;
        let mut events: Vec<Result<ModelEvent, ModelError>> =
            vec![Ok(ModelEvent::MessageStarted {
                request_id: response.request_id.clone(),
            })];
        for part in &response.message.content {
            match part {
                ContentPart::Text { text } => events.push(Ok(ModelEvent::TextDelta {
                    delta: text.clone(),
                })),
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }))
                }
                _ => {}
            }
        }
        events.push(Ok(ModelEvent::UsageUpdated {
            usage: TokenUsage {
                input_tokens: 500_000,
                output_tokens: 100,
                cached_input_tokens: 0,
            },
        }));
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, _m: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

/// Auto-compaction must SUMMARIZE the elided middle with the model, not drop it.
/// Dropping leaves the run with a bare breadcrumb (a file list), so every
/// decision, dead end, and finding from those rounds is lost and the model
/// redoes work it already did.
#[tokio::test]
async fn auto_compaction_summarizes_the_elided_middle_with_the_model() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-compact-summary-{}",
        std::process::id() as u64 * 19 + 7
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    for i in 0..16 {
        std::fs::write(
            dir.join(format!("src/f{i}.rs")),
            format!("pub fn f{i}() {{}}\n"),
        )
        .unwrap();
    }
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Distinct reads so the identical-call loop guard never refuses a round.
    let mut responses: Vec<ModelResponse> = (0..16)
        .map(|i| {
            assistant_tool_call(
                &format!("c{i}"),
                "read_file",
                serde_json::json!({"path": format!("src/f{i}.rs")}),
            )
        })
        .collect();
    responses.push(assistant_text("done reading"));

    let summary_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let saw_summary = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runtime = Arc::new(SummarizingCompactRuntime {
        responses: Mutex::new(VecDeque::from(responses)),
        summary_calls: summary_calls.clone(),
        saw_summary: saw_summary.clone(),
    });

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        20,
    )
    .with_context_budget(1000);

    let outcome = executor
        .run(
            "read the file many times",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(
        summary_calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "compaction must ask the model for a handoff summary of the elided rounds"
    );
    assert!(
        saw_summary.load(std::sync::atomic::Ordering::SeqCst),
        "the model-written summary must be carried into the compacted transcript"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Records whether the executor fell back to asking a human.
struct RecordingClarifier {
    asked: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl leveler_agent::Clarifier for RecordingClarifier {
    async fn clarify(&self, request: &leveler_agent::ClarificationRequest) -> String {
        self.asked.lock().unwrap().push(request.tool.clone());
        "拒绝".to_string()
    }
}

#[tokio::test]
async fn dollar_skill_mention_injects_full_body_into_turn() {
    // S1: `$demo` must put SKILL.md body into the model transcript without load_skill.
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-skill-inj-{}",
        std::process::id() as u64 * 53 + 3
    ));
    std::fs::create_dir_all(dir.join(".leveler/skills/demo")).unwrap();
    std::fs::write(
        dir.join(".leveler/skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: Demo.\n---\n\nUNIQUE_INJECT_BODY_7788\n",
    )
    .unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(vec![assistant_text("done")]));
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    );
    let outcome = executor
        .run(
            "please follow $demo carefully",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Answered);
    let requests = runtime.recorded_requests();
    assert!(!requests.is_empty());
    let all_text: String = requests[0]
        .messages
        .iter()
        .map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n---\n");
    assert!(
        all_text.contains("UNIQUE_INJECT_BODY_7788"),
        "skill body must be turn-injected: {all_text}"
    );
    assert!(
        all_text.contains("SKILL TURN INJECTION") || all_text.contains("Skill: demo"),
        "injection header: {all_text}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn unknown_dollar_skill_does_not_inject_fake_body() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-skill-miss-{}",
        std::process::id() as u64 * 59 + 5
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(vec![assistant_text("ok")]));
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    );
    executor
        .run(
            "use $no_such_skill_xyz",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let all_text: String = runtime.recorded_requests()[0]
        .messages
        .iter()
        .map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_text.contains("Unknown skill") || all_text.contains("no_such_skill_xyz"),
        "unknown mention should be reported safely: {all_text}"
    );
    // No invented skill instructions section with a real body package for a known skill.
    assert!(!all_text.contains("# Skill: no_such_skill_xyz\n"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn request_user_input_routes_to_clarifier_like_ask_user() {
    // The primary name must hit the same Clarifier path as legacy ask_user.
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-rui-{}",
        std::process::id() as u64 * 41 + 7
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let responses = vec![
        assistant_tool_call(
            "q1",
            "request_user_input",
            serde_json::json!({
                "question": "Ship now or open a PR?",
                "options": ["ship", "PR"]
            }),
        ),
        assistant_text("got it"),
    ];
    let runtime = Arc::new(MockRuntime::new(responses));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_clarifier(Arc::new(RecordingClarifier {
        asked: asked.clone(),
    }));

    let outcome = executor
        .run(
            "how should we ship?",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    let tools = asked.lock().unwrap().clone();
    assert_eq!(
        tools,
        vec!["request_user_input".to_string()],
        "clarifier must see the primary tool name on the request"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn legacy_ask_user_still_routes_to_clarifier() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-ask-{}",
        std::process::id() as u64 * 43 + 9
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let responses = vec![
        assistant_tool_call("q1", "ask_user", serde_json::json!({"question": "ok?"})),
        assistant_text("done"),
    ];
    let runtime = Arc::new(MockRuntime::new(responses));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_clarifier(Arc::new(RecordingClarifier {
        asked: asked.clone(),
    }));

    let outcome = executor
        .run("ok?", &mut |_| {}, &mut NoopSink, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(asked.lock().unwrap().as_slice(), ["ask_user"]);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn auto_approve_grants_a_network_permission_without_asking_a_human() {
    // `--auto-approve` exists so the interactive UI can be driven unattended
    // (its own help says so). A network permission request that still stops to
    // ask a human deadlocks exactly the run the flag promises to enable — the
    // decision belongs to the approver, which auto-approve has already answered.
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-netperm-{}",
        std::process::id() as u64 * 31 + 11
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let responses = vec![
        assistant_tool_call(
            "p1",
            "request_permissions",
            serde_json::json!({"action": "go mod tidy", "reason": "fetch deps"}),
        ),
        assistant_text("done"),
    ];
    let runtime = Arc::new(MockRuntime::new(responses));

    let asked = Arc::new(Mutex::new(Vec::new()));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_approver(Arc::new(leveler_execution::AutoApprove))
    .with_clarifier(Arc::new(RecordingClarifier {
        asked: asked.clone(),
    }));

    let outcome = executor
        .run(
            "tidy the module",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert!(
        asked.lock().unwrap().is_empty(),
        "auto-approve must decide the permission itself, not stall on a human: \
         clarifier was asked for {:?}",
        asked.lock().unwrap()
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn an_editing_turn_that_skips_a_requested_deliverable_is_audited_too() {
    // The completeness audit used to run only on turns that changed NO files, so
    // exactly the turns that DO work — the ones with a list of deliverables to
    // get through — were the ones never checked. Live: a turn was asked for five
    // things, wrote the tests/CI for four, silently skipped "commit and push",
    // and still reported the task complete. Editing a file says nothing about
    // whether the request was finished.
    let dir = std::env::temp_dir().join(format!(
        "leveler-editing-audit-{}",
        std::process::id() as u64 * 37 + 13
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.txt"), "one\n").unwrap();

    let runtime = Arc::new(MockRuntime::new(vec![
        // The turn edits a file, then answers having done only part of the ask.
        assistant_tool_call(
            "c1",
            "replace",
            serde_json::json!({"path": "notes.txt", "old": "one", "new": "two"}),
        ),
        assistant_text("Updated notes.txt."),
        // The audit names what was left undone...
        assistant_text(r#"{"complete":false,"missing":["commit and push the change"]}"#),
        // ...the model finishes it, and the re-audit passes.
        assistant_text(" Committed and pushed."),
        assistant_text(r#"{"complete":true,"missing":[]}"#),
    ]));

    let executor = Executor::new(
        runtime.clone(),
        Arc::new(default_registry()),
        ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        6,
    )
    .with_answer_audit(true);

    let outcome = executor
        .run(
            "edit notes.txt, then commit and push",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        !outcome.modified_files.is_empty(),
        "precondition: this turn really did edit a file"
    );
    assert!(
        outcome.final_text.contains("Committed and pushed"),
        "the audit must catch the skipped deliverable on an editing turn and let \
         the model finish it; got: {}",
        outcome.final_text
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn answer_audit_is_off_by_default() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-no-audit-{}",
        std::process::id() as u64 * 31 + 99
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let executor = Executor::new(
        Arc::new(MockRuntime::new(vec![assistant_text("hi")])),
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        2,
    );
    assert!(
        !executor.answer_audit_enabled(),
        "K29: answer_audit must default off"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn simple_goal_does_not_seed_host_implicit_plan() {
    // Short tasks execute without a fake one-step checklist (product: plan only
    // when the model registers update_plan, or the complex-task gate requires it).
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-no-implicit-plan-{}",
        std::process::id() as u64 * 31 + 21
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(vec![assistant_tool_call(
        "g1",
        "update_goal",
        serde_json::json!({"status": "complete", "summary": "done"}),
    )]));
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        4,
    )
    .with_goal_mode(true);
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "rename the README title",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(runtime.requests.lock().unwrap().len(), 1);
    let plan_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::PlanUpdated { .. }))
        .collect();
    assert!(
        plan_events.is_empty(),
        "simple goal must not host-seed a plan shell: {plan_events:?}"
    );
    assert_eq!(outcome.metrics.plan_updated, 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn update_goal_complete_rejects_incomplete_model_todos() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-todo-gate-{}",
        std::process::id() as u64 * 31 + 22
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "update_plan",
            serde_json::json!({
                "plan": [
                    {"step": "a", "status": "in_progress"},
                    {"step": "b", "status": "pending"}
                ]
            }),
        ),
        assistant_tool_call(
            "c2",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "nope"}),
        ),
        assistant_tool_call(
            "c3",
            "update_plan",
            serde_json::json!({
                "plan": [
                    {"step": "a", "status": "completed"},
                    {"step": "b", "status": "in_progress"}
                ]
            }),
        ),
        assistant_tool_call(
            "c4",
            "update_plan",
            serde_json::json!({
                "plan": [
                    {"step": "a", "status": "completed"},
                    {"step": "b", "status": "completed"}
                ]
            }),
        ),
        assistant_tool_call(
            "c5",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "yes"}),
        ),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_goal_mode(true);
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "multi step work please",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    let refused = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolResult {
                name,
                is_error: true,
                preview,
                ..
            } if name == "update_goal" && preview.contains("incomplete")
        )
    });
    assert!(refused, "first complete must be refused; events={events:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Second bare `update_goal(complete)` without `override_incomplete_todos` must
/// still refuse incomplete ModelExplicit todos (no attempt-count auto-pass).
#[tokio::test]
async fn update_goal_second_bare_complete_still_refuses_incomplete_todos() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-todo-gate2-{}",
        std::process::id() as u64 * 31 + 24
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "update_plan",
            serde_json::json!({
                "plan": [
                    {"step": "a", "status": "pending"},
                    {"step": "b", "status": "pending"}
                ]
            }),
        ),
        assistant_tool_call(
            "c2",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "first"}),
        ),
        assistant_tool_call(
            "c3",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "second bare"}),
        ),
        // Explicit override is the only bypass.
        assistant_tool_call(
            "c4",
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": "forced",
                "override_incomplete_todos": true
            }),
        ),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_goal_mode(true);
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "multi step work please",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    let refused = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ToolResult {
                    name,
                    is_error: true,
                    preview,
                    ..
                } if name == "update_goal"
                    && (preview.contains("incomplete") || preview.contains("override_incomplete"))
            )
        })
        .count();
    assert!(
        refused >= 2,
        "both bare completes must refuse; events={events:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Goal text-only stall increments no-progress once per drive so Engine
/// continue is capped after consecutive stalls.
#[tokio::test]
async fn goal_text_only_quiet_increments_no_progress_and_stalls() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-quiet-stall-{}",
        std::process::id() as u64 * 31 + 25
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    // Exhaust quiet nudges → Stalled; one drive → streak=1 (continue still allowed).
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_text("still thinking 1"),
        assistant_text("still thinking 2"),
        assistant_text("still thinking 3"),
        assistant_text("still thinking 4"),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        20,
    )
    .with_goal_mode(true);
    let outcome = executor
        .run(
            "do the work",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Stalled);
    assert_eq!(
        outcome.progress.no_progress_streak, 1,
        "one stalled drive → one no-progress tick: {:?}",
        outcome.progress
    );
    assert!(
        outcome
            .progress
            .allows_engine_continue(leveler_lifecycle::ProgressCaps::default()),
        "first stall may still continue once"
    );

    // Second stalled drive with seeded streak=1 must hit the continue cap.
    let mut seeded = outcome.progress.clone();
    seeded.note_no_progress_round(99);
    assert!(
        !seeded.allows_engine_continue(leveler_lifecycle::ProgressCaps::default()),
        "two consecutive stall ticks block engine continue"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Epoch command budget is seeded from ProgressLedger across continues.
#[tokio::test]
async fn epoch_command_budget_uses_seeded_cumulative_commands() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-cmd-budget-{}",
        std::process::id() as u64 * 31 + 26
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::FullAccess);
    let runtime = Arc::new(MockRuntime::new(vec![assistant_tool_call(
        "c1",
        "run_command",
        serde_json::json!({"program": "true", "args": []}),
    )]));
    let progress = leveler_lifecycle::ProgressLedger {
        cumulative_commands: 2,
        ..Default::default()
    };
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    )
    .with_step_limits(leveler_agent::StepLimits {
        max_commands: Some(2),
        ..Default::default()
    })
    .with_seeded_progress(progress);
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "run something",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    // Already at epoch command cap (2/2): the run_command must be refused.
    let refused = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolResult {
                is_error: true,
                preview,
                ..
            } if preview.contains("command budget")
        )
    });
    assert!(
        refused || outcome.stop_reason == StopReason::BudgetExhausted,
        "seeded epoch commands must count toward max_commands; events={events:?} outcome={outcome:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// First drive runs shell tools; ledger must include those commands so a
/// second drive/resume with the same max_commands trips on the epoch total.
#[tokio::test]
async fn tool_phase_command_spend_survives_into_seeded_second_drive() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-cmd-epoch2-{}",
        std::process::id() as u64 * 31 + 27
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::FullAccess);
    let registry = Arc::new(default_registry());

    // Drive 1: one successful true command, then stop with text.
    let runtime1 = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "run_command",
            serde_json::json!({"program": "true", "args": []}),
        ),
        assistant_text("did one command"),
    ]));
    let ex1 = Executor::new(
        runtime1,
        registry.clone(),
        tool_context.clone(),
        ModelRef::new("mock", "m"),
        10,
    )
    .with_step_limits(leveler_agent::StepLimits {
        max_commands: Some(2),
        ..Default::default()
    });
    let out1 = ex1
        .run(
            "run a command",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        out1.progress.cumulative_commands >= 1,
        "tool-phase command must be on ledger after drive exit: {:?}",
        out1.progress
    );
    let after_first = out1.progress.cumulative_commands;

    // Drive 2: seed ledger; one more command should hit max_commands=2.
    let runtime2 = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c2",
            "run_command",
            serde_json::json!({"program": "true", "args": []}),
        ),
        assistant_tool_call(
            "c3",
            "run_command",
            serde_json::json!({"program": "true", "args": []}),
        ),
        assistant_text("should not need third"),
    ]));
    let mut events = Vec::new();
    let ex2 = Executor::new(
        runtime2,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_step_limits(leveler_agent::StepLimits {
        max_commands: Some(2),
        ..Default::default()
    })
    .with_seeded_progress(out1.progress);
    let out2 = ex2
        .run(
            "run more",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        out2.progress.cumulative_commands >= after_first,
        "second drive must keep/seed first-drive commands: first={after_first} second={:?}",
        out2.progress
    );
    let budget_hit = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolResult {
                is_error: true,
                preview,
                ..
            } if preview.contains("command budget")
        )
    }) || out2.stop_reason == StopReason::BudgetExhausted;
    assert!(
        budget_hit,
        "epoch max_commands=2 must trip after first-drive spend+second drive tools; \
         first_cmds={after_first} final={:?} events={events:?}",
        out2.progress
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn complex_task_allows_readonly_explore_before_plan() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-explore-{}",
        std::process::id() as u64 * 31 + 23
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), "hello\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call("c1", "read_file", serde_json::json!({"path": "note.txt"})),
        assistant_tool_call(
            "c2",
            "update_plan",
            serde_json::json!({
                "plan": [
                    {"step": "inspect", "status": "completed"},
                    {"step": "edit", "status": "in_progress"}
                ]
            }),
        ),
        assistant_text("done"),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    )
    .with_structure(true, false);
    let mut events = Vec::new();
    executor
        .run(
            "1. inspect the note\n2. change something carefully",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let read_ok = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolResult {
                id,
                is_error: false,
                ..
            } if id == "c1"
        )
    });
    assert!(
        read_ok,
        "read_file before plan must succeed; events={events:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn resume_seeded_plan_is_used_by_todo_gate() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-seed-plan-{}",
        std::process::id() as u64 * 31 + 24
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let seeded = leveler_agent::PlanState::from_model_explicit(vec![leveler_agent::PlanStep {
        step: "still open".into(),
        status: "pending".into(),
        id: None,
        origin: leveler_agent::PlanOrigin::ModelExplicit,
    }])
    .unwrap();
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "early"}),
        ),
        assistant_tool_call(
            "c2",
            "update_plan",
            serde_json::json!({
                "plan": [{"step": "still open", "status": "in_progress"}]
            }),
        ),
        assistant_tool_call(
            "c3",
            "update_plan",
            serde_json::json!({
                "plan": [{"step": "still open", "status": "completed"}]
            }),
        ),
        assistant_tool_call(
            "c4",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "ok"}),
        ),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    )
    .with_goal_mode(true)
    .with_seeded_plan(seeded);
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "finish the leftover work",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolResult {
            name,
            is_error: true,
            preview,
            ..
        } if name == "update_goal" && preview.contains("incomplete")
    )));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn delivery_gate_blocks_complete_without_mutation() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-delivery-gate-{}",
        std::process::id() as u64 * 31 + 30
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "done without edits"}),
        ),
        assistant_tool_call(
            "g2",
            "update_goal",
            serde_json::json!({"status": "blocked", "summary": "cannot complete"}),
        ),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        6,
    )
    .with_goal_mode(true)
    .with_work_profile(leveler_agent::WorkProfile::Delivery);
    assert!(executor.delivery_gate_enabled());
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "fix the login bug",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Blocked);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolResult {
            name,
            is_error: true,
            preview,
            ..
        } if name == "update_goal" && preview.contains("mutation")
    )));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn delivery_complete_step_requires_fresh_verify_evidence() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-complete-step-{}",
        std::process::id() as u64 * 31 + 40
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "p1",
            "update_plan",
            serde_json::json!({
                "plan": [
                    {"step": "edit", "status": "in_progress"},
                    {"step": "verify", "status": "pending"}
                ]
            }),
        ),
        assistant_tool_call(
            "cs1",
            "complete_step",
            serde_json::json!({
                "step_id": "edit",
                "summary": "edited",
                "evidence_ref": "missing-id"
            }),
        ),
        assistant_tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "blocked", "summary": "need evidence"}),
        ),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        12,
    )
    .with_goal_mode(true)
    .with_structure(true, false)
    .with_work_profile(leveler_agent::WorkProfile::Delivery);
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "1. edit the file\n2. verify the change",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let stale_refused = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolResult {
                id,
                is_error: true,
                preview,
                ..
            } if id == "cs1" && (preview.contains("stale") || preview.contains("missing") || preview.contains("evidence"))
        )
    });
    assert!(stale_refused, "stale complete_step must fail: {events:?}");
    assert_eq!(outcome.stop_reason, StopReason::Blocked);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn delivery_complete_step_accepts_fresh_verify_evidence() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-complete-step-ok-{}",
        std::process::id() as u64 * 31 + 41
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::with_environment(
        workspace,
        PermissionProfile::FullAccess,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )),
    );
    let patch = "*** Begin Patch\n*** Update File: a.txt\n-old\n+new\n*** End Patch";
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "p1",
            "update_plan",
            serde_json::json!({
                "plan": [
                    {"step": "edit", "status": "in_progress"},
                    {"step": "verify", "status": "pending"}
                ]
            }),
        ),
        assistant_tool_call("m1", "apply_patch", serde_json::json!({"patch": patch})),
        assistant_tool_call(
            "v1",
            "run_command",
            serde_json::json!({"program": "cargo", "args": ["--version"]}),
        ),
        assistant_tool_call(
            "cs1",
            "complete_step",
            serde_json::json!({
                "step_id": "edit",
                "summary": "edited a.txt",
                "evidence_ref": "v1"
            }),
        ),
        assistant_tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "blocked", "summary": "stop after step receipt"}),
        ),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        16,
    )
    .with_goal_mode(true)
    .with_structure(true, false)
    .with_work_profile(leveler_agent::WorkProfile::Delivery)
    .with_approver(Arc::new(leveler_execution::AutoApprove));
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "1. edit the file\n2. verify the change",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let accepted = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolResult {
                id,
                is_error: false,
                preview,
                ..
            } if id == "cs1" && preview.contains("completed")
        )
    });
    assert!(accepted, "fresh complete_step must succeed: {events:?}");
    assert_eq!(outcome.stop_reason, StopReason::Blocked);
    std::fs::remove_dir_all(&dir).ok();
}

/// K36: AutoApprove + WorkspaceWrite must not persist consolidate_memory(auto_write).
#[tokio::test]
async fn auto_approve_blocks_consolidate_memory_auto_write() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-k36-consolidate-{}",
        std::process::id() as u64 * 31 + 50
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mem = dir.join("memory");
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context =
        ToolContext::new(workspace, PermissionProfile::Assisted).with_memory_root(&mem);
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "c1",
            "consolidate_memory",
            serde_json::json!({
                "transcript": "User preference: always use WorkspaceWrite for edits.",
                "auto_write": true
            }),
        ),
        assistant_text("done"),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        6,
    )
    .with_approver(Arc::new(leveler_execution::AutoApprove));
    let mut events = Vec::new();
    let _ = executor
        .run(
            "remember my prefs",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult {
                name,
                is_error: true,
                preview,
                ..
            } if name == "consolidate_memory"
                && (preview.contains("denied") || preview.contains("user"))
        )),
        "expected denial: {events:?}"
    );
    // No durable write under AutoApprove.
    if mem.join("active").exists() {
        let count = std::fs::read_dir(mem.join("active"))
            .map(|d| d.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        assert_eq!(count, 0, "active memory must stay empty under AutoApprove");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Gate intercepts emit GoalIntercepted + EvidenceLedgerUpdated for persistence.
#[tokio::test]
async fn goal_intercept_emits_ledger_events() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-intercept-events-{}",
        std::process::id() as u64 * 31 + 52
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call(
            "p1",
            "update_plan",
            serde_json::json!({
                "plan": [
                    {"step": "edit", "status": "in_progress"},
                    {"step": "verify", "status": "pending"}
                ]
            }),
        ),
        assistant_tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "too early"}),
        ),
        assistant_tool_call(
            "g2",
            "update_goal",
            serde_json::json!({"status": "blocked", "summary": "stop"}),
        ),
    ]));
    let executor = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    )
    .with_goal_mode(true)
    .with_goal_todo_gate(true);
    let mut events = Vec::new();
    let _ = executor
        .run(
            "do multi step work",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::GoalIntercepted { .. })),
        "expected GoalIntercepted: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::EvidenceLedgerUpdated { .. })),
        "expected EvidenceLedgerUpdated: {events:?}"
    );
    // Resume seed: last ledger intercept must be reconstructible.
    let led = events.iter().rev().find_map(|e| match e {
        AgentEvent::EvidenceLedgerUpdated { ledger } => Some(ledger.clone()),
        _ => None,
    });
    assert!(
        led.is_some_and(|l| !l.intercepts.is_empty()),
        "ledger must carry intercept records"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// TaskContract body is injected on the user-turn path, not the system prefix.
#[tokio::test]
async fn task_contract_is_injected_on_user_turn_not_system() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-contract-inject-{}",
        std::process::id() as u64 * 31 + 51
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let runtime = Arc::new(MockRuntime::new(vec![assistant_text("ok")]));
    // Chat path is enough: injection is in drive(), not goal-only.
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        4,
    );
    executor
        .run(
            "Request:\nfix auth timeout\nConstraints:\ndo not change public API\n",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let reqs = runtime.recorded_requests();
    assert!(!reqs.is_empty());
    let system = reqs[0]
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .map(|m| m.text_content())
        .unwrap_or_default();
    assert!(
        !system.contains("do not change public API"),
        "contract body must not enter system prefix: {system}"
    );
    let user_joined: String = reqs[0]
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        user_joined.contains("Task contract") && user_joined.contains("do not change public API"),
        "contract injection missing from user path: {user_joined}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Chat multi-turn: objective is the LATEST user message, not the first.
#[tokio::test]
async fn chat_second_message_rebinds_objective() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-obj-{}",
        std::process::id() as u64 * 17 + 5
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(MockRuntime::new(vec![assistant_text("docs updated")]));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    );
    let prior = vec![
        Message::text(Role::System, "sys"),
        Message::text(Role::User, "how many uncommitted files?"),
        Message::text(Role::Assistant, "about 66"),
    ];
    let mut events = Vec::new();
    let outcome = executor
        .run_conversation(
            prior,
            vec![ContentPart::Text {
                text: "update docs/ARCHITECTURE.md for the runtime".into(),
            }],
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        outcome.objective.text.contains("ARCHITECTURE")
            || outcome.objective.text.contains("runtime"),
        "objective must be latest user message, got {:?}",
        outcome.objective
    );
    assert!(
        !outcome.objective.text.contains("uncommitted"),
        "must not bind first-turn file-count question: {:?}",
        outcome.objective
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// No plan: pure list_files thrash ends via no-progress streak hard-stop (AC3).
#[tokio::test]
async fn no_plan_observe_streak_hard_stops() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-agent-nplan-{}",
        std::process::id() as u64 * 19 + 2
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    // Enough identical list_files rounds to hit LOOP_GUARD_THRESHOLD then
    // ProgressCaps::no_progress_rounds hard-stop; final text must not run.
    let mut responses: Vec<ModelResponse> = (0..6)
        .map(|i| {
            assistant_tool_call(
                &format!("l{i}"),
                "list_files",
                serde_json::json!({"path": "."}),
            )
        })
        .collect();
    responses.push(assistant_text("should not reach"));
    let runtime = Arc::new(MockRuntime::new(responses));
    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "explore only",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.stop_reason,
        StopReason::Incomplete,
        "no-plan observe thrash must not be Answered/Completed"
    );
    assert!(
        outcome
            .stop_detail
            .as_deref()
            .is_some_and(|d| d.contains("no-progress") || d.contains("thrash")),
        "expected hard-stop stop_detail, got {:?}",
        outcome.stop_detail
    );
    assert!(
        outcome.progress.no_progress_streak
            >= leveler_lifecycle::ProgressCaps::default().no_progress_rounds,
        "progress streak must reach cap: {}",
        outcome.progress.no_progress_streak
    );
    // Model rounds bounded: hard-stop before burning all scripted responses.
    assert!(
        runtime.recorded_requests().len() < 7,
        "expected bounded model requests, got {}",
        runtime.recorded_requests().len()
    );
    // Final scripted text must remain unused (hard-stop ends drive).
    assert!(
        !outcome.final_text.contains("should not reach"),
        "must not reach post-thrash assistant text: {}",
        outcome.final_text
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A sink that records every appended message, standing in for the resume
/// transcript store.
struct RecordingSink(Arc<Mutex<Vec<Message>>>);

#[async_trait]
impl leveler_agent::TranscriptSink for RecordingSink {
    async fn append(&mut self, messages: &[Message]) -> Result<(), AgentError> {
        self.0.lock().unwrap().extend_from_slice(messages);
        Ok(())
    }
}

/// Cancelling mid-way through a serial tool batch must not erase the work that
/// already happened in that batch: the completed calls' results are committed
/// to the transcript (paired with their tool_use), the remaining calls are
/// refused in place, and the epoch spend (commands run) is flushed — all
/// before `Cancelled` surfaces. Otherwise resume sees a model that "never ran"
/// tools whose side effects are already on disk, and the ledger under-counts.
#[tokio::test]
async fn cancel_mid_serial_batch_commits_completed_results_and_spend() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-cancel-batch-{}",
        std::process::id() as u64 * 89 + 41
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // One round, two serial commands. Cancel fires the moment the first
    // command's result is observed, so it lands before/while the second runs.
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_calls(vec![
            (
                "c1",
                "run_command",
                serde_json::json!({"program": "echo", "args": ["first-done"]}),
            ),
            (
                "c2",
                "run_command",
                serde_json::json!({"program": "echo", "args": ["second"]}),
            ),
        ]),
        assistant_text("never requested"),
    ]));

    let token = CancellationToken::new();
    let cancel = token.clone();
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let mut sink = RecordingSink(transcript.clone());
    let mut events: Vec<AgentEvent> = Vec::new();

    let result = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "run both commands",
        &mut |e| {
            if let AgentEvent::ToolResult { id, .. } = &e
                && id == "c1"
            {
                cancel.cancel();
            }
            events.push(e);
        },
        &mut sink,
        token,
    )
    .await;

    assert!(
        matches!(result, Err(AgentError::Cancelled)),
        "user cancel must surface as Cancelled: {result:?}"
    );

    // The first command ran before the cancel: its spend must be flushed into
    // a ProgressUpdated ledger, not silently dropped.
    let max_commands = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ProgressUpdated { ledger } => Some(ledger.cumulative_commands),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    assert!(
        max_commands >= 1,
        "cancel must not drop the completed command's spend; max_commands={max_commands}"
    );

    // And its result must be persisted, paired with the assistant tool calls,
    // so a resumed session knows what already executed.
    let messages = transcript.lock().unwrap();
    let c1_persisted = messages.iter().any(|m| {
        m.content.iter().any(|p| {
            matches!(
                p,
                ContentPart::ToolResult { result }
                    if result.call_id.as_str() == "c1" && result.content.contains("first-done")
            )
        })
    });
    assert!(
        c1_persisted,
        "the completed call's result must be committed to the transcript before Cancelled; \
         persisted messages: {messages:?}"
    );
    // Every tool_use must have a paired result — the cut-short second call is
    // refused in place, never left dangling.
    let c2_persisted = messages.iter().any(|m| {
        m.content.iter().any(
            |p| matches!(p, ContentPart::ToolResult { result } if result.call_id.as_str() == "c2"),
        )
    });
    assert!(
        c2_persisted,
        "the unfinished call must get a refusal result so the transcript stays paired; \
         persisted messages: {messages:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A round in which every tool call was refused by a guard (loop guard, plan
/// gate, budget, allowlist) is NOT progress. Repeated all-refused rounds must
/// feed the no-progress streak and hard-stop the turn — otherwise a model that
/// keeps re-issuing the same guarded non-observe call spins forever under
/// `UntilTerminal` (the guard refuses, the refusal resets the streak, repeat).
#[tokio::test]
async fn all_denied_rounds_count_as_no_progress_and_hard_stop() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-denied-rounds-{}",
        std::process::id() as u64 * 97 + 43
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // The same non-observe command every round. Rounds 1–2 run it (identical
    // output), from round 3 the loop guard refuses it. The refused rounds must
    // trip the no-progress hard stop long before the script runs dry.
    let echo = || {
        assistant_tool_call(
            "c1",
            "run_command",
            serde_json::json!({"program": "echo", "args": ["same-thing"]}),
        )
    };
    let runtime = Arc::new(MockRuntime::new(vec![
        echo(),
        echo(),
        echo(),
        echo(),
        echo(),
        echo(),
        echo(),
        echo(),
        assistant_text("all done"),
    ]));

    let outcome = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "do the thing",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.stop_reason,
        StopReason::Incomplete,
        "all-refused rounds must hard-stop as no progress, not run the script dry: {outcome:?}"
    );
    assert!(
        outcome.rounds <= 6,
        "the no-progress stop must fire within a few refused rounds, got {}",
        outcome.rounds
    );
    assert!(
        !outcome.final_text.contains("all done"),
        "the scripted closing text must never be reached: {}",
        outcome.final_text
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Gateways that never report streaming usage must not disable the token
/// budget: spend falls back to the transcript estimate, so `max_model_tokens`
/// still binds instead of silently never tripping.
#[tokio::test]
async fn token_budget_binds_when_the_gateway_reports_no_usage() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-zero-usage-{}",
        std::process::id() as u64 * 101 + 47
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    for i in 0..6 {
        std::fs::write(dir.join(format!("src/g{i}.rs")), "pub fn g() {}\n").unwrap();
    }
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // MockRuntime reports TokenUsage::default() (all zeros) for every round.
    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_tool_call("c0", "read_file", serde_json::json!({"path": "src/g0.rs"})),
        assistant_tool_call("c1", "read_file", serde_json::json!({"path": "src/g1.rs"})),
        assistant_tool_call("c2", "read_file", serde_json::json!({"path": "src/g2.rs"})),
        assistant_tool_call("c3", "read_file", serde_json::json!({"path": "src/g3.rs"})),
        assistant_tool_call("c4", "read_file", serde_json::json!({"path": "src/g4.rs"})),
        assistant_tool_call("c5", "read_file", serde_json::json!({"path": "src/g5.rs"})),
        assistant_text("finished everything"),
    ]));

    let outcome = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_step_limits(leveler_agent::StepLimits {
        // Tiny budget: the estimated transcript alone exceeds this by round 2.
        max_model_tokens: Some(100),
        ..Default::default()
    })
    .run(
        "read the files",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.stop_reason,
        StopReason::BudgetExhausted,
        "zero-usage gateway must not disable the token budget: {outcome:?}"
    );
    assert!(
        !outcome.final_text.contains("finished everything"),
        "the script must not run dry: {}",
        outcome.final_text
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `finish_reason: tool_calls` with no complete call is a provider/gateway
/// glitch, not a fatal condition: retry the round with feedback (bounded),
/// exactly like a parameter-level decode failure.
#[tokio::test]
async fn tool_calls_finish_without_calls_retries_instead_of_aborting() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-emptycalls-{}",
        std::process::id() as u64 * 103 + 49
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_text_finished("", FinishReason::ToolCalls),
        assistant_text("recovered"),
    ]));

    let outcome = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "do the thing",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("a tool_calls-without-calls glitch must not abort the turn");

    assert_eq!(outcome.final_text, "recovered");
    std::fs::remove_dir_all(&dir).ok();
}

/// `finish_reason: stop` alongside complete tool calls (some OpenAI-compatible
/// gateways do this) must execute the calls, not kill the turn.
#[tokio::test]
async fn stop_finish_with_tool_calls_executes_the_calls() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-stopcalls-{}",
        std::process::id() as u64 * 107 + 51
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn here() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let stop_with_call = ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new("c1"),
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": "src/lib.rs"}),
                },
            }],
        },
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    };
    let runtime = Arc::new(MockRuntime::new(vec![
        stop_with_call,
        assistant_text("read it"),
    ]));

    let mut events = Vec::new();
    let outcome = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "read the file",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("stop+tool_calls must be tolerated");

    let executed = events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolResult { id, is_error: false, .. } if id == "c1"));
    assert!(executed, "the call must actually run: {events:?}");
    assert_eq!(outcome.final_text, "read it");
    std::fs::remove_dir_all(&dir).ok();
}

/// `length` truncation while emitting a tool call: the partial call is never
/// executed, but the turn recovers with a "re-issue smaller" nudge instead of
/// aborting — text truncation already gets bounded continuations, and a
/// too-large apply_patch deserves the same second chance.
#[tokio::test]
async fn length_truncated_tool_call_recovers_with_a_smaller_reissue() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-lentool-{}",
        std::process::id() as u64 * 109 + 53
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let truncated_call = ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new("big"),
                    name: "apply_patch".to_string(),
                    arguments: serde_json::json!({"patch": "*** Begin Patch (cut off"}),
                },
            }],
        },
        finish_reason: FinishReason::Length,
        usage: TokenUsage::default(),
    };
    let runtime = Arc::new(MockRuntime::new(vec![
        truncated_call,
        assistant_tool_call("c2", "read_file", serde_json::json!({"path": "src/lib.rs"})),
        assistant_text("done smaller"),
    ]));

    let mut events = Vec::new();
    let outcome = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "patch the file",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("a truncated tool call must be recoverable");

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { id, .. } if id == "big")),
        "the truncated call must never execute: {events:?}"
    );
    assert_eq!(outcome.final_text, "done smaller");
    std::fs::remove_dir_all(&dir).ok();
}

/// A non-goal turn whose answer is empty gets one "actually answer" nudge
/// before the loop accepts it — an empty `Answered` is indistinguishable from
/// a silent failure for the caller.
#[tokio::test]
async fn empty_answer_gets_one_nudge_before_answered() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-emptyans-{}",
        std::process::id() as u64 * 113 + 57
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let runtime = Arc::new(MockRuntime::new(vec![
        assistant_text(""),
        assistant_text("the real answer"),
    ]));

    let outcome = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "answer the question",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.final_text, "the real answer",
        "an empty answer must be nudged once, not silently accepted"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// #1/#2: after the plan is complete, re-running commands (builds/tests/curl)
/// is redundant closeout work — NOT a fresh objective. The drive must stop
/// within the closeout cap and report Answered, because "plan complete" means
/// the task is done; Incomplete here would misreport a finished task as a
/// failure (which is exactly what a user sees as "任务未完成").
#[tokio::test]
async fn plan_complete_then_repeated_execute_stops_as_answered() {
    let dir = std::env::temp_dir().join(format!(
        "leveler-closeout-exec-{}",
        std::process::id() as u64 * 131 + 61
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let runtime = Arc::new(MockRuntime::new(vec![
        // Plan fully complete → enter closing.
        assistant_tool_call(
            "p1",
            "update_plan",
            serde_json::json!({"plan": [{"step": "build go backend", "status": "completed"}]}),
        ),
        // Redundant re-verification: execute commands (not observe), each
        // distinct so the loop-guard does not fire — only the closeout cap can.
        assistant_tool_call(
            "e1",
            "run_command",
            serde_json::json!({"program": "echo", "args": ["verify 1"]}),
        ),
        assistant_tool_call(
            "e2",
            "run_command",
            serde_json::json!({"program": "echo", "args": ["verify 2"]}),
        ),
        assistant_tool_call(
            "e3",
            "run_command",
            serde_json::json!({"program": "echo", "args": ["verify 3"]}),
        ),
        assistant_tool_call(
            "e4",
            "run_command",
            serde_json::json!({"program": "echo", "args": ["verify 4"]}),
        ),
        assistant_text("should never be reached — the audit loop must be cut"),
    ]));

    let outcome = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        20,
    )
    .run(
        "port the backend to Go",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.stop_reason,
        StopReason::Answered,
        "plan complete = done; must not misreport redundant closeout as Incomplete: {outcome:?}"
    );
    assert!(
        outcome.rounds <= 5,
        "the audit loop must be cut within the closeout cap, got {} rounds",
        outcome.rounds
    );
    assert!(
        !outcome.final_text.contains("should never be reached"),
        "the drive must stop before running the whole script dry: {}",
        outcome.final_text
    );
    std::fs::remove_dir_all(&dir).ok();
}
