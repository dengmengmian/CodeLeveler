//! A running command call is observable and stoppable on its own: its output
//! streams while it runs, its exit code and stop outcome reach the result, and
//! a host can stop that one call without cancelling the turn.
#![cfg(unix)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, Executor, NoopSink, SteeringSource};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{CommandStop, PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEvent, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_tools::ToolContext;

struct Scripted(Mutex<VecDeque<ModelResponse>>);

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
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let response = self.0.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })?;
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

fn shell(id: &str, cmd: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: "shell_command".to_string(),
                    arguments: serde_json::json!({ "cmd": cmd }),
                },
            }],
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

/// Records every command call's cancellation handle, the way the app does
/// for a client's per-call stop.
#[derive(Default)]
struct CallHandles {
    started: Mutex<Vec<(String, CancellationToken)>>,
    ended: Mutex<Vec<String>>,
}

impl SteeringSource for CallHandles {
    fn take_pending(&self) -> Vec<String> {
        Vec::new()
    }

    fn tool_call_started(&self, id: &str, cancel: CancellationToken) {
        self.started.lock().unwrap().push((id.to_string(), cancel));
    }

    fn tool_call_ended(&self, id: &str) {
        self.ended.lock().unwrap().push(id.to_string());
    }
}

fn workspace(tag: &str) -> (std::path::PathBuf, ToolContext) {
    let dir = std::env::temp_dir().join(format!(
        "leveler-command-controls-{tag}-{}",
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

async fn run(
    responses: Vec<ModelResponse>,
    ctx: ToolContext,
    host: Arc<CallHandles>,
) -> (
    Result<leveler_agent::AgentOutcome, leveler_agent::AgentError>,
    Vec<AgentEvent>,
) {
    let mut events = Vec::new();
    let outcome = Executor::new(
        Arc::new(Scripted(Mutex::new(responses.into()))),
        registry(),
        ctx,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_steering(host)
    .run(
        "run it",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await;
    (outcome, events)
}

/// A host stops one long command: its process tree is killed and confirmed
/// gone, the result says so, and the turn carries on to its own end.
#[tokio::test]
async fn a_host_can_stop_one_running_command_and_the_turn_continues() {
    let (dir, ctx) = workspace("stop");
    let host = Arc::new(CallHandles::default());
    let watcher = host.clone();
    tokio::spawn(async move {
        loop {
            let first = watcher.started.lock().unwrap().first().cloned();
            if let Some((_, token)) = first {
                // Let the shell actually start its child first.
                tokio::time::sleep(Duration::from_millis(200)).await;
                token.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let started = Instant::now();
    let (outcome, events) = run(
        vec![shell("c1", "sleep 30"), text("stopped as asked")],
        ctx,
        host.clone(),
    )
    .await;
    std::fs::remove_dir_all(&dir).ok();

    assert!(outcome.is_ok(), "the turn is not cancelled: {outcome:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "sleep was stopped"
    );
    let result = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id, is_error, stop, ..
            } if id == "c1" => Some((*is_error, *stop)),
            _ => None,
        })
        .expect("c1 result");
    assert_eq!(result, (true, Some(CommandStop::Confirmed)));
    assert_eq!(host.started.lock().unwrap().len(), 1);
    assert_eq!(*host.ended.lock().unwrap(), vec!["c1".to_string()]);
}

/// A stopped command's result tells the model what it had printed. Dogfood:
/// the user watched ten ticks stream by, stopped the command, and the model
/// — told only "command was cancelled" — could not say where it stopped.
#[tokio::test]
async fn a_stopped_command_result_carries_the_output_it_printed() {
    let (dir, ctx) = workspace("stop-tail");
    let host = Arc::new(CallHandles::default());
    let watcher = host.clone();
    let marker = dir.join("printed");
    let printed = marker.clone();
    tokio::spawn(async move {
        loop {
            let first = watcher.started.lock().unwrap().first().cloned();
            if let Some((_, token)) = first
                && printed.exists()
            {
                tokio::time::sleep(Duration::from_millis(300)).await;
                token.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let cmd = format!(
        "for i in 1 2 3; do echo tick $i; done; touch '{}'; sleep 30",
        marker.display()
    );
    let (outcome, events) = run(vec![shell("c1", &cmd), text("stopped")], ctx, host).await;
    std::fs::remove_dir_all(&dir).ok();
    assert!(outcome.is_ok(), "{outcome:?}");
    let (preview, stop) = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id, preview, stop, ..
            } if id == "c1" => Some((preview.clone(), *stop)),
            _ => None,
        })
        .expect("c1 result");
    assert_eq!(stop, Some(CommandStop::Confirmed));
    assert!(preview.contains("cancelled"), "{preview}");
    assert!(preview.contains("tick 3"), "{preview}");
}

/// Output arrives while the command runs, before its result, and the result
/// carries the process's own exit code.
#[tokio::test]
async fn a_running_command_streams_output_and_reports_its_exit_code() {
    let (dir, ctx) = workspace("stream");
    let host = Arc::new(CallHandles::default());
    let (outcome, events) = run(
        vec![
            shell("c1", "printf 'one\\n'; printf 'oops\\n' >&2; exit 3"),
            text("done"),
        ],
        ctx,
        host,
    )
    .await;
    std::fs::remove_dir_all(&dir).ok();
    assert!(outcome.is_ok(), "{outcome:?}");

    let result_at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolResult { id, .. } if id == "c1"))
        .expect("c1 result");
    let streamed: Vec<(leveler_execution::OutputStream, String)> = events[..result_at]
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolOutput { id, stream, text } if id == "c1" => {
                Some((*stream, text.clone()))
            }
            _ => None,
        })
        .collect();
    assert!(
        streamed
            .iter()
            .any(|(s, t)| *s == leveler_execution::OutputStream::Stdout && t.contains("one")),
        "{streamed:?}"
    );
    assert!(
        streamed
            .iter()
            .any(|(s, t)| *s == leveler_execution::OutputStream::Stderr && t.contains("oops")),
        "{streamed:?}"
    );
    let facts = match &events[result_at] {
        AgentEvent::ToolResult {
            is_error,
            exit_code,
            stop,
            ..
        } => (*is_error, *exit_code, *stop),
        _ => unreachable!(),
    };
    assert_eq!(facts, (true, Some(3), None));
}
