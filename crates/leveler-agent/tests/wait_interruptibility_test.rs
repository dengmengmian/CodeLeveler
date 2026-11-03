//! A `wait_task` that waits for a long-running process must not take the
//! agent's (and the user's) execution opportunity away for the whole bound.
//!
//! The tool is allowed to wait — a model that asked for a process to exit is
//! entitled to the observation. What it must never do is keep waiting after the
//! user typed something: a queued message is only injected at the top of the
//! next model round, so a six-hundred-second wait silently becomes a
//! six-hundred-second silence for the person using the product.
#![cfg(unix)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, Executor, NoopSink, SteeringSource};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEvent, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_tools::ToolContext;

/// The background task id the first call returned, filled by the observer as
/// the turn runs so the scripted `wait_task` can name the real task.
type TaskId = Arc<Mutex<Option<String>>>;

struct Scripted {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    task_id: TaskId,
}

fn substitute(value: &mut serde_json::Value, task_id: &Option<String>) {
    match value {
        serde_json::Value::String(text) if text == "{task}" => {
            if let Some(id) = task_id {
                *text = id.clone();
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                substitute(item, task_id);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                substitute(item, task_id);
            }
        }
        _ => {}
    }
}

#[async_trait]
impl ModelRuntime for Scripted {
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
        let mut response = self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })?;
        {
            let task_id = self.task_id.lock().unwrap().clone();
            for part in &mut response.message.content {
                if let ContentPart::ToolCall { call } = part {
                    substitute(&mut call.arguments, &task_id);
                }
            }
        }
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

