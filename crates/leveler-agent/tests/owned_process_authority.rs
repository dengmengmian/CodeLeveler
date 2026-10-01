//! Owned process control goes through Main Agent admission across turns.
//! The model is scripted; process ownership and tree cleanup use real OS processes.
#![cfg(unix)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use leveler_agent::{AgentEvent, Executor, NoopSink, TranscriptSink};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{
    AutoApprove, AutoDeny, BackgroundTaskRegistry, BackgroundTaskStatus, PermissionProfile,
    Workspace,
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_tools::ToolContext;
use tokio_util::sync::CancellationToken;

struct Script(Mutex<VecDeque<ModelResponse>>);

#[async_trait]
impl ModelRuntime for Script {
    async fn generate(
        &self,
        _: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted"))
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        Ok(leveler_model::stream_from_response(
            self.generate(request, cancel).await?,
        ))
    }
    async fn profile(&self, _: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id":"m", "provider":"mock", "model_id":"m", "protocol":"openai_chat",
            "capabilities":{"streaming":true,"tool_calling":true,"parallel_tool_calls":false,"structured_output":false,"reasoning":false,"vision":false},
            "limits":{"context_window":128000,"reliable_context":64000,"max_output_tokens":4096,"max_tool_schema_bytes":65536,"max_parallel_tool_calls":1}
        })).unwrap())
    }
}

fn response(name: &str, args: serde_json::Value) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::generate(),
                    name: name.into(),
                    arguments: args,
                },
            }],
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

fn done() -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, "done"),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

#[derive(Default)]
struct Saved(Vec<Message>);
#[async_trait]
impl TranscriptSink for Saved {
    async fn append(&mut self, messages: &[Message]) -> Result<(), leveler_engine::PortError> {
        self.0.extend_from_slice(messages);
        Ok(())
    }
}

