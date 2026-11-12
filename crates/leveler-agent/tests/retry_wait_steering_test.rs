//! Input typed while a transient provider failure is being retried must not be
//! lost or applied to a half-built request.
//!
//! The retry lifecycle re-sends the SAME logical request, so a steer that
//! arrives during the backoff cannot join it. It is queued exactly like input
//! typed while a response streams, and the next model round (after the
//! recovered attempt and its tools) carries it. If the recovered attempt ends
//! the turn, the app's own unconsumed-input delivery takes over; this test pins
//! the round-boundary path.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, Executor, NoopSink, SteeringSource};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, DeliveryState, FinishReason, Message, ModelError, ModelErrorKind, ModelEvent,
    ModelEventStream, ModelProfile, ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role,
    TokenUsage, ToolCall,
};
use leveler_tools::ToolContext;

/// Fails the physical calls listed in `fail_at` (1-based) with a NotSent
/// transport error, then serves the scripted responses for every other call.
struct FlakyThenScripted {
    fail_at: Vec<u32>,
    calls: Mutex<u32>,
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

#[async_trait]
impl ModelRuntime for FlakyThenScripted {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unreachable!("the executor streams")
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.requests.lock().unwrap().push(request);
        {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if self.fail_at.contains(&*calls) {
                return Err(ModelError::new(
                    ModelErrorKind::ProviderUnavailable,
                    "Connection refused (os error 61)",
                )
                .with_delivery_state(DeliveryState::NotSent));
            }
        }
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("the script must answer every non-failed call");
        let mut events = vec![Ok(ModelEvent::MessageStarted {
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

#[derive(Default)]
struct Steer {
    pending: Mutex<Vec<String>>,
}

impl SteeringSource for Steer {
    fn take_pending(&self) -> Vec<String> {
        std::mem::take(&mut *self.pending.lock().unwrap())
    }
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: name.to_string(),
                    arguments,
                },
            }],
            route: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

fn text(body: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, body),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

fn user_texts(request: &ModelRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| {
            message.content.iter().filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .collect()
}

#[tokio::test]
async fn a_steer_during_retry_backoff_is_delivered_to_the_next_round() {
    let dir = std::env::temp_dir().join(format!("leveler-retry-steer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), "hello").unwrap();
    let ctx = ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::FullAccess);

    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Arc::new(FlakyThenScripted {
        // One refused attempt, then the recovered round asks for a tool, then
        // the round after that answers. The steer can only ride the last one.
        fail_at: vec![1],
        calls: Mutex::new(0),
        responses: Mutex::new(
            vec![
                tool_call("c1", "read_file", serde_json::json!({"path": "note.txt"})),
                text("done"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    });

    let steering = Arc::new(Steer::default());
    let queued = steering.clone();
    // The first attempt fails immediately; the default schedule waits ~1s
    // before the retry. Typing at 300ms lands inside that wait.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        queued
            .pending
            .lock()
            .unwrap()
            .push("also check the other file".to_string());
    });

    let mut events = Vec::new();
    let outcome = Executor::new(
        runtime,
        Arc::new(leveler_tools::default_registry()),
        ctx,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_steering(steering)
    .run(
        "read the note",
        &mut |event| events.push(event),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("the turn recovers and continues");

    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ModelRetrying { attempt: 1, .. })),
        "the retry happened inside the turn: {events:?}"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        3,
        "refused attempt, recovered attempt, next round"
    );
    assert!(
        !user_texts(&requests[1])
            .iter()
            .any(|text| text.contains("also check")),
        "the retry re-sends the same admitted request, not a rewritten one"
    );
    assert!(
        user_texts(&requests[2])
            .iter()
            .any(|text| text.contains("also check the other file")),
        "the steer typed during the wait reaches the next round: {:?}",
        user_texts(&requests[2])
    );
    assert_eq!(outcome.final_text, "done");
    std::fs::remove_dir_all(&dir).ok();
}

/// The window between "a tool has executed" and "the next model request": a
/// transient failure there is retried as the same logical request, and the
/// completed tool must not run a second time.
#[tokio::test]
async fn a_retry_after_tool_execution_does_not_repeat_the_side_effect() {
    let dir =
        std::env::temp_dir().join(format!("leveler-retry-side-effect-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::FullAccess);

    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Arc::new(FlakyThenScripted {
        // The tool runs after call 1; call 2 (the request carrying its result)
        // is refused once, and the retry re-sends that same request.
        fail_at: vec![2],
        calls: Mutex::new(0),
        responses: Mutex::new(
            vec![
                tool_call(
                    "c1",
                    "write_file",
                    serde_json::json!({"path": "written.txt", "content": "once"}),
                ),
                text("done"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    });

    let mut events = Vec::new();
    let outcome = Executor::new(
        runtime,
        Arc::new(leveler_tools::default_registry()),
        ctx,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "write the file",
        &mut |event| events.push(event),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("the turn recovers");

    let starts = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolCall { .. }))
        .count();
    assert_eq!(starts, 1, "the side-effecting tool ran exactly once");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ModelRetrying { attempt: 1, .. })),
        "the failure after the tool was retried: {events:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("written.txt")).unwrap(),
        "once"
    );

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "tool round, refused request, retry");
    assert_eq!(
        serde_json::to_value(&requests[1].messages).unwrap(),
        serde_json::to_value(&requests[2].messages).unwrap(),
        "the retry re-sends the same admitted request, tool result included"
    );
    assert_eq!(outcome.final_text, "done");
    std::fs::remove_dir_all(&dir).ok();
}