fn call(id: &str, name: &str, arguments: serde_json::Value) -> ModelResponse {
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

/// A host whose steering queue can be filled while a tool call is in flight —
/// exactly what the app does when the user submits during a running turn.
#[derive(Default)]
struct Steer {
    pending: Mutex<Vec<String>>,
    started: Mutex<Vec<String>>,
    yields: Arc<Mutex<Vec<(String, CancellationToken)>>>,
}

impl Steer {
    /// What the app does when the user hits enter during a running turn: the
    /// text is queued for the next round, and every observation call in flight
    /// is told to hand the round back. Queue first: an input that arrives
    /// between the call returning and the next round is still seen.
    fn push(&self, text: &str) {
        self.pending.lock().unwrap().push(text.to_string());
        for (_, token) in self.yields.lock().unwrap().iter() {
            token.cancel();
        }
    }
}

impl SteeringSource for Steer {
    fn take_pending(&self) -> Vec<String> {
        std::mem::take(&mut *self.pending.lock().unwrap())
    }

    fn tool_call_started(
        &self,
        id: &str,
        _cancel: CancellationToken,
        yield_now: CancellationToken,
    ) {
        self.started.lock().unwrap().push(id.to_string());
        self.yields
            .lock()
            .unwrap()
            .push((id.to_string(), yield_now));
    }
}

fn workspace(tag: &str) -> (std::path::PathBuf, ToolContext) {
    let dir = std::env::temp_dir().join(format!(
        "leveler-wait-interrupt-{tag}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::FullAccess);
    (dir, ctx)
}

fn registry() -> Arc<leveler_tools::ToolRegistry> {
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    Arc::new(registry)
}

struct Run {
    outcome: Result<leveler_agent::AgentOutcome, leveler_agent::AgentError>,
    events: Vec<AgentEvent>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

async fn run(responses: Vec<ModelResponse>, ctx: ToolContext, steering: Arc<Steer>) -> Run {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let task_id: TaskId = Arc::new(Mutex::new(None));
    let observe_tasks = task_id.clone();
    let runtime = Arc::new(Scripted {
        responses: Mutex::new(responses.into()),
        requests: requests.clone(),
        task_id,
    });
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collected = events.clone();
    let mut observer = move |event: AgentEvent| {
        if let AgentEvent::ToolResult { preview, .. } = &event
            && let Some(rest) = preview.split("task_id: ").nth(1)
        {
            let id = rest.lines().next().unwrap_or("").trim().to_string();
            if !id.is_empty() {
                *observe_tasks.lock().unwrap() = Some(id);
            }
        }
        collected.lock().unwrap().push(event);
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        Executor::new(runtime, registry(), ctx, ModelRef::new("mock", "m"), 10)
            .with_steering(steering)
            .run(
                "run it",
                &mut observer,
                &mut NoopSink,
                CancellationToken::new(),
            ),
    )
    .await
    .expect("the turn must not outlive its own bound");
    drop(observer);
    let events = Arc::try_unwrap(events)
        .map(|cell| cell.into_inner().unwrap())
        .unwrap_or_default();
    Run {
        outcome,
        events,
        requests,
    }
}

fn user_inputs(request: &ModelRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| {
            m.content.iter().filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .collect()
}

fn wait_result(events: &[AgentEvent]) -> (bool, String) {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                is_error,
                preview,
                ..
            } if id == "wait" => Some((*is_error, preview.clone())),
            _ => None,
        })
        .expect("the wait call returned a result")
}

/// The user typed while the agent watched a server: the wait must end at the
/// user's message, not at the task's exit or the 600s bound.
#[tokio::test]
async fn a_user_message_ends_a_wait_for_a_running_task() {
    let (dir, ctx) = workspace("user-message");
    let steering = Arc::new(Steer::default());
    let queued = steering.clone();
    // A watcher stands in for the user: it waits until the `wait_task` call is
    // in flight and then types.
    let watcher = steering.clone();
    tokio::spawn(async move {
        loop {
            let waiting = watcher
                .started
                .lock()
                .unwrap()
                .iter()
                .any(|id| id == "wait");
            if waiting {
                tokio::time::sleep(Duration::from_millis(300)).await;
                queued.push("现在进展怎么样了？");
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let started = Instant::now();
    let run = run(
        vec![
            // A process that keeps running: it will never reach a terminal
            // status inside this test.
            call(
                "start",
                "run_command",
                serde_json::json!({ "program": "sleep", "args": ["120"], "background": true, "background_lifetime": "runtime" }),
            ),
            // The model chooses the long wait, exactly as it may.
            call(
                "wait",
                "wait_task",
                serde_json::json!({ "task_id": "{task}", "timeout_seconds": 600 }),
            ),
            text("still running; answering the user"),
        ],
        ctx,
        steering,
    )
    .await;
    std::fs::remove_dir_all(&dir).ok();

    assert!(run.outcome.is_ok(), "{:?}", run.outcome);
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the wait must yield at the user's message, took {:?}",
        started.elapsed()
    );
    let (is_error, preview) = wait_result(&run.events);
    assert!(!is_error, "yielding is not a failure: {preview}");
    assert!(preview.contains("running"), "{preview}");

    let requests = run.requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "the model got a third round");
    let last = user_inputs(&requests[2]);
    assert!(
        last.iter().any(|text| text.contains("现在进展怎么样了")),
        "the user's message reached the model: {last:?}"
    );
}

/// The same wait with no user input keeps the model's chosen observation:
/// nothing here shortens a legitimate wait.
#[tokio::test]
async fn a_wait_for_a_finite_task_still_returns_its_exit() {
    let (dir, ctx) = workspace("finite");
    let steering = Arc::new(Steer::default());
    let run = run(
        vec![
            call(
                "start",
                "run_command",
                serde_json::json!({ "program": "sh", "args": ["-c", "sleep 1; echo done"], "background": true, "background_lifetime": "runtime" }),
            ),
            call(
                "wait",
                "wait_task",
                serde_json::json!({ "task_id": "{task}", "timeout_seconds": 600 }),
            ),
            text("finished"),
        ],
        ctx,
        steering,
    )
    .await;
    std::fs::remove_dir_all(&dir).ok();

    assert!(run.outcome.is_ok(), "{:?}", run.outcome);
    let (is_error, preview) = wait_result(&run.events);
    assert!(!is_error, "{preview}");
    assert!(preview.contains("exited"), "{preview}");
    assert!(preview.contains("done"), "{preview}");
}