struct Harness {
    dir: tempfile::TempDir,
    registry: Arc<BackgroundTaskRegistry>,
    tools: Arc<leveler_tools::ToolRegistry>,
    environment: Arc<leveler_core::EnvSnapshot>,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let environment = Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap(),
            std::env::temp_dir(),
        ));
        let registry = Arc::new(BackgroundTaskRegistry::with_environment(
            environment.clone(),
        ));
        let capabilities = leveler_tools::Capabilities::in_process(environment.clone())
            .with_background_tasks(registry.clone());
        let tools = Arc::new(leveler_tools::model_surface(
            leveler_tools::CapabilityPacks::ALL,
            &capabilities,
        ));
        Self {
            dir,
            registry,
            tools,
            environment,
        }
    }

    fn executor(&self, session: &str, responses: Vec<ModelResponse>, read_only: bool) -> Executor {
        let ctx = ToolContext::with_environment(
            Workspace::new(self.dir.path()).unwrap(),
            PermissionProfile::Assisted,
            self.environment.clone(),
        )
        .with_session_scope(session)
        .with_read_only(read_only);
        Executor::new(
            Arc::new(Script(Mutex::new(responses.into()))),
            self.tools.clone(),
            ctx,
            ModelRef::new("mock", "m"),
            8,
        )
        .with_approver(Arc::new(AutoApprove))
        .with_delegation(false)
        .with_seeded_plan(leveler_agent::PlanState {
            steps: vec![leveler_agent::PlanStep {
                step: "Manage the owned background process".into(),
                status: "in_progress".into(),
                id: None,
                origin: leveler_agent::PlanOrigin::ModelExplicit,
            }],
        })
    }

    async fn spawn_tree(&self) -> (String, Vec<u32>, Saved) {
        // Three generations in the same owned group, all held alive until stop.
        let spawn = response(
            "run_command",
            serde_json::json!({
                "program":"sh", "args":["-c", "echo $$ > parent.pid; sh -c 'echo $$ > child.pid; sleep 60 & echo $! > grandchild.pid; wait' & wait"],
                "background":true, "background_lifetime":"runtime"
            }),
        );
        let mut events = Vec::new();
        let mut saved = Saved::default();
        self.executor("owner", vec![spawn, done()], false)
            .run(
                "Start a background process for this session",
                &mut |event| events.push(event),
                &mut saved,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let (_, output) = tool_result(&events, "run_command");
        let id = output
            .lines()
            .find_map(|line| line.strip_prefix("task_id: "))
            .expect("spawn must provide task id")
            .to_string();
        let pids = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let pids: Option<Vec<u32>> = ["parent.pid", "child.pid", "grandchild.pid"]
                    .into_iter()
                    .map(|name| {
                        std::fs::read_to_string(self.dir.path().join(name))
                            .ok()?
                            .trim()
                            .parse()
                            .ok()
                    })
                    .collect();
                if let Some(pids) = pids {
                    break pids;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("all generations must start");
        assert!(
            pids.iter().all(|pid| alive(*pid)),
            "real process tree must exist before stop: {pids:?}"
        );
        (id, pids, saved)
    }
}

fn tool_result<'a>(events: &'a [AgentEvent], name: &str) -> (bool, &'a str) {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolResult {
                name: actual,
                is_error,
                preview,
                ..
            } if actual == name => Some((*is_error, preview.as_str())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing result for {name}: {events:?}"))
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[tokio::test]
async fn later_main_agent_turn_observes_and_stops_owned_tree_without_file_write_scope() {
    let h = Harness::new();
    let (id, pids, saved) = h.spawn_tree().await;
    let mut events = Vec::new();
    // A new Executor with prior dialogue models a later turn. File scope is
    // intentionally empty: process control must not require claiming files.
    h.executor(
        "owner",
        vec![
            response("get_task", serde_json::json!({"task_id":id})),
            response("kill_task", serde_json::json!({"task_id":id})),
            done(),
        ],
        false,
    )
    .with_write_allowlist(Some(Vec::new()))
    // Any accidental approval requirement must fail this acceptance test;
    // an auto-approver would hide an arbitrary-kill permission regression.
    .with_approver(Arc::new(AutoDeny))
    .run_conversation(
        saved.0,
        vec![ContentPart::Text {
            text: "Inspect and stop the process from the previous turn".into(),
        }],
        &mut |event| events.push(event),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let (error, output) = tool_result(&events, "get_task");
    assert!(!error, "later owner turn must inspect: {output}");
    let (error, output) = tool_result(&events, "kill_task");
    assert!(
        !error,
        "later owner turn must stop without file authority: {output}"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let terminal =
                h.registry.get(&id).await.unwrap().status == BackgroundTaskStatus::Killed;
            if terminal && pids.iter().all(|pid| !alive(*pid)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("terminal must coincide with actual disappearance of all three generations");
}

#[tokio::test]
async fn main_agent_rejects_foreign_and_unknown_owned_task_targets() {
    let h = Harness::new();
    let (id, pids, _) = h.spawn_tree().await;
    for (session, target, reason) in [
        ("stranger", id.as_str(), "not owned by this session"),
        ("owner", "bg-unknown", "unknown task"),
    ] {
        for name in ["get_task", "kill_task"] {
            let mut events = Vec::new();
            h.executor(
                session,
                vec![
                    response(name, serde_json::json!({"task_id":target})),
                    done(),
                ],
                false,
            )
            .with_approver(Arc::new(AutoDeny))
            .run(
                "Inspect or stop the requested background task",
                &mut |event| events.push(event),
                &mut NoopSink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let (error, output) = tool_result(&events, name);
            assert!(
                error && output.contains(reason),
                "{name} must reject {target}: {output}"
            );
            assert!(
                pids.iter().all(|pid| alive(*pid)),
                "rejected call must not signal the owned tree"
            );
        }
    }
    h.registry.kill_owned(&id, "owner").await.unwrap();
    h.registry
        .wait_owned(
            &id,
            "owner",
            Some(Duration::from_secs(5)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn read_only_side_surface_observes_owned_task_but_cannot_stop_it() {
    let h = Harness::new();
    let (id, pids, _) = h.spawn_tree().await;
    let subset = h.tools.read_only_subset();
    assert!(subset.get("get_task").is_some());
    assert!(subset.get("kill_task").is_none());
    // Even a model attempting an unadvertised control call is denied by Host.
    let mut events = Vec::new();
    h.executor(
        "owner",
        vec![
            response("get_task", serde_json::json!({"task_id":id})),
            response("kill_task", serde_json::json!({"task_id":id})),
            done(),
        ],
        true,
    )
    .with_approver(Arc::new(AutoDeny))
    .run(
        "Observe the background process",
        &mut |event| events.push(event),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let (error, output) = tool_result(&events, "get_task");
    assert!(!error, "observation remains permitted: {output}");
    let (error, output) = tool_result(&events, "kill_task");
    assert!(
        error && output.contains("read-only"),
        "Host must deny control under read-only overlay: {output}"
    );
    assert!(
        pids.iter().all(|pid| alive(*pid)),
        "read-only call must leave tree alive"
    );
    h.registry.kill_owned(&id, "owner").await.unwrap();
    h.registry
        .wait_owned(
            &id,
            "owner",
            Some(Duration::from_secs(5)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
}
